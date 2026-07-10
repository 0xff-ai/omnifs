//! NFSv4.0 export adapter over the engine [`Namespace`] surface.
//!
//! `Export` is the NFS renderer. It owns NFS protocol state only: the inode
//! table that backs `(generation, id)` filehandles, the stateid open tables, the
//! `/omnifs` export-root alias, the `NFS4ERR_DELAY` deferral policy, and `fattr4`
//! construction. Every projection answer (name resolution, attributes, directory
//! paging, byte reads) comes from a [`Namespace`]: the adapter never reaches into
//! the projection tree, its caches, or its render/identity machinery.
//!
//! The inode table maps an NFS inode id to a namespace [`NodeId`] (or a backing
//! filesystem path for a resolved treeref subtree), plus the protocol-local
//! parent/scope/kind. Two export roots (`ROOT_ID` and the `/omnifs`
//! `EXPORT_ROOT_ID` alias) both project [`NodeId::ROOT`]; the same node reached
//! under the two roots gets two distinct inodes so filehandles stay
//! scope-stable.
//!
//! Invalidation and live growth arrive as [`NsEvent`]s on a subscription the
//! adapter drains inline after every namespace op (see
//! [`EventStream::try_recv`]), so a stat that observes its own invalidation
//! prunes and closes stale opens before it re-reads its inode, and a polling
//! `tail -f` picks up an `AttrsChanged` grown size on its next re-stat.

use crate::delayed::{DeferOutcome, Key, Listings};
use crate::export::{
    Attr, DirEntry, DirListing, NodeKind, OpenRead, OpenResult, OpenSeed, OpenTable,
    ReadOnlyExport, StateId, Status, StatusResult, ensure_read_access,
};
use crate::persist::{PersistInit, PersistTables, Persister};
use crate::protocol::consts::{EXPORT_ROOT_ID, MAX_NFS_READ_BYTES, NFS_EXPORT_NAME, ROOT_ID};
use dashmap::DashMap;
use omnifs_core::path::Segment;
use omnifs_engine::namespace::{
    Attrs, DirCursor, DirEntry as NsDirEntry, EntryKind, EventStream, Namespace, NodeId, NsError,
    NsEvent,
};
use std::future::Future;
use std::hash::{DefaultHasher, Hash, Hasher};
use std::io::{Read, Seek, SeekFrom};
use std::path::{Path, PathBuf};
use std::pin::Pin;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, UNIX_EPOCH};
use tokio::runtime::{Handle, RuntimeFlavor};

/// Inline wait budget for proactive `READDIR` deferral ([`delayed::Listings`]).
/// Past this duration the handler replies `NFS4ERR_DELAY` while the listing task
/// keeps running in the background. Short enough that a cold listing never holds
/// the reply; long enough that a warm listing still answers in one round trip.
/// Distinct from reactive `DELAY` in [`Status::from`](crate::export::Status) for
/// [`NsError`], which maps transient upstream errors on any op without background
/// continuation. Only `READDIR` uses proactive deferral; `LOOKUP` resolves
/// inline.
const NFS_INLINE_BUDGET: Duration = Duration::from_millis(75);

/// One inode's protocol-local state. This is the NFS analogue of the FUSE node
/// entry: the stable identity a `(generation, id)` filehandle rehydrates from,
/// plus what the node projects. It caches no provider bytes; the namespace owns
/// all projection state.
///
/// `scope`, `parent`, `name`, and `kind` are exactly the persistable identity: a
/// reloaded or reattach-cleared id re-resolves from that chain without a
/// [`NodeId`], which is meaningless across a process or a daemon instance. The
/// `body` is the only field a [`NodeId`] lives in, so clearing it turns a live
/// inode back into a [`ColdEntry`].
#[derive(Clone)]
pub(crate) struct Inode {
    /// Which export root this inode hangs under (`ROOT_ID` or `EXPORT_ROOT_ID`).
    /// The same node under the two roots gets two distinct inodes.
    pub(crate) scope: u64,
    pub(crate) parent: u64,
    /// The name under `parent` that resolves this inode. Empty for the two export
    /// roots, which anchor the chain. This is what re-resolution looks up.
    pub(crate) name: String,
    pub(crate) kind: NodeKind,
    body: Body,
}

/// A persisted or reattach-cleared inode whose [`NodeId`] is not yet known. It
/// carries the same identity chain an [`Inode`] does, minus the resolved body:
/// the first op that touches its id re-resolves it lazily (see
/// [`Export::ensure_live`]).
#[derive(Clone)]
pub(crate) struct ColdEntry {
    pub(crate) scope: u64,
    pub(crate) parent: u64,
    pub(crate) name: String,
    pub(crate) kind: NodeKind,
}

/// What an inode projects.
#[derive(Clone)]
enum Body {
    /// A namespace node: resolution, attrs, listing, and reads go through the
    /// [`Namespace`] via this handle.
    Node(NodeId),
    /// A resolved treeref subtree root: it is a namespace node, but its directory
    /// is served locally from `root` (its children have no namespace identity).
    Subtree { node: NodeId, root: PathBuf },
    /// A pure filesystem child under a subtree, served entirely from `path`.
    Backing(PathBuf),
}

impl Body {
    /// The backing directory/file this inode serves from the local filesystem,
    /// for a subtree root or a backing child.
    fn backing(&self) -> Option<&PathBuf> {
        match self {
            Self::Subtree { root, .. } => Some(root),
            Self::Backing(path) => Some(path),
            Self::Node(_) => None,
        }
    }

    /// The namespace handle this inode resolves through, absent for a pure
    /// backing child.
    fn node(&self) -> Option<NodeId> {
        match self {
            Self::Node(node) | Self::Subtree { node, .. } => Some(*node),
            Self::Backing(_) => None,
        }
    }
}

