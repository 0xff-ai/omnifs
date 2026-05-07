// SQLite (rusqlite) WAL-mode equivalent: three logical relations.
//
//   metadata(kind INT, path TEXT, value BLOB, PRIMARY KEY(kind, path))
//   path_to_blob(path TEXT PRIMARY KEY, hash BLOB)
//   blob(hash BLOB PRIMARY KEY, refcount INT, payload BLOB)

use super::{Backend, RecordKind};
use anyhow::Result;
use rusqlite::{Connection, params};
use std::path::Path;

pub struct SqliteBackend {
    conn: Connection,
}

impl SqliteBackend {
    pub fn open(dir: &Path) -> Result<Self> {
        let conn = Connection::open(dir.join("browse.sqlite"))?;
        conn.pragma_update(None, "journal_mode", "WAL")?;
        conn.pragma_update(None, "synchronous", "NORMAL")?;
        conn.pragma_update(None, "temp_store", "MEMORY")?;
        conn.pragma_update(None, "cache_size", -64_000)?; // 64 MiB
        conn.execute_batch(
            r"
            CREATE TABLE IF NOT EXISTS metadata (
                kind INTEGER NOT NULL,
                path TEXT NOT NULL,
                value BLOB NOT NULL,
                PRIMARY KEY (kind, path)
            ) WITHOUT ROWID;
            CREATE TABLE IF NOT EXISTS path_to_blob (
                path TEXT PRIMARY KEY,
                hash BLOB NOT NULL
            ) WITHOUT ROWID;
            CREATE TABLE IF NOT EXISTS blob (
                hash BLOB PRIMARY KEY,
                refcount INTEGER NOT NULL,
                payload BLOB NOT NULL
            ) WITHOUT ROWID;
            CREATE INDEX IF NOT EXISTS path_to_blob_prefix
                ON path_to_blob (path);
            ",
        )?;
        Ok(Self { conn })
    }
}

impl Backend for SqliteBackend {
    fn put_batch(&mut self, items: &[(String, RecordKind, Vec<u8>)]) -> Result<()> {
        let txn = self.conn.transaction()?;
        {
            let mut meta_stmt = txn.prepare_cached(
                "INSERT OR REPLACE INTO metadata(kind, path, value) VALUES (?, ?, ?)",
            )?;
            let mut p2b_get = txn.prepare_cached(
                "SELECT hash FROM path_to_blob WHERE path = ?",
            )?;
            let mut p2b_put = txn.prepare_cached(
                "INSERT OR REPLACE INTO path_to_blob(path, hash) VALUES (?, ?)",
            )?;
            let mut blob_get_rc = txn.prepare_cached(
                "SELECT refcount FROM blob WHERE hash = ?",
            )?;
            let mut blob_insert = txn.prepare_cached(
                "INSERT INTO blob(hash, refcount, payload) VALUES (?, 1, ?)",
            )?;
            let mut blob_bump = txn.prepare_cached(
                "UPDATE blob SET refcount = refcount + 1 WHERE hash = ?",
            )?;
            let mut blob_decref = txn.prepare_cached(
                "UPDATE blob SET refcount = refcount - 1 WHERE hash = ?",
            )?;
            let mut blob_delete_zero = txn.prepare_cached(
                "DELETE FROM blob WHERE hash = ? AND refcount <= 0",
            )?;
            for (path, kind, payload) in items {
                if kind.is_file() {
                    let hash = blake3::hash(payload);
                    let hash_bytes = hash.as_bytes();
                    let old_hash: Option<Vec<u8>> = p2b_get
                        .query_row(params![path], |r| r.get(0))
                        .ok();
                    if let Some(old) = &old_hash
                        && old.as_slice() != hash_bytes.as_slice()
                    {
                        blob_decref.execute(params![old.as_slice()])?;
                        blob_delete_zero.execute(params![old.as_slice()])?;
                    } else if old_hash.as_deref() == Some(hash_bytes.as_slice()) {
                        continue;
                    }
                    let prev_rc: Option<i64> =
                        blob_get_rc.query_row(params![hash_bytes.as_slice()], |r| r.get(0)).ok();
                    if prev_rc.is_some() {
                        blob_bump.execute(params![hash_bytes.as_slice()])?;
                    } else {
                        blob_insert
                            .execute(params![hash_bytes.as_slice(), payload.as_slice()])?;
                    }
                    p2b_put.execute(params![path, hash_bytes.as_slice()])?;
                } else {
                    meta_stmt.execute(params![kind.as_byte() as i64, path, payload.as_slice()])?;
                }
            }
        }
        txn.commit()?;
        Ok(())
    }

