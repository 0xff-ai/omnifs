use serde::{Deserialize, Serialize};
use strum::Display;

/// Host browse-cache observability kind (v1 does not expose L0 vs L2).
/// `Display` matches the serde wire form.
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

/// WIT callout variant names on the wire. `Display` matches the serde
/// wire form.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, Display)]
#[serde(rename_all = "snake_case")]
#[strum(serialize_all = "snake_case")]
pub enum CalloutKind {
    Fetch,
    FetchBlob,
    GitOpenRepo,
    OpenArchive,
    ReadBlob,
}
