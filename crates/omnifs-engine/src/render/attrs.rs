use std::fs::Metadata;
use std::time::{SystemTime, UNIX_EPOCH};

/// Maximum whole-file payload the tree will materialize in memory for one read.
///
/// Ranged reads stream through [`crate::RangedHandle`] and are not subject to
/// this cap.
pub const MATERIALIZE_MAX_BYTES: u64 = 64 * 1024 * 1024;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum BackingKind {
    Directory,
    File,
    Symlink,
    Other,
}

impl BackingKind {
    pub fn from_metadata(metadata: &Metadata) -> Self {
        if metadata.is_dir() {
            Self::Directory
        } else if metadata.file_type().is_symlink() {
            Self::Symlink
        } else if metadata.is_file() {
            Self::File
        } else {
            Self::Other
        }
    }

    pub fn readonly_mode(self) -> u32 {
        match self {
            Self::Directory => 0o555,
            Self::Symlink => 0o777,
            Self::File | Self::Other => 0o444,
        }
    }

    pub fn nlink(self) -> u32 {
        match self {
            Self::Directory => 2,
            Self::File | Self::Symlink | Self::Other => 1,
        }
    }
}

#[derive(Debug, Clone, Copy)]
pub struct BackingMetadata {
    pub kind: BackingKind,
    pub len: u64,
    pub blocks: u64,
    pub mode: u32,
    pub nlink: u32,
    pub accessed: SystemTime,
    pub modified: SystemTime,
    pub created: SystemTime,
    pub mtime_sec: i64,
}

impl BackingMetadata {
    pub fn from_metadata(metadata: &Metadata) -> Self {
        let now = SystemTime::now();
        let kind = BackingKind::from_metadata(metadata);
        let modified = metadata.modified().unwrap_or(now);
        let mtime_sec = modified.duration_since(UNIX_EPOCH).map_or(0, |duration| {
            i64::try_from(duration.as_secs()).unwrap_or(i64::MAX)
        });
        Self {
            kind,
            len: metadata.len(),
            blocks: metadata.len().div_ceil(512),
            mode: kind.readonly_mode(),
            nlink: kind.nlink(),
            accessed: metadata.accessed().unwrap_or(now),
            modified,
            created: metadata.created().unwrap_or(now),
            mtime_sec,
        }
    }
}
