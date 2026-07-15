use std::collections::{HashSet, hash_map::DefaultHasher};
use std::hash::{Hash, Hasher};
use std::io::{Read, Seek, SeekFrom};
use std::path::{Path as StdPath, PathBuf};
use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::{Duration, Instant};

use crate::tree::NodeBody;
use crate::tree_refs::TreeRef;
use dashmap::DashMap;
use futures::future::{BoxFuture, FutureExt};
use omnifs_api::events::InspectorOutcome;
use omnifs_core::path::Path;
use tokio::runtime::Handle;
use tokio::sync::broadcast;
use tracing::Instrument;

use super::{
    Attrs, DirCursor, DirEntry, DirPage, EntryKind, EventStream, LookupAnswer, Namespace, NsError,
    NsEvent, ReadAnswer, ReadStyle, StabilityClass, view_types,
};
use crate::inspect;
use crate::registry::{MountEntry, MountTable};
use crate::tree::{HostKind, ListOutcome, RangedHandle, ReadResult, RequestCtx};
use crate::view::{EntryMeta, FileAttrsCache, FileSize};
use crate::{Engine, TreeError, TreeErrorKind};

/// Effectively-infinite protocol TTL for stable exact-size entries.
const TTL_STATIC: Duration = Duration::from_secs(u32::MAX as u64);
/// Zero TTL for entries whose size or content can move under the reader.
const TTL_DYNAMIC: Duration = Duration::ZERO;
const EVENT_CAPACITY: usize = 1024;
const DRAIN_TICK: Duration = Duration::from_millis(100);
#[allow(clippy::duration_suboptimal_units)] // 60s reads clearer than 1min here.
const HANDLE_IDLE: Duration = Duration::from_secs(60);
const MOUNT_ENUMERATION_MOUNT: &str = "";

