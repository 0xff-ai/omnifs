//! NFSv4.0 export adapter over the engine [`Namespace`] surface.
//!
//! `Export` is the NFS renderer. It owns NFS protocol state only: the inode
//! table that backs `(generation, id)` filehandles, the stateid open tables, the
//! `/omnifs` export-root alias, the `NFS4ERR_DELAY` deferral policy, and `fattr4`
//! construction. Every projection answer (name resolution, attributes, directory
//! paging, byte reads) comes from a [`Namespace`]: the adapter never reaches into
//! the projection tree, its caches, or its render/identity machinery.
//!
//! The inode table maps an NFS inode id to a namespace [`Path`], plus the protocol-local
//! parent/scope/kind. Two export roots (`ROOT_ID` and the `/omnifs`
//! `EXPORT_ROOT_ID` alias) both project [`Path::root()`]; the same path reached
//! under the two roots gets two distinct inodes so filehandles stay
//! scope-stable.
//!
//! Invalidation and live growth arrive as [`NsEvent`]s on a subscription the
//! adapter drains inline after every namespace op (see
//! [`EventStream::try_recv`]), so a stat that observes its own invalidation
//! prunes and closes stale opens before it re-reads its inode, and a polling
//! `tail -f` picks up an `AttrsChanged` grown size on its next re-stat.

use crate::cache::ReplyCache;
use crate::delayed::{PendingListings, PendingOutcome};
use crate::export::{
    Attr, DirEntry, DirListing, NodeKind, OpenRead, OpenResult, OpenSeed, OpenTable,
    ReadOnlyExport, StateId, Status, StatusResult, ensure_read_access,
};
use crate::persist::{PersistInit, PersistTables, Persister};
use crate::protocol::client::ClientTable;
use crate::protocol::consts::{
    EXPORT_ROOT_ID, MAX_NFS_READ_BYTES, NFS_EXPORT_NAME, ROOT_ID, SPOTLIGHT_MARKER_ID,
    SPOTLIGHT_MARKER_NAME, is_reserved_inode,
};
use dashmap::DashMap;
use omnifs_core::path::{Path, Segment};
use omnifs_engine::namespace::{
    Attrs, DirCursor, DirEntry as NsDirEntry, EntryKind, EventStream, LookupAnswer, LookupState,
    Namespace, NsError, NsEvent,
};
use std::future::Future;
use std::pin::Pin;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;
use tokio::runtime::{Handle, RuntimeFlavor};

/// Inline wait budget for proactive `READDIR` deferral.
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
/// `scope`, `parent`, `name`, and `kind` are protocol-local identity, while the
/// validated namespace `Path` is the persisted projection identity.
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
    pub(crate) body: Body,
}

