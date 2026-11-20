//! Host projection storage and derived memory acceleration.
//!
//! `ProjectionStore` owns durable facts for one projection identity. The
//! process-local `MemoryTier` is populated only after durable publication and
//! is safe to discard on restart.
//!
//! ## Global caches, per-mount resource
//!
//! `Caches` holds one global Fjall database for projection facts and one global
//! content-addressed body store. It is opened once at process start and shared
//! via `Arc`. `Caches::mount` returns the sole `MountResources` owner for a
//! projection identity.
//!
//! The per-projection invalidation epoch lives in `MountResources`; derived
//! memory eviction happens only after a durable transition commits.

use fjall::OptimisticTxDatabase;
use fjall::Readable;
use omnifs_core::MountName;
use omnifs_core::path::Path;
use parking_lot::Mutex;
use std::collections::{BTreeMap, HashMap, HashSet};
use std::path::Path as StdPath;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Weak};
use tokio::sync::Mutex as AsyncMutex;
use tokio::sync::Notify;

use super::body::{BodyId, BodyStore};
use super::identity::ProjectionId;
use super::identity::{BlobRequestId, GitId};
use super::projection::{ProjectionStore, ProjectionStoreError};
use crate::cache::memory::MemoryTier;
use crate::object_id::ObjectId;
use crate::view::{
    AttrPayload, ByteSource, CachedCursor, DirentsPage, DirentsPayload, EntryMeta, FileAttrsCache,
    FilePayload, FileSize, ListingWrite, LookupPayload,
};

/// Result of a warm canonical lookup: the object id, the canonical bytes, and
/// the optional validator.
pub struct CachedCanonical {
    pub id: Vec<u8>,
    pub bytes: Vec<u8>,
    pub validator: Option<String>,
}

#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
pub(crate) struct BlobMetadata {
    pub status: u16,
    pub content_type: Option<String>,
    pub etag: Option<String>,
    pub response_headers: Vec<(String, String)>,
    pub size: u64,
}

#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
pub(crate) struct BlobRecord {
    pub id: u64,
    pub body: BodyId,
    pub size: u64,
    pub content_type: Option<String>,
    pub etag: Option<String>,
    pub status: u16,
    pub response_headers: Vec<(String, String)>,
}

impl BlobRecord {
    pub(crate) fn metadata(&self) -> BlobMetadata {
        BlobMetadata {
            status: self.status,
            content_type: self.content_type.clone(),
            etag: self.etag.clone(),
            response_headers: self.response_headers.clone(),
            size: self.size,
        }
    }
}

#[derive(Debug, Clone)]
pub(crate) enum FactPayload {
    Lookup(LookupPayload),
    Attr(AttrPayload),
    File(FilePayload),
    FileBody {
        version_token: Option<String>,
        content_type: Option<String>,
        body: BodyId,
        length: u64,
    },
}

#[derive(Debug, Clone)]
pub(crate) struct RecordWrite {
    pub path: Path,
    pub aux: Option<String>,
    pub fact: FactPayload,
}

#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
pub(crate) struct BlobFact {
    pub body_id: [u8; 32],
    pub length: u64,
    pub metadata: BlobMetadata,
}

/// The Git tree of a path: its clone, its relative path in the clone, and
/// the commit of its last handoff.
#[derive(Debug, Clone)]
pub(crate) struct GitFact {
    pub id: GitId,
    pub relative_path: String,
    pub commit: String,
}

#[derive(Debug, Clone)]
pub(crate) struct BlobWrite {
    pub request: BlobRequestId,
    pub body: BodyId,
    pub metadata: BlobMetadata,
}

#[derive(Debug, Clone)]
pub(crate) struct GitWrite {
    pub path: Path,
    pub id: GitId,
    pub relative_path: String,
    pub commit: String,
}

#[derive(Debug, Clone)]
pub(crate) enum DirentsMutation {
    Replace {
        path: Path,
        value: DirentsPayload,
    },
    MergeHints {
        path: Path,
        entries: Vec<crate::view::DirentRecord>,
        write: ListingWrite,
    },
    /// The next page of a listing, asked for with the listing's next cursor.
    AppendPage {
        path: Path,
        page: DirentsPage,
        next_cursor: Option<CachedCursor>,
        exhaustive: bool,
    },
    /// A changed cached page, asked for with its validator: it replaces the
    /// page of its cursor and keeps the later pages only while its next
    /// cursor stays.
    ReplacePage {
        path: Path,
        page: DirentsPage,
        next_cursor: Option<CachedCursor>,
        exhaustive: bool,
    },
    /// An `unchanged` answer for the cached page of `cursor`: the page keeps
    /// its entries, takes `interval` when the answer carries one, and gets a
    /// new deadline from its interval.
    RenewPage {
        path: Path,
        cursor: Option<CachedCursor>,
        token: String,
        interval: Option<u32>,
        now_millis: u64,
    },
}

impl DirentsMutation {
    pub(crate) fn path(&self) -> &Path {
        match self {
            Self::Replace { path, .. }
            | Self::MergeHints { path, .. }
            | Self::AppendPage { path, .. }
            | Self::ReplacePage { path, .. }
            | Self::RenewPage { path, .. } => path,
        }
    }
}

#[derive(Debug, Clone)]
pub(crate) enum ObjectMutation {
    Canonical {
        id: Vec<u8>,
        bytes: Vec<u8>,
        validator: Option<String>,
    },
    Index {
        id: Vec<u8>,
        alias: Path,
    },
}

#[derive(Debug, Clone)]
pub(crate) struct Freshness {
    pub path: Path,
    pub expires_at: Option<u64>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
#[repr(u8)]
pub enum RecordKind {
    Lookup = 0,
    Attr = 1,
    Dirents = 2,
    File = 3,
}

impl RecordKind {
    pub const ALL: [Self; 4] = [Self::Lookup, Self::Attr, Self::Dirents, Self::File];

    pub(super) fn wire_prefix(self) -> char {
        match self {
            Self::Lookup => 'L',
            Self::Attr => 'A',
            Self::Dirents => 'D',
            Self::File => 'F',
        }
    }
}

#[derive(Debug, Clone, Eq, Hash, PartialEq)]
pub struct Key {
    pub path: Path,
    pub kind: RecordKind,
    pub aux: Option<String>,
}

impl Key {
    pub fn with_aux(path: &Path, kind: RecordKind, aux: Option<impl Into<String>>) -> Self {
        Self {
            path: path.clone(),
            kind,
            aux: aux.map(Into::into),
        }
    }

    pub(super) fn wire_key(&self) -> String {
        let prefix = self.kind.wire_prefix();
        match &self.aux {
            Some(aux) => format!("{prefix}:{}\u{1f}{}", self.path, hex::encode(aux)),
            None => format!("{prefix}:{}", self.path),
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Record {
    pub kind: RecordKind,
    pub payload: Vec<u8>,
}

#[derive(Debug, Clone)]
pub(crate) enum Invalidation {
    Object(Vec<u8>),
    ListingPath(Path),
    ListingPrefix(Path),
}

#[derive(Debug, Default)]
pub(crate) struct ProjectionTransition {
    pub records: Vec<RecordWrite>,
    pub dirents: Vec<DirentsMutation>,
    pub objects: Vec<ObjectMutation>,
    pub freshness: Vec<Freshness>,
    pub invalidations: Vec<Invalidation>,
    pub blobs: Vec<BlobWrite>,
    pub git: Vec<GitWrite>,
    /// Paths whose facts, with the facts beneath them, leave the projection;
    /// each leaves its parent listing as complete as it was.
    pub removals: Vec<Path>,
    /// The answer of a feed: its records and listings are fed facts, and the
    /// feed states it brought.
    pub feed: Option<FeedWrite>,
    /// A part of a later answer of a feed, kept apart from the facts until
    /// the part that ends its answer.
    pub kept: Option<KeptWrite>,
}

impl ProjectionTransition {
    pub(crate) fn is_empty(&self) -> bool {
        self.records.is_empty()
            && self.dirents.is_empty()
            && self.objects.is_empty()
            && self.freshness.is_empty()
            && self.invalidations.is_empty()
            && self.blobs.is_empty()
            && self.git.is_empty()
            && self.removals.is_empty()
            && self.feed.is_none()
            && self.kept.is_none()
    }
}

/// The state of a feed: the token of its last part, whether its first
/// answer ended, and its deadline, after which an operation that does not
/// start it asks it for an answer.
#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub(crate) struct FeedState {
    pub token: String,
    pub ended: bool,
    pub deadline: u64,
}

/// The feed states one answer of a feed publishes: its own, renewed by an
/// `unchanged` answer or advanced by a part, which drops its kept parts, and
/// the inner states the part brought, each a token whose first answer ended.
/// Every state gets `deadline`. `anchors` pairs each anchor of another facet
/// that the part writes with the anchor of the claim of its object.
#[derive(Debug, Clone)]
pub(crate) struct FeedWrite {
    pub object: Vec<u8>,
    pub part: Option<FeedPartWrite>,
    pub inner: Vec<(Vec<u8>, String)>,
    pub anchors: Vec<(Path, Path)>,
    pub deadline: u64,
}

/// A part of a later answer of the feed of `object`, encoded by its feed
/// operation, which the projection keeps in order until the part that ends
/// the answer publishes it.
#[derive(Debug, Clone)]
pub(crate) struct KeptWrite {
    pub object: Vec<u8>,
    pub part: Vec<u8>,
}

/// The token after a part, and whether more parts follow in its answer.
#[derive(Debug, Clone)]
pub(crate) struct FeedPartWrite {
    pub token: String,
    pub more: bool,
}

#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
enum DurableFact {
    Lookup(LookupPayload),
    Attr(AttrPayload),
    Dirents(DirentsPayload),
    File {
        version_token: Option<String>,
        content_type: Option<String>,
        body_id: [u8; 32],
        length: u64,
    },
    Blob(BlobFact),
    Git {
        id: GitId,
        relative_path: String,
        commit: String,
    },
}

impl DurableFact {
    fn kind(&self) -> RecordKind {
        match self {
            Self::Lookup(_) => RecordKind::Lookup,
            Self::Attr(_) => RecordKind::Attr,
            Self::Dirents(_) => RecordKind::Dirents,
            Self::File { .. } => RecordKind::File,
            Self::Blob(_) => unreachable!("blob facts use b: keys"),
            Self::Git { .. } => unreachable!("Git facts use g: keys"),
        }
    }
}

fn blob_key(request: BlobRequestId) -> Vec<u8> {
    let mut key = b"b:".to_vec();
    key.extend_from_slice(request.filesystem_name().as_bytes());
    key
}

struct PreparedRecord {
    path: Path,
    aux: Option<String>,
    fact: DurableFact,
}

#[derive(Debug, thiserror::Error)]
pub(crate) enum ProjectionError {
    #[error("body publication failed: {0}")]
    Body(#[from] super::body::BodyStoreError),
    #[error("projection store failed: {0}")]
    Store(#[from] super::projection::ProjectionStoreError),
    #[error("projection fact encoding failed: {0}")]
    Encoding(#[from] postcard::Error),
    #[error("projection database operation failed: {0}")]
    Fjall(#[from] fjall::Error),
    #[error("projection transition conflicts with an existing object identity")]
    ClaimConflict,
    #[error("projection transition is inconsistent: {0}")]
    Inconsistent(String),
}

impl From<ProjectionError> for ProjectionStoreError {
    fn from(error: ProjectionError) -> Self {
        match error {
            ProjectionError::Store(error) => error,
            other => Self::Transaction(other.to_string()),
        }
    }
}

#[derive(Debug)]
pub(crate) enum PublicationOutcome {
    Committed { invalidations: Vec<Invalidation> },
    Fenced,
}

impl Record {
    pub fn new(kind: RecordKind, payload: Vec<u8>) -> Self {
        Self { kind, payload }
    }
}

/// Process-global body-store and projection-database factory. Opened once at
/// startup and shared via `Arc`.
pub struct Caches {
    pub(crate) body: Arc<BodyStore>,
    projection_root: std::path::PathBuf,
    pub(crate) projection_database: OptimisticTxDatabase,
    projection_fences: Mutex<HashMap<ProjectionId, Weak<ResourceFence>>>,
}

impl Caches {
    /// Open the global cache handles from `dir`.
    ///
    pub fn open(dir: &StdPath) -> anyhow::Result<Arc<Self>> {
        let dir = crate::cache::canonical_directory(dir)?;
        crate::cache::ensure_directory(&dir)?;
        let projection_root = crate::cache::canonical_directory(&dir.join("projections"))?;
        crate::cache::ensure_directory(&projection_root)?;
        let projection_metadata = std::fs::symlink_metadata(&projection_root)?;
        if projection_metadata.file_type().is_symlink() || !projection_metadata.is_dir() {
            anyhow::bail!("projection store root is not a regular directory");
        }
        let projection_database =
            OptimisticTxDatabase::builder(projection_root.join("database")).open()?;
        let body = Arc::new(BodyStore::open(dir.join("bodies"))?);
        Ok(Arc::new(Self {
            body,
            projection_root,
            projection_database,
            projection_fences: Mutex::new(HashMap::new()),
        }))
    }

    /// Return the sole owner of one mount's cache and blob resources.
    pub(crate) fn mount(
        self: &Arc<Self>,
        mount: &MountName,
        projection_id: ProjectionId,
        spec_source: &[u8],
    ) -> anyhow::Result<Arc<MountResources>> {
        let resources = self.prepare_mount(mount, projection_id, spec_source)?;
        resources.activate();
        Ok(resources)
    }

    /// Open isolated process-local resources for a candidate generation.
    ///
    /// The durable projection is shared, but publication remains fenced until
    /// the serving cell activates this owner.
    pub(crate) fn prepare_mount(
        self: &Arc<Self>,
        mount: &MountName,
        projection_id: ProjectionId,
        spec_source: &[u8],
    ) -> anyhow::Result<Arc<MountResources>> {
        let fence = self.resource_fence(projection_id);
        MountResources::new(self, mount, projection_id, spec_source, fence)
    }

    fn resource_fence(&self, projection_id: ProjectionId) -> Arc<ResourceFence> {
        let mut fences = self.projection_fences.lock();
        if let Some(fence) = fences.get(&projection_id).and_then(Weak::upgrade) {
            return fence;
        }
        let fence = Arc::new(ResourceFence::default());
        fences.insert(projection_id, Arc::downgrade(&fence));
        fence
    }
}

/// Per-projection owner over the global body store and Fjall database.
pub struct MountResources {
    pub(crate) projection: ProjectionStore,
    pub(crate) body: Arc<BodyStore>,
    pub(crate) memory: MemoryTier,
    pub(crate) coherence: Mutex<Coherence>,
    pub(crate) request_locks: dashmap::DashMap<BlobRequestId, Arc<AsyncMutex<()>>>,
    pub(crate) request_handles: dashmap::DashMap<BlobRequestId, u64>,
    pub(crate) pending_blob_writes: dashmap::DashMap<u64, Vec<BlobWrite>>,
    pub(crate) blob_handles: dashmap::DashMap<u64, Arc<BlobRecord>>,
    pub(crate) next_blob_id: AtomicU64,
    publication: PublicationReservations,
    fence: Arc<ResourceFence>,
    generation: ResourceGeneration,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct ResourceGeneration(u64);

#[derive(Default)]
struct ResourceFence {
    active: Mutex<u64>,
}

impl ResourceFence {
    fn prepare(&self) -> ResourceGeneration {
        ResourceGeneration(
            self.active
                .lock()
                .checked_add(1)
                .expect("resource generation cannot exhaust"),
        )
    }

    fn activate(&self, generation: ResourceGeneration) {
        let mut active = self.active.lock();
        if *active == generation.0 {
            return;
        }
        assert_eq!(
            active.checked_add(1),
            Some(generation.0),
            "candidate resource generation is not the next generation"
        );
        *active = generation.0;
    }

    fn retire(&self, generation: ResourceGeneration) {
        let mut active = self.active.lock();
        if *active == generation.0 {
            *active = active
                .checked_add(1)
                .expect("resource generation cannot exhaust");
        }
    }
}

pub(crate) struct BlobPublicationGuard<'a> {
    resources: &'a MountResources,
    operation_id: u64,
    armed: bool,
}

impl BlobPublicationGuard<'_> {
    pub(crate) fn take(mut self) -> Vec<BlobWrite> {
        self.armed = false;
        self.resources.take_blob_writes(self.operation_id)
    }
}

impl Drop for BlobPublicationGuard<'_> {
    fn drop(&mut self) {
        if self.armed {
            self.resources
                .pending_blob_writes
                .remove(&self.operation_id);
        }
    }
}

#[derive(Clone, Eq, Hash, PartialEq)]
pub(crate) enum PublicationKey {
    Path(Path),
    Object(ObjectId),
    Revalidate(ObjectId),
}

impl PublicationKey {
    fn blocks(&self, requested: &Self) -> bool {
        match (self, requested) {
            (Self::Path(holder), Self::Path(waiter)) => {
                holder == waiter || waiter.has_prefix(holder)
            },
            (Self::Object(holder), Self::Object(waiter))
            | (Self::Revalidate(holder), Self::Revalidate(waiter)) => holder == waiter,
            _ => false,
        }
    }
}

struct PublicationReservations {
    active: Mutex<HashSet<PublicationKey>>,
    wake: Notify,
}

pub(crate) struct PublicationPermit<'a> {
    reservations: &'a PublicationReservations,
    key: PublicationKey,
}

impl Drop for PublicationPermit<'_> {
    fn drop(&mut self) {
        self.reservations.active.lock().remove(&self.key);
        self.reservations.wake.notify_waiters();
    }
}

pub struct Coherence {
    pub invalidation_epoch: u64,
}

impl MountResources {
    fn new(
        caches: &Caches,
        mount: &MountName,
        projection_id: ProjectionId,
        spec_source: &[u8],
        fence: Arc<ResourceFence>,
    ) -> anyhow::Result<Arc<Self>> {
        let projection = ProjectionStore::open(
            &caches.projection_root,
            &caches.projection_database,
            projection_id,
            mount,
            spec_source,
        )?;
        Ok(Self::from_projection(caches, projection, fence))
    }

    fn from_projection(
        caches: &Caches,
        projection: ProjectionStore,
        fence: Arc<ResourceFence>,
    ) -> Arc<Self> {
        let body = Arc::clone(&caches.body);
        let generation = fence.prepare();
        Arc::new(Self {
            projection,
            body,
            memory: MemoryTier::new(),
            coherence: Mutex::new(Coherence {
                invalidation_epoch: 0,
            }),
            request_locks: dashmap::DashMap::new(),
            request_handles: dashmap::DashMap::new(),
            pending_blob_writes: dashmap::DashMap::new(),
            blob_handles: dashmap::DashMap::new(),
            next_blob_id: AtomicU64::new(1),
            publication: PublicationReservations {
                active: Mutex::new(HashSet::new()),
                wake: Notify::new(),
            },
            fence,
            generation,
        })
    }

    pub(crate) fn activate(&self) {
        self.fence.activate(self.generation);
    }

    pub(crate) fn retire(&self) {
        self.fence.retire(self.generation);
    }

    pub(crate) async fn reserve(&self, key: PublicationKey) -> PublicationPermit<'_> {
        loop {
            let mut notified = Box::pin(self.publication.wake.notified());
            notified.as_mut().enable();
            let available = {
                let mut active = self.publication.active.lock();
                if active.iter().any(|holder| holder.blocks(&key)) {
                    false
                } else {
                    active.insert(key.clone());
                    true
                }
            };
            if available {
                return PublicationPermit {
                    reservations: &self.publication,
                    key,
                };
            }
            notified.await;
        }
    }

    pub(crate) fn blob_publication(&self, operation_id: u64) -> BlobPublicationGuard<'_> {
        BlobPublicationGuard {
            resources: self,
            operation_id,
            armed: true,
        }
    }

