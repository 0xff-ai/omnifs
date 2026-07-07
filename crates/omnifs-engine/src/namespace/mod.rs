//! The narrow, plain-data namespace surface the runtime redesign converges on.
//!
//! [`Namespace`] is the whole contract a frontend needs to project a mount: name
//! resolution, attributes, directory paging, byte reads, and an invalidation
//! event stream. Every type crossing this boundary is plain data (serde-friendly,
//! no engine internals), because a later phase moves this surface across a wire:
//! the frontend holds a `dyn Namespace` and nothing else.
//!
//! [`TreeNamespace`] is the in-engine implementation over [`Tree`]. It owns the
//! things a frontend used to re-derive per protocol:
//!
//! - **Identity.** A [`NodeId`] is an opaque, engine-owned handle. The engine
//!   table maps it to the (mount, mount-relative path) the projection speaks;
//!   `NodeId(1)` is the namespace root. Ids are NOT stable across a daemon
//!   restart (a restart renumbers; a re-attach re-resolves, which a later phase
//!   handles); within a session an id keeps its meaning so a frontend can cache
//!   it.
//! - **Policy.** [`Attrs`] carries the already-decided protocol answer: the
//!   sentinel/learned size, the cache TTL, the direct-I/O bit, a change counter,
//!   and a stability class. The frontend copies these into its protocol reply
//!   without re-running FUSE's `ttl_for_attrs` or NFS's change hash.
//! - **Invalidation fan-out.** Every op drains its mount's pending
//!   invalidations before computing its answer (read-your-effects), maps them to
//!   the ids this table knows, bumps an epoch, and emits an event. A background
//!   tick keeps events flowing when no op arrives.
//!
//! # Consistency rule
//!
//! An op stamps every id it invalidates with the current epoch; the id's next
//! answer carries that epoch through [`Attrs::change`]. A frontend must not serve
//! protocol state older than the epoch of a node's last answer.

use std::collections::hash_map::DefaultHasher;
use std::hash::{Hash, Hasher};
use std::path::PathBuf;
use std::pin::Pin;
use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};
use std::task::{Context, Poll};
use std::time::{Duration, Instant};

use dashmap::DashMap;
use futures::future::{BoxFuture, FutureExt};
use omnifs_core::path::Path;
use serde::{Deserialize, Serialize};
use tokio::runtime::Handle;
use tokio::sync::broadcast;
use tokio_stream::wrappers::BroadcastStream;

use crate::registry::MountRuntimes;
use crate::tree::{ListOutcome, RangedHandle, ReadResult, RequestCtx};
use crate::view::{self as view_types, EntryMeta, FileAttrsCache, FileSize};
use crate::{Engine, ServingContext, Tree, TreeError, TreeErrorKind};

/// Effectively-infinite protocol TTL: the engine never expires an entry on time
/// (invalidation is the only eviction signal), so a stable exact-size entry is
/// cacheable indefinitely. Mirrors the FUSE `TTL` constant.
const TTL_STATIC: Duration = Duration::from_secs(u32::MAX as u64);
/// Zero TTL for an entry whose size or content can move under the reader (a
/// non-exact size or a non-stable file), forcing a re-stat. Mirrors FUSE's
/// `TTL_DYNAMIC` and the direct-I/O read path.
const TTL_DYNAMIC: Duration = Duration::ZERO;

/// Broadcast capacity for the invalidation event stream. A slow subscriber that
/// falls this far behind observes a lag error and re-syncs.
const EVENT_CAPACITY: usize = 1024;
/// Background cadence for draining a served mount's pending invalidations when no
/// op arrives, and for sweeping idle ranged handles.
const DRAIN_TICK: Duration = Duration::from_millis(100);
/// Idle lifetime of a cached ranged handle before the background sweep closes it.
#[allow(clippy::duration_suboptimal_units)] // 60s reads clearer than 1min here.
const HANDLE_IDLE: Duration = Duration::from_secs(60);
/// The reserved namespace-root id.
const ROOT_ID: u64 = 1;

// -----------------------------------------------------------------------------
// Plain-data surface
// -----------------------------------------------------------------------------

/// Opaque, engine-owned node handle. The engine maps it to a (mount, path); a
/// frontend treats it as a token and never inspects the integer.
///
/// No cross-restart persistence: a daemon restart may renumber ids. A frontend
/// re-attaches by re-resolving from the root, which a later phase formalizes.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord, Serialize, Deserialize)]
pub struct NodeId(pub u64);

impl NodeId {
    /// The namespace root: the mount-enumeration directory (or the single/rooted
    /// mount's root). Every resolution starts here.
    pub const ROOT: NodeId = NodeId(ROOT_ID);
}

/// A monotonic invalidation epoch. Bumped once per non-empty invalidation report;
/// stamped onto the nodes that report touched.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize)]
pub struct Epoch(pub u64);

/// Node kind at the namespace boundary. A plain mirror of the projection's kinds
/// so the wire types never depend on `view`/`tree` internals.
///
/// `Symlink` is reserved: the projection does not yet produce symlinks, so it is
/// never returned today, but the variant keeps the wire shape complete.
/// `Subtree` is a local-directory handoff (a resolved treeref clone/archive): an
/// in-process frontend keeps serving the real `root`; the wire consequences are a
/// later-phase concern.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub enum EntryKind {
    Directory,
    File,
    Symlink,
    Subtree { root: PathBuf },
}

