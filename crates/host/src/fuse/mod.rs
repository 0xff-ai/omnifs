//! FUSE filesystem implementation.
//!
//! Bridges the omnifs virtual filesystem to the kernel FUSE subsystem.
//! Routes operations to WASM providers. Supports direct filesystem
//! passthrough when providers set backing paths on nodes.

pub(crate) mod inode;

use crate::cache::l0::{BrowseCacheL0, L0Key};
use crate::cache::{self, CacheRecord, RecordKind};
use crate::omnifs::provider::types::{
    EntryKind, ErrorKind, ListResult, LookupResult, OpResult, ProviderError,
};
use crate::path_key::{PathKey, PathToInode};
use crate::path_prefix::path_prefix_matches;
use crate::registry::ProviderRegistry;
use crate::runtime::CalloutRuntime;
use dashmap::DashMap;
use dashmap::mapref::one::Ref;
use fuser::{
    Errno, FileAttr, FileHandle as FuseFileHandle, Filesystem, FopenFlags, Generation, INodeNo,
    LockOwner, MountOption, OpenFlags, ReplyAttr, ReplyData, ReplyDirectory, ReplyEmpty,
    ReplyEntry, ReplyOpen, Request,
};
use inode::NodeEntry;
use std::ffi::OsStr;
use std::path::Path;
use std::sync::Arc;
use std::sync::atomic::AtomicU64;
use std::time::{Duration, Instant};
use tokio::runtime::Handle;

/// Kernel-side entry/attr TTL. The host never expires entries on time,
/// only on capacity or explicit invalidation via the FUSE notifier and
/// provider cache-invalidate effects. We still must hand the kernel
/// a finite Duration, so pick one large enough that refresh churn is
/// irrelevant in practice (~136 years).
const TTL: Duration = Duration::from_secs(u32::MAX as u64);
const ROOT_INO: u64 = 1;

type DirSnapshot = Vec<(u64, String, EntryKind)>;

/// Map a provider error to its corresponding FUSE errno.
fn provider_errno(error: &ProviderError) -> Errno {
    match error.kind {
        ErrorKind::NotFound => Errno::ENOENT,
        ErrorKind::NotADirectory => Errno::ENOTDIR,
        ErrorKind::NotAFile => Errno::EISDIR,
        ErrorKind::PermissionDenied | ErrorKind::Denied => Errno::EACCES,
        ErrorKind::InvalidInput => Errno::EINVAL,
        ErrorKind::TooLarge => Errno::EFBIG,
        ErrorKind::Network
        | ErrorKind::Timeout
        | ErrorKind::RateLimited
        | ErrorKind::VersionMismatch
        | ErrorKind::Internal => Errno::EIO,
    }
}

struct FuseTrace {
    op: &'static str,
    detail: String,
    start: Instant,
}

impl FuseTrace {
    fn new(op: &'static str, detail: String) -> Self {
        Self {
            op,
            detail,
            start: Instant::now(),
        }
    }
}

impl Drop for FuseTrace {
    fn drop(&mut self) {
        tracing::info!(
            target: "omnifs_trace",
            kind = "fuse",
            op = self.op,
            detail = self.detail.as_str(),
            elapsed_us = self.start.elapsed().as_micros(),
            "trace_event"
        );
    }
}

pub struct FuseFs {
    rt: Handle,
    registry: Arc<ProviderRegistry>,
    inodes: DashMap<u64, NodeEntry>,
    /// Reverse lookup: (mount name, path) -> inode, for dedup.
    /// Shared via `Arc` so the FUSE notifier can also hold a reference
    /// and invalidate entries concurrently without cloning the map.
    path_to_inode: Arc<PathToInode>,
    next_ino: AtomicU64,
    dir_snapshots: DashMap<u64, DirSnapshot>,
    next_fh: AtomicU64,
    /// Caches file content by file handle; populated on first read, evicted on release.
    file_cache: DashMap<u64, Vec<u8>>,
    /// Single owner of the per-mount L0 browse caches (inode-keyed, in-memory).
    l0_caches: DashMap<String, BrowseCacheL0>,
}

impl FuseFs {
    pub fn new(rt: Handle, registry: Arc<ProviderRegistry>) -> Self {
        Self::new_with_path_map(rt, registry, Arc::new(DashMap::new()))
    }

    pub fn new_with_path_map(
        rt: Handle,
        registry: Arc<ProviderRegistry>,
        path_to_inode: Arc<PathToInode>,
    ) -> Self {
        let inodes = DashMap::new();

        let root_entry = NodeEntry {
            mount_name: registry.root_mount_name().unwrap_or("").to_string(),
            path: String::new(),
            kind: EntryKind::Directory,
            size: 0,
            backing_path: None,
        };
        inodes.insert(ROOT_INO, root_entry);

        Self {
            rt,
            registry,
            inodes,
            path_to_inode,
            next_ino: AtomicU64::new(2),
            dir_snapshots: DashMap::new(),
            next_fh: AtomicU64::new(1),
            file_cache: DashMap::new(),
            l0_caches: DashMap::new(),
        }
    }