    /// Current per-projection invalidation epoch used by publication fences.
    pub fn current_epoch(&self) -> u64 {
        self.coherence.lock().invalidation_epoch
    }

    pub(crate) fn body_for_handle(&self, handle: u64) -> Result<(BodyId, u64), ProjectionError> {
        let record = self.blob_handles.get(&handle).ok_or_else(|| {
            ProjectionError::Inconsistent(format!("unknown runtime blob handle {handle}"))
        })?;
        Ok((record.body, record.size))
    }

    pub(crate) fn blob_request_lock(&self, request: BlobRequestId) -> Arc<AsyncMutex<()>> {
        self.request_locks
            .entry(request)
            .or_insert_with(|| Arc::new(AsyncMutex::new(())))
            .clone()
    }

    pub(crate) fn blob_for_request(
        &self,
        request: BlobRequestId,
    ) -> Result<Option<Arc<BlobRecord>>, ProjectionError> {
        if let Some(handle) = self.request_handles.get(&request) {
            return Ok(self
                .blob_handles
                .get(&*handle)
                .map(|record| Arc::clone(&record)));
        }
        let Some(bytes) = self.projection.get(&blob_key(request))? else {
            return Ok(None);
        };
        let DurableFact::Blob(fact) = postcard::from_bytes(&bytes)? else {
            return Err(ProjectionError::Inconsistent(
                "blob request key contains a non-blob fact".into(),
            ));
        };
        let body = BodyId::from_digest_bytes(fact.body_id);
        let stored = self.body.read(body, Some(fact.length))?;
        if u64::try_from(stored.len()).map_err(|_| {
            ProjectionError::Inconsistent("blob body length does not fit u64".into())
        })? != fact.length
            || fact.metadata.size != fact.length
        {
            return Err(ProjectionError::Inconsistent(
                "blob fact length does not match its body".into(),
            ));
        }
        Ok(Some(self.publish_blob_handle(request, body, fact.metadata)))
    }

    pub(crate) fn publish_blob_handle(
        &self,
        request: BlobRequestId,
        body: BodyId,
        metadata: BlobMetadata,
    ) -> Arc<BlobRecord> {
        let id = self.next_blob_id.fetch_add(1, Ordering::Relaxed);
        let record = Arc::new(BlobRecord {
            id,
            body,
            size: metadata.size,
            content_type: metadata.content_type,
            etag: metadata.etag,
            status: metadata.status,
            response_headers: metadata.response_headers,
        });
        self.blob_handles.insert(id, Arc::clone(&record));
        self.request_handles.insert(request, id);
        record
    }

    pub(crate) fn stage_blob_write(
        &self,
        operation_id: u64,
        request: BlobRequestId,
        body: BodyId,
        metadata: BlobMetadata,
    ) -> Arc<BlobRecord> {
        self.pending_blob_writes
            .entry(operation_id)
            .or_default()
            .push(BlobWrite {
                request,
                body,
                metadata: metadata.clone(),
            });
        self.publish_blob_handle(request, body, metadata)
    }

    pub(crate) fn take_blob_writes(&self, operation_id: u64) -> Vec<BlobWrite> {
        self.pending_blob_writes
            .remove(&operation_id)
            .map_or_else(Vec::new, |(_, writes)| writes)
    }