pub struct Export {
    rt: Handle,
    /// The projection surface. Every name resolution, attribute, listing, and
    /// read goes through it; the adapter holds nothing else of the engine.
    namespace: Arc<dyn Namespace>,
    /// Invalidation and live-growth events, drained inline after each namespace
    /// op so the frontend applies them with drain-before-answer ordering.
    events: Mutex<EventStream>,
    /// Proactive deferral for provider-backed `READDIR` ([`delayed::Listings`]).
    delayed_lists: Listings,
    /// NFS inode id -> protocol state. `Arc` so the filehandle persister thread
    /// snapshots the same table the adapter mutates.
    inodes: Arc<DashMap<u64, Inode>>,
    /// (scope, namespace node) -> inode, so a re-resolved node keeps its inode.
    by_node: DashMap<(u64, NodeId), u64>,
    /// (scope, backing path) -> inode, for subtree-local children.
    by_backing: DashMap<(u64, PathBuf), u64>,
    /// Persisted or reattach-cleared inode ids whose `NodeId` is not yet resolved,
    /// keyed by id. `ensure_live` walks these back to a live [`Inode`]. `Arc` so
    /// the persister snapshots it alongside `inodes`.
    cold: Arc<DashMap<u64, ColdEntry>>,
    /// Allocation cursor for fresh inode ids. `Arc` so a restart resumes it and
    /// the persister records it.
    next_ino: Arc<AtomicU64>,
    /// Open stateid bookkeeping. The body is `()`: an open re-resolves its target
    /// from its inode on each read, so a reattach that clears `NodeId`s never has
    /// to touch the open table.
    opens: OpenTable<()>,
    /// Per-node live-follow size learned from an `AttrsChanged` event. `attr`
    /// reports `max(namespace size, grown[node])`, so a polling `tail -f` over
    /// the `noac` mount re-stats, sees growth, and reads the new bytes.
    grown_sizes: DashMap<NodeId, u64>,
    /// The filehandle-table persister, present only on the restartable
    /// out-of-process runner path. `None` for the in-process daemon frontend,
    /// whose mount dies with the process, so persistence would be pointless.
    persist: Option<Persister>,
}

impl Export {
    /// Build an export over `namespace` with no filehandle persistence: the
    /// in-process daemon frontend and unit tests, whose mount dies with the
    /// process.
    pub fn new(rt: Handle, namespace: Arc<dyn Namespace>) -> Self {
        Self::build(rt, namespace, None)
    }

    /// Build an export whose filehandle table is persisted (and reloaded from
    /// `init`) so a restart of this process decodes the handles a kernel client
    /// still holds. The restartable out-of-process runner path only.
    pub(crate) fn with_persistence(
        rt: Handle,
        namespace: Arc<dyn Namespace>,
        init: PersistInit,
    ) -> Self {
        Self::build(rt, namespace, Some(init))
    }

    fn build(rt: Handle, namespace: Arc<dyn Namespace>, persist: Option<PersistInit>) -> Self {
        assert!(
            !matches!(rt.runtime_flavor(), RuntimeFlavor::CurrentThread),
            "NFS adapter requires a multi-thread Tokio runtime because sync NFS workers call Handle::block_on"
        );
        let events = Mutex::new(namespace.subscribe());
        let delayed_lists = Listings::new(rt.clone());
        let inodes = Arc::new(DashMap::new());
        let by_node = DashMap::new();
        // The two export roots both project the namespace root, under distinct
        // scopes, so `/x` and `/omnifs/x` mint distinct scope-stable inodes.
        for scope in [ROOT_ID, EXPORT_ROOT_ID] {
            inodes.insert(
                scope,
                Inode {
                    scope,
                    parent: ROOT_ID,
                    name: String::new(),
                    kind: NodeKind::Directory,
                    body: Body::Node(NodeId::ROOT),
                },
            );
            by_node.insert((scope, NodeId::ROOT), scope);
        }

        // Resume the allocation cursor and cold-entry table from a reloaded state
        // file, so old filehandles decode to the same ids they were minted under.
        let next_base = persist.as_ref().map_or(EXPORT_ROOT_ID + 1, |init| {
            init.next_ino.max(EXPORT_ROOT_ID + 1)
        });
        let next_ino = Arc::new(AtomicU64::new(next_base));
        let cold = Arc::new(DashMap::new());
        if let Some(init) = &persist {
            for entry in &init.entries {
                if entry.id == ROOT_ID || entry.id == EXPORT_ROOT_ID {
                    continue;
                }
                cold.insert(
                    entry.id,
                    ColdEntry {
                        scope: entry.scope,
                        parent: entry.parent,
                        name: entry.name.clone(),
                        kind: entry.kind,
                    },
                );
            }
        }

        let persist = persist.map(|init| {
            Persister::spawn(
                init.state_path,
                PersistTables {
                    generation: init.generation,
                    next_ino: Arc::clone(&next_ino),
                    inodes: Arc::clone(&inodes),
                    cold: Arc::clone(&cold),
                },
            )
        });

        Self {
            rt,
            namespace,
            events,
            delayed_lists,
            inodes,
            by_node,
            by_backing: DashMap::new(),
            cold,
            next_ino,
            opens: OpenTable::new(),
            grown_sizes: DashMap::new(),
            persist,
        }
    }

    /// The runtime handle this export drives its blocking namespace ops on. The
    /// runner uses it to spawn the reattach listener on the same runtime.
    pub(crate) fn handle(&self) -> &Handle {
        &self.rt
    }

    fn alloc_ino(&self) -> u64 {
        let id = self.next_ino.fetch_add(1, Ordering::Relaxed);
        // The cursor advanced; a restart must resume past it.
        self.mark_dirty();
        id
    }

    /// Signal the persister (if any) that the filehandle table changed.
    fn mark_dirty(&self) {
        if let Some(persist) = &self.persist {
            persist.mark_dirty();
        }
    }

    /// Drop every cached `NodeId` on a daemon reattach: every live non-root inode
    /// becomes a [`ColdEntry`], so the next op re-resolves it against the restarted
    /// daemon. Ids, scopes, parent/name chains, opens, and leases all survive; the
    /// held filehandles keep decoding and the open table is never touched (an open
    /// re-resolves its target from its inode on read).
    pub(crate) fn on_reattach(&self) {
        let live: Vec<(u64, ColdEntry)> = self
            .inodes
            .iter()
            .filter(|entry| *entry.key() != ROOT_ID && *entry.key() != EXPORT_ROOT_ID)
            .map(|entry| {
                let inode = entry.value();
                (
                    *entry.key(),
                    ColdEntry {
                        scope: inode.scope,
                        parent: inode.parent,
                        name: inode.name.clone(),
                        kind: inode.kind,
                    },
                )
            })
            .collect();
        for (id, cold) in live {
            self.cold.insert(id, cold);
            self.inodes.remove(&id);
        }
        // Every `NodeId` mapping is stale; drop the caches keyed by it, then
        // re-anchor the two export roots (kept live in `inodes`).
        self.by_node.clear();
        self.by_backing.clear();
        self.grown_sizes.clear();
        for scope in [ROOT_ID, EXPORT_ROOT_ID] {
            self.by_node.insert((scope, NodeId::ROOT), scope);
        }
        self.mark_dirty();
    }

