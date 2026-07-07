//! FUSE filesystem frontend over the engine [`Namespace`] surface.
//!
//! The adapter bridges the omnifs projected tree to the kernel FUSE subsystem.
//! It consumes only the plain-data namespace surface (node ids, policied attrs,
//! directory pages, byte reads, and the invalidation event stream) and keeps
//! FUSE protocol state only: kernel inode numbers, the per-`fh` file-handle
//! tables (whole-file buffers and ranged read-through nodes), directory
//! snapshots, kernel dentry/inode notifications, mount/unmount mechanics, reply
//! construction, errno mapping, and the op-concurrency semaphore. Every
//! projection answer (name resolution, attributes, listing, reads) comes from a
//! [`Namespace`]; the adapter never reaches into the projection tree, its
//! caches, or its render/identity machinery.
//!
//! Invalidation and live growth arrive as [`NsEvent`]s on a subscription the
//! adapter drains inline after each namespace op and on a background pump (so
//! the kernel is told to drop huge-TTL dentries even when no op is in flight).
//! An `InvalidateSubtree` prunes the affected inode and fires
//! `inval_entry`/`inval_inode`; an `AttrsChanged` records a live-follow grown
//! size that `getattr` folds in, so a polling `tail -f` re-stats to the new end.

pub(crate) mod inode;

mod common;
mod errno;
mod filesystem;
pub mod mount;
mod ops;
mod read_helpers;
mod trace;

#[cfg(test)]
mod tests;

pub(crate) use common::{Body, DirSnapshot, Inode, NodeKind, ROOT_INO};

use dashmap::DashMap;
use fuser::{INodeNo, MountOption, Notifier};
use omnifs_engine::{EventStream, Namespace, NodeId, NsEvent};
use parking_lot::Mutex;
use std::ffi::OsStr;
use std::path::PathBuf;
use std::sync::Arc;
use std::sync::atomic::AtomicU64;
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
/// Entries are served with effectively-infinite TTLs on the premise that the
/// daemon invalidates them on change; call this when a mount is removed so it
/// does not linger as a phantom directory.
pub fn invalidate_root_child(notifier: &NotifierHandle, name: &str) {
    if let Some(notifier) = notifier.lock().as_ref() {
        let _ = notifier.inval_entry(INodeNo(ROOT_INO), OsStr::new(name));
    }
}

#[derive(Clone)]
pub(crate) struct Frontend {
    rt: Handle,
    /// The projection surface. Every name resolution, attribute, listing, and
    /// read goes through it; the adapter holds nothing else of the engine.
    namespace: Arc<dyn Namespace>,
    /// Invalidation and live-growth events, drained inline after each namespace
    /// op so the kernel is notified and grown sizes are folded promptly.
    events: Arc<Mutex<EventStream>>,
    /// Kernel inode id -> protocol state.
    inodes: Arc<DashMap<u64, Inode>>,
    /// namespace node -> inode, so a re-resolved node keeps its inode.
    by_node: Arc<DashMap<NodeId, u64>>,
    /// backing path -> inode, for subtree-local children.
    by_backing: Arc<DashMap<PathBuf, u64>>,
    next_ino: Arc<AtomicU64>,
    notifier: NotifierHandle,
    next_fh: Arc<AtomicU64>,
    dir_snapshots: Arc<DashMap<u64, DirSnapshot>>,
    /// Per-`fh` whole-file buffer for a `Whole` read style: filled once per open
    /// so a mutating control or an unversioned dynamic render runs exactly once.
    file_cache: Arc<DashMap<u64, Vec<u8>>>,
    /// Per-`fh` namespace node for a `Ranged` read style: each kernel read is a
    /// read-through, deduped behind the namespace's internal handle cache.
    ranged_fhs: Arc<DashMap<u64, NodeId>>,
    /// Per-node live-follow size learned from an `AttrsChanged` event. `getattr`
    /// reports `max(namespace size, grown[node])`, so a polling `tail -f`
    /// re-stats, sees growth, and reads the new bytes through the ranged path.
    grown_sizes: Arc<DashMap<NodeId, u64>>,
    op_permits: Arc<Semaphore>,
}

