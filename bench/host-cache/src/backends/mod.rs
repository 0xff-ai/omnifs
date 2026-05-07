// Backend trait + dispatch.

use anyhow::Result;
use clap::ValueEnum;
use std::path::Path;

pub mod fjall_split;
pub mod redb_naive;
pub mod redb_split;
pub mod redb_split_zstd;
pub mod sqlite;

#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum RecordKind {
    Lookup,
    Attr,
    Dirents,
    File,
}

impl RecordKind {
    pub fn as_byte(self) -> u8 {
        match self {
            Self::Lookup => b'L',
            Self::Attr => b'A',
            Self::Dirents => b'D',
            Self::File => b'F',
        }
    }
    pub fn is_file(self) -> bool {
        matches!(self, Self::File)
    }
}

pub trait Backend {
    fn put_batch(&mut self, items: &[(String, RecordKind, Vec<u8>)]) -> Result<()>;
    fn get(&mut self, path: &str, kind: RecordKind) -> Result<Option<Vec<u8>>>;
    fn delete_exact(&mut self, path: &str) -> Result<usize>;
    fn delete_prefix(&mut self, prefix: &str) -> Result<usize>;
    /// Force any buffered state to disk so footprint measurement is fair.
    fn flush(&mut self) -> Result<()> {
        Ok(())
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, ValueEnum)]
pub enum BackendKind {
    /// Today's shape: one redb DB, three tables, kind-prefixed string keys.
    RedbNaive,
    /// Proposed: redb with metadata + path_to_blob + blob (refcounted).
    RedbSplit,
    /// Proposed + zstd compression on blob payloads.
    RedbSplitZstd,
    /// SQLite WAL with three tables.
    Sqlite,
    /// Fjall LSM with native KV separation (key/value partitions).
    FjallSplit,
}

impl BackendKind {
    pub fn all() -> Vec<BackendKind> {
        vec![
            Self::RedbNaive,
            Self::RedbSplit,
            Self::RedbSplitZstd,
            Self::Sqlite,
            Self::FjallSplit,
        ]
    }
    pub fn slug(self) -> &'static str {
        match self {
            Self::RedbNaive => "redb_naive",
            Self::RedbSplit => "redb_split",
            Self::RedbSplitZstd => "redb_split_zstd",
            Self::Sqlite => "sqlite",
            Self::FjallSplit => "fjall_split",
        }
    }
    pub fn open(self, dir: &Path) -> Result<Box<dyn Backend>> {
        Ok(match self {
            Self::RedbNaive => Box::new(redb_naive::RedbNaive::open(dir)?),
            Self::RedbSplit => Box::new(redb_split::RedbSplit::open(dir)?),
            Self::RedbSplitZstd => Box::new(redb_split_zstd::RedbSplitZstd::open(dir)?),
            Self::Sqlite => Box::new(sqlite::SqliteBackend::open(dir)?),
            Self::FjallSplit => Box::new(fjall_split::FjallSplit::open(dir)?),
        })
    }
}

/// Helper used by backends that compose composite keys as string prefixes.
pub fn make_kind_path_key(kind: RecordKind, path: &str) -> String {
    let mut s = String::with_capacity(2 + path.len());
    s.push(kind.as_byte() as char);
    s.push(':');
    s.push_str(path);
    s
}

/// Used by prefix scans in backends with single-table layouts.
pub fn matches_path_prefix(prefix: &str, path: &str) -> bool {
    if !path.starts_with(prefix) {
        return false;
    }
    let rest = &path[prefix.len()..];
    rest.is_empty() || rest.starts_with('/') || prefix.ends_with('/')
}