    /// Ensure `id` names a live inode, re-resolving it from its cold identity
    /// chain if a restart or a reattach left only `{ scope, parent, name, kind }`.
    /// The walk anchors at a root scope (`ROOT_ID`/`EXPORT_ROOT_ID` project
    /// [`NodeId::ROOT`]) and descends, looking each name up through the namespace
    /// (or joining a backing path) and caching the fresh `NodeId` down the chain.
    /// A name that no longer resolves returns [`Status::Stale`] for that handle
    /// only; a genuinely-gone path is allowed to be stale.
    fn ensure_live(&self, id: u64) -> StatusResult<()> {
        if self.inodes.contains_key(&id) {
            return Ok(());
        }
        // The roots are always seeded; a missing root means a cleared table mid
        // re-anchor, which the caller retries.
        if id == ROOT_ID || id == EXPORT_ROOT_ID {
            return Ok(());
        }
        let cold = self
            .cold
            .get(&id)
            .map(|entry| entry.clone())
            .ok_or(Status::Stale)?;
        // Resolve the parent first, then this name under it.
        self.ensure_live(cold.parent)?;
        let parent = self.inodes.get(&cold.parent).ok_or(Status::Stale)?;
        let parent_scope = cold.scope;
        let parent_body = parent.body.clone();
        drop(parent);

        if let Some(backing_dir) = parent_body.backing() {
            let child = backing_dir.join(&cold.name);
            let metadata = std::fs::symlink_metadata(&child).map_err(|_| Status::Stale)?;
            let kind = NodeKind::try_from(&metadata)?;
            self.intern_backing(Some(id), parent_scope, cold.parent, &cold.name, child, kind);
        } else {
            let parent_node = parent_body.node().ok_or(Status::Stale)?;
            match self
                .rt
                .block_on(self.namespace.lookup(parent_node, &cold.name))
            {
                Ok(answer) => {
                    self.bind_answer(
                        Some(id),
                        parent_scope,
                        cold.parent,
                        &cold.name,
                        answer.node,
                        &answer.kind,
                    );
                },
                Err(NsError::NotFound) => return Err(Status::Stale),
                Err(error) => {
                    tracing::warn!(op = "re-resolve", name = %cold.name, error = %error, "NFS filehandle re-resolution failed");
                    return Err(Status::from(&error));
                },
            }
        }
        self.cold.remove(&id);
        Ok(())
    }

    // --- events --------------------------------------------------------------

    /// Drain the buffered namespace events emitted since the last drain and apply
    /// them: prune the inodes and close the opens for an invalidated node, and
    /// record a live-follow grown size. Called inline after every namespace op so
    /// a caller sees its own invalidation before it re-reads its inode.
    fn apply_pending_events(&self) {
        let mut events = self.events.lock().expect("events lock");
        while let Some(event) = events.try_recv() {
            match event {
                NsEvent::InvalidateSubtree { node, .. } => self.prune_node(node),
                NsEvent::AttrsChanged { node, attrs, .. } => {
                    // Live growth is monotonic; never let a stale event shrink it.
                    let mut entry = self.grown_sizes.entry(node).or_insert(0);
                    *entry = (*entry).max(attrs.size);
                },
            }
        }
    }

    /// Drop every inode mapping to `node` (across both export roots) and close
    /// the opens bound to them, preserving the two export roots themselves so a
    /// client's root filehandle never goes stale.
    fn prune_node(&self, node: NodeId) {
        for scope in [ROOT_ID, EXPORT_ROOT_ID] {
            let Some((_, ino)) = self.by_node.remove(&(scope, node)) else {
                continue;
            };
            if ino == ROOT_ID || ino == EXPORT_ROOT_ID {
                self.by_node.insert((scope, node), ino);
                continue;
            }
            self.inodes.remove(&ino);
            // The removed open bodies hold no provider resource; dropping suffices.
            let _ = self.opens.remove_inodes(&[ino]);
        }
        self.grown_sizes.remove(&node);
    }

    // --- identity ------------------------------------------------------------

    /// Allocate (or reuse) the inode for a resolved namespace node under `scope`,
    /// preserving a resolved subtree backing over a later plain provider
    /// resolution of the same node. `forced` binds to a specific id when
    /// re-resolving a persisted or reattach-cleared handle; `None` allocates or
    /// reuses the `by_node` mapping. `name` is the name under `parent` that
    /// resolves this node, recorded so the id survives a restart.
    // The arguments are the inode identity plus its resolved body; grouping them
    // into a struct would only shuffle the same fields behind one more type.
    #[allow(clippy::too_many_arguments)]
    fn intern_node(
        &self,
        forced: Option<u64>,
        scope: u64,
        parent: u64,
        name: &str,
        node: NodeId,
        kind: NodeKind,
        subtree_root: Option<PathBuf>,
    ) -> u64 {
        let ino = match forced {
            Some(id) => {
                self.by_node.insert((scope, node), id);
                id
            },
            None => *self
                .by_node
                .entry((scope, node))
                .or_insert_with(|| self.alloc_ino()),
        };
        // Never rewrite an export root's identity.
        if ino == ROOT_ID || ino == EXPORT_ROOT_ID {
            return ino;
        }
        let existing = self.inodes.get(&ino).map(|entry| entry.body.clone());
        let body = match (subtree_root, existing) {
            (Some(root), _) => Body::Subtree { node, root },
            // A listing re-binds a treeref child as a plain provider directory;
            // keep the subtree backing a prior lookup resolved.
            (None, Some(Body::Subtree { node: kept, root })) => Body::Subtree { node: kept, root },
            (None, _) => Body::Node(node),
        };
        self.inodes.insert(
            ino,
            Inode {
                scope,
                parent,
                name: name.to_string(),
                kind,
                body,
            },
        );
        self.mark_dirty();
        ino
    }