impl EntryKind {
    fn from_node(node: &crate::Node) -> Self {
        if node.is_dir() {
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
        let mode = match &kind {
            EntryKind::Directory => 0o555,
            EntryKind::File => 0o444,
            EntryKind::Symlink => 0o777,
        };
        let nlink = match &kind {
            EntryKind::Directory => 2,
            EntryKind::File | EntryKind::Symlink => 1,
        };
        Self {
            kind,
            dev: 0,
            ino: 0,
            size: attrs.map_or(0, FileAttrsCache::st_size),
            blocks: attrs.map_or(0, |attrs| FileAttrsCache::st_size(attrs).div_ceil(512)),
            mode,
            nlink,
            accessed: None,
            modified: None,
            created: None,
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
        offline: bool,
    ) -> Self {
        if budget == 0 || entries.len() <= budget {
            return Self {
                entries,
                next: then.map(DirCursor::Provider),
            };
        }
        let overflow = entries.split_off(budget);
        Self {
            entries,
            next: Some(DirCursor::Buffered {
                entries: overflow,
                then,
                offline,
            }),
        }
    }
}

impl From<TreeError> for NsError {
    fn from(err: TreeError) -> Self {
        match err.kind {
            TreeErrorKind::NotFound => Self::NotFound,
            TreeErrorKind::OfflineMiss => Self::OfflineMiss,
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

impl From<NsError> for TreeError {
    fn from(error: NsError) -> Self {
        match error {
            NsError::NotFound => TreeError::not_found("host entry not found"),
            NsError::OfflineMiss => TreeError::offline_miss("offline projection miss"),
            NsError::NotDirectory => TreeError {
                kind: TreeErrorKind::NotDirectory,
                message: "host parent is not a directory".to_string(),
                retryable: false,
                retry_after: None,
            },
            NsError::IsDirectory => TreeError::is_directory("host entry is a directory"),
            NsError::Permission => TreeError {
                kind: TreeErrorKind::PermissionDenied,
                message: "host permission denied".to_string(),
                retryable: false,
                retry_after: None,
            },
            NsError::Invalid => TreeError::invalid_input("invalid host path"),
            NsError::TooLarge => TreeError::too_large("host entry is too large"),
            NsError::RateLimited { retry_after } => TreeError {
                kind: TreeErrorKind::RateLimited,
                message: "host operation rate limited".to_string(),
                retryable: true,
                retry_after,
            },
            NsError::Timeout => TreeError {
                kind: TreeErrorKind::Timeout,
                message: "host operation timed out".to_string(),
                retryable: true,
                retry_after: None,
            },
            NsError::Network => TreeError {
                kind: TreeErrorKind::Network,
                message: "host operation failed due to network".to_string(),
                retryable: true,
                retry_after: None,
            },
            NsError::Internal { message } => TreeError::internal(message),
        }
    }
}

/// One entry in the engine identity table.
struct NodeRecord {
    /// Best-known file attrs, preserving a learned size across placeholder
    /// refreshes (the engine-internal learned-size writeback).
    attrs: Option<FileAttrsCache>,
    host: Option<HostRecord>,
}

#[derive(Clone)]
struct HostRecord {
    tree_ref: TreeRef,
    relative: PathBuf,
    kind: HostKind,
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

/// The engine-owned [`Namespace`] implementation. Owns node identity, the invalidation
/// epoch and event fan-out, and the ranged-handle cache.
pub struct TreeNamespace {
    registry: Arc<MountTable>,
    rt: Handle,
    ids: DashMap<Path, NodeRecord>,
    epoch: AtomicU64,
    /// id -> the epoch of its last invalidation, folded into `Attrs::change`.
    node_epochs: DashMap<Path, u64>,
    events: broadcast::Sender<NsEvent>,
    handles: DashMap<Path, HandleRecord>,
    /// Count of ranged opens that yielded a handle; a test hook for asserting
    /// handle reuse.
    open_count: AtomicU64,
    tick: std::sync::Mutex<Option<tokio::task::AbortHandle>>,
}

impl TreeNamespace {
    /// Production constructor: build the namespace over the immutable mount registry and
    /// start the background invalidation drain. The returned value is the
    /// frontend's complete `dyn Namespace` implementation.
    pub fn online(registry: Arc<MountTable>, rt: Handle) -> Arc<Self> {
        assert!(
            !registry.is_offline(),
            "online namespace requires an online mount table"
        );
        Self::construct(registry, rt, true)
    }

    pub fn offline(registry: Arc<MountTable>, rt: Handle) -> Arc<Self> {
        assert!(
            registry.is_offline(),
            "offline namespace requires an offline mount table"
        );
        Self::construct(registry, rt, false)
    }

    fn construct(registry: Arc<MountTable>, rt: Handle, spawn_drain: bool) -> Arc<Self> {
        let (events, _) = broadcast::channel(EVENT_CAPACITY);
        let this = Arc::new(Self {
            registry,
            rt,
            ids: DashMap::new(),
            epoch: AtomicU64::new(0),
            node_epochs: DashMap::new(),
            events,
            handles: DashMap::new(),
            open_count: AtomicU64::new(0),
            tick: std::sync::Mutex::new(None),
        });
        this.install_root();
        if spawn_drain {
            this.spawn_drain_tick();
        }
        this
    }

    pub(crate) fn runtime_for(&self, mount: &str) -> Result<Arc<Engine>, TreeError> {
        self.entry_for(mount)?
            .runtime()
            .ok_or_else(|| TreeError::offline_miss(format!("mount {mount} is cache-only")))
    }

    pub(crate) fn entry_for(&self, mount: &str) -> Result<&MountEntry, TreeError> {
        self.registry
            .entry(mount)
            .ok_or_else(|| TreeError::not_found(format!("no such mount: {mount}")))
    }

    pub(crate) fn registry_runtime(&self, mount: &str) -> Option<Arc<Engine>> {
        self.registry.get(mount)
    }

    pub(crate) fn split_mount_path(&self, path: &Path) -> Result<(String, Path), TreeError> {
        if path.is_root() {
            return Ok((MOUNT_ENUMERATION_MOUNT.to_string(), Path::root()));
        }
        let mut segments = path.segments();
        let Some(mount) = segments.next() else {
            return Err(TreeError::invalid_input(format!(
                "split_mount_path: empty path: {}",
                path.as_str()
            )));
        };
        let mount = mount.to_string();
        if self.registry.entry(&mount).is_none() {
            return Err(TreeError::not_found(format!("no such mount: {mount}")));
        }
        let rest = path
            .as_str()
            .strip_prefix('/')
            .and_then(|path| path.strip_prefix(&mount))
            .filter(|s| !s.is_empty())
            .unwrap_or("/");
        let rel = Path::parse(rest).map_err(|error| {
            TreeError::invalid_input(format!("invalid mount-relative path: {error}"))
        })?;
        Ok((mount, rel))
    }

    pub(crate) fn is_mount_enumeration_root(&self, mount: &str, path: &Path) -> bool {
        mount == MOUNT_ENUMERATION_MOUNT && path.is_root()
    }

    pub(crate) fn mount_names(&self) -> Vec<String> {
        let mut mounts = self.registry.mounts();
        mounts.sort();
        mounts
    }

    /// The root record is the synthetic mount-enumeration root.
    fn install_root(&self) {
        let root = NodeRecord {
            attrs: None,
            host: None,
        };
        self.ids.insert(Path::root(), root);
    }

    fn spawn_drain_tick(self: &Arc<Self>) {
        let weak = Arc::downgrade(self);
        let handle = self.rt.spawn(async move {
            loop {
                tokio::time::sleep(DRAIN_TICK).await;
                let Some(this) = weak.upgrade() else {
                    break;
                };
                for mount in this.registry.mounts() {
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

    fn record(&self, id: &Path) -> Result<(Path, String), NsError> {
        let full_path = id.clone();
        let mount = full_path
            .segments()
            .next()
            .map_or_else(String::new, ToOwned::to_owned);
        Ok((full_path, mount))
    }

    // --- inspector tracing ---------------------------------------------------
    //
    // `TreeNamespace` owns request spans because structural paths are opaque to
    // frontends. Only this id table can map a node to the mount and path that
    // the inspector records; child provider and callout spans inherit the
    // request span through tracing.
    fn begin_span(op: &'static str, mount: &str, path: &str) -> tracing::Span {
        inspect::request_span(op, mount, path)
    }

    /// Inspector paths are mount-relative, while namespace records keep the
    /// full protocol path needed to resolve a node. The synthetic root has no
    /// mount, so it gets an explicit identity instead of a blank mount row.
    fn inspector_identity(&self, mount: &str, full_path: &Path) -> (String, String) {
        inspector_identity(mount, full_path)
    }

    /// Record every completed request, including successful and failed
    /// operations. The tracing inspector otherwise treats an unset outcome as
    /// an internal failure when the root span closes.
    fn record_outcome<T>(span: &tracing::Span, result: &Result<T, NsError>) {
        let outcome = result
            .as_ref()
            .map_or_else(Self::outcome_for, |_| InspectorOutcome::Ok);
        inspect::record_outcome(span, outcome);
    }

    fn outcome_for(error: &NsError) -> InspectorOutcome {
        match error {
            NsError::NotFound => InspectorOutcome::NotFound,
            NsError::OfflineMiss => InspectorOutcome::Internal,
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
    fn intern(&self, node: &crate::Node) -> Path {
        let full_path = self.full_path_for(node);
        let id = full_path.clone();

        let merged = FileAttrsCache::merge_preserving_learned_size(
            self.ids.get(&id).and_then(|r| r.attrs.clone()).as_ref(),
            node.attrs().cloned(),
        );
        let incoming_host = node.host().map(|(tree_ref, relative, kind)| HostRecord {
            tree_ref: tree_ref.clone(),
            relative: relative.clone(),
            kind,
        });
        let host = merge_host_record(
            self.ids.get(&id).and_then(|record| record.host.clone()),
            incoming_host,
        );
        self.ids.insert(
            id.clone(),
            NodeRecord {
                attrs: merged,
                host,
            },
        );
        id
    }

    /// The full protocol path for a freshly resolved node.
    fn full_path_for(&self, node: &crate::Node) -> Path {
        let mount = node.mount();
        let rel = node.path();
        let joined = if rel.is_root() {
            format!("/{mount}")
        } else {
            format!("/{mount}{}", rel.as_str())
        };
        Path::parse(&joined).expect("interned mount-relative paths are valid")
    }

    /// Re-resolve a path to a fresh internal node. The resolver round-trips the
    /// full protocol path across the mount-enumeration namespace.
    async fn resolve_node(&self, full_path: &Path) -> Result<crate::Node, NsError> {
        let mut chain = Vec::new();
        let mut current = full_path.clone();
        let anchor = loop {
            if let Some(record) = self.ids.get(&current)
                && let Some(host) = &record.host
            {
                break crate::Node::new(
                    current.segments().next().unwrap_or_default().to_string(),
                    host_node_path(&current),
                    EntryMeta::directory(),
                    NodeBody::Host {
                        tree_ref: host.tree_ref.clone(),
                        relative: host.relative.clone(),
                        kind: host.kind,
                    },
                );
            }
            let Some((parent, name)) = current.parent_and_name() else {
                break self
                    .resolve(&current, &RequestCtx)
                    .await
                    .map_err(NsError::from)?;
            };
            chain.push(name.to_string());
            current = parent;
        };

        let mut node = anchor;
        for name in chain.into_iter().rev() {
            node = self.resolve_child(&node, &name, &RequestCtx).await?;
        }
        Ok(node)
    }

    // --- invalidation -------------------------------------------------------

    /// Drain a mount's pending invalidations, map them to known ids, bump the
    /// epoch once, emit an event per affected id, and evict that id's derived
    /// state (attrs + ranged handle) while preserving its stable identity.
    fn process_invalidations(&self, mount: &str) {
        let report = self.drain_invalidations(mount);
        if report.is_empty() {
            return;
        }

        let exact_paths: Vec<Path> = report
            .paths
            .iter()
            .chain(report.changed_dirs.iter())
            .map(|rel| mount_full_path(mount, rel))
            .collect();
        let prefix_paths: Vec<Path> = report
            .prefixes
            .iter()
            .map(|rel| mount_full_path(mount, rel))
            .collect();
        let mut affected: Vec<Path> = self
            .ids
            .iter()
            .filter_map(|entry| {
                if entry.key().segments().next() != Some(mount) {
                    return None;
                }
                let hit = exact_paths.iter().any(|path| path == entry.key())
                    || prefix_paths
                        .iter()
                        .any(|prefix| entry.key().has_prefix(prefix));
                hit.then(|| entry.key().clone())
            })
            .collect();

        // Effects are authoritative even when a frontend presents a persisted
        // structural path that has never been interned in this daemon instance.
        // Convert every effect path into its full namespace path before updating
        // the private change table or publishing the event.
        affected.extend(exact_paths.iter().chain(prefix_paths.iter()).cloned());
        let mut unique = HashSet::new();
        affected.retain(|path| unique.insert(path.clone()));

        let epoch = self.epoch.fetch_add(1, Ordering::SeqCst) + 1;
        let invalidation_paths: Vec<Path> = exact_paths
            .iter()
            .chain(prefix_paths.iter())
            .cloned()
            .collect();
        let handles: Vec<Path> = self
            .handles
            .iter()
            .filter_map(|entry| {
                let path = entry.key();
                (invalidation_paths
                    .iter()
                    .any(|affected| path == affected || path.has_prefix(affected)))
                .then(|| path.clone())
            })
            .collect();
        for path in handles {
            self.handles.remove(&path);
            self.node_epochs.insert(path, epoch);
        }
        for id in affected {
            self.node_epochs.insert(id.clone(), epoch);
            // Drop the learned attrs so the next answer re-resolves; keep the
            // identity so a frontend's cached id stays resolvable.
            if let Some(mut record) = self.ids.get_mut(&id) {
                record.attrs = None;
                record.host = None;
            }
            // Evicting the handle closes it and aborts its pump (Drop).
            let _ = self.events.send(NsEvent::InvalidateSubtree { path: id });
        }
    }

    fn sweep_idle_handles(&self) {
        let stale: Vec<Path> = self
            .handles
            .iter()
            .filter_map(|entry| {
                (entry.value().last_use.elapsed() >= HANDLE_IDLE).then(|| entry.key().clone())
            })
            .collect();
        for id in stale {
            self.handles.remove(&id);
        }
    }

    // --- attrs --------------------------------------------------------------

    /// Build the policied [`Attrs`] for a node from its best-known file attrs.
    fn attrs_for(&self, id: &Path, node: &crate::Node) -> Attrs {
        let record = self.ids.get(&id);
        let attrs = record
            .as_ref()
            .and_then(|record| record.attrs.as_ref())
            .or_else(|| node.attrs());
        self.attrs_from_parts(id, node, attrs)
    }

    fn attrs_from_parts(
        &self,
        id: &Path,
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
        id: &Path,
        node: &crate::Node,
        attrs: Option<&FileAttrsCache>,
        epoch: u64,
    ) -> u64 {
        let change = change_counter(node, attrs, epoch);
        if !id.is_root() {
            return change;
        }
        let mut hasher = DefaultHasher::new();
        change.hash(&mut hasher);
        let mut mounts = self.registry.mounts();
        mounts.sort();
        mounts.hash(&mut hasher);
        hasher.finish()
    }

    // --- read ---------------------------------------------------------------

    async fn read_inner(&self, id: Path, offset: u64, len: u32) -> Result<ReadAnswer, NsError> {
        let (full_path, mount) = self.record(&id)?;
        self.process_invalidations(&mount);
        let (display_mount, display_path) = self.inspector_identity(&mount, &full_path);
        let span = Self::begin_span("read", &display_mount, &display_path);
        let result = async {
            // A live ranged handle already open on this node serves the read
            // without re-resolving, so follow reads still remain inside the
            // namespace request span even though they bypass provider dispatch.
            if let Some(handle) = self.take_cached_handle(&id) {
                return self.read_ranged(&id, &handle, offset, len).await;
            }

            let node = self.resolve_node(&full_path).await?;

            if let Some((tree_ref, relative, _)) = node.host() {
                return self.host_read(tree_ref, relative, offset, len).await;
            }

            if node.is_dir() {
                return Err(NsError::IsDirectory);
            }

            // A ranged route projects a `Deferred(Ranged)` placeholder, so open a
            // provider handle and cache it; a full/whole file takes the
            // full-read path. The provider open probe returning `None` means the route
            // declared ranged but the handler answered full: fall through to
            // the full read.
            if node.attrs().is_some_and(FileAttrsCache::is_deferred_ranged)
                && let Some(handle) = self.open(&node).await?
            {
                self.open_count.fetch_add(1, Ordering::Relaxed);
                let handle = self.cache_handle(&id, &node, handle);
                return self.read_ranged(&id, &handle, offset, len).await;
            }

            self.read_full(&id, &node, offset, len).await
        }
        .instrument(span.clone())
        .await;
        Self::record_outcome(&span, &result);
        result
    }

    async fn read_ranged(
        &self,
        id: &Path,
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
        id: &Path,
        node: &crate::Node,
        offset: u64,
        len: u32,
    ) -> Result<ReadAnswer, NsError> {
        match self.read(node, &RequestCtx).await? {
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
            // A subtree node is a directory; its files are served directly by the
            // projection tree from the backing directory, never through this read
            // path (provider-backed content only).
        }
    }

    /// Compute `Attrs` for a read answer, folding in the size the read learned.
    fn attrs_for_read(&self, id: &Path, kind: EntryKind, attrs: Option<&FileAttrsCache>) -> Attrs {
        let epoch = self.node_epochs.get(&id).map_or(0, |e| *e);
        Attrs::from_cache(kind, attrs, change_counter_parts(id, attrs, epoch))
    }

    fn store_learned(&self, id: &Path, learned: FileAttrsCache) {
        if let Some(mut record) = self.ids.get_mut(&id) {
            record.attrs =
                FileAttrsCache::merge_preserving_learned_size(record.attrs.as_ref(), Some(learned));
        }
    }

    fn take_cached_handle(&self, id: &Path) -> Option<Arc<RangedHandle>> {
        let mut record = self.handles.get_mut(&id)?;
        record.last_use = Instant::now();
        Some(Arc::clone(&record.handle))
    }

    /// Cache a freshly opened ranged handle, spawning a live-follow pump for a
    /// live file when a registry is available (the production form). The pump
    /// grows the node's attrs and emits an `AttrsChanged` event.
    fn cache_handle(
        &self,
        id: &Path,
        node: &crate::Node,
        handle: RangedHandle,
    ) -> Arc<RangedHandle> {
        let handle = Arc::new(handle);
        let pump = self.spawn_pump(id, node, &handle);
        self.handles.insert(
            id.clone(),
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
        id: &Path,
        node: &crate::Node,
        handle: &Arc<RangedHandle>,
    ) -> Option<tokio::task::AbortHandle> {
        if !matches!(handle.attrs().stability(), view_types::Stability::Live) {
            return None;
        }
        let registry = Arc::clone(&self.registry);
        let mount = node.mount().to_string();
        let base = handle.attrs().clone();
        let events = self.events.clone();
        let node_epoch = self.node_epochs.get(&id).map_or(0, |e| *e);
        // The pump is a detached task; it reports growth by cloning the shared
        // pieces it needs (no back-reference to `self`).
        let id = id.clone();
        let record_growth = move |new_end: u64| {
            let grown = base.clone().with_exact_size(new_end);
            let attrs = Attrs::from_cache(
                EntryKind::File,
                Some(&grown),
                change_counter_parts(&id, Some(&grown), node_epoch),
            );
            let _ = events.send(NsEvent::AttrsChanged {
                path: id.clone(),
                attrs,
            });
        };
        Some(crate::spawn_live_follow_pump(
            &self.rt,
            registry,
            mount,
            handle.provider_handle(),
            handle.observed_end(),
            handle.open_epoch,
            record_growth,
        ))
    }

    // --- readdir ------------------------------------------------------------

    async fn readdir_inner(
        &self,
        id: Path,
        cursor: DirCursor,
        budget: usize,
    ) -> Result<DirPage, NsError> {
        let (full_path, mount) = self.record(&id)?;
        self.process_invalidations(&mount);
        let (display_mount, display_path) = self.inspector_identity(&mount, &full_path);
        let span = Self::begin_span("readdir", &display_mount, &display_path);
        let result = async {
            // A buffered cursor is pure overflow the previous page held back;
            // serve it inside the request span without touching the tree.
            if let DirCursor::Buffered {
                entries,
                then,
                offline,
            } = cursor
            {
                if offline != self.registry.is_offline() {
                    return Err(if self.registry.is_offline() {
                        NsError::OfflineMiss
                    } else {
                        NsError::Invalid
                    });
                }
                return Ok(DirPage::with_budget(entries, then, budget, offline));
            }

            let node = self.resolve_node(&full_path).await?;
            if let Some((tree_ref, relative, _)) = node.host() {
                let metadata = self.host_stat(tree_ref, relative).await?;
                if metadata.kind != EntryKind::Directory {
                    return Err(NsError::NotDirectory);
                }
                let entries = self
                    .host_listing(tree_ref, relative)
                    .await?
                    .into_iter()
                    .filter_map(|(name, child_relative, metadata)| {
                        let full = full_path.join(&name).ok()?;
                        let child = self.intern_host(full, child_relative, tree_ref, &metadata);
                        Some(DirEntry {
                            name,
                            path: child,
                            attrs: metadata.attrs(self.host_change(&metadata)),
                        })
                    })
                    .collect();
                return Ok(DirPage::with_budget(
                    entries,
                    None,
                    budget,
                    self.registry.is_offline(),
                ));
            }
            if !node.is_dir() {
                return Err(NsError::NotDirectory);
            }

            let tree_cursor = match cursor {
                DirCursor::Start => None,
                DirCursor::Provider(c) => Some(crate::Cursor(c)),
                DirCursor::Buffered { .. } => unreachable!("buffered handled above"),
            };
            let listing = match self.list(&node, tree_cursor, &RequestCtx).await? {
                ListOutcome::Listing(listing) => listing,
                ListOutcome::Host(_) => {
                    return Err(NsError::Internal {
                        message: "host listing escaped namespace traversal".to_string(),
                    });
                },
            };

            let parent_full = full_path;
            let entries = listing
                .entries
                .iter()
                .map(|entry| self.dir_entry(&parent_full, &entry.name, &entry.meta))
                .collect();
            let tree_next = listing.next_cursor.map(|c| c.0);
            Ok(DirPage::with_budget(
                entries,
                tree_next,
                budget,
                self.registry.is_offline(),
            ))
        }
        .instrument(span.clone())
        .await;
        Self::record_outcome(&span, &result);
        result
    }

    /// Turn a listing child into a `DirEntry`, allocating its id.
    fn dir_entry(&self, parent_full: &Path, name: &str, meta: &EntryMeta) -> DirEntry {
        let full = parent_full
            .join(name)
            .unwrap_or_else(|_| parent_full.clone());
        let id = full.clone();
        let attrs = meta.attrs().cloned();
        let merged = FileAttrsCache::merge_preserving_learned_size(
            self.ids.get(&id).and_then(|r| r.attrs.clone()).as_ref(),
            attrs,
        );
        let host = merge_host_record(
            self.ids.get(&id).and_then(|record| record.host.clone()),
            None,
        );
        self.ids.insert(
            id.clone(),
            NodeRecord {
                attrs: merged.clone(),
                host,
            },
        );
        let epoch = self.node_epochs.get(&id).map_or(0, |e| *e);
        let kind = EntryKind::from_meta(meta);
        let attrs = Attrs::from_cache(
            kind.clone(),
            merged.as_ref(),
            change_counter_parts(&id, merged.as_ref(), epoch),
        );
        DirEntry {
            attrs,
            name: name.to_string(),
            path: id,
        }
    }

    // --- getattr ------------------------------------------------------------

    async fn getattr_inner(&self, id: Path, exact: bool) -> Result<Attrs, NsError> {
        let (full_path, mount) = self.record(&id)?;
        self.process_invalidations(&mount);
        let op = if exact { "getattr_exact" } else { "getattr" };
        let (display_mount, display_path) = self.inspector_identity(&mount, &full_path);
        let span = Self::begin_span(op, &display_mount, &display_path);
        let result = async {
            let node = self.resolve_node(&full_path).await?;
            if let Some((tree_ref, relative, _)) = node.host() {
                let metadata = self.host_stat(tree_ref, relative).await?;
                return Ok(metadata.attrs(self.host_change(&metadata)));
            }
            let refreshed = self.refresh_record(&id, &node);

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
                && let Some(probed) = self.probe_ranged_attrs(node.mount(), node.path()).await?
            {
                self.store_learned(&id, probed.clone());
                return Ok(self.attrs_from_parts(&id, &node, Some(&probed)));
            }

            Ok(self.attrs_from_parts(&id, &node, refreshed.as_ref()))
        }
        .instrument(span.clone())
        .await;
        Self::record_outcome(&span, &result);
        result
    }

    /// Refresh a record's stored attrs from a fresh resolve, preserving a learned
    /// size, and return the best-known attrs.
    fn refresh_record(&self, id: &Path, node: &crate::Node) -> Option<FileAttrsCache> {
        let merged = FileAttrsCache::merge_preserving_learned_size(
            self.ids.get(&id).and_then(|r| r.attrs.clone()).as_ref(),
            node.attrs().cloned(),
        );
        if let Some(mut record) = self.ids.get_mut(&id) {
            record.attrs.clone_from(&merged);
        }
        merged
    }

    pub(crate) async fn resolve_host_child(
        &self,
        parent: &crate::Node,
        name: &str,
    ) -> Result<crate::Node, NsError> {
        let Some((tree_ref, relative, _)) = parent.host() else {
            return Err(NsError::Internal {
                message: "host child requires a host node".to_string(),
            });
        };
        let child_relative = host_child(relative, name).map_err(|_| NsError::Invalid)?;
        let metadata = self.host_stat(tree_ref, &child_relative).await?;
        let path = parent.path().join(name).map_err(|_| NsError::Invalid)?;
        Ok(crate::Node::new(
            parent.mount().to_string(),
            path,
            // HostKind, refreshed through symlink_metadata, is the only kind
            // authority for host nodes. EntryMeta is an inert provider-tree
            // placeholder and is never consulted for host projection.
            EntryMeta::directory(),
            NodeBody::Host {
                tree_ref: tree_ref.clone(),
                relative: child_relative,
                kind: host_kind(&metadata.kind),
            },
        ))
    }

    async fn host_stat(
        &self,
        tree_ref: &TreeRef,
        relative: &StdPath,
    ) -> Result<HostMetadata, NsError> {
        let root = Arc::clone(&tree_ref.root);
        let relative = relative.to_path_buf();
        self.rt
            .spawn_blocking(move || {
                let metadata = if relative.as_os_str().is_empty() {
                    root.dir_metadata()
                } else {
                    root.symlink_metadata(&relative)
                };
                metadata.map(|metadata| HostMetadata::from_cap(&metadata))
            })
            .await
            .map_err(|error| NsError::Internal {
                message: error.to_string(),
            })?
            .map_err(ns_error_from_io)
    }

    async fn host_listing(
        &self,
        tree_ref: &TreeRef,
        relative: &StdPath,
    ) -> Result<Vec<(String, PathBuf, HostMetadata)>, NsError> {
        let root = Arc::clone(&tree_ref.root);
        let relative = relative.to_path_buf();
        self.rt
            .spawn_blocking(move || {
                let mut entries = Vec::new();
                let read_dir = if relative.as_os_str().is_empty() {
                    root.entries()
                } else {
                    root.read_dir(&relative)
                };
                let read_dir = read_dir.map_err(ns_error_from_io)?;
                for entry in read_dir {
                    let entry = entry.map_err(ns_error_from_io)?;
                    let Some(name) = entry.file_name().to_str().map(ToOwned::to_owned) else {
                        continue;
                    };
                    let child = host_child(&relative, &name).map_err(|_| NsError::Invalid)?;
                    let metadata = root.symlink_metadata(&child).map_err(ns_error_from_io)?;
                    entries.push((name, child, HostMetadata::from_cap(&metadata)));
                }
                Ok::<_, NsError>(entries)
            })
            .await
            .map_err(|error| NsError::Internal {
                message: error.to_string(),
            })?
            .map_err(|error| error)
    }

    async fn host_read(
        &self,
        tree_ref: &TreeRef,
        relative: &StdPath,
        offset: u64,
        len: u32,
    ) -> Result<ReadAnswer, NsError> {
        let metadata = self.host_stat(tree_ref, relative).await?;
        if metadata.kind == EntryKind::Directory {
            return Err(NsError::IsDirectory);
        }
        if metadata.kind == EntryKind::Symlink {
            return Err(NsError::Invalid);
        }
        if offset >= metadata.size {
            return Ok(ReadAnswer {
                bytes: Vec::new(),
                eof: true,
                attrs: metadata.attrs(self.host_change(&metadata)),
            });
        }
        let read_len = u64::from(len).min(metadata.size - offset) as usize;
        let root = Arc::clone(&tree_ref.root);
        let relative = relative.to_path_buf();
        let bytes = self
            .rt
            .spawn_blocking(move || {
                let mut file = root.open(&relative)?;
                file.seek(SeekFrom::Start(offset))?;
                let mut bytes = vec![0; read_len];
                let count = file.read(&mut bytes)?;
                bytes.truncate(count);
                Ok::<_, std::io::Error>(bytes)
            })
            .await
            .map_err(|error| NsError::Internal {
                message: error.to_string(),
            })?
            .map_err(ns_error_from_io)?;
        let eof = offset.saturating_add(bytes.len() as u64) >= metadata.size;
        Ok(ReadAnswer {
            bytes,
            eof,
            attrs: metadata.attrs(self.host_change(&metadata)),
        })
    }

    async fn host_readlink(
        &self,
        tree_ref: &TreeRef,
        relative: &StdPath,
    ) -> Result<PathBuf, NsError> {
        let root = Arc::clone(&tree_ref.root);
        let relative = relative.to_path_buf();
        self.rt
            .spawn_blocking(move || root.read_link_contents(&relative))
            .await
            .map_err(|error| NsError::Internal {
                message: error.to_string(),
            })?
            .map_err(ns_error_from_io)
    }

    fn host_change(&self, metadata: &HostMetadata) -> u64 {
        let mut hasher = DefaultHasher::new();
        metadata.dev.hash(&mut hasher);
        metadata.ino.hash(&mut hasher);
        metadata.size.hash(&mut hasher);
        metadata.modified.hash(&mut hasher);
        hasher.finish()
    }

    fn intern_host(
        &self,
        full_path: Path,
        relative: PathBuf,
        tree_ref: &TreeRef,
        metadata: &HostMetadata,
    ) -> Path {
        let id = full_path.clone();
        let host = merge_host_record(
            self.ids.get(&id).and_then(|record| record.host.clone()),
            Some(HostRecord {
                tree_ref: tree_ref.clone(),
                relative,
                kind: host_kind(&metadata.kind),
            }),
        );
        self.ids
            .insert(id.clone(), NodeRecord { attrs: None, host });
        let _ = metadata;
        id
    }
}

fn mount_relative_path(mount: &str, full_path: &Path) -> Path {
    let mount_root =
        Path::parse(&format!("/{mount}")).expect("interned host paths have a valid mount root");
    full_path
        .strip_prefix(&mount_root)
        .expect("interned host paths are rooted in their mount")
}

fn mount_full_path(mount: &str, relative: &Path) -> Path {
    let joined = if relative.is_root() {
        format!("/{mount}")
    } else {
        format!("/{mount}{}", relative.as_str())
    };
    Path::parse(&joined).expect("effect paths are valid mount-relative paths")
}

fn host_node_path(full_path: &Path) -> Path {
    let mount = full_path
        .segments()
        .next()
        .expect("host paths have a mount segment");
    mount_relative_path(mount, full_path)
}

fn merge_host_record(
    existing: Option<HostRecord>,
    incoming: Option<HostRecord>,
) -> Option<HostRecord> {
    let _ = existing;
    incoming
}

#[derive(Debug, Clone)]
struct HostMetadata {
    kind: EntryKind,
    dev: u64,
    ino: u64,
    size: u64,
    blocks: u64,
    mode: u16,
    nlink: u32,
    accessed: Option<u64>,
    modified: Option<u64>,
    created: Option<u64>,
}

impl HostMetadata {
    fn from_cap(metadata: &cap_std::fs::Metadata) -> Self {
        use cap_std::fs::MetadataExt;
        let kind = if metadata.is_dir() {
            EntryKind::Directory
        } else if metadata.is_symlink() {
            EntryKind::Symlink
        } else {
            EntryKind::File
        };
        #[cfg(unix)]
        let (dev, ino, blocks, mode, nlink) = (
            metadata.dev(),
            metadata.ino(),
            metadata.blocks(),
            u16::try_from(metadata.mode() & 0xffff).unwrap_or(0),
            u32::try_from(metadata.nlink()).unwrap_or(1),
        );
        #[cfg(not(unix))]
        let (dev, ino, blocks, mode, nlink) = (0, 0, 0, 0, 1);
        Self {
            kind,
            dev,
            ino,
            size: metadata.len(),
            blocks,
            mode,
            nlink,
            accessed: timestamp_millis(metadata.accessed()),
            modified: timestamp_millis(metadata.modified()),
            created: timestamp_millis(metadata.created()),
        }
    }

    fn attrs(&self, change: u64) -> Attrs {
        let mode = match self.kind {
            EntryKind::Directory => 0o555,
            EntryKind::Symlink => 0o777,
            EntryKind::File => 0o444 | (self.mode & 0o111),
        };
        Attrs {
            kind: self.kind.clone(),
            dev: self.dev,
            ino: self.ino,
            size: self.size,
            blocks: self.blocks,
            mode,
            nlink: self.nlink,
            accessed: self.accessed,
            modified: self.modified,
            created: self.created,
            ttl: TTL_STATIC,
            change,
            direct_io: false,
            stability: StabilityClass::Stable,
            read_style: if self.kind == EntryKind::File {
                ReadStyle::Ranged
            } else {
                ReadStyle::Whole
            },
        }
    }
}

fn timestamp_millis(value: std::io::Result<cap_std::time::SystemTime>) -> Option<u64> {
    value
        .ok()
        .and_then(|time| {
            time.into_std()
                .duration_since(std::time::SystemTime::UNIX_EPOCH)
                .ok()
        })
        .map(|duration| u64::try_from(duration.as_millis()).unwrap_or(u64::MAX))
}

fn ns_error_from_io(error: std::io::Error) -> NsError {
    match error.kind() {
        std::io::ErrorKind::NotFound => NsError::NotFound,
        std::io::ErrorKind::PermissionDenied => NsError::Permission,
        std::io::ErrorKind::NotADirectory => NsError::NotDirectory,
        std::io::ErrorKind::InvalidInput => NsError::Invalid,
        _ => NsError::Internal {
            message: error.to_string(),
        },
    }
}

fn host_kind(kind: &EntryKind) -> HostKind {
    match kind {
        EntryKind::Directory => HostKind::Directory,
        EntryKind::File => HostKind::File,
        EntryKind::Symlink => HostKind::Symlink,
    }
}

fn host_child(parent: &StdPath, name: &str) -> Result<PathBuf, TreeError> {
    let path = StdPath::new(name);
    let mut components = path.components();
    if !matches!(components.next(), Some(std::path::Component::Normal(_)))
        || components.next().is_some()
    {
        return Err(TreeError::invalid_input(format!(
            "invalid host child name: {name:?}"
        )));
    }
    let mut child = parent.to_path_buf();
    child.push(name);
    Ok(child)
}

impl Namespace for TreeNamespace {
    fn lookup<'a>(
        &'a self,
        parent: Path,
        name: &'a str,
    ) -> BoxFuture<'a, Result<LookupAnswer, NsError>> {
        async move {
            let (parent_full, mount) = self.record(&parent)?;
            self.process_invalidations(&mount);
            let child_full = parent_full.join(name).map_err(|_| NsError::Invalid)?;
            let (display_mount, display_path) = self.inspector_identity(&mount, &child_full);
            let span = Self::begin_span("lookup", &display_mount, &display_path);
            let result = async {
                let parent_node = self.resolve_node(&parent_full).await?;
                let node = self.resolve_child(&parent_node, name, &RequestCtx).await?;
                let id = self.intern(&node);
                let attrs = if let Some((tree_ref, relative, _)) = node.host() {
                    let metadata = self.host_stat(tree_ref, relative).await?;
                    metadata.attrs(self.host_change(&metadata))
                } else {
                    self.attrs_for(&id, &node)
                };
                Ok(LookupAnswer { path: id, attrs })
            }
            .instrument(span.clone())
            .await;
            Self::record_outcome(&span, &result);
            result
        }
        .boxed()
    }

    fn getattr(&self, node: Path) -> BoxFuture<'_, Result<Attrs, NsError>> {
        self.getattr_inner(node, false).boxed()
    }

    fn getattr_exact(&self, node: Path) -> BoxFuture<'_, Result<Attrs, NsError>> {
        self.getattr_inner(node, true).boxed()
    }

    fn readdir(
        &self,
        node: Path,
        cursor: DirCursor,
        budget: usize,
    ) -> BoxFuture<'_, Result<DirPage, NsError>> {
        self.readdir_inner(node, cursor, budget).boxed()
    }

    fn read(
        &self,
        node: Path,
        offset: u64,
        len: u32,
    ) -> BoxFuture<'_, Result<ReadAnswer, NsError>> {
        self.read_inner(node, offset, len).boxed()
    }

    fn readlink(&self, node: Path) -> BoxFuture<'_, Result<PathBuf, NsError>> {
        async move {
            let (full_path, _) = self.record(&node)?;
            let node = self.resolve_node(&full_path).await?;
            let Some((tree_ref, relative, _)) = node.host() else {
                return Err(NsError::Invalid);
            };
            let metadata = self.host_stat(tree_ref, relative).await?;
            if metadata.kind != EntryKind::Symlink {
                return Err(NsError::Invalid);
            }
            self.host_readlink(tree_ref, relative).await
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

const INSPECTOR_SYNTHETIC_ROOT: &str = "<root>";

fn inspector_identity(mount: &str, full_path: &Path) -> (String, String) {
    if mount.is_empty() {
        if full_path.is_root() {
            return (INSPECTOR_SYNTHETIC_ROOT.to_string(), "/".to_string());
        }

        let mut segments = full_path.as_str().trim_start_matches('/').splitn(2, '/');
        let mount = segments.next().unwrap_or(INSPECTOR_SYNTHETIC_ROOT);
        let rel = segments
            .next()
            .map_or_else(|| "/".to_string(), |path| format!("/{path}"));
        return (mount.to_string(), rel);
    }

    let prefix = format!("/{mount}");
    let full = full_path.as_str();
    if full == prefix {
        return (mount.to_string(), "/".to_string());
    }
    if let Some(rel) = full.strip_prefix(&(prefix + "/")) {
        return (mount.to_string(), format!("/{rel}"));
    }

    (mount.to_string(), "/".to_string())
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
fn change_counter_parts(id: &Path, attrs: Option<&FileAttrsCache>, epoch: u64) -> u64 {
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

#[cfg(test)]
mod tests {
    use super::{EntryKind, INSPECTOR_SYNTHETIC_ROOT, TreeNamespace, inspector_identity};
    use crate::MountTable;
    use crate::namespace::Namespace;
    use omnifs_core::path::Path;
    use std::path::{Path as StdPath, PathBuf};
    use std::sync::Arc;

    fn fixture_registry(root: &StdPath) -> Arc<MountTable> {
        use crate::HostContext;
        use crate::cloner::GitCloner;
        use omnifs_workspace::mounts::{Registry, Spec};
        use omnifs_workspace::provider::{Artifact, ProviderStore};

        let cache = root.join("engine-cache");
        let config = root.join("engine-config");
        let mounts = root.join("engine-mounts");
        let providers = root.join("engine-providers");
        std::fs::create_dir_all(&mounts).expect("mount snapshot");
        std::fs::create_dir_all(&providers).expect("provider store");
        let wasm = StdPath::new(env!("CARGO_MANIFEST_DIR"))
            .parent()
            .expect("crate parent")
            .parent()
            .expect("workspace root")
            .join("target/wasm32-wasip2/release/test_provider.wasm");
        assert!(wasm.exists(), "build providers before the host fixture");
        let artifact = Artifact::from_bytes(
            "test_provider.wasm",
            std::fs::read(&wasm).expect("test provider"),
        )
        .expect("provider artifact");
        let reference = artifact.reference();
        ProviderStore::new(&providers)
            .retain(&artifact)
            .expect("retain provider");
        let mut desired = Registry::load(&mounts).expect("load mount snapshot");
        let spec: Spec = serde_json::from_value(serde_json::json!({
            "provider": reference,
            "mount": "test",
            "config": {}
        }))
        .expect("test mount spec");
        desired.put(&spec).expect("write test mount");
        let layout = omnifs_workspace::layout::WorkspaceLayout::under_root(&config);
        let context = HostContext::new(
            &cache,
            &layout.config_dir,
            &providers,
            &layout.credentials_file,
        )
        .with_wasm_cache_dir(crate::test_support::wasm_cache_dir());
        Arc::new(
            MountTable::load_online_with_options(
                context,
                Arc::new(GitCloner::new(root.join("engine-clones")).expect("git cloner")),
                &desired,
                &tokio::runtime::Handle::current(),
                true,
            )
            .expect("load test mount"),
        )
    }

    fn run_git(dir: &StdPath, args: &[&str]) {
        let status = std::process::Command::new("git")
            .args(args)
            .current_dir(dir)
            .env("GIT_AUTHOR_NAME", "test")
            .env("GIT_AUTHOR_EMAIL", "test@test.com")
            .env("GIT_COMMITTER_NAME", "test")
            .env("GIT_COMMITTER_EMAIL", "test@test.com")
            .status()
            .expect("run git");
        assert!(status.success(), "git {args:?} failed");
    }

    fn local_fixture_repo(path: &StdPath) {
        std::fs::create_dir_all(path.join("src")).expect("source repo");
        std::fs::write(path.join("README.md"), b"Hello from cache\n").expect("README");
        std::fs::write(path.join("src/main.rs"), b"fn main() {}\n").expect("main");
        run_git(path, &["init", "-b", "main"]);
        run_git(path, &["add", "."]);
        run_git(path, &["commit", "-m", "fixture"]);
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn git_clone_tree_ref_namespace_fixture_preserves_host_projection() {
        use crate::authority::RuntimeAuthority;
        use crate::cache::identity::GitId;
        use crate::git::GitExecutor;
        use crate::render::MATERIALIZE_MAX_BYTES;
        use crate::tree_refs::TreeRefs;
        use omnifs_wit::provider::types::{CalloutResult, GitOpenRequest};
        use std::io::{Seek, SeekFrom, Write};
        #[cfg(unix)]
        use std::os::unix::fs::{PermissionsExt, symlink};

        let temp = tempfile::tempdir().expect("fixture tempdir");
        let source = temp.path().join("source");
        local_fixture_repo(&source);

        let remote = "https://fixture.test/repo.git";
        let cloner =
            Arc::new(crate::GitCloner::new(temp.path().join("clones")).expect("git cloner"));
        let git_id = GitId::new("test", remote, Some("main"));
        let source_url = source.to_string_lossy().into_owned();
        let clone_path = cloner
            .clone_if_needed(&git_id, &source_url, remote, Some("main"), 1)
            .expect("warm local clone");

        #[cfg(unix)]
        {
            std::fs::write(clone_path.join("run.sh"), b"#!/bin/sh\n").expect("run script");
            std::fs::set_permissions(
                clone_path.join("run.sh"),
                std::fs::Permissions::from_mode(0o755),
            )
            .expect("run mode");
            symlink("README.md", clone_path.join("README.link")).expect("file symlink");
            symlink("../outside", clone_path.join("external-dir")).expect("dir symlink");
        }
        let outside = temp.path().join("outside");
        std::fs::create_dir(&outside).expect("outside directory");
        std::fs::write(outside.join("secret"), b"outside\n").expect("outside file");
        let mut large =
            std::fs::File::create(clone_path.join("large-host.bin")).expect("large host file");
        large
            .set_len(MATERIALIZE_MAX_BYTES + 5)
            .expect("sparse host file");
        large
            .seek(SeekFrom::Start(MATERIALIZE_MAX_BYTES))
            .expect("large tail offset");
        large.write_all(b"tail!").expect("large tail");

        let trees = Arc::new(TreeRefs::new());
        let reopened_root = cloner
            .open_cached("test", &git_id, "")
            .expect("reopen repository root without Git");
        let reopened_src = cloner
            .open_cached("test", &git_id, "src")
            .expect("reopen selected subtree without Git");
        let root_ref = trees
            .open(git_id.clone(), "", &reopened_root)
            .expect("open root tree");
        let src_ref = trees
            .open(git_id.clone(), "src", &reopened_src)
            .expect("open src tree");
        assert_ne!(root_ref, src_ref);
        assert_eq!(
            trees
                .open(git_id.clone(), "src", &reopened_src)
                .expect("deduplicate selected subtree"),
            src_ref
        );
        root_ref.root.metadata("README.md").expect("root selection");
        src_ref.root.metadata("main.rs").expect("src selection");

        let executor = GitExecutor::new(
            Arc::clone(&cloner),
            RuntimeAuthority::for_test(&[], &["*"], &[]),
            Arc::clone(&trees),
            "test",
        );
        let info = match executor.open_repo(
            &GitOpenRequest {
                clone_url: remote.to_string(),
                reference: Some("main".to_string()),
            },
            2,
        ) {
            CalloutResult::GitRepoOpened(info) => info,
            other => panic!("warm Git open failed: {other:?}"),
        };
        assert_eq!(info.repo, info.tree);
        let tree_ref = trees.resolve(info.tree).expect("registered tree ref");
        let namespace = TreeNamespace::online(
            fixture_registry(temp.path()),
            tokio::runtime::Handle::current(),
        );
        let checkout = Path::parse("/test/checkout").expect("checkout path");
        let checkout_metadata = namespace
            .host_stat(&tree_ref, StdPath::new(""))
            .await
            .expect("checkout stat");
        namespace.intern_host(
            checkout.clone(),
            PathBuf::new(),
            &tree_ref,
            &checkout_metadata,
        );

        let checkout_attrs = namespace
            .getattr(checkout.clone())
            .await
            .expect("checkout attrs");
        assert_eq!(checkout_attrs.kind, EntryKind::Directory);
        assert_eq!(checkout_attrs.mode, 0o555);
        let listing = namespace
            .readdir(checkout.clone(), super::DirCursor::start(), 0)
            .await
            .expect("checkout listing");
        let readme_entry = listing
            .entries
            .iter()
            .find(|entry| entry.name == "README.md")
            .expect("README listing");
        let readme = namespace
            .lookup(checkout.clone(), "README.md")
            .await
            .expect("README lookup");
        assert_eq!(
            readme.path, readme_entry.path,
            "list then lookup reuses Path identity"
        );
        assert_eq!(readme.attrs.kind, EntryKind::File);
        assert_eq!(readme.attrs.mode, 0o444);
        let readme_read = Namespace::read(namespace.as_ref(), readme.path, 0, 64)
            .await
            .expect("README read");
        assert_eq!(readme_read.bytes, b"Hello from cache\n");

        let src = namespace
            .lookup(checkout.clone(), "src")
            .await
            .expect("src lookup");
        let main = namespace
            .lookup(src.path, "main.rs")
            .await
            .expect("main lookup");
        assert_eq!(main.attrs.mode, 0o444);
        let main_read = Namespace::read(namespace.as_ref(), main.path, 0, 64)
            .await
            .expect("main read");
        assert_eq!(main_read.bytes, b"fn main() {}\n");

        let executable = namespace
            .lookup(checkout.clone(), "run.sh")
            .await
            .expect("run lookup");
        assert_eq!(executable.attrs.kind, EntryKind::File);
        assert_eq!(executable.attrs.mode, 0o555);

        #[cfg(unix)]
        {
            let link = namespace
                .lookup(checkout.clone(), "README.link")
                .await
                .expect("link lookup");
            assert_eq!(link.attrs.kind, EntryKind::Symlink);
            assert_eq!(link.attrs.mode, 0o777);
            assert_eq!(
                namespace.readlink(link.path).await.unwrap(),
                PathBuf::from("README.md")
            );
            let external = namespace
                .lookup(checkout.clone(), "external-dir")
                .await
                .expect("external link lookup");
            assert_eq!(external.attrs.kind, EntryKind::Symlink);
            assert!(matches!(
                namespace
                    .readdir(external.path, super::DirCursor::start(), 0)
                    .await,
                Err(super::NsError::NotDirectory)
            ));
        }

        let large = namespace
            .lookup(checkout, "large-host.bin")
            .await
            .expect("large lookup");
        assert_eq!(large.attrs.read_style, super::ReadStyle::Ranged);
        assert_eq!(large.attrs.size, MATERIALIZE_MAX_BYTES + 5);
        let tail = Namespace::read(namespace.as_ref(), large.path, MATERIALIZE_MAX_BYTES, 5)
            .await
            .expect("large tail read");
        assert_eq!(tail.bytes, b"tail!");
        assert!(tail.eof);
    }

    #[test]
    fn inspector_identity_normalizes_enumerated_mount_root() {
        let path = Path::parse("/github/notifications").expect("path");
        assert_eq!(
            inspector_identity("github", &path),
            ("github".to_string(), "/notifications".to_string())
        );
    }

    #[test]
    fn inspector_identity_labels_synthetic_enumeration_root() {
        let path = Path::root();
        assert_eq!(
            inspector_identity("", &path),
            (INSPECTOR_SYNTHETIC_ROOT.to_string(), "/".to_string())
        );
    }
}
