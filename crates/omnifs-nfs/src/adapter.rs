//! NFSv4.0 export adapter over the renderer-neutral [`Tree`] projection core.
//!
//! `Export` is the NFS renderer: it owns the NFS-side identity and reply
//! concerns (the inode table that backs `(generation, id)` filehandles, the
//! stateid open tables, the `/omnifs` export-root alias, the materialize cap,
//! and `fattr4` size construction) and drives
//! all path resolution / listing / reads through `Tree::resolve_child`,
//! `Tree::list`, and `Tree::read`. The cache consult+populate, the cold provider
//! round trips, the `@next`/`@all` controls, the mount-root ignore synthesis,
//! the write fence, and learned-size promotion all live in `Tree`; the renderer
//! keeps only a learned-attrs slot on its inode table (so a learned size
//! survives across ops, exactly like the FUSE inode), the flatten-renderer
//! eager size probing for ranged children, and NFS protocol state.
//!
//! There is no private object-metadata TTL table: an inode entry lives as long
//! as a path is referenced and is pruned only by explicit invalidation, mirroring
//! the FUSE adapter.

use crate::delayed::{DeferOutcome, Key, Listings};
use crate::export::{
    Attr, DirEntry, DirListing, NodeKind, OpenRead, OpenResult, OpenSeed, OpenTable,
    ReadOnlyExport, StateId, Status, StatusResult, ensure_read_access, open_data_slice,
};
use crate::protocol::consts::{
    EXPORT_ROOT_ID, MAX_NFS_READ_BYTES, NFS_EXPORT_NAME, OPEN_MATERIALIZE_MAX_BYTES, ROOT_ID,
};
use dashmap::DashMap;
use omnifs_core::MountName;
use omnifs_core::path::{Path, Segment};
use omnifs_core::view as view_types;
use omnifs_core::view::{EntryMeta, FileAttrsCache};
use omnifs_host::Runtime;
use omnifs_host::path_key::PathKey;
use omnifs_host::registry::ProviderRegistry;
use omnifs_tree::{
    Chunk, Entry as TreeEntry, ListOutcome, Listing, Node, RangedHandle, ReadResult, RequestCtx,
    Synthetic, Tree, TreeErrorKind,
};
use std::future::Future;
use std::hash::{DefaultHasher, Hash, Hasher};
use std::io::{Read, Seek, SeekFrom};
use std::path::{Path as FsPath, PathBuf};
use std::pin::Pin;
use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::{Duration, UNIX_EPOCH};
use tokio::runtime::{Handle, RuntimeFlavor};

/// Inline wait budget for proactive `READDIR` deferral ([`delayed::Listings`]).
/// Past this duration the handler replies `NFS4ERR_DELAY` while the listing
/// task keeps running in the background. Short enough that a cold listing never
/// holds the reply; long enough that a warm listing still answers in one round
/// trip. Distinct from reactive `DELAY` in [`Status::from`](crate::export::Status)
/// for [`TreeError`](omnifs_tree::TreeError), which maps transient upstream
/// errors on any op without background continuation. Only
/// `READDIR` uses proactive deferral; `LOOKUP` resolves inline (see
/// `lookup_via_tree`).
const NFS_INLINE_BUDGET: Duration = Duration::from_millis(75);

/// Per-inode renderer state. This is the NFS analogue of the FUSE `NodeEntry`:
/// it carries the stable identity a `(generation, id)` filehandle rehydrates
/// from, plus a learned-attrs slot so a size promoted by a `read`/`open` survives
/// across ops. It is NOT a cache of provider data; `Tree` owns all caching.
#[derive(Debug, Clone)]
struct NodeEntry {
    /// Which export root this inode hangs under (`ROOT_ID` or `EXPORT_ROOT_ID`).
    /// The same protocol path under the two roots gets two distinct inodes.
    scope: u64,
    mount_name: String,
    path: Path,
    parent: u64,
    kind: NodeKind,
    size: u64,
    size_exact: bool,
    attrs: Option<FileAttrsCache>,
    body: EntryBody,
}

#[derive(Debug, Clone)]
enum EntryBody {
    Provider,
    Backing(PathBuf),
    Synthetic(Synthetic),
}

impl EntryBody {
    fn backing_path(&self) -> Option<&PathBuf> {
        match self {
            Self::Backing(path) => Some(path),
            Self::Provider | Self::Synthetic(_) => None,
        }
    }

    fn synthetic(&self) -> Option<&Synthetic> {
        match self {
            Self::Synthetic(synthetic) => Some(synthetic),
            Self::Provider | Self::Backing(_) => None,
        }
    }

    fn is_provider(&self) -> bool {
        matches!(self, Self::Provider)
    }
}

struct EntrySeed<'a> {
    scope: u64,
    mount_name: &'a str,
    path: &'a Path,
    parent: u64,
    kind: NodeKind,
    size: u64,
    size_exact: bool,
    attrs: Option<FileAttrsCache>,
    body: EntryBody,
}

/// A live ranged open bound to a stateid. Holds the `Tree`-owned `RangedHandle`
/// (which owns its `Arc<Runtime>` + provider handle), so chunk reads and the
/// provider-handle release stay inside `Tree`. Not `Clone`: it owns the handle.
struct RangedOpen {
    ino: u64,
    mount_name: String,
    path: Path,
    handle: RangedHandle,
    /// Background pump that learns live (`tail -f`) growth into `follow_sizes`,
    /// aborted when this open is torn down. `None` for a non-live ranged file.
    follow_pump: Option<tokio::task::AbortHandle>,
}

#[derive(Debug, Default)]
struct FollowSizes {
    sizes: DashMap<u64, u64>,
}

impl FollowSizes {
    fn grow(&self, ino: u64, size: u64) {
        self.sizes
            .entry(ino)
            .and_modify(|current| *current = (*current).max(size))
            .or_insert(size);
    }

    fn get(&self, ino: u64) -> Option<u64> {
        self.sizes.get(&ino).map(|entry| *entry.value())
    }

    fn remove(&self, ino: u64) {
        self.sizes.remove(&ino);
    }
}

#[derive(Debug, Clone)]
struct BackingOpen {
    id: u64,
    mount_name: String,
    path: Path,
    backing_path: PathBuf,
}

enum OpenBody {
    Materialized(Vec<u8>),
    Ranged(RangedOpen),
    Backing(BackingOpen),
}

#[derive(Clone, Debug, Eq, Hash, PartialEq)]
struct ObjectKey {
    scope: u64,
    key: PathKey,
}

impl ObjectKey {
    fn new(scope: u64, mount: impl Into<String>, path: &Path) -> Self {
        let mount = mount.into();
        let mount = MountName::try_from(mount.as_str()).expect("runtime mount name");
        Self {
            scope,
            key: PathKey::new(mount, path.clone()),
        }
    }
}