    /// Allocate (or reuse) the inode for a subtree-local backing path under
    /// `scope`. `forced` binds a re-resolved persisted handle to its id.
    fn intern_backing(
        &self,
        forced: Option<u64>,
        scope: u64,
        parent: u64,
        name: &str,
        path: PathBuf,
        kind: NodeKind,
    ) -> u64 {
        let ino = match forced {
            Some(id) => {
                self.by_backing.insert((scope, path.clone()), id);
                id
            },
            None => *self
                .by_backing
                .entry((scope, path.clone()))
                .or_insert_with(|| self.alloc_ino()),
        };
        self.inodes.insert(
            ino,
            Inode {
                scope,
                parent,
                name: name.to_string(),
                kind,
                body: Body::Backing(path),
            },
        );
        self.mark_dirty();
        ino
    }

    /// Bind a resolved [`NodeAnswer`]-shaped result to an inode, recording a
    /// subtree backing when the namespace reports the node is a resolved treeref.
    fn bind_answer(
        &self,
        forced: Option<u64>,
        scope: u64,
        parent: u64,
        name: &str,
        node: NodeId,
        kind: &EntryKind,
    ) -> u64 {
        let subtree_root = match kind {
            EntryKind::Subtree { root } => Some(root.clone()),
            _ => None,
        };
        self.intern_node(
            forced,
            scope,
            parent,
            name,
            node,
            NodeKind::from(kind),
            subtree_root,
        )
    }

    /// Promote a discovered subtree: a node the listing bound as a plain provider
    /// directory that resolves to a treeref backing dir now serves locally.
    fn rebind_subtree(&self, id: u64, node: NodeId, root: PathBuf) {
        if let Some(mut entry) = self.inodes.get_mut(&id)
            && !matches!(entry.body, Body::Subtree { .. })
        {
            entry.body = Body::Subtree { node, root };
            entry.kind = NodeKind::Directory;
        }
    }

    // --- reads ---------------------------------------------------------------

    /// Read a whole namespace file by paging through the namespace until EOF.
    fn read_node_all(&self, node: NodeId) -> StatusResult<Vec<u8>> {
        let mut data = Vec::new();
        let mut offset = 0_u64;
        loop {
            let answer = self
                .rt
                .block_on(self.namespace.read(node, offset, MAX_NFS_READ_BYTES))
                .map_err(|error| {
                    tracing::warn!(op = "read", error = %error, "NFS namespace read failed");
                    Status::from(&error)
                })?;
            let read = answer.bytes.len();
            data.extend(answer.bytes);
            if answer.eof || read == 0 {
                break;
            }
            offset = offset.saturating_add(read as u64);
        }
        Ok(data)
    }

    /// Serve one `READ` chunk of a namespace file. NFS clamps the request to the
    /// max read size; the namespace enforces the no-oversize-chunk invariant and
    /// learns an exact size on an EOF-short read (a later `attr` reflects it).
    fn read_node_chunk(
        &self,
        id: u64,
        node: NodeId,
        offset: u64,
        count: u32,
    ) -> StatusResult<OpenRead> {
        let count = count.min(MAX_NFS_READ_BYTES);
        match self.rt.block_on(self.namespace.read(node, offset, count)) {
            Ok(answer) => Ok(OpenRead {
                id,
                data: answer.bytes,
                eof: answer.eof,
            }),
            Err(error) => {
                tracing::warn!(op = "read", error = %error, "NFS namespace ranged read failed");
                Err(Status::from(&error))
            },
        }
    }

    fn read_backing_state(
        id: u64,
        backing_path: &Path,
        offset: u64,
        count: u32,
    ) -> StatusResult<OpenRead> {
        let metadata = std::fs::symlink_metadata(backing_path).map_err(|_| Status::Stale)?;
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
        let mut file = std::fs::File::open(backing_path).map_err(|_| Status::Io)?;
        file.seek(SeekFrom::Start(offset)).map_err(|_| Status::Io)?;
        let mut data = vec![0; count];
        let read = file.read(&mut data).map_err(|_| Status::Io)?;
        data.truncate(read);
        let read_end = offset
            .checked_add(u64::try_from(read).map_err(|_| Status::Io)?)
            .ok_or(Status::Io)?;
        Ok(OpenRead {
            id,
            data,
            eof: read_end >= metadata.len(),
        })
    }

    // --- listing -------------------------------------------------------------

    /// The truth computation for one directory: drain every namespace page into a
    /// finite snapshot, as a `'static` future factory the deferral table spawns.
    /// Errors are logged and mapped to `Status` here so the table stays
    /// protocol-agnostic.
    fn list_op(
        &self,
        node: NodeId,
    ) -> impl FnOnce() -> Pin<Box<dyn Future<Output = crate::delayed::ListResult> + Send>> + use<>
    {
        let namespace = Arc::clone(&self.namespace);
        move || {
            Box::pin(async move {
                let mut entries = Vec::new();
                let mut cursor = DirCursor::start();
                loop {
                    let page = namespace.readdir(node, cursor, 0).await.map_err(|error| {
                        tracing::warn!(op = "readdir", error = %error, "NFS namespace readdir failed");
                        Status::from(&error)
                    })?;
                    entries.extend(page.entries);
                    match page.next {
                        Some(next) => cursor = next,
                        None => break,
                    }
                }
                Ok(entries)
            })
        }
    }

    /// Build a finite directory snapshot from a drained namespace listing,
    /// binding each child and eagerly probing a file child's exact size for the
    /// `fattr4` the flatten renderer needs.
    fn snapshot(&self, scope: u64, parent: u64, entries: &[NsDirEntry]) -> DirListing {
        let mut out = Vec::with_capacity(entries.len());
        for entry in entries {
            let kind = NodeKind::from(&entry.kind);
            // A listing never marks a treeref child; it binds as a plain provider
            // node and is promoted on the lookup that descends into it.
            let id = self.intern_node(None, scope, parent, &entry.name, entry.node, kind, None);
            let mut attr = if kind == NodeKind::File {
                match self.rt.block_on(self.namespace.getattr_exact(entry.node)) {
                    Ok(attrs) => attr_from_ns(id, parent, &attrs),
                    // A child that vanished between listing and probe keeps its
                    // listing attrs rather than dropping out of the snapshot.
                    Err(_) => attr_from_ns(id, parent, &entry.attrs),
                }
            } else {
                attr_from_ns(id, parent, &entry.attrs)
            };
            if let Some(grown) = self.grown_sizes.get(&entry.node) {
                attr.size = attr.size.max(*grown);
            }
            out.push(DirEntry {
                id,
                name: entry.name.clone(),
                attr,
            });
        }
        // NFS presents the finite known snapshot; there is no way to advertise
        // lookup-only dynamic children, so the snapshot reports EOF.
        DirListing {
            entries: out,
            exhaustive: true,
        }
    }

