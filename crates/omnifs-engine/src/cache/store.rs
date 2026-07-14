//! Host cache byte storage primitives.
//!
//! `view::Cache` owns both the in-memory moka mem and the durable fjall
//! backing behind one API. Cache entries do not carry TTLs: eviction is driven
//! purely by capacity and explicit invalidation (`invalidate_prefix` or
//! host-applied invalidation effects).
//!
//! ## Global caches, per-mount facade
//!
//! `Caches` holds the two fjall databases (a durable `object/` and a
//! non-durable `view/` cleared on startup). It is opened once at process start
//! and shared via `Arc`. `Caches::mount(name)` returns a per-mount `Store`. The
//! object tier is isolated per mount by its own keyspaces (raw keys); the
//! shared view tier is isolated by a `/{mount}` path prefix on its keys.
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

use omnifs_core::path::{Path, Segment};
use std::collections::HashSet;
use std::path::Path as StdPath;
use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};

use dashmap::DashMap;

use super::{object, view};

/// Shared handle to a host view store.
pub type Handle = Arc<Store>;

/// Result of a warm canonical lookup: the object id, the canonical bytes, and
/// the optional validator.
pub struct CachedCanonical {
    pub id: Vec<u8>,
    pub bytes: Vec<u8>,
    pub validator: Option<String>,
}

/// One entry for `Store::put_canonical_batch`.
pub struct CanonicalBatchEntry {
    pub id: Vec<u8>,
    pub bytes: Vec<u8>,
    pub validator: Option<String>,
    pub view_leaves: Vec<Path>,
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
    pub path: Path,
    pub kind: RecordKind,
    pub aux: Option<String>,
}

