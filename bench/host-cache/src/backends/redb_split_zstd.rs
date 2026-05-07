// Same as redb_split, but blob payloads above 4 KiB are zstd-compressed
// at level 3.

use super::{Backend, RecordKind, matches_path_prefix};
use anyhow::Result;
use redb::{Database, ReadableTable, TableDefinition};
use std::path::Path;

const METADATA: TableDefinition<&[u8], &[u8]> = TableDefinition::new("metadata");
const PATH_TO_BLOB: TableDefinition<&str, &[u8]> = TableDefinition::new("path_to_blob");
const BLOB: TableDefinition<&[u8], &[u8]> = TableDefinition::new("blob");
const BLOB_RC: TableDefinition<&[u8], u32> = TableDefinition::new("blob_rc");

const COMPRESS_THRESHOLD: usize = 4 * 1024;
const ZSTD_LEVEL: i32 = 3;

pub struct RedbSplitZstd {
    db: Database,
}

impl RedbSplitZstd {
    pub fn open(dir: &Path) -> Result<Self> {
        let db = Database::create(dir.join("browse.redb"))?;
        let txn = db.begin_write()?;
        {
            let _ = txn.open_table(METADATA)?;
            let _ = txn.open_table(PATH_TO_BLOB)?;
            let _ = txn.open_table(BLOB)?;
            let _ = txn.open_table(BLOB_RC)?;
        }
        txn.commit()?;
        Ok(Self { db })
    }
}

fn meta_key(kind: RecordKind, path: &str) -> Vec<u8> {
    let mut k = Vec::with_capacity(1 + path.len());
    k.push(kind.as_byte());
    k.extend_from_slice(path.as_bytes());
    k
}

fn encode_blob(payload: &[u8]) -> Vec<u8> {
    if payload.len() < COMPRESS_THRESHOLD {
        let mut v = Vec::with_capacity(1 + payload.len());
        v.push(0); // raw
        v.extend_from_slice(payload);
        v
    } else {
        match zstd::bulk::compress(payload, ZSTD_LEVEL) {
            Ok(c) if c.len() < payload.len() => {
                let mut v = Vec::with_capacity(1 + 4 + c.len());
                v.push(1); // zstd
                v.extend_from_slice(&(payload.len() as u32).to_le_bytes());
                v.extend_from_slice(&c);
                v
            },
            _ => {
                let mut v = Vec::with_capacity(1 + payload.len());
                v.push(0);
                v.extend_from_slice(payload);
                v
            },
        }
    }
}

fn decode_blob(bytes: &[u8]) -> Vec<u8> {
    if bytes.is_empty() {
        return Vec::new();
    }
    match bytes[0] {
        0 => bytes[1..].to_vec(),
        1 => {
            if bytes.len() < 5 {
                return Vec::new();
            }
            let mut len_buf = [0u8; 4];
            len_buf.copy_from_slice(&bytes[1..5]);
            let len = u32::from_le_bytes(len_buf) as usize;
            zstd::bulk::decompress(&bytes[5..], len).unwrap_or_default()
        },
        _ => Vec::new(),
    }
}

impl Backend for RedbSplitZstd {
    fn put_batch(&mut self, items: &[(String, RecordKind, Vec<u8>)]) -> Result<()> {
        let txn = self.db.begin_write()?;
        {
            let mut meta = txn.open_table(METADATA)?;
            let mut p2b = txn.open_table(PATH_TO_BLOB)?;
            let mut blob = txn.open_table(BLOB)?;
            let mut rc = txn.open_table(BLOB_RC)?;
            for (path, kind, payload) in items {
                if kind.is_file() {
                    let hash = blake3::hash(payload);
                    let hash_bytes = hash.as_bytes();
                    if let Some(old_hash) = p2b.get(path.as_str())? {
                        let old = old_hash.value().to_vec();
                        if old.as_slice() != hash_bytes.as_slice() {
                            decref(&mut rc, &mut blob, &old)?;
                        } else {
                            continue;
                        }
                    }
                    let prev_rc = rc.get(hash_bytes.as_slice())?.map(|v| v.value()).unwrap_or(0);
                    if prev_rc == 0 {
                        let encoded = encode_blob(payload);
                        blob.insert(hash_bytes.as_slice(), encoded.as_slice())?;
                    }
                    rc.insert(hash_bytes.as_slice(), prev_rc + 1)?;
                    p2b.insert(path.as_str(), hash_bytes.as_slice())?;
                } else {
                    let key = meta_key(*kind, path);
                    meta.insert(key.as_slice(), payload.as_slice())?;
                }
            }
        }
        txn.commit()?;
        Ok(())
    }

