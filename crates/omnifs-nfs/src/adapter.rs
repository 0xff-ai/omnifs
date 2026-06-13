//! NFSv4.0 export adapter over the renderer-neutral [`Tree`] projection core.
//!
//! `Export` is the NFS renderer: it owns the NFS-side identity and reply
//! concerns (the inode table that backs `(generation, id)` filehandles, the
//! stateid open tables, the `/omnifs` export-root alias, the expected-negative
//! probe table, the materialize cap, and `fattr4` size construction) and drives
//! all path resolution / listing / reads through `Tree::resolve_child`,
//! `Tree::list`, and `Tree::read`. The cache consult+populate, the cold provider
//! round trips, the `@next`/`@all` controls, the mount-root ignore synthesis,
//! the write fence, and learned-size promotion all live in `Tree`; the renderer
//! keeps only a learned-attrs slot on its inode table (so a learned size
//! survives across ops, exactly like the FUSE inode), the flatten-renderer
//! eager size probing for ranged children, and the inline-projection read path.
//!
//! There is no private object-metadata TTL table: an inode entry lives as long
//! as a path is referenced and is pruned only by explicit invalidation, mirroring
//! the FUSE adapter.

use crate::export::{
    Attr, DirEntry, DirListing, NodeKind, OpenRead, OpenResult, OpenSeed, OpenTable,
    ReadOnlyExport, StateId, StateIdOther, Status, StatusResult,
};
use crate::frontend;
use crate::frontend::LookupCacheHit;
use crate::protocol::consts::{
    EXPORT_ROOT_ID, MAX_NFS_READ_BYTES, NFS_EXPORT_NAME, OPEN_MATERIALIZE_MAX_BYTES, ROOT_ID,
};
use dashmap::DashMap;
use omnifs_cache::RecordKind;
use omnifs_core::path::{Path as ProtocolPath, Segment};
use omnifs_core::view as view_types;
use omnifs_core::view::{EntryMeta, FileAttrsCache};
use omnifs_host::path_key::PathKey;
use omnifs_host::registry::ProviderRegistry;
use omnifs_host::{Error as RuntimeError, Runtime};
use omnifs_tree::{
    Backing, Chunk, Entry as TreeEntry, ListOutcome, Listing, Node, RangedHandle, ReadResult,
    RequestCtx, Synthetic, Tree, TreeError, TreeErrorKind,
};
use omnifs_wit::provider::types as wit_types;
use std::hash::{DefaultHasher, Hash, Hasher};
use std::io::{Read, Seek, SeekFrom};
use std::path::{Path as FsPath, PathBuf};
use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::UNIX_EPOCH;
use tokio::runtime::{Handle, RuntimeFlavor};

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
    path: ProtocolPath,
    parent: u64,
    kind: NodeKind,
    size: u64,
    size_exact: bool,
    attrs: Option<FileAttrsCache>,
    backing_path: Option<PathBuf>,
    /// `Some` when this inode is a host-synthesized entry (`@next`/`@all` control
    /// or a mount-root ignore file). `read`/`open` serve its bytes through
    /// `Tree::read` (which runs the synthetic byte source) instead of a normal
    /// provider read.
    synthetic: Option<Synthetic>,
}

struct EntrySeed<'a> {
    scope: u64,
    mount_name: &'a str,
    path: &'a ProtocolPath,
    parent: u64,
    kind: NodeKind,
    size: u64,
    size_exact: bool,
    attrs: Option<FileAttrsCache>,
    backing_path: Option<PathBuf>,
    synthetic: Option<Synthetic>,
}

/// A live ranged open bound to a stateid. Holds the `Tree`-owned `RangedHandle`
/// (which owns its `Arc<Runtime>` + provider handle), so chunk reads and the
/// provider-handle release stay inside `Tree`. Not `Clone`: it owns the handle.
struct RangedOpen {
    mount_name: String,
    path: ProtocolPath,
    handle: RangedHandle,
}

#[derive(Debug, Clone)]
struct BackingOpen {
    id: u64,
    mount_name: String,
    path: ProtocolPath,
    backing_path: PathBuf,
}

#[derive(Clone, Debug, Eq, Hash, PartialEq)]
struct ObjectKey {
    scope: u64,
    mount: String,
    path: ProtocolPath,
}

impl ObjectKey {
    fn new(scope: u64, mount: impl Into<String>, path: &ProtocolPath) -> Self {
        Self {
            scope,
            mount: mount.into(),
            path: path.clone(),
        }
    }
}

pub struct Export {
    rt: Handle,
    /// The provider registry both the adapter and `Tree` hold. The adapter reads
    /// it only for mount enumeration and the synthetic NFS export root; all
    /// provider round trips go through `tree`.
    registry: Arc<ProviderRegistry>,
    /// The renderer-neutral projection core. Owns resolve/list/read decision
    /// logic; the NFS adapter enters the async runtime to call it and turns the
    /// neutral `Node`/`Listing`/`ReadResult` into NFS identity + `fattr4`.
    tree: Tree,
    inodes: DashMap<u64, NodeEntry>,
    path_to_inode: DashMap<ObjectKey, u64>,
    negative_lookups: DashMap<PathKey, ()>,
    next_ino: AtomicU64,
    root_mount: Option<String>,
    opens: OpenTable,
    ranged_opens: DashMap<StateIdOther, RangedOpen>,
    backing_opens: DashMap<StateIdOther, BackingOpen>,
}