pub struct Export {
    rt: Handle,
    /// The provider registry both the adapter and `Tree` hold. The adapter uses
    /// it only to recover the runtime for protocol-local state; mount
    /// enumeration and provider round trips go through `tree`.
    registry: Arc<ProviderRegistry>,
    /// The renderer-neutral projection core. Owns resolve/list/read decision
    /// logic; the NFS adapter enters the async runtime to call it and turns the
    /// neutral `Node`/`Listing`/`ReadResult` into NFS identity + `fattr4`.
    /// `Arc` so the deferral table can drive `list` from a background task
    /// without re-deriving the core.
    tree: Arc<Tree>,
    /// Proactive deferral for provider-backed `READDIR` ([`delayed::Listings`]).
    /// A cold list runs in the background; the handler waits
    /// [`NFS_INLINE_BUDGET`] then replies `NFS4ERR_DELAY` without cancelling the
    /// task. Only `READDIR`: `Tree` caches a listing's dirents, so the retry
    /// re-resolves into a warm cache. `LOOKUP` is not cached that way, so it
    /// stays inline (see `lookup_via_tree`). Reactive transient-error `DELAY` is
    /// separate (`Status::from`); it does not use this table.
    delayed_lists: Listings,
    inodes: DashMap<u64, NodeEntry>,
    path_to_inode: DashMap<ObjectKey, u64>,
    next_ino: AtomicU64,
    root_mount: Option<String>,
    opens: OpenTable<OpenBody>,
    /// Per-inode live-follow size learned by a ranged open's background pump.
    /// `attr` reports `max(entry.size, follow_sizes[ino])` so a polling `tail -f`
    /// over the `noac` mount re-stats, sees growth, and reads the new bytes.
    /// Shared into the spawned pump task, so `Arc`.
    follow_sizes: Arc<FollowSizes>,
}

impl Export {
    pub fn new(rt: Handle, registry: Arc<ProviderRegistry>) -> Self {
        let tree = Arc::new(Tree::new(Arc::clone(&registry)));
        assert!(
            !matches!(rt.runtime_flavor(), RuntimeFlavor::CurrentThread),
            "NFS adapter requires a multi-thread Tokio runtime because sync NFS workers call Handle::block_on"
        );
        let delayed_lists = Listings::new(rt.clone());
        let root_mount = registry.root_mount_name();
        let inodes = DashMap::new();
        let path_to_inode = DashMap::new();
        let mount = root_mount.clone().unwrap_or_default();
        for scope in [ROOT_ID, EXPORT_ROOT_ID] {
            inodes.insert(
                scope,
                NodeEntry {
                    scope,
                    mount_name: mount.clone(),
                    path: Path::root(),
                    parent: ROOT_ID,
                    kind: NodeKind::Directory,
                    size: 0,
                    size_exact: true,
                    attrs: None,
                    body: EntryBody::Provider,
                },
            );
            if root_mount.is_some() {
                path_to_inode.insert(ObjectKey::new(scope, &mount, &Path::root()), scope);
            }
        }
        Self {
            rt,
            registry,
            tree,
            delayed_lists,
            inodes,
            path_to_inode,
            next_ino: AtomicU64::new(EXPORT_ROOT_ID + 1),
            root_mount,
            opens: OpenTable::new(),
            follow_sizes: Arc::new(FollowSizes::default()),
        }
    }

    fn runtime_for_mount(&self, mount: &str) -> Option<Arc<Runtime>> {
        self.registry.get(mount)
    }

    fn remove_object(&self, id: u64) -> Option<NodeEntry> {
        let (_, entry) = self.inodes.remove(&id)?;
        self.path_to_inode
            .remove(&ObjectKey::new(entry.scope, &entry.mount_name, &entry.path));
        Some(entry)
    }

    /// Drain pending runtime invalidations through `Tree` and drive the NFS-side
    /// fan-out: prune the inode table and the open stateid tables (closing ranged
    /// provider handles).
    ///
    /// `Tree::drain_invalidations` owns the renderer-neutral half (queue drain +
    /// mem eviction); the NFS adapter consumes the returned report to prune its
    /// own identity tables, which are protocol concerns the projection core must
    /// not own. Mirrors the FUSE `drain_and_evict_pending`.
    fn drain_invalidations_for_mount(&self, mount_name: &str) {
        if mount_name.is_empty() {
            return;
        }
        let report = self.tree.drain_invalidations(mount_name);
        if report.is_empty() {
            return;
        }
        let matches = |path: &Path| {
            report.paths.iter().any(|invalidated| invalidated == path)
                || report.prefixes.iter().any(|prefix| path.has_prefix(prefix))
        };

        let stale_paths = self
            .path_to_inode
            .iter()
            .filter_map(|entry| {
                let key = entry.key();
                (key.key.mount.as_str() == mount_name
                    && !key.key.path.is_root()
                    && matches(&key.key.path))
                .then(|| key.clone())
            })
            .collect::<Vec<_>>();
        let mut stale_inodes = Vec::with_capacity(stale_paths.len());
        for key in &stale_paths {
            let Some(id) = self.path_to_inode.get(key).map(|entry| *entry.value()) else {
                continue;
            };
            if self.remove_object(id).is_some() {
                stale_inodes.push(id);
            }
        }
        for body in self.opens.remove_inodes(&stale_inodes) {
            self.close_open_body(body);
        }

        for body in self.opens.remove_where(|state| match &state.body {
            OpenBody::Materialized(_) => false,
            OpenBody::Ranged(ranged) => ranged.mount_name == mount_name && matches(&ranged.path),
            OpenBody::Backing(backing) => {
                backing.mount_name == mount_name && matches(&backing.path)
            },
        }) {
            self.close_open_body(body);
        }
    }

    fn alloc_ino(&self) -> u64 {
        self.next_ino.fetch_add(1, Ordering::Relaxed)
    }

    fn get_or_alloc(&self, seed: EntrySeed<'_>) -> u64 {
        let EntrySeed {
            scope,
            mount_name,
            path,
            parent,
            kind,
            size,
            size_exact,
            attrs,
            body,
        } = seed;
        let key = ObjectKey::new(scope, mount_name, path);
        let attrs_for_update = attrs.clone();
        let body_for_update = body.clone();
        *self
            .path_to_inode
            .entry(key)
            .and_modify(|existing| {
                if let Some(mut entry) = self.inodes.get_mut(existing) {
                    entry.parent = parent;
                    entry.kind = kind;
                    if size_exact || !entry.size_exact {
                        entry.size = size;
                        entry.size_exact = size_exact;
                    }
                    if let Some(incoming_attrs) = attrs_for_update.clone()
                        && let Some(merged_attrs) = FileAttrsCache::merge_preserving_learned_size(
                            entry.attrs.as_ref(),
                            Some(incoming_attrs),
                        )
                    {
                        entry.size = merged_attrs.st_size();
                        entry.size_exact =
                            matches!(merged_attrs.size(), view_types::FileSize::Exact(_));
                        entry.attrs = Some(merged_attrs);
                    }
                    if !(matches!(entry.body, EntryBody::Backing(_))
                        && matches!(body_for_update, EntryBody::Provider))
                    {
                        entry.body.clone_from(&body_for_update);
                    }
                    if matches!(entry.body, EntryBody::Backing(_)) {
                        entry.size_exact = true;
                        entry.attrs = None;
                    }
                }
            })
            .or_insert_with(|| {
                let id = self.alloc_ino();
                self.inodes.insert(
                    id,
                    NodeEntry {
                        scope,
                        mount_name: mount_name.to_string(),
                        path: path.clone(),
                        parent,
                        kind,
                        size,
                        size_exact,
                        attrs,
                        body,
                    },
                );
                id
            })
    }

    fn promote_file_attrs(&self, id: u64, attrs: FileAttrsCache) {
        if matches!(attrs.stability(), view_types::Stability::Live) {
            return;
        }
        if let Some(mut entry) = self.inodes.get_mut(&id)
            && entry.kind == NodeKind::File
            && !matches!(entry.body, EntryBody::Backing(_))
        {
            entry.size = attrs.st_size();
            entry.size_exact = matches!(attrs.size(), view_types::FileSize::Exact(_));
            entry.attrs = Some(attrs);
        }
    }