    // Every publication phase shares one coherence guard and one Fjall
    // transaction, so keeping this boundary whole is the atomicity invariant.
    #[allow(clippy::too_many_lines)]
    pub(crate) fn publish(
        &self,
        transition: ProjectionTransition,
        captured_epoch: u64,
    ) -> Result<PublicationOutcome, ProjectionError> {
        let ProjectionTransition {
            records,
            dirents,
            objects,
            freshness,
            invalidations,
            blobs,
            git,
            removals: removed_paths,
            feed,
            kept,
        } = transition;
        // Bodies become immutable and addressable before the projection
        // transaction can publish a reference to them. This work intentionally
        // happens before taking the coherence mutex.
        let mut bodies = Vec::<&[u8]>::new();
        for object in &objects {
            if let ObjectMutation::Canonical { bytes, .. } = object {
                bodies.push(bytes);
            }
        }
        for record in &records {
            match &record.fact {
                FactPayload::Lookup(LookupPayload::Positive(meta)) => {
                    bodies.extend(inline_bytes(meta));
                },
                FactPayload::Attr(value) => bodies.extend(inline_bytes(&value.meta)),
                FactPayload::File(value) => bodies.push(&value.content),
                FactPayload::Lookup(LookupPayload::Negative { .. })
                | FactPayload::FileBody { .. } => {},
            }
        }
        self.body.publish_all(&bodies)?;
        let mut prepared_objects = Vec::new();
        for object in &objects {
            if let ObjectMutation::Canonical {
                id,
                bytes,
                validator,
            } = object
            {
                prepared_objects.push((
                    id.clone(),
                    self.body.publish(bytes)?,
                    bytes.len() as u64,
                    validator.clone(),
                ));
            }
        }

        let mut prepared_records = Vec::new();
        for record in &records {
            let fact = match &record.fact {
                FactPayload::Lookup(value) => {
                    DurableFact::Lookup(normalize_lookup(value, &self.body)?)
                },
                FactPayload::Attr(value) => DurableFact::Attr(normalize_attr(value, &self.body)?),
                FactPayload::File(value) => DurableFact::File {
                    version_token: value.version_token.clone(),
                    content_type: value.content_type.clone(),
                    body_id: *self.body.publish(&value.content)?.as_bytes(),
                    length: value.content.len() as u64,
                },
                FactPayload::FileBody {
                    version_token,
                    content_type,
                    body,
                    length,
                } => {
                    self.body.validate(*body, Some(*length))?;
                    DurableFact::File {
                        version_token: version_token.clone(),
                        content_type: content_type.clone(),
                        body_id: *body.as_bytes(),
                        length: *length,
                    }
                },
            };
            prepared_records.push(PreparedRecord {
                path: record.path.clone(),
                aux: record.aux.clone(),
                fact,
            });
        }

        let mut prepared_blobs = Vec::new();
        for blob in &blobs {
            self.body.validate(blob.body, Some(blob.metadata.size))?;
            prepared_blobs.push((
                blob_key(blob.request),
                DurableFact::Blob(BlobFact {
                    body_id: *blob.body.as_bytes(),
                    length: blob.metadata.size,
                    metadata: blob.metadata.clone(),
                }),
            ));
        }
        let prepared_git = git
            .iter()
            .map(|git| {
                validate_git_relative(&git.relative_path)?;
                Ok((
                    git_key(&git.path),
                    DurableFact::Git {
                        id: git.id,
                        relative_path: git.relative_path.clone(),
                        commit: git.commit.clone(),
                    },
                ))
            })
            .collect::<Result<Vec<_>, ProjectionError>>()?;
        let transition_git_paths: HashSet<Path> = git.iter().map(|git| git.path.clone()).collect();

        let active_generation = self.fence.active.lock();
        if *active_generation != self.generation.0 {
            return Ok(PublicationOutcome::Fenced);
        }
        let mut coherence = self.coherence.lock();
        if captured_epoch != coherence.invalidation_epoch {
            return Ok(PublicationOutcome::Fenced);
        }
        let mut retired = HashSet::new();
        let mut memory_paths = HashSet::new();
        let mut memory_prefixes = Vec::new();
        let mut memory_object_invalidation = false;
        for record in &records {
            memory_paths.insert(record.path.clone());
        }
        for mutation in &dirents {
            memory_paths.insert(mutation.path().clone());
        }
        for freshness in &freshness {
            memory_paths.insert(freshness.path.clone());
        }
        for invalidation in &invalidations {
            match invalidation {
                Invalidation::Object(id) => {
                    retired.insert(id.clone());
                    memory_object_invalidation = true;
                    for path in self.paths_for_id(id)? {
                        memory_paths.insert(path);
                    }
                },
                Invalidation::ListingPath(path) => {
                    memory_paths.insert(path.clone());
                },
                Invalidation::ListingPrefix(prefix) => {
                    memory_prefixes.push(prefix.clone());
                },
            }
        }
        memory_prefixes.extend(removed_paths.iter().cloned());

        let next_epoch = if invalidations.is_empty() {
            None
        } else {
            Some(coherence.invalidation_epoch.checked_add(1).ok_or_else(|| {
                ProjectionError::Inconsistent("invalidation epoch overflow".into())
            })?)
        };
        let reconciled_paths = self.projection.transact(|tx, facts| {
            let mut removals = Vec::<Vec<u8>>::new();

            let mut claims = HashMap::<String, Vec<u8>>::new();
            for object in &objects {
                match object {
                    ObjectMutation::Canonical { .. } => {},
                    ObjectMutation::Index { id, alias } => {
                        validate_claim(tx, facts, &mut claims, id, alias, &retired)?;
                    },
                }
            }

            for object in &prepared_objects {
                let mut key = b"o:".to_vec();
                key.extend_from_slice(hex::encode(&object.0).as_bytes());
                let value = postcard::to_allocvec(&(object.1.as_bytes(), object.2, &object.3))
                    .map_err(ProjectionError::from)?;
                put(tx, facts, key, value)?;
            }
            for object in &objects {
                if let ObjectMutation::Index { id, alias } = object {
                    let (body, length, validator) =
                        read_object_row(tx, facts, id)?.ok_or_else(|| {
                            ProjectionError::Inconsistent(
                                "index mutation has no durable object row".into(),
                            )
                        })?;
                    let _ = (body, length, validator);
                    insert_alias(tx, facts, id, alias)?;
                }
            }

            // A feed answer writes fed facts; any other transition leaves
            // the fed facts of a path to its feeds.
            let fed_answer = feed.is_some();
            let mut written = Vec::with_capacity(prepared_records.len());
            for record in &prepared_records {
                if fed_answer {
                    put(tx, facts, fed_key(&record.path), [])?;
                } else if tx.get(facts, fed_key(&record.path))?.is_some() {
                    continue;
                }
                written.push(record);
                remove_negative_for_path(tx, facts, &record.path)?;
                if matches!(&record.fact, DurableFact::Lookup(_))
                    && !transition_git_paths.contains(&record.path)
                {
                    delete(tx, facts, git_key(&record.path))?;
                }
                let key = fact_key(
                    record.path.as_str(),
                    record.fact.kind(),
                    record.aux.as_deref(),
                );
                let value = postcard::to_allocvec(&record.fact).map_err(ProjectionError::from)?;
                put(tx, facts, key, value)?;
                if let DurableFact::Lookup(lookup) = &record.fact
                    && let LookupPayload::Negative { id: Some(id) } = lookup
                {
                    put(
                        tx,
                        facts,
                        negative_key(id, &record.path),
                        record.path.as_str().as_bytes(),
                    )?;
                }
            }
            for (key, fact) in &prepared_blobs {
                put(
                    tx,
                    facts,
                    key.clone(),
                    postcard::to_allocvec(fact).map_err(ProjectionError::from)?,
                )?;
            }
            for (key, fact) in &prepared_git {
                put(
                    tx,
                    facts,
                    key.clone(),
                    postcard::to_allocvec(fact).map_err(ProjectionError::from)?,
                )?;
            }
            for mutation in &dirents {
                if fed_answer {
                    put(tx, facts, fed_key(mutation.path()), [])?;
                    apply_dirents(tx, facts, mutation)?;
                } else {
                    let mutation = with_fed_entries(tx, facts, mutation)?;
                    apply_dirents(tx, facts, &mutation)?;
                }
            }
            if let Some(feed) = &feed {
                write_feed_states(tx, facts, feed)?;
            }
            if let Some(kept) = &kept {
                let prefix = kept_prefix(&kept.object);
                let next = tx.prefix(facts, prefix.as_slice()).count();
                put(
                    tx,
                    facts,
                    kept_key(&kept.object, next),
                    kept.part.as_slice(),
                )?;
            }
            for freshness in &freshness {
                if !fed_answer && tx.get(facts, fed_key(&freshness.path))?.is_some() {
                    continue;
                }
                let mut expiry = b"x:".to_vec();
                expiry.extend_from_slice(freshness.path.as_str().as_bytes());
                put(
                    tx,
                    facts,
                    expiry,
                    postcard::to_allocvec(&freshness.expires_at).map_err(ProjectionError::from)?,
                )?;
            }
            let mut reconciled_paths =
                reconcile_listing_relations(tx, facts, &written, &dirents, &mut removals)?;
            // Apply invalidation scanners after all transition writes. Fjall's
            // transaction reads see those writes, so same-terminal aliases,
            // records, Git facts, and negative reverse rows are retired too.
            for id in &retired {
                remove_object_facts(tx, facts, id, &mut removals)?;
            }
            for invalidation in &invalidations {
                match invalidation {
                    Invalidation::ListingPath(path) => {
                        remove_path_facts(tx, facts, path, &mut removals)?;
                        if let Some(parent) = invalidate_parent_listing(tx, facts, path)? {
                            reconciled_paths.insert(parent);
                        }
                    },
                    Invalidation::ListingPrefix(prefix) => {
                        for path in remove_prefix_facts(tx, facts, prefix, &mut removals)? {
                            if let Some((parent, _)) = path.parent_and_name()
                                && !parent.has_prefix(prefix)
                                && let Some(parent) = invalidate_parent_listing(tx, facts, &path)?
                            {
                                reconciled_paths.insert(parent);
                            }
                        }
                    },
                    Invalidation::Object(_) => {},
                }
            }
            for path in &removed_paths {
                reconciled_paths.extend(remove_subtree_facts(tx, facts, path, &mut removals)?);
                if let Some(parent) = remove_from_parent_listing(tx, facts, path)? {
                    reconciled_paths.insert(parent);
                }
            }
            for key in removals {
                delete(tx, facts, key)?;
            }
            Ok(reconciled_paths)
        })?;

        if let Some(next_epoch) = next_epoch {
            coherence.invalidation_epoch = next_epoch;
        }
        memory_paths.extend(reconciled_paths);
        if memory_object_invalidation {
            self.memory.invalidate_prefix(&Path::root());
        } else {
            for path in memory_paths {
                self.memory.delete_exact(&path);
            }
        }
        for prefix in memory_prefixes {
            self.memory.invalidate_prefix(&prefix);
        }
        Ok(PublicationOutcome::Committed { invalidations })
    }

    // --- View cache reads -----------------------------------------------------

    pub(crate) fn cache_get(
        &self,
        path: &Path,
        kind: RecordKind,
        aux: Option<&str>,
    ) -> Result<Option<Record>, ProjectionError> {
        let memory_key = Key::with_aux(path, kind, aux);
        if let Some(record) = self.memory.mem_get(&memory_key) {
            validate_fact_payload(kind, &record.payload)?;
            return Ok(Some((*record).clone()));
        }
        let bytes = self
            .projection
            .get(&fact_key(path.as_str(), kind, aux))?
            .map(|value| (value, memory_key));
        let Some((bytes, memory_key)) = bytes else {
            return Ok(None);
        };
        let fact: DurableFact = postcard::from_bytes(&bytes)?;
        let record = match (kind, fact) {
            (RecordKind::Lookup, DurableFact::Lookup(value)) => Record::new(
                kind,
                value.serialize().ok_or_else(|| {
                    ProjectionError::Inconsistent("lookup fact could not be encoded".into())
                })?,
            ),
            (RecordKind::Attr, DurableFact::Attr(value)) => Record::new(
                kind,
                value.serialize().ok_or_else(|| {
                    ProjectionError::Inconsistent("attribute fact could not be encoded".into())
                })?,
            ),
            (RecordKind::Dirents, DurableFact::Dirents(value)) => Record::new(
                kind,
                value.serialize().ok_or_else(|| {
                    ProjectionError::Inconsistent("dirents fact could not be encoded".into())
                })?,
            ),
            (
                RecordKind::File,
                DurableFact::File {
                    version_token,
                    content_type,
                    body_id,
                    length,
                },
            ) => {
                let body = self
                    .body
                    .read(BodyId::from_digest_bytes(body_id), Some(length))?;
                let payload = FilePayload::new(version_token, body).with_content_type(content_type);
                Record::new(
                    kind,
                    payload.serialize().ok_or_else(|| {
                        ProjectionError::Inconsistent("file fact could not be encoded".into())
                    })?,
                )
            },
            _ => {
                return Err(ProjectionError::Inconsistent(
                    "durable fact kind does not match its path key".into(),
                ));
            },
        };
        self.memory.mem_put(&memory_key, &record);
        Ok(Some(record))
    }

    pub(crate) fn lookup_payload(
        &self,
        path: &Path,
    ) -> Result<Option<LookupPayload>, ProjectionError> {
        self.cache_get(path, RecordKind::Lookup, None)?
            .map(|record| postcard::from_bytes(&record.payload).map_err(ProjectionError::from))
            .transpose()
    }

    pub(crate) fn dirents_payload(
        &self,
        path: &Path,
    ) -> Result<Option<DirentsPayload>, ProjectionError> {
        self.cache_get(path, RecordKind::Dirents, None)?
            .map(|record| postcard::from_bytes(&record.payload).map_err(ProjectionError::from))
            .transpose()
    }

    pub(crate) fn read_body(&self, body: BodyId, length: u64) -> Result<Vec<u8>, ProjectionError> {
        self.body.read(body, Some(length)).map_err(Into::into)
    }

    pub(crate) fn memory_get(
        &self,
        path: &Path,
        kind: RecordKind,
        aux: Option<&str>,
    ) -> Option<Arc<Record>> {
        self.memory.mem_get(&Key::with_aux(path, kind, aux))
    }

    pub(crate) fn memory_invalidate(&self, path: &Path, kind: RecordKind, aux: Option<&str>) {
        self.memory.mem_invalidate(&Key::with_aux(path, kind, aux));
    }

    pub(crate) fn memory_invalidate_paths_if<P>(&self, predicate: P)
    where
        P: Fn(&Path) -> bool + Send + Sync + 'static,
    {
        self.memory.mem_invalidate_paths_if(predicate);
    }
    // --- Canonical object cache -----------------------------------------------

    /// Warm-read input: path → id → bytes + validator. Returns the raw object
    /// id. `None` when no canonical is indexed.
    pub(crate) fn cached_canonical_for(
        &self,
        path: &Path,
    ) -> Result<Option<CachedCanonical>, ProjectionError> {
        let Some(id) = self.id_of_path(path)? else {
            return Ok(None);
        };
        let Some((body, length, validator)) = read_object_row_direct(&self.projection, &id)? else {
            return Err(ProjectionError::Inconsistent(
                "path index points to a missing object row".into(),
            ));
        };
        let bytes = self.body.read(body, Some(length))?;
        Ok(Some(CachedCanonical {
            id,
            bytes,
            validator,
        }))
    }

    /// Resolve the canonical selected for `path` by the transition currently
    /// being lowered, falling back to the durable selection when the terminal
    /// does not replace it. This keeps cold object reads within one atomic
    /// publication instead of requiring an index write before the rest of the
    /// provider terminal can be validated and committed.
    pub(crate) fn selected_canonical_for(
        &self,
        path: &Path,
        objects: &[ObjectMutation],
    ) -> Result<Option<CachedCanonical>, ProjectionError> {
        let Some(id) = objects.iter().rev().find_map(|mutation| match mutation {
            ObjectMutation::Index { id, alias } if alias == path => Some(id),
            ObjectMutation::Canonical { .. } | ObjectMutation::Index { .. } => None,
        }) else {
            return self.cached_canonical_for(path);
        };

        if let Some((bytes, validator)) = objects.iter().rev().find_map(|mutation| match mutation {
            ObjectMutation::Canonical {
                id: candidate,
                bytes,
                validator,
            } if candidate == id => Some((bytes, validator)),
            ObjectMutation::Canonical { .. } | ObjectMutation::Index { .. } => None,
        }) {
            return Ok(Some(CachedCanonical {
                id: id.clone(),
                bytes: bytes.clone(),
                validator: validator.clone(),
            }));
        }

        let Some((body, length, validator)) = read_object_row_direct(&self.projection, id)? else {
            return Ok(None);
        };
        Ok(Some(CachedCanonical {
            id: id.clone(),
            bytes: self.body.read(body, Some(length))?,
            validator,
        }))
    }

