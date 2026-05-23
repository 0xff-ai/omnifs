//! FUSE filesystem implementation.
//!
//! Bridges the omnifs virtual filesystem to the kernel FUSE subsystem.
//! Routes operations to WASM providers. Supports direct filesystem
//! passthrough when providers set backing paths on nodes.

pub(crate) mod inode;

use crate::cache::l0::Cache as L0Cache;
use crate::cache::{self, CacheRecord, EntryMeta, FilePayload, RecordKind};
use crate::cache::{FileAttrsCache, Key};
use crate::omnifs::provider::types as wit_types;
use crate::omnifs::provider::types::{
    ErrorKind, ListChildrenResult, LookupChildResult, ProviderError, ReadFileBytes, ReadFileResult,
};
use crate::path_key::{PathKey, PathToInode};
use crate::path_prefix::path_prefix_matches;
use crate::registry::ProviderRegistry;
use crate::runtime::{NotifierHandle, ProviderRuntime, RuntimeError};
use dashmap::DashMap;
use fuser::{
    Errno, FileAttr, FileHandle as FuseFileHandle, Filesystem, FopenFlags, Generation, INodeNo,
    LockOwner, MountOption, OpenFlags, ReplyAttr, ReplyData, ReplyDirectory, ReplyEmpty,
    ReplyEntry, ReplyOpen, Request,
};
use inode::NodeEntry;
use std::ffi::OsStr;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::sync::atomic::AtomicU64;
use std::time::{Duration, Instant};
use tokio::runtime::Handle;
use tracing::{debug, debug_span, info, warn};

/// Kernel-side entry/attr TTL. The host never expires entries on time,
/// only on capacity or explicit invalidation via the FUSE notifier and
/// provider cache-invalidate effects. We still must hand the kernel
/// a finite Duration, so pick one large enough that refresh churn is
/// irrelevant in practice (~136 years).
const TTL: Duration = Duration::from_secs(u32::MAX as u64);
const TTL_DYNAMIC: Duration = Duration::from_secs(0);
const ROOT_INO: u64 = 1;

type DirSnapshot = Vec<(u64, String, wit_types::EntryKind)>;

/// Construct a placeholder `wit_types::EntryKind::File(FileProj)` for FUSE
/// snapshot/inode use where only the kind discriminator matters and no
/// real projection data is available (e.g. from a backing-path read or a
/// pre-projection allocation). The embedded `FileProj` is never inspected;
/// only the variant tag is used for `FileType` resolution.
fn file_kind_placeholder() -> wit_types::EntryKind {
    wit_types::EntryKind::File(wit_types::FileProj {
        attrs: wit_types::FileAttrs {
            size: wit_types::FileSize::Unknown,
            stability: wit_types::Stability::Mutable,
            version_token: None,
        },
        bytes: wit_types::ProjBytes::Deferred(wit_types::ReadMode::Full),
    })
}

#[derive(Clone)]
struct RangedFileHandle {
    mount_name: String,
    path: String,
    provider_handle: u64,
    attrs: FileAttrsCache,
}

fn join_child_path(parent_path: &str, name: &str) -> String {
    if parent_path.is_empty() {
        name.to_string()
    } else {
        format!("{parent_path}/{name}")
    }
}

struct FullReadTarget {
    ino: u64,
    fh: u64,
    mount_name: String,
    path: String,
    backing_path: Option<PathBuf>,
    attrs: Option<FileAttrsCache>,
}

