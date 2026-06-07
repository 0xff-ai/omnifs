//! Host cache byte storage primitives.
//!
//! `view::Cache` owns both the in-memory moka mem and the durable fjall
//! backing behind one API. Cache entries do not carry TTLs: eviction is driven
//! purely by capacity and explicit invalidation (`invalidate_prefix` or
//! host-applied invalidation effects).
//!
//! ## Global caches, per-mount facade
//!
//! `Caches` holds the two global cache handles (one durable `object` keyspace
//! and one non-durable `view` keyspace deleted on startup). It is opened once
//! at process start and shared via `Arc`. `Caches::mount(name)` returns a
//! per-mount `Store` that scopes all keys with `"{mount}\x1f{key}"`.
//!
//! The per-mount generation fence lives in `Store`: each `Store` owns an
//! atomic generation counter and a tombstone map. Object writes are rejected
//! if their tombstone is newer than the originating op's `op_gen`.

/// On-disk schema version for view records. Bump on any encoding-affecting
/// change to host-owned cached payload types. The cache reader rejects records
/// whose first byte does not match this constant, so a bump invalidates stale
/// entries without an explicit purge.
///
/// Any PR that touches the on-disk encoding must include a postcard
/// fixture round-trip test against the new version.
pub const SCHEMA_VERSION: u8 = 7;

pub mod object;
pub mod view;

use omnifs_core::path::Path as ProtocolPath;
use std::collections::HashSet;
use std::path::Path;
use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};

use dashmap::DashMap;

/// Shared handle to a host view store.
pub type Handle = Arc<Store>;

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

    fn from_u8(v: u8) -> Option<Self> {
        match v {
            0 => Some(Self::Lookup),
            1 => Some(Self::Attr),
            2 => Some(Self::Dirents),
            3 => Some(Self::File),
            _ => None,
        }
    }
}

#[derive(Debug, Clone, Eq, Hash, PartialEq)]
pub struct Key {
    pub path: String,
    pub kind: RecordKind,
    pub aux: Option<String>,
}

impl Key {
    pub fn new(path: impl Into<String>, kind: RecordKind) -> Self {
        Self {
            path: path.into(),
            kind,
            aux: None,
        }
    }