    pub fn mount_config() -> fuser::Config {
        let mut config = fuser::Config::default();
        config.mount_options = vec![MountOption::RO, MountOption::FSName("omnifs".to_string())];
        config
    }

    fn runtime_for_mount(&self, mount: &str) -> Option<Arc<CalloutRuntime>> {
        self.registry.get(mount).cloned()
    }

    /// Get or lazily create the L0 cache for a mount.
    fn l0_for_mount(&self, mount: &str) -> Ref<'_, String, BrowseCacheL0> {
        self.l0_caches.entry(mount.to_string()).or_default();
        self.l0_caches.get(mount).unwrap()
    }

    fn l0_get(
        &self,
        mount: &str,
        inode: u64,
        kind: RecordKind,
        aux: Option<String>,
    ) -> Option<std::sync::Arc<CacheRecord>> {
        let l0 = self.l0_for_mount(mount);
        l0.get(&L0Key::new(inode, kind, aux))
    }

    fn l0_put(
        &self,
        mount: &str,
        inode: u64,
        kind: RecordKind,
        aux: Option<String>,
        record: CacheRecord,
    ) {
        let l0 = self.l0_for_mount(mount);
        l0.put(L0Key::new(inode, kind, aux), record);
    }

    /// Drain pending invalidation prefixes from the runtime and evict
    /// matching L0 cache entries. Coarse: iterates all inodes for the mount
    /// and evicts any whose path starts with an invalidated prefix.
    fn drain_and_evict_l0(&self, mount: &str) {
        let Some(runtime) = self.runtime_for_mount(mount) else {
            return;
        };
        let prefixes = runtime.drain_invalidated_prefixes();
        let paths = runtime.drain_invalidated_paths();
        if prefixes.is_empty() && paths.is_empty() {
            return;
        }
        let Some(l0) = self.l0_caches.get(mount) else {
            return;
        };
        // Collect (inode, path) pairs for this mount, then evict matches.
        let mount_inodes: Vec<(u64, String)> = self
            .inodes
            .iter()
            .filter(|entry| entry.value().mount_name == mount)
            .map(|entry| (*entry.key(), entry.value().path.clone()))
            .collect();

        for (ino, path) in &mount_inodes {
            for prefix in &prefixes {
                if path_prefix_matches(prefix, path) {
                    // Evict all record kinds for this inode (aux=None).
                    for kind in [
                        RecordKind::Lookup,
                        RecordKind::Attr,
                        RecordKind::Dirents,
                        RecordKind::File,
                    ] {
                        l0.invalidate(&L0Key::new(*ino, kind, None));
                    }
                    // Remove from path_to_inode so lookup's early dedup
                    // return cannot serve stale metadata.
                    // Do NOT remove from self.inodes: live FUSE handles
                    // (getattr, open, read) reference inodes by number and
                    // would get ENOENT. The inode stays alive but will be
                    // re-resolved on next lookup via the provider.
                    self.path_to_inode
                        .remove(&PathKey::new(mount, path.clone()));
                    break;
                }
            }

            if paths.iter().any(|invalidated| invalidated == path) {
                for kind in [
                    RecordKind::Lookup,
                    RecordKind::Attr,
                    RecordKind::Dirents,
                    RecordKind::File,
                ] {
                    l0.invalidate(&L0Key::new(*ino, kind, None));
                }
                self.path_to_inode
                    .remove(&PathKey::new(mount, path.clone()));
            }
        }

        // Second pass: for each invalidated path, evict the *parent's*
        // lookup(aux=child_name) entry in L0. This is the key correctness
        // fix: lookup checks L0Key(parent_ino, Lookup, Some(child_name))
        // before calling the provider. If "a/b" is invalidated, we must
        // evict L0Key(ino_of_a, Lookup, Some("b")).
        for prefix in &prefixes {
            for pto_entry in self.path_to_inode.as_ref() {
                let key = pto_entry.key();
                if key.mount != mount {
                    continue;
                }
                // Check if this path is a child directly under the invalidated prefix.
                if let Some(remainder) = key.path.strip_prefix(prefix)
                    && !remainder.contains('/')
                    && !remainder.is_empty()
                {
                    // pto_path is a direct child. Its parent's path is prefix
                    // (stripped of trailing slash). Find parent inode.
                    let parent_path = prefix.trim_end_matches('/');
                    if let Some(parent_ino_ref) =
                        self.path_to_inode.get(&PathKey::new(mount, parent_path))
                    {
                        let parent_ino = *parent_ino_ref;
                        drop(parent_ino_ref);
                        l0.invalidate(&L0Key::new(
                            parent_ino,
                            RecordKind::Lookup,
                            Some(remainder.to_string()),
                        ));
                    }
                }
            }
        }

        for path in &paths {
            let Some((parent_path, child_name)) = path.rsplit_once('/') else {
                continue;
            };
            if let Some(parent_ino_ref) = self.path_to_inode.get(&PathKey::new(mount, parent_path))
            {
                let parent_ino = *parent_ino_ref;
                drop(parent_ino_ref);
                l0.invalidate(&L0Key::new(
                    parent_ino,
                    RecordKind::Lookup,
                    Some(child_name.to_string()),
                ));
            }
        }
    }

    fn attr_for_kind(&self, ino: u64, kind: EntryKind, size: u64) -> FileAttr {
        match kind {
            EntryKind::Directory => self.dir_attr(ino),
            EntryKind::File => self.file_attr(ino, size),
        }
    }

    /// Resolve a deserialized `LookupPayload` into a `FileAttr` (positive)
    /// or `Errno::ENOENT` (negative), emitting a cache-hit trace with the
    /// given `tier` label.
    fn resolve_lookup_hit(
        &self,
        mount_name: &str,
        child_path: &str,
        lookup: &cache::LookupPayload,
        tier: &str,
    ) -> Result<FileAttr, Errno> {
        match lookup {
            cache::LookupPayload::Negative => {
                tracing::debug!(target: "omnifs_cache", kind = "negative_hit", op = "lookup", mount = mount_name, "negative cache hit");
                Err(Errno::ENOENT)
            },
            cache::LookupPayload::Positive { kind, size } => {
                tracing::debug!(target: "omnifs_cache", kind = tier, op = "lookup", mount = mount_name, "cache hit");
                let ek = EntryKind::from(*kind);
                let ino = self.get_or_alloc_ino(mount_name, child_path, ek, *size);
                Ok(self.attr_for_kind(ino, ek, *size))
            },
        }
    }

    /// Check L0/L2 caches and the path→inode dedup table for a lookup.
    ///
    /// Returns `Ok(Some(attr))` on a positive hit, `Ok(None)` on a miss,
    /// or `Err(Errno)` on a negative hit or missing runtime.
    fn lookup_check_caches(
        &self,
        mount_name: &str,
        parent_ino: u64,
        child_path: &str,
        name_str: &str,
    ) -> Result<Option<FileAttr>, Errno> {
        // Dirents-implied negative: if parent dirents are cached and
        // exhaustive, trust the cache.
        if let Some(record) = self.l0_get(mount_name, parent_ino, RecordKind::Dirents, None)
            && let Some(dirents) = cache::DirentsPayload::deserialize(&record.payload)
            && dirents.exhaustive
        {
            if let Some(dirent) = dirents.entries.iter().find(|e| e.name == name_str) {
                // Dirents-implied positive: the listing is authoritative
                // and contains this name. Answer stat from the dirent's
                // kind+size without a provider round-trip. This matters
                // for projected sibling files: the provider's list
                // enumerated them but there is no dedicated lookup
                // handler at the per-child path.
                let size = dirent.size;
                let kind: EntryKind = dirent.kind.into();
                let ino = self.get_or_alloc_ino(mount_name, child_path, kind, size);
                return Ok(Some(self.attr_for_kind(ino, kind, size)));
            }
            return Err(Errno::ENOENT);
        }

        // L0: check cached lookup by parent inode + child name.
        if let Some(record) = self.l0_get(
            mount_name,
            parent_ino,
            RecordKind::Lookup,
            Some(name_str.to_string()),
        ) && let Some(lookup) = cache::LookupPayload::deserialize(&record.payload)
        {
            return self
                .resolve_lookup_hit(mount_name, child_path, &lookup, "l0_hit")
                .map(Some);
        }

        // L2: check cached lookup by path (needs runtime).
        let Some(runtime) = self.runtime_for_mount(mount_name) else {
            return Err(Errno::ENOENT);
        };
        if let Some(record) = runtime.cache_get(child_path, RecordKind::Lookup)
            && let Some(lookup) = cache::LookupPayload::deserialize(&record.payload)
        {
            self.l0_put(
                mount_name,
                parent_ino,
                RecordKind::Lookup,
                Some(name_str.to_string()),
                record.clone(),
            );
            return self
                .resolve_lookup_hit(mount_name, child_path, &lookup, "l2_hit")
                .map(Some);
        }

        // Drain invalidations and check the dedup table.
        self.drain_and_evict_l0(mount_name);
        let child_key = PathKey::new(mount_name, child_path);
        if let Some(ino_ref) = self.path_to_inode.get(&child_key) {
            let ino = *ino_ref;
            drop(ino_ref);
            if let Some(entry) = self.inodes.get(&ino) {
                return Ok(Some(self.attr_for_kind(ino, entry.kind, entry.size)));
            }
        }

        Ok(None)
    }

    /// Perform a provider-delegated lookup and write results through to caches.
    fn lookup_via_provider(
        &self,
        runtime: &Arc<CalloutRuntime>,
        mount_name: &str,
        parent_ino: u64,
        parent_path: &str,
        child_path: &str,
        name_str: &str,
    ) -> Result<FileAttr, Errno> {
        match self
            .rt
            .block_on(runtime.call_lookup_child(parent_path, name_str))
        {
            Ok(OpResult::Lookup(LookupResult::Subtree(tree_ref))) => {
                let Some(real_root) = runtime.resolve_tree_ref(tree_ref) else {
                    return Err(Errno::EIO);
                };
                let ino = self.get_or_alloc_ino_backing(
                    mount_name,
                    child_path,
                    EntryKind::Directory,
                    0,
                    real_root,
                );
                Ok(self.dir_attr(ino))
            },
            Ok(OpResult::Lookup(LookupResult::Entry(entry))) => {
                tracing::debug!(
                    target: "omnifs_lookup",
                    path = child_path,
                    siblings_count = entry.siblings.len(),
                    sibling_files_count = entry.sibling_files.len(),
                    "received Lookup entry"
                );

                let size = entry.target.size.unwrap_or(0);
                let kind = entry.target.kind;
                let ino = self.get_or_alloc_ino(mount_name, child_path, kind, size);
                let kind_cache = cache::EntryKindCache::from(kind);
                let payload = cache::LookupPayload::Positive {
                    kind: kind_cache,
                    size,
                };
                if let Some(encoded) = payload.serialize() {
                    let record = CacheRecord::new(RecordKind::Lookup, encoded);
                    runtime.cache_put(child_path, RecordKind::Lookup, &record);
                    self.l0_put(
                        mount_name,
                        parent_ino,
                        RecordKind::Lookup,
                        Some(name_str.to_string()),
                        record,
                    );
                }
                Ok(self.attr_for_kind(ino, kind, size))
            },
            Ok(OpResult::Lookup(LookupResult::NotFound)) => {
                let neg = cache::LookupPayload::Negative;
                if let Some(encoded) = neg.serialize() {
                    let record = CacheRecord::new(RecordKind::Lookup, encoded);
                    runtime.cache_put(child_path, RecordKind::Lookup, &record);
                    self.l0_put(
                        mount_name,
                        parent_ino,
                        RecordKind::Lookup,
                        Some(name_str.to_string()),
                        record,
                    );
                }
                Err(Errno::ENOENT)
            },
            Ok(OpResult::Err(error)) => {
                tracing::warn!(
                    path = child_path,
                    kind = ?error.kind,
                    retryable = error.retryable,
                    message = error.message,
                    "provider returned typed error for lookup_child"
                );
                Err(provider_errno(&error))
            },
            Ok(other) => {
                tracing::warn!(
                    path = child_path,
                    result = ?other,
                    "lookup_child returned unexpected result"
                );
                Err(Errno::EIO)
            },
            Err(e) => {
                tracing::warn!(
                    path = child_path,
                    error = %e,
                    "lookup_child runtime error"
                );
                Err(Errno::EIO)
            },
        }
    }

    /// Build a directory snapshot by reading the real filesystem.
    fn snapshot_from_fs(
        &self,
        mount_name: &str,
        path: &str,
        rp: &Path,
    ) -> Result<DirSnapshot, Errno> {
        let read_dir = std::fs::read_dir(rp).map_err(|_| Errno::EIO)?;
        let mut snapshot = Vec::new();
        for dir_entry in read_dir.flatten() {
            let fname = dir_entry.file_name();
            let Some(fname_str) = fname.to_str() else {
                continue;
            };
            let child_rp = dir_entry.path();
            let Ok(meta) = std::fs::symlink_metadata(&child_rp) else {
                continue;
            };
            let kind = if meta.is_dir() {
                EntryKind::Directory
            } else {
                EntryKind::File
            };
            let child_path = if path.is_empty() {
                fname_str.to_string()
            } else {
                format!("{path}/{fname_str}")
            };
            let child_ino =
                self.get_or_alloc_ino_backing(mount_name, &child_path, kind, meta.len(), child_rp);
            snapshot.push((child_ino, fname_str.to_string(), kind));
        }
        Ok(snapshot)
    }

    /// Build a directory snapshot from cached dirent records.
    fn snapshot_from_dirents(
        &self,
        mount_name: &str,
        path: &str,
        dirents: &cache::DirentsPayload,
    ) -> DirSnapshot {
        dirents
            .entries
            .iter()
            .map(|e| {
                let child_path = if path.is_empty() {
                    e.name.clone()
                } else {
                    format!("{path}/{}", e.name)
                };
                let ek = EntryKind::from(e.kind);
                let child_ino = self.get_or_alloc_ino(mount_name, &child_path, ek, e.size);
                (child_ino, e.name.clone(), ek)
            })
            .collect()
    }

    /// Check L0/L2 caches for directory entries.
    ///
    /// Returns `Ok(Some(snapshot))` on a hit, `Ok(None)` on a miss,
    /// or `Err(Errno)` when no runtime is available for the mount.
    fn opendir_check_caches(
        &self,
        mount_name: &str,
        ino: u64,
        path: &str,
    ) -> Result<Option<DirSnapshot>, Errno> {
        // Only serve readdir from cache when the Dirents record was
        // marked exhaustive. Non-exhaustive records represent a partial
        // prefetched snapshot (e.g., from a Lookup that merged route-
        // derived structural children) and must NOT be returned as the
        // authoritative listing — the host has to fall through to
        // list_children so the provider enumerates the full set.
        if let Some(record) = self.l0_get(mount_name, ino, RecordKind::Dirents, None)
            && let Some(dirents) = cache::DirentsPayload::deserialize(&record.payload)
            && dirents.exhaustive
        {
            tracing::debug!(target: "omnifs_cache", kind = "l0_hit", op = "opendir", mount = mount_name, "cache hit");
            return Ok(Some(self.snapshot_from_dirents(mount_name, path, &dirents)));
        }

        // L2
        if let Some(runtime) = self.runtime_for_mount(mount_name) {
            if let Some(record) = runtime.cache_get(path, RecordKind::Dirents)
                && let Some(dirents) = cache::DirentsPayload::deserialize(&record.payload)
                && dirents.exhaustive
            {
                tracing::debug!(target: "omnifs_cache", kind = "l2_hit", op = "opendir", mount = mount_name, "cache hit");
                self.l0_put(mount_name, ino, RecordKind::Dirents, None, record.clone());
                return Ok(Some(self.snapshot_from_dirents(mount_name, path, &dirents)));
            }
        } else {
            return Err(Errno::ENOENT);
        }

        Ok(None)
    }

    /// List directory entries through the provider and cache the result.
    ///
    /// Subtree handoff folds into the `List(ListResult::Subtree(..))`
    /// variant returned from the provider; there is no separate
    /// materialize step.
    fn opendir_via_provider(
        &self,
        runtime: &Arc<CalloutRuntime>,
        mount_name: &str,
        ino: u64,
        path: &str,
    ) -> Result<DirSnapshot, Errno> {
        match self.rt.block_on(runtime.call_list_children(path)) {
            Ok(OpResult::List(ListResult::Subtree(tree_ref))) => {
                let Some(real_root) = runtime.resolve_tree_ref(tree_ref) else {
                    return Err(Errno::EIO);
                };
                if let Some(mut entry) = self.inodes.get_mut(&ino)
                    && entry.backing_path.is_none()
                {
                    entry.backing_path = Some(real_root.clone());
                }
                self.snapshot_from_fs(mount_name, path, &real_root)
            },
            Ok(OpResult::List(ListResult::Entries(listing))) => {
                let dir_entries = &listing.entries;
                let mut snapshot = Vec::with_capacity(dir_entries.len());
                let mut dirent_records = Vec::with_capacity(dir_entries.len());
                for e in dir_entries {
                    let child_path = if path.is_empty() {
                        e.name.clone()
                    } else {
                        format!("{path}/{}", e.name)
                    };
                    let size = e.size.unwrap_or(0);
                    let child_ino = self.get_or_alloc_ino(mount_name, &child_path, e.kind, size);
                    snapshot.push((child_ino, e.name.clone(), e.kind));
                    dirent_records.push(cache::DirentRecord {
                        name: e.name.clone(),
                        kind: cache::EntryKindCache::from(e.kind),
                        size,
                    });
                }
                let dirents_payload = cache::DirentsPayload {
                    entries: dirent_records,
                    exhaustive: listing.exhaustive,
                };
                if let Some(encoded) = dirents_payload.serialize() {
                    let dirents_record = CacheRecord::new(RecordKind::Dirents, encoded);
                    runtime.cache_put(path, RecordKind::Dirents, &dirents_record);
                    self.l0_put(mount_name, ino, RecordKind::Dirents, None, dirents_record);
                }
                Ok(snapshot)
            },
            Ok(OpResult::Err(error)) => {
                tracing::warn!(
                    path,
                    kind = ?error.kind,
                    retryable = error.retryable,
                    message = error.message,
                    "provider returned typed error for list_children"
                );
                Err(provider_errno(&error))
            },
            Ok(other) => {
                tracing::warn!(
                    path,
                    result = ?other,
                    "list_children returned unexpected result"
                );
                Err(Errno::EIO)
            },
            Err(e) => {
                tracing::warn!(
                    path,
                    error = %e,
                    "list_children runtime error"
                );
                Err(Errno::EIO)
            },
        }
    }
}