    fn attr_from_entry(id: u64, entry: &NodeEntry) -> Attr {
        Attr {
            id,
            parent: entry.parent,
            kind: entry.kind,
            size: entry.size,
            mode: entry.kind.mode(),
            change: Self::entry_change(id, entry),
            mtime_sec: 0,
        }
    }

    fn root_attr_from_entry(&self, id: u64, entry: &NodeEntry) -> Attr {
        let mut attr = Self::attr_from_entry(id, entry);
        attr.change = self.mount_enumeration_change(id, entry);
        attr
    }

    fn attr_from_metadata(
        id: u64,
        parent: u64,
        metadata: &std::fs::Metadata,
    ) -> StatusResult<Attr> {
        let kind = Self::backing_kind(metadata)?;
        let mtime_sec = metadata
            .modified()
            .ok()
            .and_then(|time| time.duration_since(UNIX_EPOCH).ok())
            .map_or(0, |duration| {
                i64::try_from(duration.as_secs()).unwrap_or(i64::MAX)
            });
        Ok(Attr {
            id,
            parent,
            kind,
            size: metadata.len(),
            mode: kind.mode(),
            change: Self::metadata_change(id, metadata),
            mtime_sec,
        })
    }

    fn backing_kind(metadata: &std::fs::Metadata) -> StatusResult<NodeKind> {
        if metadata.is_dir() {
            Ok(NodeKind::Directory)
        } else if metadata.file_type().is_symlink() {
            Ok(NodeKind::Symlink)
        } else if metadata.is_file() {
            Ok(NodeKind::File)
        } else {
            Err(Status::Invalid)
        }
    }

    fn entry_change(id: u64, entry: &NodeEntry) -> u64 {
        let mut hasher = DefaultHasher::new();
        id.hash(&mut hasher);
        entry.mount_name.hash(&mut hasher);
        entry.path.hash(&mut hasher);
        (entry.kind as u8).hash(&mut hasher);
        entry.size.hash(&mut hasher);
        entry.size_exact.hash(&mut hasher);
        if let Some(attrs) = &entry.attrs {
            attrs.version_token().hash(&mut hasher);
            let size = attrs.size();
            let bytes = attrs.byte_source();
            let stability = attrs.stability();
            std::mem::discriminant(&size).hash(&mut hasher);
            std::mem::discriminant(&bytes).hash(&mut hasher);
            std::mem::discriminant(&stability).hash(&mut hasher);
        }
        hasher.finish()
    }

    fn mount_enumeration_change(&self, id: u64, entry: &NodeEntry) -> u64 {
        let mut hasher = DefaultHasher::new();
        Self::entry_change(id, entry).hash(&mut hasher);
        let mut mounts = self.registry.mounts();
        mounts.sort();
        mounts.hash(&mut hasher);
        hasher.finish()
    }

    fn is_mount_enumeration_root(&self, id: u64) -> bool {
        (id == ROOT_ID || id == EXPORT_ROOT_ID) && self.root_mount.is_none()
    }

    fn metadata_change(id: u64, metadata: &std::fs::Metadata) -> u64 {
        let mut hasher = DefaultHasher::new();
        id.hash(&mut hasher);
        metadata.len().hash(&mut hasher);
        if let Ok(modified) = metadata.modified()
            && let Ok(duration) = modified.duration_since(UNIX_EPOCH)
        {
            duration.as_secs().hash(&mut hasher);
            duration.subsec_nanos().hash(&mut hasher);
        }
        hasher.finish()
    }

    fn meta_kind(meta: &EntryMeta) -> NodeKind {
        match meta.kind() {
            view_types::EntryKind::Directory => NodeKind::Directory,
            view_types::EntryKind::File => NodeKind::File,
        }
    }

    fn meta_size(meta: &EntryMeta) -> (u64, bool) {
        let exact = match meta.attrs() {
            Some(attrs) => matches!(attrs.size(), view_types::FileSize::Exact(_)),
            None => true,
        };
        (meta.st_size(), exact)
    }

    /// Promote an unsized ranged-placeholder file meta to its real attrs via a
    /// `Tree` open-probe, caching the learned attrs through the shared projection
    /// layer.
    fn promote_ranged_placeholder_meta(
        &self,
        mount_name: &str,
        child_path: &Path,
        mut meta: EntryMeta,
    ) -> EntryMeta {
        if meta.attrs().is_some_and(Self::needs_ranged_size_probe) {
            match self
                .rt
                .block_on(self.tree.probe_ranged_attrs(mount_name, child_path))
            {
                Ok(Some(attrs)) => meta = EntryMeta::file(attrs),
                Ok(None) => {},
                Err(error) => {
                    tracing::warn!(
                        path = %child_path,
                        error = %error,
                        "NFS ranged attr probe failed"
                    );
                },
            }
        }
        meta
    }

    /// A declared-ranged file whose size is not yet known. A ranged route
    /// projects `Deferred(Ranged)` with `Unknown` size; NFS flattens a directory
    /// into a finite snapshot whose `fattr4` carries each child's size, so the
    /// real size must be learned before `ls -l`/`stat` reads it (a full file
    /// learns its size lazily on `read`, so it is not probed here).
    fn needs_ranged_size_probe(attrs: &FileAttrsCache) -> bool {
        matches!(attrs.size(), view_types::FileSize::Unknown) && attrs.is_deferred_ranged()
    }

    /// The truth computation for a directory listing, as a `'static` future
    /// factory the deferral table spawns. Errors are logged and mapped to
    /// `Status` here so the table stays protocol-agnostic; the log preserves the
    /// per-listing breadcrumb the synchronous handler used to emit.
    fn list_op(
        &self,
        mount: &str,
        path: &Path,
    ) -> impl FnOnce() -> Pin<Box<dyn Future<Output = Result<ListOutcome, Status>> + Send>> + use<>
    {
        let tree = Arc::clone(&self.tree);
        let node = Node::provider_dir(mount.to_string(), path.clone());
        let mount = mount.to_string();
        let path = path.clone();
        move || {
            Box::pin(async move {
                let ctx = RequestCtx::default();
                tree.list(&node, None, &ctx).await.map_err(|error| {
                    tracing::warn!(
                        op = "readdir",
                        mount = %mount,
                        path = %path,
                        error = %error,
                        "NFS Tree readdir failed"
                    );
                    Status::from(&error)
                })
            })
        }
    }

    /// Resolve `name` under the provider directory `parent_path` through
    /// `Tree::resolve_child` and bind the resulting `Node` to an inode. `Tree`
    /// owns the provider lookup, the `@next`/`@all` control resolution, the
    /// mount-root ignore synthesis, and the subtree handoff; the adapter only
    /// mints inode identity, eagerly probing ranged children for their size.
    fn lookup_via_tree(
        &self,
        scope: u64,
        mount_name: &str,
        parent_path: &Path,
        parent: u64,
        name: &Segment,
        runtime: &Arc<Runtime>,
    ) -> StatusResult<u64> {
        // Inline (not deferred): a cold child lookup is not cached by `Tree` the
        // way a listing is, so deferring it would re-run provider work on every
        // retry. Concurrent dispatch keeps the rest of the mount responsive while
        // this one resolves.
        let parent_node = Node::provider_dir(mount_name.to_string(), parent_path.clone());
        let ctx = RequestCtx::default();
        match self
            .rt
            .block_on(self.tree.resolve_child(&parent_node, name.as_str(), &ctx))
        {
            Ok(node) => Ok(self.bind_node(scope, mount_name, parent, &node, Some(runtime))),
            Err(error) if error.kind == TreeErrorKind::NotFound => Err(Status::NoEnt),
            Err(error) => {
                tracing::warn!(
                    op = "lookup",
                    mount = %mount_name,
                    parent = %parent_path,
                    name = %name,
                    error = %error,
                    "NFS Tree lookup failed"
                );
                Err(Status::from(&error))
            },
        }
    }

