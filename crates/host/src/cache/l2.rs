//! L2 browse cache: durable, path-keyed, per-provider-instance redb database.

use crate::cache::{BatchRecord, CacheRecord, Key, L2_BULK_THRESHOLD, RecordKind};
use crate::path_prefix::path_prefix_matches;
use redb::{Database, ReadableTable, TableDefinition};
use std::path::Path;

const METADATA_TABLE: TableDefinition<&str, &[u8]> = TableDefinition::new("metadata");
const CONTENT_TABLE: TableDefinition<&str, &[u8]> = TableDefinition::new("content");
const BULK_TABLE: TableDefinition<&str, &[u8]> = TableDefinition::new("bulk");

type L2Result<T> = anyhow::Result<T>;

pub struct Cache {
    db: Database,
}

impl Cache {
    pub fn open(path: &Path) -> L2Result<Self> {
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

    pub fn get(&self, key: &Key) -> L2Result<Option<CacheRecord>> {
        let txn = self.db.begin_read()?;
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

    pub fn put(&self, key: &Key, record: &CacheRecord) -> L2Result<()> {
        let txn = self.db.begin_write()?;
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
            let is_bulk = record.payload.len() >= L2_BULK_THRESHOLD;
            let other = if is_bulk { CONTENT_TABLE } else { BULK_TABLE };
            let mut other_table = txn.open_table(other)?;
            other_table.remove(serialized.as_str())?;
        }
        txn.commit()?;
        Ok(())
    }

    pub fn put_batch(&self, records: &[BatchRecord]) -> L2Result<()> {
        let txn = self.db.begin_write()?;
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
                let is_bulk = item.record.payload.len() >= L2_BULK_THRESHOLD;
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

    fn read_from_table(
        txn: &redb::ReadTransaction,
        table_def: TableDefinition<&str, &[u8]>,
        key: &str,
    ) -> L2Result<Option<CacheRecord>> {
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
    pub fn delete_exact(&self, path: &str) -> L2Result<usize> {
        let txn = self.db.begin_write()?;
        let mut deleted = 0;
        let tables = [METADATA_TABLE, CONTENT_TABLE, BULK_TABLE];

        for table_def in tables {
            let mut table = txn.open_table(table_def)?;
            let mut to_delete = Vec::new();
            for kind in RecordKind::ALL {
                let scan_prefix = make_key(&Key::new(path, kind));
                let range_end = range_end_for_prefix(&scan_prefix);
                let range = table.range::<&str>(scan_prefix.as_str()..range_end.as_str())?;
                for entry in range {
                    let entry = entry?;
                    let key = entry.0.value().to_string();
                    if stored_key_path(&key) == Some(path) {
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

    /// Delete all records whose logical path is equal to `prefix` or lies
    /// beneath it on a segment boundary.
    ///
    /// The stored key format is `{kind_char}:{path}` plus an optional
    /// auxiliary suffix, so each record kind gets one ordered range scan.
    pub fn delete_prefix(&self, prefix: &str) -> L2Result<usize> {
        let txn = self.db.begin_write()?;
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
}

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
