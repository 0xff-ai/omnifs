//! Host browse cache types and serialization.
//!
//! Defines the shared types used by both L0 (in-memory moka) and
//! L2 (durable redb) cache tiers. Cache entries do not carry TTLs:
//! eviction is driven purely by capacity and explicit invalidation
//! (via `delete_prefix` or provider-driven cache-invalidate effects).

pub const SCHEMA_VERSION: u8 = 2;

/// L0 sizing constants.
pub const L0_MAX_WEIGHT: u64 = 32 * 1024 * 1024; // 32 MiB per provider instance
pub const L0_SKIP_THRESHOLD: usize = 256 * 1024; // 256 KiB

/// L2 table routing threshold.
pub const L2_BULK_THRESHOLD: usize = 64 * 1024; // 64 KiB

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

/// Mirror of WIT `EntryKind` for cache payloads, avoiding a dependency
/// on the generated WIT types in the cache module.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, serde::Serialize, serde::Deserialize)]
#[repr(u8)]
pub enum EntryKindCache {
    Directory = 0,
    File = 1,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CacheRecord {
    pub schema_version: u8,
    pub kind: RecordKind,
    pub payload: Vec<u8>,
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
    Positive { kind: EntryKindCache, size: u64 },
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
    pub kind: EntryKindCache,
    pub size: u64,
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
    pub kind: EntryKindCache,
    pub size: u64,
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