/// Freshness class of a file, plain-data mirror of `view::Stability`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum StabilityClass {
    Stable,
    Dynamic,
    Live,
}

/// The already-policied protocol attributes for a node. Every policy decision a
/// frontend used to make is baked in here:
///
/// - `size` is the protocol size after the sentinel and learned-size rules (an
///   unknown-length file reports `1`, a completed read promotes the exact size),
/// - `ttl` is the engine-decided protocol cache lifetime,
/// - `change` is a version counter for the NFS change attribute, stamped with the
///   node's last invalidation epoch,
/// - `direct_io` mirrors `FileAttrsCache::should_direct_io`,
/// - `stability` classifies the file's freshness.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Attrs {
    pub kind: EntryKind,
    pub size: u64,
    pub ttl: Duration,
    pub change: u64,
    pub direct_io: bool,
    pub stability: StabilityClass,
}

/// The resolved answer for a lookup.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct NodeAnswer {
    pub node: NodeId,
    pub attrs: Attrs,
    pub kind: EntryKind,
}

/// One directory child.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct DirEntry {
    pub name: String,
    pub node: NodeId,
    pub attrs: Attrs,
    pub kind: EntryKind,
}

/// A directory read page: some entries plus an optional continuation cursor.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct DirPage {
    pub entries: Vec<DirEntry>,
    pub next: Option<DirCursor>,
}

/// An opaque directory cursor. `Start` begins a listing; `Tree` continues a
/// provider-paged listing; `Buffered` carries the overflow the per-page `budget`
/// held back, so paging stays stateless (the cursor owns the resume state).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub enum DirCursor {
    Start,
    Tree(view_types::CachedCursor),
    Buffered {
        entries: Vec<DirEntry>,
        then: Option<view_types::CachedCursor>,
    },
}

impl DirCursor {
    /// Begin a directory listing.
    pub fn start() -> Self {
        Self::Start
    }
}

/// The answer for one byte read. `attrs` lets a caller promote a learned size
/// without a second `getattr`: the learned-size writeback that FUSE/NFS did per
/// protocol is engine-internal here.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ReadAnswer {
    pub bytes: Vec<u8>,
    pub eof: bool,
    pub attrs: Attrs,
}

/// A namespace event. Plain data so it can cross the wire in a later phase.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub enum NsEvent {
    /// The subtree rooted at `node` may have changed; drop protocol-cached state
    /// for it and re-resolve.
    InvalidateSubtree { node: NodeId, epoch: Epoch },
    /// `node`'s attributes changed in place (a live file grew).
    AttrsChanged {
        node: NodeId,
        attrs: Attrs,
        epoch: Epoch,
    },
}

/// Retry classification for an [`NsError`], derivable without importing the
/// engine's tree errors. Mirrors the frontend `retry_class` partition.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum NsRetryClass {
    Retry,
    Gone,
    Terminal,
    TooLarge,
}

/// Plain-data error surface. Mirrors the frontend-relevant classification of the
/// engine's tree errors plus the retry class, so a frontend maps to errno /
/// nfsstat4 without importing engine internals.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum NsError {
    #[error("not found")]
    NotFound,
    #[error("not a directory")]
    NotDirectory,
    #[error("is a directory")]
    IsDirectory,
    #[error("permission denied")]
    Permission,
    #[error("invalid argument")]
    Invalid,
    #[error("too large")]
    TooLarge,
    #[error("rate limited")]
    RateLimited { retry_after: Option<Duration> },
    #[error("timeout")]
    Timeout,
    #[error("network error")]
    Network,
    #[error("internal error: {message}")]
    Internal { message: String },
}

impl NsError {
    /// How a frontend should treat this error on retry.
    pub fn retry_class(&self) -> NsRetryClass {
        match self {
            Self::RateLimited { .. } | Self::Timeout | Self::Network => NsRetryClass::Retry,
            Self::NotFound | Self::NotDirectory | Self::IsDirectory => NsRetryClass::Gone,
            Self::TooLarge => NsRetryClass::TooLarge,
            Self::Permission | Self::Invalid | Self::Internal { .. } => NsRetryClass::Terminal,
        }
    }
}

impl From<TreeError> for NsError {
    fn from(err: TreeError) -> Self {
        match err.kind {
            TreeErrorKind::NotFound => Self::NotFound,
            TreeErrorKind::NotDirectory => Self::NotDirectory,
            TreeErrorKind::IsDirectory => Self::IsDirectory,
            TreeErrorKind::PermissionDenied => Self::Permission,
            TreeErrorKind::InvalidInput => Self::Invalid,
            TreeErrorKind::TooLarge => Self::TooLarge,
            TreeErrorKind::RateLimited => Self::RateLimited {
                retry_after: err.retry_after,
            },
            TreeErrorKind::Timeout => Self::Timeout,
            TreeErrorKind::Network => Self::Network,
            TreeErrorKind::Internal => Self::Internal {
                message: err.message,
            },
        }
    }
}

/// The invalidation event stream a subscriber drives. Wraps a broadcast receiver
/// and drops lag errors (a lagged subscriber simply resyncs on the next event).
pub struct EventStream {
    inner: BroadcastStream<NsEvent>,
}