impl Export {
    pub fn new(rt: Handle, registry: Arc<ProviderRegistry>) -> Self {
        let tree = Tree::new(Arc::clone(&registry));
        assert!(
            !matches!(rt.runtime_flavor(), RuntimeFlavor::CurrentThread),
            "NFS adapter requires a multi-thread Tokio runtime because sync NFS workers call Handle::block_on"
        );
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
                    path: ProtocolPath::root(),
                    parent: ROOT_ID,
                    kind: NodeKind::Directory,
                    size: 0,
                    size_exact: true,
                    attrs: None,
                    backing_path: None,
                    synthetic: None,
                },
            );
            if root_mount.is_some() {
                path_to_inode.insert(ObjectKey::new(scope, &mount, &ProtocolPath::root()), scope);
            }
        }
        Self {
            rt,
            registry,
            tree,
            inodes,
            path_to_inode,
            negative_lookups: DashMap::new(),
            next_ino: AtomicU64::new(EXPORT_ROOT_ID + 1),
            root_mount,
            opens: OpenTable::new(),
            ranged_opens: DashMap::new(),
            backing_opens: DashMap::new(),
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
    /// fan-out: prune the inode table, the negative-lookup table, and the open
    /// stateid tables (closing ranged provider handles).
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
        let matches = |path: &ProtocolPath| {
            report.paths.iter().any(|invalidated| invalidated == path)
                || report.prefixes.iter().any(|prefix| path.has_prefix(prefix))
        };

        let stale_negative_lookups = self
            .negative_lookups
            .iter()
            .filter_map(|entry| {
                let key = entry.key();
                let path = ProtocolPath::parse(&key.path).ok()?;
                (key.mount == mount_name && matches(&path)).then(|| key.clone())
            })
            .collect::<Vec<_>>();
        for key in stale_negative_lookups {
            self.negative_lookups.remove(&key);
        }

        let stale_paths = self
            .path_to_inode
            .iter()
            .filter_map(|entry| {
                let key = entry.key();
                (key.mount == mount_name && !key.path.is_root() && matches(&key.path))
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
        self.opens.remove_inodes(&stale_inodes);

        let stale_opens = self
            .ranged_opens
            .iter()
            .filter_map(|entry| {
                let ranged = entry.value();
                (ranged.mount_name == mount_name && matches(&ranged.path)).then(|| *entry.key())
            })
            .collect::<Vec<_>>();
        for stateid in stale_opens {
            if let Some((_, ranged)) = self.ranged_opens.remove(&stateid) {
                Self::close_ranged_provider_handle(ranged);
            }
        }

        let stale_backing_opens = self
            .backing_opens
            .iter()
            .filter_map(|entry| {
                let backing = entry.value();
                (backing.mount_name == mount_name && matches(&backing.path)).then(|| *entry.key())
            })
            .collect::<Vec<_>>();
        for stateid in stale_backing_opens {
            self.backing_opens.remove(&stateid);
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
            backing_path,
            synthetic,
        } = seed;
        let key = ObjectKey::new(scope, mount_name, path);
        let attrs_for_update = attrs.clone();
        let synthetic_for_update = synthetic.clone();
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
                        && let Some(merged_attrs) =
                            frontend::merge_file_attrs(entry.attrs.as_ref(), Some(incoming_attrs))
                    {
                        entry.size = merged_attrs.st_size();
                        entry.size_exact =
                            matches!(merged_attrs.size, view_types::FileSize::Exact(_));
                        entry.attrs = Some(merged_attrs);
                    }
                    // A genuine resolution carries an explicit synthetic state:
                    // a real provider/backing entry clears any prior synthetic
                    // marker (a real `.gitignore` wins), a synthetic entry sets
                    // it. Every caller passes the resolved state, so there is no
                    // origin-agnostic refresh to preserve.
                    entry.synthetic.clone_from(&synthetic_for_update);
                    if backing_path.is_some() {
                        entry.backing_path.clone_from(&backing_path);
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
                        backing_path,
                        synthetic,
                    },
                );
                id
            })
    }

    fn promote_file_attrs(&self, id: u64, attrs: FileAttrsCache) {
        if matches!(attrs.stability, view_types::Stability::Volatile) {
            return;
        }
        if let Some(mut entry) = self.inodes.get_mut(&id)
            && entry.kind == NodeKind::File
            && entry.backing_path.is_none()
        {
            entry.size = attrs.st_size();
            entry.size_exact = matches!(attrs.size, view_types::FileSize::Exact(_));
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
            attrs.version_token.hash(&mut hasher);
            std::mem::discriminant(&attrs.size).hash(&mut hasher);
            std::mem::discriminant(&attrs.bytes).hash(&mut hasher);
            std::mem::discriminant(&attrs.stability).hash(&mut hasher);
        }
        hasher.finish()
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
        match &meta.kind {
            view_types::EntryKind::Directory => NodeKind::Directory,
            view_types::EntryKind::File => NodeKind::File,
        }
    }

    fn meta_size(meta: &EntryMeta) -> (u64, bool) {
        let exact = match &meta.attrs {
            Some(attrs) => matches!(&attrs.size, view_types::FileSize::Exact(_)),
            None => true,
        };
        (meta.st_size(), exact)
    }

    /// Probe a ranged file's exact size by opening it through the provider. NFS
    /// flattens a directory into a finite snapshot whose `fattr4` carries each
    /// child's size, so a ranged child that lists as a static `Unknown`/`Full`
    /// placeholder must be promoted to its real size before `ls -l` stats it.
    /// FUSE has no analogue because it promotes size lazily on the inode at read
    /// time. The probe writes the learned attrs back through the cache so a later
    /// lookup serves them without re-probing.
    fn probe_ranged_attrs(
        &self,
        runtime: &Arc<Runtime>,
        path: &ProtocolPath,
    ) -> Option<FileAttrsCache> {
        let opened = match self
            .rt
            .block_on(runtime.namespace().open_file(path.as_str()))
        {
            Ok(opened) => opened,
            Err(RuntimeError::ProviderError(error))
                if matches!(
                    error.kind,
                    wit_types::ErrorKind::InvalidInput | wit_types::ErrorKind::NotFound
                ) =>
            {
                return None;
            },
            Err(RuntimeError::ProviderError(error)) => {
                tracing::warn!(
                    path = %path,
                    kind = ?error.kind,
                    retryable = error.retryable,
                    message = %error.message,
                    "NFS ranged attr probe failed"
                );
                return None;
            },
            Err(error) => {
                tracing::warn!(path = %path, error = %error, "NFS ranged attr probe failed");
                return None;
            },
        };

        let attrs = FileAttrsCache {
            size: omnifs_host::wit_protocol::file_size_from_wit(opened.attrs.size),
            bytes: view_types::ByteSource::Deferred(view_types::ReadMode::Ranged),
            stability: omnifs_host::wit_protocol::stability_from_wit(opened.attrs.stability),
            version_token: opened.attrs.version_token,
        };
        if let Err(error) = attrs.validate() {
            tracing::warn!(path = %path, error, "NFS ranged attr probe returned invalid attrs");
            let _ = runtime.call_close_file(opened.handle);
            return None;
        }
        if let Err(error) = runtime.call_close_file(opened.handle) {
            tracing::warn!(path = %path, error = %error, "NFS ranged attr probe close failed");
        }
        Some(attrs)
    }

    /// Promote a static-placeholder file meta to its real ranged attrs via a
    /// provider open-probe, caching the learned attrs. A non-placeholder meta is
    /// returned unchanged.
    fn promote_static_placeholder_meta(
        &self,
        runtime: &Arc<Runtime>,
        child_path: &ProtocolPath,
        mut meta: EntryMeta,
    ) -> EntryMeta {
        if meta
            .attrs
            .as_ref()
            .is_some_and(Self::is_static_file_placeholder)
            && let Some(attrs) = self.probe_ranged_attrs(runtime, child_path)
        {
            frontend::cache_file_metadata(runtime, child_path, attrs.clone());
            meta = EntryMeta::file(attrs);
        }
        meta
    }

    fn is_static_file_placeholder(attrs: &FileAttrsCache) -> bool {
        matches!(attrs.size, view_types::FileSize::Unknown)
            && matches!(
                attrs.bytes,
                view_types::ByteSource::Deferred(view_types::ReadMode::Full)
            )
    }

    fn tree_status(error: &TreeError) -> Status {
        match error.kind {
            TreeErrorKind::NotFound => Status::NoEnt,
            TreeErrorKind::NotDirectory => Status::NotDir,
            TreeErrorKind::IsDirectory => Status::IsDir,
            TreeErrorKind::PermissionDenied => Status::Access,
            TreeErrorKind::InvalidInput => Status::Invalid,
            TreeErrorKind::TooLarge
            | TreeErrorKind::RateLimited
            | TreeErrorKind::Timeout
            | TreeErrorKind::Network
            | TreeErrorKind::Internal => Status::Io,
        }
    }

    fn expected_negative_probe(name: &str) -> bool {
        name == ".DS_Store" || name.starts_with("._")
    }

    /// Resolve a child from the parent's cached dirents record, if present. NFS
    /// consults a non-exhaustive cached listing for a positive entry so a probe
    /// name (e.g. `.DS_Store`) seen in a partial listing beats the
    /// expected-negative shortcut. Returns `None` when the record is absent or
    /// the name is not a positive entry; the caller then falls through to `Tree`.
    fn lookup_from_cached_dirents(
        &self,
        scope: u64,
        mount_name: &str,
        parent_path: &ProtocolPath,
        parent: u64,
        name: &Segment,
        runtime: &Arc<Runtime>,
    ) -> Option<u64> {
        let record = frontend::cache_get(runtime, parent_path, RecordKind::Dirents, None)?;
        let LookupCacheHit::Positive(meta) =
            frontend::cached_dirent_lookup(&record, name.as_str())?
        else {
            return None;
        };
        let child_path = parent_path.join_segment(name);
        Some(self.allocate_meta_entry(scope, mount_name, &child_path, parent, meta, Some(runtime)))
    }

    /// Allocate an inode for a resolved positive `meta`, promoting a static
    /// ranged placeholder to its probed size and clearing any stale negative.
    fn allocate_meta_entry(
        &self,
        scope: u64,
        mount_name: &str,
        child_path: &ProtocolPath,
        parent: u64,
        mut meta: EntryMeta,
        runtime: Option<&Arc<Runtime>>,
    ) -> u64 {
        self.negative_lookups
            .remove(&PathKey::new(mount_name, child_path.as_str()));
        if let Some(runtime) = runtime {
            meta = self.promote_static_placeholder_meta(runtime, child_path, meta);
        }
        let kind = Self::meta_kind(&meta);
        let (size, size_exact) = Self::meta_size(&meta);
        self.get_or_alloc(EntrySeed {
            scope,
            mount_name,
            path: child_path,
            parent,
            kind,
            size,
            size_exact,
            attrs: meta.attrs,
            backing_path: None,
            synthetic: None,
        })
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
        parent_path: &ProtocolPath,
        parent: u64,
        name: &Segment,
        runtime: &Arc<Runtime>,
    ) -> StatusResult<u64> {
        let parent_node = provider_dir_node(mount_name, parent_path);
        let ctx = RequestCtx::default();
        let child_path = parent_path.join_segment(name);
        match self
            .rt
            .block_on(self.tree.resolve_child(&parent_node, name.as_str(), &ctx))
        {
            Ok(node) => Ok(self.bind_node(scope, mount_name, parent, &node, Some(runtime))),
            Err(error) if error.kind == TreeErrorKind::NotFound => {
                if Self::expected_negative_probe(name.as_str()) {
                    self.negative_lookups
                        .insert(PathKey::new(mount_name, child_path.as_str()), ());
                }
                Err(Status::NoEnt)
            },
            Err(error) => {
                tracing::warn!(
                    op = "lookup",
                    mount = %mount_name,
                    parent = %parent_path,
                    name = %name,
                    error = %error,
                    "NFS Tree lookup failed"
                );
                Err(Self::tree_status(&error))
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
        self.negative_lookups
            .remove(&PathKey::new(mount_name, child_path.as_str()));
        if let Backing::Subtree(dir) = node.backing() {
            return self.get_or_alloc(EntrySeed {
                scope,
                mount_name,
                path: &child_path,
                parent,
                kind: NodeKind::Directory,
                size: 0,
                size_exact: true,
                attrs: None,
                backing_path: Some(dir.clone()),
                synthetic: None,
            });
        }

        let mut meta = node_meta(node);
        if let (None, Some(runtime)) = (node.synthetic_kind(), runtime) {
            meta = self.promote_static_placeholder_meta(runtime, &child_path, meta);
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
            attrs: meta.attrs,
            backing_path: None,
            synthetic: node.synthetic_kind().cloned(),
        })
    }

    fn lookup_backing_child(
        &self,
        scope: u64,
        mount_name: &str,
        parent_path: &ProtocolPath,
        parent: u64,
        name: &Segment,
        root: &FsPath,
    ) -> StatusResult<u64> {
        let child = root.join(name.as_str());
        let metadata = std::fs::symlink_metadata(&child).map_err(|_| Status::NoEnt)?;
        let kind = Self::backing_kind(&metadata)?;
        let child_path = parent_path.join_segment(name);
        self.negative_lookups
            .remove(&PathKey::new(mount_name, child_path.as_str()));
        Ok(self.get_or_alloc(EntrySeed {
            scope,
            mount_name,
            path: &child_path,
            parent,
            kind,
            size: metadata.len(),
            size_exact: true,
            attrs: None,
            backing_path: Some(child),
            synthetic: None,
        }))
    }

    /// Build a finite directory snapshot from a `Tree` `Listing`: provider
    /// children (each ranged-probed and inode-allocated), then the host-synthesized
    /// `@next`/`@all` controls and mount-root ignore files materialized as files.
    fn snapshot_from_listing(
        &self,
        scope: u64,
        mount_name: &str,
        path: &ProtocolPath,
        parent: u64,
        listing: &Listing,
        runtime: &Arc<Runtime>,
    ) -> DirListing {
        let mut entries = Vec::with_capacity(listing.entries.len() + listing.synthetic.len());
        for entry in &listing.entries {
            if let Some(dir_entry) =
                self.dir_entry_from_tree(scope, mount_name, path, parent, entry, Some(runtime))
            {
                entries.push(dir_entry);
            }
        }
        for entry in &listing.synthetic {
            if let Some(dir_entry) =
                self.dir_entry_from_tree(scope, mount_name, path, parent, entry, None)
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
        path: &ProtocolPath,
        parent: u64,
        entry: &TreeEntry,
        runtime: Option<&Arc<Runtime>>,
    ) -> Option<DirEntry> {
        let name = Segment::try_from(entry.name.as_str()).ok()?;
        let child_path = path.join_segment(&name);
        self.negative_lookups
            .remove(&PathKey::new(mount_name, child_path.as_str()));
        let mut meta = entry.meta.clone();
        if let (None, Some(runtime)) = (entry.synthetic.as_ref(), runtime) {
            meta = self.promote_static_placeholder_meta(runtime, &child_path, meta);
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
            attrs: meta.attrs,
            backing_path: None,
            synthetic: entry.synthetic.clone(),
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
        path: &ProtocolPath,
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
                backing_path: Some(backing_path),
                synthetic: None,
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

    /// Read a provider-backed file. A host-synthesized node and a treeref backing
    /// node are served by `Tree::read`; an inline-projection node (whose bytes
    /// travel in its cached attrs, with no provider file route) is served from
    /// those bytes directly. Everything else renders through `Tree::read`, which
    /// owns the cache cascade, the write fence, and learned-size promotion.
    fn read_provider_file(
        &self,
        id: u64,
        mount_name: &str,
        path: &ProtocolPath,
        attrs: Option<&FileAttrsCache>,
        synthetic: Option<Synthetic>,
        runtime: &Arc<Runtime>,
    ) -> StatusResult<Vec<u8>> {
        // Inline cached projection: the bytes live in the attrs (a manually
        // cached dirents/lookup entry with no provider file route), so serve them
        // directly and learn the exact size, without a provider round trip.
        if synthetic.is_none()
            && let Some(attrs) =
                frontend::cached_file_attrs(runtime, path).or_else(|| attrs.cloned())
            && let Some(bytes) = attrs.inline_bytes()
        {
            let data = bytes.to_vec();
            let attrs = frontend::learned_full_read_attrs(attrs.clone(), data.len());
            if !frontend::full_read_matches_attrs(&attrs, data.len()) {
                tracing::warn!(
                    path = %path,
                    expected = ?attrs.size,
                    actual = data.len(),
                    "cached inline file attrs contradict content length"
                );
                return Err(Status::Io);
            }
            self.promote_file_attrs(id, attrs.clone());
            frontend::cache_file_metadata(runtime, path, attrs);
            return Ok(data);
        }

        let node = file_node(mount_name, path, attrs, synthetic);
        let ctx = RequestCtx::default();
        match self.rt.block_on(self.tree.read(&node, &ctx)) {
            Ok(ReadResult::Bytes {
                data,
                attrs: read_attrs,
                ..
            }) => {
                if let Some(read_attrs) = read_attrs {
                    self.promote_file_attrs(id, read_attrs.clone());
                    frontend::cache_file_metadata(runtime, path, read_attrs);
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
                Err(Self::tree_status(&error))
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
                return Err(Self::tree_status(&error));
            },
        };
        if let Some(attrs) = learned_attrs {
            self.promote_file_attrs(id, attrs.clone());
            if let Some(runtime) = self.runtime_for_mount(&ranged.mount_name) {
                frontend::cache_file_metadata(&runtime, &ranged.path, attrs);
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
        path: &ProtocolPath,
        attrs: Option<&FileAttrsCache>,
        backing_path: Option<&FsPath>,
    ) -> StatusResult<()> {
        if backing_path.is_some() {
            return Ok(());
        }
        if let Some(projected_attrs) = attrs
            && matches!(
                projected_attrs.bytes,
                view_types::ByteSource::Deferred(view_types::ReadMode::Full)
            )
        {
            match projected_attrs.size {
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
    /// stateid table.
    fn close_ranged_provider_handle(ranged: RangedOpen) {
        let mount = ranged.mount_name.clone();
        let path = ranged.path.clone();
        if let Err(error) = ranged.handle.close() {
            tracing::warn!(
                mount = %mount,
                path = %path,
                error = %error,
                "NFS runtime ranged close failed"
            );
        }
    }

    fn promote_ranged_attrs_for_open(
        &self,
        id: u64,
        mount_name: &str,
        path: &ProtocolPath,
        attrs: &mut Option<FileAttrsCache>,
        backing_path: Option<&FsPath>,
    ) {
        if backing_path.is_some()
            || attrs
                .as_ref()
                .is_some_and(|attrs| !Self::is_static_file_placeholder(attrs))
        {
            return;
        }
        let Some(runtime) = self.runtime_for_mount(mount_name) else {
            return;
        };
        let Some(probed_attrs) = self.probe_ranged_attrs(&runtime, path) else {
            return;
        };
        self.promote_file_attrs(id, probed_attrs.clone());
        frontend::cache_file_metadata(&runtime, path, probed_attrs.clone());
        *attrs = Some(probed_attrs);
    }

    /// Open a `Deferred(Ranged)` file through `Tree::open` and register the
    /// resulting handle under a fresh stateid. `Tree` owns the provider open and
    /// chunk reads; the adapter keeps the renderer-side stateid binding.
    fn open_ranged_state(
        &self,
        seed: OpenSeed,
        mount_name: String,
        path: ProtocolPath,
        projected_attrs: &FileAttrsCache,
        runtime: &Arc<Runtime>,
    ) -> StatusResult<OpenResult> {
        let node = file_node(&mount_name, &path, Some(projected_attrs), None);
        let ctx = RequestCtx::default();
        let handle = match self.rt.block_on(self.tree.open(&node, &ctx)) {
            Ok(handle) => handle,
            Err(error) => {
                tracing::warn!(
                    op = "open",
                    mount = %mount_name,
                    path = %path,
                    error = %error,
                    "NFS Tree ranged open failed"
                );
                return Err(Self::tree_status(&error));
            },
        };
        let opened_attrs = handle.attrs().clone();
        if let Err(error) = opened_attrs.validate() {
            tracing::warn!(path = %path, error, "open-file returned invalid attrs");
            let _ = handle.close();
            return Err(Status::Io);
        }
        self.promote_file_attrs(seed.inode, opened_attrs.clone());
        frontend::cache_file_metadata(runtime, &path, opened_attrs);
        let attr = match self.attr(seed.inode) {
            Ok(attr) => attr,
            Err(status) => {
                let _ = handle.close();
                return Err(status);
            },
        };
        let stateid = self.opens.open(seed);
        self.ranged_opens.insert(
            stateid.other(),
            RangedOpen {
                mount_name,
                path,
                handle,
            },
        );
        Ok(OpenResult { stateid, attr })
    }
}

/// The `EntryMeta` a resolved `Node` projects (kind + optional attrs).
fn node_meta(node: &Node) -> EntryMeta {
    EntryMeta {
        kind: node.kind(),
        attrs: node.attrs().cloned(),
    }
}

/// Build the provider-backed (or synthetic) file `Node` `Tree::read`/`Tree::open`
/// consume, from inode-cached projection state.
fn file_node(
    mount_name: &str,
    path: &ProtocolPath,
    attrs: Option<&FileAttrsCache>,
    synthetic: Option<Synthetic>,
) -> Node {
    let meta = match attrs {
        Some(attrs) => EntryMeta::file(attrs.clone()),
        None => EntryMeta {
            kind: view_types::EntryKind::File,
            attrs: None,
        },
    };
    match synthetic {
        Some(synthetic) => Node::synthetic(mount_name.to_string(), path.clone(), meta, synthetic),
        None => Node::new(
            mount_name.to_string(),
            path.clone(),
            meta,
            Backing::Provider,
        ),
    }
}

/// Build the minimal provider-backed directory `Node` `Tree` needs to resolve a
/// child or list a directory. The inode table has already proved this is a dir.
fn provider_dir_node(mount_name: &str, path: &ProtocolPath) -> Node {
    Node::new(
        mount_name.to_string(),
        path.clone(),
        EntryMeta::directory(),
        Backing::Provider,
    )
}

impl ReadOnlyExport for Export {
    fn root(&self) -> u64 {
        ROOT_ID
    }

    fn attr(&self, id: u64) -> StatusResult<Attr> {
        let entry = self.inodes.get(&id).ok_or(Status::Stale)?;
        let mount_name = entry.mount_name.clone();
        let backing_path = entry.backing_path.clone();
        drop(entry);

        self.drain_invalidations_for_mount(&mount_name);

        let entry = self.inodes.get(&id).ok_or(Status::Stale)?;
        if let Some(path) = &backing_path {
            let metadata = std::fs::symlink_metadata(path).map_err(|_| Status::Stale)?;
            Self::attr_from_metadata(id, entry.parent, &metadata)
        } else {
            Ok(Self::attr_from_entry(id, &entry))
        }
    }

    fn lookup(&self, parent: u64, name: &str) -> StatusResult<u64> {
        let name = Segment::try_from(name).map_err(|_| Status::Invalid)?;

        if (parent == ROOT_ID || parent == EXPORT_ROOT_ID)
            && self.root_mount.is_none()
            && self.registry.get(name.as_str()).is_some()
        {
            return Ok(self.get_or_alloc(EntrySeed {
                scope: parent,
                mount_name: name.as_str(),
                path: &ProtocolPath::root(),
                parent,
                kind: NodeKind::Directory,
                size: 0,
                size_exact: true,
                attrs: None,
                backing_path: None,
                synthetic: None,
            }));
        }

        if parent == ROOT_ID && name.as_str() == NFS_EXPORT_NAME {
            return Ok(EXPORT_ROOT_ID);
        }

        let parent_entry = self.inodes.get(&parent).ok_or(Status::Stale)?;
        if parent_entry.kind != NodeKind::Directory {
            return Err(Status::NotDir);
        }
        let mount_name = parent_entry.mount_name.clone();
        let parent_path = parent_entry.path.clone();
        let scope = parent_entry.scope;
        let backing_path = parent_entry.backing_path.clone();
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
        let child_path = parent_path.join_segment(&name);

        // NFS-specific: a positive entry in a (possibly non-exhaustive) cached
        // listing beats the expected-negative shortcut.
        if let Some(id) = self.lookup_from_cached_dirents(
            scope,
            &mount_name,
            &parent_path,
            parent,
            &name,
            &runtime,
        ) {
            return Ok(id);
        }

        if Self::expected_negative_probe(name.as_str())
            && self
                .negative_lookups
                .contains_key(&PathKey::new(&mount_name, child_path.as_str()))
        {
            return Err(Status::NoEnt);
        }

        self.lookup_via_tree(scope, &mount_name, &parent_path, parent, &name, &runtime)
    }

    fn readdir(&self, id: u64) -> StatusResult<DirListing> {
        if (id == ROOT_ID || id == EXPORT_ROOT_ID) && self.root_mount.is_none() {
            let mut mounts = self.registry.mounts();
            mounts.sort();
            let entries = mounts
                .into_iter()
                .map(|mount| {
                    let child = self.get_or_alloc(EntrySeed {
                        scope: id,
                        mount_name: &mount,
                        path: &ProtocolPath::root(),
                        parent: id,
                        kind: NodeKind::Directory,
                        size: 0,
                        size_exact: true,
                        attrs: None,
                        backing_path: None,
                        synthetic: None,
                    });
                    DirEntry {
                        id: child,
                        name: mount,
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
        let backing_path = entry.backing_path.clone();
        drop(entry);

        self.drain_invalidations_for_mount(&mount_name);
        if !self.inodes.contains_key(&id) {
            return Err(Status::Stale);
        }

        if let Some(root) = backing_path {
            return self.readdir_backing(scope, &mount_name, &path, id, &root);
        }

        let runtime = self.runtime_for_mount(&mount_name).ok_or(Status::NoEnt)?;
        let node = provider_dir_node(&mount_name, &path);
        let ctx = RequestCtx::default();
        match self.rt.block_on(self.tree.list(&node, None, &ctx)) {
            Ok(ListOutcome::Listing(listing)) => {
                Ok(self.snapshot_from_listing(scope, &mount_name, &path, id, &listing, &runtime))
            },
            Ok(ListOutcome::Subtree(dir)) => {
                if let Some(mut entry) = self.inodes.get_mut(&id)
                    && entry.backing_path.is_none()
                {
                    entry.backing_path = Some(dir.clone());
                }
                self.readdir_backing(scope, &mount_name, &path, id, &dir)
            },
            Err(error) => {
                tracing::warn!(
                    op = "readdir",
                    mount = %mount_name,
                    path = %path,
                    error = %error,
                    "NFS Tree readdir failed"
                );
                Err(Self::tree_status(&error))
            },
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
        let backing_path = entry.backing_path.clone();
        let synthetic = entry.synthetic.clone();
        drop(entry);

        self.drain_invalidations_for_mount(&mount_name);
        if !self.inodes.contains_key(&id) {
            return Err(Status::Stale);
        }

        if let Some(backing_path) = backing_path {
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

        let runtime = self.runtime_for_mount(&mount_name).ok_or(Status::NoEnt)?;
        self.read_provider_file(id, &mount_name, &path, attrs.as_ref(), synthetic, &runtime)
    }

    fn readlink(&self, id: u64) -> StatusResult<Vec<u8>> {
        let entry = self.inodes.get(&id).ok_or(Status::Stale)?;
        if entry.kind != NodeKind::Symlink {
            return Err(Status::Invalid);
        }
        let mount_name = entry.mount_name.clone();
        let Some(path) = entry.backing_path.clone() else {
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
        let mut attrs = entry.attrs.clone();
        let backing_path = entry.backing_path.clone();
        let synthetic = entry.synthetic.clone();
        drop(entry);
        self.drain_invalidations_for_mount(&mount_name);
        if !self.inodes.contains_key(&id) {
            return Err(Status::Stale);
        }

        // A host-synthesized control / ignore file is materialized whole through
        // `Tree::read` like any other full-mode file (no ranged streaming).
        if synthetic.is_none() {
            self.promote_ranged_attrs_for_open(
                id,
                &mount_name,
                &path,
                &mut attrs,
                backing_path.as_deref(),
            );
        }

        Self::enforce_materialize_cap(&mount_name, &path, attrs.as_ref(), backing_path.as_deref())?;

        if synthetic.is_none()
            && backing_path.is_none()
            && let Some(projected_attrs) = attrs.as_ref()
            && matches!(
                projected_attrs.bytes,
                view_types::ByteSource::Deferred(view_types::ReadMode::Ranged)
            )
        {
            let runtime = self.runtime_for_mount(&mount_name).ok_or(Status::NoEnt)?;
            return self.open_ranged_state(
                OpenSeed {
                    generation,
                    inode: id,
                    clientid,
                    access,
                    materialized_bytes: Vec::new(),
                },
                mount_name,
                path,
                projected_attrs,
                &runtime,
            );
        }

        if let Some(backing_path) = backing_path {
            let attr = self.attr(id)?;
            let stateid = self.opens.open(OpenSeed {
                generation,
                inode: id,
                clientid,
                access,
                materialized_bytes: Vec::new(),
            });
            self.backing_opens.insert(
                stateid.other(),
                BackingOpen {
                    id,
                    mount_name,
                    path,
                    backing_path,
                },
            );
            return Ok(OpenResult { stateid, attr });
        }

        let data = self.materialize_for_open(id)?;
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
            materialized_bytes: data,
        });
        Ok(OpenResult { stateid, attr })
    }

    fn validate_state(&self, stateid: StateId) -> StatusResult<()> {
        match self.opens.touch(stateid) {
            Ok(_) => Ok(()),
            Err(Status::Expired) => {
                if let Some((_, ranged)) = self.ranged_opens.remove(&stateid.other()) {
                    Self::close_ranged_provider_handle(ranged);
                }
                self.backing_opens.remove(&stateid.other());
                Err(Status::Expired)
            },
            Err(status) => Err(status),
        }
    }

    fn read_state(&self, stateid: StateId, offset: u64, count: u32) -> StatusResult<OpenRead> {
        if let Some(backing) = self
            .backing_opens
            .get(&stateid.other())
            .map(|entry| entry.clone())
        {
            self.drain_invalidations_for_mount(&backing.mount_name);
            if !self.backing_opens.contains_key(&stateid.other()) {
                return Err(Status::BadStateId);
            }
            match self.opens.read_info(stateid) {
                Ok(_) => {},
                Err(Status::Expired) => {
                    self.backing_opens.remove(&stateid.other());
                    return Err(Status::Expired);
                },
                Err(status) => return Err(status),
            }
            return Self::read_backing_state(&backing, offset, count);
        }

        if let Some(mount_name) = self
            .ranged_opens
            .get(&stateid.other())
            .map(|entry| entry.mount_name.clone())
        {
            // Drain before re-binding the open: an invalidation may evict this
            // stateid (closing its provider handle). The drain must not run while
            // a `ranged_opens` guard is held, so the mount name is copied out
            // first and the entry is re-acquired afterward.
            self.drain_invalidations_for_mount(&mount_name);
            let info = match self.opens.read_info(stateid) {
                Ok(info) => info,
                Err(Status::Expired) => {
                    if let Some((_, ranged)) = self.ranged_opens.remove(&stateid.other()) {
                        Self::close_ranged_provider_handle(ranged);
                    }
                    return Err(Status::Expired);
                },
                Err(status) => return Err(status),
            };
            // `read_ranged_state` drives the `RangedHandle` (which only touches
            // the runtime, never `ranged_opens`), so holding the guard across the
            // chunk read is safe.
            let Some(ranged) = self.ranged_opens.get(&stateid.other()) else {
                return Err(Status::BadStateId);
            };
            return self.read_ranged_state(info.id, &ranged, offset, count);
        }

        let info = self.opens.touch(stateid)?;
        let entry = self.inodes.get(&info.id).ok_or(Status::Stale)?;
        let mount_name = entry.mount_name.clone();
        drop(entry);
        self.drain_invalidations_for_mount(&mount_name);
        self.opens.read(stateid, offset, count)
    }

    fn close_state(&self, stateid: StateId) -> StatusResult<StateId> {
        match self.opens.close(stateid) {
            Ok(next_stateid) => {
                if let Some((_, ranged)) = self.ranged_opens.remove(&stateid.other()) {
                    Self::close_ranged_provider_handle(ranged);
                }
                self.backing_opens.remove(&stateid.other());
                Ok(next_stateid)
            },
            Err(Status::Expired) => {
                if let Some((_, ranged)) = self.ranged_opens.remove(&stateid.other()) {
                    Self::close_ranged_provider_handle(ranged);
                }
                self.backing_opens.remove(&stateid.other());
                Err(Status::Expired)
            },
            Err(status) => Err(status),
        }
    }

    fn renew_client(&self, clientid: u64) -> StatusResult<()> {
        self.opens.renew_client(clientid);
        Ok(())
    }

    fn materialize_for_open(&self, id: u64) -> StatusResult<Vec<u8>> {
        self.read(id)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use omnifs_host::Dirs;
    use omnifs_host::cloner::GitCloner;
    use omnifs_host::tools::archive::ARCHIVE_TOOL_WASM;
    use std::path::Path;
    use tempfile::TempDir;
    use tokio::runtime::Runtime as TokioRuntime;

    struct TestExport {
        export: Export,
        _runtime: TokioRuntime,
        _cache_dir: TempDir,
        _config_dir: TempDir,
        _providers_dir: TempDir,
    }

    fn wasm_artifact_path(file_name: &str) -> PathBuf {
        let workspace_root = Path::new(env!("CARGO_MANIFEST_DIR"))
            .parent()
            .expect("crate must have a workspace parent")
            .parent()
            .expect("workspace root must exist");
        workspace_root
            .join("target")
            .join("wasm32-wasip2")
            .join("release")
            .join(file_name)
    }

    /// Build an `Export` over a `ProviderRegistry` with no mounts. Provider round
    /// trips therefore short-circuit on a missing mount (`runtime_for_mount`
    /// returns `None`), so these tests drive only the renderer-side budget /
    /// backing / learned-size logic. Mirrors the FUSE in-crate harness.
    fn empty_export() -> TestExport {
        let cache_dir = tempfile::tempdir().expect("cache dir");
        let config_dir = tempfile::tempdir().expect("config dir");
        let providers_dir = tempfile::tempdir().expect("providers dir");
        let src = wasm_artifact_path(ARCHIVE_TOOL_WASM);
        assert!(
            src.exists(),
            "{ARCHIVE_TOOL_WASM} missing at {}. Run `just providers-build` first.",
            src.display()
        );
        std::fs::copy(&src, providers_dir.path().join(ARCHIVE_TOOL_WASM)).expect("copy wasm");
        let credentials_file = config_dir.path().join("credentials.json");
        let cloner = Arc::new(GitCloner::new(cache_dir.path().join("clones")));
        let registry = ProviderRegistry::new(
            Dirs::new(
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

    fn attrs(
        size: view_types::FileSize,
        stability: view_types::Stability,
        version_token: Option<&str>,
    ) -> FileAttrsCache {
        FileAttrsCache {
            size,
            bytes: view_types::ByteSource::Deferred(view_types::ReadMode::Full),
            stability,
            version_token: version_token.map(str::to_string),
        }
    }

    fn exact_size(attrs: &FileAttrsCache) -> Option<u64> {
        match attrs.size {
            view_types::FileSize::Exact(size) => Some(size),
            view_types::FileSize::NonZero | view_types::FileSize::Unknown => None,
        }
    }

    fn test_path(name: &str) -> ProtocolPath {
        ProtocolPath::root()
            .join(name)
            .expect("test path segment is valid")
    }

    fn insert_full_mode_leaf(
        export: &Export,
        id: u64,
        name: &str,
        declared_size: view_types::FileSize,
    ) {
        let path = test_path(name);
        let attrs = FileAttrsCache {
            size: declared_size,
            bytes: view_types::ByteSource::Deferred(view_types::ReadMode::Full),
            stability: view_types::Stability::Immutable,
            version_token: None,
        };
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
                backing_path: None,
                synthetic: None,
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
                backing_path: Some(backing),
                synthetic: None,
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
    fn learned_exact_size_survives_only_same_version_non_exact_refresh() {
        for (existing_version, incoming_size, incoming_version, expected) in [
            (
                Some("v1"),
                view_types::FileSize::Unknown,
                Some("v1"),
                Some(42),
            ),
            (
                Some("v1"),
                view_types::FileSize::Exact(7),
                Some("v1"),
                Some(7),
            ),
            (
                Some("current"),
                view_types::FileSize::Unknown,
                Some("next"),
                None,
            ),
            (None, view_types::FileSize::Unknown, None, None),
        ] {
            let existing = attrs(
                view_types::FileSize::Exact(42),
                view_types::Stability::Mutable,
                existing_version,
            );
            let incoming = attrs(
                incoming_size,
                view_types::Stability::Mutable,
                incoming_version,
            );

            let merged = frontend::merge_file_attrs(Some(&existing), Some(incoming)).unwrap();
            assert_eq!(
                exact_size(&merged),
                expected,
                "existing_version={existing_version:?}, incoming_size={incoming_size:?}, incoming_version={incoming_version:?}"
            );
        }
    }
}