    /// Forward index: path → object id bytes.
    pub(crate) fn id_of_path(&self, path: &Path) -> Result<Option<Vec<u8>>, ProjectionError> {
        Ok(self.projection.get(&index_key(path))?)
    }

    /// Reverse index: object id bytes → current alias paths.
    pub(crate) fn paths_for_id(&self, id: &[u8]) -> Result<Vec<Path>, ProjectionError> {
        let prefix = alias_prefix(id);
        self.projection
            .read_prefix(&prefix)?
            .into_iter()
            .map(|key| {
                let path = key.get(prefix.len()..).ok_or_else(|| {
                    ProjectionError::Inconsistent("reverse alias key is truncated".into())
                })?;
                let path = std::str::from_utf8(path).map_err(|_| {
                    ProjectionError::Inconsistent("reverse alias path is not UTF-8".into())
                })?;
                Path::parse(path).map_err(|error| ProjectionError::Inconsistent(error.to_string()))
            })
            .collect()
    }

    pub(crate) fn git_for_path(&self, path: &Path) -> Result<Option<GitFact>, ProjectionError> {
        let Some(bytes) = self.projection.get(&git_key(path))? else {
            return Ok(None);
        };
        let DurableFact::Git {
            id,
            relative_path,
            commit,
        } = postcard::from_bytes(&bytes)?
        else {
            return Err(ProjectionError::Inconsistent(
                "Git fact key contains a non-Git fact".into(),
            ));
        };
        validate_git_relative(&relative_path)?;
        Ok(Some(GitFact {
            id,
            relative_path,
            commit,
        }))
    }

    /// The state of the feed of the object `id`.
    pub(crate) fn feed_state(&self, id: &[u8]) -> Result<Option<FeedState>, ProjectionError> {
        self.projection
            .get(&feed_key(id))?
            .map(|bytes| postcard::from_bytes(&bytes).map_err(ProjectionError::from))
            .transpose()
    }

    /// Whether the facts of `path` are fed: they come from feeds alone.
    pub(crate) fn is_fed(&self, path: &Path) -> Result<bool, ProjectionError> {
        Ok(self.projection.get(&fed_key(path))?.is_some())
    }

    /// Whether the feed of the object `id` has kept parts of a later answer.
    pub(crate) fn has_kept_parts(&self, id: &[u8]) -> Result<bool, ProjectionError> {
        Ok(!self.projection.read_prefix(&kept_prefix(id))?.is_empty())
    }

    /// The kept parts of the later answer of the feed of the object `id`, in
    /// their order.
    pub(crate) fn kept_parts(&self, id: &[u8]) -> Result<Vec<Vec<u8>>, ProjectionError> {
        let mut keys = self.projection.read_prefix(&kept_prefix(id))?;
        keys.sort();
        keys.iter()
            .map(|key| {
                self.projection.get(key)?.ok_or_else(|| {
                    ProjectionError::Inconsistent("a kept part left during its read".into())
                })
            })
            .collect()
    }

    /// The anchor of the claim of the object whose anchor of another facet is
    /// `path`.
    pub(crate) fn facet_anchor(&self, path: &Path) -> Result<Option<Path>, ProjectionError> {
        self.projection
            .get(&anchor_key(path))?
            .map(|bytes| {
                let anchor: String = postcard::from_bytes(&bytes)?;
                Path::parse(&anchor)
                    .map_err(|error| ProjectionError::Inconsistent(error.to_string()))
            })
            .transpose()
    }

    /// Whether the indexed view leaf has reached its freshness deadline.
    pub(crate) fn view_expired(
        &self,
        path: &Path,
        now_millis: u64,
    ) -> Result<bool, ProjectionError> {
        let Some(bytes) = self.projection.get(&expiry_key(path))? else {
            return Ok(false);
        };
        let expiry: Option<u64> = postcard::from_bytes(&bytes)?;
        Ok(expiry.is_some_and(|deadline| deadline <= now_millis))
    }

    /// Expiry-aware view read: returns `None` when the leaf is past its deadline.
    pub(crate) fn view_get(
        &self,
        path: &Path,
        kind: RecordKind,
        aux: Option<&str>,
        now_millis: u64,
    ) -> Result<Option<Record>, ProjectionError> {
        if self.view_expired(path, now_millis)? {
            return Ok(None);
        }
        self.cache_get(path, kind, aux)
    }

