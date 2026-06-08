//! `SQLite` backend (`rusqlite`) and thin backend errors.
//!
//! Kept thin: providers surface domain errors
//! through `ProviderError`, so the backend layer just classifies
//! between "open failed" and everything else.

use thiserror::Error;

#[derive(Debug, Error)]
pub(crate) enum BackendError {
    /// The connection could not be opened (file missing, sandbox
    /// denied, journal-mode mismatch, etc.).
    #[error("open database: {0}")]
    Open(String),
    /// A query against an open connection failed.
    #[error("query database: {0}")]
    Query(#[from] rusqlite::Error),
}

// rusqlite connection wrapper and query helpers.

use std::path::Path;

use rusqlite::{Connection, OpenFlags, types::ValueRef};
use serde::{Deserialize, Serialize};
use serde_json::{Number, Value as JsonValue, json};
use sha2::{Digest, Sha256};

/// Encapsulates the `SQLite` connection plus helpers for the
/// surfaces the provider needs (schema, indexes, count, sample).
pub(crate) struct SqliteBackend {
    pub(crate) conn: Connection,
    /// The configured database path. Re-exposed for `meta/path.txt`.
    pub(crate) path: String,
    /// Whether the connection was opened read-only.
    pub(crate) read_only: bool,
}

impl SqliteBackend {
    pub fn open(path: &str, read_only: bool) -> Result<Self, BackendError> {
        let conn = open_connection(path, read_only)?;
        Ok(Self {
            conn,
            path: path.to_string(),
            read_only,
        })
    }

    pub fn library_version(&self) -> &'static str {
        let _ = self;
        rusqlite::version()
    }

    /// File-level metadata: size, page count/size, app id, user
    /// version, journal mode. Pulled via PRAGMA so we don't need to
    /// stat the file separately (PRAGMA reads the header `SQLite`
    /// itself sees).
    pub fn file_info(&self) -> Result<FileInfo, BackendError> {
        let page_size: i64 = self.conn.query_row("PRAGMA page_size", [], |r| r.get(0))?;
        let page_count: i64 = self.conn.query_row("PRAGMA page_count", [], |r| r.get(0))?;
        let app_id: i64 = self
            .conn
            .query_row("PRAGMA application_id", [], |r| r.get(0))
            .unwrap_or(0);
        let user_version: i64 = self
            .conn
            .query_row("PRAGMA user_version", [], |r| r.get(0))
            .unwrap_or(0);
        let journal_mode: String = self
            .conn
            .query_row("PRAGMA journal_mode", [], |r| r.get(0))
            .unwrap_or_else(|_| "unknown".to_string());
        let size_bytes = u64::try_from(page_size)
            .unwrap_or(0)
            .saturating_mul(u64::try_from(page_count).unwrap_or(0));
        Ok(FileInfo {
            path: self.path.clone(),
            read_only: self.read_only,
            sqlite_version: self.library_version().to_string(),
            size_bytes,
            page_size,
            page_count,
            application_id: app_id,
            user_version,
            journal_mode,
        })
    }

    /// List all user tables (excluding `sqlite_*` internals).
    pub fn list_tables(&self) -> Result<Vec<String>, BackendError> {
        let mut stmt = self.conn.prepare(
            "SELECT name FROM sqlite_master \
             WHERE type='table' AND name NOT LIKE 'sqlite_%' \
             ORDER BY name",
        )?;
        let rows = stmt
            .query_map([], |row| row.get::<_, String>(0))?
            .collect::<Result<Vec<_>, _>>()?;
        Ok(rows)
    }

    /// Verify a table exists by name. Returns `None` if it does not
    /// (used to convert lookup failures to `ENOENT`-flavored
    /// `ProviderError::not_found` at the handler boundary).
    pub fn table_exists(&self, name: &str) -> Result<bool, BackendError> {
        let count: i64 = self.conn.query_row(
            "SELECT count(*) FROM sqlite_master \
             WHERE type='table' AND name = ?1",
            [name],
            |row| row.get(0),
        )?;
        Ok(count > 0)
    }

