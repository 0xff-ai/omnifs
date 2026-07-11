//! Unified browse-cache tier: in-memory moka `mem` tier in front of a durable fjall `disk` tier.
//!
//! `Cache` owns both tiers behind one API. Reads check `mem` first and
//! promote `disk` hits into `mem`. Writes go to both. Invalidations
//! remove from both, keeping them coherent.
//!
//! The disk tier is a fjall optimistic-transaction database. Plain reads and
//! writes use the keyspaces directly (lock-free); the metadata read-modify-write
//! merge runs through `update_fetch`, which reruns on a write-write conflict and
//! always merges onto the latest value, so the lock-free plain writes never lose
//! a concurrent merge.

use super::store::{BatchRecord, Key, Record, RecordKind};
use anyhow::Result;
use fjall::{KeyspaceCreateOptions, OptimisticTxDatabase, OptimisticTxKeyspace};
use moka::sync::Cache as MokaCache;
use omnifs_core::path::Path;
use std::path::Path as StdPath;
use std::sync::Arc;

/// Maximum total byte weight of the `mem` tier per provider instance (32 MiB).
pub const VIEW_MEM_MAX_WEIGHT: u64 = 32 * 1024 * 1024;

/// Records larger than this threshold are not inserted into `mem` (256 KiB).
pub const VIEW_MEM_SKIP_THRESHOLD: usize = 256 * 1024;

/// Records larger than this threshold are stored in the bulk keyspace instead of
/// the content keyspace (64 KiB).
pub const VIEW_BULK_THRESHOLD: usize = 64 * 1024;

const METADATA_KEYSPACE: &str = "metadata";
const CONTENT_KEYSPACE: &str = "content";
const BULK_KEYSPACE: &str = "bulk";
const EXPIRY_KEYSPACE: &str = "expiry";

#[derive(serde::Serialize, serde::Deserialize, Clone, Copy)]
pub struct Expiry {
    pub expires_at: Option<u64>,
    pub generation: u64,
}

/// Durable view backing: a fjall optimistic-transaction database with one
/// keyspace per record class. The keyspaces are shared across mounts; mount
/// isolation comes from the `/{mount}` path prefix the caller bakes into keys.
struct Disk {
    metadata: OptimisticTxKeyspace,
    content: OptimisticTxKeyspace,
    bulk: OptimisticTxKeyspace,
    expiry: OptimisticTxKeyspace,
}

impl Disk {
    /// Keyspace a File record of `payload_len` bytes belongs in, or the metadata
    /// keyspace for non-File kinds.
    fn keyspace_for(&self, kind: RecordKind, payload_len: usize) -> &OptimisticTxKeyspace {
        match kind {
            RecordKind::File if payload_len >= VIEW_BULK_THRESHOLD => &self.bulk,
            RecordKind::File => &self.content,
            _ => &self.metadata,
        }
    }

    fn get(&self, key: &Key) -> Result<Option<Record>> {
        let wire_key = make_key(key);
        // For File records, check content first, then bulk.
        if key.kind == RecordKind::File {
            if let Some(record) = read_record(&self.content, &wire_key)? {
                return Ok(Some(record));
            }
            return read_record(&self.bulk, &wire_key);
        }
        read_record(&self.metadata, &wire_key)
    }

    fn put(&self, key: &Key, record: &Record) -> Result<()> {
        let wire_key = make_key(key);
        let bytes = record.serialize();
        let target = self.keyspace_for(key.kind, record.payload.len());
        target.insert(wire_key.as_bytes(), bytes.as_slice())?;
        // Drop any stale copy from the other file keyspace if the record crossed
        // the bulk threshold since its last write.
        if key.kind == RecordKind::File {
            let is_bulk = record.payload.len() >= VIEW_BULK_THRESHOLD;
            let other = if is_bulk { &self.content } else { &self.bulk };
            other.remove(wire_key.as_bytes())?;
        }
        Ok(())
    }

    fn put_batch(&self, records: &[BatchRecord]) -> Result<()> {
        for item in records {
            let key = Key::with_aux(&item.path, item.kind, item.aux.as_deref());
            self.put(&key, &item.record)?;
        }
        Ok(())
    }