impl EventStream {
    /// Await the next event, or `None` when the sender is gone.
    pub async fn recv(&mut self) -> Option<NsEvent> {
        use futures::StreamExt;
        loop {
            match self.inner.next().await {
                Some(Ok(event)) => return Some(event),
                // A lagged receiver skips the gap and keeps going.
                Some(Err(_)) => {},
                None => return None,
            }
        }
    }
}

impl futures::Stream for EventStream {
    type Item = NsEvent;

    fn poll_next(mut self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Option<NsEvent>> {
        loop {
            match Pin::new(&mut self.inner).poll_next(cx) {
                Poll::Ready(Some(Ok(event))) => return Poll::Ready(Some(event)),
                Poll::Ready(Some(Err(_))) => {},
                Poll::Ready(None) => return Poll::Ready(None),
                Poll::Pending => return Poll::Pending,
            }
        }
    }
}

// -----------------------------------------------------------------------------
// The trait
// -----------------------------------------------------------------------------

/// The narrow namespace surface a frontend consumes. Dyn-compatible: methods
/// return [`BoxFuture`] rather than `async fn`, so the projection has no
/// async-trait dependency and a frontend can hold a `dyn Namespace`.
pub trait Namespace: Send + Sync {
    /// Resolve `name` under `parent`, allocating a stable id for the child.
    fn lookup<'a>(
        &'a self,
        parent: NodeId,
        name: &'a str,
    ) -> BoxFuture<'a, Result<NodeAnswer, NsError>>;

    /// The current attributes of `node`.
    fn getattr(&self, node: NodeId) -> BoxFuture<'_, Result<Attrs, NsError>>;

    /// Like [`getattr`](Namespace::getattr), but may perform provider I/O (the
    /// engine's ranged-attr probe) to learn an exact size. The NFS renderer's
    /// directory flattening needs an exact size per child.
    fn getattr_exact(&self, node: NodeId) -> BoxFuture<'_, Result<Attrs, NsError>>;

    /// Read one directory page from `cursor`, returning at most `budget` entries
    /// (0 = engine default).
    fn readdir(
        &self,
        node: NodeId,
        cursor: DirCursor,
        budget: usize,
    ) -> BoxFuture<'_, Result<DirPage, NsError>>;

    /// Read `len` bytes at `offset` from `node`.
    fn read(
        &self,
        node: NodeId,
        offset: u64,
        len: u32,
    ) -> BoxFuture<'_, Result<ReadAnswer, NsError>>;

    /// The link target of a symlink node.
    fn readlink(&self, node: NodeId) -> BoxFuture<'_, Result<PathBuf, NsError>>;

    /// Subscribe to invalidation events.
    fn subscribe(&self) -> EventStream;
}

// -----------------------------------------------------------------------------
// TreeNamespace
// -----------------------------------------------------------------------------

/// One entry in the engine identity table.
struct NodeRecord {
    /// Full protocol path (the rehydration key: `Tree::resolve` round-trips it).
    full_path: Path,
    /// Mount name (`""` for the synthetic enumeration root).
    mount: String,
    /// Mount-relative path (the invalidation-match key).
    rel: Path,
    /// Last-known kind.
    kind: view_types::EntryKind,
    /// Best-known file attrs, preserving a learned size across placeholder
    /// refreshes (the engine-internal learned-size writeback).
    attrs: Option<FileAttrsCache>,
    /// Backing dir when this node is a resolved treeref subtree.
    subtree_root: Option<PathBuf>,
}

/// A cached ranged read handle, its live-follow pump, and its idle clock.
struct HandleRecord {
    handle: Arc<RangedHandle>,
    pump: Option<tokio::task::AbortHandle>,
    last_use: Instant,
}

impl Drop for HandleRecord {
    fn drop(&mut self) {
        if let Some(pump) = self.pump.take() {
            pump.abort();
        }
        // Release the provider handle by reference; the cache owns the sole
        // reference at eviction, so this closes it exactly once.
        let _ = self.handle.release();
    }
}

/// The in-engine [`Namespace`] over [`Tree`]. Owns the id table, the invalidation
/// epoch and event fan-out, and the ranged-handle cache.
pub struct TreeNamespace {
    tree: Arc<Tree>,
    /// Present in the production (registry-backed) form; drives the live-follow
    /// pump, which needs the registry to re-fetch a runtime. Absent in the
    /// single-mount test form, which cannot spawn the pump.
    registry: Option<Arc<MountRuntimes>>,
    rt: Handle,
    ids: DashMap<u64, NodeRecord>,
    /// (mount, mount-relative path) -> id, so a re-resolved path keeps its id.
    by_path: DashMap<(String, String), u64>,
    next_id: AtomicU64,
    epoch: AtomicU64,
    /// id -> the epoch of its last invalidation, folded into `Attrs::change`.
    node_epochs: DashMap<u64, u64>,
    events: broadcast::Sender<NsEvent>,
    handles: DashMap<u64, HandleRecord>,
    /// Count of ranged opens that yielded a handle; a test hook for asserting
    /// handle reuse.
    open_count: AtomicU64,
    tick: std::sync::Mutex<Option<tokio::task::AbortHandle>>,
}