    /// `CREATE TABLE ...` SQL from `sqlite_master`. Returns the
    /// SQL as stored, with a trailing newline. `sqlite_master.sql`
    /// can be NULL for some internal tables; the user-table filter
    /// avoids most of those.
    pub fn table_create_sql(&self, name: &str) -> Result<Option<String>, BackendError> {
        let raw: Option<String> = self
            .conn
            .query_row(
                "SELECT sql FROM sqlite_master WHERE type='table' AND name = ?1",
                [name],
                |row| row.get::<_, Option<String>>(0),
            )
            .ok()
            .flatten();
        Ok(raw.map(|sql| {
            let mut s = sql;
            s.push('\n');
            s
        }))
    }

    pub fn table_columns(&self, table: &str) -> Result<Vec<ColumnInfo>, BackendError> {
        // PRAGMA table_info doesn't accept bound parameters, so we
        // splice the table name. The name is sourced from sqlite_master
        // (verified by `table_exists`); we still escape embedded
        // quotes defensively.
        let pragma = format!("PRAGMA table_info(\"{}\")", escape_identifier(table));
        let mut stmt = self.conn.prepare(&pragma)?;
        let rows = stmt
            .query_map([], |row| {
                Ok(ColumnInfo {
                    cid: row.get::<_, i64>(0)?,
                    name: row.get::<_, String>(1)?,
                    decl_type: row.get::<_, Option<String>>(2)?,
                    not_null: row.get::<_, i64>(3)? != 0,
                    default_value: row.get::<_, Option<String>>(4)?,
                    pk_position: row.get::<_, i64>(5)?,
                })
            })?
            .collect::<Result<Vec<_>, _>>()?;
        Ok(rows)
    }

    pub fn table_indexes(&self, table: &str) -> Result<Vec<IndexInfo>, BackendError> {
        let list_sql = format!("PRAGMA index_list(\"{}\")", escape_identifier(table));
        let mut list = self.conn.prepare(&list_sql)?;
        let infos = list
            .query_map([], |row| {
                Ok(IndexListRow {
                    seq: row.get::<_, i64>(0)?,
                    name: row.get::<_, String>(1)?,
                    unique: row.get::<_, i64>(2)? != 0,
                    origin: row.get::<_, String>(3)?,
                    partial: row.get::<_, i64>(4)? != 0,
                })
            })?
            .collect::<Result<Vec<_>, _>>()?;
        let mut out = Vec::with_capacity(infos.len());
        for info in infos {
            let cols_sql = format!("PRAGMA index_info(\"{}\")", escape_identifier(&info.name));
            let mut cols = self.conn.prepare(&cols_sql)?;
            let columns = cols
                .query_map([], |row| {
                    Ok(IndexColumn {
                        seqno: row.get::<_, i64>(0)?,
                        cid: row.get::<_, i64>(1)?,
                        name: row.get::<_, Option<String>>(2)?,
                    })
                })?
                .collect::<Result<Vec<_>, _>>()?;
            out.push(IndexInfo {
                name: info.name,
                seq: info.seq,
                unique: info.unique,
                origin: info.origin,
                partial: info.partial,
                columns,
            });
        }
        Ok(out)
    }

    pub fn table_row_count(&self, table: &str) -> Result<i64, BackendError> {
        let sql = format!("SELECT count(*) FROM \"{}\"", escape_identifier(table));
        let count: i64 = self.conn.query_row(&sql, [], |row| row.get(0))?;
        Ok(count)
    }

    pub fn table_sample(&self, table: &str, limit: u32) -> Result<Vec<JsonValue>, BackendError> {
        let sql = format!("SELECT * FROM \"{}\" LIMIT ?1", escape_identifier(table));
        let mut stmt = self.conn.prepare(&sql)?;
        let column_names: Vec<String> = stmt
            .column_names()
            .iter()
            .map(|s| (*s).to_string())
            .collect();
        let mut rows = stmt.query([limit])?;
        let mut out = Vec::new();
        while let Some(row) = rows.next()? {
            let mut entry = serde_json::Map::with_capacity(column_names.len());
            for (idx, col) in column_names.iter().enumerate() {
                let value = match row.get_ref(idx)? {
                    ValueRef::Null => JsonValue::Null,
                    ValueRef::Integer(i) => JsonValue::Number(i.into()),
                    ValueRef::Real(f) => {
                        Number::from_f64(f).map_or(JsonValue::Null, JsonValue::Number)
                    },
                    ValueRef::Text(t) => match std::str::from_utf8(t) {
                        Ok(s) => JsonValue::String(s.to_string()),
                        Err(_) => json!({ "_blob_hex": hex::encode(t) }),
                    },
                    ValueRef::Blob(b) => json!({ "_blob_hex": hex::encode(b) }),
                };
                entry.insert(col.clone(), value);
            }
            out.push(JsonValue::Object(entry));
        }
        Ok(out)
    }