    /// Live negative for `path`. `None` when absent or expired.
    pub(crate) fn negative_for_checked(
        &self,
        path: &Path,
        now_millis: u64,
    ) -> Result<bool, ProjectionError> {
        let Some(record) = self.cache_get(path, RecordKind::Lookup, None)? else {
            return Ok(false);
        };
        let payload: LookupPayload = postcard::from_bytes(&record.payload)?;
        let LookupPayload::Negative { .. } = payload else {
            return Ok(false);
        };
        let expiry = self
            .projection
            .get(&expiry_key(path))?
            .map(|bytes| postcard::from_bytes::<Option<u64>>(&bytes))
            .transpose()?
            .flatten();
        if expiry.is_some_and(|deadline| now_millis >= deadline) {
            return Ok(false);
        }
        Ok(true)
    }
}

/// Write `value` at `key` unless the projection has it there, so that a
/// publication without a change commits nothing.
fn put(
    tx: &mut fjall::OptimisticWriteTx,
    facts: &fjall::OptimisticTxKeyspace,
    key: impl AsRef<[u8]>,
    value: impl AsRef<[u8]>,
) -> Result<(), ProjectionError> {
    if tx.get(facts, key.as_ref())?.as_deref() != Some(value.as_ref()) {
        tx.insert(facts, key.as_ref(), value.as_ref());
    }
    Ok(())
}

/// Remove `key` unless the projection lacks it, as [`put`] writes.
fn delete(
    tx: &mut fjall::OptimisticWriteTx,
    facts: &fjall::OptimisticTxKeyspace,
    key: impl AsRef<[u8]>,
) -> Result<(), ProjectionError> {
    if tx.get(facts, key.as_ref())?.is_some() {
        tx.remove(facts, key.as_ref());
    }
    Ok(())
}

fn fact_key(path: &str, kind: RecordKind, aux: Option<&str>) -> Vec<u8> {
    let key = Key::with_aux(&Path::from_validated(path.to_owned()), kind, aux).wire_key();
    let mut result = b"r:".to_vec();
    result.extend_from_slice(key.as_bytes());
    result
}

fn expiry_key(path: &Path) -> Vec<u8> {
    let mut key = b"x:".to_vec();
    key.extend_from_slice(path.as_str().as_bytes());
    key
}

fn object_key(id: &[u8]) -> Vec<u8> {
    let mut key = b"o:".to_vec();
    key.extend_from_slice(hex::encode(id).as_bytes());
    key
}

fn read_object_row(
    tx: &mut fjall::OptimisticWriteTx,
    facts: &fjall::OptimisticTxKeyspace,
    id: &[u8],
) -> Result<Option<(BodyId, u64, Option<String>)>, ProjectionError> {
    let Some(bytes) = tx.get(facts, object_key(id))? else {
        return Ok(None);
    };
    let (body, length, validator): ([u8; 32], u64, Option<String>) = postcard::from_bytes(&bytes)?;
    Ok(Some((BodyId::from_digest_bytes(body), length, validator)))
}

fn read_object_row_direct(
    projection: &ProjectionStore,
    id: &[u8],
) -> Result<Option<(BodyId, u64, Option<String>)>, ProjectionError> {
    let Some(bytes) = projection.get(&object_key(id))? else {
        return Ok(None);
    };
    let (body, length, validator): ([u8; 32], u64, Option<String>) = postcard::from_bytes(&bytes)?;
    Ok(Some((BodyId::from_digest_bytes(body), length, validator)))
}

fn validate_fact_payload(kind: RecordKind, bytes: &[u8]) -> Result<(), ProjectionError> {
    let result = match kind {
        RecordKind::Lookup => postcard::from_bytes::<LookupPayload>(bytes).map(|_| ()),
        RecordKind::Attr => postcard::from_bytes::<AttrPayload>(bytes).map(|_| ()),
        RecordKind::Dirents => postcard::from_bytes::<DirentsPayload>(bytes).map(|_| ()),
        RecordKind::File => postcard::from_bytes::<FilePayload>(bytes).map(|_| ()),
    };
    result.map_err(|error| {
        ProjectionError::Inconsistent(format!("durable fact payload is corrupt: {error}"))
    })
}

fn normalize_lookup(
    value: &LookupPayload,
    body: &BodyStore,
) -> Result<LookupPayload, ProjectionError> {
    Ok(match value {
        LookupPayload::Positive(meta) => LookupPayload::Positive(normalize_meta(meta, body)?),
        LookupPayload::Negative { id } => LookupPayload::Negative { id: id.clone() },
    })
}

fn normalize_attr(value: &AttrPayload, body: &BodyStore) -> Result<AttrPayload, ProjectionError> {
    Ok(AttrPayload {
        meta: normalize_meta(&value.meta, body)?,
    })
}

/// The inline bytes of the file of `meta`, which its publication stores as a
/// body.
fn inline_bytes(meta: &EntryMeta) -> Option<&[u8]> {
    let EntryMeta::File { attrs: Some(attrs) } = meta else {
        return None;
    };
    attrs.inline_bytes()
}

fn normalize_meta(meta: &EntryMeta, body: &BodyStore) -> Result<EntryMeta, ProjectionError> {
    let EntryMeta::File { attrs: Some(attrs) } = meta else {
        return Ok(meta.clone());
    };
    let attrs = if let Some(bytes) = attrs.inline_bytes() {
        let body_id = body.publish(bytes)?;
        let length = u64::try_from(bytes.len()).map_err(|_| {
            ProjectionError::Inconsistent("inline body length does not fit u64".into())
        })?;
        FileAttrsCache::from_parts(
            FileSize::Exact(length),
            ByteSource::Body(body_id),
            attrs.stability(),
            attrs.version_token_owned(),
        )
        .map_err(ProjectionError::Inconsistent)?
    } else {
        if let ByteSource::Body(body_id) = attrs.byte_source() {
            let expected = match attrs.size() {
                FileSize::Exact(length) => Some(length),
                FileSize::NonZero | FileSize::Unknown => None,
            };
            body.validate(body_id, expected)?;
        }
        attrs.clone()
    };
    Ok(EntryMeta::file(attrs))
}

fn feed_key(id: &[u8]) -> Vec<u8> {
    let mut key = b"e:".to_vec();
    key.extend_from_slice(hex::encode(id).as_bytes());
    key
}

fn fed_key(path: &Path) -> Vec<u8> {
    let mut key = b"f:".to_vec();
    key.extend_from_slice(path.as_str().as_bytes());
    key
}

/// The key of the anchor of the claim of the object whose anchor of another
/// facet is `path`.
fn anchor_key(path: &Path) -> Vec<u8> {
    let mut key = b"a:".to_vec();
    key.extend_from_slice(path.as_str().as_bytes());
    key
}

/// The prefix of the keys of the kept parts of the feed of the object `id`.
fn kept_prefix(id: &[u8]) -> Vec<u8> {
    let mut key = b"k:".to_vec();
    key.extend_from_slice(hex::encode(id).as_bytes());
    key.push(b':');
    key
}

/// The key of the kept part `index` of the feed of the object `id`, which
/// sorts in the order of the parts.
fn kept_key(id: &[u8], index: usize) -> Vec<u8> {
    let mut key = kept_prefix(id);
    key.extend_from_slice(format!("{index:08x}").as_bytes());
    key
}

fn write_feed_states(
    tx: &mut fjall::OptimisticWriteTx,
    facts: &fjall::OptimisticTxKeyspace,
    feed: &FeedWrite,
) -> Result<(), ProjectionError> {
    let key = feed_key(&feed.object);
    let previous = tx
        .get(facts, &key)?
        .map(|bytes| postcard::from_bytes::<FeedState>(&bytes))
        .transpose()?;
    let state = match (&feed.part, previous) {
        (Some(part), previous) => FeedState {
            token: part.token.clone(),
            ended: previous.is_some_and(|state| state.ended) || !part.more,
            deadline: feed.deadline,
        },
        (None, Some(previous)) => FeedState {
            deadline: feed.deadline,
            ..previous
        },
        (None, None) => {
            return Err(ProjectionError::Inconsistent(
                "unchanged answer for a feed without a state".into(),
            ));
        },
    };
    put(tx, facts, key, postcard::to_allocvec(&state)?)?;
    if feed.part.is_some() {
        let prefix = kept_prefix(&feed.object);
        let kept = tx
            .prefix(facts, prefix.as_slice())
            .map(|guard| Ok(guard.key()?.to_vec()))
            .collect::<Result<Vec<_>, ProjectionError>>()?;
        for key in kept {
            delete(tx, facts, key)?;
        }
    }
    for (path, anchor) in &feed.anchors {
        put(
            tx,
            facts,
            anchor_key(path),
            postcard::to_allocvec(anchor.as_str())?,
        )?;
    }
    for (id, token) in &feed.inner {
        put(
            tx,
            facts,
            feed_key(id),
            postcard::to_allocvec(&FeedState {
                token: token.clone(),
                ended: true,
                deadline: feed.deadline,
            })?,
        )?;
    }
    Ok(())
}

/// A listing mutation of a transition other than a feed answer, with the
/// entries of fed children carrying their fed facts.
fn with_fed_entries(
    tx: &mut fjall::OptimisticWriteTx,
    facts: &fjall::OptimisticTxKeyspace,
    mutation: &DirentsMutation,
) -> Result<DirentsMutation, ProjectionError> {
    let mut mutation = mutation.clone();
    let parent = mutation.path().clone();
    let entries: Vec<&mut crate::view::DirentRecord> = match &mut mutation {
        DirentsMutation::Replace { value, .. } => value
            .pages
            .iter_mut()
            .flat_map(|page| page.entries.iter_mut())
            .collect(),
        DirentsMutation::MergeHints { entries, .. } => entries.iter_mut().collect(),
        DirentsMutation::AppendPage { page, .. } | DirentsMutation::ReplacePage { page, .. } => {
            page.entries.iter_mut().collect()
        },
        DirentsMutation::RenewPage { .. } => Vec::new(),
    };
    for entry in entries {
        let child = parent
            .join(&entry.name)
            .map_err(|error| ProjectionError::Inconsistent(error.to_string()))?;
        if tx.get(facts, fed_key(&child))?.is_none() {
            continue;
        }
        if let Some(bytes) = tx.get(facts, fact_key(child.as_str(), RecordKind::Lookup, None))?
            && let DurableFact::Lookup(LookupPayload::Positive(meta)) =
                postcard::from_bytes(&bytes)?
        {
            entry.meta = meta;
        }
    }
    Ok(mutation)
}

/// Remove the facts of `root` and of every path beneath it, returning the
/// paths whose facts left.
fn remove_subtree_facts(
    tx: &mut fjall::OptimisticWriteTx,
    facts: &fjall::OptimisticTxKeyspace,
    root: &Path,
    removals: &mut Vec<Vec<u8>>,
) -> Result<HashSet<Path>, ProjectionError> {
    let mut paths = HashSet::new();
    for kind in RecordKind::ALL {
        let prefix = fact_key(root.as_str(), kind, None);
        for guard in tx.prefix(facts, prefix.as_slice()) {
            let key = guard.key()?.to_vec();
            let (path, _, _) = decode_fact_key(&key)?;
            if path.has_prefix(root) {
                paths.insert(path);
            }
        }
    }
    let git_prefix = git_key(root);
    for guard in tx.prefix(facts, git_prefix.as_slice()) {
        let key = guard.key()?.to_vec();
        let path = Path::parse(
            std::str::from_utf8(&key[2..])
                .map_err(|_| ProjectionError::Inconsistent("Git fact path is not UTF-8".into()))?,
        )
        .map_err(|error| ProjectionError::Inconsistent(error.to_string()))?;
        if path.has_prefix(root) {
            paths.insert(path);
        }
    }
    for path in &paths {
        remove_path_facts(tx, facts, path, removals)?;
    }
    Ok(paths)
}

/// Remove `path` from its parent listing, which stays as complete as it was.
fn remove_from_parent_listing(
    tx: &mut fjall::OptimisticWriteTx,
    facts: &fjall::OptimisticTxKeyspace,
    path: &Path,
) -> Result<Option<Path>, ProjectionError> {
    let Some((parent, name)) = path.parent_and_name() else {
        return Ok(None);
    };
    let Some(mut listing) = read_dirents_fact(tx, facts, &parent)? else {
        return Ok(None);
    };
    if !listing.entries().any(|entry| entry.name == name) {
        return Ok(None);
    }
    listing.remove_entry(name);
    apply_dirents(
        tx,
        facts,
        &DirentsMutation::Replace {
            path: parent.clone(),
            value: listing,
        },
    )?;
    Ok(Some(parent))
}

fn index_key(path: &Path) -> Vec<u8> {
    let mut key = b"i:".to_vec();
    key.extend_from_slice(path.as_str().as_bytes());
    key
}

fn git_key(path: &Path) -> Vec<u8> {
    let mut key = b"g:".to_vec();
    key.extend_from_slice(path.as_str().as_bytes());
    key
}

fn validate_git_relative(value: &str) -> Result<(), ProjectionError> {
    if value.is_empty() {
        return Ok(());
    }
    let path = std::path::Path::new(value);
    if path.is_absolute()
        || path.components().any(|component| {
            matches!(
                component,
                std::path::Component::Prefix(_)
                    | std::path::Component::RootDir
                    | std::path::Component::ParentDir
                    | std::path::Component::CurDir
            )
        })
    {
        return Err(ProjectionError::Inconsistent(
            "Git fact contains an invalid relative path".into(),
        ));
    }
    Ok(())
}

fn negative_key(id: &[u8], path: &Path) -> Vec<u8> {
    let mut key = b"n:".to_vec();
    key.extend_from_slice(hex::encode(id).as_bytes());
    key.push(b':');
    key.extend_from_slice(path.as_str().as_bytes());
    key
}

fn alias_prefix(id: &[u8]) -> Vec<u8> {
    let mut key = b"a:".to_vec();
    key.extend_from_slice(hex::encode(id).as_bytes());
    key.push(b':');
    key
}

fn insert_alias(
    tx: &mut fjall::OptimisticWriteTx,
    facts: &fjall::OptimisticTxKeyspace,
    id: &[u8],
    path: &Path,
) -> Result<(), ProjectionError> {
    put(tx, facts, index_key(path), id)?;
    let mut alias = alias_prefix(id);
    alias.extend_from_slice(path.as_str().as_bytes());
    put(tx, facts, alias, [])?;
    Ok(())
}

fn validate_claim(
    tx: &mut fjall::OptimisticWriteTx,
    facts: &fjall::OptimisticTxKeyspace,
    claims: &mut HashMap<String, Vec<u8>>,
    id: &[u8],
    path: &Path,
    retired: &HashSet<Vec<u8>>,
) -> Result<(), ProjectionError> {
    if let Some(existing) = tx.get(facts, index_key(path))? {
        let existing = existing.to_vec();
        if existing != id && !retired.contains(&existing) {
            return Err(ProjectionError::ClaimConflict);
        }
    }
    if let Some(existing) = claims.insert(path.as_str().to_owned(), id.to_vec())
        && existing != id
    {
        return Err(ProjectionError::ClaimConflict);
    }
    Ok(())
}

fn remove_object_facts(
    tx: &mut fjall::OptimisticWriteTx,
    facts: &fjall::OptimisticTxKeyspace,
    id: &[u8],
    removals: &mut Vec<Vec<u8>>,
) -> Result<(), ProjectionError> {
    let prefix = alias_prefix(id);
    for guard in tx.prefix(facts, prefix.as_slice()) {
        let alias = guard.key()?.to_vec();
        let path =
            Path::parse(std::str::from_utf8(&alias[prefix.len()..]).map_err(|_| {
                ProjectionError::Inconsistent("object alias path is not UTF-8".into())
            })?)
            .map_err(|error| ProjectionError::Inconsistent(error.to_string()))?;
        let current_id = tx.get(facts, index_key(&path))?;
        if current_id.as_deref().is_none_or(|current| current == id) {
            remove_path_facts(tx, facts, &path, removals)?;
            removals.push(index_key(&path));
        }
        removals.push(alias);
    }
    let mut object = b"o:".to_vec();
    object.extend_from_slice(hex::encode(id).as_bytes());
    removals.push(object);
    remove_negative_for_id(tx, facts, id, removals)?;
    Ok(())
}

fn remove_negative_for_id(
    tx: &mut fjall::OptimisticWriteTx,
    facts: &fjall::OptimisticTxKeyspace,
    id: &[u8],
    removals: &mut Vec<Vec<u8>>,
) -> Result<(), ProjectionError> {
    let mut prefix = b"n:".to_vec();
    prefix.extend_from_slice(hex::encode(id).as_bytes());
    prefix.push(b':');
    for guard in tx.prefix(facts, &prefix) {
        let key = guard.key()?.to_vec();
        let path_bytes = key.get(prefix.len()..).ok_or_else(|| {
            ProjectionError::Inconsistent("negative reverse key is truncated".into())
        })?;
        let path = Path::parse(std::str::from_utf8(path_bytes).map_err(|_| {
            ProjectionError::Inconsistent("negative reverse path is not UTF-8".into())
        })?)
        .map_err(|error| ProjectionError::Inconsistent(error.to_string()))?;
        removals.push(key);
        removals.push(fact_key(path.as_str(), RecordKind::Lookup, None));
        let mut expiry = b"x:".to_vec();
        expiry.extend_from_slice(path.as_str().as_bytes());
        removals.push(expiry);
    }
    Ok(())
}

fn remove_negative_for_path(
    tx: &mut fjall::OptimisticWriteTx,
    facts: &fjall::OptimisticTxKeyspace,
    path: &Path,
) -> Result<(), ProjectionError> {
    let key = fact_key(path.as_str(), RecordKind::Lookup, None);
    let Some(bytes) = tx.get(facts, &key)? else {
        return Ok(());
    };
    let fact: DurableFact = postcard::from_bytes(&bytes)?;
    let DurableFact::Lookup(lookup) = fact else {
        return Err(ProjectionError::Inconsistent(
            "lookup key contains a non-lookup fact".into(),
        ));
    };
    if let LookupPayload::Negative { id: Some(id) } = lookup {
        delete(tx, facts, negative_key(&id, path))?;
    }
    Ok(())
}

fn remove_path_facts(
    tx: &mut fjall::OptimisticWriteTx,
    facts: &fjall::OptimisticTxKeyspace,
    path: &Path,
    removals: &mut Vec<Vec<u8>>,
) -> Result<(), ProjectionError> {
    remove_negative_for_path(tx, facts, path)?;
    for kind in RecordKind::ALL {
        let prefix = fact_key(path.as_str(), kind, None);
        for guard in tx.prefix(facts, prefix.as_slice()) {
            let key = guard.key()?.to_vec();
            let (key_path, _, _) = decode_fact_key(&key)?;
            if key_path == *path {
                removals.push(key);
            }
        }
    }
    let mut expiry = b"x:".to_vec();
    expiry.extend_from_slice(path.as_str().as_bytes());
    removals.push(expiry);
    removals.push(git_key(path));
    removals.push(fed_key(path));
    removals.push(anchor_key(path));
    Ok(())
}

fn remove_prefix_facts(
    tx: &mut fjall::OptimisticWriteTx,
    facts: &fjall::OptimisticTxKeyspace,
    prefix_path: &Path,
    removals: &mut Vec<Vec<u8>>,
) -> Result<HashSet<Path>, ProjectionError> {
    let prefix = b"r:";
    let mut paths = HashSet::new();
    for guard in tx.prefix(facts, prefix) {
        let key = guard.key()?.to_vec();
        let (path, _, _) = decode_fact_key(&key)?;
        if path.has_prefix(prefix_path) {
            paths.insert(path);
        }
    }
    let git_prefix = b"g:".to_vec();
    for guard in tx.prefix(facts, &git_prefix) {
        let key = guard.key()?.to_vec();
        let path_bytes = key
            .get(git_prefix.len()..)
            .ok_or_else(|| ProjectionError::Inconsistent("Git fact key is truncated".into()))?;
        let path = Path::parse(
            std::str::from_utf8(path_bytes)
                .map_err(|_| ProjectionError::Inconsistent("Git fact path is not UTF-8".into()))?,
        )
        .map_err(|error| ProjectionError::Inconsistent(error.to_string()))?;
        if path.has_prefix(prefix_path) {
            paths.insert(path);
        }
    }
    for path in &paths {
        remove_path_facts(tx, facts, path, removals)?;
    }
    Ok(paths)
}

fn invalidate_parent_listing(
    tx: &mut fjall::OptimisticWriteTx,
    facts: &fjall::OptimisticTxKeyspace,
    path: &Path,
) -> Result<Option<Path>, ProjectionError> {
    let Some((parent, name)) = path.parent_and_name() else {
        return Ok(None);
    };
    let Some(listing) = read_dirents_fact(tx, facts, &parent)? else {
        return Ok(None);
    };
    // An exact child invalidation makes the parent's prior completeness claim,
    // cursor, and page validators stale: the remaining entries collapse into
    // one first page without a validator. The next ordinary browse refetches
    // the parent; rate-limited serve-stale may retain unaffected siblings.
    let entries = listing
        .entries()
        .filter(|entry| entry.name != name)
        .cloned()
        .collect();
    apply_dirents(
        tx,
        facts,
        &DirentsMutation::Replace {
            path: parent.clone(),
            value: DirentsPayload::single_page(entries, false, None),
        },
    )?;
    Ok(Some(parent))
}

// Lookup and listing reconciliation is one transaction scan. Splitting it
// would obscure which mutations participate in the same durable update.
#[allow(clippy::too_many_lines)]
fn reconcile_listing_relations(
    tx: &mut fjall::OptimisticWriteTx,
    facts: &fjall::OptimisticTxKeyspace,
    records: &[&PreparedRecord],
    dirents: &[DirentsMutation],
    removals: &mut Vec<Vec<u8>>,
) -> Result<HashSet<Path>, ProjectionError> {
    let mut mutated = HashSet::new();
    // Exact lookup results and a parent's accumulated listing are one logical
    // projection. A positive hint may extend a snapshot but cannot silently
    // preserve an exhaustive claim; a definitive negative removes the name.
    for record in records {
        let DurableFact::Lookup(lookup) = &record.fact else {
            continue;
        };
        let Some((parent, name)) = record.path.parent_and_name() else {
            continue;
        };
        let Some(mut listing) = read_dirents_fact(tx, facts, &parent)? else {
            continue;
        };
        match lookup {
            LookupPayload::Positive(meta) => {
                let mut entries = BTreeMap::new();
                entries.insert(
                    name.to_string(),
                    crate::view::DirentRecord {
                        name: name.to_string(),
                        meta: meta.clone(),
                    },
                );
                listing = DirentsPayload::merged(Some(listing), entries, ListingWrite::Partial);
                apply_dirents(
                    tx,
                    facts,
                    &DirentsMutation::Replace {
                        path: parent.clone(),
                        value: listing,
                    },
                )?;
                mutated.insert(parent);
            },
            LookupPayload::Negative { .. } if listing.entries().any(|entry| entry.name == name) => {
                listing.remove_entry(name);
                apply_dirents(
                    tx,
                    facts,
                    &DirentsMutation::Replace {
                        path: parent.clone(),
                        value: listing,
                    },
                )?;
                mutated.insert(parent);
            },
            LookupPayload::Negative { .. } => {},
        }
    }

    // When a listing transition reaches a complete state, its omissions and
    // inclusions retire contradictory direct-child lookup facts in the same
    // transaction. Object index rows remain independently owned.
    let touched: HashSet<Path> = dirents
        .iter()
        .map(|mutation| mutation.path().clone())
        .collect();
    for parent in touched {
        let Some(listing) = read_dirents_fact(tx, facts, &parent)? else {
            continue;
        };
        if !listing.is_complete_offline() {
            continue;
        }
        let names: HashSet<&str> = listing.entries().map(|entry| entry.name.as_str()).collect();
        let mut contradictions = Vec::new();
        for (path, lookup) in child_lookups(tx, facts, &parent)? {
            let Some((_, name)) = path.parent_and_name() else {
                continue;
            };
            let contradicts = match lookup {
                LookupPayload::Positive(_) => !names.contains(name),
                LookupPayload::Negative { .. } => names.contains(name),
            };
            if contradicts {
                contradictions.push(path);
            }
        }
        for path in contradictions {
            remove_path_facts(tx, facts, &path, removals)?;
            mutated.insert(path);
        }
    }
    Ok(mutated)
}

/// The lookup facts of the direct children of `parent`. The keys of the
/// lookup facts are in the order of their paths, so the facts below a child
/// follow its own: each read starts after the facts below the child before
/// it, and the reads are as many as the children, whatever is below them.
fn child_lookups(
    tx: &fjall::OptimisticWriteTx,
    facts: &fjall::OptimisticTxKeyspace,
    parent: &Path,
) -> Result<Vec<(Path, LookupPayload)>, ProjectionError> {
    let mut prefix = fact_key(parent.as_str(), RecordKind::Lookup, None);
    if !parent.is_root() {
        prefix.push(b'/');
    }
    // The first key after every key with the prefix: `/` + 1 is `0`.
    let mut end = prefix.clone();
    end.pop();
    end.push(b'0');
    let mut start = prefix.clone();
    let mut children = Vec::new();
    while let Some(guard) = tx.range(facts, start.as_slice()..end.as_slice()).next() {
        let (key, value) = guard.into_inner()?;
        let name = &key[prefix.len()..];
        if let Some(length) = name.iter().position(|byte| *byte == b'/') {
            // A fact below a child: the next read starts after all of them.
            start.truncate(prefix.len());
            start.extend_from_slice(&name[..length]);
            start.push(b'0');
            continue;
        }
        let (path, kind, aux) = decode_fact_key(&key)?;
        if kind != RecordKind::Lookup || aux.is_some() {
            return Err(ProjectionError::Inconsistent(
                "lookup prefix contained a noncanonical fact key".into(),
            ));
        }
        let DurableFact::Lookup(lookup) = postcard::from_bytes(&value)? else {
            return Err(ProjectionError::Inconsistent(
                "lookup key contains a non-lookup fact".into(),
            ));
        };
        children.push((path, lookup));
        start = key.to_vec();
        start.push(0);
    }
    Ok(children)
}

fn decode_fact_key(key: &[u8]) -> Result<(Path, RecordKind, Option<String>), ProjectionError> {
    let rest = key.strip_prefix(b"r:").ok_or_else(|| {
        ProjectionError::Inconsistent("durable fact key has an invalid prefix".into())
    })?;
    let kind = match rest.first().copied() {
        Some(b'L') => RecordKind::Lookup,
        Some(b'A') => RecordKind::Attr,
        Some(b'D') => RecordKind::Dirents,
        Some(b'F') => RecordKind::File,
        _ => {
            return Err(ProjectionError::Inconsistent(
                "durable fact key has an unknown kind".into(),
            ));
        },
    };
    if rest.get(1) != Some(&b':') {
        return Err(ProjectionError::Inconsistent(
            "durable fact key is missing its separator".into(),
        ));
    }
    let value = &rest[2..];
    let (path_bytes, aux_bytes) = match value.iter().position(|byte| *byte == 0x1f) {
        Some(index) => (&value[..index], Some(&value[index + 1..])),
        None => (value, None),
    };
    let path = Path::parse(
        std::str::from_utf8(path_bytes)
            .map_err(|_| ProjectionError::Inconsistent("durable fact path is not UTF-8".into()))?,
    )
    .map_err(|error| ProjectionError::Inconsistent(error.to_string()))?;
    let aux = aux_bytes
        .map(|encoded| {
            let bytes = hex::decode(encoded).map_err(|_| {
                ProjectionError::Inconsistent("durable fact auxiliary key is not hex".into())
            })?;
            if hex::encode(&bytes).as_bytes() != encoded {
                return Err(ProjectionError::Inconsistent(
                    "durable fact auxiliary key is not canonical lowercase hex".into(),
                ));
            }
            String::from_utf8(bytes).map_err(|_| {
                ProjectionError::Inconsistent("durable fact auxiliary key is not UTF-8".into())
            })
        })
        .transpose()?;
    Ok((path, kind, aux))
}

fn apply_dirents(
    tx: &mut fjall::OptimisticWriteTx,
    facts: &fjall::OptimisticTxKeyspace,
    mutation: &DirentsMutation,
) -> Result<(), ProjectionError> {
    let (path, value) = match mutation {
        DirentsMutation::Replace { path, value } => (path, value.clone()),
        DirentsMutation::MergeHints {
            path,
            entries,
            write,
        } => {
            let current = read_dirents_fact(tx, facts, path)?;
            let map = entries
                .iter()
                .cloned()
                .map(|entry| (entry.name.clone(), entry))
                .collect();
            (path, DirentsPayload::merged(current, map, *write))
        },
        DirentsMutation::AppendPage {
            path,
            page,
            next_cursor,
            exhaustive,
        } => {
            let mut current = read_dirents_fact(tx, facts, path)?.ok_or_else(|| {
                ProjectionError::Inconsistent("missing accumulated listing".into())
            })?;
            if current.next_cursor.is_none() || current.next_cursor != page.cursor {
                return Err(ProjectionError::Inconsistent(
                    "listing cursor changed".into(),
                ));
            }
            current.pages.push(page.clone());
            current.next_cursor.clone_from(next_cursor);
            current.exhaustive = *exhaustive;
            (path, current)
        },
        DirentsMutation::ReplacePage {
            path,
            page,
            next_cursor,
            exhaustive,
        } => {
            let mut current = read_dirents_fact(tx, facts, path)?.ok_or_else(|| {
                ProjectionError::Inconsistent("missing revalidated listing".into())
            })?;
            let index = current
                .pages
                .iter()
                .position(|cached| cached.cursor == page.cursor)
                .ok_or_else(|| {
                    ProjectionError::Inconsistent("revalidated listing page is not cached".into())
                })?;
            let following = current
                .pages
                .get(index + 1)
                .map_or_else(|| current.next_cursor.clone(), |later| later.cursor.clone());
            current.pages[index] = page.clone();
            if following != *next_cursor {
                current.pages.truncate(index + 1);
                current.next_cursor.clone_from(next_cursor);
            }
            if index + 1 == current.pages.len() {
                current.exhaustive = *exhaustive;
            }
            (path, current)
        },
        DirentsMutation::RenewPage {
            path,
            cursor,
            token,
            interval,
            now_millis,
        } => {
            let mut current = read_dirents_fact(tx, facts, path)?.ok_or_else(|| {
                ProjectionError::Inconsistent("missing revalidated listing".into())
            })?;
            let validator = current
                .pages
                .iter_mut()
                .find(|page| page.cursor == *cursor)
                .and_then(|page| page.validator.as_mut())
                .ok_or_else(|| {
                    ProjectionError::Inconsistent(
                        "unchanged answer for a page without a cached validator".into(),
                    )
                })?;
            validator.token.clone_from(token);
            validator.interval = *interval;
            validator.deadline = crate::clock::page_deadline(*interval, *now_millis);
            (path, current)
        },
    };
    put(
        tx,
        facts,
        fact_key(path.as_str(), RecordKind::Dirents, None),
        postcard::to_allocvec(&DurableFact::Dirents(value))?,
    )?;
    Ok(())
}

fn read_dirents_fact(
    tx: &mut fjall::OptimisticWriteTx,
    facts: &fjall::OptimisticTxKeyspace,
    path: &Path,
) -> Result<Option<DirentsPayload>, ProjectionError> {
    let Some(bytes) = tx.get(facts, fact_key(path.as_str(), RecordKind::Dirents, None))? else {
        return Ok(None);
    };
    let fact: DurableFact = postcard::from_bytes(&bytes)?;
    let DurableFact::Dirents(value) = fact else {
        return Err(ProjectionError::Inconsistent(
            "dirents mutation found a non-dirents fact".into(),
        ));
    };
    Ok(Some(value))
}

#[cfg(test)]
mod tests {
    use super::*;

