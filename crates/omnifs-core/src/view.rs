//! Host-owned decoded schema for view-cache records.
//!
//! `omnifs-cache` stores opaque `Record` payload bytes. This module owns the
//! host's decoded view of those payloads: file attributes, lookup payloads,
//! dirents payloads, and file content payloads. Keeping these types here keeps
//! provider/WIT semantics out of the storage crate.

use std::collections::{BTreeMap, HashMap};

pub const MAX_INLINE_PROJECTABLE_BYTES: usize = 64 * 1024;
pub const MAX_EAGER_RESPONSE_BYTES: usize = 512 * 1024;
pub const MAX_VERSION_TOKEN_BYTES: usize = 256;

#[derive(Debug, Clone, Copy, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub enum FileSize {
    Exact(u64),
    NonZero,
    Unknown,
}

#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub enum ByteSource {
    Inline(Vec<u8>),
    Deferred(ReadMode),
    Canonical,
    Blob(u64),
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub enum ReadMode {
    Full,
    Ranged,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub enum Stability {
    Immutable,
    Mutable,
    Volatile,
}

#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
pub struct FileAttrsCache {
    pub size: FileSize,
    pub bytes: ByteSource,
    pub stability: Stability,
    pub version_token: Option<String>,
}

impl FileAttrsCache {
    pub fn st_size(&self) -> u64 {
        match self.size {
            FileSize::Exact(size) => size,
            FileSize::NonZero | FileSize::Unknown => 1,
        }
    }

    pub fn should_direct_io(&self) -> bool {
        !matches!(self.size, FileSize::Exact(_)) || !matches!(self.stability, Stability::Immutable)
    }

    pub fn inline_bytes(&self) -> Option<&[u8]> {
        match &self.bytes {
            ByteSource::Inline(bytes) => Some(bytes),
            ByteSource::Canonical | ByteSource::Blob(_) | ByteSource::Deferred(_) => None,
        }
    }

    pub fn cache_key_aux(&self) -> Option<String> {
        if matches!(self.stability, Stability::Mutable) {
            self.version_token
                .as_ref()
                .map(|token| format!("version:{token}"))
        } else {
            None
        }
    }

    pub fn durable_cache_aux(&self) -> Option<Option<String>> {
        match self.stability {
            Stability::Immutable => Some(None),
            Stability::Mutable => self.cache_key_aux().map(Some),
            Stability::Volatile => None,
        }
    }

    pub fn durable_content_cacheable(&self) -> bool {
        match self.stability {
            Stability::Immutable => true,
            Stability::Mutable => self.version_token.is_some(),
            Stability::Volatile => false,
        }
    }

    #[must_use]
    pub fn with_exact_size(mut self, size: u64) -> Self {
        self.size = FileSize::Exact(size);
        self
    }

    pub fn eager_byte_len(&self) -> usize {
        self.inline_bytes().map_or(0, <[u8]>::len)
    }

    pub fn validate(&self) -> Result<(), String> {
        if matches!(self.stability, Stability::Volatile)
            && !matches!(self.bytes, ByteSource::Deferred(ReadMode::Ranged))
        {
            return Err(
                "Stability::Volatile requires ByteSource::Deferred(ReadMode::Ranged)".to_string(),
            );
        }

        if let ByteSource::Inline(bytes) = &self.bytes {
            let FileSize::Exact(size) = self.size else {
                return Err("inline bytes require FileSize::Exact(bytes.len())".to_string());
            };
            let len = u64::try_from(bytes.len())
                .map_err(|_| "inline byte length does not fit u64".to_string())?;
            if size != len {
                return Err(format!(
                    "inline file declares size {size} but carries {len} bytes"
                ));
            }
            if bytes.len() > MAX_INLINE_PROJECTABLE_BYTES {
                return Err(format!(
                    "inline projection exceeds eager byte limit of {MAX_INLINE_PROJECTABLE_BYTES} bytes"
                ));
            }
        }

        if let Some(token) = &self.version_token {
            if token.is_empty() {
                return Err("version token must not be empty".to_string());
            }
            if token.len() > MAX_VERSION_TOKEN_BYTES {
                return Err(format!(
                    "version token exceeds {MAX_VERSION_TOKEN_BYTES} bytes"
                ));
            }
        }

        Ok(())
    }

    pub fn validate_observed_size(&self, observed_size: u64) -> Result<(), String> {
        match self.size {
            FileSize::Exact(size) => {
                if size == observed_size {
                    Ok(())
                } else {
                    Err(format!(
                        "declares exact size {size} but observed {observed_size} bytes"
                    ))
                }
            },
            FileSize::NonZero if observed_size == 0 => {
                Err("declares FileSize::NonZero but observed zero bytes".to_string())
            },
            FileSize::NonZero | FileSize::Unknown => Ok(()),
        }
    }

    pub fn validate_complete_content(&self, content_len: usize) -> Result<(), String> {
        let observed_size = u64::try_from(content_len)
            .map_err(|_| "content length does not fit u64".to_string())?;
        self.validate_observed_size(observed_size)
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub enum EntryKind {
    Directory,
    File,
}

#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
pub struct EntryMeta {
    pub kind: EntryKind,
    pub attrs: Option<FileAttrsCache>,
}

impl EntryMeta {
    pub fn directory() -> Self {
        Self {
            kind: EntryKind::Directory,
            attrs: None,
        }
    }

    pub fn file(attrs: FileAttrsCache) -> Self {
        Self {
            kind: EntryKind::File,
            attrs: Some(attrs),
        }
    }

    pub fn is_directory(&self) -> bool {
        matches!(self.kind, EntryKind::Directory)
    }

    pub fn is_file(&self) -> bool {
        matches!(self.kind, EntryKind::File)
    }

    pub fn st_size(&self) -> u64 {
        self.attrs.as_ref().map_or(0, FileAttrsCache::st_size)
    }
}

#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
pub enum LookupPayload {
    Positive(EntryMeta),
    Negative,
}

impl LookupPayload {
    pub fn serialize(&self) -> Option<Vec<u8>> {
        postcard::to_allocvec(self).ok()
    }

    pub fn deserialize(bytes: &[u8]) -> Option<Self> {
        postcard::from_bytes(bytes).ok()
    }
}

#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
pub struct AttrPayload {
    pub meta: EntryMeta,
}

impl AttrPayload {
    pub fn serialize(&self) -> Option<Vec<u8>> {
        postcard::to_allocvec(self).ok()
    }

    pub fn deserialize(bytes: &[u8]) -> Option<Self> {
        postcard::from_bytes(bytes).ok()
    }
}

#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
pub struct DirentRecord {
    pub name: String,
    pub meta: EntryMeta,
}

#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub enum CachedCursor {
    Opaque(String),
    Page(u32),
}

#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
pub struct DirentsPayload {
    pub entries: Vec<DirentRecord>,
    pub exhaustive: bool,
    #[serde(default)]
    pub validator: Option<String>,
    #[serde(default)]
    pub next_cursor: Option<CachedCursor>,
    #[serde(default)]
    pub paginated: bool,
}

impl DirentsPayload {
    pub fn serialize(&self) -> Option<Vec<u8>> {
        postcard::to_allocvec(self).ok()
    }

    pub fn deserialize(bytes: &[u8]) -> Option<Self> {
        postcard::from_bytes(bytes).ok()
    }

    #[must_use]
    pub fn is_authoritative_listing(&self) -> bool {
        self.exhaustive || self.paginated || self.next_cursor.is_some()
    }

    pub fn merged(
        existing_record: Option<Self>,
        new_children: BTreeMap<String, DirentRecord>,
        listing_exhaustive: bool,
    ) -> Self {
        if listing_exhaustive {
            return Self {
                entries: new_children.into_values().collect(),
                exhaustive: true,
                validator: None,
                next_cursor: None,
                paginated: false,
            };
        }

        let (previously_exhaustive, validator, next_cursor, paginated, mut existing) =
            existing_record.map_or_else(
                || (false, None, None, false, HashMap::new()),
                |payload| {
                    (
                        payload.exhaustive,
                        payload.validator,
                        payload.next_cursor,
                        payload.paginated,
                        payload
                            .entries
                            .into_iter()
                            .map(|entry| (entry.name.clone(), entry))
                            .collect(),
                    )
                },
            );
        let introduced = new_children.keys().any(|name| !existing.contains_key(name));
        existing.extend(new_children);
        Self {
            entries: existing.into_values().collect(),
            exhaustive: previously_exhaustive && !introduced,
            validator,
            next_cursor,
            paginated,
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub struct FilePayload {
    pub version_token: Option<String>,
    pub content: Vec<u8>,
    #[serde(default)]
    pub content_type: Option<String>,
}

impl FilePayload {
    pub fn new(version_token: Option<String>, content: Vec<u8>) -> Self {
        Self {
            version_token,
            content,
            content_type: None,
        }
    }

    #[must_use]
    pub fn with_content_type(mut self, content_type: Option<String>) -> Self {
        self.content_type = content_type;
        self
    }

    pub fn serialize(&self) -> Option<Vec<u8>> {
        postcard::to_allocvec(self).ok()
    }

    pub fn deserialize(bytes: &[u8]) -> Option<Self> {
        postcard::from_bytes(bytes).ok()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn inline_file_payload_round_trips() {
        let meta = EntryMeta::file(FileAttrsCache {
            size: FileSize::Exact(4),
            bytes: ByteSource::Inline(vec![0xde, 0xad, 0xbe, 0xef]),
            stability: Stability::Immutable,
            version_token: Some("v1".to_string()),
        });

        let lookup_bytes = LookupPayload::Positive(meta.clone()).serialize().unwrap();
        let Some(LookupPayload::Positive(decoded)) = LookupPayload::deserialize(&lookup_bytes)
        else {
            panic!("expected positive lookup payload");
        };
        assert!(decoded.is_file());
        let attrs = decoded.attrs.expect("file should carry attrs");
        assert_eq!(attrs.size, FileSize::Exact(4));
        assert_eq!(attrs.stability, Stability::Immutable);
        assert_eq!(attrs.version_token.as_deref(), Some("v1"));
        assert_eq!(attrs.inline_bytes(), Some(&[0xde, 0xad, 0xbe, 0xef][..]));

        let attr_bytes = AttrPayload { meta: meta.clone() }.serialize().unwrap();
        let decoded = AttrPayload::deserialize(&attr_bytes).unwrap();
        assert!(decoded.meta.is_file());
        assert_eq!(decoded.meta.st_size(), 4);

        let dirents_bytes = DirentsPayload {
            entries: vec![DirentRecord {
                name: "blob".to_string(),
                meta,
            }],
            exhaustive: true,
            validator: None,
            next_cursor: None,
            paginated: false,
        }
        .serialize()
        .unwrap();
        let decoded = DirentsPayload::deserialize(&dirents_bytes).unwrap();
        assert_eq!(decoded.entries.len(), 1);
        assert!(decoded.entries[0].meta.is_file());
    }

    #[test]
    fn ranged_volatile_payload_round_trips() {
        let meta = EntryMeta::file(FileAttrsCache {
            size: FileSize::Unknown,
            bytes: ByteSource::Deferred(ReadMode::Ranged),
            stability: Stability::Volatile,
            version_token: None,
        });
        let bytes = AttrPayload { meta }.serialize().unwrap();
        let decoded = AttrPayload::deserialize(&bytes).unwrap();
        let attrs = decoded.meta.attrs.expect("file should carry attrs");
        assert_eq!(attrs.stability, Stability::Volatile);
        assert_eq!(attrs.bytes, ByteSource::Deferred(ReadMode::Ranged));
    }
}