impl TreeNamespace {
    /// Production constructor: build the [`Tree`] over the mount registry and
    /// start the background invalidation drain, so a later step can hand a
    /// frontend a `dyn Namespace` and nothing else.
    // The public constructors take an owned `Handle` (callers pass
    // `Handle::current()`); the runtime handle is cloned into the background tick.
    #[allow(clippy::needless_pass_by_value)]
    pub fn new(registry: Arc<MountRuntimes>, rt: Handle) -> Arc<Self> {
        let ctx = ServingContext::from_runtimes(Arc::clone(&registry));
        Self::assemble(Tree::new(ctx), Some(registry), &rt)
    }

    /// Single-mount constructor for the kernel-free test harness and any
    /// single-mount embedding. The live-follow pump is unavailable in this form
    /// (it needs the mount registry).
    #[allow(clippy::needless_pass_by_value)]
    pub fn single(mount: String, runtime: Arc<Engine>, rt: Handle) -> Arc<Self> {
        let ctx = ServingContext::single(mount, runtime);
        Self::assemble(Tree::new(ctx), None, &rt)
    }

    fn assemble(tree: Tree, registry: Option<Arc<MountRuntimes>>, rt: &Handle) -> Arc<Self> {
        let tree = Arc::new(tree);
        let (events, _) = broadcast::channel(EVENT_CAPACITY);
        let this = Arc::new(Self {
            tree,
            registry,
            rt: rt.clone(),
            ids: DashMap::new(),
            by_path: DashMap::new(),
            next_id: AtomicU64::new(ROOT_ID + 1),
            epoch: AtomicU64::new(0),
            node_epochs: DashMap::new(),
            events,
            handles: DashMap::new(),
            open_count: AtomicU64::new(0),
            tick: std::sync::Mutex::new(None),
        });
        this.install_root();
        this.spawn_drain_tick(rt);
        this
    }

    /// The root record: the namespace root maps to the served root mount's root
    /// (or the synthetic enumeration root, mount `""`).
    fn install_root(&self) {
        let mount = self.tree.root_mount_name();
        let root = NodeRecord {
            full_path: Path::root(),
            mount: mount.clone(),
            rel: Path::root(),
            kind: view_types::EntryKind::Directory,
            attrs: None,
            subtree_root: None,
        };
        self.by_path.insert((mount, "/".to_string()), ROOT_ID);
        self.ids.insert(ROOT_ID, root);
    }

    fn spawn_drain_tick(self: &Arc<Self>, rt: &Handle) {
        let weak = Arc::downgrade(self);
        let handle = rt.spawn(async move {
            loop {
                tokio::time::sleep(DRAIN_TICK).await;
                let Some(this) = weak.upgrade() else {
                    break;
                };
                for mount in this.tree.served_mounts() {
                    this.process_invalidations(&mount);
                }
                this.sweep_idle_handles();
            }
        });
        *self.tick.lock().expect("tick lock") = Some(handle.abort_handle());
    }

    /// Test hook: the number of ranged opens that yielded a handle. Two reads of
    /// the same ranged node that reuse the cached handle leave this at one.
    #[doc(hidden)]
    pub fn ranged_open_count(&self) -> u64 {
        self.open_count.load(Ordering::Relaxed)
    }

    // --- identity -----------------------------------------------------------

    fn record(&self, id: NodeId) -> Result<(Path, String), NsError> {
        self.ids
            .get(&id.0)
            .map(|r| (r.full_path.clone(), r.mount.clone()))
            .ok_or(NsError::NotFound)
    }

    /// Allocate (or reuse) the id for a resolved node, and refresh its record,
    /// preserving a learned size across placeholder refreshes.
    fn intern(&self, node: &crate::Node) -> NodeId {
        let mount = node.mount().to_string();
        let rel = node.path().clone();
        let key = (mount.clone(), rel.as_str().to_string());
        let full_path = self.full_path_for(node);

        let id = *self
            .by_path
            .entry(key)
            .or_insert_with(|| self.next_id.fetch_add(1, Ordering::Relaxed));

        let merged = FileAttrsCache::merge_preserving_learned_size(
            self.ids.get(&id).and_then(|r| r.attrs.clone()).as_ref(),
            node.attrs().cloned(),
        );
        self.ids.insert(
            id,
            NodeRecord {
                full_path,
                mount,
                rel,
                kind: node.kind(),
                attrs: merged,
                subtree_root: node.subtree_path().cloned(),
            },
        );
        NodeId(id)
    }

    /// The full protocol path for a freshly resolved node. For a single-mount or
    /// root-mounted registry the mount-relative path already is the full path;
    /// for the enumeration registry a mount-rooted child is `/<mount><rel>`.
    fn full_path_for(&self, node: &crate::Node) -> Path {
        let mount = node.mount();
        let rel = node.path();
        // The enumeration registry is the only backing where a node's mount is a
        // real path segment: reconstruct `/<mount><rel>`. Every other backing
        // serves one namespace whose mount-relative path is the full path.
        if self.tree.root_mount_name().is_empty() && !mount.is_empty() {
            let joined = if rel.is_root() {
                format!("/{mount}")
            } else {
                format!("/{mount}{}", rel.as_str())
            };
            return Path::parse(&joined).unwrap_or_else(|_| rel.clone());
        }
        rel.clone()
    }