/// Map a provider error to its corresponding FUSE errno.
impl From<&ProviderError> for Errno {
    fn from(error: &ProviderError) -> Self {
        match error.kind {
            ErrorKind::NotFound => Errno::ENOENT,
            ErrorKind::NotADirectory => Errno::ENOTDIR,
            ErrorKind::NotAFile => Errno::EISDIR,
            ErrorKind::PermissionDenied | ErrorKind::Denied => Errno::EACCES,
            ErrorKind::InvalidInput => Errno::EINVAL,
            ErrorKind::TooLarge => Errno::EFBIG,
            ErrorKind::RateLimited => Errno::EAGAIN,
            ErrorKind::Network
            | ErrorKind::Timeout
            | ErrorKind::VersionMismatch
            | ErrorKind::Internal => Errno::EIO,
        }
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
        info!(
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
        _notifier: NotifierHandle,
    ) -> Self {
        let inodes = DashMap::new();

        let root_entry = NodeEntry {
            mount_name: registry.root_mount_name().unwrap_or("").to_string(),
            path: String::new(),
            kind: wit_types::EntryKind::Directory,
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

    fn runtime_for_mount(&self, mount: &str) -> Option<Arc<ProviderRuntime>> {
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
            }
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

    fn attr_for_kind(&self, ino: u64, kind: &wit_types::EntryKind, size: u64) -> FileAttr {
        match kind {
            wit_types::EntryKind::Directory => self.dir_attr(ino),
            wit_types::EntryKind::File(_) => self.file_attr(ino, size),
        }
    }

    fn attr_for_inode_or_meta(
        &self,
        ino: u64,
        fallback_kind: &wit_types::EntryKind,
        fallback_size: u64,
    ) -> FileAttr {
        if let Some(entry) = self.inodes.get(&ino) {
            return self.attr_for_kind(ino, &entry.kind, entry.size);
        }
        self.attr_for_kind(ino, fallback_kind, fallback_size)
    }

    fn ttl_for_attrs(attrs: Option<&FileAttrsCache>) -> Duration {
        let Some(attrs) = attrs else {
            return TTL;
        };
        if !matches!(attrs.size, wit_types::FileSize::Exact(_))
            || !matches!(attrs.stability, wit_types::Stability::Immutable)
        {
            return TTL_DYNAMIC;
        }
        TTL
    }

    fn ttl_for_meta(meta: &EntryMeta) -> Duration {
        Self::ttl_for_attrs(meta.attrs.as_ref())
    }

    fn ttl_for_entry(entry: &NodeEntry) -> Duration {
        Self::ttl_for_attrs(entry.attrs.as_ref())
    }

    /// Resolve a deserialized `LookupPayload` into `FileAttr` plus kernel
    /// TTL (positive) or `Errno::ENOENT` (negative), emitting a cache-hit
    /// trace with the given `tier` label.
    fn resolve_lookup_hit(
        &self,
        mount_name: &str,
        child_path: &str,
        lookup: &cache::LookupPayload,
        tier: &str,
    ) -> Result<(FileAttr, Duration), Errno> {
        match lookup {
            cache::LookupPayload::Negative => {
                debug!(target: "omnifs_cache", kind = "negative_hit", op = "lookup", mount = mount_name, "negative cache hit");
                Err(Errno::ENOENT)
            },
            cache::LookupPayload::Positive(meta) => {
                debug!(target: "omnifs_cache", kind = tier, op = "lookup", mount = mount_name, "cache hit");
                let ino = self.get_or_alloc_ino_meta(mount_name, child_path, meta.clone());
                Ok((
                    self.attr_for_inode_or_meta(ino, &meta.kind, meta.st_size()),
                    Self::ttl_for_meta(meta),
                ))
            },
        }
    }

    /// Check L0/L2 caches and the path→inode dedup table for a lookup.
    ///
    /// Returns `Ok(Some((attr, ttl)))` on a positive hit, `Ok(None)` on a
    /// miss, or `Err(Errno)` on a negative hit or missing runtime.
    fn lookup_check_caches(
        &self,
        mount_name: &str,
        parent_path: &str,
        name_str: &str,
    ) -> Result<Option<(FileAttr, Duration)>, Errno> {
        let child_path = join_child_path(parent_path, name_str);
        self.drain_and_evict_pending(mount_name);

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
                return Ok(Some((
                    self.attr_for_inode_or_meta(ino, &dirent.meta.kind, dirent.meta.st_size()),
                    Self::ttl_for_meta(&dirent.meta),
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
        if let Some(record) = runtime.cache_get(&child_path, RecordKind::Lookup, None)
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
                let attr = self.attr_for_kind(ino, &entry.kind, entry.size);
                let ttl = Self::ttl_for_entry(&entry);
                return Ok(Some((attr, ttl)));
            }
        }

        Ok(None)
    }

    /// Perform a provider-delegated lookup and write results through to caches.
    fn lookup_via_provider(
        &self,
        runtime: &Arc<ProviderRuntime>,
        mount_name: &str,
        parent_path: &str,
        name_str: &str,
    ) -> Result<(FileAttr, Duration), Errno> {
        let child_path = join_child_path(parent_path, name_str);

        match self
            .rt
            .block_on(runtime.lookup_child(parent_path, name_str))
        {
            Ok(LookupChildResult::Subtree(tree_ref)) => {
                let Some(real_root) = runtime.resolve_tree_ref(tree_ref) else {
                    return Err(Errno::EIO);
                };
                let ino = self.get_or_alloc_ino_backing(
                    mount_name,
                    &child_path,
                    wit_types::EntryKind::Directory,
                    0,
                    real_root,
                );
                Ok((self.dir_attr(ino), TTL))
            },
            Ok(LookupChildResult::Entry(entry)) => {
                debug!(
                    target: "omnifs_lookup",
                    path = child_path,
                    siblings_count = entry.siblings.len(),
                    "received Lookup entry"
                );

                let meta = EntryMeta::from(&entry.target.kind);
                let size = meta.st_size();
                let kind = meta.kind.clone();
                let ttl = Self::ttl_for_meta(&meta);
                let ino = self.get_or_alloc_ino_meta(mount_name, &child_path, meta.clone());
                let payload = cache::LookupPayload::Positive(meta);
                if let Some(encoded) = payload.serialize() {
                    let record = CacheRecord::new(RecordKind::Lookup, encoded);
                    runtime.cache_put(&child_path, RecordKind::Lookup, None, &record);
                    self.l0_put(mount_name, &child_path, RecordKind::Lookup, record);
                }
                Ok((self.attr_for_inode_or_meta(ino, &kind, size), ttl))
            },
            Ok(LookupChildResult::NotFound) => {
                let neg = cache::LookupPayload::Negative;
                if let Some(encoded) = neg.serialize() {
                    let record = CacheRecord::new(RecordKind::Lookup, encoded);
                    runtime.cache_put(&child_path, RecordKind::Lookup, None, &record);
                    self.l0_put(mount_name, &child_path, RecordKind::Lookup, record);
                }
                Err(Errno::ENOENT)
            },
            Err(RuntimeError::ProviderError(error)) => {
                warn!(
                    path = child_path,
                    kind = ?error.kind,
                    retryable = error.retryable,
                    message = error.message,
                    "provider returned typed error for lookup_child"
                );
                Err((&error).into())
            },
            Err(error) => {
                warn!(
                    path = child_path,
                    error = %error,
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
                wit_types::EntryKind::Directory
            } else {
                file_kind_placeholder()
            };
            let child_path = if path.is_empty() {
                fname_str.to_string()
            } else {
                format!("{path}/{fname_str}")
            };
            let child_ino = self.get_or_alloc_ino_backing(
                mount_name,
                &child_path,
                kind.clone(),
                meta.len(),
                child_rp,
            );
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
                (child_ino, e.name.clone(), e.meta.kind.clone())
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
        self.drain_and_evict_pending(mount_name);

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
            debug!(target: "omnifs_cache", kind = "l0_hit", op = "opendir", mount = mount_name, "cache hit");
            return Ok(Some(self.snapshot_from_dirents(mount_name, path, &dirents)));
        }

        // L2
        if let Some(runtime) = self.runtime_for_mount(mount_name) {
            if let Some(record) = runtime.cache_get(path, RecordKind::Dirents, None)
                && let Some(dirents) = cache::DirentsPayload::deserialize(&record.payload)
                && dirents.exhaustive
            {
                debug!(target: "omnifs_cache", kind = "l2_hit", op = "opendir", mount = mount_name, "cache hit");
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
    /// Subtree handoff folds into the `ListChildrenResult::Subtree(..)`
    /// variant returned from the provider.
    fn opendir_via_provider(
        &self,
        runtime: &Arc<ProviderRuntime>,
        mount_name: &str,
        ino: u64,
        path: &str,
    ) -> Result<DirSnapshot, Errno> {
        match self.rt.block_on(runtime.list_children(path)) {
            Ok(ListChildrenResult::Subtree(tree_ref)) => {
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
            Ok(ListChildrenResult::Entries(listing)) => {
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
                    snapshot.push((child_ino, e.name.clone(), meta.kind.clone()));
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
                    runtime.cache_put(path, RecordKind::Dirents, None, &dirents_record);
                    self.l0_put(mount_name, path, RecordKind::Dirents, dirents_record);
                }
                Ok(snapshot)
            },
            Err(RuntimeError::ProviderError(error)) => {
                warn!(
                    path,
                    kind = ?error.kind,
                    retryable = error.retryable,
                    message = error.message,
                    "provider returned typed error for list_children"
                );
                Err((&error).into())
            },
            Err(error) => {
                warn!(
                    path,
                    error = %error,
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
            .block_on(runtime.read_chunk(ranged.provider_handle, offset, size))
        {
            Ok(chunk) => {
                if chunk.content.len() > size as usize {
                    warn!(
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
                        warn!(
                            path = ranged.path.as_str(),
                            error, "provider returned ranged EOF that contradicts file attrs"
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
            Err(RuntimeError::ProviderError(error)) => {
                warn!(
                    path = ranged.path.as_str(),
                    kind = ?error.kind,
                    retryable = error.retryable,
                    message = error.message,
                    "provider returned typed error for read_chunk"
                );
                reply.error((&error).into());
            },
            Err(error) => {
                warn!(path = ranged.path.as_str(), error = %error, "read_chunk runtime error");
                reply.error(Errno::EIO);
            },
        }
    }

    fn read_full_handle(
        &self,
        ino: INodeNo,
        fh: FuseFileHandle,
        offset: u64,
        size: u32,
        reply: ReplyData,
    ) {
        let Some(inode_entry) = self.inodes.get(&ino.0) else {
            reply.error(Errno::ENOENT);
            return;
        };
        let target = FullReadTarget {
            ino: ino.0,
            fh: fh.0,
            mount_name: inode_entry.mount_name.clone(),
            path: inode_entry.path.clone(),
            backing_path: inode_entry.backing_path.clone(),
            attrs: inode_entry.attrs.clone(),
        };
        drop(inode_entry);

        self.drain_and_evict_pending(&target.mount_name);

        if let Some(attrs) = target.attrs.as_ref()
            && matches!(attrs.size, wit_types::FileSize::Exact(0))
        {
            reply.data(&[]);
            return;
        }

        let durable_aux = target
            .attrs
            .as_ref()
            .and_then(FileAttrsCache::durable_cache_aux);

        if let Some(aux) = durable_aux.clone()
            && let Some(record) = self.l0_get_with_aux(
                &target.mount_name,
                &target.path,
                RecordKind::File,
                aux.as_deref(),
            )
            && let Some(payload) = file_payload_for_attrs(&record, target.attrs.as_ref())
        {
            debug!(target: "omnifs_cache", kind = "l0_hit", op = "read", mount = target.mount_name.as_str(), "cache hit");
            reply.data(data_slice(&payload.content, offset, size));
            self.file_cache.insert(target.fh, payload.content);
            return;
        }

        if target.backing_path.is_none()
            && let Some(aux) = durable_aux.clone()
            && let Some(runtime) = self.runtime_for_mount(&target.mount_name)
            && let Some(record) = runtime.cache_get(&target.path, RecordKind::File, aux.as_deref())
            && let Some(payload) = file_payload_for_attrs(&record, target.attrs.as_ref())
        {
            debug!(target: "omnifs_cache", kind = "l2_hit", op = "read", mount = target.mount_name.as_str(), "cache hit");
            let data = payload.content;
            self.l0_put_with_aux(
                &target.mount_name,
                &target.path,
                RecordKind::File,
                aux,
                record,
            );
            reply.data(data_slice(&data, offset, size));
            self.file_cache.insert(target.fh, data);
            return;
        }

        if let Some(ref rp) = target.backing_path {
            match std::fs::read(rp) {
                Ok(data) => {
                    reply.data(data_slice(&data, offset, size));
                    self.file_cache.insert(target.fh, data);
                },
                Err(e) => {
                    warn!(path = ?rp, err = %e, "backing fs error");
                    reply.error(Errno::EIO);
                },
            }
            return;
        }

        let Some(runtime) = self.runtime_for_mount(&target.mount_name) else {
            reply.error(Errno::ENOENT);
            return;
        };

        self.drain_and_evict_pending(&target.mount_name);

        debug!(target: "omnifs_cache", kind = "miss", op = "read", mount = target.mount_name.as_str(), "cache miss");

        match self.rt.block_on(runtime.read_file(&target.path)) {
            Ok(result) => {
                self.finish_full_read(&target, &runtime, offset, size, result, reply);
            },
            Err(RuntimeError::ProviderError(error)) => {
                warn!(
                    path = target.path.as_str(),
                    kind = ?error.kind,
                    retryable = error.retryable,
                    message = error.message,
                    "provider returned typed error for read_file"
                );
                reply.error((&error).into());
            },
            Err(error) => {
                warn!(path = target.path.as_str(), error = %error, "read_file runtime error");
                reply.error(Errno::EIO);
            },
        }
    }

    fn finish_full_read(
        &self,
        target: &FullReadTarget,
        runtime: &ProviderRuntime,
        offset: u64,
        size: u32,
        result: ReadFileResult,
        reply: ReplyData,
    ) {
        let Some((data, result_attrs)) = resolve_read_payload(runtime, &target.path, result) else {
            reply.error(Errno::EIO);
            return;
        };
        debug!(
            target: "omnifs_read",
            path = target.path.as_str(),
            content_len = data.len(),
            "received Read result"
        );
        let attrs_cache = learned_full_read_attrs(result_attrs, data.len());
        if !full_read_matches_attrs(&attrs_cache, data.len()) {
            warn!(
                path = target.path.as_str(),
                expected = ?attrs_cache.size,
                actual = data.len(),
                "provider returned bytes that contradict file attrs"
            );
            reply.error(Errno::EIO);
            return;
        }
        self.promote_inode_attrs(target.ino, attrs_cache.clone());
        if self
            .cache_durable_file_payload(&target.mount_name, &target.path, &attrs_cache, &data)
            .is_err()
        {
            reply.error(Errno::EIO);
            return;
        }
        reply.data(data_slice(&data, offset, size));
        self.file_cache.insert(target.fh, data);
    }

    fn cache_durable_file_payload(
        &self,
        mount_name: &str,
        path: &str,
        attrs_cache: &FileAttrsCache,
        data: &[u8],
    ) -> Result<(), Errno> {
        let Some(aux) = attrs_cache.durable_cache_aux() else {
            return Ok(());
        };
        let payload = FilePayload::new(attrs_cache.version_token.clone(), data.to_vec());
        let Some(payload) = payload.serialize() else {
            return Err(Errno::EIO);
        };
        let file_record = CacheRecord::new(RecordKind::File, payload);
        if let Some(rt) = self.runtime_for_mount(mount_name) {
            rt.cache_put(path, RecordKind::File, aux.as_deref(), &file_record);
        }
        self.l0_put_with_aux(mount_name, path, RecordKind::File, aux, file_record);
        Ok(())
    }

    fn open_ranged_file(&self, target: &FullReadTarget) -> Result<Option<FopenFlags>, Errno> {
        if target.backing_path.is_some()
            || !target.attrs.as_ref().is_some_and(|attrs| {
                matches!(
                    &attrs.bytes,
                    wit_types::ProjBytes::Deferred(wit_types::ReadMode::Ranged)
                )
            })
        {
            return Ok(None);
        }

        let Some(runtime) = self.runtime_for_mount(&target.mount_name) else {
            return Err(Errno::ENOENT);
        };
        match self.rt.block_on(runtime.open_file(&target.path)) {
            Ok(opened) => {
                let opened_attrs =
                    opened_file_attrs(&target.path, target.attrs.as_ref(), &opened.attrs)?;
                self.promote_inode_attrs(target.ino, opened_attrs.clone());
                self.ranged_handles.insert(
                    target.fh,
                    RangedFileHandle {
                        mount_name: target.mount_name.clone(),
                        path: target.path.clone(),
                        provider_handle: opened.handle,
                        attrs: opened_attrs,
                    },
                );
                Ok(Some(FopenFlags::FOPEN_DIRECT_IO))
            },
            Err(RuntimeError::ProviderError(error)) => Err((&error).into()),
            Err(error) => {
                warn!(
                    path = target.path.as_str(),
                    error = %error,
                    "open_file runtime error"
                );
                Err(Errno::EIO)
            },
        }
    }

    fn prefetch_full_file_on_open(
        &self,
        target: &FullReadTarget,
    ) -> Result<Option<FopenFlags>, Errno> {
        if target.backing_path.is_some()
            || !target
                .attrs
                .as_ref()
                .is_some_and(should_prefetch_full_on_open)
        {
            return Ok(None);
        }

        let Some(runtime) = self.runtime_for_mount(&target.mount_name) else {
            return Err(Errno::ENOENT);
        };
        self.drain_and_evict_pending(&target.mount_name);
        match self.rt.block_on(runtime.read_file(&target.path)) {
            Ok(result) => {
                let Some((data, result_attrs)) =
                    resolve_read_payload(&runtime, &target.path, result)
                else {
                    return Err(Errno::EIO);
                };
                let attrs_cache = learned_full_read_attrs(result_attrs, data.len());
                if !full_read_matches_attrs(&attrs_cache, data.len()) {
                    warn!(
                        path = target.path.as_str(),
                        expected = ?attrs_cache.size,
                        actual = data.len(),
                        "provider returned bytes that contradict file attrs"
                    );
                    return Err(Errno::EIO);
                }
                self.promote_inode_attrs(target.ino, attrs_cache.clone());
                self.cache_durable_file_payload(
                    &target.mount_name,
                    &target.path,
                    &attrs_cache,
                    &data,
                )?;
                self.file_cache.insert(target.fh, data);
                Ok(Some(FopenFlags::FOPEN_DIRECT_IO))
            },
            Err(RuntimeError::ProviderError(error)) => Err((&error).into()),
            Err(error) => {
                warn!(
                    path = target.path.as_str(),
                    error = %error,
                    "read_file runtime error during open"
                );
                Err(Errno::EIO)
            },
        }
    }

    fn promote_inode_attrs(&self, ino: u64, attrs: FileAttrsCache) {
        if matches!(attrs.stability, wit_types::Stability::Volatile) {
            return;
        }
        let Some(mut entry) = self.inodes.get_mut(&ino) else {
            return;
        };
        entry.size = attrs.st_size();
        entry.attrs = Some(attrs);
    }
}

impl Filesystem for FuseFs {
    fn lookup(&self, _req: &Request, parent: INodeNo, name: &OsStr, reply: ReplyEntry) {
        let Some(name_str) = name.to_str() else {
            reply.error(Errno::EINVAL);
            return;
        };
        let _trace = FuseTrace::new("lookup", format!("parent={} name={}", parent.0, name_str));
        let _span = debug_span!("fuse::lookup", parent = parent.0, name = name_str).entered();

        // Synthetic root (no root_mount): mount points are children.
        if parent.0 == ROOT_INO && self.registry.root_mount_name().is_none() {
            if self.registry.get(name_str).is_some() {
                let ino = self.get_or_alloc_ino(name_str, "", wit_types::EntryKind::Directory, 0);
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
                        wit_types::EntryKind::Directory
                    } else {
                        file_kind_placeholder()
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
                    warn!(path = ?child_rp, err = %e, "backing fs error");
                    reply.error(Errno::ENOENT);
                },
            }
            return;
        }

        // L0/L2 cache path.
        match self.lookup_check_caches(&mount_name, &parent_path, name_str) {
            Ok(Some((attr, ttl))) => {
                reply.entry(&ttl, &attr, Generation(0));
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

        debug!(target: "omnifs_cache", kind = "miss", op = "lookup", mount = mount_name.as_str(), "cache miss");

        match self.lookup_via_provider(&runtime, &mount_name, &parent_path, name_str) {
            Ok((attr, ttl)) => reply.entry(&ttl, &attr, Generation(0)),
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
                    warn!(path = ?rp, err = %e, "backing fs error");
                    reply.error(Errno::ENOENT);
                },
            }
            return;
        }

        let attr = match &entry.kind {
            wit_types::EntryKind::Directory => self.dir_attr(ino.0),
            wit_types::EntryKind::File(_) => self.file_attr(ino.0, entry.size),
        };
        let ttl = Self::ttl_for_entry(&entry);
        reply.attr(&ttl, &attr);
    }

    fn opendir(&self, _req: &Request, ino: INodeNo, _flags: OpenFlags, reply: ReplyOpen) {
        let _trace = FuseTrace::new("opendir", format!("ino={}", ino.0));
        let _span = debug_span!("fuse::opendir", inode = ino.0).entered();

        let fh = self.alloc_fh();

        // Synthetic root (no root_mount): list mount points.
        if ino.0 == ROOT_INO && self.registry.root_mount_name().is_none() {
            let mounts = self.registry.mounts();
            let mut entries = Vec::new();
            for m in mounts {
                let child_ino = self.get_or_alloc_ino(&m, "", wit_types::EntryKind::Directory, 0);
                entries.push((child_ino, m, wit_types::EntryKind::Directory));
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

        debug!(target: "omnifs_cache", kind = "miss", op = "opendir", mount = mount_name.as_str(), "cache miss");

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
                wit_types::EntryKind::Directory => fuser::FileType::Directory,
                wit_types::EntryKind::File(_) => fuser::FileType::RegularFile,
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
        let _span = debug_span!("fuse::read", inode = ino.0, offset, size).entered();

        if let Some(ranged) = self.ranged_handles.get(&fh.0).map(|entry| entry.clone()) {
            self.read_ranged_handle(ino.0, &ranged, offset, size, reply);
            return;
        }

        // Serve from cache if this file handle already has data.
        if let Some(cached) = self.file_cache.get(&fh.0) {
            reply.data(data_slice(&cached, offset, size));
            return;
        }

        self.read_full_handle(ino, fh, offset, size, reply);
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

        let target = FullReadTarget {
            ino: ino.0,
            fh,
            mount_name,
            path,
            backing_path,
            attrs,
        };

        match self.open_ranged_file(&target) {
            Ok(Some(flags)) => {
                reply.opened(FuseFileHandle(fh), flags);
                return;
            },
            Ok(None) => {},
            Err(errno) => {
                reply.error(errno);
                return;
            },
        }

        match self.prefetch_full_file_on_open(&target) {
            Ok(Some(flags)) => {
                reply.opened(FuseFileHandle(fh), flags);
                return;
            },
            Ok(None) => {},
            Err(errno) => {
                reply.error(errno);
                return;
            },
        }

        let flags = target
            .attrs
            .filter(FileAttrsCache::should_direct_io)
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
            debug!(
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
                    warn!(path = ?rp, err = %e, "backing fs error");
                    reply.error(Errno::EIO);
                },
            }
        } else {
            reply.error(Errno::EINVAL);
        }
    }
}

/// Materialize a `read-file` terminal into the bytes the FUSE response
/// will return. Inline content travels in the WIT; blob content gets
/// pulled from the host's blob cache. Returns `None` when a blob-backed
/// payload can't be resolved (logged at warn for diagnostics).
fn resolve_read_payload(
    runtime: &ProviderRuntime,
    path: &str,
    result: ReadFileResult,
) -> Option<(Vec<u8>, FileAttrsCache)> {
    let attrs = FileAttrsCache::from(&result.attrs);
    match result.bytes {
        ReadFileBytes::Inline(bytes) => Some((bytes, attrs)),
        ReadFileBytes::Blob(blob) => match runtime.read_blob_full(blob) {
            Ok(bytes) => Some((bytes, attrs)),
            Err(e) => {
                warn!(path, error = %e, "blob-backed read failed");
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
    data.get(start..end).unwrap_or(&[])
}

fn should_prefetch_full_on_open(attrs: &FileAttrsCache) -> bool {
    matches!(
        attrs.bytes,
        wit_types::ProjBytes::Deferred(wit_types::ReadMode::Full)
    ) && !matches!(attrs.size, wit_types::FileSize::Exact(_))
}

fn learned_full_read_attrs(attrs: FileAttrsCache, content_len: usize) -> FileAttrsCache {
    if !can_publish_learned_size(&attrs) {
        return attrs;
    }
    match attrs.size {
        wit_types::FileSize::Exact(_) => attrs,
        wit_types::FileSize::NonZero | wit_types::FileSize::Unknown => {
            attrs.with_exact_size(u64::try_from(content_len).unwrap_or(u64::MAX))
        },
    }
}

fn learned_ranged_eof_attrs(attrs: FileAttrsCache, eof_size: u64) -> Option<FileAttrsCache> {
    if !can_publish_learned_size(&attrs) {
        return None;
    }
    match attrs.size {
        wit_types::FileSize::Exact(_) => None,
        wit_types::FileSize::NonZero | wit_types::FileSize::Unknown => {
            Some(attrs.with_exact_size(eof_size))
        },
    }
}

fn opened_file_attrs(
    path: &str,
    projected: Option<&FileAttrsCache>,
    opened: &crate::omnifs::provider::types::FileAttrs,
) -> Result<FileAttrsCache, Errno> {
    let Some(projected) = projected else {
        warn!(
            path,
            "open-file returned without a prior ranged file projection"
        );
        return Err(Errno::EIO);
    };
    if !matches!(
        projected.bytes,
        wit_types::ProjBytes::Deferred(wit_types::ReadMode::Ranged)
    ) {
        warn!(
            path,
            "open-file requires proj-bytes::deferred(read-mode::ranged)"
        );
        return Err(Errno::EIO);
    }
    Ok(FileAttrsCache {
        size: opened.size,
        bytes: projected.bytes.clone(),
        stability: opened.stability,
        version_token: opened.version_token.clone(),
    })
}

fn can_publish_learned_size(attrs: &FileAttrsCache) -> bool {
    match attrs.stability {
        wit_types::Stability::Immutable | wit_types::Stability::Mutable => true,
        wit_types::Stability::Volatile => false,
    }
}

fn full_read_matches_attrs(attrs: &FileAttrsCache, content_len: usize) -> bool {
    match attrs.size {
        wit_types::FileSize::Exact(size) => {
            u64::try_from(content_len).is_ok_and(|content_len| content_len == size)
        },
        wit_types::FileSize::NonZero => content_len > 0,
        wit_types::FileSize::Unknown => true,
    }
}

fn file_payload_for_attrs(
    record: &CacheRecord,
    attrs: Option<&FileAttrsCache>,
) -> Option<FilePayload> {
    let payload = FilePayload::deserialize(&record.payload)?;
    let attrs = attrs?;
    if matches!(attrs.stability, wit_types::Stability::Mutable)
        && payload.version_token != attrs.version_token
    {
        return None;
    }
    if !full_read_matches_attrs(attrs, payload.content.len()) {
        return None;
    }
    Some(payload)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn rate_limited_provider_errors_surface_as_try_again() {
        let error = ProviderError {
            kind: ErrorKind::RateLimited,
            message: "rate limited".to_string(),
            retryable: true,
        };

        assert_eq!(i32::from(Errno::from(&error)), i32::from(Errno::EAGAIN));
    }
}
