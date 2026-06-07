//! Unified browse-cache tier: in-memory moka `mem` tier in front of a durable redb `disk` tier.
//!
//! `Cache` owns both tiers behind one API. Reads check `mem` first and
//! promote `disk` hits into `mem`. Writes go to both. Invalidations
//! remove from both, keeping them coherent.

use crate::{BatchRecord, Key, Record, RecordKind, path_prefix_matches};
use anyhow::Result;
use moka::sync::Cache as MokaCache;
use redb::{Database, ReadableTable, TableDefinition};
use std::path::Path;
use std::sync::Arc;

/// Maximum total byte weight of the `mem` tier per provider instance (32 MiB).
pub const VIEW_MEM_MAX_WEIGHT: u64 = 32 * 1024 * 1024;

/// Records larger than this threshold are not inserted into `mem` (256 KiB).
pub const VIEW_MEM_SKIP_THRESHOLD: usize = 256 * 1024;

/// Records larger than this threshold are stored in the redb bulk table instead of
/// the content table (64 KiB).
pub const VIEW_BULK_THRESHOLD: usize = 64 * 1024;

const METADATA_TABLE: TableDefinition<&str, &[u8]> = TableDefinition::new("metadata");
const CONTENT_TABLE: TableDefinition<&str, &[u8]> = TableDefinition::new("content");
const BULK_TABLE: TableDefinition<&str, &[u8]> = TableDefinition::new("bulk");
const FRESHNESS_TABLE: TableDefinition<&str, &[u8]> = TableDefinition::new("freshness");

#[derive(serde::Serialize, serde::Deserialize, Clone, Copy)]
pub struct Freshness {
    pub expires_at: Option<u64>,
    pub generation: u64,
}

/// Unified view cache: byte-weighted moka `mem` tier in front of a durable redb `disk` tier.
///
/// The `mem` tier is always present. The `disk` (redb) tier is optional — when absent
/// (e.g. the database file could not be opened) the cache operates as
/// mem-only.
pub struct Cache {
    mem: MokaCache<Key, Arc<Record>>,
    disk: Option<Database>,
}

impl Cache {
    /// Create a cache with no durable backing (mem-only).
    pub fn new() -> Self {
        Self {
            mem: Self::build_mem(),
            disk: None,
        }
    }

    /// Open a view cache backed by the redb database at `path`.
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
        // a restart (Codex #5).
        if path.exists() {
            std::fs::remove_file(path)?;
        }
        let disk = Database::create(path)?;
        // Ensure all three tables exist before any reads or writes.
        let txn = disk.begin_write()?;
        {
            let _ = txn.open_table(METADATA_TABLE)?;
            let _ = txn.open_table(CONTENT_TABLE)?;
            let _ = txn.open_table(BULK_TABLE)?;
            let _ = txn.open_table(FRESHNESS_TABLE)?;
        }
        txn.commit()?;
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

    // --- Mem-only operations (fast path, no redb I/O) --------------------

    /// Look up a record in the mem only. Does not read from the database.
    ///
    /// Use this for hot-path reads where falling through to redb would
    /// change caching semantics (e.g. the FUSE pagination accumulator).
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

    // --- Unified operations (mem + redb) ---------------------------------