    /// Bind a resolved `Tree` `Node` to an inode. A subtree node records its
    /// backing dir; a synthetic node carries its synthetic descriptor; a provider
    /// node is promoted if it is a static ranged placeholder.
    fn bind_node(
        &self,
        scope: u64,
        mount_name: &str,
        parent: u64,
        node: &Node,
        runtime: Option<&Arc<Runtime>>,
    ) -> u64 {
        let child_path = node.path().clone();
        if let Some(dir) = node.subtree_path() {
            return self.get_or_alloc(EntrySeed {
                scope,
                mount_name,
                path: &child_path,
                parent,
                kind: NodeKind::Directory,
                size: 0,
                size_exact: true,
                attrs: None,
                body: EntryBody::Backing(dir.clone()),
            });
        }

        let mut meta = node.projected_meta();
        if node.synthetic_kind().is_none() && runtime.is_some() {
            meta = self.promote_ranged_placeholder_meta(mount_name, &child_path, meta);
        }
        let kind = Self::meta_kind(&meta);
        let (size, size_exact) = Self::meta_size(&meta);
        self.get_or_alloc(EntrySeed {
            scope,
            mount_name,
            path: &child_path,
            parent,
            kind,
            size,
            size_exact,
            attrs: meta.into_attrs(),
            body: node
                .synthetic_kind()
                .cloned()
                .map_or(EntryBody::Provider, EntryBody::Synthetic),
        })
    }

    fn lookup_backing_child(
        &self,
        scope: u64,
        mount_name: &str,
        parent_path: &Path,
        parent: u64,
        name: &Segment,
        root: &FsPath,
    ) -> StatusResult<u64> {
        let child = root.join(name.as_str());
        let metadata = std::fs::symlink_metadata(&child).map_err(|_| Status::NoEnt)?;
        let kind = Self::backing_kind(&metadata)?;
        let child_path = parent_path.join_segment(name);
        Ok(self.get_or_alloc(EntrySeed {
            scope,
            mount_name,
            path: &child_path,
            parent,
            kind,
            size: metadata.len(),
            size_exact: true,
            attrs: None,
            body: EntryBody::Backing(child),
        }))
    }

    /// Build a finite directory snapshot from a `Tree` `Listing`.
    fn snapshot_from_listing(
        &self,
        scope: u64,
        mount_name: &str,
        path: &Path,
        parent: u64,
        listing: &Listing,
        runtime: &Arc<Runtime>,
    ) -> DirListing {
        let mut entries = Vec::with_capacity(listing.entries.len());
        for entry in &listing.entries {
            let runtime = if entry.is_synthetic() {
                None
            } else {
                Some(runtime)
            };
            if let Some(dir_entry) =
                self.dir_entry_from_tree(scope, mount_name, path, parent, entry, runtime)
            {
                entries.push(dir_entry);
            }
        }
        // Sorting happens centrally in `handle_readdir`; a local pre-sort here
        // would diverge silently if that policy changes.
        DirListing {
            entries,
            // NFS presents the finite known snapshot; the dynamic-directory
            // exhaustiveness is irrelevant to the NFS wire (there is no way to
            // advertise lookup-only children), so the snapshot reports EOF.
            exhaustive: true,
        }
    }

    fn dir_entry_from_tree(
        &self,
        scope: u64,
        mount_name: &str,
        path: &Path,
        parent: u64,
        entry: &TreeEntry,
        runtime: Option<&Arc<Runtime>>,
    ) -> Option<DirEntry> {
        let name = Segment::try_from(entry.name.as_str()).ok()?;
        let child_path = path.join_segment(&name);
        let mut meta = entry.meta.clone();
        let synthetic = entry.synthetic_kind().cloned();
        if synthetic.is_none() && runtime.is_some() {
            meta = self.promote_ranged_placeholder_meta(mount_name, &child_path, meta);
        }
        let kind = Self::meta_kind(&meta);
        let (size, size_exact) = Self::meta_size(&meta);
        let id = self.get_or_alloc(EntrySeed {
            scope,
            mount_name,
            path: &child_path,
            parent,
            kind,
            size,
            size_exact,
            attrs: meta.into_attrs(),
            body: synthetic.map_or(EntryBody::Provider, EntryBody::Synthetic),
        });
        let attr = self.attr(id).unwrap_or(Attr {
            id,
            parent,
            kind,
            size,
            mode: kind.mode(),
            change: id,
            mtime_sec: 0,
        });
        Some(DirEntry {
            id,
            name: entry.name.clone(),
            attr,
        })
    }

    fn readdir_backing(
        &self,
        scope: u64,
        mount_name: &str,
        path: &Path,
        parent: u64,
        root: &FsPath,
    ) -> StatusResult<DirListing> {
        let mut entries = Vec::new();
        for entry in std::fs::read_dir(root).map_err(|_| Status::Io)? {
            let entry = entry.map_err(|_| Status::Io)?;
            let name = entry.file_name();
            let Some(name) = name.to_str() else {
                continue;
            };
            let Ok(name) = Segment::try_from(name) else {
                continue;
            };
            let child_path = path.join_segment(&name);
            let backing_path = entry.path();
            let metadata = std::fs::symlink_metadata(&backing_path).map_err(|_| Status::Io)?;
            let Ok(kind) = Self::backing_kind(&metadata) else {
                continue;
            };
            let id = self.get_or_alloc(EntrySeed {
                scope,
                mount_name,
                path: &child_path,
                parent,
                kind,
                size: metadata.len(),
                size_exact: true,
                attrs: None,
                body: EntryBody::Backing(backing_path),
            });
            entries.push(DirEntry {
                id,
                name: name.as_str().to_string(),
                attr: Self::attr_from_metadata(id, parent, &metadata)?,
            });
        }
        Ok(DirListing {
            entries,
            exhaustive: true,
        })
    }

    /// Read a provider-backed file. Host-synthesized nodes, treeref backing
    /// nodes, inline projected bytes, cache hits, cold provider reads, write
    /// fencing, and learned-size publication are all served by `Tree::read`.
    /// The adapter only promotes the returned attrs onto its inode.
    fn read_provider_file(
        &self,
        id: u64,
        mount_name: &str,
        path: &Path,
        attrs: Option<&FileAttrsCache>,
        synthetic: Option<Synthetic>,
    ) -> StatusResult<Vec<u8>> {
        let node = match synthetic {
            Some(synthetic) => Node::synthetic_file(
                mount_name.to_string(),
                path.clone(),
                attrs.cloned(),
                synthetic,
            ),
            None => Node::provider_file(mount_name.to_string(), path.clone(), attrs.cloned()),
        };
        let ctx = RequestCtx::default();
        match self.rt.block_on(self.tree.read(&node, &ctx)) {
            Ok(ReadResult::Bytes {
                data,
                attrs: read_attrs,
                ..
            }) => {
                if let Some(read_attrs) = read_attrs {
                    self.promote_file_attrs(id, read_attrs);
                }
                Ok(data)
            },
            Ok(ReadResult::Backing(backing_path)) => {
                std::fs::read(backing_path).map_err(|_| Status::Io)
            },
            Err(error) => {
                tracing::warn!(
                    op = "read",
                    mount = %mount_name,
                    path = %path,
                    error = %error,
                    "NFS Tree read failed"
                );
                Err(Status::from(&error))
            },
        }
    }

