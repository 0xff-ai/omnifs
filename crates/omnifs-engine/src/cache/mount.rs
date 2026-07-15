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
use omnifs_core::path::Path;
use omnifs_workspace::mounts::Name;
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
use super::projection::{ProjectionRow, ProjectionStore, ProjectionStoreError};
use crate::cache::memory::MemoryTier;
use crate::object_id::ObjectId;
use crate::view::{
    AttrPayload, ByteSource, CachedCursor, DirentsPayload, EntryMeta, FileAttrsCache, FilePayload,
    FileSize, LookupPayload,
};
use omnifs_workspace::ids::ProviderId;

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

#[derive(Debug, Clone)]
pub(crate) struct GitFact {
    pub id: GitId,
    pub relative_path: String,
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
        exhaustive: bool,
    },
    AppendPage {
        path: Path,
        expected_cursor: CachedCursor,
        entries: Vec<crate::view::DirentRecord>,
        next_cursor: Option<CachedCursor>,
        exhaustive: bool,
    },
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
    projection_owners: Mutex<HashMap<ProjectionId, Weak<MountResources>>>,
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
            projection_owners: Mutex::new(HashMap::new()),
        }))
    }

    /// Open the existing global projection database and body store without
    /// creating, sweeping, or repairing cache state.
    pub(crate) fn open_existing(dir: &StdPath) -> Result<Arc<Self>, ProjectionError> {
        let dir =
            crate::cache::existing_directory(dir).map_err(super::body::BodyStoreError::from)?;
        let projection_root = crate::cache::existing_directory(&dir.join("projections"))
            .map_err(super::body::BodyStoreError::from)?;
        let database_root = crate::cache::existing_directory(&projection_root.join("database"))
            .map_err(super::body::BodyStoreError::from)?;
        let version = database_root.join("version");
        let metadata =
            std::fs::symlink_metadata(&version).map_err(super::body::BodyStoreError::from)?;
        if metadata.file_type().is_symlink() || !metadata.is_file() {
            return Err(ProjectionError::Inconsistent(
                "projection database version marker is not a regular file".into(),
            ));
        }
        let projection_database = OptimisticTxDatabase::builder(database_root).open()?;
        let body = Arc::new(BodyStore::open_existing(dir.join("bodies"))?);
        Ok(Arc::new(Self {
            body,
            projection_root,
            projection_database,
            projection_owners: Mutex::new(HashMap::new()),
        }))
    }

    /// Return the sole owner of one mount's cache and blob resources.
    pub(crate) fn mount(
        self: &Arc<Self>,
        mount: &Name,
        projection_id: ProjectionId,
        provider_id: ProviderId,
        spec_source: &[u8],
    ) -> anyhow::Result<Arc<MountResources>> {
        let mut owners = self.projection_owners.lock();
        if let Some(owner) = owners.get(&projection_id).and_then(Weak::upgrade) {
            return Ok(owner);
        }
        let owner = MountResources::new(self, mount, projection_id, provider_id, spec_source)?;
        owners.insert(projection_id, Arc::downgrade(&owner));
        Ok(owner)
    }

    /// Return the sole owner for one existing projection after validating its
    /// complete durable fact graph.
    pub(crate) fn mount_existing(
        self: &Arc<Self>,
        mount: &Name,
        projection_id: ProjectionId,
        provider_id: ProviderId,
        spec_source: &[u8],
    ) -> Result<Arc<MountResources>, ProjectionError> {
        let mut owners = self.projection_owners.lock();
        if let Some(owner) = owners.get(&projection_id).and_then(Weak::upgrade) {
            owner
                .projection
                .validate_existing(mount, spec_source, provider_id)?;
            owner.validate_durable_projection()?;
            return Ok(owner);
        }
        let owner =
            MountResources::new_existing(self, mount, projection_id, provider_id, spec_source)?;
        owner.validate_durable_projection()?;
        owners.insert(projection_id, Arc::downgrade(&owner));
        Ok(owner)
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
        mount: &Name,
        projection_id: ProjectionId,
        provider_id: ProviderId,
        spec_source: &[u8],
    ) -> anyhow::Result<Arc<Self>> {
        let projection = ProjectionStore::open(
            &caches.projection_root,
            &caches.projection_database,
            projection_id,
            mount,
            spec_source,
            provider_id,
        )?;
        Ok(Self::from_projection(caches, projection))
    }

    fn new_existing(
        caches: &Caches,
        mount: &Name,
        projection_id: ProjectionId,
        provider_id: ProviderId,
        spec_source: &[u8],
    ) -> Result<Arc<Self>, ProjectionError> {
        let projection = ProjectionStore::open_existing(
            &caches.projection_root,
            &caches.projection_database,
            projection_id,
            mount,
            spec_source,
            provider_id,
        )?;
        Ok(Self::from_projection(caches, projection))
    }

    fn from_projection(caches: &Caches, projection: ProjectionStore) -> Arc<Self> {
        let body = Arc::clone(&caches.body);
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
        })
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
        } = transition;
        // Bodies become immutable and addressable before the projection
        // transaction can publish a reference to them. This work intentionally
        // happens before taking the coherence mutex.
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
                    },
                ))
            })
            .collect::<Result<Vec<_>, ProjectionError>>()?;
        let transition_git_paths: HashSet<Path> = git.iter().map(|git| git.path.clone()).collect();

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
            let path = match mutation {
                DirentsMutation::Replace { path, .. }
                | DirentsMutation::MergeHints { path, .. }
                | DirentsMutation::AppendPage { path, .. } => path,
            };
            memory_paths.insert(path.clone());
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
                tx.insert(facts, key, value);
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
                    insert_alias(tx, facts, id, alias);
                }
            }

            for record in &prepared_records {
                remove_negative_for_path(tx, facts, &record.path)?;
                if matches!(&record.fact, DurableFact::Lookup(_))
                    && !transition_git_paths.contains(&record.path)
                {
                    tx.remove(facts, git_key(&record.path));
                }
                let key = fact_key(
                    record.path.as_str(),
                    record.fact.kind(),
                    record.aux.as_deref(),
                );
                let value = postcard::to_allocvec(&record.fact).map_err(ProjectionError::from)?;
                tx.insert(facts, key, value);
                if let DurableFact::Lookup(lookup) = &record.fact
                    && let LookupPayload::Negative { id: Some(id) } = lookup
                {
                    tx.insert(
                        facts,
                        negative_key(id, &record.path),
                        record.path.as_str().as_bytes(),
                    );
                }
            }
            for (key, fact) in &prepared_blobs {
                tx.insert(
                    facts,
                    key.clone(),
                    postcard::to_allocvec(fact).map_err(ProjectionError::from)?,
                );
            }
            for (key, fact) in &prepared_git {
                tx.insert(
                    facts,
                    key.clone(),
                    postcard::to_allocvec(fact).map_err(ProjectionError::from)?,
                );
            }
            for mutation in &dirents {
                apply_dirents(tx, facts, mutation)?;
            }
            for freshness in &freshness {
                let mut expiry = b"x:".to_vec();
                expiry.extend_from_slice(freshness.path.as_str().as_bytes());
                tx.insert(
                    facts,
                    expiry,
                    postcard::to_allocvec(&freshness.expires_at).map_err(ProjectionError::from)?,
                );
            }
            let mut reconciled_paths =
                reconcile_listing_relations(tx, facts, &prepared_records, &dirents, &mut removals)?;
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
            for key in removals {
                tx.remove(facts, key);
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

    pub(crate) fn memory_invalidate_entries_if<P>(&self, predicate: P)
    where
        P: Fn(&Key, &Arc<Record>) -> bool + Send + Sync + 'static,
    {
        self.memory.mem_invalidate_entries_if(predicate);
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
        let DurableFact::Git { id, relative_path } = postcard::from_bytes(&bytes)? else {
            return Err(ProjectionError::Inconsistent(
                "Git fact key contains a non-Git fact".into(),
            ));
        };
        validate_git_relative(&relative_path)?;
        Ok(Some(GitFact { id, relative_path }))
    }

    pub(crate) fn git_facts(&self) -> Result<Vec<(Path, GitFact)>, ProjectionError> {
        self.projection
            .rows()?
            .into_iter()
            .filter(|row| row.key.starts_with(b"g:"))
            .map(|row| {
                let path = decode_path(&row.key[2..], "Git fact")?;
                let DurableFact::Git { id, relative_path } = postcard::from_bytes(&row.value)?
                else {
                    return Err(inconsistent("Git key contains a non-Git fact"));
                };
                validate_git_relative(&relative_path)?;
                Ok((path, GitFact { id, relative_path }))
            })
            .collect()
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

    /// Validate every row and cross-row relation before an offline mount can
    /// become visible through the fixed mount table.
    fn validate_durable_projection(&self) -> Result<(), ProjectionError> {
        let mut state = ProjectionValidation::default();
        for row in self.projection.rows()? {
            state.read_row(row, &self.body)?;
        }
        state.finish(&self.body)
    }

    // --- Invalidation ---------------------------------------------------------
}

#[derive(Default)]
struct ProjectionValidation {
    objects: HashMap<Vec<u8>, (BodyId, u64)>,
    indexes: HashMap<Path, Vec<u8>>,
    aliases: HashSet<(Vec<u8>, Path)>,
    negatives: HashMap<Path, Option<Vec<u8>>>,
    negative_reverse: HashSet<(Vec<u8>, Path)>,
    positives: HashSet<Path>,
    positive_directories: HashSet<Path>,
    git_paths: HashSet<Path>,
    complete_listings: HashMap<Path, HashSet<String>>,
    metas: Vec<(Path, EntryMeta)>,
}

impl ProjectionValidation {
    fn read_row(&mut self, row: ProjectionRow, bodies: &BodyStore) -> Result<(), ProjectionError> {
        let ProjectionRow { key, value } = row;
        if key.starts_with(b"r:") {
            return self.read_record(&key, &value, bodies);
        }
        if let Some(raw) = key.strip_prefix(b"b:") {
            let request = std::str::from_utf8(raw)
                .map_err(|_| inconsistent("blob request key is not UTF-8"))?;
            if BlobRequestId::from_hex(request).is_none() {
                return Err(inconsistent(
                    "blob request key is not canonical lowercase hex",
                ));
            }
            let fact: DurableFact = postcard::from_bytes(&value)?;
            let DurableFact::Blob(fact) = fact else {
                return Err(inconsistent("blob request key contains a non-blob fact"));
            };
            let body = BodyId::from_digest_bytes(fact.body_id);
            if fact.metadata.size != fact.length {
                return Err(inconsistent(
                    "blob metadata size does not match its body length",
                ));
            }
            bodies.validate(body, Some(fact.length))?;
            return Ok(());
        }
        if let Some(raw) = key.strip_prefix(b"g:") {
            let path = decode_path(raw, "Git fact")?;
            let fact: DurableFact = postcard::from_bytes(&value)?;
            let DurableFact::Git { relative_path, .. } = fact else {
                return Err(inconsistent("Git key contains a non-Git fact"));
            };
            validate_git_relative(&relative_path)?;
            self.git_paths.insert(path);
            return Ok(());
        }
        if let Some(raw) = key.strip_prefix(b"x:") {
            let _ = decode_path(raw, "expiry")?;
            let _: Option<u64> = postcard::from_bytes(&value)?;
            return Ok(());
        }
        if let Some(raw) = key.strip_prefix(b"o:") {
            let id = decode_object_hex(raw, "object")?;
            let (body, length, _validator): ([u8; 32], u64, Option<String>) =
                postcard::from_bytes(&value)?;
            let body = BodyId::from_digest_bytes(body);
            bodies.validate(body, Some(length))?;
            if self.objects.insert(id, (body, length)).is_some() {
                return Err(inconsistent("duplicate durable object identity"));
            }
            return Ok(());
        }
        if let Some(raw) = key.strip_prefix(b"i:") {
            let path = decode_path(raw, "object index")?;
            validate_object_id(&value, "object index")?;
            if self.indexes.insert(path, value).is_some() {
                return Err(inconsistent("duplicate durable object index"));
            }
            return Ok(());
        }
        if let Some(raw) = key.strip_prefix(b"a:") {
            let (id, path) = decode_id_path(raw, "object alias")?;
            if !value.is_empty() {
                return Err(inconsistent("object alias row must have an empty value"));
            }
            if !self.aliases.insert((id, path)) {
                return Err(inconsistent("duplicate durable object alias"));
            }
            return Ok(());
        }
        if let Some(raw) = key.strip_prefix(b"n:") {
            let (id, path) = decode_id_path(raw, "negative reverse")?;
            if value.as_slice() != path.as_str().as_bytes() {
                return Err(inconsistent(
                    "negative reverse value does not match its path",
                ));
            }
            if !self.negative_reverse.insert((id, path)) {
                return Err(inconsistent("duplicate durable negative reverse relation"));
            }
            return Ok(());
        }
        Err(inconsistent("durable projection row has an unknown prefix"))
    }

    fn read_record(
        &mut self,
        key: &[u8],
        value: &[u8],
        bodies: &BodyStore,
    ) -> Result<(), ProjectionError> {
        let (path, kind, aux) = decode_fact_key(key)?;
        if aux.is_some() && kind != RecordKind::File {
            return Err(inconsistent("only file facts may use an auxiliary key"));
        }
        let fact: DurableFact = postcard::from_bytes(value)?;
        match (kind, fact) {
            (RecordKind::Lookup, DurableFact::Lookup(payload)) => match payload {
                LookupPayload::Positive(meta) => {
                    self.positives.insert(path.clone());
                    if meta.is_directory() {
                        self.positive_directories.insert(path.clone());
                    }
                    self.metas.push((path, meta));
                },
                LookupPayload::Negative { id } => {
                    if let Some(id) = &id {
                        validate_object_id(id, "negative lookup")?;
                    }
                    if self.negatives.insert(path, id).is_some() {
                        return Err(inconsistent("duplicate durable negative lookup"));
                    }
                },
            },
            (RecordKind::Attr, DurableFact::Attr(payload)) => {
                self.metas.push((path, payload.meta));
            },
            (RecordKind::Dirents, DurableFact::Dirents(payload)) => {
                if payload.next_cursor.is_some() && !payload.paginated {
                    return Err(inconsistent("listing cursor requires paginated state"));
                }
                if payload.next_cursor.is_some() && payload.exhaustive {
                    return Err(inconsistent("an exhaustive listing cannot retain a cursor"));
                }
                let complete =
                    payload.exhaustive || (payload.paginated && payload.next_cursor.is_none());
                let mut names = HashSet::new();
                for entry in payload.entries {
                    if !names.insert(entry.name.clone()) {
                        return Err(inconsistent("durable listing contains a duplicate name"));
                    }
                    let child = path.join(&entry.name).map_err(|error| {
                        inconsistent(format!("durable listing name is invalid: {error}"))
                    })?;
                    self.metas.push((child, entry.meta));
                }
                if complete {
                    self.complete_listings.insert(path, names);
                }
            },
            (
                RecordKind::File,
                DurableFact::File {
                    body_id, length, ..
                },
            ) => bodies.validate(BodyId::from_digest_bytes(body_id), Some(length))?,
            _ => {
                return Err(inconsistent(
                    "durable fact kind does not match its path key",
                ));
            },
        }
        Ok(())
    }

    fn finish(self, bodies: &BodyStore) -> Result<(), ProjectionError> {
        for (path, id) in &self.indexes {
            if !self.objects.contains_key(id) {
                return Err(inconsistent("object index points to a missing object row"));
            }
            if !self.aliases.contains(&(id.clone(), path.clone())) {
                return Err(inconsistent("object index is missing its reverse alias"));
            }
        }
        for (id, path) in &self.aliases {
            if !self.objects.contains_key(id) {
                return Err(inconsistent("object alias points to a missing object row"));
            }
            if self.indexes.get(path) != Some(id) {
                return Err(inconsistent(
                    "object alias disagrees with its forward index",
                ));
            }
        }
        for (path, id) in &self.negatives {
            if let Some(id) = id
                && !self.negative_reverse.contains(&(id.clone(), path.clone()))
            {
                return Err(inconsistent(
                    "negative lookup is missing its reverse relation",
                ));
            }
        }
        for (id, path) in &self.negative_reverse {
            if self.negatives.get(path).and_then(Option::as_ref) != Some(id) {
                return Err(inconsistent(
                    "negative reverse relation has no matching lookup",
                ));
            }
        }
        if self
            .git_paths
            .iter()
            .any(|path| self.negatives.contains_key(path))
        {
            return Err(inconsistent(
                "durable Git subtree conflicts with an exact negative lookup",
            ));
        }
        for path in &self.git_paths {
            if self.positives.contains(path) && !self.positive_directories.contains(path) {
                return Err(inconsistent(
                    "durable Git subtree conflicts with an exact file lookup",
                ));
            }
            if !self.positive_directories.contains(path) {
                return Err(inconsistent(
                    "durable Git subtree has no exact positive directory identity",
                ));
            }
        }
        for positive in self.positives.iter().chain(&self.git_paths) {
            if let Some((parent, name)) = positive.parent_and_name()
                && let Some(names) = self.complete_listings.get(&parent)
                && !names.contains(name)
            {
                return Err(inconsistent(
                    "completed listing omits an exact durable positive child",
                ));
            }
        }
        for negative in self.negatives.keys() {
            if let Some((parent, name)) = negative.parent_and_name()
                && self
                    .complete_listings
                    .get(&parent)
                    .is_some_and(|names| names.contains(name))
            {
                return Err(inconsistent(
                    "completed listing contains an exact durable negative child",
                ));
            }
        }
        for (path, meta) in self.metas {
            validate_durable_meta(&path, &meta, &self.indexes, &self.objects, bodies)?;
        }
        Ok(())
    }
}

fn inconsistent(message: impl Into<String>) -> ProjectionError {
    ProjectionError::Inconsistent(message.into())
}

fn decode_path(bytes: &[u8], owner: &str) -> Result<Path, ProjectionError> {
    let value = std::str::from_utf8(bytes)
        .map_err(|_| inconsistent(format!("{owner} path is not UTF-8")))?;
    Path::parse(value).map_err(|error| inconsistent(format!("{owner} path is invalid: {error}")))
}

fn decode_object_hex(bytes: &[u8], owner: &str) -> Result<Vec<u8>, ProjectionError> {
    let value = std::str::from_utf8(bytes)
        .map_err(|_| inconsistent(format!("{owner} identity is not UTF-8")))?;
    let decoded = hex::decode(value)
        .map_err(|_| inconsistent(format!("{owner} identity is not lowercase hex")))?;
    if hex::encode(&decoded) != value {
        return Err(inconsistent(format!(
            "{owner} identity is not canonical lowercase hex"
        )));
    }
    validate_object_id(&decoded, owner)?;
    Ok(decoded)
}

fn validate_object_id(id: &[u8], owner: &str) -> Result<(), ProjectionError> {
    if ObjectId::from_bytes(id.to_vec()).to_wit().is_none() {
        return Err(inconsistent(format!(
            "{owner} contains an invalid object identity"
        )));
    }
    Ok(())
}

fn decode_id_path(bytes: &[u8], owner: &str) -> Result<(Vec<u8>, Path), ProjectionError> {
    let split = bytes
        .iter()
        .position(|byte| *byte == b':')
        .ok_or_else(|| inconsistent(format!("{owner} key is missing its path separator")))?;
    let id = decode_object_hex(&bytes[..split], owner)?;
    let path = decode_path(&bytes[split + 1..], owner)?;
    Ok((id, path))
}

fn validate_durable_meta(
    path: &Path,
    meta: &EntryMeta,
    indexes: &HashMap<Path, Vec<u8>>,
    objects: &HashMap<Vec<u8>, (BodyId, u64)>,
    bodies: &BodyStore,
) -> Result<(), ProjectionError> {
    let EntryMeta::File { attrs: Some(attrs) } = meta else {
        return Ok(());
    };
    attrs.validate().map_err(inconsistent)?;
    match attrs.byte_source() {
        ByteSource::Inline(_) => Err(inconsistent(
            "durable metadata contains inline bytes outside the global body store",
        )),
        ByteSource::Body(body) => {
            let FileSize::Exact(length) = attrs.size() else {
                return Err(inconsistent(
                    "durable body metadata requires an exact length",
                ));
            };
            bodies.validate(body, Some(length))?;
            Ok(())
        },
        ByteSource::Canonical => {
            let id = indexes
                .get(path)
                .ok_or_else(|| inconsistent("canonical metadata has no object index"))?;
            let (_, object_length) = objects
                .get(id)
                .ok_or_else(|| inconsistent("canonical metadata has no object row"))?;
            if attrs.size() != FileSize::Exact(*object_length) {
                return Err(inconsistent(
                    "canonical metadata length disagrees with its object body",
                ));
            }
            Ok(())
        },
        ByteSource::Deferred(_) => Ok(()),
    }
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
) {
    tx.insert(facts, index_key(path), id.to_vec());
    let mut alias = alias_prefix(id);
    alias.extend_from_slice(path.as_str().as_bytes());
    tx.insert(facts, alias, []);
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
        tx.remove(facts, negative_key(&id, path));
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
    let Some(mut listing) = read_dirents_fact(tx, facts, &parent)? else {
        return Ok(None);
    };
    listing.entries.retain(|entry| entry.name != name);
    // An exact child invalidation makes the parent's prior completeness claim,
    // cursor, and validator stale. The next ordinary browse revalidates the
    // parent; rate-limited serve-stale may retain unaffected siblings.
    listing.exhaustive = false;
    listing.paginated = false;
    listing.next_cursor = None;
    listing.validator = None;
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

// Lookup and listing reconciliation is one transaction scan. Splitting it
// would obscure which mutations participate in the same durable update.
#[allow(clippy::too_many_lines)]
fn reconcile_listing_relations(
    tx: &mut fjall::OptimisticWriteTx,
    facts: &fjall::OptimisticTxKeyspace,
    records: &[PreparedRecord],
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
                listing = DirentsPayload::merged(Some(listing), entries, false);
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
            LookupPayload::Negative { .. }
                if listing.entries.iter().any(|entry| entry.name == name) =>
            {
                listing.entries.retain(|entry| entry.name != name);
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
        .map(|mutation| match mutation {
            DirentsMutation::Replace { path, .. }
            | DirentsMutation::MergeHints { path, .. }
            | DirentsMutation::AppendPage { path, .. } => path.clone(),
        })
        .collect();
    for parent in touched {
        let Some(listing) = read_dirents_fact(tx, facts, &parent)? else {
            continue;
        };
        if !listing.is_complete_offline() {
            continue;
        }
        let names: HashSet<&str> = listing
            .entries
            .iter()
            .map(|entry| entry.name.as_str())
            .collect();
        let mut contradictions = Vec::new();
        for guard in tx.prefix(facts, b"r:L:") {
            let (key, value) = guard.into_inner()?;
            let key = key.to_vec();
            let (path, kind, aux) = decode_fact_key(&key)?;
            if kind != RecordKind::Lookup || aux.is_some() {
                return Err(ProjectionError::Inconsistent(
                    "lookup prefix contained a noncanonical fact key".into(),
                ));
            }
            let Some((candidate_parent, name)) = path.parent_and_name() else {
                continue;
            };
            if candidate_parent != parent {
                continue;
            }
            let fact: DurableFact = postcard::from_bytes(&value)?;
            let DurableFact::Lookup(lookup) = fact else {
                return Err(ProjectionError::Inconsistent(
                    "lookup key contains a non-lookup fact".into(),
                ));
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
            exhaustive,
        } => {
            let current = read_dirents_fact(tx, facts, path)?;
            let map = entries
                .iter()
                .cloned()
                .map(|entry| (entry.name.clone(), entry))
                .collect();
            (path, DirentsPayload::merged(current, map, *exhaustive))
        },
        DirentsMutation::AppendPage {
            path,
            expected_cursor,
            entries,
            next_cursor,
            exhaustive,
        } => {
            let current = read_dirents_fact(tx, facts, path)?.ok_or_else(|| {
                ProjectionError::Inconsistent("missing accumulated listing".into())
            })?;
            if current.next_cursor.as_ref() != Some(expected_cursor) {
                return Err(ProjectionError::Inconsistent(
                    "listing cursor changed".into(),
                ));
            }
            let mut merged = current;
            merged.entries.extend(entries.iter().cloned());
            merged.next_cursor.clone_from(next_cursor);
            merged.exhaustive = *exhaustive;
            (path, merged)
        },
    };
    tx.insert(
        facts,
        fact_key(path.as_str(), RecordKind::Dirents, None),
        postcard::to_allocvec(&DurableFact::Dirents(value))?,
    );
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

    fn open_store(mount: &str) -> (tempfile::TempDir, Arc<Caches>, Arc<MountResources>) {
        let dir = tempfile::tempdir().unwrap();
        let caches = Caches::open(dir.path()).unwrap();
        let name = Name::new(mount).unwrap();
        let source = mount.as_bytes();
        let provider_id = ProviderId::from_wasm_bytes(source);
        let projection_id = ProjectionId::new(source, provider_id);
        let store = caches
            .mount(&name, projection_id, provider_id, source)
            .unwrap();
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

        let name_b = Name::new("b").unwrap();
        let source = b"b";
        let provider_id = ProviderId::from_wasm_bytes(source);
        let store_b = caches
            .mount(
                &name_b,
                ProjectionId::new(source, provider_id),
                provider_id,
                source,
            )
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
                        value: DirentsPayload {
                            entries: Vec::new(),
                            exhaustive: true,
                            validator: None,
                            next_cursor: None,
                            paginated: false,
                        },
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
                        value: DirentsPayload {
                            entries: Vec::new(),
                            exhaustive: true,
                            validator: None,
                            next_cursor: None,
                            paginated: false,
                        },
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
        assert_eq!(listing.entries[0].name, "child");
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
            store.dirents_payload(&parent).unwrap().unwrap().entries[0]
                .meta
                .is_file()
        );

        publish_negative(&store, &child, Some(OBJ_ID.to_vec()), 30_000);
        let listing = store.dirents_payload(&parent).unwrap().unwrap();
        assert!(listing.entries.is_empty());
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
                        value: DirentsPayload {
                            entries: vec![crate::view::DirentRecord {
                                name: "child".into(),
                                meta: EntryMeta::directory(),
                            }],
                            exhaustive: true,
                            validator: Some("validator".into()),
                            next_cursor: None,
                            paginated: false,
                        },
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
        assert!(listing.entries.is_empty());
        assert!(listing.validator.is_none());
        assert!(!listing.is_complete_offline());

        let partial_parent = p("/partial");
        let partial_child = p("/partial/absent");
        store
            .publish(
                ProjectionTransition {
                    dirents: vec![DirentsMutation::Replace {
                        path: partial_parent.clone(),
                        value: DirentsPayload {
                            entries: vec![crate::view::DirentRecord {
                                name: "sibling".into(),
                                meta: EntryMeta::directory(),
                            }],
                            exhaustive: false,
                            validator: Some("cursor-validator".into()),
                            next_cursor: Some(crate::view::CachedCursor::Opaque("cursor".into())),
                            paginated: true,
                        },
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
        assert_eq!(listing.entries.len(), 1);
        assert_eq!(listing.entries[0].name, "sibling");
        assert!(listing.validator.is_none());
        assert!(listing.next_cursor.is_none());
        assert!(!listing.paginated);
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
                        value: DirentsPayload {
                            entries: vec![crate::view::DirentRecord {
                                name: "reverse".into(),
                                meta: EntryMeta::directory(),
                            }],
                            exhaustive: true,
                            validator: None,
                            next_cursor: None,
                            paginated: false,
                        },
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
                    }],
                    ..ProjectionTransition::default()
                },
                store.current_epoch(),
            )
            .unwrap();
        publish_negative(&store, &stale_git, None, 40_000);
        assert!(store.git_for_path(&stale_git).unwrap().is_none());
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