    const COMMIT: &str = "0123456789abcdef0123456789abcdef01234567";

    fn open_store(mount: &str) -> (tempfile::TempDir, Arc<Caches>, Arc<MountResources>) {
        let dir = tempfile::tempdir().unwrap();
        let caches = Caches::open(dir.path()).unwrap();
        let name = MountName::new(mount).unwrap();
        let source = mount.as_bytes();
        let projection_id = ProjectionId::new(source);
        let store = caches.mount(&name, projection_id, source).unwrap();
        (dir, caches, store)
    }

    fn p(path: &str) -> Path {
        Path::parse(path).unwrap()
    }

    fn transition_for_object(id: &[u8], bytes: &[u8], aliases: &[Path]) -> ProjectionTransition {
        let mut objects = vec![ObjectMutation::Canonical {
            id: id.to_vec(),
            bytes: bytes.to_vec(),
            validator: None,
        }];
        objects.extend(aliases.iter().cloned().map(|alias| ObjectMutation::Index {
            id: id.to_vec(),
            alias,
        }));
        ProjectionTransition {
            objects,
            ..ProjectionTransition::default()
        }
    }

    fn publish_object(store: &MountResources, id: &[u8], bytes: &[u8], aliases: &[Path]) {
        store
            .publish(
                transition_for_object(id, bytes, aliases),
                store.current_epoch(),
            )
            .unwrap();
    }

    fn publish_negative(store: &MountResources, path: &Path, id: Option<Vec<u8>>, expiry: u64) {
        store
            .publish(
                ProjectionTransition {
                    records: vec![RecordWrite {
                        path: path.clone(),
                        aux: None,
                        fact: FactPayload::Lookup(LookupPayload::Negative { id }),
                    }],
                    freshness: vec![Freshness {
                        path: path.clone(),
                        expires_at: Some(expiry),
                    }],
                    ..ProjectionTransition::default()
                },
                store.current_epoch(),
            )
            .unwrap();
    }

    #[test]
    fn successor_generation_fences_retired_projection_writes() {
        let (_dir, caches, first) = open_store("m");
        let name = MountName::new("m").unwrap();
        let source = b"m";
        let projection_id = ProjectionId::new(source);
        let successor = caches.prepare_mount(&name, projection_id, source).unwrap();

        let old_path = p("/old");
        assert!(matches!(
            first
                .publish(
                    transition_for_object(b"old", b"old", std::slice::from_ref(&old_path)),
                    first.current_epoch(),
                )
                .unwrap(),
            PublicationOutcome::Committed { .. }
        ));

        first.retire();
        successor.activate();
        assert!(matches!(
            first
                .publish(
                    transition_for_object(b"late", b"late", &[p("/late")]),
                    first.current_epoch(),
                )
                .unwrap(),
            PublicationOutcome::Fenced
        ));
        assert!(matches!(
            successor
                .publish(
                    transition_for_object(b"next", b"next", &[p("/next")]),
                    successor.current_epoch(),
                )
                .unwrap(),
            PublicationOutcome::Committed { .. }
        ));
    }