    /// Serve a ranged read from the `Tree`-owned `RangedHandle` bound to this
    /// stateid. `Tree` drives `read_chunk`, validates the chunk against the
    /// requested length and the projected attrs, and learns the exact size on an
    /// EOF-short read; the adapter promotes the learned size onto its inode (and
    /// the cache) so a later `attr` reflects it. NFS clamps the request to the
    /// max read size; `Tree` enforces the no-oversize-chunk invariant.
    fn read_ranged_state(
        &self,
        id: u64,
        ranged: &RangedOpen,
        offset: u64,
        count: u32,
    ) -> StatusResult<OpenRead> {
        let count = count.min(MAX_NFS_READ_BYTES);
        let Chunk {
            bytes,
            eof,
            learned_attrs,
        } = match self.rt.block_on(ranged.handle.read(offset, count)) {
            Ok(chunk) => chunk,
            Err(error) => {
                tracing::warn!(
                    op = "read",
                    mount = %ranged.mount_name,
                    path = %ranged.path,
                    error = %error,
                    "NFS Tree ranged read failed"
                );
                return Err(Status::from(&error));
            },
        };
        if let Some(attrs) = learned_attrs {
            // A live file's learned size grows monotonically and is owned by the
            // follow path, not the inode (`promote_file_attrs` skips Live); fold
            // an EOF-short read's growth into `follow_sizes` so the next stat
            // reflects it without waiting for the pump's next tick.
            if matches!(attrs.stability(), view_types::Stability::Live) {
                self.follow_sizes.grow(id, attrs.st_size());
            } else {
                self.promote_file_attrs(id, attrs.clone());
            }
        }
        Ok(OpenRead {
            id,
            data: bytes,
            eof,
        })
    }

    fn read_backing_state(
        backing: &BackingOpen,
        offset: u64,
        count: u32,
    ) -> StatusResult<OpenRead> {
        let metadata =
            std::fs::symlink_metadata(&backing.backing_path).map_err(|_| Status::Stale)?;
        if metadata.file_type().is_symlink() {
            return Err(Status::Symlink);
        }
        if metadata.is_dir() {
            return Err(Status::IsDir);
        }
        if !metadata.is_file() {
            return Err(Status::Invalid);
        }

        let count = usize::try_from(count.min(MAX_NFS_READ_BYTES)).map_err(|_| Status::Io)?;
        let mut file = std::fs::File::open(&backing.backing_path).map_err(|_| Status::Io)?;
        file.seek(SeekFrom::Start(offset)).map_err(|_| Status::Io)?;
        let mut data = vec![0; count];
        let read = file.read(&mut data).map_err(|_| Status::Io)?;
        data.truncate(read);
        let read_end = offset
            .checked_add(u64::try_from(read).map_err(|_| Status::Io)?)
            .ok_or(Status::Io)?;
        Ok(OpenRead {
            id: backing.id,
            data,
            eof: read_end >= metadata.len(),
        })
    }

    /// Refuse the open up-front when we can already prove it would blow the
    /// materialization budget, or when the declared bytes contract leaves
    /// the post-read size unbounded. Ranged opens stream chunks through a
    /// provider handle and bypass this check; backing files and full-mode
    /// projected files share the same byte-cap policy.
    fn enforce_materialize_cap(
        mount_name: &str,
        path: &Path,
        attrs: Option<&FileAttrsCache>,
        backing_path: Option<&FsPath>,
    ) -> StatusResult<()> {
        if backing_path.is_some() {
            return Ok(());
        }
        if let Some(projected_attrs) = attrs
            && projected_attrs.is_deferred_full()
        {
            match projected_attrs.size() {
                view_types::FileSize::Exact(declared) if declared > OPEN_MATERIALIZE_MAX_BYTES => {
                    tracing::warn!(
                        op = "open",
                        mount = %mount_name,
                        path = %path,
                        size = declared,
                        cap = OPEN_MATERIALIZE_MAX_BYTES,
                        "rejecting full-mode open: declared exact size exceeds materialize cap"
                    );
                    return Err(Status::Resource);
                },
                view_types::FileSize::Exact(_)
                | view_types::FileSize::NonZero
                | view_types::FileSize::Unknown => {},
            }
        }
        Ok(())
    }

    /// Release the provider handle behind a removed ranged open through
    /// `RangedHandle::close`. Consumes the owned `RangedOpen` taken out of the
    /// stateid table: aborts the live-follow pump, drops the inode's follow size
    /// once no other open holds the inode live, then closes the provider handle.
    fn close_ranged_open(&self, ranged: RangedOpen) {
        if let Some(pump) = &ranged.follow_pump {
            pump.abort();
        }
        if !self
            .opens
            .any(|state| matches!(&state.body, OpenBody::Ranged(open) if open.ino == ranged.ino))
        {
            self.follow_sizes.remove(ranged.ino);
        }
        if let Err(error) = ranged.handle.close() {
            tracing::warn!(
                mount = %ranged.mount_name,
                path = %ranged.path,
                error = %error,
                "NFS runtime ranged close failed"
            );
        }
    }

    fn close_open_body(&self, body: OpenBody) {
        if let OpenBody::Ranged(ranged) = body {
            self.close_ranged_open(ranged);
        }
    }

    fn close_expired_open(&self, stateid: StateId) {
        if let Some(body) = self.opens.remove_body(stateid) {
            self.close_open_body(body);
        }
    }

    /// Open a `Deferred(Ranged)` file through `Tree::open` and register the
    /// resulting handle under a fresh stateid. `Tree` owns the provider open and
    /// chunk reads; the adapter keeps the renderer-side stateid binding. Returns
    /// `Ok(None)` when the route declared `ranged` but the provider answered full
    /// (`Tree::open` reports the mismatch), so the caller serves it as a full read.
    fn open_ranged_state(
        &self,
        seed: OpenSeed<()>,
        mount_name: &str,
        path: &Path,
        projected_attrs: &FileAttrsCache,
    ) -> StatusResult<Option<OpenResult>> {
        let ino = seed.inode;
        let node = Node::provider_file(
            mount_name.to_string(),
            path.clone(),
            Some(projected_attrs.clone()),
        );
        let ctx = RequestCtx::default();
        let handle = match self.rt.block_on(self.tree.open(&node, &ctx)) {
            Ok(Some(handle)) => handle,
            Ok(None) => return Ok(None),
            Err(error) => {
                tracing::warn!(
                    op = "open",
                    mount = %mount_name,
                    path = %path,
                    error = %error,
                    "NFS Tree ranged open failed"
                );
                return Err(Status::from(&error));
            },
        };
        let opened_attrs = handle.attrs().clone();
        if let Err(error) = opened_attrs.validate() {
            tracing::warn!(path = %path, error, "open-file returned invalid attrs");
            let _ = handle.close();
            return Err(Status::Io);
        }
        self.promote_file_attrs(ino, opened_attrs.clone());
        let attr = match self.attr(ino) {
            Ok(attr) => attr,
            Err(status) => {
                let _ = handle.close();
                return Err(status);
            },
        };
        // A live (`tail -f`) file grows while observed. Seed the inode's follow
        // size from the provider's open-time size, then spawn a pump that learns
        // upstream growth on a cadence; `attr` reports it so an idle reader over
        // the `noac` mount re-stats and reads forward. The size-learning is
        // `Tree`'s (via `probe_live_growth`); the reporting is the adapter's.
        let follow_pump = if matches!(opened_attrs.stability(), view_types::Stability::Live) {
            let initial = opened_attrs
                .st_size()
                .max(handle.observed_end().load(Ordering::Relaxed));
            self.follow_sizes.grow(ino, initial);
            Some(self.spawn_follow_pump(
                ino,
                mount_name.to_string(),
                handle.provider_handle(),
                handle.observed_end(),
            ))
        } else {
            None
        };
        let stateid = self.opens.open(seed.with_body(OpenBody::Ranged(RangedOpen {
            ino,
            mount_name: mount_name.to_string(),
            path: path.clone(),
            handle,
            follow_pump,
        })));
        Ok(Some(OpenResult { stateid, attr }))
    }