    fn readdir_backing(&self, scope: u64, parent: u64, root: &Path) -> StatusResult<DirListing> {
        let mut entries = Vec::new();
        for entry in std::fs::read_dir(root).map_err(|_| Status::Io)? {
            let entry = entry.map_err(|_| Status::Io)?;
            let name = entry.file_name();
            let Some(name) = name.to_str() else {
                continue;
            };
            let backing_path = entry.path();
            let metadata = std::fs::symlink_metadata(&backing_path).map_err(|_| Status::Io)?;
            let Ok(kind) = NodeKind::try_from(&metadata) else {
                continue;
            };
            let id = self.intern_backing(None, scope, parent, name, backing_path, kind);
            entries.push(DirEntry {
                id,
                name: name.to_string(),
                attr: attr_from_metadata(id, parent, &metadata)?,
            });
        }
        Ok(DirListing {
            entries,
            exhaustive: true,
        })
    }

    fn lookup_backing_child(
        &self,
        scope: u64,
        parent: u64,
        dir: &Path,
        name: &str,
    ) -> StatusResult<u64> {
        let child = dir.join(name);
        let metadata = std::fs::symlink_metadata(&child).map_err(|_| Status::NoEnt)?;
        let kind = NodeKind::try_from(&metadata)?;
        Ok(self.intern_backing(None, scope, parent, name, child, kind))
    }
}

impl ReadOnlyExport for Export {
    fn root(&self) -> u64 {
        ROOT_ID
    }

    fn attr(&self, id: u64) -> StatusResult<Attr> {
        self.ensure_live(id)?;
        let inode = self.inodes.get(&id).ok_or(Status::Stale)?;
        let parent = inode.parent;
        let body = inode.body.clone();
        drop(inode);

        if let Some(backing) = body.backing() {
            self.apply_pending_events();
            self.inodes.get(&id).ok_or(Status::Stale)?;
            let metadata = std::fs::symlink_metadata(backing).map_err(|_| Status::Stale)?;
            return attr_from_metadata(id, parent, &metadata);
        }

        let node = body.node().ok_or(Status::Stale)?;
        let attrs = match self.rt.block_on(self.namespace.getattr(node)) {
            Ok(attrs) => attrs,
            // A vanished node is a stale filehandle, not a plain lookup miss.
            Err(NsError::NotFound) => {
                self.apply_pending_events();
                return Err(Status::Stale);
            },
            Err(error) => {
                self.apply_pending_events();
                return Err(Status::from(&error));
            },
        };
        self.apply_pending_events();
        // An invalidation the getattr emitted may have pruned this inode.
        if self.inodes.get(&id).is_none() {
            return Err(Status::Stale);
        }

        // A node that resolves to a treeref backing dir now serves locally.
        if let EntryKind::Subtree { root } = &attrs.kind {
            self.rebind_subtree(id, node, root.clone());
            let metadata = std::fs::symlink_metadata(root).map_err(|_| Status::Stale)?;
            return attr_from_metadata(id, parent, &metadata);
        }

        let mut attr = attr_from_ns(id, parent, &attrs);
        if let Some(grown) = self.grown_sizes.get(&node) {
            attr.size = attr.size.max(*grown);
        }
        Ok(attr)
    }

    fn lookup(&self, parent: u64, name: &str) -> StatusResult<u64> {
        let name = Segment::try_from(name).map_err(|_| Status::Invalid)?;
        self.apply_pending_events();
        self.ensure_live(parent)?;

        let inode = self.inodes.get(&parent).ok_or(Status::Stale)?;
        if inode.kind != NodeKind::Directory {
            return Err(Status::NotDir);
        }
        let scope = inode.scope;
        let body = inode.body.clone();
        drop(inode);

        if let Some(backing) = body.backing() {
            return self.lookup_backing_child(scope, parent, backing, name.as_str());
        }

        let parent_node = body.node().ok_or(Status::Stale)?;
        match self
            .rt
            .block_on(self.namespace.lookup(parent_node, name.as_str()))
        {
            Ok(answer) => {
                let id = self.bind_answer(
                    None,
                    scope,
                    parent,
                    name.as_str(),
                    answer.node,
                    &answer.kind,
                );
                // Eagerly probe a file child's exact size so a bare `stat`/`ls -l`
                // after the lookup reflects a ranged file's real size; the
                // namespace caches the learned size for the later `getattr`.
                if matches!(NodeKind::from(&answer.kind), NodeKind::File) {
                    let _ = self.rt.block_on(self.namespace.getattr_exact(answer.node));
                }
                self.apply_pending_events();
                Ok(id)
            },
            // A cold provider lookup that misses at the protocol root resolves the
            // `/omnifs` export alias, mirroring how the client mounts `:/omnifs`.
            Err(NsError::NotFound) if parent == ROOT_ID && name.as_str() == NFS_EXPORT_NAME => {
                Ok(EXPORT_ROOT_ID)
            },
            Err(NsError::NotFound) => Err(Status::NoEnt),
            Err(error) => {
                tracing::warn!(op = "lookup", name = %name, error = %error, "NFS namespace lookup failed");
                Err(Status::from(&error))
            },
        }
    }