    #[tokio::test]
    async fn publication_reservations_order_path_ancestors_and_boundaries() {
        let (_dir, _caches, resources) = open_store("gh");
        let parent = resources.reserve(PublicationKey::Path(p("/a"))).await;
        assert!(
            tokio::time::timeout(
                std::time::Duration::from_millis(20),
                resources.reserve(PublicationKey::Path(p("/a/b")))
            )
            .await
            .is_err()
        );
        let boundary = tokio::time::timeout(
            std::time::Duration::from_millis(100),
            resources.reserve(PublicationKey::Path(p("/abcd"))),
        )
        .await
        .expect("a non-descendant path is not covered by /a");
        drop(boundary);
        drop(parent);
    }

    const OBJ_ID: &[u8] = b"issue:42";

    #[test]
    fn transition_keeps_forward_reverse_identity_and_mount_isolation() {
        let (_dir, caches, store_a) = open_store("a");
        let alias = p("/issues/42/item.json");
        publish_object(&store_a, OBJ_ID, b"from-a", std::slice::from_ref(&alias));
        assert_eq!(store_a.id_of_path(&alias).unwrap().as_deref(), Some(OBJ_ID));
        assert_eq!(store_a.paths_for_id(OBJ_ID).unwrap(), vec![alias.clone()]);
        assert_eq!(
            store_a.cached_canonical_for(&alias).unwrap().unwrap().bytes,
            b"from-a"
        );

        let name_b = MountName::new("b").unwrap();
        let source = b"b";
        let store_b = caches
            .mount(&name_b, ProjectionId::new(source), source)
            .unwrap();
        publish_object(&store_b, OBJ_ID, b"from-b", std::slice::from_ref(&alias));
        assert_eq!(
            store_b.cached_canonical_for(&alias).unwrap().unwrap().bytes,
            b"from-b"
        );
        assert_eq!(
            store_a.cached_canonical_for(&alias).unwrap().unwrap().bytes,
            b"from-a"
        );
    }

    #[test]
    fn same_transition_writes_then_invalidates_target_and_keeps_survivor() {
        let (_dir, _caches, store) = open_store("m");
        let target = p("/target");
        let rebound = p("/rebound");
        let survivor = p("/survivor");
        let survivor_id = b"survivor:42";
        let rebound_id = b"rebound:42";
        publish_object(
            &store,
            OBJ_ID,
            b"old-rebound",
            std::slice::from_ref(&rebound),
        );
        let mut transition = transition_for_object(OBJ_ID, b"data", std::slice::from_ref(&target));
        transition.objects.extend(
            transition_for_object(rebound_id, b"new-rebound", std::slice::from_ref(&rebound))
                .objects,
        );
        transition.objects.extend(
            transition_for_object(survivor_id, b"other", std::slice::from_ref(&survivor)).objects,
        );
        transition
            .invalidations
            .push(Invalidation::Object(OBJ_ID.to_vec()));
        store.publish(transition, store.current_epoch()).unwrap();
        assert!(store.cached_canonical_for(&target).unwrap().is_none());
        assert!(store.id_of_path(&target).unwrap().is_none());
        assert!(store.paths_for_id(OBJ_ID).unwrap().is_empty());
        assert_eq!(
            store.id_of_path(&survivor).unwrap().as_deref(),
            Some(survivor_id.as_slice())
        );
        assert_eq!(
            store
                .cached_canonical_for(&survivor)
                .unwrap()
                .unwrap()
                .bytes,
            b"other"
        );
        assert_eq!(
            store.id_of_path(&rebound).unwrap().as_deref(),
            Some(rebound_id.as_slice())
        );
        assert_eq!(
            store.cached_canonical_for(&rebound).unwrap().unwrap().bytes,
            b"new-rebound"
        );
    }

    #[test]
    fn listing_invalidation_retains_identity_and_removes_only_listing_facts() {
        let (_dir, _caches, store) = open_store("m");
        let leaf = p("/dir/child.json");
        publish_object(&store, OBJ_ID, b"data", std::slice::from_ref(&leaf));
        store
            .publish(
                ProjectionTransition {
                    git: vec![GitWrite {
                        path: leaf.clone(),
                        id: GitId::new("m", "https://example.test/repo.git", None),
                        relative_path: "tree".into(),
                        commit: COMMIT.into(),
                    }],
                    ..ProjectionTransition::default()
                },
                store.current_epoch(),
            )
            .unwrap();
        assert_eq!(
            store.git_for_path(&leaf).unwrap().unwrap().relative_path,
            "tree"
        );
        let dir = p("/dir");
        store
            .publish(
                ProjectionTransition {
                    dirents: vec![DirentsMutation::Replace {
                        path: dir.clone(),
                        value: DirentsPayload::single_page(Vec::new(), true, None),
                    }],
                    invalidations: vec![Invalidation::ListingPrefix(dir.clone())],
                    ..ProjectionTransition::default()
                },
                store.current_epoch(),
            )
            .unwrap();
        assert!(store.cached_canonical_for(&leaf).unwrap().is_some());
        assert!(
            store
                .cache_get(&dir, RecordKind::Dirents, None)
                .unwrap()
                .is_none()
        );
        assert!(store.git_for_path(&leaf).unwrap().is_none());
        assert_eq!(store.id_of_path(&leaf).unwrap().as_deref(), Some(OBJ_ID));
    }

    #[test]
    // One fixture proves that failed, fenced, and successful publication paths
    // preserve the same object/path coherence boundary.
    #[allow(clippy::too_many_lines)]
    fn stale_object_and_path_facts_are_fenced() {
        let (_dir, _caches, store) = open_store("m");
        let path = p("/x");
        publish_object(&store, OBJ_ID, b"old", std::slice::from_ref(&path));
        let conflict_path = p("/conflict");
        let conflict_id = b"conflict:1";
        publish_object(
            &store,
            conflict_id,
            b"conflict",
            std::slice::from_ref(&conflict_path),
        );
        let failed_epoch = store.current_epoch();
        let failed = store.publish(
            ProjectionTransition {
                objects: vec![ObjectMutation::Index {
                    id: OBJ_ID.to_vec(),
                    alias: conflict_path,
                }],
                invalidations: vec![Invalidation::Object(b"unrelated-retired".to_vec())],
                ..ProjectionTransition::default()
            },
            failed_epoch,
        );
        assert!(matches!(
            failed,
            Err(ProjectionError::Store(ProjectionStoreError::Transaction(message)))
                if message.contains("existing object identity")
        ));
        assert_eq!(store.current_epoch(), failed_epoch);

        let captured = store.current_epoch();
        store
            .publish(
                ProjectionTransition {
                    invalidations: vec![Invalidation::Object(OBJ_ID.to_vec())],
                    ..ProjectionTransition::default()
                },
                captured,
            )
            .unwrap();
        assert_eq!(store.current_epoch(), captured + 1);
        let result = store.publish(
            ProjectionTransition {
                records: vec![RecordWrite {
                    path: path.clone(),
                    aux: None,
                    fact: FactPayload::Lookup(LookupPayload::Positive(EntryMeta::directory())),
                }],
                freshness: vec![Freshness {
                    path: path.clone(),
                    expires_at: Some(100),
                }],
                ..ProjectionTransition::default()
            },
            captured,
        );
        assert!(matches!(result, Ok(PublicationOutcome::Fenced)));

        let canonical = store.publish(
            transition_for_object(OBJ_ID, b"resurrected", std::slice::from_ref(&path)),
            captured,
        );
        assert!(matches!(canonical, Ok(PublicationOutcome::Fenced)));

        for (invalidated, stale_path, invalidation) in [
            (
                p("/listing"),
                p("/listing"),
                Invalidation::ListingPath(p("/listing")),
            ),
            (
                p("/parent"),
                p("/parent/child"),
                Invalidation::ListingPrefix(p("/parent")),
            ),
        ] {
            let captured = store.current_epoch();
            store
                .publish(
                    ProjectionTransition {
                        invalidations: vec![invalidation],
                        ..ProjectionTransition::default()
                    },
                    captured,
                )
                .unwrap();
            let result = store.publish(
                ProjectionTransition {
                    records: vec![RecordWrite {
                        path: stale_path.clone(),
                        aux: None,
                        fact: FactPayload::Lookup(LookupPayload::Positive(EntryMeta::directory())),
                    }],
                    freshness: vec![Freshness {
                        path: stale_path,
                        expires_at: Some(100),
                    }],
                    git: vec![GitWrite {
                        path: invalidated,
                        id: GitId::new("m", "https://example.test/repo.git", None),
                        relative_path: "tree".into(),
                        commit: COMMIT.into(),
                    }],
                    ..ProjectionTransition::default()
                },
                captured,
            );
            assert!(matches!(result, Ok(PublicationOutcome::Fenced)));
        }
    }