    /// Re-resolve a node to a fresh [`crate::Node`]. `Tree::resolve` round-trips
    /// the full protocol path across every backing (single, rooted, enumeration).
    async fn resolve_node(&self, full_path: &Path) -> Result<crate::Node, NsError> {
        let ctx = RequestCtx::default();
        self.tree.resolve(full_path, &ctx).await.map_err(Into::into)
    }

    // --- invalidation -------------------------------------------------------

    /// Drain a mount's pending invalidations, map them to known ids, bump the
    /// epoch once, emit an event per affected id, and evict that id's derived
    /// state (attrs + ranged handle) while preserving its stable identity.
    fn process_invalidations(&self, mount: &str) {
        let report = self.tree.drain_invalidations(mount);
        if report.is_empty() {
            return;
        }

        let affected: Vec<u64> = self
            .ids
            .iter()
            .filter_map(|entry| {
                let record = entry.value();
                if record.mount != mount {
                    return None;
                }
                let hit = report.paths.iter().any(|p| p == &record.rel)
                    || report
                        .prefixes
                        .iter()
                        .any(|prefix| record.rel.has_prefix(prefix));
                hit.then(|| *entry.key())
            })
            .collect();

        let epoch = self.epoch.fetch_add(1, Ordering::SeqCst) + 1;
        for id in affected {
            self.node_epochs.insert(id, epoch);
            // Drop the learned attrs so the next answer re-resolves; keep the
            // identity so a frontend's cached id stays resolvable.
            if let Some(mut record) = self.ids.get_mut(&id) {
                record.attrs = None;
            }
            // Evicting the handle closes it and aborts its pump (Drop).
            self.handles.remove(&id);
            let _ = self.events.send(NsEvent::InvalidateSubtree {
                node: NodeId(id),
                epoch: Epoch(epoch),
            });
        }
    }

    fn sweep_idle_handles(&self) {
        let stale: Vec<u64> = self
            .handles
            .iter()
            .filter_map(|entry| {
                (entry.value().last_use.elapsed() >= HANDLE_IDLE).then(|| *entry.key())
            })
            .collect();
        for id in stale {
            self.handles.remove(&id);
        }
    }

    // --- attrs --------------------------------------------------------------

    /// Build the policied [`Attrs`] for a node from its best-known file attrs.
    fn attrs_for(&self, id: u64, node: &crate::Node) -> Attrs {
        let attrs = self
            .ids
            .get(&id)
            .and_then(|r| r.attrs.clone())
            .or_else(|| node.attrs().cloned());
        self.attrs_from_parts(id, node, attrs.as_ref())
    }

    fn attrs_from_parts(
        &self,
        id: u64,
        node: &crate::Node,
        attrs: Option<&FileAttrsCache>,
    ) -> Attrs {
        let kind = ns_kind(node);
        let epoch = self.node_epochs.get(&id).map_or(0, |e| *e);
        Attrs {
            size: attrs.map_or(0, FileAttrsCache::st_size),
            ttl: ttl_for(attrs),
            direct_io: attrs.is_some_and(FileAttrsCache::should_direct_io),
            stability: attrs.map_or(StabilityClass::Stable, |a| stability_class(a.stability())),
            change: change_counter(node, attrs, epoch),
            kind,
        }
    }

    // --- read ---------------------------------------------------------------

    async fn read_inner(&self, id: NodeId, offset: u64, len: u32) -> Result<ReadAnswer, NsError> {
        // A live ranged handle already open on this node serves the read without
        // re-resolving, so a follow read reuses the single open.
        if let Some(handle) = self.take_cached_handle(id.0) {
            return self.read_ranged(id.0, &handle, offset, len).await;
        }

        let (full_path, mount) = self.record(id)?;
        self.process_invalidations(&mount);
        let node = self.resolve_node(&full_path).await?;

        if node.is_dir() {
            return Err(NsError::IsDirectory);
        }

        // A ranged route projects a `Deferred(Ranged)` placeholder, so open a
        // provider handle and cache it; a full/whole file takes the whole-read
        // path. `Tree::open` returning `None` means the route declared ranged but
        // the handler answered full: fall through to the full read.
        if node.attrs().is_some_and(FileAttrsCache::is_deferred_ranged) {
            let ctx = RequestCtx::default();
            if let Some(handle) = self.tree.open(&node, &ctx).await? {
                self.open_count.fetch_add(1, Ordering::Relaxed);
                let handle = self.cache_handle(id.0, &node, handle);
                return self.read_ranged(id.0, &handle, offset, len).await;
            }
        }

        self.read_full(id.0, &node, offset, len).await
    }

    async fn read_ranged(
        &self,
        id: u64,
        handle: &Arc<RangedHandle>,
        offset: u64,
        len: u32,
    ) -> Result<ReadAnswer, NsError> {
        let chunk = handle.read(offset, len).await?;
        // Learn the exact size the chunk observed, if any, and rebuild attrs.
        let learned = chunk
            .learned_attrs
            .clone()
            .or_else(|| Some(handle.attrs().clone()));
        if let Some(attrs) = &learned {
            self.store_learned(id, attrs.clone());
        }
        let attrs = self.attrs_for_learned(id, view_types::EntryKind::File, None, learned.as_ref());
        Ok(ReadAnswer {
            bytes: chunk.bytes,
            eof: chunk.eof,
            attrs,
        })
    }