impl Filesystem for FuseFs {
    fn lookup(&self, _req: &Request, parent: INodeNo, name: &OsStr, reply: ReplyEntry) {
        let Some(name_str) = name.to_str() else {
            reply.error(Errno::EINVAL);
            return;
        };
        let _trace = FuseTrace::new("lookup", format!("parent={} name={}", parent.0, name_str));
        let _span =
            tracing::debug_span!("fuse::lookup", parent = parent.0, name = name_str).entered();

        // Synthetic root (no root_mount): mount points are children.
        if parent.0 == ROOT_INO && self.registry.root_mount_name().is_none() {
            if self.registry.get(name_str).is_some() {
                let ino = self.get_or_alloc_ino(name_str, "", EntryKind::Directory, 0);
                reply.entry(&TTL, &self.dir_attr(ino), Generation(0));
                return;
            }
            reply.error(Errno::ENOENT);
            return;
        }

        let Some(parent_entry) = self.inodes.get(&parent.0) else {
            reply.error(Errno::ENOENT);
            return;
        };
        let mount_name = parent_entry.mount_name.clone();
        let parent_path = parent_entry.path.clone();
        let parent_backing_path = parent_entry.backing_path.clone();
        drop(parent_entry);

        let child_path = if parent_path.is_empty() {
            name_str.to_string()
        } else {
            format!("{parent_path}/{name_str}")
        };

        // If the parent has a backing path, resolve the child from the filesystem.
        if let Some(ref parent_rp) = parent_backing_path {
            let child_rp = parent_rp.join(name_str);
            match std::fs::symlink_metadata(&child_rp) {
                Ok(meta) => {
                    let kind = if meta.is_dir() {
                        EntryKind::Directory
                    } else {
                        EntryKind::File
                    };
                    let ino = self.get_or_alloc_ino_backing(
                        &mount_name,
                        &child_path,
                        kind,
                        meta.len(),
                        child_rp,
                    );
                    reply.entry(&TTL, &self.attr_from_metadata(ino, &meta), Generation(0));
                },
                Err(e) => {
                    tracing::warn!(path = ?child_rp, err = %e, "backing fs error");
                    reply.error(Errno::ENOENT);
                },
            }
            return;
        }

        // L0/L2 cache path.
        match self.lookup_check_caches(&mount_name, parent.0, &child_path, name_str) {
            Ok(Some(attr)) => {
                reply.entry(&TTL, &attr, Generation(0));
                return;
            },
            Err(e) => {
                reply.error(e);
                return;
            },
            Ok(None) => {},
        }

        let Some(runtime) = self.runtime_for_mount(&mount_name) else {
            reply.error(Errno::ENOENT);
            return;
        };

        tracing::debug!(target: "omnifs_cache", kind = "miss", op = "lookup", mount = mount_name.as_str(), "cache miss");

        match self.lookup_via_provider(
            &runtime,
            &mount_name,
            parent.0,
            &parent_path,
            &child_path,
            name_str,
        ) {
            Ok(attr) => reply.entry(&TTL, &attr, Generation(0)),
            Err(e) => reply.error(e),
        }
    }