    pub fn with_aux(
        path: impl Into<String>,
        kind: RecordKind,
        aux: Option<impl Into<String>>,
    ) -> Self {
        Self {
            path: path.into(),
            kind,
            aux: aux.map(Into::into),
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Record {
    pub schema_version: u8,
    pub kind: RecordKind,
    pub payload: Vec<u8>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct BatchRecord {
    pub path: String,
    pub kind: RecordKind,
    pub aux: Option<String>,
    pub record: Record,
}

impl BatchRecord {
    pub fn new(
        path: impl Into<String>,
        kind: RecordKind,
        aux: Option<String>,
        record: Record,
    ) -> Self {
        Self {
            path: path.into(),
            kind,
            aux,
            record,
        }
    }
}

impl Record {
    pub fn new(kind: RecordKind, payload: Vec<u8>) -> Self {
        Self {
            schema_version: SCHEMA_VERSION,
            kind,
            payload,
        }
    }

    /// Serialize to bytes: `[schema_version:1][kind:1][payload:*]`.
    pub fn serialize(&self) -> Vec<u8> {
        let mut buf = Vec::with_capacity(2 + self.payload.len());
        buf.push(self.schema_version);
        buf.push(self.kind as u8);
        buf.extend_from_slice(&self.payload);
        buf
    }

    pub fn deserialize(bytes: &[u8]) -> Option<Self> {
        if bytes.len() < 2 {
            return None;
        }
        if bytes[0] != SCHEMA_VERSION {
            return None;
        }
        let kind = RecordKind::from_u8(bytes[1])?;
        let payload = bytes[2..].to_vec();
        Some(Self {
            schema_version: SCHEMA_VERSION,
            kind,
            payload,
        })
    }
}

/// Soft cap on retained tombstones per mount. GC fires past this to amortise
/// the scan. See `Store::gc_tombstones`.
const TOMBSTONE_SOFT_CAP: usize = 4096;

/// Generations of tombstone history retained after GC.
const TOMBSTONE_RETAIN_GENERATIONS: u64 = 1024;

/// Process-global cache handles. Opened once at startup; shared via `Arc`.
///
/// `Caches::open(dir)` creates a durable `object` keyspace and deletes+recreates
/// the `view` keyspace (always cold after restart, Codex #5).
pub struct Caches {
    pub object: object::Cache,
    pub view: view::Cache,
}

impl Caches {
    /// Open the global cache handles from `dir`.
    ///
    /// Creates `dir/object` (durable) and deletes+recreates
    /// `dir/view` (non-durable, always cold, Codex #5).
    pub fn open(dir: &Path) -> anyhow::Result<Arc<Self>> {
        std::fs::create_dir_all(dir)?;
        let object = object::Cache::open(&dir.join("object"))?;
        let view = view::Cache::open(&dir.join("view"))?;
        Ok(Arc::new(Self { object, view }))
    }

    /// Return a per-mount `Store` facade. All keys for this store are scoped
    /// with `"{mount}\x1f"` by `Store::scoped`.
    pub fn mount(self: &Arc<Self>, mount: impl Into<String>) -> Store {
        Store {
            caches: Arc::clone(self),
            mount: mount.into(),
            generation: AtomicU64::new(0),
            tombstones: DashMap::new(),
            negatives: DashMap::new(),
            neg_by_id: DashMap::new(),
        }
    }
}

/// Scoped negative cache entry for a `NotFound` terminal.
#[derive(Clone)]
pub struct Negative {
    pub id: Option<Vec<u8>>,
    pub expires_at: Option<u64>,
    pub as_of_gen: u64,
}

/// Per-mount facade over the global `Caches`.
///
/// All public methods take unscoped `path` strings and opaque `id` bytes
/// (without the mount prefix). Scoping helpers inject the `mount\x1f` prefix.
///
/// The generation fence is per-mount and runtime-only: `generation`,
/// `tombstones`, and `negatives` are reset on construction and never persisted.
pub struct Store {
    caches: Arc<Caches>,
    mount: String,
    generation: AtomicU64,
    /// Scoped `ObjectId` bytes → generation at which the id was invalidated.
    tombstones: DashMap<Vec<u8>, u64>,
    /// Scoped path → negative record.
    negatives: DashMap<String, Negative>,
    /// Scoped `ObjectId` bytes → scoped paths with negatives.
    neg_by_id: DashMap<Vec<u8>, HashSet<String>>,
}

impl Store {
    /// Construct an in-memory-only `Store` backed by an in-memory view cache
    /// and no durable object cache. Used by tests that don't need persistence.
    pub fn new_in_memory(mount: impl Into<String>) -> Self {
        // Build a minimal Caches with in-memory caches.
        // The object cache is always durable and needs a real fjall keyspace
        // directory; for in-memory use a temp directory that we leak so the
        // keyspace keeps a live backing path. Tests that need the object cache
        // should use `Caches::open`.
        let caches = Arc::new(Caches {
            object: {
                let dir = tempfile::tempdir().expect("tempdir for in-memory object cache");
                // Persist (leak) the temp dir: fjall actively writes journal
                // and SST files, so the directory must outlive this scope.
                let path = dir.keep();
                object::Cache::open(&path.join("object")).expect("in-memory object cache")
            },
            view: view::Cache::new(),
        });
        Self {
            caches,
            mount: mount.into(),
            generation: AtomicU64::new(0),
            tombstones: DashMap::new(),
            negatives: DashMap::new(),
            neg_by_id: DashMap::new(),
        }
    }

    /// Scoped view/path key: `"{mount}\x1f{path}"`.
    fn scoped(&self, key: &str) -> String {
        format!("{}\x1f{key}", self.mount)
    }

    /// Scoped `ObjectId` bytes: `mount.as_bytes() ++ [0x1F] ++ id`.
    fn scoped_id(&self, id: &[u8]) -> Vec<u8> {
        let mut key = self.mount.as_bytes().to_vec();
        key.push(0x1f);
        key.extend_from_slice(id);
        key
    }

    /// Scoped path bytes for the object forward index.
    fn scoped_path_bytes(&self, path: &str) -> Vec<u8> {
        let mut key = self.mount.as_bytes().to_vec();
        key.push(0x1f);
        key.extend_from_slice(path.as_bytes());
        key
    }

    fn host_id_from_scoped(&self, scoped_id: &[u8]) -> Option<Vec<u8>> {
        let prefix = self.mount.as_bytes();
        if scoped_id.len() <= prefix.len() + 1 {
            return None;
        }
        if scoped_id[..prefix.len()] != prefix[..] || scoped_id[prefix.len()] != 0x1f {
            return None;
        }
        Some(scoped_id[prefix.len() + 1..].to_vec())
    }

    fn id_tombstoned_after(&self, scoped_id: &[u8], op_gen: u64) -> bool {
        self.tombstones.get(scoped_id).is_some_and(|g| *g > op_gen)
    }

    /// Current per-mount generation. Capture this before starting a browse op
    /// and pass it back as `op_gen` to `put_canonical`.
    pub fn current_generation(&self) -> u64 {
        self.generation.load(Ordering::Acquire)
    }

    /// Whether a view write for `path` derived at `op_gen` must be dropped
    /// because the path's object id carries a tombstone newer than `op_gen`.
    pub fn write_fenced(&self, path: &ProtocolPath, op_gen: u64) -> bool {
        let scoped_path = self.scoped_path_bytes(path.as_str());
        let Some(scoped_id) = self.caches.object.id_of(&scoped_path) else {
            return false;
        };
        self.id_tombstoned_after(&scoped_id, op_gen)
    }

    // --- View cache reads -----------------------------------------------------

    pub fn cache_get(
        &self,
        path: &ProtocolPath,
        kind: RecordKind,
        aux: Option<&str>,
    ) -> Option<Record> {
        self.caches
            .view
            .get(&Key::with_aux(self.scoped(path.as_str()), kind, aux))
            .map(|arc| (*arc).clone())
    }

    pub fn cache_put(
        &self,
        path: &ProtocolPath,
        kind: RecordKind,
        aux: Option<&str>,
        record: &Record,
    ) {
        self.caches.view.put(
            &Key::with_aux(self.scoped(path.as_str()), kind, aux),
            record,
        );
    }

    pub fn cache_put_batch(&self, records: &[BatchRecord]) {
        // The batch records carry unscoped paths; scope them before writing.
        let scoped: Vec<BatchRecord> = records
            .iter()
            .map(|r| {
                BatchRecord::new(
                    self.scoped(&r.path),
                    r.kind,
                    r.aux.clone(),
                    r.record.clone(),
                )
            })
            .collect();
        self.caches.view.put_batch(&scoped);
    }

    // --- Mem-only operations (FUSE pagination accumulator) ----------------

    pub fn mem_get(
        &self,
        path: &ProtocolPath,
        kind: RecordKind,
        aux: Option<&str>,
    ) -> Option<Arc<Record>> {
        self.caches
            .view
            .mem_get(&Key::with_aux(self.scoped(path.as_str()), kind, aux))
    }

    pub fn mem_invalidate(&self, path: &ProtocolPath, kind: RecordKind, aux: Option<&str>) {
        self.caches
            .view
            .mem_invalidate(&Key::with_aux(self.scoped(path.as_str()), kind, aux));
    }

    pub fn mem_invalidate_entries_if<P>(&self, predicate: P)
    where
        P: Fn(&Key, &Arc<Record>) -> bool + Send + Sync + 'static,
    {
        // The cache sees scoped keys, but callers predicate on mount-local
        // paths; strip this store's mount prefix before delegating.
        let mount_prefix = format!("{}\x1f", self.mount);
        self.caches.view.mem_invalidate_entries_if(move |k, v| {
            // Only match keys belonging to this mount.
            if !k.path.starts_with(mount_prefix.as_str()) {
                return false;
            }
            // Strip the mount prefix before passing to the caller's predicate.
            let unscoped_key = Key {
                path: k.path[mount_prefix.len()..].to_string(),
                kind: k.kind,
                aux: k.aux.clone(),
            };
            predicate(&unscoped_key, v)
        });
    }

    // --- Canonical object cache -----------------------------------------------

    /// Warm-read input: path → id → bytes + validator. Returns opaque host id
    /// bytes (mount prefix stripped). `None` when no canonical is indexed.
    pub fn cached_canonical_for(
        &self,
        path: &ProtocolPath,
    ) -> Option<(Vec<u8>, Vec<u8>, Option<String>)> {
        let scoped_path = self.scoped_path_bytes(path.as_str());
        let scoped_id = self.caches.object.id_of(&scoped_path)?;
        let obj = self.caches.object.get(&scoped_id)?;
        let canonical = obj.canonical?;
        let host_id = self.host_id_from_scoped(&obj.id)?;
        Some((host_id, canonical.bytes, canonical.validator))
    }

    /// Store a canonical object entry, gated on the per-mount id fence.
    pub fn put_canonical(
        &self,
        id: &[u8],
        bytes: Vec<u8>,
        validator: Option<String>,
        view_leaves: &[String],
        op_gen: u64,
    ) -> bool {
        let scoped_id = self.scoped_id(id);
        if self.id_tombstoned_after(&scoped_id, op_gen) {
            return false;
        }

        let scoped_leaves: Vec<String> = view_leaves.iter().map(|p| self.scoped(p)).collect();
        let canonical = object::Canonical { bytes, validator };
        let view = &self.caches.view;
        self.caches
            .object
            .store(&scoped_id, canonical, &scoped_leaves, |scoped_leaf| {
                view.delete_exact(scoped_leaf);
            })
    }

    /// Preload index-only store, fenced. Canonical-beats-preload in the object tier.
    pub fn put_index_only(&self, id: &[u8], view_leaves: &[String], op_gen: u64) -> bool {
        let scoped_id = self.scoped_id(id);
        if self.id_tombstoned_after(&scoped_id, op_gen) {
            return false;
        }
        let scoped_leaves: Vec<String> = view_leaves.iter().map(|p| self.scoped(p)).collect();
        self.caches
            .object
            .store_index_only(&scoped_id, &scoped_leaves)
    }

    /// Store a fenced negative for `path`. Rejected when the id tombstone is newer than `op_gen`.
    pub fn put_negative(
        &self,
        path: &ProtocolPath,
        id: Option<&[u8]>,
        op_gen: u64,
        ttl_millis: u64,
        now_millis: u64,
    ) -> bool {
        if let Some(id) = id {
            let scoped_id = self.scoped_id(id);
            if self.id_tombstoned_after(&scoped_id, op_gen) {
                return false;
            }
        }

        let scoped_path = self.scoped(path.as_str());
        let expires_at = ttl_millis
            .checked_add(now_millis)
            .filter(|_| ttl_millis > 0);
        let neg = Negative {
            id: id.map(|raw| self.scoped_id(raw)),
            expires_at,
            as_of_gen: op_gen,
        };
        self.negatives.insert(scoped_path.clone(), neg);
        if let Some(id) = id {
            self.neg_by_id
                .entry(self.scoped_id(id))
                .or_default()
                .insert(scoped_path);
        }
        true
    }

    /// Forward index: unscoped path → host `ObjectId` bytes (mount prefix stripped).
    pub fn id_of_path(&self, path: &ProtocolPath) -> Option<Vec<u8>> {
        let scoped_path = self.scoped_path_bytes(path.as_str());
        self.caches
            .object
            .id_of(&scoped_path)
            .and_then(|scoped_id| self.host_id_from_scoped(&scoped_id))
    }

    /// Reverse index: host `ObjectId` bytes → current alias paths (mount prefix stripped).
    pub fn paths_for_id(&self, id: &[u8]) -> Vec<String> {
        let scoped_id = self.scoped_id(id);
        let prefix = format!("{}\x1f", self.mount);
        self.caches
            .object
            .leaves_of(&scoped_id)
            .into_iter()
            .filter_map(|scoped_leaf| scoped_leaf.strip_prefix(&prefix).map(str::to_string))
            .collect()
    }

    /// Write a view leaf's records and its shared freshness stamp.
    pub fn cache_view_leaf(
        &self,
        path: &ProtocolPath,
        records: &[BatchRecord],
        expires_at: Option<u64>,
        op_gen: u64,
    ) -> bool {
        if self.write_fenced(path, op_gen) {
            return false;
        }
        self.cache_put_batch(records);
        self.caches.view.put_freshness(
            &self.scoped(path.as_str()),
            view::Freshness {
                expires_at,
                generation: op_gen,
            },
        );
        true
    }

    /// Freshness-aware view read: returns `None` when the leaf is past its deadline.
    pub fn view_get(
        &self,
        path: &ProtocolPath,
        kind: RecordKind,
        aux: Option<&str>,
        now_millis: u64,
    ) -> Option<Record> {
        let scoped = self.scoped(path.as_str());
        if let Some(f) = self.caches.view.get_freshness(&scoped)
            && f.expires_at.is_some_and(|exp| now_millis >= exp)
        {
            return None;
        }
        self.cache_get(path, kind, aux)
    }

    /// Atomically update one view record. The cache supplies raw bytes only;
    /// callers own payload decoding, merging, and re-encoding.
    pub fn update_metadata_record<F>(
        &self,
        path: &ProtocolPath,
        kind: RecordKind,
        aux: Option<&str>,
        update: F,
    ) where
        F: FnOnce(Option<Record>) -> Option<Record>,
    {
        let key = Key::with_aux(self.scoped(path.as_str()), kind, aux);
        self.caches.view.update_metadata_record(&key, update);
    }

    /// Live negative for `path`. `None` when absent or expired.
    pub fn negative_for(&self, path: &ProtocolPath, now_millis: u64) -> Option<Negative> {
        let scoped_path = self.scoped(path.as_str());
        let neg = self.negatives.get(&scoped_path)?;
        if neg.expires_at.is_some_and(|exp| now_millis >= exp) {
            return None;
        }
        Some(neg.clone())
    }

    // --- Invalidation ---------------------------------------------------------

    /// Invalidate object(id): bump gen, tombstone id, evict object + index + negatives.
    pub fn delete_object(&self, id: &[u8]) {
        let scoped_id = self.scoped_id(id);
        let g = self.generation.fetch_add(1, Ordering::AcqRel) + 1;
        self.tombstones.insert(scoped_id.clone(), g);
        self.clear_negatives_for_id(&scoped_id);

        let view = &self.caches.view;
        self.caches
            .object
            .evict_object(&scoped_id, |scoped_leaf| view.delete_exact(scoped_leaf));
        self.gc_tombstones();
    }

    /// View-only listing invalidation at an exact path.
    pub fn delete_listing_path(&self, path: &ProtocolPath) {
        self.caches.view.delete_exact(&self.scoped(path.as_str()));
    }

    /// View-only listing invalidation under a prefix (segment boundary).
    pub fn delete_listing_prefix(&self, prefix: &ProtocolPath) {
        self.caches
            .view
            .invalidate_scoped_prefix(&self.scoped(prefix.as_str()));
    }

    // --- Private helpers ------------------------------------------------------

    fn clear_negatives_for_id(&self, scoped_id: &[u8]) {
        if let Some((_, paths)) = self.neg_by_id.remove(scoped_id) {
            for path in paths {
                self.negatives.remove(&path);
            }
        }
    }

    fn gc_tombstones(&self) {
        if self.tombstones.len() <= TOMBSTONE_SOFT_CAP {
            return;
        }
        let cutoff = self
            .current_generation()
            .saturating_sub(TOMBSTONE_RETAIN_GENERATIONS);
        self.tombstones.retain(|_, g| *g >= cutoff);
    }
}

pub(crate) fn path_prefix_matches(prefix: &str, path: &str) -> bool {
    let Ok(prefix) = omnifs_core::path::Path::parse(prefix) else {
        return false;
    };
    let Ok(path) = omnifs_core::path::Path::parse(path) else {
        return false;
    };
    path.has_prefix(&prefix)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn open_store(mount: &str) -> (tempfile::TempDir, Arc<Caches>, Store) {
        let dir = tempfile::tempdir().unwrap();
        let caches = Caches::open(dir.path()).unwrap();
        let store = caches.mount(mount);
        (dir, caches, store)
    }

    fn test_scoped_id(mount: &str, id: &[u8]) -> Vec<u8> {
        let mut key = mount.as_bytes().to_vec();
        key.push(0x1f);
        key.extend_from_slice(id);
        key
    }

    fn test_scoped_path(mount: &str, path: &str) -> Vec<u8> {
        let mut key = mount.as_bytes().to_vec();
        key.push(0x1f);
        key.extend_from_slice(path.as_bytes());
        key
    }

    fn p(path: &str) -> ProtocolPath {
        ProtocolPath::parse(path).unwrap()
    }

    const OBJ_ID: &[u8] = b"issue:42";

    #[test]
    fn identity_collapse() {
        let (_dir, _caches, store) = open_store("gh");
        let leaves = [
            "/issues/open/42/item.json".to_string(),
            "/issues/all/42/item.json".to_string(),
        ];
        assert!(store.put_canonical(
            OBJ_ID,
            b"payload".to_vec(),
            None,
            &leaves,
            store.current_generation(),
        ));

        let p1 = test_scoped_path("gh", "/issues/open/42/item.json");
        let p2 = test_scoped_path("gh", "/issues/all/42/item.json");
        let scoped_id = test_scoped_id("gh", OBJ_ID);
        assert_eq!(
            store.caches.object.id_of(&p1).as_deref(),
            Some(scoped_id.as_slice())
        );
        assert_eq!(
            store.caches.object.id_of(&p2).as_deref(),
            Some(scoped_id.as_slice())
        );
        assert!(
            store
                .caches
                .object
                .get(&scoped_id)
                .unwrap()
                .canonical
                .is_some()
        );
        assert!(
            store
                .cached_canonical_for(&p("/issues/open/42/item.json"))
                .is_some()
        );
    }

    #[test]
    fn mount_isolation() {
        let (_dir, caches, store_a) = open_store("a");
        let store_b = caches.mount("b");
        let path = "/issues/42/item.json";
        let op_gen = store_a.current_generation();

        assert!(store_a.put_canonical(
            OBJ_ID,
            b"from-a".to_vec(),
            None,
            &[path.to_string()],
            op_gen,
        ));
        assert!(store_b.put_canonical(
            OBJ_ID,
            b"from-b".to_vec(),
            None,
            &[path.to_string()],
            op_gen,
        ));

        let a = store_a.cached_canonical_for(&p(path)).unwrap();
        let b = store_b.cached_canonical_for(&p(path)).unwrap();
        assert_eq!(a.1, b"from-a");
        assert_eq!(b.1, b"from-b");
        assert_ne!(a.1, b.1);
    }

    #[test]
    fn overwrite_unions_aliases_keeps_index() {
        let (_dir, _caches, store) = open_store("m");
        let l1 = "/p/L1".to_string();
        let l2 = "/p/L2".to_string();
        let scoped_id = test_scoped_id("m", OBJ_ID);

        store.put_canonical(OBJ_ID, b"v1".to_vec(), None, std::slice::from_ref(&l1), 0);

        let record = Record::new(RecordKind::File, vec![9, 9, 9]);
        store.cache_put(&p(&l1), RecordKind::File, None, &record);

        assert!(store.put_canonical(OBJ_ID, b"v2".to_vec(), None, std::slice::from_ref(&l2), 0));

        let obj = store.caches.object.get(&scoped_id).unwrap();
        assert!(obj.leaves.iter().any(|p| p.ends_with("/p/L1")));
        assert!(obj.leaves.iter().any(|p| p.ends_with("/p/L2")));
        assert_eq!(
            store
                .caches
                .object
                .id_of(&test_scoped_path("m", &l1))
                .as_deref(),
            Some(scoped_id.as_slice())
        );
        assert!(
            store.cache_get(&p(&l1), RecordKind::File, None).is_none(),
            "rendered view for L1 should have been evicted on overwrite"
        );
    }

    #[test]
    fn canonical_beats_preload() {
        let (_dir, _caches, store) = open_store("m");
        let scoped_id = test_scoped_id("m", OBJ_ID);
        let l1 = "/issues/42/item.json".to_string();
        assert!(store.put_canonical(
            OBJ_ID,
            b"data".to_vec(),
            Some("v1".to_string()),
            std::slice::from_ref(&l1),
            0,
        ));
        assert!(store.put_index_only(OBJ_ID, &["/issues/42/title".to_string()], 0));

        let got = store.caches.object.get(&scoped_id).unwrap();
        assert!(got.canonical.is_some());
        assert_eq!(got.canonical.as_ref().unwrap().bytes, b"data");
    }

    #[test]
    fn capacity_evict_keeps_index_drops_validator() {
        let (_dir, _caches, store) = open_store("m");
        let scoped_id = test_scoped_id("m", OBJ_ID);
        let leaf = "/a/leaf".to_string();
        store.put_canonical(
            OBJ_ID,
            b"data".to_vec(),
            Some("etag".to_string()),
            std::slice::from_ref(&leaf),
            0,
        );

        store.caches.object.capacity_evict(&scoped_id, |_| {});

        let got = store.caches.object.get(&scoped_id).unwrap();
        assert!(got.canonical.is_none());
        assert_eq!(
            store
                .caches
                .object
                .id_of(&test_scoped_path("m", &leaf))
                .as_deref(),
            Some(scoped_id.as_slice())
        );
    }

    #[test]
    fn delete_object_removes_index() {
        let (_dir, _caches, store) = open_store("m");
        let scoped_id = test_scoped_id("m", OBJ_ID);
        let leaf = "/issues/42/item.json".to_string();
        store.put_canonical(
            OBJ_ID,
            b"data".to_vec(),
            None,
            std::slice::from_ref(&leaf),
            0,
        );
        assert!(store.put_negative(&p(&leaf), Some(OBJ_ID), 0, 10_000, 1_000));

        store.delete_object(OBJ_ID);

        assert!(store.caches.object.get(&scoped_id).is_none());
        assert!(
            store
                .caches
                .object
                .id_of(&test_scoped_path("m", &leaf))
                .is_none()
        );
        assert!(store.negative_for(&p(&leaf), 1_000).is_none());
    }

    #[test]
    fn delete_listing_keeps_canonicals() {
        let (_dir, _caches, store) = open_store("m");
        let scoped_id = test_scoped_id("m", OBJ_ID);
        let leaf = "/dir/child.json".to_string();
        store.put_canonical(
            OBJ_ID,
            b"data".to_vec(),
            None,
            std::slice::from_ref(&leaf),
            0,
        );

        store.cache_put(
            &p("/dir"),
            RecordKind::Dirents,
            None,
            &Record::new(RecordKind::Dirents, b"dirents".to_vec()),
        );

        store.delete_listing_prefix(&p("/dir"));

        assert!(
            store
                .caches
                .object
                .get(&scoped_id)
                .unwrap()
                .canonical
                .is_some()
        );
        assert_eq!(
            store
                .caches
                .object
                .id_of(&test_scoped_path("m", &leaf))
                .as_deref(),
            Some(scoped_id.as_slice())
        );
        assert!(
            store
                .cache_get(&p("/dir"), RecordKind::Dirents, None)
                .is_none()
        );
    }

    #[test]
    fn fence_rejects_stale_write() {
        let (_dir, _caches, store) = open_store("m");
        let op_gen = store.current_generation();
        store.delete_object(OBJ_ID);
        assert!(!store.put_canonical(OBJ_ID, b"late".to_vec(), None, &["/x".to_string()], op_gen,));
        assert!(store.cached_canonical_for(&p("/x")).is_none());
    }

    #[test]
    fn fence_rejects_stale_negative() {
        let (_dir, _caches, store) = open_store("m");
        let op_gen = store.current_generation();
        store.delete_object(OBJ_ID);
        assert!(!store.put_negative(&p("/missing"), Some(OBJ_ID), op_gen, 10_000, 0));
    }

    #[test]
    fn negative_lifecycle() {
        let (_dir, _caches, store) = open_store("m");
        let path = "/issues/42/missing";
        let now = 1_000_u64;
        assert!(store.put_negative(&p(path), Some(OBJ_ID), 0, 10_000, now));
        assert!(store.negative_for(&p(path), now).is_some());
        assert!(store.negative_for(&p(path), now + 11_000).is_none());

        store.put_negative(&p(path), Some(OBJ_ID), 0, 10_000, now);
        store.delete_object(OBJ_ID);
        assert!(store.negative_for(&p(path), now).is_none());
    }

    #[test]
    fn delete_listing_prefix_evicts_exact_and_descendants_but_not_siblings() {
        let (_dir, _caches, store) = open_store("test");
        let record = Record::new(RecordKind::Attr, vec![1, 2, 3]);

        store.cache_put(&p("/owner/repo"), RecordKind::Attr, None, &record);
        store.cache_put(&p("/owner/repo/issues"), RecordKind::Attr, None, &record);
        store.cache_put(&p("/owner/repobaz"), RecordKind::Attr, None, &record);

        store.delete_listing_prefix(&p("/owner/repo"));

        assert!(
            store
                .cache_get(&p("/owner/repo"), RecordKind::Attr, None)
                .is_none(),
            "/owner/repo should be gone"
        );
        assert!(
            store
                .cache_get(&p("/owner/repo/issues"), RecordKind::Attr, None)
                .is_none(),
            "/owner/repo/issues should be gone"
        );
        assert!(
            store
                .cache_get(&p("/owner/repobaz"), RecordKind::Attr, None)
                .is_some(),
            "/owner/repobaz should remain"
        );
    }
}