    fn get(&mut self, path: &str, kind: RecordKind) -> Result<Option<Vec<u8>>> {
        let txn = self.db.begin_read()?;
        if kind.is_file() {
            let p2b = txn.open_table(PATH_TO_BLOB)?;
            let Some(hash) = p2b.get(path)? else {
                return Ok(None);
            };
            let blob = txn.open_table(BLOB)?;
            return Ok(blob.get(hash.value())?.map(|v| decode_blob(v.value())));
        }
        let key = meta_key(kind, path);
        let meta = txn.open_table(METADATA)?;
        Ok(meta.get(key.as_slice())?.map(|v| v.value().to_vec()))
    }

    fn delete_exact(&mut self, path: &str) -> Result<usize> {
        let txn = self.db.begin_write()?;
        let mut deleted = 0;
        {
            let mut meta = txn.open_table(METADATA)?;
            for k in [RecordKind::Lookup, RecordKind::Attr, RecordKind::Dirents] {
                if meta.remove(meta_key(k, path).as_slice())?.is_some() {
                    deleted += 1;
                }
            }
            let mut p2b = txn.open_table(PATH_TO_BLOB)?;
            let mut blob = txn.open_table(BLOB)?;
            let mut rc = txn.open_table(BLOB_RC)?;
            if let Some(h) = p2b.remove(path)? {
                let hash = h.value().to_vec();
                decref(&mut rc, &mut blob, &hash)?;
                deleted += 1;
            }
        }
        txn.commit()?;
        Ok(deleted)
    }

    fn delete_prefix(&mut self, prefix: &str) -> Result<usize> {
        let txn = self.db.begin_write()?;
        let mut deleted = 0;
        {
            let mut meta = txn.open_table(METADATA)?;
            let mut to_delete: Vec<Vec<u8>> = Vec::new();
            for k in [RecordKind::Lookup, RecordKind::Attr, RecordKind::Dirents] {
                let start = meta_key(k, prefix);
                let mut end = start.clone();
                end.push(0xFF);
                for entry in meta.range::<&[u8]>(start.as_slice()..end.as_slice())? {
                    let entry = entry?;
                    let key_bytes = entry.0.value();
                    if key_bytes.is_empty() {
                        continue;
                    }
                    let path_bytes = &key_bytes[1..];
                    let path = std::str::from_utf8(path_bytes).unwrap_or("");
                    if matches_path_prefix(prefix, path) {
                        to_delete.push(key_bytes.to_vec());
                    }
                }
            }
            for key in &to_delete {
                meta.remove(key.as_slice())?;
                deleted += 1;
            }

            let mut p2b = txn.open_table(PATH_TO_BLOB)?;
            let mut blob = txn.open_table(BLOB)?;
            let mut rc = txn.open_table(BLOB_RC)?;
            let mut paths_to_delete: Vec<(String, Vec<u8>)> = Vec::new();
            let mut end = String::from(prefix);
            end.push('\u{ffff}');
            for entry in p2b.range::<&str>(prefix..end.as_str())? {
                let entry = entry?;
                let path = entry.0.value().to_string();
                if matches_path_prefix(prefix, &path) {
                    paths_to_delete.push((path, entry.1.value().to_vec()));
                }
            }
            for (path, hash) in &paths_to_delete {
                p2b.remove(path.as_str())?;
                decref(&mut rc, &mut blob, hash)?;
                deleted += 1;
            }
        }
        txn.commit()?;
        Ok(deleted)
    }
}

fn decref(
    rc: &mut redb::Table<&[u8], u32>,
    blob: &mut redb::Table<&[u8], &[u8]>,
    hash: &[u8],
) -> Result<()> {
    let prev = rc.get(hash)?.map(|v| v.value()).unwrap_or(0);
    if prev <= 1 {
        rc.remove(hash)?;
        blob.remove(hash)?;
    } else {
        rc.insert(hash, prev - 1)?;
    }
    Ok(())
}