impl Frontend {
    pub(crate) fn new(rt: Handle, namespace: Arc<dyn Namespace>, notifier: NotifierHandle) -> Self {
        let events = Arc::new(Mutex::new(namespace.subscribe()));
        let inodes = Arc::new(DashMap::new());
        let by_node = Arc::new(DashMap::new());
        // The root inode projects the namespace root (the mount-enumeration root
        // or the single/rooted mount's root); every resolution starts here.
        inodes.insert(
            ROOT_INO,
            Inode {
                parent: ROOT_INO,
                name: String::new(),
                kind: NodeKind::Directory,
                body: Body::Node(NodeId::ROOT),
            },
        );
        by_node.insert(NodeId::ROOT, ROOT_INO);
        Self {
            rt,
            namespace,
            events,
            inodes,
            by_node,
            by_backing: Arc::new(DashMap::new()),
            next_ino: Arc::new(AtomicU64::new(ROOT_INO + 1)),
            notifier,
            next_fh: Arc::new(AtomicU64::new(1)),
            dir_snapshots: Arc::new(DashMap::new()),
            file_cache: Arc::new(DashMap::new()),
            ranged_fhs: Arc::new(DashMap::new()),
            grown_sizes: Arc::new(DashMap::new()),
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

    // --- events --------------------------------------------------------------

    /// Drain the buffered namespace events emitted since the last drain and
    /// apply them. Called inline after every namespace op so a caller's own
    /// invalidation is folded in before it answers, and the kernel is told to
    /// drop the dentry it cached with a huge TTL.
    pub(crate) fn apply_pending_events(&self) {
        let mut events = self.events.lock();
        while let Some(event) = events.try_recv() {
            drop(events);
            self.apply_event(&event);
            events = self.events.lock();
        }
    }

    /// Background pump: apply namespace events continuously so an invalidation
    /// from the engine's background drain tick reaches the kernel even when no
    /// op is in flight. The old FUSE adapter drained only on ops; the namespace
    /// now emits events out of band, so a pump keeps the kernel current.
    pub(crate) fn spawn_event_pump(&self) {
        let fs = self.clone();
        let mut sub = fs.namespace.subscribe();
        drop(fs.rt.spawn({
            let fs = fs.clone();
            async move {
                while let Some(event) = sub.recv().await {
                    fs.apply_event(&event);
                }
            }
        }));
    }

    fn apply_event(&self, event: &NsEvent) {
        match event {
            NsEvent::InvalidateSubtree { node, .. } => self.invalidate_node(*node),
            NsEvent::AttrsChanged { node, attrs, .. } => {
                // Live growth is monotonic; never let a stale event shrink it.
                let mut entry = self.grown_sizes.entry(*node).or_insert(0);
                *entry = (*entry).max(attrs.size);
                drop(entry);
                if let Some(ino) = self.by_node.get(node).map(|r| *r) {
                    self.notify_inode_changed(ino);
                }
            },
        }
    }

    /// Prune the inode for an invalidated node and fire the kernel dentry/inode
    /// notifications so the kernel re-looks-up and re-stats. The root inode is
    /// preserved so a client's root handle never goes stale.
    fn invalidate_node(&self, node: NodeId) {
        let Some((_, ino)) = self.by_node.remove(&node) else {
            return;
        };
        if ino == ROOT_INO {
            self.by_node.insert(node, ino);
            return;
        }
        self.grown_sizes.remove(&node);
        if let Some((_, inode)) = self.inodes.remove(&ino) {
            self.notify_entry_deleted(inode.parent, &inode.name);
        }
        self.notify_inode_changed(ino);
    }

    fn notify_entry_deleted(&self, parent_ino: u64, name: &str) {
        if name.is_empty() {
            return;
        }
        if let Some(notifier) = self.notifier.lock().as_ref() {
            let _ = notifier.inval_entry(INodeNo(parent_ino), OsStr::new(name));
        }
    }

    fn notify_inode_changed(&self, ino: u64) {
        if let Some(notifier) = self.notifier.lock().as_ref() {
            let _ = notifier.inval_inode(INodeNo(ino), 0, 0);
        }
    }
}
