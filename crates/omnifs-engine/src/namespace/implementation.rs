use std::collections::hash_map::DefaultHasher;
use std::hash::{Hash, Hasher};
use std::path::PathBuf;
use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::{Duration, Instant};

use dashmap::DashMap;
use futures::future::{BoxFuture, FutureExt};
use omnifs_api::events::{InspectorOutcome, TraceId};
use omnifs_core::path::Path;
use tokio::runtime::Handle;
use tokio::sync::broadcast;

use super::{
    Attrs, DirCursor, DirEntry, DirPage, EntryKind, Epoch, EventStream, Namespace, NodeAnswer,
    NodeId, NsError, NsEvent, ReadAnswer, ReadStyle, StabilityClass, view_types,
};
use crate::inspect::{self, InspectorRequestScope};
use crate::registry::MountRuntimes;
use crate::tree::{ListOutcome, RangedHandle, ReadResult, RequestCtx};
use crate::view::{EntryMeta, FileAttrsCache, FileSize};
use crate::{Engine, ServingContext, Tree, TreeError, TreeErrorKind};

/// Effectively-infinite protocol TTL for stable exact-size entries.
const TTL_STATIC: Duration = Duration::from_secs(u32::MAX as u64);
/// Zero TTL for entries whose size or content can move under the reader.
const TTL_DYNAMIC: Duration = Duration::ZERO;
const EVENT_CAPACITY: usize = 1024;
const DRAIN_TICK: Duration = Duration::from_millis(100);
#[allow(clippy::duration_suboptimal_units)] // 60s reads clearer than 1min here.
const HANDLE_IDLE: Duration = Duration::from_secs(60);
const ROOT_ID: u64 = 1;

impl EntryKind {
    fn from_node(node: &crate::Node) -> Self {
        if let Some(root) = node.subtree_path() {
            Self::Subtree { root: root.clone() }
        } else if node.is_dir() {
            Self::Directory
        } else {
            Self::File
        }
    }

    fn from_meta(meta: &EntryMeta) -> Self {
        match meta.kind() {
            view_types::EntryKind::Directory => Self::Directory,
            view_types::EntryKind::File => Self::File,
        }
    }
}

impl Attrs {
    fn from_cache(kind: EntryKind, attrs: Option<&FileAttrsCache>, change: u64) -> Self {
        let ttl = attrs.map_or(TTL_STATIC, |attrs| {
            if matches!(attrs.size(), FileSize::Exact(_))
                && matches!(attrs.stability(), view_types::Stability::Stable)
            {
                TTL_STATIC
            } else {
                TTL_DYNAMIC
            }
        });
        let stability = attrs.map_or(StabilityClass::Stable, |attrs| match attrs.stability() {
            view_types::Stability::Stable => StabilityClass::Stable,
            view_types::Stability::Dynamic => StabilityClass::Dynamic,
            view_types::Stability::Live => StabilityClass::Live,
        });
        let read_style = if attrs.is_some_and(FileAttrsCache::is_deferred_ranged) {
            ReadStyle::Ranged
        } else {
            ReadStyle::Whole
        };
        Self {
            kind,
            size: attrs.map_or(0, FileAttrsCache::st_size),
            ttl,
            change,
            direct_io: attrs.is_some_and(FileAttrsCache::should_direct_io),
            stability,
            read_style,
        }
    }
}