    /// Atomic read-modify-write of one metadata record via `update_fetch`,
    /// which reruns `update` on a write-write conflict. Returns the merged
    /// record (or `None` when removed).
    fn update_metadata<F>(&self, key: &Key, mut update: F) -> Result<Option<Record>>
    where
        F: FnMut(Option<Record>) -> Option<Record>,
    {
        let wire_key = make_key(key);
        let new_value = self
            .metadata
            .update_fetch(wire_key.as_bytes(), |existing| {
                let existing = existing.and_then(|v| Record::deserialize(v));
                update(existing).map(|record| record.serialize().into())
            })?;
        Ok(new_value.and_then(|v| Record::deserialize(&v)))
    }

    fn delete_exact(&self, path: &str) -> Result<usize> {
        let aux_separator = format!("{path}\x1f");
        self.delete_where(path, |rest| {
            rest == path || rest.starts_with(aux_separator.as_str())
        })
    }

    fn delete_prefix(&self, prefix: &Path) -> Result<usize> {
        self.delete_where(prefix.as_str(), |rest| {
            let path = rest.split_once('\u{1f}').map_or(rest, |(p, _)| p);
            Path::parse(path).is_ok_and(|parsed| parsed.has_prefix(prefix))
        })
    }

    /// Scan the metadata, content, and bulk keyspaces for wire keys under every
    /// record kind at `scan_path`, removing those whose path-and-aux suffix (the
    /// `rest` after the `"{kind}:"` tag) satisfies `matches`. Returns the number
    /// of rows removed.
    ///
    /// Wire key format: `"{kind_char}:{path}"` or `"{kind_char}:{path}\x1f{hex_aux}"`.
    /// The prefix scan bounds I/O to keys sharing `"{kind}:{scan_path}"`;
    /// `matches` decides the exact boundary (exact / child / aux) per caller.
    fn delete_where(&self, scan_path: &str, matches: impl Fn(&str) -> bool) -> Result<usize> {
        let mut deleted = 0;
        for ks in [&self.metadata, &self.content, &self.bulk] {
            let mut to_delete: Vec<Vec<u8>> = Vec::new();
            for kind in RecordKind::ALL {
                let after_kind = format!("{}:", kind_prefix(kind));
                let wire_prefix = format!("{after_kind}{scan_path}");
                for guard in ks.inner().prefix(wire_prefix.as_bytes()) {
                    let wire_key = guard.key()?;
                    let Ok(wire_str) = std::str::from_utf8(&wire_key) else {
                        continue;
                    };
                    let Some(rest) = wire_str.strip_prefix(after_kind.as_str()) else {
                        continue;
                    };
                    if matches(rest) {
                        to_delete.push(wire_key.to_vec());
                    }
                }
            }
            for key in &to_delete {
                ks.remove(key.as_slice())?;
                deleted += 1;
            }
        }
        Ok(deleted)
    }
}

fn read_record(ks: &OptimisticTxKeyspace, wire_key: &str) -> Result<Option<Record>> {
    let Some(value) = ks.get(wire_key.as_bytes())? else {
        return Ok(None);
    };
    // A corrupt or unknown schema version is treated as a miss so the host
    // re-fetches from the provider.
    Ok(Record::deserialize(&value))
}

/// Unified view cache: byte-weighted moka `mem` tier in front of a durable fjall `disk` tier.
///
/// The `mem` tier is always present. The `disk` (fjall) tier is optional — when
/// absent the cache operates as mem-only.
pub struct Cache {
    mem: MokaCache<Key, Arc<Record>>,
    disk: Option<Disk>,
}

impl Cache {
    /// Create a cache with no durable backing (mem-only).
    pub fn new() -> Self {
        Self {
            mem: Self::build_mem(),
            disk: None,
        }
    }