    fn readdir(&self, id: u64) -> StatusResult<DirListing> {
        self.apply_pending_events();
        self.ensure_live(id)?;
        let inode = self.inodes.get(&id).ok_or(Status::Stale)?;
        if inode.kind != NodeKind::Directory {
            return Err(Status::NotDir);
        }
        let scope = inode.scope;
        let body = inode.body.clone();
        drop(inode);

        if let Some(backing) = body.backing() {
            let backing = backing.clone();
            return self.readdir_backing(scope, id, &backing);
        }

        let node = body.node().ok_or(Status::Stale)?;
        // Proactive deferral only. On persistent failure the namespace does not
        // cache the error, so each retry may re-defer until the listing succeeds
        // or maps to a terminal `Status`.
        let outcome =
            self.delayed_lists
                .resolve(&Key::new(node), NFS_INLINE_BUDGET, self.list_op(node));
        match outcome {
            DeferOutcome::Ready(result) => {
                self.apply_pending_events();
                match result.as_ref() {
                    Ok(entries) => Ok(self.snapshot(scope, id, entries)),
                    Err(status) => Err(*status),
                }
            },
            DeferOutcome::Pending => Err(Status::Delay),
        }
    }

    fn read(&self, id: u64) -> StatusResult<Vec<u8>> {
        self.apply_pending_events();
        self.ensure_live(id)?;
        let inode = self.inodes.get(&id).ok_or(Status::Stale)?;
        if inode.kind == NodeKind::Directory {
            return Err(Status::IsDir);
        }
        if inode.kind == NodeKind::Symlink {
            return Err(Status::Invalid);
        }
        let body = inode.body.clone();
        drop(inode);

        match body {
            Body::Backing(path) => {
                let metadata = std::fs::symlink_metadata(&path).map_err(|_| Status::Stale)?;
                if metadata.file_type().is_symlink() {
                    return Err(Status::Symlink);
                }
                if metadata.is_dir() {
                    return Err(Status::IsDir);
                }
                if !metadata.is_file() {
                    return Err(Status::Invalid);
                }
                std::fs::read(path).map_err(|_| Status::Io)
            },
            // A subtree root is a directory.
            Body::Subtree { .. } => Err(Status::IsDir),
            Body::Node(node) => self.read_node_all(node),
        }
    }

