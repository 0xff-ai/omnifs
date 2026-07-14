use serde::{Deserialize, Serialize};
use strum::Display;

/// Host browse-cache observability kind. Does not expose the internal
/// in-memory `mem` vs durable `disk` tiers; subscribers see one logical
/// browse cache. `Display` matches the serde wire form.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, Display)]
#[serde(rename_all = "snake_case")]
#[strum(serialize_all = "snake_case")]
pub enum CacheKind {
    BrowseHit,
    BrowseMiss,
    FileHit,
    FileMiss,
    BlobHit,
    BlobMiss,
    PreloadStored,
    Invalidated,
}

impl CacheKind {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::BrowseHit => "browse_hit",
            Self::BrowseMiss => "browse_miss",
            Self::FileHit => "file_hit",
            Self::FileMiss => "file_miss",
            Self::BlobHit => "blob_hit",
            Self::BlobMiss => "blob_miss",
            Self::PreloadStored => "preload_stored",
            Self::Invalidated => "invalidated",
        }
    }

    pub fn from_field(value: &str) -> Option<Self> {
        Some(match value {
            "browse_hit" => Self::BrowseHit,
            "browse_miss" => Self::BrowseMiss,
            "file_hit" => Self::FileHit,
            "file_miss" => Self::FileMiss,
            "blob_hit" => Self::BlobHit,
            "blob_miss" => Self::BlobMiss,
            "preload_stored" => Self::PreloadStored,
            "invalidated" => Self::Invalidated,
            _ => return None,
        })
    }
}

/// WIT callout variant names on the wire. `Display` matches the serde
/// wire form.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, Display)]
#[serde(rename_all = "snake_case")]
#[strum(serialize_all = "snake_case")]
pub enum CalloutKind {
    Fetch,
    FetchBlob,
    GitOpenRepo,
    ReadBlob,
}

impl CalloutKind {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Fetch => "fetch",
            Self::FetchBlob => "fetch_blob",
            Self::GitOpenRepo => "git_open_repo",
            Self::ReadBlob => "read_blob",
        }
    }

    pub fn from_field(value: &str) -> Option<Self> {
        Some(match value {
            "fetch" => Self::Fetch,
            "fetch_blob" => Self::FetchBlob,
            "git_open_repo" => Self::GitOpenRepo,
            "read_blob" => Self::ReadBlob,
            _ => return None,
        })
    }
}