    /// Spawn a background pump for a live file: on a cadence it asks `Tree` to
    /// probe upstream growth (a sizing read at the current observed end),
    /// recording any new end in `follow_sizes`. `attr` reports that size, so a
    /// polling `tail -f` re-stats (the mount is `noac`), sees growth, and reads
    /// the new bytes through the ranged path. Aborted on teardown.
    fn spawn_follow_pump(
        &self,
        ino: u64,
        mount_name: String,
        provider_handle: u64,
        observed_end: Arc<AtomicU64>,
    ) -> tokio::task::AbortHandle {
        let follow_sizes = Arc::clone(&self.follow_sizes);
        omnifs_tree::spawn_live_follow_pump(
            &self.rt,
            Arc::clone(&self.registry),
            mount_name,
            provider_handle,
            observed_end,
            move |new_end| follow_sizes.grow(ino, new_end),
        )
    }
}

impl ReadOnlyExport for Export {
    fn root(&self) -> u64 {
        ROOT_ID
    }

    fn attr(&self, id: u64) -> StatusResult<Attr> {
        let entry = self.inodes.get(&id).ok_or(Status::Stale)?;
        let mount_name = entry.mount_name.clone();
        let backing_path = entry.body.backing_path().cloned();
        drop(entry);

        self.drain_invalidations_for_mount(&mount_name);

        let entry = self.inodes.get(&id).ok_or(Status::Stale)?;
        if let Some(path) = &backing_path {
            let metadata = std::fs::symlink_metadata(path).map_err(|_| Status::Stale)?;
            Self::attr_from_metadata(id, entry.parent, &metadata)
        } else {
            let mut attr = if self.is_mount_enumeration_root(id) {
                self.root_attr_from_entry(id, &entry)
            } else {
                Self::attr_from_entry(id, &entry)
            };
            // A live file's size is owned by its follow pump, never the inode
            // (`promote_file_attrs` skips Live); report the learned growth so a
            // polling `tail -f` re-stats and reads forward. Never shrinks.
            if let Some(grown) = self.follow_sizes.get(id) {
                attr.size = attr.size.max(grown);
            }
            Ok(attr)
        }
    }

    fn lookup(&self, parent: u64, name: &str) -> StatusResult<u64> {
        let name = Segment::try_from(name).map_err(|_| Status::Invalid)?;

        if (parent == ROOT_ID || parent == EXPORT_ROOT_ID) && self.root_mount.is_none() {
            let child_path = Path::root().join_segment(&name);
            let ctx = RequestCtx::default();
            if let Ok(node) = self.rt.block_on(self.tree.resolve(&child_path, &ctx))
                && !node.is_synthetic()
            {
                return Ok(self.bind_node(parent, node.mount(), parent, &node, None));
            }

            if parent == ROOT_ID && name.as_str() == NFS_EXPORT_NAME {
                return Ok(EXPORT_ROOT_ID);
            }

            return Err(Status::NoEnt);
        }

        let parent_entry = self.inodes.get(&parent).ok_or(Status::Stale)?;
        if parent_entry.kind != NodeKind::Directory {
            return Err(Status::NotDir);
        }
        let mount_name = parent_entry.mount_name.clone();
        let parent_path = parent_entry.path.clone();
        let scope = parent_entry.scope;
        let backing_path = parent_entry.body.backing_path().cloned();
        drop(parent_entry);

        self.drain_invalidations_for_mount(&mount_name);
        // Invalidations may have just removed `parent` from the inode table.
        // Re-confirm before binding a child to it, otherwise the child would
        // inherit an orphan parent inode that fails Status::Stale on every later
        // attr/lookupp.
        if !self.inodes.contains_key(&parent) {
            return Err(Status::Stale);
        }

        if let Some(root) = backing_path {
            return self.lookup_backing_child(
                scope,
                &mount_name,
                &parent_path,
                parent,
                &name,
                &root,
            );
        }

        let runtime = self.runtime_for_mount(&mount_name).ok_or(Status::NoEnt)?;

        match self.lookup_via_tree(scope, &mount_name, &parent_path, parent, &name, &runtime) {
            Err(Status::NoEnt) if parent == ROOT_ID && name.as_str() == NFS_EXPORT_NAME => {
                Ok(EXPORT_ROOT_ID)
            },
            result => result,
        }
    }

    fn readdir(&self, id: u64) -> StatusResult<DirListing> {
        if (id == ROOT_ID || id == EXPORT_ROOT_ID) && self.root_mount.is_none() {
            let ctx = RequestCtx::default();
            let root = self
                .rt
                .block_on(self.tree.resolve(&Path::root(), &ctx))
                .map_err(|error| Status::from(&error))?;
            let listing = match self
                .rt
                .block_on(self.tree.list(&root, None, &ctx))
                .map_err(|error| Status::from(&error))?
            {
                ListOutcome::Listing(listing) => listing,
                ListOutcome::Subtree(_) => return Err(Status::Io),
            };
            let entries = listing
                .entries
                .into_iter()
                .filter(|entry| !entry.is_synthetic())
                .map(|entry| {
                    let child = self.get_or_alloc(EntrySeed {
                        scope: id,
                        mount_name: &entry.name,
                        path: &Path::root(),
                        parent: id,
                        kind: NodeKind::Directory,
                        size: entry.meta.st_size(),
                        size_exact: true,
                        attrs: entry.meta.into_attrs(),
                        body: EntryBody::Provider,
                    });
                    DirEntry {
                        id: child,
                        name: entry.name,
                        attr: self.attr(child).expect("fresh mount attr"),
                    }
                })
                .collect();
            return Ok(DirListing {
                entries,
                exhaustive: true,
            });
        }

        let entry = self.inodes.get(&id).ok_or(Status::Stale)?;
        if entry.kind != NodeKind::Directory {
            return Err(Status::NotDir);
        }
        let mount_name = entry.mount_name.clone();
        let path = entry.path.clone();
        let scope = entry.scope;
        let backing_path = entry.body.backing_path().cloned();
        drop(entry);

        self.drain_invalidations_for_mount(&mount_name);
        if !self.inodes.contains_key(&id) {
            return Err(Status::Stale);
        }

        if let Some(root) = backing_path {
            return self.readdir_backing(scope, &mount_name, &path, id, &root);
        }

        let runtime = self.runtime_for_mount(&mount_name).ok_or(Status::NoEnt)?;
        // Proactive deferral only. On persistent listing failure `Tree` does not
        // cache the error, so each retry may re-defer until the listing succeeds
        // or maps to a terminal `Status` via `list_op`.
        let list_key = Key::new(&mount_name, &path);
        match self.delayed_lists.resolve(
            &list_key,
            NFS_INLINE_BUDGET,
            self.list_op(&mount_name, &path),
        ) {
            DeferOutcome::Ready(result) => match result.as_ref() {
                Ok(ListOutcome::Listing(listing)) => Ok(self.snapshot_from_listing(
                    scope,
                    &mount_name,
                    &path,
                    id,
                    listing,
                    &runtime,
                )),
                Ok(ListOutcome::Subtree(dir)) => {
                    if let Some(mut entry) = self.inodes.get_mut(&id)
                        && !matches!(entry.body, EntryBody::Backing(_))
                    {
                        entry.body = EntryBody::Backing(dir.clone());
                    }
                    self.readdir_backing(scope, &mount_name, &path, id, dir.as_path())
                },
                Err(status) => Err(*status),
            },
            DeferOutcome::Pending => Err(Status::Delay),
        }
    }