    /// Open a view cache backed by the fjall database at `path` (a directory).
    ///
    /// Always deletes and recreates `path` before opening (Codex #5): the view
    /// is disposable — it is derived from the durable object cache and must
    /// never survive a restart to disagree with it. No sentinel, no crash
    /// detection; the host removes and reopens unconditionally.
    pub fn open(path: &StdPath) -> Result<Self> {
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent)?;
        }
        // Delete whatever was there — stale rendered bytes must not survive a
        // restart (Codex #5).
        if path.exists() {
            std::fs::remove_dir_all(path)?;
        }
        let db = OptimisticTxDatabase::builder(path).open()?;
        let disk = Disk {
            metadata: db.keyspace(METADATA_KEYSPACE, KeyspaceCreateOptions::default)?,
            content: db.keyspace(CONTENT_KEYSPACE, KeyspaceCreateOptions::default)?,
            bulk: db.keyspace(BULK_KEYSPACE, KeyspaceCreateOptions::default)?,
            expiry: db.keyspace(EXPIRY_KEYSPACE, KeyspaceCreateOptions::default)?,
        };
        Ok(Self {
            mem: Self::build_mem(),
            disk: Some(disk),
        })
    }

    fn build_mem() -> MokaCache<Key, Arc<Record>> {
        MokaCache::builder()
            .max_capacity(VIEW_MEM_MAX_WEIGHT)
            .support_invalidation_closures()
            .weigher(|key: &Key, value: &Arc<Record>| -> u32 {
                let key_size = 1 + key.path.len() + key.aux.as_ref().map_or(0, String::len);
                let val_size = 2 + value.payload.len();
                (key_size + val_size).try_into().unwrap_or(u32::MAX)
            })
            .build()
    }

    // --- Mem-only operations (fast path, no disk I/O) --------------------

    /// Look up a record in the mem only. Does not read from the database.
    ///
    /// Use this for hot-path reads where falling through to disk would change
    /// caching semantics (e.g. the FUSE pagination accumulator).
    pub fn mem_get(&self, key: &Key) -> Option<Arc<Record>> {
        self.mem.get(key)
    }

    /// Remove the exact key from the mem only. Does not touch the database.
    pub fn mem_invalidate(&self, key: &Key) {
        self.mem.invalidate(key);
    }

    /// Remove all mem entries matching `predicate`. Does not touch the
    /// database.
    pub fn mem_invalidate_entries_if<P>(&self, predicate: P)
    where
        P: Fn(&Key, &Arc<Record>) -> bool + Send + Sync + 'static,
    {
        self.mem
            .invalidate_entries_if(predicate)
            .expect("invalidation closures enabled at cache construction");
    }

    // --- Unified operations (mem + disk) ---------------------------------

    /// Look up a record. Checks the mem first; on a miss, reads from disk and
    /// promotes the result into the mem.
    pub fn get(&self, key: &Key) -> Option<Arc<Record>> {
        if let Some(record) = self.mem.get(key) {
            return Some(record);
        }
        let record = self.disk.as_ref()?.get(key).ok().flatten()?;
        let arc = Arc::new(record);
        // Promote from disk into the mem if it fits the threshold.
        if arc.payload.len() <= VIEW_MEM_SKIP_THRESHOLD {
            self.mem.insert(key.clone(), arc.clone());
        }
        Some(arc)
    }

    /// Write a record to the mem and the backing database.
    pub fn put(&self, key: &Key, record: &Record) {
        if record.payload.len() <= VIEW_MEM_SKIP_THRESHOLD {
            self.mem.insert(key.clone(), Arc::new(record.clone()));
        }
        if let Some(ref disk) = self.disk
            && let Err(e) = disk.put(key, record)
        {
            tracing::debug!(path = key.path.as_str(), error = %e, "view cache disk put failed");
        }
    }

    /// Write a batch of records to the mem and the backing database.
    pub fn put_batch(&self, records: &[BatchRecord]) {
        for item in records {
            let key = Key::with_aux(&item.path, item.kind, item.aux.as_deref());
            if item.record.payload.len() <= VIEW_MEM_SKIP_THRESHOLD {
                self.mem.insert(key, Arc::new(item.record.clone()));
            }
        }
        if let Some(ref disk) = self.disk
            && let Err(e) = disk.put_batch(records)
        {
            tracing::debug!(error = %e, "view cache disk batch put failed");
        }
    }

    /// Atomically update one metadata record. The caller owns payload
    /// semantics; this cache owns read-modify-write atomicity. `update` may be
    /// rerun on a write-write conflict, so it must be a pure function of its
    /// input.
    pub fn update_metadata_record<F>(&self, key: &Key, mut update: F)
    where
        F: FnMut(Option<Record>) -> Option<Record>,
    {
        debug_assert_ne!(key.kind, RecordKind::File);
        let updated = if let Some(ref disk) = self.disk {
            match disk.update_metadata(key, update) {
                Ok(updated) => updated,
                Err(e) => {
                    tracing::debug!(path = key.path.as_str(), error = %e, "view cache record update failed");
                    return;
                },
            }
        } else {
            let existing = self.mem.get(key).map(|record| (*record).clone());
            update(existing)
        };

        match updated {
            Some(record) if record.payload.len() <= VIEW_MEM_SKIP_THRESHOLD => {
                self.mem.insert(key.clone(), Arc::new(record));
            },
            Some(_) | None => self.mem.invalidate(key),
        }
    }

    /// Remove all records at `prefix` or beneath it on a segment boundary from
    /// both the mem and the database.
    ///
    /// `prefix` may be a mount-scoped or unscoped omnifs path. Matching uses
    /// typed path segment boundaries.
    pub fn invalidate_prefix(&self, prefix: &Path) {
        // Mem: use predicate-based eviction on path prefix.
        let prefix_owned = prefix.clone();
        self.mem
            .invalidate_entries_if(move |k, _| k.path.has_prefix(&prefix_owned))
            .expect("invalidation closures enabled at cache construction");
        // Flush pending maintenance so the predicate is applied immediately
        // (moka applies invalidate_entries_if lazily otherwise).
        self.mem.run_pending_tasks();
        if let Some(ref disk) = self.disk
            && let Err(e) = disk.delete_prefix(prefix)
        {
            tracing::debug!(prefix = %prefix, error = %e, "view cache disk prefix delete failed");
        }
    }

    /// Remove all records whose logical path equals `path` from both tiers.
    pub fn delete_exact(&self, path: &Path) {
        // Mem: exact-path eviction across all record kinds.
        let path_owned = path.clone();
        self.mem
            .invalidate_entries_if(move |k, _| k.path == path_owned)
            .expect("invalidation closures enabled at cache construction");
        // Flush pending maintenance so the predicate is applied immediately.
        self.mem.run_pending_tasks();
        if let Some(ref disk) = self.disk
            && let Err(e) = disk.delete_exact(path.as_str())
        {
            tracing::debug!(path = %path, error = %e, "view cache disk exact delete failed");
        }
        self.delete_expiry(path.as_str());
    }

    pub fn put_expiry(&self, scoped_path: &str, expiry: Expiry) {
        let Some(ref disk) = self.disk else {
            return;
        };
        if let Ok(bytes) = postcard::to_allocvec(&expiry)
            && let Err(error) = disk.expiry.insert(scoped_path.as_bytes(), bytes.as_slice())
        {
            tracing::debug!(path = scoped_path, error = %error, "view expiry put failed");
        }
    }

    pub fn get_expiry(&self, scoped_path: &str) -> Option<Expiry> {
        let disk = self.disk.as_ref()?;
        let value = disk.expiry.get(scoped_path.as_bytes()).ok()??;
        postcard::from_bytes(&value).ok()
    }

    fn delete_expiry(&self, scoped_path: &str) {
        let Some(ref disk) = self.disk else {
            return;
        };
        if let Err(error) = disk.expiry.remove(scoped_path.as_bytes()) {
            tracing::debug!(path = scoped_path, error = %error, "view expiry delete failed");
        }
    }
}

impl Default for Cache {
    fn default() -> Self {
        Self::new()
    }
}

// --- Key serialization helpers -----------------------------------------------

fn make_key(key: &Key) -> String {
    let prefix = kind_prefix(key.kind);
    match &key.aux {
        Some(aux) => format!("{prefix}:{}\u{1f}{}", key.path, hex::encode(aux)),
        None => format!("{prefix}:{}", key.path),
    }
}

fn kind_prefix(kind: RecordKind) -> char {
    match kind {
        RecordKind::Lookup => 'L',
        RecordKind::Attr => 'A',
        RecordKind::Dirents => 'D',
        RecordKind::File => 'F',
    }
}