    fn get(&mut self, path: &str, kind: RecordKind) -> Result<Option<Vec<u8>>> {
        if kind.is_file() {
            let mut q = self.conn.prepare_cached(
                "SELECT b.payload FROM path_to_blob p JOIN blob b ON p.hash = b.hash WHERE p.path = ?",
            )?;
            let v: Option<Vec<u8>> = q.query_row(params![path], |r| r.get(0)).ok();
            Ok(v)
        } else {
            let mut q = self
                .conn
                .prepare_cached("SELECT value FROM metadata WHERE kind = ? AND path = ?")?;
            let v: Option<Vec<u8>> = q
                .query_row(params![kind.as_byte() as i64, path], |r| r.get(0))
                .ok();
            Ok(v)
        }
    }

    fn delete_exact(&mut self, path: &str) -> Result<usize> {
        let txn = self.conn.transaction()?;
        let mut deleted = 0;
        {
            let mut del_meta = txn
                .prepare_cached("DELETE FROM metadata WHERE path = ?")?;
            deleted += del_meta.execute(params![path])?;
            let hash: Option<Vec<u8>> = txn
                .prepare_cached("SELECT hash FROM path_to_blob WHERE path = ?")?
                .query_row(params![path], |r| r.get(0))
                .ok();
            if let Some(h) = hash {
                txn.execute("DELETE FROM path_to_blob WHERE path = ?", params![path])?;
                txn.execute(
                    "UPDATE blob SET refcount = refcount - 1 WHERE hash = ?",
                    params![h.as_slice()],
                )?;
                txn.execute(
                    "DELETE FROM blob WHERE hash = ? AND refcount <= 0",
                    params![h.as_slice()],
                )?;
                deleted += 1;
            }
        }
        txn.commit()?;
        Ok(deleted)
    }

    fn delete_prefix(&mut self, prefix: &str) -> Result<usize> {
        let txn = self.conn.transaction()?;
        let mut deleted = 0;
        let like_pattern = format!("{prefix}/%");
        {
            // metadata
            let mut del_meta = txn.prepare_cached(
                "DELETE FROM metadata WHERE path = ? OR path LIKE ?",
            )?;
            deleted += del_meta.execute(params![prefix, like_pattern])?;

            // path_to_blob: collect hashes first, then delete + decref
            let mut hashes: Vec<Vec<u8>> = Vec::new();
            {
                let mut q = txn.prepare_cached(
                    "SELECT path, hash FROM path_to_blob WHERE path = ? OR path LIKE ?",
                )?;
                let mut rows = q.query(params![prefix, like_pattern])?;
                while let Some(row) = rows.next()? {
                    let h: Vec<u8> = row.get(1)?;
                    hashes.push(h);
                }
            }
            txn.execute(
                "DELETE FROM path_to_blob WHERE path = ? OR path LIKE ?",
                params![prefix, like_pattern],
            )?;
            for h in &hashes {
                txn.execute(
                    "UPDATE blob SET refcount = refcount - 1 WHERE hash = ?",
                    params![h.as_slice()],
                )?;
                txn.execute(
                    "DELETE FROM blob WHERE hash = ? AND refcount <= 0",
                    params![h.as_slice()],
                )?;
                deleted += 1;
            }
        }
        txn.commit()?;
        Ok(deleted)
    }
}
