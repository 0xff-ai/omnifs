//! Per-table subtree: schema, indexes, count, sample.

use std::str::FromStr;

use omnifs_sdk::prelude::*;

use crate::sqlite_backend::SqliteBackend;
use crate::{Result, State};

/// Bind capture for `/_tables/{name}`. Only validates that the
/// name is non-empty and contains no NUL/slash/backslash; existence
/// in the database is checked when handlers actually run.
#[derive(Clone, Debug)]
pub struct TableName(String);

impl TableName {
    pub fn into_inner(self) -> String {
        self.0
    }
}

impl FromStr for TableName {
    type Err = ();

    fn from_str(s: &str) -> std::result::Result<Self, Self::Err> {
        if s.is_empty() || s.contains(['\0', '/', '\\']) {
            return Err(());
        }
        Ok(Self(s.to_string()))
    }
}

impl std::fmt::Display for TableName {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(&self.0)
    }
}

pub struct TableSubtree {
    pub name: String,
}

#[subtree]
impl TableSubtree {
    #[dir("/")]
    fn root(cx: &BindCtx<'_, State, TableSubtree>) -> Result<Projection> {
        ensure_table_exists(cx)?;
        // Sibling `#[file]` handlers project their own entries; we
        // mark the listing exhaustive so the host can satisfy
        // negative lookups locally.
        let mut p = Projection::new();
        p.page(PageStatus::Exhaustive);
        Ok(p)
    }

    #[file("/_schema.sql")]
    fn schema_sql(cx: &BindCtx<'_, State, TableSubtree>) -> Result<FileContent> {
        let (bytes, version) = with_backend(cx, |backend, table| {
            let sql = backend
                .table_create_sql(table)
                .map_err(|e| ProviderError::internal(format!("read schema: {e}")))?
                .ok_or_else(|| ProviderError::not_found(format!("table not found: {table}")))?;
            let version = backend.table_version(table).ok();
            Ok((sql.into_bytes(), version))
        })?;
        Ok(file_with_table_version(bytes, version))
    }

    #[file("/_schema.json")]
    fn schema_json(cx: &BindCtx<'_, State, TableSubtree>) -> Result<FileContent> {
        let (bytes, version) = with_backend(cx, |backend, table| {
            if !backend
                .table_exists(table)
                .map_err(|e| ProviderError::internal(format!("table_exists: {e}")))?
            {
                return Err(ProviderError::not_found(format!(
                    "table not found: {table}"
                )));
            }
            let cols = backend
                .table_columns(table)
                .map_err(|e| ProviderError::internal(format!("columns: {e}")))?;
            let mut bytes = serde_json::to_vec_pretty(&cols)
                .map_err(|e| ProviderError::internal(format!("encode schema: {e}")))?;
            bytes.push(b'\n');
            let version = backend.table_version(table).ok();
            Ok((bytes, version))
        })?;
        Ok(file_with_table_version(bytes, version))
    }

    #[file("/_indexes.json")]
    fn indexes_json(cx: &BindCtx<'_, State, TableSubtree>) -> Result<FileContent> {
        let (bytes, version) = with_backend(cx, |backend, table| {
            if !backend
                .table_exists(table)
                .map_err(|e| ProviderError::internal(format!("table_exists: {e}")))?
            {
                return Err(ProviderError::not_found(format!(
                    "table not found: {table}"
                )));
            }
            let idx = backend
                .table_indexes(table)
                .map_err(|e| ProviderError::internal(format!("indexes: {e}")))?;
            let mut bytes = serde_json::to_vec_pretty(&idx)
                .map_err(|e| ProviderError::internal(format!("encode indexes: {e}")))?;
            bytes.push(b'\n');
            let version = backend.table_version(table).ok();
            Ok((bytes, version))
        })?;
        Ok(file_with_table_version(bytes, version))
    }

    #[file("/_count.txt")]
    fn count(cx: &BindCtx<'_, State, TableSubtree>) -> Result<FileContent> {
        let (bytes, version) = with_backend(cx, |backend, table| {
            if !backend
                .table_exists(table)
                .map_err(|e| ProviderError::internal(format!("table_exists: {e}")))?
            {
                return Err(ProviderError::not_found(format!(
                    "table not found: {table}"
                )));
            }
            let count = backend
                .table_row_count(table)
                .map_err(|e| ProviderError::internal(format!("count: {e}")))?;
            let mut bytes = count.to_string().into_bytes();
            bytes.push(b'\n');
            let version = backend.table_version(table).ok();
            Ok((bytes, version))
        })?;
        Ok(file_with_table_version(bytes, version))
    }

    #[file("/_sample.json")]
    fn sample(cx: &BindCtx<'_, State, TableSubtree>) -> Result<FileContent> {
        let limit = cx.state(|state| state.config.sample_limit);
        let (bytes, version) = with_backend(cx, |backend, table| {
            if !backend
                .table_exists(table)
                .map_err(|e| ProviderError::internal(format!("table_exists: {e}")))?
            {
                return Err(ProviderError::not_found(format!(
                    "table not found: {table}"
                )));
            }
            let rows = backend
                .table_sample(table, limit)
                .map_err(|e| ProviderError::internal(format!("sample: {e}")))?;
            let mut bytes = serde_json::to_vec_pretty(&rows)
                .map_err(|e| ProviderError::internal(format!("encode sample: {e}")))?;
            bytes.push(b'\n');
            let version = backend.table_version(table).ok();
            Ok((bytes, version))
        })?;
        // Samples can be large; switch to a deferred ranged
        // projection when over the inline cap so the host serves
        // bytes directly from the buffer without round-tripping.
        if bytes.len() > MAX_PROJECTED_BYTES {
            let attrs = build_attrs(bytes.len() as u64, version);
            return Ok(FileContent::range_bytes(attrs, bytes));
        }
        Ok(file_with_table_version(bytes, version))
    }
}

fn with_backend<R>(
    cx: &BindCtx<'_, State, TableSubtree>,
    f: impl FnOnce(&SqliteBackend, &str) -> Result<R>,
) -> Result<R> {
    let table = cx.bindings().name.clone();
    cx.state(|state| {
        let backend = state.backend.borrow();
        f(&backend, &table)
    })
}

fn ensure_table_exists(cx: &BindCtx<'_, State, TableSubtree>) -> Result<()> {
    let table = cx.bindings().name.clone();
    cx.state(|state| {
        let backend = state.backend.borrow();
        match backend.table_exists(&table) {
            Ok(true) => Ok(()),
            Ok(false) => Err(ProviderError::not_found(format!(
                "table not found: {table}"
            ))),
            Err(e) => Err(ProviderError::internal(format!("table_exists: {e}"))),
        }
    })
}

fn file_with_table_version(bytes: Vec<u8>, version: Option<String>) -> FileContent {
    let size = u64::try_from(bytes.len()).unwrap_or(u64::MAX);
    let attrs = build_attrs(size, version);
    FileContent::bytes_with_attrs(attrs, bytes)
}

fn build_attrs(size: u64, version: Option<String>) -> FileAttrs {
    let mut attrs = FileAttrs::new(Size::Exact(size), Stability::Mutable);
    if let Some(v) = version {
        attrs = attrs.with_version(v);
    }
    attrs
}
