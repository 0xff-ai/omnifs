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

pub(crate) use common::{DirSnapshot, ROOT_INO, RangedSlot, TTL, TTL_DYNAMIC};

use common::InodeBody;
use dashmap::DashMap;
use fuser::{FileAttr, INodeNo, MountOption, Notifier};
use inode::NodeEntry;
#[cfg(test)]
use omnifs_engine::Engine;
use omnifs_engine::MountRuntimes;
use omnifs_engine::ServingContext;
use omnifs_engine::Tree;
use omnifs_engine::render::{PathKey, PathToInode};
use omnifs_engine::view as view_types;
use omnifs_engine::view::{EntryKind, EntryMeta, FileAttrsCache};
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

pub(crate) struct Frontend {
    rt: Handle,
    registry: Arc<MountRuntimes>,
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
    /// `Tree`-owned ranged read handles bound to a FUSE `fh`, each paired with
    /// the kernel inode it serves. The handle owns its `Arc<Engine>` + provider
    /// handle; the adapter drives `read`/`close` and promotes any learned size
    /// to the inode.
    ranged_handles: DashMap<u64, RangedSlot>,
    /// Latest upstream size observed by a per-handle follow pump, keyed by
    /// inode. `getattr` reports `max(entry.size, follow_sizes[ino])` so a
    /// polling `tail -f` sees a live file grow between its own reads. The size
    /// source is `Tree::probe_live_growth`; this map is the FUSE-side reporting.
    follow_sizes: Arc<DashMap<u64, u64>>,
    /// Abort handles for follow pumps, keyed by file handle; aborted on release.
    follow_pumps: DashMap<u64, tokio::task::AbortHandle>,
}

impl Frontend {
    #[cfg(test)]
    pub(crate) fn new(rt: Handle, registry: Arc<MountRuntimes>) -> Self {
        Self::new_with_path_map(rt, registry, Arc::new(DashMap::new()))
    }

    #[cfg(test)]
    pub(crate) fn new_with_path_map(
        rt: Handle,
        registry: Arc<MountRuntimes>,
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
        registry: Arc<MountRuntimes>,
        path_to_inode: Arc<PathToInode>,
        notifier: NotifierHandle,
    ) -> Self {
        let inodes = DashMap::new();

        let root_entry = NodeEntry {
            mount_name: registry.root_mount_name().unwrap_or_default(),
            path: omnifs_core::path::Path::root(),
            kind: EntryKind::Directory,
            attrs: None,
            size: 0,
            body: InodeBody::Provider,
        };
        inodes.insert(ROOT_INO, root_entry);

        let tree = Tree::new(ServingContext::from_runtimes(Arc::clone(&registry)));

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
            follow_sizes: Arc::new(DashMap::new()),
            follow_pumps: DashMap::new(),
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
    pub(crate) fn runtime_for_mount(&self, mount: &str) -> Option<Arc<Engine>> {
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
            if key.mount.as_str() != mount {
                continue;
            }
            let path = &key.path;
            let matches_exact = report.paths.iter().any(|p| p == path);
            let matches_prefix = report.prefixes.iter().any(|prefix| path.has_prefix(prefix));
            if matches_exact || matches_prefix {
                to_remove.push(key.clone());
            }
        }

        for path_key in &to_remove {
            self.notify_entry_deleted(mount, &path_key.path);
            self.path_to_inode.remove(path_key);
        }
        for path in &report.changed_dirs {
            self.notify_dir_changed(mount, path);
        }
    }

    fn notify_entry_deleted(&self, mount: &str, path: &omnifs_core::path::Path) {
        let Some((parent_path, child_name)) = parent_child_for_notify(path) else {
            return;
        };
        let parent_ino = self
            .path_to_inode
            .get(&PathKey::with_mount_str(mount, parent_path).expect("runtime mount name"))
            .map_or(ROOT_INO, |r| *r.value());
        if let Some(notifier) = self.notifier.lock().as_ref() {
            let _ = notifier.inval_entry(INodeNo(parent_ino), OsStr::new(&child_name));
        }
    }

    fn notify_dir_changed(&self, mount: &str, path: &omnifs_core::path::Path) {
        let Some(dir_ino) = self
            .path_to_inode
            .get(&PathKey::with_mount_str(mount, path.clone()).expect("runtime mount name"))
            .map(|r| *r.value())
        else {
            return;
        };
        if let Some(notifier) = self.notifier.lock().as_ref() {
            let _ = notifier.inval_inode(INodeNo(dir_ino), 0, 0);
        }
    }

    fn attr_for_kind(&self, ino: u64, kind: EntryKind, size: u64) -> FileAttr {
        match kind {
            EntryKind::Directory => self.dir_attr(ino),
            EntryKind::File => self.file_attr(ino, size),
        }
    }

    pub(crate) fn attr_for_inode_or_meta(
        &self,
        ino: u64,
        fallback_kind: EntryKind,
        fallback_size: u64,
    ) -> FileAttr {
        if let Some(entry) = self.inodes.get(&ino) {
            return self.attr_for_kind(ino, entry.kind, entry.size);
        }
        self.attr_for_kind(ino, fallback_kind, fallback_size)
    }

    fn ttl_for_attrs(attrs: Option<&FileAttrsCache>) -> Duration {
        let Some(attrs) = attrs else {
            return TTL;
        };
        if !matches!(attrs.size(), view_types::FileSize::Exact(_))
            || !matches!(attrs.stability(), view_types::Stability::Stable)
        {
            return TTL_DYNAMIC;
        }
        TTL
    }

    pub(crate) fn ttl_for_meta(meta: &EntryMeta) -> Duration {
        Self::ttl_for_attrs(meta.attrs())
    }

    fn ttl_for_entry(entry: &NodeEntry) -> Duration {
        Self::ttl_for_attrs(entry.attrs.as_ref())
    }
}

pub(crate) fn parent_child_for_notify(
    path: &omnifs_core::path::Path,
) -> Option<(omnifs_core::path::Path, String)> {
    let (parent, child) = path.parent_and_name()?;
    Some((parent, child.to_string()))
}
