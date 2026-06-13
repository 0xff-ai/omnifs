//! FUSE filesystem implementation.
//!
//! Bridges the omnifs virtual filesystem to the kernel FUSE subsystem.
//! Routes operations to WASM providers. Supports direct filesystem
//! passthrough when providers set backing paths on nodes.

pub(crate) mod inode;

mod common;
mod errno;
mod filesystem;
mod listing;
mod lookup;
pub mod mount;
mod read;
mod read_helpers;
mod trace;

#[cfg(test)]
mod tests;

pub(crate) use common::{DirSnapshot, ROOT_INO, TTL, TTL_DYNAMIC};
use omnifs_tree::RangedHandle;

use dashmap::DashMap;
use fuser::{FileAttr, INodeNo, MountOption, Notifier};
use inode::NodeEntry;
use omnifs_core::view as view_types;
use omnifs_core::view::{EntryMeta, FileAttrsCache};
#[cfg(test)]
use omnifs_host::Runtime;
use omnifs_host::path_key::{PathKey, PathToInode};
use omnifs_host::registry::ProviderRegistry;
use omnifs_tree::Tree;
use omnifs_wit::provider::types as wit_types;
use parking_lot::Mutex;
use std::ffi::OsStr;
use std::sync::Arc;
use std::sync::atomic::AtomicU64;
use std::time::Duration;
use tokio::runtime::Handle;

/// Shared slot for the kernel notifier. The daemon owns one, passes it to
/// [`mount::run_blocking`] (which fills it once the session is up), and uses
/// it to invalidate dentries when mounts are removed at runtime.
pub type NotifierHandle = Arc<Mutex<Option<Notifier>>>;

#[must_use]
pub fn new_notifier_handle() -> NotifierHandle {
    Arc::new(Mutex::new(None))
}

/// Invalidate the kernel dentry for a direct child of the filesystem root.
/// Entries are served with effectively-infinite TTLs on the premise that
/// the daemon invalidates them on change; call this when a mount is
/// removed so it does not linger as a phantom directory.
pub fn invalidate_root_child(notifier: &NotifierHandle, name: &str) {
    if let Some(notifier) = notifier.lock().as_ref() {
        let _ = notifier.inval_entry(INodeNo(ROOT_INO), std::ffi::OsStr::new(name));
    }
}

fn path_prefix_matches(prefix: &str, path: &str) -> bool {
    let Ok(prefix) = omnifs_core::path::Path::parse(prefix) else {
        return false;
    };
    let Ok(path) = omnifs_core::path::Path::parse(path) else {
        return false;
    };
    path.has_prefix(&prefix)
}

pub(crate) struct Frontend {
    rt: Handle,
    registry: Arc<ProviderRegistry>,
    /// The renderer-neutral projection core. Owns the listing/lookup/read
    /// DECISION logic (cache consult+populate, pagination, the `@next`/`@all`
    /// controls and mount-root ignore files, invalidation drain). The FUSE
    /// adapter enters the async runtime once per fuser callback to call it, then
    /// builds kernel identity (inodes) and reply structures on the neutral
    /// `Node`/`Listing`/`ReadResult` it returns.
    tree: Tree,
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
    /// `Tree`-owned ranged read handles bound to a FUSE `fh`. Each owns its
    /// `Arc<Runtime>` + provider handle; the adapter drives `read`/`close` and
    /// promotes any learned size to the inode.
    ranged_handles: DashMap<u64, RangedHandle>,
}

impl Frontend {
    #[cfg(test)]
    pub(crate) fn new(rt: Handle, registry: Arc<ProviderRegistry>) -> Self {
        Self::new_with_path_map(rt, registry, Arc::new(DashMap::new()))
    }