    /// Hash for table-specific projections: name + create SQL +
    /// row count. Changes when schema or row count moves.
    pub fn table_version(&self, table: &str) -> Result<String, BackendError> {
        let create = self.table_create_sql(table)?.unwrap_or_default();
        let count = self.table_row_count(table).unwrap_or(0);
        let mut hasher = Sha256::new();
        hasher.update(table.as_bytes());
        hasher.update([0u8]);
        hasher.update(create.as_bytes());
        hasher.update([0u8]);
        hasher.update(count.to_le_bytes());
        Ok(hex::encode(hasher.finalize()))
    }
}

fn open_connection(path: &str, read_only: bool) -> Result<Connection, BackendError> {
    let (flags, uri) = if read_only {
        (
            OpenFlags::SQLITE_OPEN_READ_ONLY | OpenFlags::SQLITE_OPEN_URI,
            format!("file:{}?mode=ro&immutable=1", encode_uri_path(path)),
        )
    } else {
        (
            OpenFlags::SQLITE_OPEN_READ_WRITE | OpenFlags::SQLITE_OPEN_URI,
            format!("file:{}", encode_uri_path(path)),
        )
    };
    let conn = match Connection::open_with_flags(&uri, flags) {
        Ok(c) => c,
        Err(e) if read_only => {
            // Some databases left mid-WAL refuse to open with
            // `immutable=1`. Retry without it before giving up. If
            // even that fails, surface the original error.
            let retry_uri = format!("file:{}?mode=ro", encode_uri_path(path));
            Connection::open_with_flags(
                &retry_uri,
                OpenFlags::SQLITE_OPEN_READ_ONLY | OpenFlags::SQLITE_OPEN_URI,
            )
            .map_err(|_| BackendError::Open(e.to_string()))?
        },
        Err(e) => return Err(BackendError::Open(e.to_string())),
    };
    // Confirm the file is reachable. A bad path or missing file
    // tends to surface as a query error rather than at open time
    // with URI mode.
    conn.query_row("SELECT 1", [], |_| Ok(()))
        .map_err(|e| BackendError::Open(e.to_string()))?;
    let _ = Path::new(path);
    Ok(conn)
}

fn encode_uri_path(path: &str) -> String {
    // SQLite URIs are file-URI-shaped: spaces, '?', '#' need
    // percent-encoding so they don't break query-string parsing.
    let mut out = String::with_capacity(path.len());
    for ch in path.chars() {
        match ch {
            ' ' => out.push_str("%20"),
            '?' => out.push_str("%3F"),
            '#' => out.push_str("%23"),
            '%' => out.push_str("%25"),
            c => out.push(c),
        }
    }
    out
}

fn escape_identifier(name: &str) -> String {
    name.replace('"', "\"\"")
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub(crate) struct FileInfo {
    pub path: String,
    pub read_only: bool,
    pub sqlite_version: String,
    pub size_bytes: u64,
    pub page_size: i64,
    pub page_count: i64,
    pub application_id: i64,
    pub user_version: i64,
    pub journal_mode: String,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub(crate) struct ColumnInfo {
    pub cid: i64,
    pub name: String,
    /// Declared type (`TEXT`, `INTEGER`, etc.). `None` means the
    /// column has no declared type (a `SQLite` quirk).
    pub decl_type: Option<String>,
    pub not_null: bool,
    pub default_value: Option<String>,
    /// `0` means not a PK member; `>=1` is its position in a
    /// composite PK.
    pub pk_position: i64,
}

#[derive(Debug, Clone)]
struct IndexListRow {
    seq: i64,
    name: String,
    unique: bool,
    /// `c` = created via CREATE INDEX; `u` = unique constraint;
    /// `pk` = primary key.
    origin: String,
    partial: bool,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub(crate) struct IndexInfo {
    pub name: String,
    pub seq: i64,
    pub unique: bool,
    pub origin: String,
    pub partial: bool,
    pub columns: Vec<IndexColumn>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub(crate) struct IndexColumn {
    pub seqno: i64,
    pub cid: i64,
    pub name: Option<String>,
}