    fn getattr(&self, _req: &Request, ino: INodeNo, _fh: Option<FuseFileHandle>, reply: ReplyAttr) {
        let _trace = FuseTrace::new("getattr", format!("ino={}", ino.0));
        if ino.0 == ROOT_INO {
            reply.attr(&TTL, &self.dir_attr(ROOT_INO));
            return;
        }

        let Some(entry) = self.inodes.get(&ino.0) else {
            reply.error(Errno::ENOENT);
            return;
        };

        // Passthrough for inodes with backing_path.
        if let Some(ref rp) = entry.backing_path {
            match std::fs::symlink_metadata(rp) {
                Ok(meta) => {
                    let attr = self.attr_from_metadata(ino.0, &meta);
                    reply.attr(&TTL, &attr);
                },
                Err(e) => {
                    tracing::warn!(path = ?rp, err = %e, "backing fs error");
                    reply.error(Errno::ENOENT);
                },
            }
            return;
        }

        let attr = match entry.kind {
            EntryKind::Directory => self.dir_attr(ino.0),
            EntryKind::File => self.file_attr(ino.0, entry.size),
        };
        reply.attr(&TTL, &attr);
    }

    fn opendir(&self, _req: &Request, ino: INodeNo, _flags: OpenFlags, reply: ReplyOpen) {
        let _trace = FuseTrace::new("opendir", format!("ino={}", ino.0));
        let _span = tracing::debug_span!("fuse::opendir", inode = ino.0).entered();

        let fh = self.alloc_fh();

        // Synthetic root (no root_mount): list mount points.
        if ino.0 == ROOT_INO && self.registry.root_mount_name().is_none() {
            let mounts = self.registry.mounts();
            let mut entries = Vec::new();
            for m in mounts {
                let child_ino = self.get_or_alloc_ino(&m, "", EntryKind::Directory, 0);
                entries.push((child_ino, m, EntryKind::Directory));
            }
            self.dir_snapshots.insert(fh, entries);
            reply.opened(FuseFileHandle(fh), FopenFlags::empty());
            return;
        }

        let Some(inode_entry) = self.inodes.get(&ino.0) else {
            reply.error(Errno::ENOENT);
            return;
        };
        let mount_name = inode_entry.mount_name.clone();
        let path = inode_entry.path.clone();
        let backing_path = inode_entry.backing_path.clone();
        drop(inode_entry);

        // Passthrough for inodes with backing_path.
        if let Some(ref rp) = backing_path {
            match self.snapshot_from_fs(&mount_name, &path, rp) {
                Ok(snapshot) => {
                    self.dir_snapshots.insert(fh, snapshot);
                    reply.opened(FuseFileHandle(fh), FopenFlags::empty());
                },
                Err(e) => reply.error(e),
            }
            return;
        }

        // L0/L2 cache path.
        match self.opendir_check_caches(&mount_name, ino.0, &path) {
            Ok(Some(snapshot)) => {
                self.dir_snapshots.insert(fh, snapshot);
                reply.opened(FuseFileHandle(fh), FopenFlags::empty());
                return;
            },
            Err(e) => {
                reply.error(e);
                return;
            },
            Ok(None) => {},
        }

        self.drain_and_evict_l0(&mount_name);

        let Some(runtime) = self.runtime_for_mount(&mount_name) else {
            reply.error(Errno::ENOENT);
            return;
        };

        tracing::debug!(target: "omnifs_cache", kind = "miss", op = "opendir", mount = mount_name.as_str(), "cache miss");

        match self.opendir_via_provider(&runtime, &mount_name, ino.0, &path) {
            Ok(snapshot) => {
                self.dir_snapshots.insert(fh, snapshot);
                reply.opened(FuseFileHandle(fh), FopenFlags::empty());
            },
            Err(e) => reply.error(e),
        }
    }

