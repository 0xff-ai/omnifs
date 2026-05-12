//! Host browse cache types and serialization.
//!
//! Defines the shared types used by both L0 (in-memory moka) and
//! L2 (durable redb) cache tiers. Cache entries do not carry TTLs:
//! eviction is driven purely by capacity and explicit invalidation
//! (via `delete_prefix` or provider-driven cache-invalidate effects).

// Bump on-disk records when browse metadata or file payload encoding changes.
// v4 adds FileAttrs payloads and version-token auxiliary file cache keys.
pub const SCHEMA_VERSION: u8 = 4;

pub const MAX_PROJECTED_BYTES: usize = 64 * 1024;
pub const MAX_EAGER_RESPONSE_BYTES: usize = 512 * 1024;
pub const MAX_VERSION_TOKEN_BYTES: usize = 256;

/// L0 sizing constants.
pub const L0_MAX_WEIGHT: u64 = 32 * 1024 * 1024; // 32 MiB per provider instance
pub const L0_SKIP_THRESHOLD: usize = 256 * 1024; // 256 KiB

/// L2 table routing threshold.
pub const L2_BULK_THRESHOLD: usize = 64 * 1024; // 64 KiB

pub mod blobs;
pub mod l0;
pub mod l2;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
#[repr(u8)]
pub enum RecordKind {
    Lookup = 0,
    Attr = 1,
    Dirents = 2,
    File = 3,
}

impl RecordKind {
    pub const ALL: [Self; 4] = [Self::Lookup, Self::Attr, Self::Dirents, Self::File];

    fn from_u8(v: u8) -> Option<Self> {
        match v {
            0 => Some(Self::Lookup),
            1 => Some(Self::Attr),
            2 => Some(Self::Dirents),
            3 => Some(Self::File),
            _ => None,
        }
    }
}

#[derive(Debug, Clone, Eq, Hash, PartialEq)]
pub struct Key {
    pub path: String,
    pub kind: RecordKind,
    pub aux: Option<String>,
}

impl Key {
    pub fn new(path: impl Into<String>, kind: RecordKind) -> Self {
        Self {
            path: path.into(),
            kind,
            aux: None,
        }
    }

    pub fn with_aux(
        path: impl Into<String>,
        kind: RecordKind,
        aux: Option<impl Into<String>>,
    ) -> Self {
        Self {
            path: path.into(),
            kind,
            aux: aux.map(Into::into),
        }
    }
}

/// Mirror of WIT `EntryKind` for cache payloads, avoiding a dependency
/// on the generated WIT types in the cache module.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, serde::Serialize, serde::Deserialize)]
#[repr(u8)]
pub enum EntryKindCache {
    Directory = 0,
    File = 1,
}

#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub struct FileAttrsCache {
    pub size: SizeCache,
    pub bytes: BytesCache,
    pub stability: StabilityCache,
    pub version_token: Option<String>,
}

impl FileAttrsCache {
    pub fn st_size(&self) -> u64 {
        match self.size {
            SizeCache::Exact(size) => size,
            SizeCache::NonZero | SizeCache::Unknown => 1,
        }
    }

    pub fn should_direct_io(&self) -> bool {
        !matches!(self.size, SizeCache::Exact(_))
            || !matches!(self.stability, StabilityCache::Immutable)
    }

    pub fn inline_bytes(&self) -> Option<&[u8]> {
        match &self.bytes {
            BytesCache::Inline(bytes) => Some(bytes),
            BytesCache::Deferred(_) => None,
        }
    }

    pub fn cache_key_aux(&self) -> Option<String> {
        if matches!(self.stability, StabilityCache::Mutable) {
            self.version_token
                .as_ref()
                .map(|token| format!("version:{token}"))
        } else {
            None
        }
    }

    pub fn durable_cache_aux(&self) -> Option<Option<String>> {
        match self.stability {
            StabilityCache::Immutable => Some(None),
            StabilityCache::Mutable => self.cache_key_aux().map(Some),
            StabilityCache::Volatile => None,
        }
    }

    pub fn durable_content_cacheable(&self) -> bool {
        match self.stability {
            StabilityCache::Immutable => true,
            StabilityCache::Mutable => self.version_token.is_some(),
            StabilityCache::Volatile => false,
        }
    }

    #[must_use]
    pub fn with_exact_size(mut self, size: u64) -> Self {
        self.size = SizeCache::Exact(size);
        self
    }

    pub fn eager_byte_len(&self) -> usize {
        self.inline_bytes().map_or(0, <[u8]>::len)
    }