    async fn read_full(
        &self,
        id: u64,
        node: &crate::Node,
        offset: u64,
        len: u32,
    ) -> Result<ReadAnswer, NsError> {
        let ctx = RequestCtx::default();
        match self.tree.read(node, &ctx).await? {
            ReadResult::Bytes { data, attrs, .. } => {
                if let Some(a) = &attrs {
                    self.store_learned(id, a.clone());
                }
                let start = usize::try_from(offset)
                    .unwrap_or(usize::MAX)
                    .min(data.len());
                let end = start.saturating_add(len as usize).min(data.len());
                let bytes = data[start..end].to_vec();
                let eof = end >= data.len();
                let attrs = self.attrs_for_learned(id, node.kind(), Some(node), attrs.as_ref());
                Ok(ReadAnswer { bytes, eof, attrs })
            },
            // A subtree node is a directory; its files are served by the
            // in-process frontend from the backing dir, never through this read.
            ReadResult::Subtree(_) => Err(NsError::IsDirectory),
        }
    }

    /// Compute `Attrs` for a read answer, folding in the size the read learned.
    fn attrs_for_learned(
        &self,
        id: u64,
        kind: view_types::EntryKind,
        node: Option<&crate::Node>,
        attrs: Option<&FileAttrsCache>,
    ) -> Attrs {
        let epoch = self.node_epochs.get(&id).map_or(0, |e| *e);
        Attrs {
            kind: node.map_or(
                match kind {
                    view_types::EntryKind::Directory => EntryKind::Directory,
                    view_types::EntryKind::File => EntryKind::File,
                },
                ns_kind,
            ),
            size: attrs.map_or(0, FileAttrsCache::st_size),
            ttl: ttl_for(attrs),
            direct_io: attrs.is_some_and(FileAttrsCache::should_direct_io),
            stability: attrs.map_or(StabilityClass::Stable, |a| stability_class(a.stability())),
            change: change_counter_parts(id, attrs, epoch),
        }
    }

    fn store_learned(&self, id: u64, learned: FileAttrsCache) {
        if let Some(mut record) = self.ids.get_mut(&id) {
            record.attrs =
                FileAttrsCache::merge_preserving_learned_size(record.attrs.as_ref(), Some(learned));
        }
    }

    fn take_cached_handle(&self, id: u64) -> Option<Arc<RangedHandle>> {
        let mut record = self.handles.get_mut(&id)?;
        record.last_use = Instant::now();
        Some(Arc::clone(&record.handle))
    }

    /// Cache a freshly opened ranged handle, spawning a live-follow pump for a
    /// live file when a registry is available (the production form). The pump
    /// grows the node's attrs and emits an `AttrsChanged` event.
    fn cache_handle(&self, id: u64, node: &crate::Node, handle: RangedHandle) -> Arc<RangedHandle> {
        let handle = Arc::new(handle);
        let pump = self.spawn_pump(id, node, &handle);
        self.handles.insert(
            id,
            HandleRecord {
                handle: Arc::clone(&handle),
                pump,
                last_use: Instant::now(),
            },
        );
        handle
    }

    fn spawn_pump(
        &self,
        id: u64,
        node: &crate::Node,
        handle: &Arc<RangedHandle>,
    ) -> Option<tokio::task::AbortHandle> {
        if !matches!(handle.attrs().stability(), view_types::Stability::Live) {
            return None;
        }
        // The live-follow pump needs the registry to re-fetch a runtime each
        // probe; the single-mount test form cannot spawn it.
        let registry = self.registry.clone()?;
        let mount = node.mount().to_string();
        let base = handle.attrs().clone();
        let events = self.events.clone();
        let node_epoch = self.node_epochs.get(&id).map_or(0, |e| *e);
        // The pump is a detached task; it reports growth by cloning the shared
        // pieces it needs (no back-reference to `self`).
        let record_growth = move |new_end: u64| {
            let grown = base.clone().with_exact_size(new_end);
            let attrs = Attrs {
                kind: EntryKind::File,
                size: grown.st_size(),
                ttl: ttl_for(Some(&grown)),
                direct_io: grown.should_direct_io(),
                stability: stability_class(grown.stability()),
                change: change_counter_parts(id, Some(&grown), node_epoch),
            };
            let _ = events.send(NsEvent::AttrsChanged {
                node: NodeId(id),
                attrs,
                epoch: Epoch(node_epoch),
            });
        };
        Some(crate::spawn_live_follow_pump(
            &self.rt,
            registry,
            mount,
            handle.provider_handle(),
            handle.observed_end(),
            record_growth,
        ))
    }

    // --- readdir ------------------------------------------------------------