    fn readdir(
        &self,
        _req: &Request,
        _ino: INodeNo,
        fh: FuseFileHandle,
        offset: u64,
        mut reply: ReplyDirectory,
    ) {
        let _trace = FuseTrace::new("readdir", format!("fh={} offset={}", fh.0, offset));
        let Some(snapshot) = self.dir_snapshots.get(&fh.0) else {
            reply.error(Errno::EBADF);
            return;
        };

        #[allow(clippy::cast_possible_truncation)]
        let skip = offset as usize;
        for (index, (ino, name, kind)) in snapshot.iter().enumerate().skip(skip) {
            let ftype = match kind {
                EntryKind::Directory => fuser::FileType::Directory,
                EntryKind::File => fuser::FileType::RegularFile,
            };
            let buffer_full = reply.add(INodeNo(*ino), (index + 1) as u64, ftype, name.as_str());
            if buffer_full {
                break;
            }
        }
        reply.ok();
    }

    fn releasedir(
        &self,
        _req: &Request,
        _ino: INodeNo,
        fh: FuseFileHandle,
        _flags: OpenFlags,
        reply: ReplyEmpty,
    ) {
        let _trace = FuseTrace::new("releasedir", format!("fh={}", fh.0));
        self.dir_snapshots.remove(&fh.0);
        reply.ok();
    }