    #[cfg(test)]
    pub(crate) fn new_with_path_map(
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

    pub(crate) fn new_with_path_map_and_notifier(
        rt: Handle,
        registry: Arc<ProviderRegistry>,
        path_to_inode: Arc<PathToInode>,
        notifier: NotifierHandle,
    ) -> Self {
        let inodes = DashMap::new();

        let root_entry = NodeEntry {
            mount_name: registry.root_mount_name().unwrap_or_default(),
            path: omnifs_core::path::Path::ROOT.to_string(),
            kind: wit_types::EntryKind::Directory,
            attrs: None,
            size: 0,
            backing_path: None,
            synthetic: false,
        };
        inodes.insert(ROOT_INO, root_entry);

        let tree = Tree::new(Arc::clone(&registry));

        Self {
            rt,
            registry,
            tree,
            inodes,
            path_to_inode,
            notifier,
            next_ino: AtomicU64::new(2),
            dir_snapshots: DashMap::new(),
            next_fh: AtomicU64::new(1),
            file_cache: DashMap::new(),
            ranged_handles: DashMap::new(),
        }
    }

    pub(crate) fn mount_config() -> fuser::Config {
        let mut config = fuser::Config::default();
        config.mount_options = vec![MountOption::RO, MountOption::FSName("omnifs".to_string())];
        config
    }

    /// The runtime serving `mount`, if present. The live adapter reaches the
    /// runtime through `Tree`; this registry accessor exists for the in-crate
    /// harness, which seeds caches and inspects mount-level state directly.
    #[cfg(test)]
    pub(crate) fn runtime_for_mount(&self, mount: &str) -> Option<Arc<Runtime>> {
        self.registry.get(mount)
    }

    /// Re-bind the cached root inode to the live root mount and return the
    /// current binding. Mounts arrive at runtime, so a root-mounted
    /// provider may appear (or disappear) after the `Frontend` was
    /// constructed. The stale check uses a shared read; the write lock is
    /// taken only on an actual root-mount change (once per add/remove).
    pub(crate) fn sync_root_mount(&self) -> Option<String> {
        let current = self.registry.root_mount_name();
        let name = current.clone().unwrap_or_default();
        let stale = self
            .inodes
            .get(&ROOT_INO)
            .is_some_and(|entry| entry.mount_name != name);
        if stale && let Some(mut entry) = self.inodes.get_mut(&ROOT_INO) {
            entry.mount_name = name;
        }
        current
    }

    /// Drain pending runtime invalidations and drive the kernel-side fan-out.
    ///
    /// `Tree::drain_invalidations` owns the renderer-neutral half: it drains the
    /// runtime's invalidation queues and evicts the matching mem entries. The
    /// FUSE adapter consumes the returned `InvalidationReport` to drive its own
    /// kernel notifier (`inval_entry`/`inval_inode`) and prune the
    /// `path_to_inode` dedup table, which are kernel/inode-table concerns the
    /// projection core must not own.
    pub(crate) fn drain_and_evict_pending(&self, mount: &str) {
        let report = self.tree.drain_invalidations(mount);
        if report.is_empty() {
            return;
        }

        let mut to_remove = Vec::new();
        for entry in self.path_to_inode.iter() {
            let key = entry.key();
            if key.mount != mount {
                continue;
            }
            let path = &key.path;
            let matches_exact = report.paths.iter().any(|p| p.as_str() == path);
            let matches_prefix = report
                .prefixes
                .iter()
                .any(|prefix| path_prefix_matches(prefix.as_str(), path));
            if matches_exact || matches_prefix {
                to_remove.push(key.clone());
            }
        }

        for path_key in &to_remove {
            self.notify_entry_deleted(mount, &path_key.path);
            self.path_to_inode.remove(path_key);
        }
        for path in &report.changed_dirs {
            self.notify_dir_changed(mount, path.as_str());
        }
    }

    fn notify_entry_deleted(&self, mount: &str, path: &str) {
        let Some((parent_path, child_name)) = parent_child_for_notify(path) else {
            return;
        };
        let parent_ino = self
            .path_to_inode
            .get(&PathKey::new(mount.to_string(), parent_path))
            .map_or(ROOT_INO, |r| *r.value());
        if let Some(notifier) = self.notifier.lock().as_ref() {
            let _ = notifier.inval_entry(INodeNo(parent_ino), OsStr::new(&child_name));
        }
    }

    fn notify_dir_changed(&self, mount: &str, path: &str) {
        let Some(dir_ino) = self
            .path_to_inode
            .get(&PathKey::new(mount.to_string(), path.to_string()))
            .map(|r| *r.value())
        else {
            return;
        };
        if let Some(notifier) = self.notifier.lock().as_ref() {
            let _ = notifier.inval_inode(INodeNo(dir_ino), 0, 0);
        }
    }

    fn attr_for_kind(&self, ino: u64, kind: &wit_types::EntryKind, size: u64) -> FileAttr {
        match kind {
            wit_types::EntryKind::Directory => self.dir_attr(ino),
            wit_types::EntryKind::File(_) => self.file_attr(ino, size),
        }
    }

    pub(crate) fn attr_for_inode_or_meta(
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
        if !matches!(attrs.size, view_types::FileSize::Exact(_))
            || !matches!(attrs.stability, view_types::Stability::Immutable)
        {
            return TTL_DYNAMIC;
        }
        TTL
    }

    pub(crate) fn ttl_for_meta(meta: &EntryMeta) -> Duration {
        Self::ttl_for_attrs(meta.attrs.as_ref())
    }

    fn ttl_for_entry(entry: &NodeEntry) -> Duration {
        Self::ttl_for_attrs(entry.attrs.as_ref())
    }
}

pub(crate) fn parent_child_for_notify(path: &str) -> Option<(String, String)> {
    common::split_parent_leaf(path)
}