impl Key {
    pub fn new(path: &Path, kind: RecordKind) -> Self {
        Self {
            path: path.clone(),
            kind,
            aux: None,
        }
    }

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
    pub schema_version: u8,
    pub kind: RecordKind,
    pub payload: Vec<u8>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct BatchRecord {
    pub path: Path,
    pub kind: RecordKind,
    pub aux: Option<String>,
    pub record: Record,
}

impl BatchRecord {
    pub fn new(path: Path, kind: RecordKind, aux: Option<String>, record: Record) -> Self {
        Self {
            path,
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

/// Soft cap on retained negatives per mount. `gc_negatives` fires past this.
const NEGATIVES_SOFT_CAP: usize = 4096;

/// Wall-clock milliseconds since the Unix epoch. Used only for GC sweep timing
/// so that `delete_object` does not need a caller-supplied clock argument.
fn now_millis_for_gc() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map_or(0, |d| u64::try_from(d.as_millis()).unwrap_or(u64::MAX))
}

/// Process-global cache handles. Opened once at startup; shared via `Arc`.
///
/// `Caches::open(dir)` opens a durable `object/` database and clears and
/// reopens a `view/` database, which is always cold after restart.
pub struct Caches {
    pub object: object::Cache,
    pub view: view::Cache,
}

impl Caches {
    /// Open the global cache handles from `dir`.
    ///
    /// Opens `dir/object/` (durable) and clears and reopens `dir/view/`
    /// (non-durable, always cold).
    pub fn open(dir: &StdPath) -> anyhow::Result<Arc<Self>> {
        std::fs::create_dir_all(dir)?;
        let object = object::Cache::open(&dir.join("object"))?;
        let view = view::Cache::open(&dir.join("view"))?;
        Ok(Arc::new(Self { object, view }))
    }

    /// Return a per-mount `Store` facade. The object tier gets its own
    /// keyspaces (raw keys); view keys are scoped by `Store::scoped`.
    pub fn mount(self: &Arc<Self>, mount: impl Into<String>) -> Store {
        Store::new(Arc::clone(self), mount)
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
/// The object tier is structurally isolated per mount (its own keyspaces), so
/// its keys are raw: object ids and view-leaf paths carry no mount prefix. The
/// shared view tier is isolated by a `/{mount}` path prefix on its keys
/// (`Store::scoped`).
///
/// The generation fence is per-mount and runtime-only: `generation`,
/// `tombstones`, and `negatives` are reset on construction and never persisted.
pub struct Store {
    caches: Arc<Caches>,
    /// This mount's object keyspaces (raw-keyed).
    object: object::MountObjects,
    mount: String,
    generation: AtomicU64,
    /// `ObjectId` bytes → generation at which the id was invalidated.
    tombstones: DashMap<Vec<u8>, u64>,
    /// Unscoped path → negative record.
    negatives: DashMap<String, Negative>,
    /// `ObjectId` bytes → unscoped paths with negatives.
    neg_by_id: DashMap<Vec<u8>, HashSet<String>>,
}

impl Store {
    fn new(caches: Arc<Caches>, mount: impl Into<String>) -> Self {
        let mount = mount.into();
        // Mount names must be a single path segment; fail fast at construction
        // so `scope_unscoped` can build keys without re-validating.
        Segment::try_from(mount.as_str()).expect("store mount must be a path segment");
        let object = caches
            .object
            .mount(&mount)
            .expect("open object keyspaces for mount");
        Self {
            caches,
            object,
            mount,
            generation: AtomicU64::new(0),
            tombstones: DashMap::new(),
            negatives: DashMap::new(),
            neg_by_id: DashMap::new(),
        }
    }

    /// Scoped view-cache key as a valid protocol path: `/{mount}{path}`.
    fn scoped(&self, path: &Path) -> Path {
        self.scope_unscoped(path.as_str())
    }

    /// Scope an already-valid, unscoped protocol-path string into this mount's
    /// view-cache key. Both `unscoped` and `self.mount` are validated, so the
    /// key is built by concatenation with no re-parsing: root scopes to
    /// `/{mount}` (no trailing slash), any other path appends its leading-slash
    /// string to `/{mount}`.
    fn scope_unscoped(&self, unscoped: &str) -> Path {
        if unscoped == "/" {
            Path::from_validated(format!("/{}", self.mount))
        } else {
            Path::from_validated(format!("/{}{}", self.mount, unscoped))
        }
    }

    fn id_tombstoned_after(&self, id: &[u8], op_gen: u64) -> bool {
        self.tombstones.get(id).is_some_and(|g| *g > op_gen)
    }

    /// Current per-mount generation. Capture this before starting a browse op
    /// and pass it back as `op_gen` to `put_canonical_batch`.
    pub fn current_generation(&self) -> u64 {
        self.generation.load(Ordering::Acquire)
    }

    /// Whether a view write for `path` derived at `op_gen` must be dropped
    /// because the path's object id carries a tombstone newer than `op_gen`.
    pub fn write_fenced(&self, path: &Path, op_gen: u64) -> bool {
        let Some(id) = self.object.id_of(path.as_str().as_bytes()) else {
            return false;
        };
        self.id_tombstoned_after(&id, op_gen)
    }

    // --- View cache reads -----------------------------------------------------

    pub fn cache_get(&self, path: &Path, kind: RecordKind, aux: Option<&str>) -> Option<Record> {
        self.caches
            .view
            .get(&Key::with_aux(&self.scoped(path), kind, aux))
            .map(|arc| (*arc).clone())
    }

    pub fn cache_put(&self, path: &Path, kind: RecordKind, aux: Option<&str>, record: &Record) {
        self.caches
            .view
            .put(&Key::with_aux(&self.scoped(path), kind, aux), record);
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

    pub fn mem_get(&self, path: &Path, kind: RecordKind, aux: Option<&str>) -> Option<Arc<Record>> {
        self.caches
            .view
            .mem_get(&Key::with_aux(&self.scoped(path), kind, aux))
    }

    pub fn mem_invalidate(&self, path: &Path, kind: RecordKind, aux: Option<&str>) {
        self.caches
            .view
            .mem_invalidate(&Key::with_aux(&self.scoped(path), kind, aux));
    }

    pub fn mem_invalidate_entries_if<P>(&self, predicate: P)
    where
        P: Fn(&Key, &Arc<Record>) -> bool + Send + Sync + 'static,
    {
        // The cache sees scoped keys, but callers predicate on mount-local
        // paths; strip this store's mount prefix before delegating.
        let mount_prefix = self.scoped(&Path::root());
        self.caches.view.mem_invalidate_entries_if(move |k, v| {
            // Only match keys belonging to this mount.
            if !k.path.has_prefix(&mount_prefix) {
                return false;
            }
            // Strip the mount prefix before passing to the caller's predicate.
            let Some(path) = k.path.strip_prefix(&mount_prefix) else {
                return false;
            };
            let unscoped_key = Key {
                path,
                kind: k.kind,
                aux: k.aux.clone(),
            };
            predicate(&unscoped_key, v)
        });
    }

    // --- Canonical object cache -----------------------------------------------

    /// Warm-read input: path → id → bytes + validator. Returns the raw object
    /// id. `None` when no canonical is indexed.
    pub fn cached_canonical_for(&self, path: &Path) -> Option<CachedCanonical> {
        let id = self.object.id_of(path.as_str().as_bytes())?;
        let obj = self.object.get(&id)?;
        let canonical = obj.canonical?;
        Some(CachedCanonical {
            id,
            bytes: canonical.bytes,
            validator: canonical.validator,
        })
    }

    /// Batch canonical store for effect application. Fenced per-entry; rejected
    /// entries are skipped and the rest of the batch proceeds. Prior-leaf view
    /// evictions fire per-object before the batch commit.
    ///
    /// Ownership is consumed so the caller need not clone; the function drains
    /// the Vec.
    pub fn put_canonical_batch(&self, entries: Vec<CanonicalBatchEntry>, op_gen: u64) -> bool {
        let view = &self.caches.view;

        // Per-entry fence check and view eviction; collect accepted entries.
        let batch: Vec<object::StoreBatchEntry> = entries
            .into_iter()
            .filter_map(|entry| {
                if self.id_tombstoned_after(&entry.id, op_gen) {
                    return None;
                }
                // Evict prior view leaves before the object is replaced.
                for leaf in self.object.leaves_of(&entry.id) {
                    view.delete_exact(&self.scope_unscoped(&leaf));
                }
                let leaves: Vec<String> = entry
                    .view_leaves
                    .iter()
                    .map(|p| p.as_str().to_string())
                    .collect();
                Some(object::StoreBatchEntry {
                    id: entry.id,
                    canonical: object::StoredObject {
                        bytes: entry.bytes,
                        validator: entry.validator,
                    },
                    new_leaves: leaves,
                })
            })
            .collect();

        self.object.store_batch(&batch);
        true
    }

    /// Preload index-only store, fenced. Canonical-beats-preload in the object tier.
    pub fn put_index_only(&self, id: &[u8], view_leaves: &[Path], op_gen: u64) -> bool {
        if self.id_tombstoned_after(id, op_gen) {
            return false;
        }
        let leaves: Vec<String> = view_leaves.iter().map(|p| p.as_str().to_string()).collect();
        self.object.store_index_only(id, &leaves)
    }

    /// Store a fenced negative for `path`. Rejected when the id tombstone is newer than `op_gen`.
    pub fn put_negative(
        &self,
        path: &Path,
        id: Option<&[u8]>,
        op_gen: u64,
        ttl_millis: u64,
        now_millis: u64,
    ) -> bool {
        if let Some(id) = id
            && self.id_tombstoned_after(id, op_gen)
        {
            return false;
        }

        let path_key = path.as_str().to_string();
        let expires_at = ttl_millis
            .checked_add(now_millis)
            .filter(|_| ttl_millis > 0);
        let neg = Negative {
            id: id.map(<[u8]>::to_vec),
            expires_at,
            as_of_gen: op_gen,
        };
        self.negatives.insert(path_key.clone(), neg);
        if let Some(id) = id {
            self.neg_by_id
                .entry(id.to_vec())
                .or_default()
                .insert(path_key);
        }
        true
    }

    /// Forward index: path → object id bytes.
    pub fn id_of_path(&self, path: &Path) -> Option<Vec<u8>> {
        self.object.id_of(path.as_str().as_bytes())
    }

    /// Reverse index: object id bytes → current alias paths.
    pub fn paths_for_id(&self, id: &[u8]) -> Vec<Path> {
        self.object
            .leaves_of(id)
            .into_iter()
            .filter_map(|leaf| Path::parse(&leaf).ok())
            .collect()
    }

    /// Write a view leaf's records and its shared expiry stamp.
    pub fn cache_view_leaf(
        &self,
        path: &Path,
        records: &[BatchRecord],
        expires_at: Option<u64>,
        op_gen: u64,
    ) -> bool {
        if self.write_fenced(path, op_gen) {
            return false;
        }
        self.cache_put_batch(records);
        self.caches.view.put_expiry(
            self.scoped(path).as_str(),
            view::Expiry {
                expires_at,
                generation: op_gen,
            },
        );
        true
    }

    /// Expiry-aware view read: returns `None` when the leaf is past its deadline.
    pub fn view_get(
        &self,
        path: &Path,
        kind: RecordKind,
        aux: Option<&str>,
        now_millis: u64,
    ) -> Option<Record> {
        let scoped = self.scoped(path);
        if let Some(f) = self.caches.view.get_expiry(scoped.as_str())
            && f.expires_at.is_some_and(|exp| now_millis >= exp)
        {
            return None;
        }
        self.cache_get(path, kind, aux)
    }

    /// Atomically update one view record. The cache supplies raw bytes only;
    /// callers own payload decoding, merging, and re-encoding. `update` may be
    /// rerun on a write-write conflict, so it must be a pure function of its
    /// input.
    pub fn update_metadata_record<F>(
        &self,
        path: &Path,
        kind: RecordKind,
        aux: Option<&str>,
        update: F,
    ) where
        F: FnMut(Option<Record>) -> Option<Record>,
    {
        let key = Key::with_aux(&self.scoped(path), kind, aux);
        self.caches.view.update_metadata_record(&key, update);
    }

    /// Live negative for `path`. `None` when absent or expired.
    pub fn negative_for(&self, path: &Path, now_millis: u64) -> Option<Negative> {
        let neg = self.negatives.get(path.as_str())?;
        if neg.expires_at.is_some_and(|exp| now_millis >= exp) {
            return None;
        }
        Some(neg.clone())
    }

    // --- Invalidation ---------------------------------------------------------

    /// Invalidate object(id): bump gen, tombstone id, evict object + index + negatives.
    pub fn delete_object(&self, id: &[u8]) {
        let g = self.generation.fetch_add(1, Ordering::AcqRel) + 1;
        self.tombstones.insert(id.to_vec(), g);
        self.clear_negatives_for_id(id);

        let view = &self.caches.view;
        self.object.evict_object(id, |leaf| {
            view.delete_exact(&self.scope_unscoped(leaf));
        });
        self.gc_tombstones();
        if self.negatives.len() > NEGATIVES_SOFT_CAP {
            self.gc_negatives(now_millis_for_gc());
        }
    }

    /// View-only listing invalidation at an exact path.
    pub fn delete_listing_path(&self, path: &Path) {
        self.caches.view.delete_exact(&self.scoped(path));
    }

    /// View-only listing invalidation under a prefix (segment boundary).
    pub fn delete_listing_prefix(&self, prefix: &Path) {
        self.caches.view.invalidate_prefix(&self.scoped(prefix));
    }

    // --- Private helpers ------------------------------------------------------

    fn clear_negatives_for_id(&self, id: &[u8]) {
        if let Some((_, paths)) = self.neg_by_id.remove(id) {
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

    /// Prune expired negative entries from `negatives` and keep `neg_by_id`
    /// consistent. The caller is responsible for checking the soft cap before
    /// calling (see `delete_object`).
    fn gc_negatives(&self, now_millis: u64) {
        // Collect paths of expired entries.
        let expired: Vec<String> = self
            .negatives
            .iter()
            .filter_map(|entry| {
                let expired = entry
                    .value()
                    .expires_at
                    .is_some_and(|exp| now_millis >= exp);
                expired.then(|| entry.key().clone())
            })
            .collect();

        for path in &expired {
            if let Some((_, neg)) = self.negatives.remove(path) {
                // Drop this path from the reverse index; remove empty sets.
                if let Some(id) = &neg.id {
                    if let Some(mut paths_set) = self.neg_by_id.get_mut(id) {
                        paths_set.remove(path);
                    }
                    // Remove the reverse-index entry if its set is now empty.
                    self.neg_by_id
                        .remove_if(id, |_, paths_set| paths_set.is_empty());
                }
            }
        }
    }
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

    fn p(path: &str) -> Path {
        Path::parse(path).unwrap()
    }

    const OBJ_ID: &[u8] = b"issue:42";

    #[test]
    fn identity_collapse() {
        let (_dir, _caches, store) = open_store("gh");
        let leaves = [
            p("/issues/open/42/item.json"),
            p("/issues/all/42/item.json"),
        ];
        assert!(store.put_canonical_batch(
            vec![CanonicalBatchEntry {
                id: OBJ_ID.to_vec(),
                bytes: b"payload".to_vec(),
                validator: None,
                view_leaves: leaves.to_vec(),
            }],
            store.current_generation(),
        ));

        assert_eq!(
            store.object.id_of(b"/issues/open/42/item.json").as_deref(),
            Some(OBJ_ID)
        );
        assert_eq!(
            store.object.id_of(b"/issues/all/42/item.json").as_deref(),
            Some(OBJ_ID)
        );
        assert!(store.object.get(OBJ_ID).unwrap().canonical.is_some());
        let cached = store
            .cached_canonical_for(&p("/issues/open/42/item.json"))
            .unwrap();
        assert_eq!(cached.id, OBJ_ID);
    }

    #[test]
    fn mount_isolation() {
        let (_dir, caches, store_a) = open_store("a");
        let store_b = caches.mount("b");
        let path = "/issues/42/item.json";
        let op_gen = store_a.current_generation();

        assert!(store_a.put_canonical_batch(
            vec![CanonicalBatchEntry {
                id: OBJ_ID.to_vec(),
                bytes: b"from-a".to_vec(),
                validator: None,
                view_leaves: vec![p(path)],
            }],
            op_gen,
        ));
        assert!(store_b.put_canonical_batch(
            vec![CanonicalBatchEntry {
                id: OBJ_ID.to_vec(),
                bytes: b"from-b".to_vec(),
                validator: None,
                view_leaves: vec![p(path)],
            }],
            op_gen,
        ));

        let a = store_a.cached_canonical_for(&p(path)).unwrap();
        let b = store_b.cached_canonical_for(&p(path)).unwrap();
        assert_eq!(a.bytes, b"from-a");
        assert_eq!(b.bytes, b"from-b");
        assert_ne!(a.bytes, b.bytes);
    }

    #[test]
    fn overwrite_unions_aliases_keeps_index() {
        let (_dir, _caches, store) = open_store("m");
        let l1 = p("/p/L1");
        let l2 = p("/p/L2");

        store.put_canonical_batch(
            vec![CanonicalBatchEntry {
                id: OBJ_ID.to_vec(),
                bytes: b"v1".to_vec(),
                validator: None,
                view_leaves: vec![l1.clone()],
            }],
            0,
        );

        let record = Record::new(RecordKind::File, vec![9, 9, 9]);
        store.cache_put(&p(&l1), RecordKind::File, None, &record);

        assert!(store.put_canonical_batch(
            vec![CanonicalBatchEntry {
                id: OBJ_ID.to_vec(),
                bytes: b"v2".to_vec(),
                validator: None,
                view_leaves: vec![l2.clone()],
            }],
            0,
        ));

        let obj = store.object.get(OBJ_ID).unwrap();
        assert!(obj.leaves.iter().any(|p| p.ends_with("/p/L1")));
        assert!(obj.leaves.iter().any(|p| p.ends_with("/p/L2")));
        assert_eq!(
            store.object.id_of(l1.as_str().as_bytes()).as_deref(),
            Some(OBJ_ID)
        );
        assert!(
            store.cache_get(&p(&l1), RecordKind::File, None).is_none(),
            "rendered view for L1 should have been evicted on overwrite"
        );
    }

    #[test]
    fn delete_object_removes_index() {
        let (_dir, _caches, store) = open_store("m");
        let leaf = p("/issues/42/item.json");
        store.put_canonical_batch(
            vec![CanonicalBatchEntry {
                id: OBJ_ID.to_vec(),
                bytes: b"data".to_vec(),
                validator: None,
                view_leaves: vec![leaf.clone()],
            }],
            0,
        );
        assert!(store.put_negative(&p(&leaf), Some(OBJ_ID), 0, 10_000, 1_000));

        store.delete_object(OBJ_ID);

        assert!(store.object.get(OBJ_ID).is_none());
        assert!(store.object.id_of(leaf.as_str().as_bytes()).is_none());
        assert!(store.negative_for(&p(&leaf), 1_000).is_none());
    }

    #[test]
    fn delete_listing_keeps_canonicals() {
        let (_dir, _caches, store) = open_store("m");
        let leaf = p("/dir/child.json");
        store.put_canonical_batch(
            vec![CanonicalBatchEntry {
                id: OBJ_ID.to_vec(),
                bytes: b"data".to_vec(),
                validator: None,
                view_leaves: vec![leaf.clone()],
            }],
            0,
        );

        store.cache_put(
            &p("/dir"),
            RecordKind::Dirents,
            None,
            &Record::new(RecordKind::Dirents, b"dirents".to_vec()),
        );

        store.delete_listing_prefix(&p("/dir"));

        assert!(store.object.get(OBJ_ID).unwrap().canonical.is_some());
        assert_eq!(
            store.object.id_of(leaf.as_str().as_bytes()).as_deref(),
            Some(OBJ_ID)
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
        assert!(store.put_canonical_batch(
            vec![CanonicalBatchEntry {
                id: OBJ_ID.to_vec(),
                bytes: b"late".to_vec(),
                validator: None,
                view_leaves: vec![p("/x")],
            }],
            op_gen,
        ));
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

    /// `gc_negatives` prunes expired entries, keeps fresh ones, and leaves
    /// `neg_by_id` consistent after the sweep.
    #[test]
    fn gc_negatives_prunes_expired_keeps_fresh_and_stays_consistent() {
        let (_dir, _caches, store) = open_store("m");
        let now = 5_000_u64;
        let expired_path = "/issues/1/missing";
        let fresh_path = "/issues/2/missing";
        let no_ttl_path = "/issues/3/missing";

        let id_expired = b"obj:expired" as &[u8];
        let id_fresh = b"obj:fresh" as &[u8];

        // Expired: TTL puts deadline in the past relative to `now`.
        assert!(store.put_negative(&p(expired_path), Some(id_expired), 0, 1_000, 1_000));
        // Fresh: deadline is in the future relative to `now`.
        assert!(store.put_negative(&p(fresh_path), Some(id_fresh), 0, 10_000, now));
        // No TTL (no expiry): must never be swept.
        assert!(store.put_negative(&p(no_ttl_path), None, 0, 0, now));

        // Force gc_negatives by lowering the negatives count below threshold is
        // impractical for a unit test, so call the private helper directly.
        store.gc_negatives(now);

        // The expired negative must be gone.
        assert!(
            store.negative_for(&p(expired_path), now).is_none(),
            "expired negative should have been pruned"
        );
        // The fresh negative must survive.
        assert!(
            store.negative_for(&p(fresh_path), now).is_some(),
            "fresh negative should be retained"
        );
        // The no-TTL negative must survive.
        assert!(
            store.negative_for(&p(no_ttl_path), now).is_some(),
            "no-TTL negative should be retained"
        );

        // Reverse index consistency: id_expired must have no entry (or empty set).
        assert!(
            store.neg_by_id.get(id_expired).is_none_or(|s| s.is_empty()),
            "neg_by_id for expired id should be absent or empty"
        );
        assert!(
            store.neg_by_id.get(id_fresh).is_some_and(|s| !s.is_empty()),
            "neg_by_id for fresh id should still have entries"
        );
    }
}