    fn read(
        &self,
        _req: &Request,
        ino: INodeNo,
        fh: FuseFileHandle,
        offset: u64,
        size: u32,
        _flags: OpenFlags,
        _lock_owner: Option<LockOwner>,
        reply: ReplyData,
    ) {
        let _trace = FuseTrace::new(
            "read",
            format!("ino={} fh={} offset={} size={}", ino.0, fh.0, offset, size),
        );
        let _span = tracing::debug_span!("fuse::read", inode = ino.0, offset, size).entered();

        // Serve from cache if this file handle already has data.
        if let Some(cached) = self.file_cache.get(&fh.0) {
            reply.data(data_slice(&cached, offset, size));
            return;
        }

        let Some(inode_entry) = self.inodes.get(&ino.0) else {
            reply.error(Errno::ENOENT);
            return;
        };
        let mount_name = inode_entry.mount_name.clone();
        let path = inode_entry.path.clone();
        let backing_path = inode_entry.backing_path.clone();
        drop(inode_entry);

        // L0: check cached file by inode.
        if let Some(record) = self.l0_get(&mount_name, ino.0, RecordKind::File, None) {
            tracing::debug!(target: "omnifs_cache", kind = "l0_hit", op = "read", mount = mount_name.as_str(), "cache hit");
            reply.data(data_slice(&record.payload, offset, size));
            self.file_cache.insert(fh.0, record.payload.clone());
            return;
        }

        // L2: check cached file by path (only for non-passthrough).
        if backing_path.is_none()
            && let Some(runtime) = self.runtime_for_mount(&mount_name)
            && let Some(record) = runtime.cache_get(&path, RecordKind::File)
        {
            tracing::debug!(target: "omnifs_cache", kind = "l2_hit", op = "read", mount = mount_name.as_str(), "cache hit");
            let data = record.payload.clone();
            self.l0_put(&mount_name, ino.0, RecordKind::File, None, record.clone());
            reply.data(data_slice(&data, offset, size));
            self.file_cache.insert(fh.0, data);
            return;
        }

        // Passthrough for inodes with backing_path.
        if let Some(ref rp) = backing_path {
            match std::fs::read(rp) {
                Ok(data) => {
                    reply.data(data_slice(&data, offset, size));
                    self.file_cache.insert(fh.0, data);
                },
                Err(e) => {
                    tracing::warn!(path = ?rp, err = %e, "backing fs error");
                    reply.error(Errno::EIO);
                },
            }
            return;
        }

        let Some(runtime) = self.runtime_for_mount(&mount_name) else {
            reply.error(Errno::ENOENT);
            return;
        };

        self.drain_and_evict_l0(&mount_name);

        tracing::debug!(target: "omnifs_cache", kind = "miss", op = "read", mount = mount_name.as_str(), "cache miss");

        match self.rt.block_on(runtime.call_read_file(&path)) {
            Ok(OpResult::Read(result)) => {
                tracing::debug!(
                    target: "omnifs_read",
                    path = path,
                    content_len = result.content.len(),
                    sibling_files_count = result.sibling_files.len(),
                    "received Read result"
                );
                let data = result.content;
                let file_record = CacheRecord::new(RecordKind::File, data.clone());
                if let Some(rt) = self.runtime_for_mount(&mount_name) {
                    rt.cache_put(&path, RecordKind::File, &file_record);
                }
                self.l0_put(&mount_name, ino.0, RecordKind::File, None, file_record);
                reply.data(data_slice(&data, offset, size));
                self.file_cache.insert(fh.0, data);
            },
            Ok(OpResult::Err(error)) => {
                tracing::warn!(
                    path,
                    kind = ?error.kind,
                    retryable = error.retryable,
                    message = error.message,
                    "provider returned typed error for read_file"
                );
                reply.error(provider_errno(&error));
            },
            Ok(other) => {
                tracing::warn!(path, result = ?other, "read_file returned unexpected result");
                reply.error(Errno::EIO);
            },
            Err(e) => {
                tracing::warn!(path, error = %e, "read_file runtime error");
                reply.error(Errno::EIO);
            },
        }
    }