    #[test]
    // This is the representative durable-cache lifecycle fixture; splitting it
    // would hide the ordering between TTL replacement and object invalidation.
    #[allow(clippy::too_many_lines)]
    fn negative_ttl_replacement_and_object_invalidation_are_durable() {
        let (_dir, _caches, store) = open_store("m");
        let path = p("/missing");
        publish_negative(&store, &path, Some(OBJ_ID.to_vec()), 10_000);
        assert!(store.negative_for_checked(&path, 1_000).unwrap());
        assert!(!store.negative_for_checked(&path, 11_000).unwrap());
        publish_negative(&store, &path, Some(OBJ_ID.to_vec()), 20_000);
        assert!(store.negative_for_checked(&path, 1_000).unwrap());
        store
            .publish(
                ProjectionTransition {
                    invalidations: vec![Invalidation::Object(OBJ_ID.to_vec())],
                    ..ProjectionTransition::default()
                },
                store.current_epoch(),
            )
            .unwrap();
        assert!(!store.negative_for_checked(&path, 1_000).unwrap());

        let parent = p("/dir");
        let child = p("/dir/child");
        store
            .publish(
                ProjectionTransition {
                    dirents: vec![DirentsMutation::Replace {
                        path: parent.clone(),
                        value: DirentsPayload::single_page(Vec::new(), true, None),
                    }],
                    ..ProjectionTransition::default()
                },
                store.current_epoch(),
            )
            .unwrap();
        store
            .cache_get(&parent, RecordKind::Dirents, None)
            .unwrap()
            .expect("seed derived listing memory");
        store
            .publish(
                ProjectionTransition {
                    records: vec![RecordWrite {
                        path: child.clone(),
                        aux: None,
                        fact: FactPayload::Lookup(LookupPayload::Positive(EntryMeta::directory())),
                    }],
                    ..ProjectionTransition::default()
                },
                store.current_epoch(),
            )
            .unwrap();
        let listing: DirentsPayload = postcard::from_bytes(
            &store
                .cache_get(&parent, RecordKind::Dirents, None)
                .unwrap()
                .unwrap()
                .payload,
        )
        .unwrap();
        assert_eq!(listing.pages[0].entries[0].name, "child");
        assert!(!listing.is_complete_offline());
        store
            .publish(
                ProjectionTransition {
                    records: vec![RecordWrite {
                        path: child.clone(),
                        aux: None,
                        fact: FactPayload::Lookup(LookupPayload::Positive(
                            EntryMeta::file_without_attrs(),
                        )),
                    }],
                    ..ProjectionTransition::default()
                },
                store.current_epoch(),
            )
            .unwrap();
        assert!(
            store.dirents_payload(&parent).unwrap().unwrap().pages[0].entries[0]
                .meta
                .is_file()
        );

        publish_negative(&store, &child, Some(OBJ_ID.to_vec()), 30_000);
        let listing = store.dirents_payload(&parent).unwrap().unwrap();
        assert!(listing.entries().next().is_none());
        assert!(store.negative_for_checked(&child, 1_000).unwrap());

        let invalidated_parent = p("/invalidated");
        let invalidated_child = p("/invalidated/child");
        store
            .publish(
                ProjectionTransition {
                    records: vec![RecordWrite {
                        path: invalidated_child.clone(),
                        aux: None,
                        fact: FactPayload::Lookup(LookupPayload::Positive(EntryMeta::directory())),
                    }],
                    dirents: vec![DirentsMutation::Replace {
                        path: invalidated_parent.clone(),
                        value: DirentsPayload::single_page(
                            vec![crate::view::DirentRecord {
                                name: "child".into(),
                                meta: EntryMeta::directory(),
                            }],
                            true,
                            None,
                        ),
                    }],
                    ..ProjectionTransition::default()
                },
                store.current_epoch(),
            )
            .unwrap();
        store
            .publish(
                ProjectionTransition {
                    invalidations: vec![Invalidation::ListingPath(invalidated_child)],
                    ..ProjectionTransition::default()
                },
                store.current_epoch(),
            )
            .unwrap();
        let listing = store.dirents_payload(&invalidated_parent).unwrap().unwrap();
        assert!(listing.entries().next().is_none());
        assert!(!listing.is_complete_offline());

        let partial_parent = p("/partial");
        let partial_child = p("/partial/absent");
        store
            .publish(
                ProjectionTransition {
                    dirents: vec![DirentsMutation::Replace {
                        path: partial_parent.clone(),
                        value: DirentsPayload::single_page(
                            vec![crate::view::DirentRecord {
                                name: "sibling".into(),
                                meta: EntryMeta::directory(),
                            }],
                            false,
                            Some(crate::view::CachedCursor::Opaque("cursor".into())),
                        ),
                    }],
                    ..ProjectionTransition::default()
                },
                store.current_epoch(),
            )
            .unwrap();
        store
            .publish(
                ProjectionTransition {
                    invalidations: vec![Invalidation::ListingPath(partial_child)],
                    ..ProjectionTransition::default()
                },
                store.current_epoch(),
            )
            .unwrap();
        let listing = store.dirents_payload(&partial_parent).unwrap().unwrap();
        assert_eq!(listing.pages.len(), 1);
        assert_eq!(listing.pages[0].entries.len(), 1);
        assert_eq!(listing.pages[0].entries[0].name, "sibling");
        assert!(listing.next_cursor.is_none());
        assert!(!listing.is_authoritative_listing());
        assert!(!listing.is_complete_offline());

        let stale = p("/dir/stale");
        let reverse = p("/dir/reverse");
        store
            .publish(
                ProjectionTransition {
                    records: vec![RecordWrite {
                        path: stale.clone(),
                        aux: None,
                        fact: FactPayload::Lookup(LookupPayload::Positive(EntryMeta::directory())),
                    }],
                    ..ProjectionTransition::default()
                },
                store.current_epoch(),
            )
            .unwrap();
        publish_negative(&store, &reverse, Some(OBJ_ID.to_vec()), 30_000);
        store
            .cache_get(&stale, RecordKind::Lookup, None)
            .unwrap()
            .expect("seed stale child memory");
        store
            .cache_get(&reverse, RecordKind::Lookup, None)
            .unwrap()
            .expect("seed reverse-negative memory");
        store
            .publish(
                ProjectionTransition {
                    dirents: vec![DirentsMutation::Replace {
                        path: parent,
                        value: DirentsPayload::single_page(
                            vec![crate::view::DirentRecord {
                                name: "reverse".into(),
                                meta: EntryMeta::directory(),
                            }],
                            true,
                            None,
                        ),
                    }],
                    ..ProjectionTransition::default()
                },
                store.current_epoch(),
            )
            .unwrap();
        assert!(store.lookup_payload(&stale).unwrap().is_none());
        assert!(store.lookup_payload(&reverse).unwrap().is_none());
        assert!(
            store
                .cache_get(&stale, RecordKind::Lookup, None)
                .unwrap()
                .is_none()
        );
        assert!(
            store
                .cache_get(&reverse, RecordKind::Lookup, None)
                .unwrap()
                .is_none()
        );
        assert!(
            store
                .projection
                .get(&negative_key(OBJ_ID, &reverse))
                .unwrap()
                .is_none()
        );

        let stale_git = p("/dir/stale-git");
        store
            .publish(
                ProjectionTransition {
                    records: vec![RecordWrite {
                        path: stale_git.clone(),
                        aux: None,
                        fact: FactPayload::Lookup(LookupPayload::Positive(EntryMeta::directory())),
                    }],
                    git: vec![GitWrite {
                        path: stale_git.clone(),
                        id: GitId::new("m", "https://example.test/repo.git", None),
                        relative_path: String::new(),
                        commit: COMMIT.into(),
                    }],
                    ..ProjectionTransition::default()
                },
                store.current_epoch(),
            )
            .unwrap();
        assert!(store.git_for_path(&stale_git).unwrap().is_some());
        store
            .publish(
                ProjectionTransition {
                    records: vec![RecordWrite {
                        path: stale_git.clone(),
                        aux: None,
                        fact: FactPayload::Lookup(LookupPayload::Positive(
                            EntryMeta::file_without_attrs(),
                        )),
                    }],
                    ..ProjectionTransition::default()
                },
                store.current_epoch(),
            )
            .unwrap();
        assert!(store.git_for_path(&stale_git).unwrap().is_none());
        store
            .publish(
                ProjectionTransition {
                    records: vec![RecordWrite {
                        path: stale_git.clone(),
                        aux: None,
                        fact: FactPayload::Lookup(LookupPayload::Positive(EntryMeta::directory())),
                    }],
                    git: vec![GitWrite {
                        path: stale_git.clone(),
                        id: GitId::new("m", "https://example.test/repo.git", None),
                        relative_path: String::new(),
                        commit: COMMIT.into(),
                    }],
                    ..ProjectionTransition::default()
                },
                store.current_epoch(),
            )
            .unwrap();
        publish_negative(&store, &stale_git, None, 40_000);
        assert!(store.git_for_path(&stale_git).unwrap().is_none());
    }

    fn fed_dir(path: &Path, children: &[&str], write: ListingWrite) -> ProjectionTransition {
        let mut records = vec![RecordWrite {
            path: path.clone(),
            aux: None,
            fact: FactPayload::Lookup(LookupPayload::Positive(EntryMeta::directory())),
        }];
        let entries = children
            .iter()
            .map(|name| {
                records.push(RecordWrite {
                    path: path.join(name).unwrap(),
                    aux: None,
                    fact: FactPayload::Lookup(LookupPayload::Positive(EntryMeta::directory())),
                });
                crate::view::DirentRecord {
                    name: (*name).to_string(),
                    meta: EntryMeta::directory(),
                }
            })
            .collect();
        ProjectionTransition {
            records,
            dirents: vec![DirentsMutation::MergeHints {
                path: path.clone(),
                entries,
                write,
            }],
            feed: Some(FeedWrite {
                object: b"feed".to_vec(),
                part: Some(FeedPartWrite {
                    token: "token".into(),
                    more: false,
                }),
                inner: Vec::new(),
                anchors: Vec::new(),
                deadline: 1_000,
            }),
            ..ProjectionTransition::default()
        }
    }

    #[test]
    fn a_publication_without_a_change_commits_nothing() {
        let (_dir, _caches, store) = open_store("unchanged");
        let transition = || ProjectionTransition {
            records: vec![RecordWrite {
                path: p("/dir/child"),
                aux: None,
                fact: FactPayload::Lookup(LookupPayload::Positive(EntryMeta::directory())),
            }],
            dirents: vec![DirentsMutation::MergeHints {
                path: p("/dir"),
                entries: vec![crate::view::DirentRecord {
                    name: "child".into(),
                    meta: EntryMeta::directory(),
                }],
                write: ListingWrite::Exhaustive,
            }],
            freshness: vec![Freshness {
                path: p("/dir/child"),
                expires_at: Some(1_000),
            }],
            ..ProjectionTransition::default()
        };
        store.publish(transition(), store.current_epoch()).unwrap();
        let committed = store.projection.seqno();
        store.publish(transition(), store.current_epoch()).unwrap();
        assert_eq!(store.projection.seqno(), committed);

        let mut changed = transition();
        changed.freshness[0].expires_at = Some(2_000);
        store.publish(changed, store.current_epoch()).unwrap();
        assert_ne!(store.projection.seqno(), committed);
    }

    #[test]
    fn a_complete_listing_retires_only_the_lookups_of_its_own_children() {
        let (_dir, _caches, store) = open_store("children");
        let lookups = [
            "/dir/a",
            "/dir/a/x",
            "/dir/a/x/y",
            "/dir/a-b",
            "/dir/a-b/x",
            "/dir/a0",
            "/dir/b",
            "/dir2/a-b",
            "/dira",
            "/other/z",
        ];
        store
            .publish(
                ProjectionTransition {
                    records: lookups
                        .iter()
                        .map(|path| RecordWrite {
                            path: p(path),
                            aux: None,
                            fact: FactPayload::Lookup(LookupPayload::Positive(
                                EntryMeta::directory(),
                            )),
                        })
                        .collect(),
                    ..ProjectionTransition::default()
                },
                store.current_epoch(),
            )
            .unwrap();
        store
            .publish(
                ProjectionTransition {
                    dirents: vec![DirentsMutation::Replace {
                        path: p("/dir"),
                        value: DirentsPayload::single_page(
                            vec![crate::view::DirentRecord {
                                name: "a".into(),
                                meta: EntryMeta::directory(),
                            }],
                            true,
                            None,
                        ),
                    }],
                    ..ProjectionTransition::default()
                },
                store.current_epoch(),
            )
            .unwrap();
        let kept: Vec<&str> = lookups
            .into_iter()
            .filter(|path| store.lookup_payload(&p(path)).unwrap().is_some())
            .collect();
        assert_eq!(
            kept,
            [
                "/dir/a",
                "/dir/a/x",
                "/dir/a/x/y",
                "/dir/a-b/x",
                "/dir2/a-b",
                "/dira",
                "/other/z"
            ]
        );
    }

    #[test]
    fn fed_listings_stay_complete_through_reconciliation_and_removals() {
        let (_dir, _caches, store) = open_store("m");
        let books = p("/shelf/books");

        // A partial part, then a complete one whose child joins the listing
        // and whose lookup reconciliation keeps it complete.
        store
            .publish(
                fed_dir(&books, &["1"], ListingWrite::Partial),
                store.current_epoch(),
            )
            .unwrap();
        assert!(!store.dirents_payload(&books).unwrap().unwrap().exhaustive);
        store
            .publish(
                fed_dir(&books, &["2"], ListingWrite::Complete),
                store.current_epoch(),
            )
            .unwrap();
        let listing = store.dirents_payload(&books).unwrap().unwrap();
        assert!(listing.is_complete_offline());
        assert_eq!(listing.entries().count(), 2);
        assert!(store.is_fed(&books).unwrap());
        assert_eq!(
            store.feed_state(b"feed").unwrap(),
            Some(FeedState {
                token: "token".into(),
                ended: true,
                deadline: 1_000,
            })
        );

        // A removal retracts the child and its subtree and leaves the
        // listing as complete as it was.
        store
            .publish(
                ProjectionTransition {
                    removals: vec![p("/shelf/books/1")],
                    ..ProjectionTransition::default()
                },
                store.current_epoch(),
            )
            .unwrap();
        let listing = store.dirents_payload(&books).unwrap().unwrap();
        assert!(listing.is_complete_offline());
        assert_eq!(
            listing
                .entries()
                .map(|entry| entry.name.as_str())
                .collect::<Vec<_>>(),
            ["2"]
        );
        assert!(
            store
                .lookup_payload(&p("/shelf/books/1"))
                .unwrap()
                .is_none()
        );
        assert!(!store.is_fed(&p("/shelf/books/1")).unwrap());

        // Another terminal leaves the fed facts of a path to its feeds.
        store
            .publish(
                ProjectionTransition {
                    records: vec![RecordWrite {
                        path: p("/shelf/books/2"),
                        aux: None,
                        fact: FactPayload::Lookup(LookupPayload::Positive(
                            EntryMeta::file_without_attrs(),
                        )),
                    }],
                    ..ProjectionTransition::default()
                },
                store.current_epoch(),
            )
            .unwrap();
        assert!(matches!(
            store.lookup_payload(&p("/shelf/books/2")).unwrap(),
            Some(LookupPayload::Positive(meta)) if meta.is_directory()
        ));
    }

    #[test]
    fn blob_publication_rehydrates_and_scopes_reused_hits() {
        let (_dir, _caches, store) = open_store("m");
        let body = store.body.publish(b"body").unwrap();
        let request = BlobRequestId::new(None, "GET", "https://example.test/body", &[], None);
        let metadata = BlobMetadata {
            size: 4,
            content_type: None,
            etag: None,
            status: 200,
            response_headers: Vec::new(),
        };
        let failed = store.blob_publication(1);
        store.stage_blob_write(1, request, body, metadata.clone());
        drop(failed);
        assert!(store.take_blob_writes(1).is_empty());

        let second = store.blob_publication(2);
        store.stage_blob_write(2, request, body, metadata.clone());
        let writes = second.take();
        assert_eq!(writes.len(), 1);
        assert_eq!(writes[0].request, request);

        store
            .publish(
                ProjectionTransition {
                    blobs: writes,
                    ..ProjectionTransition::default()
                },
                store.current_epoch(),
            )
            .unwrap();
        store.request_handles.clear();
        store.blob_handles.clear();
        let reopened = store.blob_for_request(request).unwrap().unwrap();
        assert_eq!(reopened.body, body);
        assert_eq!(reopened.size, 4);

        let corrupt = BlobRequestId::new(None, "GET", "https://example.test/corrupt", &[], None);
        let corrupt_fact = DurableFact::Blob(BlobFact {
            body_id: [0; 32],
            length: 4,
            metadata,
        });
        store.stage_blob_write(
            3,
            corrupt,
            body,
            BlobMetadata {
                size: 4,
                content_type: None,
                etag: None,
                status: 200,
                response_headers: Vec::new(),
            },
        );
        store.take_blob_writes(3);
        store
            .projection
            .transact(|tx, facts| {
                tx.insert(
                    facts,
                    blob_key(corrupt),
                    postcard::to_allocvec(&corrupt_fact)
                        .map_err(|error| ProjectionStoreError::Transaction(error.to_string()))?,
                );
                Ok(())
            })
            .unwrap();
        store.request_handles.clear();
        store.blob_handles.clear();
        assert!(store.blob_for_request(corrupt).is_err());
    }
}