    async fn readdir_inner(
        &self,
        id: NodeId,
        cursor: DirCursor,
        budget: usize,
    ) -> Result<DirPage, NsError> {
        // A buffered cursor is pure overflow the previous page held back; serve
        // it without touching the tree.
        if let DirCursor::Buffered { entries, then } = cursor {
            return Ok(page_from_buffer(entries, then, budget));
        }

        let (full_path, mount) = self.record(id)?;
        self.process_invalidations(&mount);
        let node = self.resolve_node(&full_path).await?;
        if !node.is_dir() {
            return Err(NsError::NotDirectory);
        }

        let tree_cursor = match cursor {
            DirCursor::Start => None,
            DirCursor::Tree(c) => Some(crate::Cursor(c)),
            DirCursor::Buffered { .. } => unreachable!("buffered handled above"),
        };
        let ctx = RequestCtx::default();
        let listing = match self.tree.list(&node, tree_cursor, &ctx).await? {
            ListOutcome::Listing(listing) => listing,
            // A subtree node's children are served by the in-process frontend
            // from the backing dir; this listing path does not enumerate them.
            ListOutcome::Subtree(_) => return Err(NsError::NotDirectory),
        };

        let mount = node.mount().to_string();
        let parent_full = full_path;
        let mut entries = Vec::with_capacity(listing.entries.len());
        for entry in &listing.entries {
            entries.push(self.dir_entry(
                &mount,
                &parent_full,
                node.path(),
                &entry.name,
                &entry.meta,
            ));
        }
        let tree_next = listing.next_cursor.map(|c| c.0);
        Ok(page_split(entries, tree_next, budget))
    }

    /// Turn a listing child into a `DirEntry`, allocating its id.
    fn dir_entry(
        &self,
        mount: &str,
        parent_full: &Path,
        parent_rel: &Path,
        name: &str,
        meta: &EntryMeta,
    ) -> DirEntry {
        let rel = parent_rel.join(name).unwrap_or_else(|_| parent_rel.clone());
        let full = parent_full
            .join(name)
            .unwrap_or_else(|_| parent_full.clone());
        let key = (mount.to_string(), rel.as_str().to_string());
        let id = *self
            .by_path
            .entry(key)
            .or_insert_with(|| self.next_id.fetch_add(1, Ordering::Relaxed));
        let attrs = meta.attrs().cloned();
        let merged = FileAttrsCache::merge_preserving_learned_size(
            self.ids.get(&id).and_then(|r| r.attrs.clone()).as_ref(),
            attrs,
        );
        self.ids.insert(
            id,
            NodeRecord {
                full_path: full,
                mount: mount.to_string(),
                rel,
                kind: meta.kind(),
                attrs: merged.clone(),
                subtree_root: None,
            },
        );
        let epoch = self.node_epochs.get(&id).map_or(0, |e| *e);
        let kind = ns_kind_from_meta(meta);
        DirEntry {
            attrs: Attrs {
                size: merged.as_ref().map_or(0, FileAttrsCache::st_size),
                ttl: ttl_for(merged.as_ref()),
                direct_io: merged
                    .as_ref()
                    .is_some_and(FileAttrsCache::should_direct_io),
                stability: merged
                    .as_ref()
                    .map_or(StabilityClass::Stable, |a| stability_class(a.stability())),
                change: change_counter_parts(id, merged.as_ref(), epoch),
                kind: kind.clone(),
            },
            kind,
            name: name.to_string(),
            node: NodeId(id),
        }
    }

    // --- getattr ------------------------------------------------------------

    async fn getattr_inner(&self, id: NodeId, exact: bool) -> Result<Attrs, NsError> {
        let (full_path, mount) = self.record(id)?;
        self.process_invalidations(&mount);
        let node = self.resolve_node(&full_path).await?;
        let refreshed = self.refresh_record(id.0, &node);

        // The exact-size flavor probes provider I/O for a deferred ranged file so
        // the NFS renderer can flatten a directory with real child sizes.
        if exact
            && refreshed
                .as_ref()
                .is_some_and(FileAttrsCache::is_deferred_ranged)
            && !refreshed
                .as_ref()
                .is_some_and(FileAttrsCache::has_exact_size)
            && let Some(probed) = self
                .tree
                .probe_ranged_attrs(node.mount(), node.path())
                .await?
        {
            self.store_learned(id.0, probed.clone());
            return Ok(self.attrs_from_parts(id.0, &node, Some(&probed)));
        }

        Ok(self.attrs_from_parts(id.0, &node, refreshed.as_ref()))
    }

    /// Refresh a record's stored attrs from a fresh resolve, preserving a learned
    /// size, and return the best-known attrs.
    fn refresh_record(&self, id: u64, node: &crate::Node) -> Option<FileAttrsCache> {
        let merged = FileAttrsCache::merge_preserving_learned_size(
            self.ids.get(&id).and_then(|r| r.attrs.clone()).as_ref(),
            node.attrs().cloned(),
        );
        if let Some(mut record) = self.ids.get_mut(&id) {
            record.attrs.clone_from(&merged);
            record.kind = node.kind();
            record.subtree_root = node.subtree_path().cloned();
        }
        merged
    }
}