    pub fn validate(&self) -> Result<(), String> {
        if matches!(self.stability, StabilityCache::Volatile)
            && !matches!(&self.bytes, BytesCache::Deferred(ReadModeCache::Ranged))
        {
            return Err(
                "Stability::Volatile requires Bytes::Deferred { read: ReadMode::Ranged }"
                    .to_string(),
            );
        }

        if let BytesCache::Inline(bytes) = &self.bytes {
            let SizeCache::Exact(size) = self.size else {
                return Err("inline bytes require Size::Exact(bytes.len())".to_string());
            };
            let len = u64::try_from(bytes.len())
                .map_err(|_| "inline byte length does not fit u64".to_string())?;
            if size != len {
                return Err(format!(
                    "inline file declares size {size} but carries {len} bytes"
                ));
            }
            if bytes.len() > MAX_PROJECTED_BYTES {
                return Err(format!(
                    "inline projection exceeds eager byte limit of {MAX_PROJECTED_BYTES} bytes"
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
            SizeCache::Exact(size) => {
                if size == observed_size {
                    Ok(())
                } else {
                    Err(format!(
                        "declares exact size {size} but observed {observed_size} bytes"
                    ))
                }
            },
            SizeCache::NonZero if observed_size == 0 => {
                Err("declares Size::NonZero but observed zero bytes".to_string())
            },
            SizeCache::NonZero | SizeCache::Unknown => Ok(()),
        }
    }

    pub fn validate_complete_content(&self, content_len: usize) -> Result<(), String> {
        let observed_size = u64::try_from(content_len)
            .map_err(|_| "content length does not fit u64".to_string())?;
        self.validate_observed_size(observed_size)
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub enum SizeCache {
    Exact(u64),
    NonZero,
    Unknown,
}

#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub enum BytesCache {
    Inline(Vec<u8>),
    Deferred(ReadModeCache),
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub enum ReadModeCache {
    Full,
    Ranged,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub enum StabilityCache {
    Immutable,
    Mutable,
    Volatile,
}

#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub struct EntryMeta {
    pub kind: EntryKindCache,
    pub attrs: Option<FileAttrsCache>,
}

impl EntryMeta {
    pub fn directory() -> Self {
        Self {
            kind: EntryKindCache::Directory,
            attrs: None,
        }
    }

    pub fn file(attrs: FileAttrsCache) -> Self {
        Self {
            kind: EntryKindCache::File,
            attrs: Some(attrs),
        }
    }

    pub fn st_size(&self) -> u64 {
        self.attrs.as_ref().map_or(0, FileAttrsCache::st_size)
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CacheRecord {
    pub schema_version: u8,
    pub kind: RecordKind,
    pub payload: Vec<u8>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct BatchRecord {
    pub path: String,
    pub kind: RecordKind,
    pub aux: Option<String>,
    pub record: CacheRecord,
}

impl BatchRecord {
    pub fn new(
        path: impl Into<String>,
        kind: RecordKind,
        aux: Option<String>,
        record: CacheRecord,
    ) -> Self {
        Self {
            path: path.into(),
            kind,
            aux,
            record,
        }
    }
}

impl CacheRecord {
    pub fn new(kind: RecordKind, payload: Vec<u8>) -> Self {
        Self {
            schema_version: SCHEMA_VERSION,
            kind,
            payload,
        }
    }

    /// Serialize to bytes: `[schema_version:1][kind:1][payload:*]`.
    pub fn serialize(&self) -> Vec<u8> {
        let mut buf = Vec::with_capacity(2 + self.payload.len());
        buf.push(self.schema_version);
        buf.push(self.kind as u8);
        buf.extend_from_slice(&self.payload);
        buf
    }

    pub fn deserialize(bytes: &[u8]) -> Option<Self> {
        if bytes.len() < 2 {
            return None;
        }
        if bytes[0] != SCHEMA_VERSION {
            return None;
        }
        let kind = RecordKind::from_u8(bytes[1])?;
        let payload = bytes[2..].to_vec();
        Some(Self {
            schema_version: SCHEMA_VERSION,
            kind,
            payload,
        })
    }
}

// --- Payload types (serialized via postcard) ---

#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
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

#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
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

#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub struct DirentRecord {
    pub name: String,
    pub meta: EntryMeta,
}

#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub struct DirentsPayload {
    pub entries: Vec<DirentRecord>,
    /// Whether the listing is exhaustive (every child is present).
    /// When true, the host may return ENOENT for absent names
    /// without consulting the provider.
    pub exhaustive: bool,
}

impl DirentsPayload {
    pub fn serialize(&self) -> Option<Vec<u8>> {
        postcard::to_allocvec(self).ok()
    }

    pub fn deserialize(bytes: &[u8]) -> Option<Self> {
        postcard::from_bytes(bytes).ok()
    }
}

#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub struct FilePayload {
    pub version_token: Option<String>,
    pub content: Vec<u8>,
}

impl FilePayload {
    pub fn new(version_token: Option<String>, content: Vec<u8>) -> Self {
        Self {
            version_token,
            content,
        }
    }

    pub fn serialize(&self) -> Option<Vec<u8>> {
        postcard::to_allocvec(self).ok()
    }

    pub fn deserialize(bytes: &[u8]) -> Option<Self> {
        postcard::from_bytes(bytes).ok()
    }
}
