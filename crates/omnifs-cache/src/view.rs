//! Unified browse-cache tier: in-memory moka `mem` tier in front of a fjall `disk` tier.
//!
//! `Cache` owns both tiers behind one API. Reads check `mem` first and
//! promote `disk` hits into `mem`. Writes go to both. Invalidations
//! remove from both, keeping them coherent.

use crate::{BatchRecord, Key, Record, RecordKind, path_prefix_matches};
use anyhow::Result;
use fjall::{Config, Database, Keyspace, KeyspaceCreateOptions};
use moka::sync::Cache as MokaCache;
use parking_lot::Mutex;
use std::path::Path;
use std::sync::Arc;

/// Maximum total byte weight of the `mem` tier per provider instance (32 MiB).
pub const VIEW_MEM_MAX_WEIGHT: u64 = 32 * 1024 * 1024;

/// Records larger than this threshold are not inserted into `mem` (256 KiB).
pub const VIEW_MEM_SKIP_THRESHOLD: usize = 256 * 1024;

/// Records larger than this threshold are stored in the bulk partition instead of
/// the content partition (64 KiB).
pub const VIEW_BULK_THRESHOLD: usize = 64 * 1024;

const METADATA_KEYSPACE: &str = "metadata";
const CONTENT_KEYSPACE: &str = "content";
const BULK_KEYSPACE: &str = "bulk";
const FRESHNESS_KEYSPACE: &str = "freshness";

#[derive(serde::Serialize, serde::Deserialize, Clone, Copy)]
pub struct Freshness {
    pub expires_at: Option<u64>,
    pub generation: u64,
}

/// Durable view backing: a fjall database with one keyspace per record class.
///
/// All ordinary writes (`put`, `put_batch`, the prefix/exact deletes) run
/// lock-free — fjall makes individual inserts/removes and batch commits atomic
/// and is safe under concurrent writers. The only coordination here is
/// `merge_lock`, which serializes the *read-modify-write* in
/// `disk_update_metadata_record` (the dirents-listing merge): get-then-insert is
/// two calls, so concurrent merges of the same key would lose updates without
/// it. This is the only RMW writer of a metadata key, so a plain mutex suffices;
/// redb's single write transaction provided the same guarantee implicitly.
struct Disk {
    db: Database,
    metadata: Keyspace,
    content: Keyspace,
    bulk: Keyspace,
    freshness: Keyspace,
    merge_lock: Mutex<()>,
}

impl Disk {
    fn partition_for(&self, kind: RecordKind, payload_len: usize) -> &Keyspace {
        match kind {
            RecordKind::File if payload_len >= VIEW_BULK_THRESHOLD => &self.bulk,
            RecordKind::File => &self.content,
            _ => &self.metadata,
        }
    }
}