impl Namespace for TreeNamespace {
    fn lookup<'a>(
        &'a self,
        parent: NodeId,
        name: &'a str,
    ) -> BoxFuture<'a, Result<NodeAnswer, NsError>> {
        async move {
            let (parent_full, mount) = self.record(parent)?;
            self.process_invalidations(&mount);
            let child_full = parent_full.join(name).map_err(|_| NsError::Invalid)?;
            let node = self.resolve_node(&child_full).await?;
            let id = self.intern(&node);
            let attrs = self.attrs_for(id.0, &node);
            Ok(NodeAnswer {
                node: id,
                kind: attrs.kind.clone(),
                attrs,
            })
        }
        .boxed()
    }

    fn getattr(&self, node: NodeId) -> BoxFuture<'_, Result<Attrs, NsError>> {
        async move { self.getattr_inner(node, false).await }.boxed()
    }

    fn getattr_exact(&self, node: NodeId) -> BoxFuture<'_, Result<Attrs, NsError>> {
        async move { self.getattr_inner(node, true).await }.boxed()
    }

    fn readdir(
        &self,
        node: NodeId,
        cursor: DirCursor,
        budget: usize,
    ) -> BoxFuture<'_, Result<DirPage, NsError>> {
        async move { self.readdir_inner(node, cursor, budget).await }.boxed()
    }

    fn read(
        &self,
        node: NodeId,
        offset: u64,
        len: u32,
    ) -> BoxFuture<'_, Result<ReadAnswer, NsError>> {
        async move { self.read_inner(node, offset, len).await }.boxed()
    }

    fn readlink(&self, node: NodeId) -> BoxFuture<'_, Result<PathBuf, NsError>> {
        async move {
            // The projection does not produce symlinks; a resolved node is a
            // directory or file. Report a non-symlink as an invalid argument.
            let _ = self.record(node)?;
            Err(NsError::Invalid)
        }
        .boxed()
    }

    fn subscribe(&self) -> EventStream {
        EventStream {
            inner: BroadcastStream::new(self.events.subscribe()),
        }
    }
}

impl Drop for TreeNamespace {
    fn drop(&mut self) {
        if let Some(tick) = self.tick.lock().expect("tick lock").take() {
            tick.abort();
        }
    }
}

// -----------------------------------------------------------------------------
// Free helpers
// -----------------------------------------------------------------------------

fn ns_kind(node: &crate::Node) -> EntryKind {
    if let Some(root) = node.subtree_path() {
        EntryKind::Subtree { root: root.clone() }
    } else if node.is_dir() {
        EntryKind::Directory
    } else {
        EntryKind::File
    }
}

fn ns_kind_from_meta(meta: &EntryMeta) -> EntryKind {
    match meta.kind() {
        view_types::EntryKind::Directory => EntryKind::Directory,
        view_types::EntryKind::File => EntryKind::File,
    }
}

fn stability_class(stability: view_types::Stability) -> StabilityClass {
    match stability {
        view_types::Stability::Stable => StabilityClass::Stable,
        view_types::Stability::Dynamic => StabilityClass::Dynamic,
        view_types::Stability::Live => StabilityClass::Live,
    }
}

/// Port of the FUSE `ttl_for_attrs` policy: a directory (no attrs) and a stable
/// exact-size file cache indefinitely; anything that can move under the reader
/// caches for zero seconds.
fn ttl_for(attrs: Option<&FileAttrsCache>) -> Duration {
    let Some(attrs) = attrs else {
        return TTL_STATIC;
    };
    if !matches!(attrs.size(), FileSize::Exact(_))
        || !matches!(attrs.stability(), view_types::Stability::Stable)
    {
        return TTL_DYNAMIC;
    }
    TTL_STATIC
}

/// Change counter over a resolved node, folding the node's last invalidation
/// epoch into the same facts NFS's `entry_change` hashes.
fn change_counter(node: &crate::Node, attrs: Option<&FileAttrsCache>, epoch: u64) -> u64 {
    let mut hasher = DefaultHasher::new();
    node.mount().hash(&mut hasher);
    node.path().hash(&mut hasher);
    hash_attr_facts(&mut hasher, attrs, epoch);
    hasher.finish()
}

/// Change counter without a resolved node in hand (a read/live-growth answer).
fn change_counter_parts(id: u64, attrs: Option<&FileAttrsCache>, epoch: u64) -> u64 {
    let mut hasher = DefaultHasher::new();
    id.hash(&mut hasher);
    hash_attr_facts(&mut hasher, attrs, epoch);
    hasher.finish()
}

fn hash_attr_facts(hasher: &mut DefaultHasher, attrs: Option<&FileAttrsCache>, epoch: u64) {
    epoch.hash(hasher);
    if let Some(attrs) = attrs {
        attrs.version_token().hash(hasher);
        attrs.st_size().hash(hasher);
        std::mem::discriminant(&attrs.size()).hash(hasher);
        std::mem::discriminant(&attrs.byte_source()).hash(hasher);
        std::mem::discriminant(&attrs.stability()).hash(hasher);
    }
}

/// Split a freshly listed page against a per-page `budget`: return at most
/// `budget` entries, carrying the overflow into a `Buffered` cursor chained
/// before the tree's own continuation.
fn page_split(
    mut entries: Vec<DirEntry>,
    tree_next: Option<view_types::CachedCursor>,
    budget: usize,
) -> DirPage {
    if budget == 0 || entries.len() <= budget {
        return DirPage {
            entries,
            next: tree_next.map(DirCursor::Tree),
        };
    }
    let overflow = entries.split_off(budget);
    DirPage {
        entries,
        next: Some(DirCursor::Buffered {
            entries: overflow,
            then: tree_next,
        }),
    }
}

/// Serve buffered overflow entries against a `budget`.
fn page_from_buffer(
    mut entries: Vec<DirEntry>,
    then: Option<view_types::CachedCursor>,
    budget: usize,
) -> DirPage {
    if budget == 0 || entries.len() <= budget {
        return DirPage {
            entries,
            next: then.map(DirCursor::Tree),
        };
    }
    let overflow = entries.split_off(budget);
    DirPage {
        entries,
        next: Some(DirCursor::Buffered {
            entries: overflow,
            then,
        }),
    }
}