    fn read(&self, id: u64) -> StatusResult<Vec<u8>> {
        let entry = self.inodes.get(&id).ok_or(Status::Stale)?;
        if entry.kind == NodeKind::Directory {
            return Err(Status::IsDir);
        }
        if entry.kind == NodeKind::Symlink {
            return Err(Status::Invalid);
        }
        let mount_name = entry.mount_name.clone();
        let path = entry.path.clone();
        let attrs = entry.attrs.clone();
        let body = entry.body.clone();
        drop(entry);

        self.drain_invalidations_for_mount(&mount_name);
        if !self.inodes.contains_key(&id) {
            return Err(Status::Stale);
        }

        if let Some(backing_path) = body.backing_path().cloned() {
            let metadata = std::fs::symlink_metadata(&backing_path).map_err(|_| Status::Stale)?;
            if metadata.file_type().is_symlink() {
                return Err(Status::Symlink);
            }
            if metadata.is_dir() {
                return Err(Status::IsDir);
            }
            if !metadata.is_file() {
                return Err(Status::Invalid);
            }
            return std::fs::read(backing_path).map_err(|_| Status::Io);
        }

        if self.runtime_for_mount(&mount_name).is_none() {
            return Err(Status::NoEnt);
        }
        self.read_provider_file(
            id,
            &mount_name,
            &path,
            attrs.as_ref(),
            body.synthetic().cloned(),
        )
    }

    fn readlink(&self, id: u64) -> StatusResult<Vec<u8>> {
        let entry = self.inodes.get(&id).ok_or(Status::Stale)?;
        if entry.kind != NodeKind::Symlink {
            return Err(Status::Invalid);
        }
        let mount_name = entry.mount_name.clone();
        let Some(path) = entry.body.backing_path().cloned() else {
            return Err(Status::Invalid);
        };
        drop(entry);
        self.drain_invalidations_for_mount(&mount_name);
        if !self.inodes.contains_key(&id) {
            return Err(Status::Stale);
        }
        std::fs::read_link(path)
            .map(|target| target.as_os_str().as_encoded_bytes().to_vec())
            .map_err(|_| Status::Io)
    }

    fn open_state(
        &self,
        generation: u64,
        id: u64,
        clientid: u64,
        access: u32,
    ) -> StatusResult<OpenResult> {
        let entry = self.inodes.get(&id).ok_or(Status::Stale)?;
        let mount_name = entry.mount_name.clone();
        let path = entry.path.clone();
        let attrs = entry.attrs.clone();
        let body = entry.body.clone();
        drop(entry);
        self.drain_invalidations_for_mount(&mount_name);
        if !self.inodes.contains_key(&id) {
            return Err(Status::Stale);
        }

        Self::enforce_materialize_cap(
            &mount_name,
            &path,
            attrs.as_ref(),
            body.backing_path().map(PathBuf::as_path),
        )?;

        // The projected placeholder declares the read mode (a `ranged` route
        // carries `Deferred(Ranged)`), so `open` dispatches on it directly: no
        // discovery probe. A host-synthesized control / ignore file is full-mode
        // and falls through to the whole-file materialize path below.
        if body.is_provider()
            && let Some(projected_attrs) = attrs.as_ref()
            && projected_attrs.is_deferred_ranged()
            && let Some(result) = self.open_ranged_state(
                OpenSeed {
                    generation,
                    inode: id,
                    clientid,
                    access,
                    body: (),
                },
                &mount_name,
                &path,
                projected_attrs,
            )?
        {
            return Ok(result);
        }
        // If a route declared `ranged` but the provider answered full
        // (`Tree::open` returned `None`), fall through to the whole-file
        // materialize path so a mis-declared route degrades, not breaks.

        if let Some(backing_path) = body.backing_path().cloned() {
            let attr = self.attr(id)?;
            let stateid = self.opens.open(OpenSeed {
                generation,
                inode: id,
                clientid,
                access,
                body: OpenBody::Backing(BackingOpen {
                    id,
                    mount_name,
                    path,
                    backing_path,
                }),
            });
            return Ok(OpenResult { stateid, attr });
        }

        let data = self.read(id)?;
        // Belt-and-braces guard: providers that declared non-exact sizes can
        // still return arbitrarily large payloads. The pre-check above only
        // catches declared exact sizes.
        if u64::try_from(data.len()).unwrap_or(u64::MAX) > OPEN_MATERIALIZE_MAX_BYTES {
            tracing::warn!(
                op = "open",
                mount = %mount_name,
                path = %path,
                size = data.len(),
                cap = OPEN_MATERIALIZE_MAX_BYTES,
                "rejecting full-mode open: observed payload exceeds materialize cap"
            );
            return Err(Status::Resource);
        }
        let attr = self.attr(id)?;
        let stateid = self.opens.open(OpenSeed {
            generation,
            inode: id,
            clientid,
            access,
            body: OpenBody::Materialized(data),
        });
        Ok(OpenResult { stateid, attr })
    }

    fn validate_state(&self, stateid: StateId) -> StatusResult<()> {
        match self.opens.touch(stateid) {
            Ok(()) => Ok(()),
            Err(Status::Expired) => {
                self.close_expired_open(stateid);
                Err(Status::Expired)
            },
            Err(status) => Err(status),
        }
    }

    fn read_state(&self, stateid: StateId, offset: u64, count: u32) -> StatusResult<OpenRead> {
        let mount_name = match self.opens.with_state(stateid, |state| match &state.body {
            OpenBody::Materialized(_) => self
                .inodes
                .get(&state.inode)
                .map(|entry| entry.mount_name.clone())
                .ok_or(Status::Stale),
            OpenBody::Ranged(ranged) => Ok(ranged.mount_name.clone()),
            OpenBody::Backing(backing) => Ok(backing.mount_name.clone()),
        }) {
            Ok(Ok(mount_name)) => mount_name,
            Err(Status::Expired) => {
                self.close_expired_open(stateid);
                return Err(Status::Expired);
            },
            Ok(Err(status)) | Err(status) => return Err(status),
        };
        self.drain_invalidations_for_mount(&mount_name);

        match self.opens.with_state(stateid, |state| {
            ensure_read_access(state.access)?;
            match &mut state.body {
                OpenBody::Materialized(data) => {
                    let (data, eof) = open_data_slice(data, offset, count);
                    Ok(OpenRead {
                        id: state.inode,
                        data,
                        eof,
                    })
                },
                OpenBody::Ranged(ranged) => {
                    self.read_ranged_state(state.inode, ranged, offset, count)
                },
                OpenBody::Backing(backing) => Self::read_backing_state(backing, offset, count),
            }
        }) {
            Ok(result) => result,
            Err(Status::Expired) => {
                self.close_expired_open(stateid);
                Err(Status::Expired)
            },
            Err(status) => Err(status),
        }
    }

