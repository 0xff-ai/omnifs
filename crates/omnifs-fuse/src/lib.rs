//! FUSE filesystem frontend over the engine [`Namespace`] surface.
//!
//! The adapter bridges the omnifs projected tree to the kernel FUSE subsystem.
//! It consumes only the plain-data namespace surface (validated paths, policied attrs,
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

#[cfg(test)]
mod tests;

pub(crate) use common::{DirSnapshot, Inode, NodeKind, ROOT_INO};

use dashmap::DashMap;
use fuser::{Errno, INodeNo, MountOption, Notifier};
use omnifs_core::path::Path;
use omnifs_engine::{Namespace, NsEvent};
use parking_lot::Mutex;
use std::ffi::OsStr;
use std::sync::Arc;
use std::sync::atomic::AtomicU64;
use tokio::runtime::Handle;
use tokio::sync::{mpsc, oneshot};

type FlushRequest = oneshot::Sender<()>;

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
    /// Kernel inode id -> protocol state.
    inodes: Arc<DashMap<u64, Inode>>,
    /// namespace node -> inode, so a re-resolved node keeps its inode.
    by_node: Arc<DashMap<Path, u64>>,
    next_ino: Arc<AtomicU64>,
    notifier: NotifierHandle,
    next_fh: Arc<AtomicU64>,
    dir_snapshots: Arc<DashMap<u64, DirSnapshot>>,
    /// Per-`fh` whole-file buffer for a `Whole` read style: filled once per open
    /// so a mutating control or an unversioned dynamic render runs exactly once.
    file_cache: Arc<DashMap<u64, Vec<u8>>>,
    /// Per-`fh` namespace node for a `Ranged` read style: each kernel read is a
    /// read-through, deduped behind the namespace's internal handle cache.
    ranged_fhs: Arc<DashMap<u64, Path>>,
    /// Per-node live-follow size learned from an `AttrsChanged` event. `getattr`
    /// reports `max(namespace size, grown[node])`, so a polling `tail -f`
    /// re-stats, sees growth, and reads the new bytes through the ranged path.
    grown_sizes: Arc<DashMap<Path, u64>>,
    flush_tx: Arc<Mutex<Option<mpsc::UnboundedSender<FlushRequest>>>>,
}

impl Frontend {
    pub(crate) fn new(rt: Handle, namespace: Arc<dyn Namespace>, notifier: NotifierHandle) -> Self {
        let inodes = Arc::new(DashMap::new());
        let by_node = Arc::new(DashMap::new());
        // The root inode projects the namespace mount-enumeration root; every
        // resolution starts here.
        inodes.insert(
            ROOT_INO,
            Inode {
                parent: ROOT_INO,
                name: String::new(),
                kind: NodeKind::Directory,
                body: Path::root(),
            },
        );
        by_node.insert(Path::root(), ROOT_INO);
        Self {
            rt,
            namespace,
            inodes,
            by_node,
            next_ino: Arc::new(AtomicU64::new(ROOT_INO + 1)),
            notifier,
            next_fh: Arc::new(AtomicU64::new(1)),
            dir_snapshots: Arc::new(DashMap::new()),
            file_cache: Arc::new(DashMap::new()),
            ranged_fhs: Arc::new(DashMap::new()),
            grown_sizes: Arc::new(DashMap::new()),
            flush_tx: Arc::new(Mutex::new(None)),
        }
    }

    pub(crate) fn mount_config() -> fuser::Config {
        let mut config = fuser::Config::default();
        config.mount_options = vec![MountOption::RO, MountOption::FSName("omnifs".to_string())];
        config
    }

    // --- events --------------------------------------------------------------

    /// Background pump: apply namespace events continuously so an invalidation
    /// from the engine's background drain tick reaches the kernel even when no
    /// op is in flight.
    pub(crate) fn spawn_event_pump(&self) {
        let fs = self.clone();
        let mut sub = fs.namespace.subscribe();
        let (flush_tx, mut flush_rx) = mpsc::unbounded_channel();
        *fs.flush_tx.lock() = Some(flush_tx);
        drop(fs.rt.spawn({
            let fs = fs.clone();
            async move {
                loop {
                    tokio::select! {
                        event = sub.recv() => {
                            let Some(event) = event else { break; };
                            fs.apply_event(&event);
                        }
                        request = flush_rx.recv() => {
                            let Some(request) = request else { break; };
                            while let Some(event) = sub.try_recv() {
                                fs.apply_event(&event);
                            }
                            let _ = request.send(());
                        }
                    }
                }
            }
        }));
    }

    /// Wait for the sole background event owner to apply every event already
    /// buffered by the wire before the operation publishes local identity.
    pub(crate) async fn flush_events(&self) -> Result<(), Errno> {
        let Some(sender) = self.flush_tx.lock().clone() else {
            return Ok(());
        };
        let (reply, receiver) = oneshot::channel();
        sender.send(reply).map_err(|_| Errno::EIO)?;
        receiver.await.map_err(|_| Errno::EIO)
    }

    fn apply_event(&self, event: &NsEvent) {
        match event {
            NsEvent::InvalidateSubtree { path } if path.is_root() => {
                self.grown_sizes.clear();
                for entry in self.inodes.iter() {
                    self.notify_entry_deleted(entry.parent, &entry.name);
                    self.notify_inode_changed(*entry.key());
                }
            },
            NsEvent::InvalidateSubtree { path } => self.invalidate_node(path),
            NsEvent::AttrsChanged { path, attrs } => {
                // Live growth is monotonic; never let a stale event shrink it.
                let mut entry = self.grown_sizes.entry(path.clone()).or_insert(0);
                *entry = (*entry).max(attrs.size);
                drop(entry);
                if let Some(ino) = self.by_node.get(path).map(|r| *r) {
                    self.notify_inode_changed(ino);
                }
            },
        }
    }

    /// Invalidate every known descendant by structural path prefix. FUSE keeps
    /// the path-backed inode and ranged-handle identities, while the kernel is
    /// told to refresh each affected dentry and inode.
    fn invalidate_node(&self, path: &Path) {
        let affected: Vec<(Path, u64, u64, String)> = self
            .by_node
            .iter()
            .filter_map(|entry| {
                entry.key().has_prefix(path).then(|| {
                    let ino = *entry.value();
                    let (parent, name) = self
                        .inodes
                        .get(&ino)
                        .map_or((ROOT_INO, String::new()), |inode| {
                            (inode.parent, inode.name.clone())
                        });
                    (entry.key().clone(), ino, parent, name)
                })
            })
            .collect();
        for (affected_path, ino, parent, name) in affected {
            self.grown_sizes.remove(&affected_path);
            self.notify_entry_deleted(parent, &name);
            self.notify_inode_changed(ino);
        }
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
