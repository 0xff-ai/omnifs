//! L2 browse cache: durable, path-keyed, per-provider-instance redb database.

use crate::cache::{CacheRecord, Key, L2_BULK_THRESHOLD, RecordKind};
use crate::path_prefix::path_prefix_matches;
use redb::{Database, TableDefinition};
use std::path::Path;

const METADATA_TABLE: TableDefinition<&str, &[u8]> = TableDefinition::new("metadata");
const CONTENT_TABLE: TableDefinition<&str, &[u8]> = TableDefinition::new("content");
const BULK_TABLE: TableDefinition<&str, &[u8]> = TableDefinition::new("bulk");

pub type Result<T> = anyhow::Result<T>;

pub struct Cache {
    db: Database,
}

impl Cache {
    pub fn open(path: &Path) -> Result<Self> {
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent).ok();
        }
        let db = Database::create(path)?;
        // Ensure tables exist.
        let txn = db.begin_write()?;
        {
            let _ = txn.open_table(METADATA_TABLE)?;
            let _ = txn.open_table(CONTENT_TABLE)?;
            let _ = txn.open_table(BULK_TABLE)?;
        }
        txn.commit()?;
        Ok(Self { db })
    }

    pub fn get(&self, key: &Key) -> Result<Option<CacheRecord>> {
        let txn = self.db.begin_read()?;
        let serialized = stored_key(key.path.as_str(), key.kind);

        // For File records, check content first, then bulk.
        if key.kind == RecordKind::File {
            if let Some(record) = Self::read_from_table(&txn, CONTENT_TABLE, &serialized)? {
                return Ok(Some(record));
            }
            return Self::read_from_table(&txn, BULK_TABLE, &serialized);
        }

        Self::read_from_table(&txn, METADATA_TABLE, &serialized)
    }

    pub fn put(&self, key: &Key, record: &CacheRecord) -> Result<()> {
        let txn = self.db.begin_write()?;
        let serialized = stored_key(key.path.as_str(), key.kind);
        let bytes = record.serialize();
        let target = Self::table_for(key.kind, record.payload.len());
        {
            let mut table = txn.open_table(target)?;
            table.insert(serialized.as_str(), bytes.as_slice())?;
        }
        // Remove stale copy from the other file table if the record
        // crossed the bulk threshold since last write.
        if key.kind == RecordKind::File {
            let is_bulk = record.payload.len() >= L2_BULK_THRESHOLD;
            let other = if is_bulk { CONTENT_TABLE } else { BULK_TABLE };
            let mut other_table = txn.open_table(other)?;
            other_table.remove(serialized.as_str())?;
        }
        txn.commit()?;
        Ok(())
    }

    pub fn put_batch(&self, records: &[(String, RecordKind, CacheRecord)]) -> Result<()> {
        let txn = self.db.begin_write()?;
        {
            let mut meta = txn.open_table(METADATA_TABLE)?;
            let mut content = txn.open_table(CONTENT_TABLE)?;
            let mut bulk = txn.open_table(BULK_TABLE)?;
            for (path, kind, record) in records {
                let key = stored_key(path, *kind);
                let bytes = record.serialize();
                let is_bulk = record.payload.len() >= L2_BULK_THRESHOLD;
                match (*kind, is_bulk) {
                    (RecordKind::File, true) => {
                        bulk.insert(key.as_str(), bytes.as_slice())?;
                        content.remove(key.as_str())?; // clear stale small copy
                    },
                    (RecordKind::File, false) => {
                        content.insert(key.as_str(), bytes.as_slice())?;
                        bulk.remove(key.as_str())?; // clear stale large copy
                    },
                    _ => {
                        meta.insert(key.as_str(), bytes.as_slice())?;
                    },
                }
            }
        }
        txn.commit()?;
        Ok(())
    }

    fn read_from_table(
        txn: &redb::ReadTransaction,
        table_def: TableDefinition<&str, &[u8]>,
        key: &str,
    ) -> Result<Option<CacheRecord>> {
        let table = txn.open_table(table_def)?;
        let Some(value) = table.get(key)? else {
            return Ok(None);
        };
        // Corrupt or unknown schema version is treated as a miss so the
        // host re-fetches from the provider.
        Ok(CacheRecord::deserialize(value.value()))
    }

    const fn table_for(
        kind: RecordKind,
        payload_len: usize,
    ) -> TableDefinition<'static, &'static str, &'static [u8]> {
        match kind {
            RecordKind::File if payload_len >= L2_BULK_THRESHOLD => BULK_TABLE,
            RecordKind::File => CONTENT_TABLE,
            _ => METADATA_TABLE,
        }
    }
}

impl Cache {
    pub fn delete_exact(&self, path: &str) -> Result<usize> {
        let txn = self.db.begin_write()?;
        let scan_prefix = format!("{path}\0");
        let range_end = range_end(&scan_prefix);
        let mut deleted = 0;
        for table_def in [METADATA_TABLE, CONTENT_TABLE, BULK_TABLE] {
            let mut table = txn.open_table(table_def)?;
            deleted += table
                .extract_from_if::<&str, _>(scan_prefix.as_str()..range_end.as_str(), |_, _| true)?
                .try_fold(0, |count, entry| entry.map(|_| count + 1))?;
        }
        txn.commit()?;
        Ok(deleted)
    }

    /// Delete all records whose logical path is equal to `prefix` or lies
    /// beneath it on a segment boundary.
    ///
    /// The stored key format is `{path}\0{kind_char}`, so one broad
    /// ordered range scan per table covers every record kind.
    pub fn delete_prefix(&self, prefix: &str) -> Result<usize> {
        let txn = self.db.begin_write()?;
        let scan_prefix = prefix.to_string();
        let range_end = range_end(&scan_prefix);
        let mut deleted = 0;
        for table_def in [METADATA_TABLE, CONTENT_TABLE, BULK_TABLE] {
            let mut table = txn.open_table(table_def)?;
            deleted += table
                .extract_from_if::<&str, _>(scan_prefix.as_str()..range_end.as_str(), |key, _| {
                    let path = key
                        .split_once('\0')
                        .map_or("", |(logical_path, _)| logical_path);
                    path_prefix_matches(prefix, path)
                })?
                .try_fold(0, |count, entry| entry.map(|_| count + 1))?;
        }
        txn.commit()?;
        Ok(deleted)
    }
}

// Stored as path first so exact-path and subtree invalidation can use
// one ordered range scan across all record kinds.
fn stored_key(path: &str, kind: RecordKind) -> String {
    format!("{path}\0{}", kind_tag(kind))
}

fn kind_tag(kind: RecordKind) -> char {
    match kind {
        RecordKind::Lookup => 'L',
        RecordKind::Attr => 'A',
        RecordKind::Dirents => 'D',
        RecordKind::File => 'F',
    }
}

fn range_end(prefix: &str) -> String {
    let mut end = prefix.to_string();
    end.push('\u{ffff}');
    end
}