    /// Look up a record. Checks the mem first; on a miss, reads from redb
    /// and promotes the result into the mem.
    pub fn get(&self, key: &Key) -> Option<Arc<Record>> {
        if let Some(record) = self.mem.get(key) {
            return Some(record);
        }
        let record = self.get_from_disk(key).ok().flatten()?;
        let arc = Arc::new(record);
        // Promote from redb into the mem if it fits the threshold.
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

    fn disk_update_metadata_record<F>(
        disk: &Database,
        key: &Key,
        update: F,
    ) -> Result<Option<Record>>
    where
        F: FnOnce(Option<Record>) -> Option<Record>,
    {
        let serialized = make_key(key);
        let txn = disk.begin_write()?;
        let updated;
        {
            let mut table = txn.open_table(METADATA_TABLE)?;
            let existing = table
                .get(serialized.as_str())?
                .and_then(|guard| Record::deserialize(guard.value()));
            updated = update(existing);
            match &updated {
                Some(record) => {
                    let bytes = record.serialize();
                    table.insert(serialized.as_str(), bytes.as_slice())?;
                },
                None => {
                    table.remove(serialized.as_str())?;
                },
            }
        }
        txn.commit()?;
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
            && let Err(error) = (|| -> anyhow::Result<()> {
                let txn = disk.begin_write()?;
                {
                    let mut table = txn.open_table(FRESHNESS_TABLE)?;
                    table.insert(scoped_path, bytes.as_slice())?;
                }
                txn.commit()?;
                Ok(())
            })()
        {
            tracing::debug!(path = scoped_path, error = %error, "view freshness put failed");
        }
    }

    pub fn get_freshness(&self, scoped_path: &str) -> Option<Freshness> {
        let disk = self.disk.as_ref()?;
        let txn = disk.begin_read().ok()?;
        let table = txn.open_table(FRESHNESS_TABLE).ok()?;
        let guard = table.get(scoped_path).ok()??;
        postcard::from_bytes(guard.value()).ok()
    }

    pub fn is_fresh(&self, scoped_path: &str, now_millis: u64) -> bool {
        self.get_freshness(scoped_path)
            .is_some_and(|f| f.expires_at.is_none_or(|exp| now_millis < exp))
    }

    fn delete_freshness(&self, scoped_path: &str) {
        let Some(ref disk) = self.disk else {
            return;
        };
        if let Err(error) = (|| -> anyhow::Result<()> {
            let txn = disk.begin_write()?;
            {
                let mut table = txn.open_table(FRESHNESS_TABLE)?;
                table.remove(scoped_path)?;
            }
            txn.commit()?;
            Ok(())
        })() {
            tracing::debug!(path = scoped_path, error = %error, "view freshness delete failed");
        }
    }

    // --- Internal redb helpers -----------------------------------------------

    fn get_from_disk(&self, key: &Key) -> Result<Option<Record>> {
        let Some(ref disk) = self.disk else {
            return Ok(None);
        };
        let txn = disk.begin_read()?;
        let serialized = make_key(key);

        // For File records, check content first, then bulk.
        if key.kind == RecordKind::File {
            if let Some(record) = Self::read_from_table(&txn, CONTENT_TABLE, &serialized)? {
                return Ok(Some(record));
            }
            return Self::read_from_table(&txn, BULK_TABLE, &serialized);
        }

        Self::read_from_table(&txn, METADATA_TABLE, &serialized)
    }

    fn disk_put(disk: &Database, key: &Key, record: &Record) -> Result<()> {
        let txn = disk.begin_write()?;
        let serialized = make_key(key);
        let bytes = record.serialize();
        let target = Self::table_for(key.kind, record.payload.len());
        {
            let mut table = txn.open_table(target)?;
            table.insert(serialized.as_str(), bytes.as_slice())?;
        }
        // Remove stale copy from the other file table if the record
        // crossed the bulk threshold since last write.
        if key.kind == RecordKind::File {
            let is_bulk = record.payload.len() >= VIEW_BULK_THRESHOLD;
            let other = if is_bulk { CONTENT_TABLE } else { BULK_TABLE };
            let mut other_table = txn.open_table(other)?;
            other_table.remove(serialized.as_str())?;
        }
        txn.commit()?;
        Ok(())
    }

    fn disk_put_batch(disk: &Database, records: &[BatchRecord]) -> Result<()> {
        let txn = disk.begin_write()?;
        {
            let mut meta = txn.open_table(METADATA_TABLE)?;
            let mut content = txn.open_table(CONTENT_TABLE)?;
            let mut bulk = txn.open_table(BULK_TABLE)?;
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
                        bulk.insert(wire_key.as_str(), bytes.as_slice())?;
                        content.remove(wire_key.as_str())?; // clear stale small copy
                    },
                    (RecordKind::File, false) => {
                        content.insert(wire_key.as_str(), bytes.as_slice())?;
                        bulk.remove(wire_key.as_str())?; // clear stale large copy
                    },
                    _ => {
                        meta.insert(wire_key.as_str(), bytes.as_slice())?;
                    },
                }
            }
        }
        txn.commit()?;
        Ok(())
    }

    fn disk_delete_exact(disk: &Database, path: &str) -> Result<usize> {
        let txn = disk.begin_write()?;
        let mut deleted = 0;
        let tables = [METADATA_TABLE, CONTENT_TABLE, BULK_TABLE];
        // Suffix that distinguishes "exact path with aux" from a child path.
        // For example, for path "mount\x1f/a/b" the key with aux is
        // "F:mount\x1f/a/b\x1f<hex>" while a child key starts with
        // "L:mount\x1f/a/b/". The `\x1f` suffix catches the aux case.
        let aux_separator = format!("{path}\x1f");

        for table_def in tables {
            let mut table = txn.open_table(table_def)?;
            let mut to_delete = Vec::new();
            for kind in RecordKind::ALL {
                let wire_prefix = format!("{}:{path}", kind_prefix(kind));
                let range_end = range_end_for_prefix(&wire_prefix);
                let after_kind = format!("{}:", kind_prefix(kind));
                let range = table.range::<&str>(wire_prefix.as_str()..range_end.as_str())?;
                for entry in range {
                    let entry = entry?;
                    let wire_key = entry.0.value();
                    let Some(rest) = wire_key.strip_prefix(after_kind.as_str()) else {
                        continue;
                    };
                    // Match: rest == path (exact, no aux)
                    //        rest starts with path + "\x1f" (same path, has aux)
                    // Do NOT match rest starting with path + "/" (child path).
                    if rest == path || rest.starts_with(aux_separator.as_str()) {
                        to_delete.push(wire_key.to_string());
                    }
                }
            }
            for key in &to_delete {
                table.remove(key.as_str())?;
                deleted += 1;
            }
        }

        txn.commit()?;
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
    fn disk_delete_scoped_prefix(disk: &Database, scoped_prefix: &str) -> Result<usize> {
        let txn = disk.begin_write()?;
        let mut deleted = 0;
        let tables = [METADATA_TABLE, CONTENT_TABLE, BULK_TABLE];
        let child_prefix = format!("{scoped_prefix}/");
        let aux_prefix = format!("{scoped_prefix}\x1f");

        for table_def in tables {
            let mut table = txn.open_table(table_def)?;
            let mut to_delete = Vec::new();
            for kind in RecordKind::ALL {
                // Scan wire keys that start with "{kind}:{scoped_prefix}".
                // A wire key matches if the path portion (everything after
                // "{kind}:") equals `scoped_prefix`, or starts with
                // `scoped_prefix + "/"` (descendant on a segment boundary), or
                // starts with `scoped_prefix + "\x1f"` (same path with an aux
                // field, e.g. a versioned file record).
                let wire_prefix = format!("{}:{scoped_prefix}", kind_prefix(kind));
                let range_end = range_end_for_prefix(&wire_prefix);
                let after_kind = format!("{}:", kind_prefix(kind));
                let range = table.range::<&str>(wire_prefix.as_str()..range_end.as_str())?;
                for entry in range {
                    let entry = entry?;
                    let wire_key = entry.0.value();
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
                        to_delete.push(wire_key.to_string());
                    }
                }
            }
            for key in &to_delete {
                table.remove(key.as_str())?;
                deleted += 1;
            }
        }
        txn.commit()?;
        Ok(deleted)
    }

    /// Delete all records whose logical path is equal to `prefix` or lies
    /// beneath it on a segment boundary.
    fn disk_delete_prefix(disk: &Database, prefix: &str) -> Result<usize> {
        let txn = disk.begin_write()?;
        let mut deleted = 0;
        let tables = [METADATA_TABLE, CONTENT_TABLE, BULK_TABLE];

        for table_def in tables {
            let mut table = txn.open_table(table_def)?;
            let mut to_delete = Vec::new();
            for kind in RecordKind::ALL {
                let scan_prefix = make_key(&Key::new(prefix, kind));
                let range_end = range_end_for_prefix(&scan_prefix);
                let range = table.range::<&str>(scan_prefix.as_str()..range_end.as_str())?;
                for entry in range {
                    let entry = entry?;
                    let key = entry.0.value().to_string();
                    let path = stored_key_path(&key).unwrap_or("");
                    if path_prefix_matches(prefix, path) {
                        to_delete.push(key);
                    }
                }
            }
            for key in &to_delete {
                table.remove(key.as_str())?;
                deleted += 1;
            }
        }
        txn.commit()?;
        Ok(deleted)
    }

    fn read_from_table(
        txn: &redb::ReadTransaction,
        table_def: TableDefinition<&str, &[u8]>,
        key: &str,
    ) -> Result<Option<Record>> {
        let table = txn.open_table(table_def)?;
        let Some(value) = table.get(key)? else {
            return Ok(None);
        };
        // A corrupt or unknown schema version is treated as a miss so the
        // host re-fetches from the provider.
        Ok(Record::deserialize(value.value()))
    }

    const fn table_for(
        kind: RecordKind,
        payload_len: usize,
    ) -> TableDefinition<'static, &'static str, &'static [u8]> {
        match kind {
            RecordKind::File if payload_len >= VIEW_BULK_THRESHOLD => BULK_TABLE,
            RecordKind::File => CONTENT_TABLE,
            _ => METADATA_TABLE,
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

fn range_end_for_prefix(prefix: &str) -> String {
    let mut end = prefix.to_string();
    end.push('\u{10ffff}');
    end
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