/// What an inode projects.
#[derive(Clone)]
pub(crate) enum Body {
    /// A namespace node: resolution, attrs, listing, and reads go through the
    /// [`Namespace`] via this handle.
    Node(Path),
    /// A frontend-owned hidden marker, served without entering the namespace.
    Synthetic(&'static [u8]),
}

impl Body {
    fn node(&self) -> Option<Path> {
        match self {
            Self::Node(node) => Some(node.clone()),
            Self::Synthetic(_) => None,
        }
    }
}

pub struct Export {
    rt: Handle,
    /// Filehandle and client identity generation. It is pinned by persisted
    /// state and otherwise minted once for this export instance.
    generation: u64,
    /// NFS client identity and confirmation state. Server workers never own or
    /// pass this table; all protocol state transitions go through `Export`.
    clients: ClientTable,
    /// The projection surface. Every name resolution, attribute, listing, and
    /// read goes through it; the adapter holds nothing else of the engine.
    namespace: Arc<dyn Namespace>,
    /// Bounded protocol reply cache for the NFS mount, whose kernel attribute
    /// and negative-name caches are deliberately disabled.
    replies: ReplyCache,
    /// Invalidation and live-growth events, drained inline after each namespace
    /// op so the frontend applies them with drain-before-answer ordering.
    events: Mutex<EventStream>,
    /// Proactive deferral for provider-backed `READDIR`.
    delayed_lists: PendingListings,
    /// NFS inode id -> protocol state. `Arc` so the filehandle persister thread
    /// snapshots the same table the adapter mutates.
    inodes: Arc<DashMap<u64, Inode>>,
    /// (scope, namespace path) -> inode, so a stable path keeps its inode.
    by_node: DashMap<(u64, Path), u64>,
    /// Allocation cursor for fresh inode ids. `Arc` so a restart resumes it and
    /// the persister records it.
    next_ino: Arc<AtomicU64>,
    /// Open stateid bookkeeping. The body is `()`: an open re-resolves its target
    /// from its inode on each read, so daemon replacement never has to touch
    /// the open table.
    opens: OpenTable<()>,
    /// Per-node live-follow size learned from an `AttrsChanged` event. `attr`
    /// reports `max(namespace size, grown[node])`, so a polling `tail -f` over
    /// the `noac` mount re-stats, sees growth, and reads the new bytes.
    grown_sizes: DashMap<Path, u64>,
    /// The filehandle-table persister, present only on the restartable
    /// out-of-process NFS runner path. `None` for FUSE (whose inode table lives
    /// with the mount process) and unit tests.
    persist: Option<Persister>,
}

impl Export {
    /// Build an export over `namespace` with no filehandle persistence: used by
    /// FUSE (inodes die with the process) and unit tests.
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
        let delayed_lists = PendingListings::new(rt.clone());
        let generation = persist
            .as_ref()
            .map_or_else(crate::protocol::filehandle::generation, |init| {
                init.generation
            });
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
                    body: Body::Node(Path::root()),
                },
            );
            by_node.insert((scope, Path::root()), scope);
        }
        // Keep the marker inode stable across frontend restarts without
        // persisting it as a namespace path that would need re-resolution.
        inodes.insert(
            SPOTLIGHT_MARKER_ID,
            Inode {
                scope: EXPORT_ROOT_ID,
                parent: EXPORT_ROOT_ID,
                name: SPOTLIGHT_MARKER_NAME.to_string(),
                kind: NodeKind::File,
                body: Body::Synthetic(b""),
            },
        );

        // Resume the allocation cursor and path-bearing filehandles from the
        // persisted state file.
        let next_base = persist.as_ref().map_or(EXPORT_ROOT_ID + 1, |init| {
            init.next_ino.max(EXPORT_ROOT_ID + 1)
        });
        let next_ino = Arc::new(AtomicU64::new(next_base));
        if let Some(init) = &persist {
            for entry in &init.entries {
                if entry.id == ROOT_ID || entry.id == EXPORT_ROOT_ID || is_reserved_inode(entry.id)
                {
                    continue;
                }
                inodes.insert(
                    entry.id,
                    Inode {
                        scope: entry.scope,
                        parent: entry.parent,
                        name: entry.name.clone(),
                        kind: entry.kind,
                        body: Body::Node(entry.path.clone()),
                    },
                );
                by_node.insert((entry.scope, entry.path.clone()), entry.id);
            }
        }

        let persist = persist.map(|init| {
            Persister::spawn(
                init.state_path,
                PersistTables {
                    generation: init.generation,
                    next_ino: Arc::clone(&next_ino),
                    inodes: Arc::clone(&inodes),
                },
            )
        });

        Self {
            rt,
            generation,
            clients: ClientTable::new(generation),
            namespace,
            replies: ReplyCache::new(),
            events,
            delayed_lists,
            inodes,
            by_node,
            next_ino,
            opens: OpenTable::new(),
            grown_sizes: DashMap::new(),
            persist,
        }
    }

    /// Run one namespace request and apply every event already enqueued by that
    /// request before the result becomes visible to the NFS caller.
    fn block_on_namespace<T>(
        &self,
        future: impl Future<Output = Result<T, NsError>>,
    ) -> Result<T, NsError> {
        let result = self.rt.block_on(future);
        self.apply_pending_events();
        result
    }

    fn node_attrs(&self, path: Path, exact: bool) -> Result<Attrs, NsError> {
        self.apply_pending_events();
        if let Some(attrs) = self.replies.attrs(&path) {
            return Ok(attrs);
        }

        let fence = self.replies.fence();
        let result = if exact {
            self.block_on_namespace(self.namespace.getattr_exact(path.clone()))
        } else {
            self.block_on_namespace(self.namespace.getattr(path.clone()))
        };
        if let Ok(attrs) = &result {
            self.replies.remember_attrs(fence, path, attrs);
        }
        result
    }

    fn child_lookup(&self, parent: Path, name: &str) -> Result<LookupAnswer, NsError> {
        self.apply_pending_events();
        if let Some(answer) = self.replies.lookup(&parent, name) {
            return Ok(answer);
        }

        let fence = self.replies.fence();
        let result = self.block_on_namespace(self.namespace.lookup(parent.clone(), name));
        if let Ok(answer) = &result {
            self.replies
                .remember_lookup(fence, parent, name.to_string(), answer);
        }
        result
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

    // --- events --------------------------------------------------------------

    /// Drain the buffered namespace events emitted since the last drain and apply
    /// them: prune the inodes and close the opens for an invalidated node, and
    /// record a live-follow grown size. Called inline after every namespace op so
    /// a caller sees its own invalidation before it re-reads its inode.
    fn apply_pending_events(&self) {
        let mut events = self.events.lock().expect("events lock");
        while let Some(event) = events.try_recv() {
            match event {
                NsEvent::InvalidateSubtree { path } => {
                    self.replies.invalidate(&path);
                    self.delayed_lists.reset(&path);
                    if path.is_root() {
                        self.grown_sizes.clear();
                    } else {
                        self.prune_node(&path);
                    }
                },
                NsEvent::AttrsChanged { path, attrs } => {
                    self.replies.invalidate(&path);
                    // Live growth is monotonic; never let a stale event shrink it.
                    let mut entry = self.grown_sizes.entry(path).or_insert(0);
                    *entry = (*entry).max(attrs.size);
                },
            }
        }
    }

    /// Drop every non-root inode in `path`'s subtree and close the opens bound
    /// to them. A root invalidation uses the stable-path refresh boundary and
    /// deliberately preserves all inode, open, stateid, and lease identities.
    fn prune_node(&self, path: &Path) {
        let affected: Vec<u64> = self
            .inodes
            .iter()
            .filter_map(|entry| {
                let node = entry.value().body.node()?;
                (node.has_prefix(path) && *entry.key() != ROOT_ID && *entry.key() != EXPORT_ROOT_ID)
                    .then_some(*entry.key())
            })
            .collect();
        for ino in &affected {
            if let Some((_, inode)) = self.inodes.remove(ino)
                && let Some(node) = inode.body.node()
            {
                for scope in [ROOT_ID, EXPORT_ROOT_ID] {
                    self.by_node.remove(&(scope, node.clone()));
                }
                self.grown_sizes.remove(&node);
            }
        }
        let _ = self.opens.remove_inodes(&affected);
        self.mark_dirty();
    }

    // --- identity ------------------------------------------------------------

    /// Allocate (or reuse) the inode for a resolved namespace path under `scope`.
    /// `forced` binds to a protocol-local filehandle id when restoring persisted
    /// state; `None` allocates or reuses the existing mapping. `name` is the name
    /// under `parent` recorded alongside the validated namespace path.
    // The arguments are the inode identity plus its resolved body; grouping them
    // into a struct would only shuffle the same fields behind one more type.
    #[allow(clippy::too_many_arguments)]
    fn intern_node(
        &self,
        forced: Option<u64>,
        scope: u64,
        parent: u64,
        name: &str,
        node: Path,
        kind: NodeKind,
    ) -> u64 {
        let ino = match forced {
            Some(id) => {
                self.by_node.insert((scope, node.clone()), id);
                id
            },
            None => *self
                .by_node
                .entry((scope, node.clone()))
                .or_insert_with(|| self.alloc_ino()),
        };
        // Never rewrite an export root's identity.
        if ino == ROOT_ID || ino == EXPORT_ROOT_ID {
            return ino;
        }
        self.inodes.insert(
            ino,
            Inode {
                scope,
                parent,
                name: name.to_string(),
                kind,
                body: Body::Node(node),
            },
        );
        self.mark_dirty();
        ino
    }

    // --- reads ---------------------------------------------------------------

    /// Read a whole namespace file by paging through the namespace until EOF.
    fn read_node_all(&self, node: &Path) -> StatusResult<Vec<u8>> {
        let mut data = Vec::new();
        let mut offset = 0_u64;
        loop {
            let answer = self
                .block_on_namespace(
                    self.namespace
                        .read(node.clone(), offset, MAX_NFS_READ_BYTES),
                )
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
        node: Path,
        offset: u64,
        count: u32,
    ) -> StatusResult<OpenRead> {
        let count = count.min(MAX_NFS_READ_BYTES);
        match self.block_on_namespace(self.namespace.read(node, offset, count)) {
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

    // --- listing -------------------------------------------------------------

    /// The truth computation for one directory: drain every namespace page into a
    /// finite snapshot, as a `'static` future factory the deferral table spawns.
    /// Errors are logged and mapped to `Status` here so the table stays
    /// protocol-agnostic.
    fn list_op(
        &self,
        node: Path,
    ) -> impl FnOnce() -> Pin<Box<dyn Future<Output = crate::delayed::ListResult> + Send>> + use<>
    {
        let namespace = Arc::clone(&self.namespace);
        move || {
            Box::pin(async move {
                let mut entries = Vec::new();
                let mut cursor = DirCursor::start();
                loop {
                    let page = namespace.readdir(node.clone(), cursor, 0).await.map_err(|error| {
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
    fn snapshot(
        &self,
        scope: u64,
        parent: u64,
        parent_path: &Path,
        entries: &[NsDirEntry],
    ) -> DirListing {
        self.replies
            .seed(self.replies.fence(), parent_path, entries);
        let mut out = Vec::with_capacity(entries.len());
        for entry in entries {
            let kind = NodeKind::from(&entry.attrs.kind);
            let id = self.intern_node(None, scope, parent, &entry.name, entry.path.clone(), kind);
            let mut attr = if kind == NodeKind::File {
                match self.node_attrs(entry.path.clone(), true) {
                    Ok(attrs) => Attr::from_namespace(id, parent, &attrs),
                    // A child that vanished between listing and probe keeps its
                    // listing attrs rather than dropping out of the snapshot.
                    Err(_) => Attr::from_namespace(id, parent, &entry.attrs),
                }
            } else {
                Attr::from_namespace(id, parent, &entry.attrs)
            };
            if let Some(grown) = self.grown_sizes.get(&entry.path) {
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
}

impl ReadOnlyExport for Export {
    fn generation(&self) -> u64 {
        self.generation
    }

    fn set_clientid(&self, verifier: [u8; 8], owner: Vec<u8>) -> (u64, [u8; 8]) {
        self.clients.set_clientid(verifier, owner)
    }

    fn confirm_client(&self, clientid: u64, verifier: &[u8]) -> StatusResult<()> {
        self.clients.confirm(clientid, verifier)
    }

    fn client_confirmed(&self, clientid: u64) -> bool {
        self.clients.is_confirmed(clientid)
    }

    fn root(&self) -> u64 {
        ROOT_ID
    }

    fn attr(&self, id: u64) -> StatusResult<Attr> {
        let inode = self.inodes.get(&id).ok_or(Status::Stale)?;
        let parent = inode.parent;
        let body = inode.body.clone();
        drop(inode);

        if let Body::Synthetic(data) = body {
            return Ok(Attr {
                id,
                parent,
                kind: NodeKind::File,
                size: data.len() as u64,
                mode: NodeKind::File.mode(),
                change: 0,
                mtime_sec: 0,
            });
        }

        let node = body.node().ok_or(Status::Stale)?;
        let attrs = match self.node_attrs(node.clone(), false) {
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

        let mut attr = Attr::from_namespace(id, parent, &attrs);
        if let Some(grown) = self.grown_sizes.get(&node) {
            attr.size = attr.size.max(*grown);
        }
        Ok(attr)
    }

    fn lookup(&self, parent: u64, name: &str) -> StatusResult<u64> {
        let name = Segment::try_from(name).map_err(|_| Status::Invalid)?;
        self.apply_pending_events();
        let inode = self.inodes.get(&parent).ok_or(Status::Stale)?;
        if inode.kind != NodeKind::Directory {
            return Err(Status::NotDir);
        }
        let scope = inode.scope;
        let body = inode.body.clone();
        drop(inode);

        if parent == EXPORT_ROOT_ID && name.as_str() == SPOTLIGHT_MARKER_NAME {
            return Ok(SPOTLIGHT_MARKER_ID);
        }

        let parent_node = body.node().ok_or(Status::Stale)?;
        match self.child_lookup(parent_node, name.as_str()) {
            Ok(answer) => match answer.state {
                LookupState::Found { attrs } => {
                    let answer_path = answer.path;
                    let id = self.intern_node(
                        None,
                        scope,
                        parent,
                        name.as_str(),
                        answer_path.clone(),
                        NodeKind::from(&attrs.kind),
                    );
                    // Eagerly probe a file child's exact size so a bare `stat`/`ls -l`
                    // after the lookup reflects a ranged file's real size; the
                    // namespace caches the learned size for the later `getattr`.
                    // `getattr_exact` only learns a deferred-ranged file's size; an
                    // unknown-length deferred-full file only learns its exact size
                    // through a read, which materializes it and lets the namespace
                    // cache the result. This must happen at lookup time: the macOS
                    // NFS client pins the size it knew before OPEN for the lifetime
                    // of that open (even `fstat` on the open fd serves the pinned
                    // value), so learning at OPEN is too late and the first `cat` of
                    // a cold file would be clamped to the 1-byte sentinel.
                    if matches!(NodeKind::from(&attrs.kind), NodeKind::File)
                        && let Ok(exact) = self.node_attrs(answer_path.clone(), true)
                        && exact.size <= 1
                    {
                        let _ = self.read_node_chunk(id, answer_path, 0, 1);
                    }
                    self.apply_pending_events();
                    Ok(id)
                },
                LookupState::Missing { .. }
                    if parent == ROOT_ID && name.as_str() == NFS_EXPORT_NAME =>
                {
                    Ok(EXPORT_ROOT_ID)
                },
                LookupState::Missing { .. } => Err(Status::NoEnt),
            },
            // A cold provider lookup that misses at the protocol root resolves the
            // `/omnifs` export alias, mirroring how the client mounts `:/omnifs`.
            Err(error) => {
                tracing::warn!(op = "lookup", name = %name, error = %error, "NFS namespace lookup failed");
                Err(Status::from(&error))
            },
        }
    }

    fn readdir(&self, id: u64) -> StatusResult<DirListing> {
        self.apply_pending_events();
        let inode = self.inodes.get(&id).ok_or(Status::Stale)?;
        if inode.kind != NodeKind::Directory {
            return Err(Status::NotDir);
        }
        let scope = inode.scope;
        let body = inode.body.clone();
        drop(inode);

        let node = body.node().ok_or(Status::Stale)?;
        // Proactive deferral only. On persistent failure the namespace does not
        // cache the error, so each retry may re-defer until the listing succeeds
        // or maps to a terminal `Status`.
        let outcome =
            self.delayed_lists
                .resolve(node.clone(), NFS_INLINE_BUDGET, self.list_op(node.clone()));
        self.apply_pending_events();
        match outcome {
            PendingOutcome::Ready(result) => {
                self.apply_pending_events();
                match result.as_ref() {
                    Ok(entries) => Ok(self.snapshot(scope, id, &node, entries)),
                    Err(status) => Err(*status),
                }
            },
            PendingOutcome::Pending => Err(Status::Delay),
        }
    }

    fn read(&self, id: u64) -> StatusResult<Vec<u8>> {
        self.apply_pending_events();
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
            Body::Node(node) => self.read_node_all(&node),
            Body::Synthetic(data) => Ok(data.to_vec()),
        }
    }

    fn readlink(&self, id: u64) -> StatusResult<Vec<u8>> {
        let inode = self.inodes.get(&id).ok_or(Status::Stale)?;
        if inode.kind != NodeKind::Symlink {
            return Err(Status::Invalid);
        }
        let Body::Node(node) = inode.body.clone() else {
            return Err(Status::Invalid);
        };
        drop(inode);
        self.apply_pending_events();
        if self.inodes.get(&id).is_none() {
            return Err(Status::Stale);
        }
        self.block_on_namespace(self.namespace.readlink(node))
            .map(|target| target.as_os_str().as_encoded_bytes().to_vec())
            .map_err(|error| Status::from(&error))
    }

    fn open_state(&self, id: u64, clientid: u64, access: u32) -> StatusResult<OpenResult> {
        // The protocol validated `attr.kind != Directory/Symlink` before OPEN, so
        // this is a file. `attr` both drains pending events and rebinds a
        // subtree, so re-read the body afterwards.
        let mut attr = self.attr(id)?;
        let inode = self.inodes.get(&id).ok_or(Status::Stale)?;
        let body = inode.body.clone();
        drop(inode);

        match body {
            Body::Synthetic(_) => {},
            Body::Node(node) => {
                // Learn the exact size before the OPEN reply. The lookup that
                // precedes OPEN already runs this probe, so this is the backstop
                // for opens that arrive without a fresh lookup (e.g. a stale
                // cached inode reopened directly). Seek-from-end readers
                // (`tail -n`) trust the size their post-open stat reports, so a
                // cold unknown-length file must not answer the 1-byte sentinel
                // past OPEN. One 1-byte read makes the namespace learn and cache
                // the exact size; subsequent READs stay read-through.
                if attr.size <= 1 {
                    self.read_node_chunk(id, node, 0, 1)?;
                    attr = self.attr(id)?;
                }
            },
        }
        // The open records the validated namespace path; each read uses that
        // path directly after any invalidation refresh.
        let stateid = self.opens.open(OpenSeed {
            generation: self.generation,
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
        // Resolve the persisted namespace path through the shared namespace.
        let body = self
            .inodes
            .get(&inode_id)
            .ok_or(Status::Stale)?
            .body
            .clone();
        match body {
            Body::Node(node) => self.read_node_chunk(inode_id, node, offset, count),
            Body::Synthetic(data) => {
                let start = usize::try_from(offset).map_err(|_| Status::Invalid)?;
                let count = usize::try_from(count).map_err(|_| Status::Invalid)?;
                let end = start.saturating_add(count).min(data.len());
                Ok(OpenRead {
                    id: inode_id,
                    data: data.get(start..end).unwrap_or_default().to_vec(),
                    eof: end >= data.len(),
                })
            },
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
            EntryKind::Directory => Self::Directory,
            EntryKind::File => Self::File,
            EntryKind::Symlink => Self::Symlink,
        }
    }
}

impl Attr {
    fn from_namespace(id: u64, parent: u64, attrs: &Attrs) -> Self {
        let kind = NodeKind::from(&attrs.kind);
        Self {
            id,
            parent,
            kind,
            size: attrs.size,
            mode: u32::from(attrs.mode),
            change: attrs.change,
            mtime_sec: attrs
                .modified
                .map_or(0, |millis| i64::try_from(millis / 1000).unwrap_or(i64::MAX)),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use omnifs_engine::{MountTable, TreeNamespace};
    use tempfile::TempDir;
    use tokio::runtime::Runtime as TokioRuntime;

    struct TestExport {
        export: Export,
        _runtime: TokioRuntime,
        _cache_dir: TempDir,
        _config_dir: TempDir,
        _providers_dir: TempDir,
    }

    /// Build an `Export` over an empty registry: there are no mounts, so these
    /// tests drive only the namespace path, which never touches
    /// the namespace's provider surface.
    fn empty_export() -> TestExport {
        let runtime = TokioRuntime::new().expect("tokio runtime");
        let handle = runtime.handle().clone();
        let cache_dir = tempfile::tempdir().expect("cache dir");
        let config_dir = tempfile::tempdir().expect("config dir");
        let providers_dir = tempfile::tempdir().expect("providers dir");
        let credentials_file = config_dir.path().join("credentials.json");
        let mounts_dir = tempfile::tempdir().expect("mounts dir");
        let desired = omnifs_workspace::mounts::Registry::load(mounts_dir.path())
            .expect("load mount snapshot");
        let host = omnifs_engine::test_support::open_test_host(
            cache_dir.path(),
            providers_dir.path(),
            &credentials_file,
            cache_dir.path().join("clones"),
        )
        .expect("open test host");
        let registry = Arc::new(
            MountTable::load_online(host.as_online().expect("online host"), &desired, &handle)
                .expect("load mount snapshot"),
        );

        let namespace = TreeNamespace::online(registry, handle.clone());
        let export = Export::new(handle, namespace);
        TestExport {
            export,
            _runtime: runtime,
            _cache_dir: cache_dir,
            _config_dir: config_dir,
            _providers_dir: providers_dir,
        }
    }

    // --- re-resolution and stateid recovery ----------------------------------

    use crate::export::StateId;
    use crate::persist::{FhEntry, PersistInit};
    use omnifs_core::path::Path;
    use omnifs_engine::namespace::{
        Attrs, DirEntry as NamespaceDirEntry, DirPage, LookupAnswer, ReadAnswer, ReadStyle,
        StabilityClass,
    };
    use std::collections::HashMap;
    use std::path::PathBuf;
    use std::sync::atomic::{AtomicBool, AtomicU64};
    use std::time::Duration;

    fn path(value: &str) -> Path {
        Path::parse(value).expect("valid test path")
    }

    /// A minimal in-memory namespace: a fixed `(parent, name) -> (node, kind)`
    /// tree, counting lookups so a test can prove the parent chain was walked.
    struct StubNamespace {
        children: HashMap<(Path, String), (Path, EntryKind)>,
        kinds: HashMap<Path, EntryKind>,
        lookups: AtomicU64,
        /// A file node that reports the unknown-size sentinel until a read
        /// probes it, simulating a deferred-full file that only learns its
        /// exact size by materializing through a read.
        deferred_node: Option<Path>,
        materialized: AtomicBool,
        getattrs: AtomicU64,
        reads: AtomicU64,
        last_read: Mutex<Option<(u64, u32)>>,
    }

    fn stub_attrs(kind: EntryKind) -> Attrs {
        let size = if matches!(kind, EntryKind::File) {
            5
        } else {
            0
        };
        Attrs {
            kind,
            dev: 0,
            ino: 0,
            size,
            blocks: size.div_ceil(512),
            mode: 0o444,
            nlink: 1,
            accessed: None,
            modified: None,
            created: None,
            ttl: Duration::from_mins(1),
            change: 1,
            direct_io: false,
            stability: StabilityClass::Stable,
            read_style: ReadStyle::Whole,
        }
    }

    impl StubNamespace {
        /// Attrs for `node`, clamped to the unknown-size sentinel while it is
        /// the tree's deferred file and hasn't been materialized by a read yet.
        fn attrs_for(&self, node: &Path, kind: EntryKind) -> Attrs {
            let mut attrs = stub_attrs(kind);
            if self.deferred_node.as_ref() == Some(node)
                && !self.materialized.load(Ordering::Relaxed)
            {
                attrs.size = 1;
                attrs.ttl = Duration::ZERO;
            }
            attrs
        }
    }

    impl Namespace for StubNamespace {
        fn lookup<'a>(
            &'a self,
            parent: Path,
            name: &'a str,
        ) -> Pin<Box<dyn Future<Output = Result<LookupAnswer, NsError>> + Send + 'a>> {
            self.lookups.fetch_add(1, Ordering::Relaxed);
            let answer = self
                .children
                .get(&(parent.clone(), name.to_string()))
                .map_or_else(
                    || {
                        LookupAnswer::missing(
                            parent.join(name).expect("stub lookup path"),
                            Duration::from_mins(1),
                        )
                    },
                    |(node, kind)| {
                        let attrs = self.attrs_for(node, kind.clone());
                        LookupAnswer::found(node.clone(), attrs)
                    },
                );
            Box::pin(async move { Ok(answer) })
        }

        fn getattr(
            &self,
            node: Path,
        ) -> Pin<Box<dyn Future<Output = Result<Attrs, NsError>> + Send + '_>> {
            self.getattrs.fetch_add(1, Ordering::Relaxed);
            let answer = self
                .kinds
                .get(&node)
                .map(|kind| self.attrs_for(&node, kind.clone()))
                .ok_or(NsError::NotFound);
            Box::pin(async move { answer })
        }

        fn getattr_exact(
            &self,
            node: Path,
        ) -> Pin<Box<dyn Future<Output = Result<Attrs, NsError>> + Send + '_>> {
            self.getattr(node)
        }

        fn readdir(
            &self,
            node: Path,
            _cursor: DirCursor,
            _budget: usize,
        ) -> Pin<Box<dyn Future<Output = Result<DirPage, NsError>> + Send + '_>> {
            let entries = self
                .children
                .iter()
                .filter(|((parent, _), _)| parent == &node)
                .map(|((_, name), (path, kind))| {
                    let attrs = self.attrs_for(path, kind.clone());
                    NamespaceDirEntry {
                        name: name.clone(),
                        path: path.clone(),
                        attrs,
                    }
                })
                .collect();
            Box::pin(async move {
                Ok(DirPage {
                    entries,
                    next: None,
                })
            })
        }

        fn read(
            &self,
            node: Path,
            offset: u64,
            len: u32,
        ) -> Pin<Box<dyn Future<Output = Result<ReadAnswer, NsError>> + Send + '_>> {
            self.reads.fetch_add(1, Ordering::Relaxed);
            *self.last_read.lock().expect("lock last_read") = Some((offset, len));
            if self.deferred_node.as_ref() == Some(&node) {
                self.materialized.store(true, Ordering::Relaxed);
            }
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
            _node: Path,
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
            (Path::root(), "test".to_string()),
            (path("/test"), EntryKind::Directory),
        );
        children.insert(
            (path("/test"), "message".to_string()),
            (path("/test/message"), EntryKind::File),
        );
        let mut kinds = HashMap::new();
        kinds.insert(path("/test"), EntryKind::Directory);
        kinds.insert(path("/test/message"), EntryKind::File);
        StubNamespace {
            children,
            kinds,
            lookups: AtomicU64::new(0),
            deferred_node: None,
            materialized: AtomicBool::new(false),
            getattrs: AtomicU64::new(0),
            reads: AtomicU64::new(0),
            last_read: Mutex::new(None),
        }
    }

    /// Variant of [`stub_tree`] whose `message` file reports the unknown-size
    /// sentinel until a probe read materializes it, for testing the
    /// lookup-time size-learning probe.
    fn stub_tree_with_deferred_file() -> StubNamespace {
        let mut tree = stub_tree();
        tree.deferred_node = Some(path("/test/message"));
        tree
    }

    fn cold_entries() -> Vec<FhEntry> {
        vec![
            FhEntry {
                id: 100,
                scope: ROOT_ID,
                parent: ROOT_ID,
                name: "test".to_string(),
                kind: NodeKind::Directory,
                path: path("/test"),
            },
            FhEntry {
                id: 101,
                scope: ROOT_ID,
                parent: 100,
                name: "message".to_string(),
                kind: NodeKind::File,
                path: path("/test/message"),
            },
        ]
    }

    #[test]
    fn persisted_path_handle_reads_without_parent_rewalk() {
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

        // The path-bearing entry is live immediately after reload, so attr/open/read
        // need no namespace lookup or parent walk.
        let attr = export.attr(101).expect("persisted path handle");
        assert_eq!(attr.kind, NodeKind::File);
        assert_eq!(attr.size, 5);
        assert!(
            namespace.lookups.load(Ordering::Relaxed) == 0,
            "a persisted structural path must not rewalk its parents"
        );

        // A held-open read against the same id also re-resolves.
        let opened = export.open_state(101, 1, 1).expect("open persisted handle");
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
            path: path("/missing"),
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
    fn lookup_seeds_attrs_without_getattr() {
        let runtime = TokioRuntime::new().expect("tokio runtime");
        let namespace = Arc::new(stub_tree());
        let export = Export::new(
            runtime.handle().clone(),
            Arc::clone(&namespace) as Arc<dyn Namespace>,
        );

        let test_dir = export.lookup(export.root(), "test").expect("lookup test");
        export.attr(test_dir).expect("first attr");
        export.attr(test_dir).expect("second attr");

        assert_eq!(
            namespace.getattrs.load(Ordering::Relaxed),
            0,
            "a positive lookup must seed the child's attribute answer"
        );
    }

    #[test]
    fn cacheable_attrs_round_trip_once() {
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

        for _ in 0..128 {
            export.attr(101).expect("cached attr");
        }

        assert_eq!(
            namespace.getattrs.load(Ordering::Relaxed),
            1,
            "a cacheable namespace attr must cross the process boundary once"
        );
    }

    #[test]
    fn zero_ttl_attrs_always_round_trip() {
        let runtime = TokioRuntime::new().expect("tokio runtime");
        let namespace = Arc::new(stub_tree_with_deferred_file());
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

        export.attr(101).expect("first attr");
        export.attr(101).expect("second attr");

        assert_eq!(
            namespace.getattrs.load(Ordering::Relaxed),
            2,
            "an unknown-size zero-TTL attr must never be retained"
        );
    }

    #[test]
    fn negative_lookup_round_trips_once() {
        let runtime = TokioRuntime::new().expect("tokio runtime");
        let namespace = Arc::new(stub_tree());
        let export = Export::new(
            runtime.handle().clone(),
            Arc::clone(&namespace) as Arc<dyn Namespace>,
        );

        for _ in 0..128 {
            assert_eq!(export.lookup(export.root(), ".git"), Err(Status::NoEnt));
        }

        assert_eq!(
            namespace.lookups.load(Ordering::Relaxed),
            1,
            "a leased negative lookup must cross the process boundary once"
        );
    }

    #[test]
    fn readdir_seeds_lookup() {
        let runtime = TokioRuntime::new().expect("tokio runtime");
        let namespace = Arc::new(stub_tree());
        let export = Export::new(
            runtime.handle().clone(),
            Arc::clone(&namespace) as Arc<dyn Namespace>,
        );

        export.readdir(export.root()).expect("root listing");
        export.lookup(export.root(), "test").expect("seeded lookup");

        assert_eq!(
            namespace.lookups.load(Ordering::Relaxed),
            0,
            "a listed child must not require a second namespace lookup"
        );
    }

    #[test]
    fn lookup_probes_unknown_size_file_and_learns_exact_size() {
        let runtime = TokioRuntime::new().expect("tokio runtime");
        let namespace = Arc::new(stub_tree_with_deferred_file());
        let export = Export::new(
            runtime.handle().clone(),
            Arc::clone(&namespace) as Arc<dyn Namespace>,
        );

        let test_dir = export.lookup(export.root(), "test").expect("lookup test");
        let message = export.lookup(test_dir, "message").expect("lookup message");

        assert_eq!(
            namespace.reads.load(Ordering::Relaxed),
            1,
            "an unknown-size file must be probed with exactly one read during lookup"
        );
        assert_eq!(
            *namespace.last_read.lock().expect("lock last_read"),
            Some((0, 1)),
            "the lookup-time probe must be a one-byte read at offset 0"
        );

        let attr = export.attr(message).expect("attr after lookup probe");
        assert_eq!(
            attr.size, 5,
            "attrs served after lookup must reflect the size learned by the probe"
        );
    }

    #[test]
    fn spotlight_marker_is_hidden_lookup_only_and_read_only() {
        let harness = empty_export();
        let export_root = harness
            .export
            .lookup(harness.export.root(), NFS_EXPORT_NAME)
            .expect("lookup export root");
        let marker = harness
            .export
            .lookup(export_root, SPOTLIGHT_MARKER_NAME)
            .expect("lookup Spotlight marker");

        assert_eq!(marker, SPOTLIGHT_MARKER_ID);
        assert_eq!(harness.export.read(marker).expect("read marker"), b"");
        assert_eq!(harness.export.attr(marker).expect("marker attrs").size, 0);
        assert!(
            !harness
                .export
                .readdir(export_root)
                .expect("list export root")
                .entries
                .iter()
                .any(|entry| entry.name == SPOTLIGHT_MARKER_NAME),
            "the marker must not pollute normal directory listings"
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