    fn open(&self, _req: &Request, _ino: INodeNo, _flags: OpenFlags, reply: ReplyOpen) {
        let _trace = FuseTrace::new("open", String::new());
        let fh = self.alloc_fh();
        reply.opened(FuseFileHandle(fh), FopenFlags::empty());
    }

    fn release(
        &self,
        _req: &Request,
        _ino: INodeNo,
        fh: FuseFileHandle,
        _flags: OpenFlags,
        _lock_owner: Option<LockOwner>,
        _flush: bool,
        reply: ReplyEmpty,
    ) {
        let _trace = FuseTrace::new("release", format!("fh={}", fh.0));
        self.file_cache.remove(&fh.0);
        reply.ok();
    }

    fn readlink(&self, _req: &Request, ino: INodeNo, reply: ReplyData) {
        let _trace = FuseTrace::new("readlink", format!("ino={}", ino.0));
        let Some(entry) = self.inodes.get(&ino.0) else {
            reply.error(Errno::ENOENT);
            return;
        };
        if let Some(ref rp) = entry.backing_path {
            match std::fs::read_link(rp) {
                Ok(target) => reply.data(target.as_os_str().as_encoded_bytes()),
                Err(e) => {
                    tracing::warn!(path = ?rp, err = %e, "backing fs error");
                    reply.error(Errno::EIO);
                },
            }
        } else {
            reply.error(Errno::EINVAL);
        }
    }
}

impl From<cache::EntryKindCache> for EntryKind {
    fn from(kind: cache::EntryKindCache) -> Self {
        match kind {
            cache::EntryKindCache::Directory => Self::Directory,
            cache::EntryKindCache::File => Self::File,
        }
    }
}

// NOTE: `impl From<EntryKind> for cache::EntryKindCache` lives in
// `crate::runtime::mod` so it is available regardless of target_os
// (this `fuse` module is Linux-only). Do not duplicate it here.

/// Slice `data` at the given FUSE `offset` and `size`, returning the relevant
/// byte range. Returns an empty slice when `offset` is past the end.
#[allow(clippy::cast_possible_truncation)]
fn data_slice(data: &[u8], offset: u64, size: u32) -> &[u8] {
    let start = offset as usize;
    let end = (start + size as usize).min(data.len());
    if start >= data.len() {
        &[]
    } else {
        &data[start..end]
    }
}
