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

use common::{InodeBody, split_parent_leaf};
use dashmap::DashMap;
use fuser::{FileAttr, INodeNo, MountOption, Notifier};
use inode::NodeEntry;
#[cfg(test)]
use omnifs_engine::Engine;
use omnifs_engine::MountRuntimes;
use omnifs_engine::ServingContext;
use omnifs_engine::Tree;
use omnifs_engine::render::{FollowSizeTable, PathKey, PathToInode, stale_ids};
use omnifs_engine::view as view_types;
use omnifs_engine::view::{EntryKind, EntryMeta, FileAttrsCache};
use parking_lot::Mutex;
use std::ffi::OsStr;
use std::sync::Arc;
use std::sync::atomic::AtomicU64;
use std::time::Duration;
use tokio::runtime::Handle;
use tokio::sync::{OwnedSemaphorePermit, Semaphore};

const MAX_IN_FLIGHT_OPS: usize = 64;

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

#[derive(Clone)]
pub(crate) struct Frontend {
    rt: Handle,
    registry: Arc<MountRuntimes>,
    /// The renderer-neutral projection core. Owns the listing/lookup/read
    /// DECISION logic (cache consult+populate, pagination, the `@next`/`@all`
    /// controls and mount-root ignore files, invalidation drain). The FUSE
    /// adapter dispatches each fuser callback onto the async runtime, then builds
    /// kernel identity (inodes) and reply structures on the neutral
    /// `Node`/`Listing`/`ReadResult` it returns.
    tree: Arc<Tree>,
    inodes: Arc<PathToInode<InodeBody>>,
    /// Reverse lookup: (mount name, path) -> inode, for dedup.
    /// Shared via `Arc` so the FUSE notifier can also hold a reference
    /// and invalidate entries concurrently without cloning the map.
    path_to_inode: Arc<PathToInode<InodeBody>>,
    notifier: NotifierHandle,
    next_ino: Arc<AtomicU64>,
    dir_snapshots: Arc<DashMap<u64, DirSnapshot>>,
    next_fh: Arc<AtomicU64>,
    /// Caches file content by file handle; populated on first read, evicted on release.
    file_cache: Arc<DashMap<u64, Vec<u8>>>,
    /// `Tree`-owned ranged read handles bound to a FUSE `fh`, each paired with
    /// the kernel inode it serves. The handle owns its `Arc<Engine>` + provider
    /// handle; the adapter drives `read`/`close` and promotes any learned size
    /// to the inode.
    ranged_handles: Arc<DashMap<u64, RangedSlot>>,
    /// Latest upstream size observed by a per-handle follow pump, keyed by
    /// inode. `getattr` reports `max(entry.size, follow_sizes[ino])` so a
    /// polling `tail -f` sees a live file grow between its own reads. The size
    /// source is `Tree::probe_live_growth`; this map is the FUSE-side reporting.
    follow_sizes: Arc<FollowSizeTable>,
    /// Abort handles for follow pumps, keyed by file handle; aborted on release.
    follow_pumps: Arc<DashMap<u64, tokio::task::AbortHandle>>,
    op_permits: Arc<Semaphore>,
}

impl Frontend {
    #[cfg(test)]
    pub(crate) fn new(rt: Handle, registry: Arc<MountRuntimes>) -> Self {
        Self::new_with_path_map(rt, registry, Arc::new(PathToInode::new()))
    }

    #[cfg(test)]
    pub(crate) fn new_with_path_map(
        rt: Handle,
        registry: Arc<MountRuntimes>,
        path_to_inode: Arc<PathToInode<InodeBody>>,
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
        path_to_inode: Arc<PathToInode<InodeBody>>,
        notifier: NotifierHandle,
    ) -> Self {
        let inodes = Arc::clone(&path_to_inode);

        let root_entry = NodeEntry {
            mount_name: registry.root_mount_name().unwrap_or_default(),
            path: omnifs_core::path::Path::root(),
            kind: EntryKind::Directory,
            attrs: None,
            size: 0,
            size_exact: true,
            body: InodeBody::Provider,
            extra: (),
        };
        inodes.insert_entry(ROOT_INO, root_entry);

        let tree = Tree::new(ServingContext::from_runtimes(Arc::clone(&registry)));

        Self {
            rt,
            registry,
            tree: Arc::new(tree),
            inodes,
            path_to_inode,
            notifier,
            next_ino: Arc::new(AtomicU64::new(2)),
            dir_snapshots: Arc::new(DashMap::new()),
            next_fh: Arc::new(AtomicU64::new(1)),
            file_cache: Arc::new(DashMap::new()),
            ranged_handles: Arc::new(DashMap::new()),
            follow_sizes: Arc::new(FollowSizeTable::default()),
            follow_pumps: Arc::new(DashMap::new()),
            op_permits: Arc::new(Semaphore::new(MAX_IN_FLIGHT_OPS)),
        }
    }

    pub(crate) fn mount_config() -> fuser::Config {
        let mut config = fuser::Config::default();
        config.mount_options = vec![MountOption::RO, MountOption::FSName("omnifs".to_string())];
        config
    }

    async fn acquire_op_permit(&self) -> OwnedSemaphorePermit {
        self.op_permits
            .clone()
            .acquire_owned()
            .await
            .expect("FUSE op semaphore is never closed")
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

        let stale = stale_ids(&report, &self.path_to_inode, mount);

        for ino in stale {
            let Some(entry) = self.path_to_inode.get(&ino) else {
                continue;
            };
            let path = entry.path.clone();
            drop(entry);
            self.notify_entry_deleted(mount, &path);
            if let Ok(key) = PathKey::with_mount_str(mount, path.clone()) {
                self.path_to_inode.remove_key(&key);
            }
        }
        for path in &report.changed_dirs {
            self.notify_dir_changed(mount, path);
        }
    }

    fn notify_entry_deleted(&self, mount: &str, path: &omnifs_core::path::Path) {
        let Some((parent_path, child_name)) = split_parent_leaf(path) else {
            return;
        };
        let parent_ino = self
            .path_to_inode
            .id_for_key(&PathKey::with_mount_str(mount, parent_path).expect("runtime mount name"))
            .unwrap_or(ROOT_INO);
        if let Some(notifier) = self.notifier.lock().as_ref() {
            let _ = notifier.inval_entry(INodeNo(parent_ino), OsStr::new(&child_name));
        }
    }

    fn notify_dir_changed(&self, mount: &str, path: &omnifs_core::path::Path) {
        let Some(dir_ino) = self
            .path_to_inode
            .id_for_key(&PathKey::with_mount_str(mount, path.clone()).expect("runtime mount name"))
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