impl DirPage {
    fn with_budget(
        mut entries: Vec<DirEntry>,
        then: Option<view_types::CachedCursor>,
        budget: usize,
    ) -> Self {
        if budget == 0 || entries.len() <= budget {
            return Self {
                entries,
                next: then.map(DirCursor::Tree),
            };
        }
        let overflow = entries.split_off(budget);
        Self {
            entries,
            next: Some(DirCursor::Buffered {
                entries: overflow,
                then,
            }),
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

/// One entry in the engine identity table.
struct NodeRecord {
    /// Full protocol path (the rehydration key: `Tree::resolve` round-trips it).
    full_path: Path,
    /// Mount name (`""` for the synthetic enumeration root).
    mount: String,
    /// Mount-relative path (the invalidation-match key).
    rel: Path,
    /// Best-known file attrs, preserving a learned size across placeholder
    /// refreshes (the engine-internal learned-size writeback).
    attrs: Option<FileAttrsCache>,
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
    /// start the background invalidation drain. The returned value is the
    /// frontend's complete `dyn Namespace` implementation.
    pub fn new(registry: Arc<MountRuntimes>, rt: Handle) -> Arc<Self> {
        let ctx = ServingContext::from_runtimes(Arc::clone(&registry));
        Self::assemble(Tree::new(ctx), Some(registry), rt)
    }

    /// Single-mount constructor for the kernel-free test harness and any
    /// single-mount embedding. The live-follow pump is unavailable in this form
    /// (it needs the mount registry).
    pub fn single(mount: String, runtime: Arc<Engine>, rt: Handle) -> Arc<Self> {
        let ctx = ServingContext::single(mount, runtime);
        Self::assemble(Tree::new(ctx), None, rt)
    }

    fn assemble(tree: Tree, registry: Option<Arc<MountRuntimes>>, rt: Handle) -> Arc<Self> {
        let tree = Arc::new(tree);
        let (events, _) = broadcast::channel(EVENT_CAPACITY);
        let this = Arc::new(Self {
            tree,
            registry,
            rt,
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
        this.spawn_drain_tick();
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
            attrs: None,
        };
        self.by_path.insert((mount, "/".to_string()), ROOT_ID);
        self.ids.insert(ROOT_ID, root);
    }

    fn spawn_drain_tick(self: &Arc<Self>) {
        let weak = Arc::downgrade(self);
        let handle = self.rt.spawn(async move {
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

    // --- inspector tracing ---------------------------------------------------
    //
    // `TreeNamespace` is the sole trace-id minting authority for `RequestCtx`.
    // The `Namespace` trait deliberately keeps a `NodeId` opaque to callers (an
    // engine-owned handle, not a (mount, path) pair), so FUSE, NFS, and the wire
    // server dispatching on a remote frontend's behalf cannot derive the mount
    // and path an inspector span needs to display; only this id table (via
    // `record`) can. Minting here once per `Namespace` call gives downstream
    // engine and provider spans (`Runtime::run_op`, callouts) the trace id
    // without coupling a frontend to tree internals.

    /// Begin an inspector span for one `Namespace` call, when the process-wide
    /// sink is attached. `None` when the inspector is disabled (the default).
    fn begin_span(op: &'static str, mount: &str, path: &str) -> Option<InspectorRequestScope> {
        inspect::global().map(|sink| InspectorRequestScope::begin(sink, op, mount, path))
    }

    /// The trace id a span minted, for threading into `RequestCtx`.
    fn span_trace(span: Option<&InspectorRequestScope>) -> Option<TraceId> {
        span.map(InspectorRequestScope::trace_id)
    }

    /// Record `result`'s outcome on `span`, if there is a live one and the call
    /// failed. A live span always reports `Ok` on `Drop` unless this overrides
    /// it, so the `Ok` path needs no explicit call.
    ///
    /// Takes `span` only after the traced future has already been awaited (see
    /// call sites), never while it is in flight: `InspectorRequestScope` holds
    /// a `Cell`, so a live `&InspectorRequestScope` is `!Sync` and cannot be
    /// held across an `.await` inside a `BoxFuture: Send` (the `Namespace`
    /// trait's bound).
    fn record_outcome<T>(span: Option<&InspectorRequestScope>, result: &Result<T, NsError>) {
        if let Err(error) = result
            && let Some(span) = span
        {
            span.set_outcome(Self::outcome_for(error));
        }
    }

    fn outcome_for(error: &NsError) -> InspectorOutcome {
        match error {
            NsError::NotFound => InspectorOutcome::NotFound,
            NsError::NotDirectory | NsError::IsDirectory | NsError::Invalid => {
                InspectorOutcome::InvalidInput
            },
            NsError::Permission => InspectorOutcome::Denied,
            NsError::TooLarge => InspectorOutcome::TooLarge,
            NsError::RateLimited { .. } | NsError::Timeout => InspectorOutcome::Timeout,
            NsError::Network => InspectorOutcome::Network,
            NsError::Internal { .. } => InspectorOutcome::Internal,
        }
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
                attrs: merged,
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
    async fn resolve_node(
        &self,
        full_path: &Path,
        trace: Option<TraceId>,
    ) -> Result<crate::Node, NsError> {
        let ctx = RequestCtx { trace };
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
        let record = self.ids.get(&id);
        let attrs = record
            .as_ref()
            .and_then(|record| record.attrs.as_ref())
            .or_else(|| node.attrs());
        self.attrs_from_parts(id, node, attrs)
    }

    fn attrs_from_parts(
        &self,
        id: u64,
        node: &crate::Node,
        attrs: Option<&FileAttrsCache>,
    ) -> Attrs {
        let epoch = self.node_epochs.get(&id).map_or(0, |e| *e);
        Attrs::from_cache(
            EntryKind::from_node(node),
            attrs,
            self.root_aware_change(id, node, attrs, epoch),
        )
    }

    /// The change counter for a node, folding the served-mount set into the
    /// enumeration root's answer. Adding or removing a mount does not invalidate
    /// the synthetic `/` node (its mount name is `""`, never a served mount), so
    /// its epoch never moves; a frontend cache keyed on the change attribute
    /// (the NFS root directory listing under `noac`) would otherwise never drop a
    /// stale empty listing. Mixing the sorted served mounts in bumps the root's
    /// change exactly when the mount set changes.
    fn root_aware_change(
        &self,
        id: u64,
        node: &crate::Node,
        attrs: Option<&FileAttrsCache>,
        epoch: u64,
    ) -> u64 {
        let change = change_counter(node, attrs, epoch);
        if id != ROOT_ID || !self.tree.root_mount_name().is_empty() {
            return change;
        }
        let mut hasher = DefaultHasher::new();
        change.hash(&mut hasher);
        let mut mounts = self.tree.served_mounts();
        mounts.sort();
        mounts.hash(&mut hasher);
        hasher.finish()
    }

    // --- read ---------------------------------------------------------------

    async fn read_inner(&self, id: NodeId, offset: u64, len: u32) -> Result<ReadAnswer, NsError> {
        // A live ranged handle already open on this node serves the read without
        // re-resolving, so a follow read reuses the single open. It bypasses
        // tracing because the raw provider handle is not driven through
        // `Tree`/`Runtime::run_op`, leaving no span for a trace id to attach to.
        if let Some(handle) = self.take_cached_handle(id.0) {
            return self.read_ranged(id.0, &handle, offset, len).await;
        }

        let (full_path, mount) = self.record(id)?;
        self.process_invalidations(&mount);
        let span = Self::begin_span("read", &mount, full_path.as_str());
        let trace = Self::span_trace(span.as_ref());
        let result = async {
            let node = self.resolve_node(&full_path, trace).await?;

            if node.is_dir() {
                return Err(NsError::IsDirectory);
            }

            // A ranged route projects a `Deferred(Ranged)` placeholder, so open a
            // provider handle and cache it; a full/whole file takes the
            // full-read path. `Tree::open` returning `None` means the route
            // declared ranged but the handler answered full: fall through to
            // the full read.
            if node.attrs().is_some_and(FileAttrsCache::is_deferred_ranged)
                && let Some(handle) = self.tree.open(&node).await?
            {
                self.open_count.fetch_add(1, Ordering::Relaxed);
                let handle = self.cache_handle(id.0, &node, handle);
                return self.read_ranged(id.0, &handle, offset, len).await;
            }

            self.read_full(id.0, &node, offset, len, trace).await
        }
        .await;
        Self::record_outcome(span.as_ref(), &result);
        result
    }

    async fn read_ranged(
        &self,
        id: u64,
        handle: &Arc<RangedHandle>,
        offset: u64,
        len: u32,
    ) -> Result<ReadAnswer, NsError> {
        let chunk = handle.read(offset, len).await?;
        // Learn the exact size the chunk observed, falling back to the handle's
        // declared attrs when the read did not refine them.
        let learned = chunk
            .learned_attrs
            .unwrap_or_else(|| handle.attrs().clone());
        self.store_learned(id, learned.clone());
        let attrs = self.attrs_for_read(id, EntryKind::File, Some(&learned));
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
        trace: Option<TraceId>,
    ) -> Result<ReadAnswer, NsError> {
        let ctx = RequestCtx { trace };
        match self.tree.read(node, &ctx).await? {
            ReadResult::Bytes { data, attrs, .. } => {
                if let Some(attrs) = &attrs {
                    self.store_learned(id, attrs.clone());
                }
                let start = usize::try_from(offset)
                    .unwrap_or(usize::MAX)
                    .min(data.len());
                let end = start.saturating_add(len as usize).min(data.len());
                let bytes = data[start..end].to_vec();
                let eof = end >= data.len();
                let attrs = self.attrs_for_read(id, EntryKind::from_node(node), attrs.as_ref());
                Ok(ReadAnswer { bytes, eof, attrs })
            },
            // A subtree node is a directory; its files are served by the
            // in-process frontend from the backing dir, never through this read.
            ReadResult::Subtree(_) => Err(NsError::IsDirectory),
        }
    }

    /// Compute `Attrs` for a read answer, folding in the size the read learned.
    fn attrs_for_read(&self, id: u64, kind: EntryKind, attrs: Option<&FileAttrsCache>) -> Attrs {
        let epoch = self.node_epochs.get(&id).map_or(0, |e| *e);
        Attrs::from_cache(kind, attrs, change_counter_parts(id, attrs, epoch))
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
            let attrs = Attrs::from_cache(
                EntryKind::File,
                Some(&grown),
                change_counter_parts(id, Some(&grown), node_epoch),
            );
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
            return Ok(DirPage::with_budget(entries, then, budget));
        }

        let (full_path, mount) = self.record(id)?;
        self.process_invalidations(&mount);
        let span = Self::begin_span("readdir", &mount, full_path.as_str());
        let trace = Self::span_trace(span.as_ref());
        let result = async {
            let node = self.resolve_node(&full_path, trace).await?;
            if !node.is_dir() {
                return Err(NsError::NotDirectory);
            }

            let tree_cursor = match cursor {
                DirCursor::Start => None,
                DirCursor::Tree(c) => Some(crate::Cursor(c)),
                DirCursor::Buffered { .. } => unreachable!("buffered handled above"),
            };
            let ctx = RequestCtx { trace };
            let listing = match self.tree.list(&node, tree_cursor, &ctx).await? {
                ListOutcome::Listing(listing) => listing,
                // A subtree node's children are served by the in-process
                // frontend from the backing dir; this listing path does not
                // enumerate them.
                ListOutcome::Subtree(_) => return Err(NsError::NotDirectory),
            };

            let mount = node.mount().to_string();
            let parent_full = full_path;
            let entries = listing
                .entries
                .iter()
                .map(|entry| {
                    self.dir_entry(&mount, &parent_full, node.path(), &entry.name, &entry.meta)
                })
                .collect();
            let tree_next = listing.next_cursor.map(|c| c.0);
            Ok(DirPage::with_budget(entries, tree_next, budget))
        }
        .await;
        Self::record_outcome(span.as_ref(), &result);
        result
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
        let full = parent_full
            .join(name)
            .unwrap_or_else(|_| parent_full.clone());
        // The synthetic enumeration root (mount `""`) lists mount roots: a child's
        // canonical identity is (its mount, `/`), the same key `lookup`/`intern`
        // mint when the same mount root is reached by name. Deriving it from the
        // parent's mount (`""`) and joined path instead would give the mount root a
        // second node id on the readdir path, so a frontend that keys inodes on the
        // node id would see the same object under two identities.
        let (child_mount, child_rel) = if mount.is_empty() {
            (name.to_string(), Path::root())
        } else {
            (
                mount.to_string(),
                parent_rel.join(name).unwrap_or_else(|_| parent_rel.clone()),
            )
        };
        let key = (child_mount.clone(), child_rel.as_str().to_string());
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
                mount: child_mount,
                rel: child_rel,
                attrs: merged.clone(),
            },
        );
        let epoch = self.node_epochs.get(&id).map_or(0, |e| *e);
        let kind = EntryKind::from_meta(meta);
        let attrs = Attrs::from_cache(
            kind.clone(),
            merged.as_ref(),
            change_counter_parts(id, merged.as_ref(), epoch),
        );
        DirEntry {
            attrs,
            kind,
            name: name.to_string(),
            node: NodeId(id),
        }
    }

    // --- getattr ------------------------------------------------------------

    async fn getattr_inner(&self, id: NodeId, exact: bool) -> Result<Attrs, NsError> {
        let (full_path, mount) = self.record(id)?;
        self.process_invalidations(&mount);
        let op = if exact { "getattr_exact" } else { "getattr" };
        let span = Self::begin_span(op, &mount, full_path.as_str());
        let trace = Self::span_trace(span.as_ref());
        let result = async {
            let node = self.resolve_node(&full_path, trace).await?;
            let refreshed = self.refresh_record(id.0, &node);

            // The exact-size flavor probes provider I/O for a deferred ranged
            // file so the NFS renderer can flatten a directory with real child
            // sizes.
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
        .await;
        Self::record_outcome(span.as_ref(), &result);
        result
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
            let span = Self::begin_span("lookup", &mount, child_full.as_str());
            let trace = Self::span_trace(span.as_ref());
            let result = async {
                let node = self.resolve_node(&child_full, trace).await?;
                let id = self.intern(&node);
                let attrs = self.attrs_for(id.0, &node);
                Ok(NodeAnswer {
                    node: id,
                    kind: attrs.kind.clone(),
                    attrs,
                })
            }
            .await;
            Self::record_outcome(span.as_ref(), &result);
            result
        }
        .boxed()
    }

    fn getattr(&self, node: NodeId) -> BoxFuture<'_, Result<Attrs, NsError>> {
        self.getattr_inner(node, false).boxed()
    }

    fn getattr_exact(&self, node: NodeId) -> BoxFuture<'_, Result<Attrs, NsError>> {
        self.getattr_inner(node, true).boxed()
    }

    fn readdir(
        &self,
        node: NodeId,
        cursor: DirCursor,
        budget: usize,
    ) -> BoxFuture<'_, Result<DirPage, NsError>> {
        self.readdir_inner(node, cursor, budget).boxed()
    }

    fn read(
        &self,
        node: NodeId,
        offset: u64,
        len: u32,
    ) -> BoxFuture<'_, Result<ReadAnswer, NsError>> {
        self.read_inner(node, offset, len).boxed()
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
        EventStream::from_broadcast(self.events.subscribe())
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