    fn close_state(&self, stateid: StateId) -> StatusResult<StateId> {
        match self.opens.close(stateid) {
            Ok((next_stateid, body)) => {
                self.close_open_body(body);
                Ok(next_stateid)
            },
            Err(Status::Expired) => {
                self.close_expired_open(stateid);
                Err(Status::Expired)
            },
            Err(status) => Err(status),
        }
    }

    fn renew_client(&self, clientid: u64) -> StatusResult<()> {
        self.opens.renew_client(clientid);
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use omnifs_host::HostContext;
    use omnifs_host::cloner::GitCloner;
    use tempfile::TempDir;
    use tokio::runtime::Runtime as TokioRuntime;

    struct TestExport {
        export: Export,
        _runtime: TokioRuntime,
        _cache_dir: TempDir,
        _config_dir: TempDir,
        _providers_dir: TempDir,
    }

    /// Build an `Export` over a `ProviderRegistry` with no mounts. Provider round
    /// trips therefore short-circuit on a missing mount (`runtime_for_mount`
    /// returns `None`), so these tests drive only the renderer-side budget /
    /// backing / learned-size logic. Mirrors the FUSE in-crate harness.
    fn empty_export() -> TestExport {
        let cache_dir = tempfile::tempdir().expect("cache dir");
        let config_dir = tempfile::tempdir().expect("config dir");
        let providers_dir = tempfile::tempdir().expect("providers dir");
        let credentials_file = config_dir.path().join("credentials.json");
        let cloner = Arc::new(GitCloner::new(cache_dir.path().join("clones")));
        let registry = ProviderRegistry::new(
            HostContext::new(
                cache_dir.path(),
                config_dir.path(),
                providers_dir.path(),
                &credentials_file,
            ),
            cloner,
        )
        .expect("registry init");

        let runtime = TokioRuntime::new().expect("tokio runtime");
        let export = Export::new(runtime.handle().clone(), Arc::new(registry));
        TestExport {
            export,
            _runtime: runtime,
            _cache_dir: cache_dir,
            _config_dir: config_dir,
            _providers_dir: providers_dir,
        }
    }

    fn test_path(name: &str) -> Path {
        Path::root().join(name).expect("test path segment is valid")
    }

    fn insert_full_mode_leaf(
        export: &Export,
        id: u64,
        name: &str,
        declared_size: view_types::FileSize,
    ) {
        let path = test_path(name);
        let attrs = FileAttrsCache::deferred(
            declared_size,
            view_types::ReadMode::Full,
            view_types::Stability::Stable,
            None,
        )
        .expect("test attrs are valid");
        let recorded_size = match declared_size {
            view_types::FileSize::Exact(n) => n,
            _ => 0,
        };
        export
            .path_to_inode
            .insert(ObjectKey::new(ROOT_ID, "test", &path), id);
        export.inodes.insert(
            id,
            NodeEntry {
                scope: ROOT_ID,
                mount_name: "test".to_string(),
                path,
                parent: ROOT_ID,
                kind: NodeKind::File,
                size: recorded_size,
                size_exact: matches!(declared_size, view_types::FileSize::Exact(_)),
                attrs: Some(attrs),
                body: EntryBody::Provider,
            },
        );
    }

    fn insert_backing_file(export: &Export, id: u64, name: &str, backing: PathBuf, size: u64) {
        let path = test_path(name);
        export
            .path_to_inode
            .insert(ObjectKey::new(ROOT_ID, "test", &path), id);
        export.inodes.insert(
            id,
            NodeEntry {
                scope: ROOT_ID,
                mount_name: "test".to_string(),
                path,
                parent: ROOT_ID,
                kind: NodeKind::File,
                size,
                size_exact: true,
                attrs: None,
                body: EntryBody::Backing(backing),
            },
        );
    }

    #[test]
    fn open_state_allows_non_exact_full_mode_to_reach_provider() {
        // Static-shape file routes can enter NFS with Unknown/Full placeholder
        // attrs before the file handler projects exact metadata. They must reach
        // provider materialization instead of being rejected by the pre-read
        // budget check. This empty registry has no runtime, so success for this
        // guard is the later NoEnt path rather than Resource.
        for size in [view_types::FileSize::Unknown, view_types::FileSize::NonZero] {
            let harness = empty_export();
            insert_full_mode_leaf(&harness.export, 700, "unknown-full", size);
            let result = harness.export.open_state(7, 700, 1, 1);
            assert!(
                matches!(result, Err(Status::NoEnt)),
                "expected provider lookup for full-mode {size:?}, got {result:?}"
            );
            assert!(harness.export.opens.active_inodes().is_empty());
        }
    }

    #[test]
    fn open_state_rejects_oversized_full_mode_declared_exact() {
        let harness = empty_export();
        insert_full_mode_leaf(
            &harness.export,
            500,
            "huge-full",
            view_types::FileSize::Exact(OPEN_MATERIALIZE_MAX_BYTES + 1),
        );

        let result = harness.export.open_state(
            7,
            500,
            harness.export.opens.active_inodes().len() as u64 + 1,
            1,
        );
        assert!(
            matches!(result, Err(Status::Resource)),
            "expected Resource for oversized full-mode OPEN, got {result:?}"
        );
        assert!(
            harness.export.opens.active_inodes().is_empty(),
            "OPEN must not register an open instance when it rejects the materialize"
        );
    }

    #[test]
    fn open_state_streams_oversized_backing_file() {
        let harness = empty_export();
        let temp = tempfile::tempdir().expect("backing tempdir");
        let backing = temp.path().join("huge.bin");
        let file = std::fs::File::create(&backing).expect("create backing file");
        file.set_len(OPEN_MATERIALIZE_MAX_BYTES + 1)
            .expect("set backing len");
        drop(file);

        insert_backing_file(
            &harness.export,
            600,
            "huge-backing",
            backing,
            OPEN_MATERIALIZE_MAX_BYTES + 1,
        );

        let opened = harness
            .export
            .open_state(7, 600, 1, 1)
            .expect("backing open");
        let chunk = harness
            .export
            .read_state(opened.stateid, OPEN_MATERIALIZE_MAX_BYTES, 8)
            .expect("backing read");
        assert_eq!(chunk.data, vec![0]);
        assert!(chunk.eof);
    }

    #[test]
    fn provider_rebind_preserves_resolved_backing_subtree() {
        let harness = empty_export();
        let temp = tempfile::tempdir().expect("backing tempdir");
        std::fs::write(temp.path().join("README.md"), b"hello from checkout\n")
            .expect("write backing child");
        let checkout = test_path("checkout");

        let id = harness.export.get_or_alloc(EntrySeed {
            scope: ROOT_ID,
            mount_name: "test",
            path: &checkout,
            parent: ROOT_ID,
            kind: NodeKind::Directory,
            size: 0,
            size_exact: true,
            attrs: None,
            body: EntryBody::Backing(temp.path().to_path_buf()),
        });

        let rebound = harness.export.get_or_alloc(EntrySeed {
            scope: ROOT_ID,
            mount_name: "test",
            path: &checkout,
            parent: ROOT_ID,
            kind: NodeKind::Directory,
            size: 0,
            size_exact: true,
            attrs: None,
            body: EntryBody::Provider,
        });

        assert_eq!(rebound, id);
        let readme = harness
            .export
            .lookup(id, "README.md")
            .expect("backing child lookup after provider rebind");
        assert_eq!(
            harness.export.read(readme).expect("backing child read"),
            b"hello from checkout\n".to_vec()
        );
    }
}