    fn readlink(&self, id: u64) -> StatusResult<Vec<u8>> {
        self.ensure_live(id)?;
        let inode = self.inodes.get(&id).ok_or(Status::Stale)?;
        if inode.kind != NodeKind::Symlink {
            return Err(Status::Invalid);
        }
        let Some(path) = inode.body.backing().cloned() else {
            return Err(Status::Invalid);
        };
        drop(inode);
        self.apply_pending_events();
        if self.inodes.get(&id).is_none() {
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
        // The protocol validated `attr.kind != Directory/Symlink` before OPEN, so
        // this is a file. `attr` both drains pending events and rebinds a
        // subtree, so re-read the body afterwards.
        let mut attr = self.attr(id)?;
        let inode = self.inodes.get(&id).ok_or(Status::Stale)?;
        let body = inode.body.clone();
        drop(inode);

        match body {
            Body::Backing(_) => {},
            Body::Node(node) => {
                // Learn the exact size before the OPEN reply. Seek-from-end
                // readers (`tail -n`) trust the size their post-open stat
                // reports, so a cold unknown-length file must not answer the
                // 1-byte sentinel past OPEN. One 1-byte read makes the
                // namespace learn and cache the exact size (the old adapter
                // achieved this by materializing the whole file at open);
                // subsequent READs stay read-through.
                if attr.size <= 1 {
                    self.read_node_chunk(id, node, 0, 1)?;
                    attr = self.attr(id)?;
                }
            },
            Body::Subtree { .. } => return Err(Status::IsDir),
        }
        // The open records only the inode id; each read re-resolves the read
        // target from the inode, so a reattach that clears `NodeId`s never has to
        // rewrite the open table.
        let stateid = self.opens.open(OpenSeed {
            generation,
            inode: id,
            clientid,
            access,
            body: (),
        });
        Ok(OpenResult { stateid, attr })
    }

    fn validate_state(&self, stateid: StateId) -> StatusResult<()> {
        match self.opens.touch(stateid) {
            Ok(()) => Ok(()),
            Err(Status::Expired) => {
                let _ = self.opens.remove_body(stateid);
                Err(Status::Expired)
            },
            Err(status) => Err(status),
        }
    }

    fn read_state(&self, stateid: StateId, offset: u64, count: u32) -> StatusResult<OpenRead> {
        // Drain first: an invalidation may have closed this open.
        self.apply_pending_events();
        // Validate access and lease under the open-table lock, then release it and
        // do the read (which may re-resolve the inode) without holding the lock.
        let inode_id = match self.opens.with_state(stateid, |state| {
            ensure_read_access(state.access).map(|()| state.inode)
        }) {
            Ok(result) => result?,
            Err(Status::Expired) => {
                let _ = self.opens.remove_body(stateid);
                return Err(Status::Expired);
            },
            Err(status) => return Err(status),
        };
        // Re-resolve the read target from the inode: a reattach may have cleared
        // its `NodeId`, and a subtree may have rebound to a backing path.
        self.ensure_live(inode_id)?;
        let body = self
            .inodes
            .get(&inode_id)
            .ok_or(Status::Stale)?
            .body
            .clone();
        match body {
            Body::Node(node) => self.read_node_chunk(inode_id, node, offset, count),
            Body::Backing(path) => Self::read_backing_state(inode_id, &path, offset, count),
            Body::Subtree { .. } => Err(Status::IsDir),
        }
    }

    fn close_state(&self, stateid: StateId) -> StatusResult<StateId> {
        match self.opens.close(stateid) {
            Ok((next_stateid, _body)) => Ok(next_stateid),
            Err(Status::Expired) => {
                let _ = self.opens.remove_body(stateid);
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

// -----------------------------------------------------------------------------
// Free helpers
// -----------------------------------------------------------------------------

impl From<&EntryKind> for NodeKind {
    fn from(kind: &EntryKind) -> Self {
        match kind {
            EntryKind::Directory | EntryKind::Subtree { .. } => Self::Directory,
            EntryKind::File => Self::File,
            EntryKind::Symlink => Self::Symlink,
        }
    }
}

impl TryFrom<&std::fs::Metadata> for NodeKind {
    type Error = Status;

    fn try_from(metadata: &std::fs::Metadata) -> StatusResult<Self> {
        let file_type = metadata.file_type();
        if file_type.is_dir() {
            Ok(Self::Directory)
        } else if file_type.is_symlink() {
            Ok(Self::Symlink)
        } else if file_type.is_file() {
            Ok(Self::File)
        } else {
            Err(Status::Invalid)
        }
    }
}

fn attr_from_ns(id: u64, parent: u64, attrs: &Attrs) -> Attr {
    let kind = NodeKind::from(&attrs.kind);
    Attr {
        id,
        parent,
        kind,
        size: attrs.size,
        mode: kind.mode(),
        change: attrs.change,
        mtime_sec: 0,
    }
}

/// Build a `fattr4`-shaped answer from local filesystem metadata. The read-only
/// mode follows the node kind (`0o555`/`0o444`/`0o777`), matching the byte
/// boundary's backing-metadata mapping.
fn attr_from_metadata(id: u64, parent: u64, metadata: &std::fs::Metadata) -> StatusResult<Attr> {
    let kind = NodeKind::try_from(metadata)?;
    let mtime_sec = metadata
        .modified()
        .ok()
        .and_then(|modified| modified.duration_since(UNIX_EPOCH).ok())
        .map_or(0, |duration| {
            i64::try_from(duration.as_secs()).unwrap_or(i64::MAX)
        });
    Ok(Attr {
        id,
        parent,
        kind,
        size: metadata.len(),
        mode: kind.mode(),
        change: metadata_change(id, metadata),
        mtime_sec,
    })
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

#[cfg(test)]
mod tests {
    use super::*;
    use omnifs_engine::{GitCloner, HostContext, MountRuntimes, TreeNamespace};
    use tempfile::TempDir;
    use tokio::runtime::Runtime as TokioRuntime;

    /// Old open-materialize limit, inlined so the test never imports an engine
    /// byte-boundary constant (the structural boundary the port enforces).
    const OVERSIZED_BACKING_BYTES: u64 = 64 * 1024 * 1024;

    struct TestExport {
        export: Export,
        _runtime: TokioRuntime,
        _cache_dir: TempDir,
        _config_dir: TempDir,
        _providers_dir: TempDir,
    }

    /// Build an `Export` over an empty registry: there are no mounts, so these
    /// tests drive only the backing (treeref subtree) path, which never touches
    /// the namespace's provider surface.
    fn empty_export() -> TestExport {
        let cache_dir = tempfile::tempdir().expect("cache dir");
        let config_dir = tempfile::tempdir().expect("config dir");
        let providers_dir = tempfile::tempdir().expect("providers dir");
        let credentials_file = config_dir.path().join("credentials.json");
        let cloner = Arc::new(GitCloner::new(cache_dir.path().join("clones")));
        let registry = Arc::new(
            MountRuntimes::new(
                HostContext::new(
                    cache_dir.path(),
                    config_dir.path(),
                    providers_dir.path(),
                    &credentials_file,
                ),
                cloner,
            )
            .expect("registry init"),
        );

        let runtime = TokioRuntime::new().expect("tokio runtime");
        let namespace = TreeNamespace::new(registry, runtime.handle().clone());
        let export = Export::new(runtime.handle().clone(), namespace);
        TestExport {
            export,
            _runtime: runtime,
            _cache_dir: cache_dir,
            _config_dir: config_dir,
            _providers_dir: providers_dir,
        }
    }

    #[test]
    fn open_state_streams_oversized_backing_file() {
        let harness = empty_export();
        let temp = tempfile::tempdir().expect("backing tempdir");
        let backing = temp.path().join("huge.bin");
        let file = std::fs::File::create(&backing).expect("create backing file");
        file.set_len(OVERSIZED_BACKING_BYTES + 1)
            .expect("set backing len");
        drop(file);

        let id = harness.export.intern_backing(
            None,
            ROOT_ID,
            ROOT_ID,
            "huge.bin",
            backing,
            NodeKind::File,
        );

        let opened = harness
            .export
            .open_state(7, id, 1, 1)
            .expect("backing open");
        let chunk = harness
            .export
            .read_state(opened.stateid, OVERSIZED_BACKING_BYTES, 8)
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

        // A treeref node first resolves as a subtree backing dir...
        let subtree_node = NodeId(5000);
        let id = harness.export.intern_node(
            None,
            ROOT_ID,
            ROOT_ID,
            "checkout",
            subtree_node,
            NodeKind::Directory,
            Some(temp.path().to_path_buf()),
        );
        // ...and a later plain provider resolution of the same node keeps it.
        let rebound = harness.export.intern_node(
            None,
            ROOT_ID,
            ROOT_ID,
            "checkout",
            subtree_node,
            NodeKind::Directory,
            None,
        );
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

    // --- re-resolution and stateid recovery ----------------------------------

    use crate::export::StateId;
    use crate::persist::{FhEntry, PersistInit};
    use omnifs_engine::namespace::{
        Attrs, DirPage, NodeAnswer, ReadAnswer, ReadStyle, StabilityClass,
    };
    use std::collections::HashMap;
    use std::sync::atomic::AtomicU64;
    use std::time::Duration;

    /// A minimal in-memory namespace: a fixed `(parent, name) -> (node, kind)`
    /// tree, counting lookups so a test can prove the parent chain was walked.
    struct StubNamespace {
        children: HashMap<(u64, String), (u64, EntryKind)>,
        kinds: HashMap<u64, EntryKind>,
        lookups: AtomicU64,
    }

    fn stub_attrs(kind: EntryKind) -> Attrs {
        let size = if matches!(kind, EntryKind::File) {
            5
        } else {
            0
        };
        Attrs {
            kind,
            size,
            ttl: Duration::ZERO,
            change: 1,
            direct_io: false,
            stability: StabilityClass::Stable,
            read_style: ReadStyle::Whole,
        }
    }

    impl Namespace for StubNamespace {
        fn lookup<'a>(
            &'a self,
            parent: NodeId,
            name: &'a str,
        ) -> Pin<Box<dyn Future<Output = Result<NodeAnswer, NsError>> + Send + 'a>> {
            self.lookups.fetch_add(1, Ordering::Relaxed);
            let answer = self
                .children
                .get(&(parent.0, name.to_string()))
                .map(|(node, kind)| NodeAnswer {
                    node: NodeId(*node),
                    attrs: stub_attrs(kind.clone()),
                    kind: kind.clone(),
                })
                .ok_or(NsError::NotFound);
            Box::pin(async move { answer })
        }

        fn getattr(
            &self,
            node: NodeId,
        ) -> Pin<Box<dyn Future<Output = Result<Attrs, NsError>> + Send + '_>> {
            let answer = self
                .kinds
                .get(&node.0)
                .map(|kind| stub_attrs(kind.clone()))
                .ok_or(NsError::NotFound);
            Box::pin(async move { answer })
        }

        fn getattr_exact(
            &self,
            node: NodeId,
        ) -> Pin<Box<dyn Future<Output = Result<Attrs, NsError>> + Send + '_>> {
            self.getattr(node)
        }

        fn readdir(
            &self,
            _node: NodeId,
            _cursor: DirCursor,
            _budget: usize,
        ) -> Pin<Box<dyn Future<Output = Result<DirPage, NsError>> + Send + '_>> {
            Box::pin(async {
                Ok(DirPage {
                    entries: Vec::new(),
                    next: None,
                })
            })
        }

        fn read(
            &self,
            _node: NodeId,
            offset: u64,
            _len: u32,
        ) -> Pin<Box<dyn Future<Output = Result<ReadAnswer, NsError>> + Send + '_>> {
            Box::pin(async move {
                let all = b"hello".to_vec();
                let start = usize::try_from(offset).unwrap_or(all.len()).min(all.len());
                Ok(ReadAnswer {
                    bytes: all[start..].to_vec(),
                    eof: true,
                    attrs: stub_attrs(EntryKind::File),
                })
            })
        }

        fn readlink(
            &self,
            _node: NodeId,
        ) -> Pin<Box<dyn Future<Output = Result<PathBuf, NsError>> + Send + '_>> {
            Box::pin(async { Err(NsError::NotFound) })
        }

        fn subscribe(&self) -> EventStream {
            EventStream::from_broadcast(tokio::sync::broadcast::channel(4).1)
        }
    }

