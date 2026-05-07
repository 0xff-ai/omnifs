// fjall LSM with key/value separation. fjall-3 lets a keyspace opt
// into KvSeparationOptions so large values move to a side blob log.

use super::{Backend, RecordKind, matches_path_prefix};
use anyhow::Result;
use fjall::{Database, Keyspace, KeyspaceCreateOptions, KvSeparationOptions, PersistMode};
use std::path::Path;

pub struct FjallSplit {
    db: Database,
    metadata: Keyspace,
    path_to_blob: Keyspace,
    blob: Keyspace,
    blob_rc: Keyspace,
}

impl FjallSplit {
    pub fn open(dir: &Path) -> Result<Self> {
        let db = Database::builder(dir.join("fjall")).open()?;
        let metadata = db.keyspace("metadata", KeyspaceCreateOptions::default)?;
        let path_to_blob = db.keyspace("path_to_blob", KeyspaceCreateOptions::default)?;
        let blob = db.keyspace("blob", || {
            KeyspaceCreateOptions::default()
                .with_kv_separation(Some(KvSeparationOptions::default()))
        })?;
        let blob_rc = db.keyspace("blob_rc", KeyspaceCreateOptions::default)?;
        Ok(Self {
            db,
            metadata,
            path_to_blob,
            blob,
            blob_rc,
        })
    }
}

fn meta_key(kind: RecordKind, path: &str) -> Vec<u8> {
    let mut k = Vec::with_capacity(1 + path.len());
    k.push(kind.as_byte());
    k.extend_from_slice(path.as_bytes());
    k
}

fn rc_get(p: &Keyspace, hash: &[u8]) -> Result<u32> {
    let v = p.get(hash)?;
    Ok(match v {
        Some(b) if b.len() >= 4 => {
            let mut buf = [0u8; 4];
            buf.copy_from_slice(&b[..4]);
            u32::from_le_bytes(buf)
        },
        _ => 0,
    })
}
fn rc_set(p: &Keyspace, hash: &[u8], n: u32) -> Result<()> {
    p.insert(hash, n.to_le_bytes().as_slice())?;
    Ok(())
}

impl Backend for FjallSplit {
    fn put_batch(&mut self, items: &[(String, RecordKind, Vec<u8>)]) -> Result<()> {
        for (path, kind, payload) in items {
            if kind.is_file() {
                let hash = blake3::hash(payload);
                let hash_bytes = hash.as_bytes();
                if let Some(old_hash) = self.path_to_blob.get(path.as_bytes())? {
                    let old: Vec<u8> = old_hash.to_vec();
                    if old.as_slice() != hash_bytes.as_slice() {
                        let prev = rc_get(&self.blob_rc, &old)?;
                        if prev <= 1 {
                            self.blob_rc.remove(old.as_slice())?;
                            self.blob.remove(old.as_slice())?;
                        } else {
                            rc_set(&self.blob_rc, &old, prev - 1)?;
                        }
                    } else {
                        continue;
                    }
                }
                let prev = rc_get(&self.blob_rc, hash_bytes)?;
                if prev == 0 {
                    self.blob.insert(hash_bytes.as_slice(), payload.as_slice())?;
                }
                rc_set(&self.blob_rc, hash_bytes, prev + 1)?;
                self.path_to_blob
                    .insert(path.as_bytes(), hash_bytes.as_slice())?;
            } else {
                let key = meta_key(*kind, path);
                self.metadata.insert(key.as_slice(), payload.as_slice())?;
            }
        }
        Ok(())
    }

    fn get(&mut self, path: &str, kind: RecordKind) -> Result<Option<Vec<u8>>> {
        if kind.is_file() {
            let Some(hash) = self.path_to_blob.get(path.as_bytes())? else {
                return Ok(None);
            };
            return Ok(self.blob.get(&hash[..])?.map(|v| v.to_vec()));
        }
        let key = meta_key(kind, path);
        Ok(self.metadata.get(key.as_slice())?.map(|v| v.to_vec()))
    }

    fn delete_exact(&mut self, path: &str) -> Result<usize> {
        let mut deleted = 0;
        for k in [RecordKind::Lookup, RecordKind::Attr, RecordKind::Dirents] {
            if self.metadata.get(meta_key(k, path).as_slice())?.is_some() {
                self.metadata.remove(meta_key(k, path).as_slice())?;
                deleted += 1;
            }
        }
        if let Some(h) = self.path_to_blob.get(path.as_bytes())? {
            let h: Vec<u8> = h.to_vec();
            self.path_to_blob.remove(path.as_bytes())?;
            let prev = rc_get(&self.blob_rc, &h)?;
            if prev <= 1 {
                self.blob_rc.remove(h.as_slice())?;
                self.blob.remove(h.as_slice())?;
            } else {
                rc_set(&self.blob_rc, &h, prev - 1)?;
            }
            deleted += 1;
        }
        Ok(deleted)
    }

    fn delete_prefix(&mut self, prefix: &str) -> Result<usize> {
        let mut deleted = 0;

        // metadata: scan each kind range
        for k in [RecordKind::Lookup, RecordKind::Attr, RecordKind::Dirents] {
            let start = meta_key(k, prefix);
            let mut end = start.clone();
            end.push(0xFF);
            let mut to_delete: Vec<Vec<u8>> = Vec::new();
            for guard in self.metadata.range(start.as_slice()..end.as_slice()) {
                let (key, _) = guard.into_inner()?;
                let key_bytes: Vec<u8> = key.to_vec();
                if key_bytes.is_empty() {
                    continue;
                }
                let path_bytes = &key_bytes[1..];
                let path = std::str::from_utf8(path_bytes).unwrap_or("");
                if matches_path_prefix(prefix, path) {
                    to_delete.push(key_bytes);
                }
            }
            for k in &to_delete {
                self.metadata.remove(k.as_slice())?;
                deleted += 1;
            }
        }

        // path_to_blob
        let mut paths_and_hashes: Vec<(Vec<u8>, Vec<u8>)> = Vec::new();
        let prefix_bytes = prefix.as_bytes().to_vec();
        let mut end = prefix_bytes.clone();
        end.push(0xFF);
        for guard in self.path_to_blob.range(prefix_bytes.as_slice()..end.as_slice()) {
            let (path_b, hash_b) = guard.into_inner()?;
            let path = std::str::from_utf8(&path_b).unwrap_or("");
            if matches_path_prefix(prefix, path) {
                paths_and_hashes.push((path_b.to_vec(), hash_b.to_vec()));
            }
        }
        for (path, hash) in &paths_and_hashes {
            self.path_to_blob.remove(path.as_slice())?;
            let prev = rc_get(&self.blob_rc, hash)?;
            if prev <= 1 {
                self.blob_rc.remove(hash.as_slice())?;
                self.blob.remove(hash.as_slice())?;
            } else {
                rc_set(&self.blob_rc, hash, prev - 1)?;
            }
            deleted += 1;
        }
        Ok(deleted)
    }

    fn flush(&mut self) -> Result<()> {
        self.db.persist(PersistMode::SyncAll)?;
        Ok(())
    }
}
