//! Host-owned decoded schema for view-cache records.
//!
//! engine cache stores opaque `Record` payload bytes. This module owns the
//! host's decoded view of those payloads: file attributes, lookup payloads,
//! dirents payloads, and file content payloads. Keeping these types here keeps
//! provider/WIT semantics out of the storage crate.

use std::collections::BTreeMap;

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
    Stable,
    Dynamic,
    Live,
}

/// Version evidence validated at the untrusted provider boundary.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct VersionToken(String);

impl VersionToken {
    pub fn new(value: String) -> Result<Self, String> {
        let token = Self(value);
        token.validate()?;
        Ok(token)
    }

    pub fn as_str(&self) -> &str {
        &self.0
    }

    pub fn validate(&self) -> Result<(), String> {
        if self.0.is_empty() {
            return Err("version token must not be empty".to_string());
        }
        if self.0.len() > MAX_VERSION_TOKEN_BYTES {
            return Err(format!(
                "version token exceeds {MAX_VERSION_TOKEN_BYTES} bytes"
            ));
        }
        Ok(())
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum FileAttrsCache {
    Inline {
        bytes: Vec<u8>,
        freshness: Freshness,
    },
    DeferredFull {
        size: FileSize,
        freshness: Freshness,
    },
    DeferredRanged {
        size: FileSize,
        freshness: RangedFreshness,
    },
    Canonical {
        size: FileSize,
        freshness: Freshness,
    },
    Blob {
        blob: u64,
        size: FileSize,
        freshness: Freshness,
    },
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Freshness {
    Stable { version_token: Option<VersionToken> },
    Dynamic { version_token: Option<VersionToken> },
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum RangedFreshness {
    Stable { version_token: Option<VersionToken> },
    Dynamic { version_token: Option<VersionToken> },
    Live { version_token: Option<VersionToken> },
}

#[derive(Debug, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
struct FileAttrsCacheWire {
    size: FileSize,
    bytes: ByteSource,
    stability: Stability,
    #[serde(default)]
    version_token: Option<String>,
}

impl From<&FileAttrsCache> for FileAttrsCacheWire {
    fn from(attrs: &FileAttrsCache) -> Self {
        Self {
            size: attrs.size(),
            bytes: attrs.byte_source(),
            stability: attrs.stability(),
            version_token: attrs.version_token_owned(),
        }
    }
}

impl serde::Serialize for FileAttrsCache {
    fn serialize<S>(&self, serializer: S) -> Result<S::Ok, S::Error>
    where
        S: serde::Serializer,
    {
        FileAttrsCacheWire::from(self).serialize(serializer)
    }
}

impl<'de> serde::Deserialize<'de> for FileAttrsCache {
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: serde::Deserializer<'de>,
    {
        let wire = FileAttrsCacheWire::deserialize(deserializer)?;
        Self::from_parts(wire.size, wire.bytes, wire.stability, wire.version_token)
            .map_err(serde::de::Error::custom)
    }
}

impl FileAttrsCache {
    pub fn inline(
        bytes: Vec<u8>,
        stability: Stability,
        version_token: Option<String>,
    ) -> Result<Self, String> {
        if bytes.len() > MAX_INLINE_PROJECTABLE_BYTES {
            return Err(format!(
                "inline projection exceeds eager byte limit of {MAX_INLINE_PROJECTABLE_BYTES} bytes"
            ));
        }
        Ok(Self::Inline {
            bytes,
            freshness: Freshness::new(stability, version_token)?,
        })
    }

    pub fn deferred(
        size: FileSize,
        mode: ReadMode,
        stability: Stability,
        version_token: Option<String>,
    ) -> Result<Self, String> {
        match mode {
            ReadMode::Full => Ok(Self::DeferredFull {
                size,
                freshness: Freshness::new(stability, version_token)?,
            }),
            ReadMode::Ranged => Ok(Self::DeferredRanged {
                size,
                freshness: RangedFreshness::new(stability, version_token)?,
            }),
        }
    }

    pub fn canonical(
        size: FileSize,
        stability: Stability,
        version_token: Option<String>,
    ) -> Result<Self, String> {
        Ok(Self::Canonical {
            size,
            freshness: Freshness::new(stability, version_token)?,
        })
    }

    pub fn blob(
        blob: u64,
        size: FileSize,
        stability: Stability,
        version_token: Option<String>,
    ) -> Result<Self, String> {
        Ok(Self::Blob {
            blob,
            size,
            freshness: Freshness::new(stability, version_token)?,
        })
    }

    pub fn from_parts(
        size: FileSize,
        bytes: ByteSource,
        stability: Stability,
        version_token: Option<String>,
    ) -> Result<Self, String> {
        match bytes {
            ByteSource::Inline(bytes) => {
                let len = u64::try_from(bytes.len())
                    .map_err(|_| "inline byte length does not fit u64".to_string())?;
                match size {
                    FileSize::Exact(size) if size == len => {},
                    FileSize::Exact(size) => {
                        return Err(format!(
                            "inline file declares size {size} but carries {len} bytes"
                        ));
                    },
                    FileSize::NonZero | FileSize::Unknown => {
                        return Err("inline bytes require FileSize::Exact".to_string());
                    },
                }
                Self::inline(bytes, stability, version_token)
            },
            ByteSource::Deferred(mode) => Self::deferred(size, mode, stability, version_token),
            ByteSource::Canonical => Self::canonical(size, stability, version_token),
            ByteSource::Blob(blob) => Self::blob(blob, size, stability, version_token),
        }
    }

    pub fn size(&self) -> FileSize {
        match self {
            Self::Inline { bytes, .. } => {
                FileSize::Exact(u64::try_from(bytes.len()).unwrap_or(u64::MAX))
            },
            Self::DeferredFull { size, .. }
            | Self::DeferredRanged { size, .. }
            | Self::Canonical { size, .. }
            | Self::Blob { size, .. } => *size,
        }
    }

    pub fn byte_source(&self) -> ByteSource {
        match self {
            Self::Inline { bytes, .. } => ByteSource::Inline(bytes.clone()),
            Self::DeferredFull { .. } => ByteSource::Deferred(ReadMode::Full),
            Self::DeferredRanged { .. } => ByteSource::Deferred(ReadMode::Ranged),
            Self::Canonical { .. } => ByteSource::Canonical,
            Self::Blob { blob, .. } => ByteSource::Blob(*blob),
        }
    }

    pub fn stability(&self) -> Stability {
        match self {
            Self::Inline { freshness, .. }
            | Self::DeferredFull { freshness, .. }
            | Self::Canonical { freshness, .. }
            | Self::Blob { freshness, .. } => freshness.stability(),
            Self::DeferredRanged { freshness, .. } => freshness.stability(),
        }
    }

    pub fn version_token(&self) -> Option<&str> {
        match self {
            Self::Inline { freshness, .. }
            | Self::DeferredFull { freshness, .. }
            | Self::Canonical { freshness, .. }
            | Self::Blob { freshness, .. } => freshness.version_token(),
            Self::DeferredRanged { freshness, .. } => freshness.version_token(),
        }
    }

    pub fn version_token_owned(&self) -> Option<String> {
        self.version_token().map(ToOwned::to_owned)
    }

    pub fn st_size(&self) -> u64 {
        match self.size() {
            FileSize::Exact(size) => size,
            FileSize::NonZero | FileSize::Unknown => 1,
        }
    }

    pub fn should_direct_io(&self) -> bool {
        !matches!(self.size(), FileSize::Exact(_)) || !matches!(self.stability(), Stability::Stable)
    }

    pub fn has_exact_size(&self) -> bool {
        matches!(self.size(), FileSize::Exact(_))
    }

    pub fn is_deferred_full(&self) -> bool {
        matches!(self, Self::DeferredFull { .. })
    }

    pub fn is_deferred_ranged(&self) -> bool {
        matches!(self, Self::DeferredRanged { .. })
    }

    pub fn is_deferred(&self) -> bool {
        matches!(
            self,
            Self::DeferredFull { .. } | Self::DeferredRanged { .. }
        )
    }

    pub fn inline_bytes(&self) -> Option<&[u8]> {
        match self {
            Self::Inline { bytes, .. } => Some(bytes),
            Self::DeferredFull { .. }
            | Self::DeferredRanged { .. }
            | Self::Canonical { .. }
            | Self::Blob { .. } => None,
        }
    }

    pub fn cache_key_aux(&self) -> Option<String> {
        if matches!(self.stability(), Stability::Dynamic) {
            self.version_token().map(|token| format!("version:{token}"))
        } else {
            None
        }
    }

    pub fn durable_cache_aux(&self) -> Option<Option<String>> {
        match self.stability() {
            Stability::Stable => Some(None),
            Stability::Dynamic => self.cache_key_aux().map(Some),
            Stability::Live => None,
        }
    }

    pub fn durable_content_cacheable(&self) -> bool {
        match self.stability() {
            Stability::Stable => true,
            Stability::Dynamic => self.version_token().is_some(),
            Stability::Live => false,
        }
    }

    /// Whether a size learned from a complete read on `self` survives being
    /// refreshed by `incoming`, so a kind-derived listing placeholder cannot
    /// erase the real size after a `cat`.
    ///
    /// True only when `self` carries a learned `Exact` size, `incoming` is
    /// silent about size (no `Exact` of its own), the file is not `Live`, and
    /// `incoming` does not prove the content changed (see the version rule
    /// below). Stability is otherwise ignored, because directory listings
    /// project a kind-derived placeholder stability rather than the file's real
    /// one; only `Live` is rejected outright (a live file is never durably
    /// size-learned, so the `Exact` guard already excludes it, but the explicit
    /// check states the intent).
    ///
    /// Byte source is deliberately NOT compared: it is a promise-vs-fulfillment
    /// field, not a content identity. A listing dirent declares the promise
    /// (`Deferred`, or `Canonical` for an object representation) while a read
    /// fulfills it (`Inline`/`Blob`/`Canonical`), so the two legitimately
    /// differ for the same file.
    ///
    /// Version rule: a learned size is dropped only when `incoming` carries a
    /// DIFFERENT explicit version token, which is the sole proof that the
    /// content changed. An `incoming` with no version token does not prove a
    /// change: object representation leaves are listed through a static
    /// placeholder (`FileProj::listing_shape`, version-less and `Stable`) that
    /// knows nothing about the loaded object's real `Dynamic` version, so
    /// requiring token equality clobbered the learned size of every
    /// representation on the next lookup.
    ///
    /// Keeping `self`'s attributes is safe even for dynamic files: the next
    /// complete read re-learns the size from the bytes it returns (and a
    /// changed upstream yields a new version that does drop it), so a stale
    /// value never reaches a read check.
    pub fn keeps_learned_size_over(&self, incoming: &FileAttrsCache) -> bool {
        let version_proves_change =
            incoming.version_token().is_some() && incoming.version_token() != self.version_token();
        matches!(self.size(), FileSize::Exact(_))
            && !matches!(incoming.size(), FileSize::Exact(_))
            && !matches!(self.stability(), Stability::Live)
            && !version_proves_change
    }

    pub fn merge_preserving_learned_size(
        existing: Option<&FileAttrsCache>,
        incoming: Option<FileAttrsCache>,
    ) -> Option<FileAttrsCache> {
        match (existing, incoming) {
            (Some(existing), Some(incoming)) if existing.keeps_learned_size_over(&incoming) => {
                Some(existing.clone())
            },
            (_, incoming) => incoming,
        }
    }

    pub fn learned_complete_content_attrs(
        &self,
        content_len: usize,
    ) -> Result<FileAttrsCache, String> {
        let observed_size = u64::try_from(content_len)
            .map_err(|_| "content length does not fit u64".to_string())?;
        self.validate_observed_size(observed_size)?;
        let mut attrs = self.clone();
        if attrs.can_publish_learned_size() && !matches!(attrs.size(), FileSize::Exact(_)) {
            attrs = attrs.with_exact_size(observed_size);
        }
        Ok(attrs)
    }

    pub fn learned_ranged_eof_attrs(
        &self,
        eof_size: u64,
    ) -> Result<Option<FileAttrsCache>, String> {
        self.validate_observed_size(eof_size)?;
        if !self.can_publish_learned_size() || matches!(self.size(), FileSize::Exact(_)) {
            return Ok(None);
        }
        Ok(Some(self.clone().with_exact_size(eof_size)))
    }

    pub fn can_publish_learned_size(&self) -> bool {
        match self.stability() {
            Stability::Stable | Stability::Dynamic => true,
            Stability::Live => false,
        }
    }

    #[must_use]
    pub fn with_exact_size(self, size: u64) -> Self {
        match self {
            Self::Inline { bytes, freshness } => Self::Inline { bytes, freshness },
            Self::DeferredFull { freshness, .. } => Self::DeferredFull {
                size: FileSize::Exact(size),
                freshness,
            },
            Self::DeferredRanged { freshness, .. } => Self::DeferredRanged {
                size: FileSize::Exact(size),
                freshness,
            },
            Self::Canonical { freshness, .. } => Self::Canonical {
                size: FileSize::Exact(size),
                freshness,
            },
            Self::Blob {
                blob, freshness, ..
            } => Self::Blob {
                blob,
                size: FileSize::Exact(size),
                freshness,
            },
        }
    }

    pub fn eager_byte_len(&self) -> usize {
        self.inline_bytes().map_or(0, <[u8]>::len)
    }

    pub fn validate(&self) -> Result<(), String> {
        if let Self::Inline { bytes, .. } = self
            && bytes.len() > MAX_INLINE_PROJECTABLE_BYTES
        {
            return Err(format!(
                "inline projection exceeds eager byte limit of {MAX_INLINE_PROJECTABLE_BYTES} bytes"
            ));
        }

        match self {
            Self::Inline { freshness, .. }
            | Self::DeferredFull { freshness, .. }
            | Self::Canonical { freshness, .. }
            | Self::Blob { freshness, .. } => freshness.validate()?,
            Self::DeferredRanged { freshness, .. } => freshness.validate()?,
        }

        Ok(())
    }

    pub fn validate_observed_size(&self, observed_size: u64) -> Result<(), String> {
        match self.size() {
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

impl Freshness {
    fn new(stability: Stability, version_token: Option<String>) -> Result<Self, String> {
        let version_token = version_token.map(VersionToken::new).transpose()?;
        match stability {
            Stability::Stable => Ok(Self::Stable { version_token }),
            Stability::Dynamic => Ok(Self::Dynamic { version_token }),
            Stability::Live => {
                Err("Stability::Live requires ByteSource::Deferred(ReadMode::Ranged)".to_string())
            },
        }
    }

    fn stability(&self) -> Stability {
        match self {
            Self::Stable { .. } => Stability::Stable,
            Self::Dynamic { .. } => Stability::Dynamic,
        }
    }

    fn version_token(&self) -> Option<&str> {
        match self {
            Self::Stable { version_token } | Self::Dynamic { version_token } => {
                version_token.as_ref().map(VersionToken::as_str)
            },
        }
    }

    fn validate(&self) -> Result<(), String> {
        match self {
            Self::Stable { version_token } | Self::Dynamic { version_token } => version_token
                .as_ref()
                .map_or(Ok(()), VersionToken::validate),
        }
    }
}

impl RangedFreshness {
    fn new(stability: Stability, version_token: Option<String>) -> Result<Self, String> {
        let version_token = version_token.map(VersionToken::new).transpose()?;
        match stability {
            Stability::Stable => Ok(Self::Stable { version_token }),
            Stability::Dynamic => Ok(Self::Dynamic { version_token }),
            Stability::Live => Ok(Self::Live { version_token }),
        }
    }

    fn stability(&self) -> Stability {
        match self {
            Self::Stable { .. } => Stability::Stable,
            Self::Dynamic { .. } => Stability::Dynamic,
            Self::Live { .. } => Stability::Live,
        }
    }

    fn version_token(&self) -> Option<&str> {
        match self {
            Self::Stable { version_token }
            | Self::Dynamic { version_token }
            | Self::Live { version_token } => version_token.as_ref().map(VersionToken::as_str),
        }
    }

    fn validate(&self) -> Result<(), String> {
        match self {
            Self::Stable { version_token }
            | Self::Dynamic { version_token }
            | Self::Live { version_token } => version_token
                .as_ref()
                .map_or(Ok(()), VersionToken::validate),
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub enum EntryKind {
    Directory,
    File,
}

#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
pub enum EntryMeta {
    Directory,
    File { attrs: Option<FileAttrsCache> },
}

impl EntryMeta {
    pub fn directory() -> Self {
        Self::Directory
    }

    pub fn file(attrs: FileAttrsCache) -> Self {
        Self::File { attrs: Some(attrs) }
    }

    pub fn file_without_attrs() -> Self {
        Self::File { attrs: None }
    }

    pub fn kind(&self) -> EntryKind {
        match self {
            Self::Directory => EntryKind::Directory,
            Self::File { .. } => EntryKind::File,
        }
    }

    pub fn attrs(&self) -> Option<&FileAttrsCache> {
        match self {
            Self::Directory => None,
            Self::File { attrs } => attrs.as_ref(),
        }
    }

    pub fn into_attrs(self) -> Option<FileAttrsCache> {
        match self {
            Self::Directory => None,
            Self::File { attrs } => attrs,
        }
    }

    pub fn is_directory(&self) -> bool {
        matches!(self, Self::Directory)
    }

    pub fn is_file(&self) -> bool {
        matches!(self, Self::File { .. })
    }

    pub fn st_size(&self) -> u64 {
        self.attrs().map_or(0, FileAttrsCache::st_size)
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
        mut new_children: BTreeMap<String, DirentRecord>,
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

        let (previously_exhaustive, validator, next_cursor, paginated, mut entries) =
            existing_record.map_or_else(
                || (false, None, None, false, Vec::new()),
                |payload| {
                    (
                        payload.exhaustive,
                        payload.validator,
                        payload.next_cursor,
                        payload.paginated,
                        payload.entries,
                    )
                },
            );

        // Replace matched entries in place; entries absent from new_children
        // are untouched. No name cloning: BTreeMap::remove gives ownership of
        // both key and value.
        for entry in &mut entries {
            if let Some(updated) = new_children.remove(&entry.name) {
                *entry = updated;
            }
        }

        // Any names still left in new_children are introductions. Append them
        // in BTreeMap order (deterministic, sorted by name).
        let introduced = !new_children.is_empty();
        entries.extend(new_children.into_values());

        Self {
            entries,
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
    fn cache_payload_postcard_roundtrip() {
        let attrs = FileAttrsCache::inline(
            vec![0xde, 0xad, 0xbe, 0xef],
            Stability::Stable,
            Some("v1".to_string()),
        )
        .unwrap();
        let attr_wire_bytes = postcard::to_allocvec(&attrs).unwrap();
        let wire: FileAttrsCacheWire = postcard::from_bytes(&attr_wire_bytes).unwrap();
        assert_eq!(
            wire,
            FileAttrsCacheWire {
                size: FileSize::Exact(4),
                bytes: ByteSource::Inline(vec![0xde, 0xad, 0xbe, 0xef]),
                stability: Stability::Stable,
                version_token: Some("v1".to_string()),
            }
        );
        let decoded_attrs: FileAttrsCache = postcard::from_bytes(&attr_wire_bytes).unwrap();
        assert_eq!(decoded_attrs, attrs);

        let inline = EntryMeta::file(attrs);
        let lookup_bytes = LookupPayload::Positive(inline.clone()).serialize().unwrap();
        let Some(LookupPayload::Positive(decoded)) = LookupPayload::deserialize(&lookup_bytes)
        else {
            panic!("expected positive lookup payload");
        };
        assert!(decoded.is_file());
        let attrs = decoded.into_attrs().expect("file should carry attrs");
        assert_eq!(attrs.size(), FileSize::Exact(4));
        assert_eq!(attrs.stability(), Stability::Stable);
        assert_eq!(attrs.version_token(), Some("v1"));
        assert_eq!(attrs.inline_bytes(), Some(&[0xde, 0xad, 0xbe, 0xef][..]));

        let attr_bytes = AttrPayload { meta: inline }.serialize().unwrap();
        let decoded = AttrPayload::deserialize(&attr_bytes).unwrap();
        assert!(decoded.meta.is_file());
        assert_eq!(decoded.meta.st_size(), 4);

        let ranged = EntryMeta::file(
            FileAttrsCache::deferred(FileSize::Unknown, ReadMode::Ranged, Stability::Live, None)
                .unwrap(),
        );
        let bytes = AttrPayload { meta: ranged }.serialize().unwrap();
        let decoded = AttrPayload::deserialize(&bytes).unwrap();
        let attrs = decoded.meta.into_attrs().expect("file should carry attrs");
        assert_eq!(attrs.stability(), Stability::Live);
        assert_eq!(attrs.byte_source(), ByteSource::Deferred(ReadMode::Ranged));
    }

    // Regression: an object representation (`repo.json`) is read-answered as a
    // learned `Exact`, `Dynamic`, etag-versioned attr, but every later listing
    // re-applies the static `FileProj::listing_shape` placeholder: `Unknown`
    // size, `Stable`, NO version, `Deferred(Full)`. The learned exact size must
    // survive that placeholder; otherwise `stat` reports the `1` sentinel even
    // after a `cat`. Only a DIFFERENT explicit version drops it.
    #[test]
    fn learned_size_survives_versionless_listing_placeholder() {
        let learned = FileAttrsCache::deferred(
            FileSize::Exact(6815),
            ReadMode::Full,
            Stability::Dynamic,
            Some("etag-1".to_string()),
        )
        .unwrap();
        // The real listing placeholder: version-less, Stable, Unknown size.
        let placeholder =
            FileAttrsCache::deferred(FileSize::Unknown, ReadMode::Full, Stability::Stable, None)
                .unwrap();
        assert!(learned.keeps_learned_size_over(&placeholder));

        // A DIFFERENT explicit version proves new content and drops the size.
        let newer = FileAttrsCache::deferred(
            FileSize::Unknown,
            ReadMode::Full,
            Stability::Dynamic,
            Some("etag-2".to_string()),
        )
        .unwrap();
        assert!(!learned.keeps_learned_size_over(&newer));
    }

    // --- DirentsPayload::merged ---

    fn dir_record(name: &str) -> DirentRecord {
        DirentRecord {
            name: name.to_string(),
            meta: EntryMeta::directory(),
        }
    }

    fn new_children(names: &[&str]) -> BTreeMap<String, DirentRecord> {
        names
            .iter()
            .map(|&n| (n.to_string(), dir_record(n)))
            .collect()
    }

    fn names(payload: &DirentsPayload) -> Vec<&str> {
        payload.entries.iter().map(|e| e.name.as_str()).collect()
    }

    // Exhaustive short-circuit: replaces everything, marks exhaustive, clears
    // pagination state.
    #[test]
    fn merged_exhaustive_replaces_all() {
        let existing = DirentsPayload {
            entries: vec![dir_record("old-a"), dir_record("old-b")],
            exhaustive: false,
            validator: Some("v0".to_string()),
            next_cursor: Some(CachedCursor::Page(1)),
            paginated: true,
        };
        let result = DirentsPayload::merged(Some(existing), new_children(&["new-x"]), true);
        assert!(result.exhaustive);
        assert_eq!(names(&result), vec!["new-x"]);
        assert!(result.validator.is_none());
        assert!(result.next_cursor.is_none());
        assert!(!result.paginated);
    }

    // Replacement keeps position: an entry already in the vec is updated in
    // place, not appended. Entry count does not grow.
    #[test]
    fn merged_replacement_keeps_position_and_count() {
        let existing = DirentsPayload {
            entries: vec![dir_record("alpha"), dir_record("beta"), dir_record("gamma")],
            exhaustive: false,
            validator: None,
            next_cursor: None,
            paginated: false,
        };
        let updated = new_children(&["beta"]);
        let result = DirentsPayload::merged(Some(existing), updated, false);
        // Count unchanged: beta was replaced, not appended.
        assert_eq!(result.entries.len(), 3);
        // Order unchanged: alpha, beta, gamma.
        assert_eq!(names(&result), vec!["alpha", "beta", "gamma"]);
        // No introduction: exhaustive flag is preserved as-is (false && !false = false).
        assert!(!result.exhaustive);
    }

    // Introduction appends in name order and demotes exhaustive.
    #[test]
    fn merged_introduction_appends_and_demotes_exhaustive() {
        let existing = DirentsPayload {
            entries: vec![dir_record("alpha"), dir_record("beta")],
            exhaustive: true,
            validator: Some("v1".to_string()),
            next_cursor: None,
            paginated: false,
        };
        // Introduce two new names; BTreeMap order is "delta" then "gamma".
        let result =
            DirentsPayload::merged(Some(existing), new_children(&["gamma", "delta"]), false);
        // No longer exhaustive because new names appeared.
        assert!(!result.exhaustive);
        // Existing entries first, introductions appended in name (BTreeMap) order.
        assert_eq!(names(&result), vec!["alpha", "beta", "delta", "gamma"]);
        // Validator and pagination state are carried through.
        assert_eq!(result.validator.as_deref(), Some("v1"));
    }

    // No-overlap merge (empty existing): all children become introductions,
    // appended in name order.
    #[test]
    fn merged_no_existing_record_appends_all_in_name_order() {
        let result = DirentsPayload::merged(None, new_children(&["zed", "alpha", "mid"]), false);
        // BTreeMap insertion order is alphabetical.
        assert_eq!(names(&result), vec!["alpha", "mid", "zed"]);
        assert!(!result.exhaustive);
    }

    // Mix: some names replaced in place, others introduced and appended.
    #[test]
    fn merged_mixed_replace_and_introduce() {
        let existing = DirentsPayload {
            entries: vec![dir_record("a"), dir_record("b"), dir_record("c")],
            exhaustive: false,
            validator: None,
            next_cursor: None,
            paginated: false,
        };
        // "b" is a replacement; "d" and "e" are introductions.
        let result = DirentsPayload::merged(Some(existing), new_children(&["b", "e", "d"]), false);
        assert_eq!(result.entries.len(), 5);
        // Existing order preserved for a, b, c; introductions d, e appended in name order.
        assert_eq!(names(&result), vec!["a", "b", "c", "d", "e"]);
        // Introduction occurred: exhaustive demoted.
        assert!(!result.exhaustive);
    }
}