    /// A `/test` dir with a `message` file, plus a persisted cold table for both.
    fn stub_tree() -> StubNamespace {
        let mut children = HashMap::new();
        children.insert(
            (NodeId::ROOT.0, "test".to_string()),
            (10, EntryKind::Directory),
        );
        children.insert((10, "message".to_string()), (11, EntryKind::File));
        let mut kinds = HashMap::new();
        kinds.insert(10, EntryKind::Directory);
        kinds.insert(11, EntryKind::File);
        StubNamespace {
            children,
            kinds,
            lookups: AtomicU64::new(0),
        }
    }

    fn cold_entries() -> Vec<FhEntry> {
        vec![
            FhEntry {
                id: 100,
                scope: ROOT_ID,
                parent: ROOT_ID,
                name: "test".to_string(),
                kind: NodeKind::Directory,
            },
            FhEntry {
                id: 101,
                scope: ROOT_ID,
                parent: 100,
                name: "message".to_string(),
                kind: NodeKind::File,
            },
        ]
    }

    #[test]
    fn re_resolves_persisted_handle_by_walking_the_parent_chain() {
        let runtime = TokioRuntime::new().expect("tokio runtime");
        let namespace = Arc::new(stub_tree());
        let state_dir = tempfile::tempdir().expect("state dir");
        let export = Export::with_persistence(
            runtime.handle().clone(),
            Arc::clone(&namespace) as Arc<dyn Namespace>,
            PersistInit {
                generation: 0x1234,
                next_ino: 200,
                entries: cold_entries(),
                state_path: state_dir.path().join("filehandles.json"),
            },
        );

        // The child id 101 arrives with no live inode (a decoded old filehandle).
        // `attr` must re-resolve it by walking id 100 -> root, and report the
        // file's stub size.
        let attr = export.attr(101).expect("re-resolve persisted file handle");
        assert_eq!(attr.kind, NodeKind::File);
        assert_eq!(attr.size, 5);
        assert!(
            namespace.lookups.load(Ordering::Relaxed) >= 2,
            "re-resolution must look up each ancestor on the chain"
        );

        // A held-open read against the same id also re-resolves.
        let opened = export
            .open_state(0x1234, 101, 1, 1)
            .expect("open persisted handle");
        let chunk = export
            .read_state(opened.stateid, 0, 8)
            .expect("read persisted handle");
        assert_eq!(chunk.data, b"hello".to_vec());

        // A path that no longer resolves is stale for that handle only.
        let gone = vec![FhEntry {
            id: 102,
            scope: ROOT_ID,
            parent: ROOT_ID,
            name: "missing".to_string(),
            kind: NodeKind::Directory,
        }];
        let export2 = Export::with_persistence(
            runtime.handle().clone(),
            Arc::new(stub_tree()) as Arc<dyn Namespace>,
            PersistInit {
                generation: 0x1234,
                next_ino: 200,
                entries: gone,
                state_path: state_dir.path().join("filehandles2.json"),
            },
        );
        assert!(matches!(export2.attr(102), Err(Status::Stale)));
    }

    #[test]
    fn reattach_clears_node_ids_and_re_resolves() {
        let runtime = TokioRuntime::new().expect("tokio runtime");
        let namespace = Arc::new(stub_tree());
        let export = Export::new(
            runtime.handle().clone(),
            Arc::clone(&namespace) as Arc<dyn Namespace>,
        );

        // Resolve `/test/message` live, then simulate a daemon reattach.
        let test_dir = export.lookup(export.root(), "test").expect("lookup test");
        let message = export.lookup(test_dir, "message").expect("lookup message");
        let before = namespace.lookups.load(Ordering::Relaxed);

        export.on_reattach();

        // The ids survive; a fresh op re-resolves them (more lookups), and the
        // held id still answers.
        let attr = export.attr(message).expect("re-resolve after reattach");
        assert_eq!(attr.kind, NodeKind::File);
        assert!(
            namespace.lookups.load(Ordering::Relaxed) > before,
            "a reattach must force re-resolution through the namespace again"
        );
    }

    #[test]
    fn unknown_stateid_is_bad_stateid() {
        let harness = empty_export();
        let unknown = StateId::new(1, 0xdead_beef, 999);
        assert_eq!(
            harness.export.validate_state(unknown),
            Err(Status::BadStateId)
        );
        assert!(matches!(
            harness.export.read_state(unknown, 0, 8),
            Err(Status::BadStateId)
        ));
    }
}
