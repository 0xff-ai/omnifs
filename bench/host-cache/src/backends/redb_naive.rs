// Mirrors crates/host/src/cache/l2.rs — one redb DB, three tables
// (metadata / content / bulk), kind-prefixed string keys.

use super::{Backend, RecordKind, make_kind_path_key, matches_path_prefix};
use anyhow::Result;
use redb::{Database, ReadableTable, TableDefinition};
use std::path::Path;

const METADATA: TableDefinition<&str, &[u8]> = TableDefinition::new("metadata");
const CONTENT: TableDefinition<&str, &[u8]> = TableDefinition::new("content");
const BULK: TableDefinition<&str, &[u8]> = TableDefinition::new("bulk");
const BULK_THRESHOLD: usize = 64 * 1024;

pub struct RedbNaive {
    db: Database,
}

impl RedbNaive {
    pub fn open(dir: &Path) -> Result<Self> {
        let db = Database::create(dir.join("browse.redb"))?;
        let txn = db.begin_write()?;
        {
            let _ = txn.open_table(METADATA)?;
            let _ = txn.open_table(CONTENT)?;
            let _ = txn.open_table(BULK)?;
        }
        txn.commit()?;
        Ok(Self { db })
    }
}

impl Backend for RedbNaive {
    fn put_batch(&mut self, items: &[(String, RecordKind, Vec<u8>)]) -> Result<()> {
        let txn = self.db.begin_write()?;
        {
            let mut meta = txn.open_table(METADATA)?;
            let mut content = txn.open_table(CONTENT)?;
            let mut bulk = txn.open_table(BULK)?;
            for (path, kind, payload) in items {
                let key = make_kind_path_key(*kind, path);
                let bytes = encode_record(*kind, payload);
                if kind.is_file() {
                    if payload.len() >= BULK_THRESHOLD {
                        bulk.insert(key.as_str(), bytes.as_slice())?;
                        content.remove(key.as_str())?;
                    } else {
                        content.insert(key.as_str(), bytes.as_slice())?;
                        bulk.remove(key.as_str())?;
                    }
                } else {
                    meta.insert(key.as_str(), bytes.as_slice())?;
                }
            }
        }
        txn.commit()?;
        Ok(())
    }

    fn get(&mut self, path: &str, kind: RecordKind) -> Result<Option<Vec<u8>>> {
        let txn = self.db.begin_read()?;
        let key = make_kind_path_key(kind, path);
        if kind.is_file() {
            let content = txn.open_table(CONTENT)?;
            if let Some(v) = content.get(key.as_str())? {
                return Ok(Some(decode_record(v.value())));
            }
            let bulk = txn.open_table(BULK)?;
            return Ok(bulk.get(key.as_str())?.map(|v| decode_record(v.value())));
        }
        let meta = txn.open_table(METADATA)?;
        Ok(meta.get(key.as_str())?.map(|v| decode_record(v.value())))
    }

    fn delete_exact(&mut self, path: &str) -> Result<usize> {
        let txn = self.db.begin_write()?;
        let mut deleted = 0;
        let kinds = [
            RecordKind::Lookup,
            RecordKind::Attr,
            RecordKind::Dirents,
            RecordKind::File,
        ];
        {
            let mut meta = txn.open_table(METADATA)?;
            let mut content = txn.open_table(CONTENT)?;
            let mut bulk = txn.open_table(BULK)?;
            for k in kinds {
                let key = make_kind_path_key(k, path);
                if k.is_file() {
                    if content.remove(key.as_str())?.is_some() {
                        deleted += 1;
                    }
                    if bulk.remove(key.as_str())?.is_some() {
                        deleted += 1;
                    }
                } else if meta.remove(key.as_str())?.is_some() {
                    deleted += 1;
                }
            }
        }
        txn.commit()?;
        Ok(deleted)
    }

    fn delete_prefix(&mut self, prefix: &str) -> Result<usize> {
        let txn = self.db.begin_write()?;
        let mut deleted = 0;
        let tables = [METADATA, CONTENT, BULK];
        let kind_chars = ['L', 'A', 'D', 'F'];
        for table_def in tables {
            let mut to_delete = Vec::new();
            {
                let table = txn.open_table(table_def)?;
                for ch in &kind_chars {
                    let scan_prefix = format!("{ch}:{prefix}");
                    let mut end = scan_prefix.clone();
                    end.push('\u{ffff}');
                    let range = table.range::<&str>(scan_prefix.as_str()..end.as_str())?;
                    for entry in range {
                        let entry = entry?;
                        let key = entry.0.value().to_string();
                        let path = key
                            .split_once(':')
                            .map_or("", |(_, logical_path)| logical_path);
                        if matches_path_prefix(prefix, path) {
                            to_delete.push(key);
                        }
                    }
                }
            }
            let mut table = txn.open_table(table_def)?;
            for key in &to_delete {
                table.remove(key.as_str())?;
                deleted += 1;
            }
        }
        txn.commit()?;
        Ok(deleted)
    }
}

fn encode_record(kind: RecordKind, payload: &[u8]) -> Vec<u8> {
    let mut v = Vec::with_capacity(2 + payload.len());
    v.push(2); // schema_version
    v.push(kind.as_byte());
    v.extend_from_slice(payload);
    v
}

fn decode_record(bytes: &[u8]) -> Vec<u8> {
    if bytes.len() < 2 {
        return Vec::new();
    }
    bytes[2..].to_vec()
}