/// Unified view cache: byte-weighted moka `mem` tier in front of a fjall `disk` tier.
///
/// The `mem` tier is always present. The `disk` (fjall) tier is optional — when
/// absent (e.g. the keyspace could not be opened) the cache operates as
/// mem-only.
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

    /// Open a view cache backed by the fjall keyspace at `path`.
    ///
    /// Always deletes and recreates `path` before opening (Codex #5): the
    /// view is disposable — it is derived from the durable object cache and
    /// must never survive a restart to disagree with it. No sentinel, no
    /// crash detection; the host removes and reopens unconditionally.
    pub fn open(path: &Path) -> Result<Self> {
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent)?;
        }
        // Delete whatever was there — stale rendered bytes must not survive
        // a restart (Codex #5). The keyspace is a directory; tolerate a stale
        // plain file left by an earlier storage engine.
        if path.is_dir() {
            std::fs::remove_dir_all(path)?;
        } else if path.exists() {
            std::fs::remove_file(path)?;
        }
        let db = Database::open(Config::new(path))?;
        // Open all keyspaces before any reads or writes.
        let metadata = db.keyspace(METADATA_KEYSPACE, KeyspaceCreateOptions::default)?;
        let content = db.keyspace(CONTENT_KEYSPACE, KeyspaceCreateOptions::default)?;
        let bulk = db.keyspace(BULK_KEYSPACE, KeyspaceCreateOptions::default)?;
        let freshness = db.keyspace(FRESHNESS_KEYSPACE, KeyspaceCreateOptions::default)?;
        Ok(Self {
            mem: Self::build_mem(),
            disk: Some(Disk {
                db,
                metadata,
                content,
                bulk,
                freshness,
                merge_lock: Mutex::new(()),
            }),
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
    /// Use this for hot-path reads where falling through to the disk tier
    /// would change caching semantics (e.g. the FUSE pagination accumulator).
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

    /// Look up a record. Checks the mem first; on a miss, reads from the disk
    /// tier and promotes the result into the mem.
    pub fn get(&self, key: &Key) -> Option<Arc<Record>> {
        if let Some(record) = self.mem.get(key) {
            return Some(record);
        }
        let record = self.get_from_disk(key).ok().flatten()?;
        let arc = Arc::new(record);
        // Promote from the disk tier into the mem if it fits the threshold.
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
            && let Err(e) = Self::disk_put(disk, key, record)
        {
            tracing::debug!(path = key.path.as_str(), error = %e, "view cache disk put failed");
        }
    }

    /// Write a batch of records to the mem and the backing database.
    pub fn put_batch(&self, records: &[BatchRecord]) {
        for item in records {
            let key = Key::with_aux(item.path.clone(), item.kind, item.aux.as_deref());
            if item.record.payload.len() <= VIEW_MEM_SKIP_THRESHOLD {
                self.mem.insert(key, Arc::new(item.record.clone()));
            }
        }
        if let Some(ref disk) = self.disk
            && let Err(e) = Self::disk_put_batch(disk, records)
        {
            tracing::debug!(error = %e, "view cache disk batch put failed");
        }
    }

    /// Transactionally update one metadata-table record. The caller owns
    /// payload semantics; this cache owns read-modify-write atomicity.
    pub fn update_metadata_record<F>(&self, key: &Key, update: F)
    where
        F: FnOnce(Option<Record>) -> Option<Record>,
    {
        debug_assert_ne!(key.kind, RecordKind::File);
        let updated = if let Some(ref disk) = self.disk {
            match Self::disk_update_metadata_record(disk, key, update) {
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

    fn disk_update_metadata_record<F>(disk: &Disk, key: &Key, update: F) -> Result<Option<Record>>
    where
        F: FnOnce(Option<Record>) -> Option<Record>,
    {
        let serialized = make_key(key);
        // Serialize the read-modify-write so concurrent merges of the same
        // record do not lose updates.
        let _guard = disk.merge_lock.lock();
        let existing = disk
            .metadata
            .get(serialized.as_str())?
            .and_then(|value| Record::deserialize(&value));
        let updated = update(existing);
        match &updated {
            Some(record) => {
                let bytes = record.serialize();
                disk.metadata
                    .insert(serialized.as_str(), bytes.as_slice())?;
            },
            None => {
                disk.metadata.remove(serialized.as_str())?;
            },
        }
        Ok(updated)
    }

    /// Remove the exact key from the mem and the database.
    pub fn invalidate(&self, key: &Key) {
        self.mem.invalidate(key);
        if let Some(ref disk) = self.disk
            && let Err(e) = Self::disk_delete_exact(disk, &key.path)
        {
            tracing::debug!(path = key.path.as_str(), error = %e, "view cache disk invalidate failed");
        }
    }

    /// Remove all entries whose key matches `predicate` from the mem.
    /// Does not touch the database (prefix-level database cleanup is done
    /// by `invalidate_prefix`).
    pub fn invalidate_entries_if<P>(&self, predicate: P)
    where
        P: Fn(&Key, &Arc<Record>) -> bool + Send + Sync + 'static,
    {
        self.mem
            .invalidate_entries_if(predicate)
            .expect("invalidation closures enabled at cache construction");
    }

    /// Remove all records at `prefix` or beneath it on a segment boundary from
    /// both the mem and the database.
    ///
    /// `prefix` must be an unscoped omnifs path (e.g. `/owner/repo`); the path
    /// matching uses `path_prefix_matches` which parses it as an omnifs path.
    /// For pre-scoped keys use `invalidate_scoped_prefix` instead.
    pub fn invalidate_prefix(&self, prefix: &str) {
        // Mem: use predicate-based eviction on path prefix.
        let prefix_owned = prefix.to_string();
        self.mem
            .invalidate_entries_if(move |k, _| path_prefix_matches(&prefix_owned, &k.path))
            .expect("invalidation closures enabled at cache construction");
        // Flush pending maintenance so the predicate is applied immediately
        // (moka applies invalidate_entries_if lazily otherwise).
        self.mem.run_pending_tasks();
        if let Some(ref disk) = self.disk
            && let Err(e) = Self::disk_delete_prefix(disk, prefix)
        {
            tracing::debug!(prefix, error = %e, "view cache disk prefix delete failed");
        }
    }

    /// Remove all records whose path equals `scoped_prefix` or starts with
    /// `scoped_prefix + "/"`. For use with pre-scoped keys that include the
    /// mount separator `"\x1f"` and cannot be parsed as omnifs paths.
    pub fn invalidate_scoped_prefix(&self, scoped_prefix: &str) {
        let owned = scoped_prefix.to_string();
        let child_prefix = format!("{owned}/");
        // The segment boundary: a path `p` matches if p == prefix or p starts
        // with prefix followed by '/'. This is safe because the scope separator
        // `\x1f` is never `/`, so cross-mount matches are impossible.
        self.mem
            .invalidate_entries_if(move |k, _| {
                k.path == owned || k.path.starts_with(child_prefix.as_str())
            })
            .expect("invalidation closures enabled at cache construction");
        // Flush pending maintenance so the predicate is applied immediately.
        self.mem.run_pending_tasks();
        if let Some(ref disk) = self.disk
            && let Err(e) = Self::disk_delete_scoped_prefix(disk, scoped_prefix)
        {
            tracing::debug!(scoped_prefix, error = %e, "view cache disk scoped-prefix delete failed");
        }
    }

    /// Remove all records whose logical path equals `path` from both tiers.
    pub fn delete_exact(&self, path: &str) {
        // Mem: exact-path eviction across all record kinds.
        let path_owned = path.to_string();
        self.mem
            .invalidate_entries_if(move |k, _| k.path == path_owned)
            .expect("invalidation closures enabled at cache construction");
        // Flush pending maintenance so the predicate is applied immediately.
        self.mem.run_pending_tasks();
        if let Some(ref disk) = self.disk
            && let Err(e) = Self::disk_delete_exact(disk, path)
        {
            tracing::debug!(path, error = %e, "view cache disk exact delete failed");
        }
        self.delete_freshness(path);
    }

    pub fn put_freshness(&self, scoped_path: &str, freshness: Freshness) {
        let Some(ref disk) = self.disk else {
            return;
        };
        if let Ok(bytes) = postcard::to_allocvec(&freshness)
            && let Err(error) = disk.freshness.insert(scoped_path, bytes.as_slice())
        {
            tracing::debug!(path = scoped_path, error = %error, "view freshness put failed");
        }
    }

    pub fn get_freshness(&self, scoped_path: &str) -> Option<Freshness> {
        let disk = self.disk.as_ref()?;
        let value = disk.freshness.get(scoped_path).ok()??;
        postcard::from_bytes(&value).ok()
    }

    pub fn is_fresh(&self, scoped_path: &str, now_millis: u64) -> bool {
        self.get_freshness(scoped_path)
            .is_some_and(|f| f.expires_at.is_none_or(|exp| now_millis < exp))
    }

    fn delete_freshness(&self, scoped_path: &str) {
        let Some(ref disk) = self.disk else {
            return;
        };
        if let Err(error) = disk.freshness.remove(scoped_path) {
            tracing::debug!(path = scoped_path, error = %error, "view freshness delete failed");
        }
    }

    // --- Internal fjall helpers ----------------------------------------------

    fn get_from_disk(&self, key: &Key) -> Result<Option<Record>> {
        let Some(ref disk) = self.disk else {
            return Ok(None);
        };
        let serialized = make_key(key);

        // For File records, check content first, then bulk.
        if key.kind == RecordKind::File {
            if let Some(record) = read_from_partition(&disk.content, &serialized)? {
                return Ok(Some(record));
            }
            return read_from_partition(&disk.bulk, &serialized);
        }

        read_from_partition(&disk.metadata, &serialized)
    }

    fn disk_put(disk: &Disk, key: &Key, record: &Record) -> Result<()> {
        let serialized = make_key(key);
        let bytes = record.serialize();
        let target = disk.partition_for(key.kind, record.payload.len());
        let mut batch = disk.db.batch();
        batch.insert(target, serialized.as_str(), bytes.as_slice());
        // Remove stale copy from the other file partition if the record
        // crossed the bulk threshold since last write.
        if key.kind == RecordKind::File {
            let is_bulk = record.payload.len() >= VIEW_BULK_THRESHOLD;
            let other = if is_bulk { &disk.content } else { &disk.bulk };
            batch.remove(other, serialized.as_str());
        }
        batch.commit()?;
        Ok(())
    }

    fn disk_put_batch(disk: &Disk, records: &[BatchRecord]) -> Result<()> {
        let mut batch = disk.db.batch();
        for item in records {
            let wire_key = make_key(&Key::with_aux(
                item.path.clone(),
                item.kind,
                item.aux.as_deref(),
            ));
            let bytes = item.record.serialize();
            let is_bulk = item.record.payload.len() >= VIEW_BULK_THRESHOLD;
            match (item.kind, is_bulk) {
                (RecordKind::File, true) => {
                    batch.insert(&disk.bulk, wire_key.as_str(), bytes.as_slice());
                    batch.remove(&disk.content, wire_key.as_str()); // clear stale small copy
                },
                (RecordKind::File, false) => {
                    batch.insert(&disk.content, wire_key.as_str(), bytes.as_slice());
                    batch.remove(&disk.bulk, wire_key.as_str()); // clear stale large copy
                },
                _ => {
                    batch.insert(&disk.metadata, wire_key.as_str(), bytes.as_slice());
                },
            }
        }
        batch.commit()?;
        Ok(())
    }

    fn disk_delete_exact(disk: &Disk, path: &str) -> Result<usize> {
        let partitions = [&disk.metadata, &disk.content, &disk.bulk];
        // Suffix that distinguishes "exact path with aux" from a child path.
        // For example, for path "mount\x1f/a/b" the key with aux is
        // "F:mount\x1f/a/b\x1f<hex>" while a child key starts with
        // "L:mount\x1f/a/b/". The `\x1f` suffix catches the aux case.
        let aux_separator = format!("{path}\x1f");

        let mut batch = disk.db.batch();
        let mut deleted = 0;
        for partition in partitions {
            for kind in RecordKind::ALL {
                let wire_prefix = format!("{}:{path}", kind_prefix(kind));
                let after_kind = format!("{}:", kind_prefix(kind));
                for entry in partition.prefix(wire_prefix.as_bytes()) {
                    let wire_key = entry.key()?;
                    let Ok(wire_key) = std::str::from_utf8(&wire_key) else {
                        continue;
                    };
                    let Some(rest) = wire_key.strip_prefix(after_kind.as_str()) else {
                        continue;
                    };
                    // Match: rest == path (exact, no aux)
                    //        rest starts with path + "\x1f" (same path, has aux)
                    // Do NOT match rest starting with path + "/" (child path).
                    if rest == path || rest.starts_with(aux_separator.as_str()) {
                        batch.remove(partition, wire_key);
                        deleted += 1;
                    }
                }
            }
        }
        batch.commit()?;
        Ok(deleted)
    }

    /// Delete all records whose stored path equals `scoped_prefix` or starts
    /// with `scoped_prefix + "/"`. For use with pre-scoped keys that include
    /// the mount separator `"\x1f"` — no omnifs path parsing, plain string
    /// segment matching only.
    ///
    /// Wire key format: `"{kind_char}:{path}"` or `"{kind_char}:{path}\x1f{hex_aux}"`.
    /// Because scoped paths contain `\x1f` themselves (the mount separator), we
    /// must match on the wire key directly rather than extracting the path via
    /// `stored_key_path` (which splits on the first `\x1f` and would stop at the
    /// mount boundary).
    fn disk_delete_scoped_prefix(disk: &Disk, scoped_prefix: &str) -> Result<usize> {
        let partitions = [&disk.metadata, &disk.content, &disk.bulk];
        let child_prefix = format!("{scoped_prefix}/");
        let aux_prefix = format!("{scoped_prefix}\x1f");

        let mut batch = disk.db.batch();
        let mut deleted = 0;
        for partition in partitions {
            for kind in RecordKind::ALL {
                // Scan wire keys that start with "{kind}:{scoped_prefix}".
                // A wire key matches if the path portion (everything after
                // "{kind}:") equals `scoped_prefix`, or starts with
                // `scoped_prefix + "/"` (descendant on a segment boundary), or
                // starts with `scoped_prefix + "\x1f"` (same path with an aux
                // field, e.g. a versioned file record).
                let wire_prefix = format!("{}:{scoped_prefix}", kind_prefix(kind));
                let after_kind = format!("{}:", kind_prefix(kind));
                for entry in partition.prefix(wire_prefix.as_bytes()) {
                    let wire_key = entry.key()?;
                    let Ok(wire_key) = std::str::from_utf8(&wire_key) else {
                        continue;
                    };
                    // Extract the raw path+aux suffix after "{kind}:".
                    let Some(rest) = wire_key.strip_prefix(after_kind.as_str()) else {
                        continue;
                    };
                    // Match: rest == scoped_prefix (exact, no aux)
                    //        rest starts with scoped_prefix + "/" (child path)
                    //        rest starts with scoped_prefix + "\x1f" (same path, has aux)
                    let is_match = rest == scoped_prefix
                        || rest.starts_with(child_prefix.as_str())
                        || rest.starts_with(aux_prefix.as_str());
                    if is_match {
                        batch.remove(partition, wire_key);
                        deleted += 1;
                    }
                }
            }
        }
        batch.commit()?;
        Ok(deleted)
    }

    /// Delete all records whose logical path is equal to `prefix` or lies
    /// beneath it on a segment boundary.
    fn disk_delete_prefix(disk: &Disk, prefix: &str) -> Result<usize> {
        let partitions = [&disk.metadata, &disk.content, &disk.bulk];

        let mut batch = disk.db.batch();
        let mut deleted = 0;
        for partition in partitions {
            for kind in RecordKind::ALL {
                let scan_prefix = make_key(&Key::new(prefix, kind));
                for entry in partition.prefix(scan_prefix.as_bytes()) {
                    let wire_key = entry.key()?;
                    let Ok(wire_key) = std::str::from_utf8(&wire_key) else {
                        continue;
                    };
                    let path = stored_key_path(wire_key).unwrap_or("");
                    if path_prefix_matches(prefix, path) {
                        batch.remove(partition, wire_key);
                        deleted += 1;
                    }
                }
            }
        }
        batch.commit()?;
        Ok(deleted)
    }
}

fn read_from_partition(partition: &Keyspace, key: &str) -> Result<Option<Record>> {
    let Some(value) = partition.get(key)? else {
        return Ok(None);
    };
    // A corrupt or unknown schema version is treated as a miss so the
    // host re-fetches from the provider.
    Ok(Record::deserialize(&value))
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
        Some(aux) => format!("{prefix}:{}\u{1f}{}", key.path, hex_bytes(aux)),
        None => format!("{prefix}:{}", key.path),
    }
}

fn stored_key_path(key: &str) -> Option<&str> {
    let (_, path_and_aux) = key.split_once(':')?;
    Some(
        path_and_aux
            .split_once('\u{1f}')
            .map_or(path_and_aux, |(path, _)| path),
    )
}

fn kind_prefix(kind: RecordKind) -> char {
    match kind {
        RecordKind::Lookup => 'L',
        RecordKind::Attr => 'A',
        RecordKind::Dirents => 'D',
        RecordKind::File => 'F',
    }
}

fn hex_bytes(value: &str) -> String {
    const HEX: &[u8; 16] = b"0123456789abcdef";
    let bytes = value.as_bytes();
    let mut out = String::with_capacity(bytes.len() * 2);
    for byte in bytes {
        out.push(HEX[(byte >> 4) as usize] as char);
        out.push(HEX[(byte & 0x0f) as usize] as char);
    }
    out
}
