//! FUSE filesystem implementation.
//!
//! Bridges the omnifs virtual filesystem to the kernel FUSE subsystem.
//! Routes operations to WASM providers. Supports direct filesystem
//! passthrough when providers set backing paths on nodes.

pub(crate) mod inode;

use crate::cache::Key;
use crate::cache::l0::Cache as L0Cache;
use crate::cache::{self, CacheRecord, EntryKindCache, EntryMeta, FilePayload, RecordKind};
use crate::omnifs::provider::types::{
    ErrorKind, ListResult, LookupResult, OpResult, ProviderError,
};
use crate::path_key::{PathKey, PathToInode};
use crate::path_prefix::path_prefix_matches;
use crate::registry::ProviderRegistry;
use crate::runtime::{CalloutRuntime, NotifierHandle};
use dashmap::DashMap;
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

type DirSnapshot = Vec<(u64, String, EntryKindCache)>;

#[derive(Clone)]
struct RangedFileHandle {
    mount_name: String,
    path: String,
    provider_handle: u64,
    attrs: cache::FileAttrsCache,
}

fn join_child_path(parent_path: &str, name: &str) -> String {
    if parent_path.is_empty() {
        name.to_string()
    } else {
        format!("{parent_path}/{name}")
    }
}

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
    notifier: NotifierHandle,
    next_ino: AtomicU64,
    dir_snapshots: DashMap<u64, DirSnapshot>,
    next_fh: AtomicU64,
    /// Caches file content by file handle; populated on first read, evicted on release.
    file_cache: DashMap<u64, Vec<u8>>,
    ranged_handles: DashMap<u64, RangedFileHandle>,
    /// Single owner of the per-mount L0 browse caches (path-keyed, in-memory).
    l0_caches: DashMap<String, L0Cache>,
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
        Self::new_with_path_map_and_notifier(
            rt,
            registry,
            path_to_inode,
            Arc::new(parking_lot::Mutex::new(None)),
        )
    }

    pub fn new_with_path_map_and_notifier(
        rt: Handle,
        registry: Arc<ProviderRegistry>,
        path_to_inode: Arc<PathToInode>,
        notifier: NotifierHandle,
    ) -> Self {
        let inodes = DashMap::new();

        let root_entry = NodeEntry {
            mount_name: registry.root_mount_name().unwrap_or("").to_string(),
            path: String::new(),
            kind: EntryKindCache::Directory,
            attrs: None,
            size: 0,
            backing_path: None,
        };
        inodes.insert(ROOT_INO, root_entry);

        Self {
            rt,
            registry,
            inodes,
            path_to_inode,
            notifier,
            next_ino: AtomicU64::new(2),
            dir_snapshots: DashMap::new(),
            next_fh: AtomicU64::new(1),
            file_cache: DashMap::new(),
            ranged_handles: DashMap::new(),
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

    fn l0_get(&self, mount: &str, path: &str, kind: RecordKind) -> Option<Arc<CacheRecord>> {
        let l0 = self.l0_caches.entry(mount.to_string()).or_default();
        l0.get(&Key::new(path, kind))
    }

    fn l0_get_with_aux(
        &self,
        mount: &str,
        path: &str,
        kind: RecordKind,
        aux: Option<&str>,
    ) -> Option<Arc<CacheRecord>> {
        let l0 = self.l0_caches.entry(mount.to_string()).or_default();
        l0.get(&Key::with_aux(path, kind, aux))
    }

    fn l0_put(&self, mount: &str, path: &str, kind: RecordKind, record: CacheRecord) {
        let l0 = self.l0_caches.entry(mount.to_string()).or_default();
        l0.put(Key::new(path, kind), record);
    }

    fn l0_put_with_aux(
        &self,
        mount: &str,
        path: &str,
        kind: RecordKind,
        aux: Option<String>,
        record: CacheRecord,
    ) {
        let l0 = self.l0_caches.entry(mount.to_string()).or_default();
        l0.put(Key::with_aux(path, kind, aux), record);
    }

    /// Drain pending invalidation prefixes from the runtime and evict
    /// matching L0 cache entries.
    fn drain_and_evict_pending(&self, mount: &str) {
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

        let mut to_remove = Vec::new();
        for entry in self.path_to_inode.iter() {
            let key = entry.key();
            if key.mount != mount {
                continue;
            };
            let path = &key.path;
            let matches_exact = paths.iter().any(|p| p == path);
            let matches_prefix = prefixes
                .iter()
                .any(|prefix| path_prefix_matches(prefix, path));
            if matches_exact || matches_prefix {
                to_remove.push(key.clone());
            }
        }

        for path_key in &to_remove {
            self.path_to_inode.remove(path_key);
        }

        l0.invalidate_entries_if({
            let paths = paths.clone();
            let prefixes = prefixes.clone();
            move |k, _| {
                paths.contains(&k.path)
                    || prefixes
                        .iter()
                        .any(|prefix| path_prefix_matches(prefix, &k.path))
            }
        });
    }

    fn attr_for_kind(&self, ino: u64, kind: EntryKindCache, size: u64) -> FileAttr {
        match kind {
            EntryKindCache::Directory => self.dir_attr(ino),
            EntryKindCache::File => self.file_attr(ino, size),
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
            cache::LookupPayload::Positive(meta) => {
                tracing::debug!(target: "omnifs_cache", kind = tier, op = "lookup", mount = mount_name, "cache hit");
                let ino = self.get_or_alloc_ino_meta(mount_name, child_path, meta.clone());
                Ok(self.attr_for_kind(ino, meta.kind, meta.st_size()))
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
        parent_path: &str,
        name_str: &str,
    ) -> Result<Option<FileAttr>, Errno> {
        let child_path = join_child_path(parent_path, name_str);

        // Dirents-implied negative: if parent dirents are cached and
        // exhaustive, trust the cache.
        if let Some(record) = self.l0_get(mount_name, parent_path, RecordKind::Dirents)
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
                let ino = self.get_or_alloc_ino_meta(mount_name, &child_path, dirent.meta.clone());
                return Ok(Some(self.attr_for_kind(
                    ino,
                    dirent.meta.kind,
                    dirent.meta.st_size(),
                )));
            }
            return Err(Errno::ENOENT);
        }

        // L0: check cached lookup by child path.
        if let Some(record) = self.l0_get(mount_name, &child_path, RecordKind::Lookup)
            && let Some(lookup) = cache::LookupPayload::deserialize(&record.payload)
        {
            return self
                .resolve_lookup_hit(mount_name, &child_path, &lookup, "l0_hit")
                .map(Some);
        }

        // L2: check cached lookup by path (needs runtime).
        let Some(runtime) = self.runtime_for_mount(mount_name) else {
            return Err(Errno::ENOENT);
        };
        if let Some(record) = runtime.cache_get(&child_path, RecordKind::Lookup)
            && let Some(lookup) = cache::LookupPayload::deserialize(&record.payload)
        {
            self.l0_put(mount_name, &child_path, RecordKind::Lookup, record.clone());
            return self
                .resolve_lookup_hit(mount_name, &child_path, &lookup, "l2_hit")
                .map(Some);
        }

        // Drain invalidations and check the dedup table.
        self.drain_and_evict_pending(mount_name);
        let child_key = PathKey::new(mount_name, &child_path);
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
        parent_path: &str,
        name_str: &str,
    ) -> Result<FileAttr, Errno> {
        let child_path = join_child_path(parent_path, name_str);

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
                    &child_path,
                    EntryKindCache::Directory,
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

                let meta = EntryMeta::from(&entry.target.kind);
                let size = meta.st_size();
                let kind = meta.kind;
                let ino = self.get_or_alloc_ino_meta(mount_name, &child_path, meta.clone());
                let payload = cache::LookupPayload::Positive(meta);
                if let Some(encoded) = payload.serialize() {
                    let record = CacheRecord::new(RecordKind::Lookup, encoded);
                    runtime.cache_put(&child_path, RecordKind::Lookup, &record);
                    self.l0_put(mount_name, &child_path, RecordKind::Lookup, record);
                }
                Ok(self.attr_for_kind(ino, kind, size))
            },
            Ok(OpResult::Lookup(LookupResult::NotFound)) => {
                let neg = cache::LookupPayload::Negative;
                if let Some(encoded) = neg.serialize() {
                    let record = CacheRecord::new(RecordKind::Lookup, encoded);
                    runtime.cache_put(&child_path, RecordKind::Lookup, &record);
                    self.l0_put(mount_name, &child_path, RecordKind::Lookup, record);
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
                EntryKindCache::Directory
            } else {
                EntryKindCache::File
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
                let child_ino = self.get_or_alloc_ino_meta(mount_name, &child_path, e.meta.clone());
                (child_ino, e.name.clone(), e.meta.kind)
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
        _ino: u64,
        path: &str,
    ) -> Result<Option<DirSnapshot>, Errno> {
        // Only serve readdir from cache when the Dirents record was
        // marked exhaustive. Non-exhaustive records represent a partial
        // prefetched snapshot (e.g., from a Lookup that merged route-
        // derived structural children) and must NOT be returned as the
        // authoritative listing. The host has to fall through to
        // list_children so the provider enumerates the full set.
        if let Some(record) = self.l0_get(mount_name, path, RecordKind::Dirents)
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
                self.l0_put(mount_name, path, RecordKind::Dirents, record.clone());
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
                    let meta = EntryMeta::from(&e.kind);
                    let child_ino =
                        self.get_or_alloc_ino_meta(mount_name, &child_path, meta.clone());
                    snapshot.push((child_ino, e.name.clone(), meta.kind));
                    dirent_records.push(cache::DirentRecord {
                        name: e.name.clone(),
                        meta,
                    });
                }
                let dirents_payload = cache::DirentsPayload {
                    entries: dirent_records,
                    exhaustive: listing.exhaustive,
                };
                if let Some(encoded) = dirents_payload.serialize() {
                    let dirents_record = CacheRecord::new(RecordKind::Dirents, encoded);
                    runtime.cache_put(path, RecordKind::Dirents, &dirents_record);
                    self.l0_put(mount_name, path, RecordKind::Dirents, dirents_record);
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

    fn read_ranged_handle(
        &self,
        ino: u64,
        ranged: &RangedFileHandle,
        offset: u64,
        size: u32,
        reply: ReplyData,
    ) {
        let Some(runtime) = self.runtime_for_mount(&ranged.mount_name) else {
            reply.error(Errno::ENOENT);
            return;
        };

        match self
            .rt
            .block_on(runtime.call_read_chunk(ranged.provider_handle, offset, size))
        {
            Ok(OpResult::ReadChunk(chunk)) => {
                if chunk.content.len() > size as usize {
                    tracing::warn!(
                        path = ranged.path.as_str(),
                        requested = size,
                        returned = chunk.content.len(),
                        "provider returned oversized ranged chunk"
                    );
                    reply.error(Errno::EIO);
                    return;
                }
                if chunk.eof {
                    let Some(content_len) = u64::try_from(chunk.content.len()).ok() else {
                        reply.error(Errno::EIO);
                        return;
                    };
                    let Some(eof_size) = offset.checked_add(content_len) else {
                        reply.error(Errno::EIO);
                        return;
                    };
                    if let Err(error) = ranged.attrs.validate_observed_size(eof_size) {
                        tracing::warn!(
                            path = ranged.path.as_str(),
                            error,
                            "provider returned ranged EOF that contradicts file attrs"
                        );
                        reply.error(Errno::EIO);
                        return;
                    }
                    if let Some(attrs) = learned_ranged_eof_attrs(ranged.attrs.clone(), eof_size) {
                        self.promote_inode_attrs(ino, attrs);
                    }
                }
                reply.data(&chunk.content);
            },
            Ok(OpResult::Err(error)) => {
                tracing::warn!(
                    path = ranged.path.as_str(),
                    kind = ?error.kind,
                    retryable = error.retryable,
                    message = error.message,
                    "provider returned typed error for read_chunk"
                );
                reply.error(provider_errno(&error));
            },
            Ok(other) => {
                tracing::warn!(path = ranged.path.as_str(), result = ?other, "read_chunk returned unexpected result");
                reply.error(Errno::EIO);
            },
            Err(e) => {
                tracing::warn!(path = ranged.path.as_str(), error = %e, "read_chunk runtime error");
                reply.error(Errno::EIO);
            },
        }
    }

    fn promote_inode_attrs(&self, ino: u64, attrs: cache::FileAttrsCache) {
        if matches!(attrs.stability, cache::StabilityCache::Volatile) {
            return;
        }
        let Some(mut entry) = self.inodes.get_mut(&ino) else {
            return;
        };
        entry.size = attrs.st_size();
        entry.attrs = Some(attrs);
        drop(entry);
        if let Some(notifier) = self.notifier.lock().as_ref()
            && let Err(error) = notifier.inval_inode(INodeNo(ino), 0, 0)
        {
            tracing::debug!(ino, error = %error, "kernel inode attr invalidation failed");
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
                let ino = self.get_or_alloc_ino(name_str, "", EntryKindCache::Directory, 0);
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

        let child_path = join_child_path(&parent_path, name_str);

        // If the parent has a backing path, resolve the child from the filesystem.
        if let Some(ref parent_rp) = parent_backing_path {
            let child_rp = parent_rp.join(name_str);
            match std::fs::symlink_metadata(&child_rp) {
                Ok(meta) => {
                    let kind = if meta.is_dir() {
                        EntryKindCache::Directory
                    } else {
                        EntryKindCache::File
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
        match self.lookup_check_caches(&mount_name, &parent_path, name_str) {
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

        match self.lookup_via_provider(&runtime, &mount_name, &parent_path, name_str) {
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
            EntryKindCache::Directory => self.dir_attr(ino.0),
            EntryKindCache::File => self.file_attr(ino.0, entry.size),
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
                let child_ino = self.get_or_alloc_ino(&m, "", EntryKindCache::Directory, 0);
                entries.push((child_ino, m, EntryKindCache::Directory));
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

        self.drain_and_evict_pending(&mount_name);

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
                EntryKindCache::Directory => fuser::FileType::Directory,
                EntryKindCache::File => fuser::FileType::RegularFile,
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

        if let Some(ranged) = self.ranged_handles.get(&fh.0).map(|entry| entry.clone()) {
            self.read_ranged_handle(ino.0, &ranged, offset, size, reply);
            return;
        }

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
        let attrs = inode_entry.attrs.clone();
        drop(inode_entry);

        if let Some(attrs) = attrs.as_ref()
            && matches!(attrs.size, cache::SizeCache::Exact(0))
        {
            reply.data(&[]);
            return;
        }

        let durable_aux = attrs
            .as_ref()
            .and_then(cache::FileAttrsCache::durable_cache_aux);

        // L0: check cached file by path.
        if let Some(aux) = durable_aux.clone()
            && let Some(record) =
                self.l0_get_with_aux(&mount_name, &path, RecordKind::File, aux.as_deref())
            && let Some(payload) = file_payload_for_attrs(&record, attrs.as_ref())
        {
            tracing::debug!(target: "omnifs_cache", kind = "l0_hit", op = "read", mount = mount_name.as_str(), "cache hit");
            reply.data(data_slice(&payload.content, offset, size));
            self.file_cache.insert(fh.0, payload.content);
            return;
        }

        // L2: check cached file by path (only for non-passthrough).
        if backing_path.is_none()
            && let Some(aux) = durable_aux.clone()
            && let Some(runtime) = self.runtime_for_mount(&mount_name)
            && let Some(record) =
                runtime.cache_get_with_aux(&path, RecordKind::File, aux.as_deref())
            && let Some(payload) = file_payload_for_attrs(&record, attrs.as_ref())
        {
            tracing::debug!(target: "omnifs_cache", kind = "l2_hit", op = "read", mount = mount_name.as_str(), "cache hit");
            let data = payload.content;
            self.l0_put_with_aux(&mount_name, &path, RecordKind::File, aux, record.clone());
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

        self.drain_and_evict_pending(&mount_name);

        tracing::debug!(target: "omnifs_cache", kind = "miss", op = "read", mount = mount_name.as_str(), "cache miss");

        match self.rt.block_on(runtime.call_read_file(&path)) {
            Ok(OpResult::Read(result)) => {
                let Some((data, result_attrs, sibling_count)) =
                    resolve_read_payload(&runtime, &path, result)
                else {
                    reply.error(Errno::EIO);
                    return;
                };
                tracing::debug!(
                    target: "omnifs_read",
                    path = path,
                    content_len = data.len(),
                    sibling_files_count = sibling_count,
                    "received Read result"
                );
                let attrs_cache = learned_full_read_attrs(result_attrs, data.len());
                if !full_read_matches_attrs(&attrs_cache, data.len()) {
                    tracing::warn!(
                        path,
                        expected = ?attrs_cache.size,
                        actual = data.len(),
                        "provider returned bytes that contradict file attrs"
                    );
                    reply.error(Errno::EIO);
                    return;
                }
                self.promote_inode_attrs(ino.0, attrs_cache.clone());
                if let Some(aux) = attrs_cache.durable_cache_aux() {
                    let payload = FilePayload::new(attrs_cache.version_token.clone(), data.clone());
                    let Some(payload) = payload.serialize() else {
                        reply.error(Errno::EIO);
                        return;
                    };
                    let file_record = CacheRecord::new(RecordKind::File, payload);
                    if let Some(rt) = self.runtime_for_mount(&mount_name) {
                        rt.cache_put_with_aux(
                            &path,
                            RecordKind::File,
                            aux.as_deref(),
                            &file_record,
                        );
                    }
                    self.l0_put_with_aux(&mount_name, &path, RecordKind::File, aux, file_record);
                }
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

    fn open(&self, _req: &Request, ino: INodeNo, _flags: OpenFlags, reply: ReplyOpen) {
        let _trace = FuseTrace::new("open", format!("ino={}", ino.0));
        let fh = self.alloc_fh();
        let Some(entry) = self.inodes.get(&ino.0) else {
            reply.error(Errno::ENOENT);
            return;
        };
        let mount_name = entry.mount_name.clone();
        let path = entry.path.clone();
        let backing_path = entry.backing_path.clone();
        let attrs = entry.attrs.clone();
        drop(entry);

        if backing_path.is_none()
            && let Some(attrs) = attrs.as_ref()
            && matches!(
                &attrs.bytes,
                cache::BytesCache::Deferred(cache::ReadModeCache::Ranged)
            )
        {
            let Some(runtime) = self.runtime_for_mount(&mount_name) else {
                reply.error(Errno::ENOENT);
                return;
            };
            match self.rt.block_on(runtime.call_open_file(&path)) {
                Ok(OpResult::OpenFile(opened)) => {
                    let opened_attrs = cache::FileAttrsCache::from(&opened.attrs);
                    if !matches!(
                        &opened_attrs.bytes,
                        cache::BytesCache::Deferred(cache::ReadModeCache::Ranged)
                    ) {
                        reply.error(Errno::EIO);
                        return;
                    }
                    self.promote_inode_attrs(ino.0, opened_attrs.clone());
                    self.ranged_handles.insert(
                        fh,
                        RangedFileHandle {
                            mount_name,
                            path,
                            provider_handle: opened.handle,
                            attrs: opened_attrs,
                        },
                    );
                    reply.opened(FuseFileHandle(fh), FopenFlags::FOPEN_DIRECT_IO);
                    return;
                },
                Ok(OpResult::Err(error)) => {
                    reply.error(provider_errno(&error));
                    return;
                },
                Ok(other) => {
                    tracing::warn!(path, result = ?other, "open_file returned unexpected result");
                    reply.error(Errno::EIO);
                    return;
                },
                Err(e) => {
                    tracing::warn!(path, error = %e, "open_file runtime error");
                    reply.error(Errno::EIO);
                    return;
                },
            }
        }

        let flags = attrs
            .filter(cache::FileAttrsCache::should_direct_io)
            .map_or_else(FopenFlags::empty, |_| FopenFlags::FOPEN_DIRECT_IO);
        reply.opened(FuseFileHandle(fh), flags);
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
        if let Some((_, ranged)) = self.ranged_handles.remove(&fh.0)
            && let Some(runtime) = self.runtime_for_mount(&ranged.mount_name)
            && let Err(e) = runtime.call_close_file(ranged.provider_handle)
        {
            tracing::debug!(
                path = ranged.path,
                error = %e,
                "close_file runtime error"
            );
        }
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

/// Materialize a `read-file` terminal into the bytes the FUSE response
/// will return. Inline content travels in the WIT; blob content gets
/// pulled from the host's blob cache. Returns `None` when a blob-backed
/// payload can't be resolved (logged at warn for diagnostics).
fn resolve_read_payload(
    runtime: &crate::runtime::CalloutRuntime,
    path: &str,
    result: crate::omnifs::provider::types::FileContentResult,
) -> Option<(Vec<u8>, cache::FileAttrsCache, usize)> {
    use crate::omnifs::provider::types::FileContentResult;
    match result {
        FileContentResult::Inline(inline) => {
            let count = inline.sibling_files.len();
            Some((
                inline.content,
                cache::FileAttrsCache::from(&inline.attrs),
                count,
            ))
        },
        FileContentResult::Blob(blob) => match runtime.read_blob_full(blob.blob) {
            Ok(bytes) => Some((
                bytes,
                cache::FileAttrsCache::from(&blob.attrs),
                blob.sibling_files.len(),
            )),
            Err(e) => {
                tracing::warn!(path, error = %e, "blob-backed read failed");
                None
            },
        },
    }
}

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

fn learned_full_read_attrs(
    attrs: cache::FileAttrsCache,
    content_len: usize,
) -> cache::FileAttrsCache {
    if !can_publish_learned_size(&attrs) {
        return attrs;
    }
    match attrs.size {
        cache::SizeCache::Exact(_) => attrs,
        cache::SizeCache::NonZero | cache::SizeCache::Unknown => {
            attrs.with_exact_size(u64::try_from(content_len).unwrap_or(u64::MAX))
        },
    }
}

fn learned_ranged_eof_attrs(
    attrs: cache::FileAttrsCache,
    eof_size: u64,
) -> Option<cache::FileAttrsCache> {
    if !can_publish_learned_size(&attrs) {
        return None;
    }
    match attrs.size {
        cache::SizeCache::Exact(_) => None,
        cache::SizeCache::NonZero | cache::SizeCache::Unknown => {
            Some(attrs.with_exact_size(eof_size))
        },
    }
}

fn can_publish_learned_size(attrs: &cache::FileAttrsCache) -> bool {
    match attrs.stability {
        cache::StabilityCache::Immutable => true,
        cache::StabilityCache::Mutable => attrs.version_token.is_some(),
        cache::StabilityCache::Volatile => false,
    }
}

fn full_read_matches_attrs(attrs: &cache::FileAttrsCache, content_len: usize) -> bool {
    match attrs.size {
        cache::SizeCache::Exact(size) => {
            u64::try_from(content_len).is_ok_and(|content_len| content_len == size)
        },
        cache::SizeCache::NonZero => content_len > 0,
        cache::SizeCache::Unknown => true,
    }
}

fn file_payload_for_attrs(
    record: &CacheRecord,
    attrs: Option<&cache::FileAttrsCache>,
) -> Option<FilePayload> {
    let payload = FilePayload::deserialize(&record.payload)?;
    let attrs = attrs?;
    if matches!(attrs.stability, cache::StabilityCache::Mutable)
        && payload.version_token != attrs.version_token
    {
        return None;
    }
    if !full_read_matches_attrs(attrs, payload.content.len()) {
        return None;
    }
    Some(payload)
}
