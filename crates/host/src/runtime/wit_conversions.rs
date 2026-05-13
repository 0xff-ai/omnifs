use crate::cache::{self, EntryMeta, SizeCache};
use crate::omnifs::provider::types as wit_types;

impl From<&wit_types::FileProj> for cache::FileAttrsCache {
    fn from(file: &wit_types::FileProj) -> Self {
        Self {
            size: SizeCache::from(&file.attrs.size),
            bytes: cache::BytesCache::from(&file.bytes),
            stability: cache::StabilityCache::from(file.attrs.stability),
            version_token: file.attrs.version_token.clone(),
        }
    }
}

impl From<&wit_types::FileAttrs> for cache::FileAttrsCache {
    fn from(attrs: &wit_types::FileAttrs) -> Self {
        Self {
            size: SizeCache::from(&attrs.size),
            bytes: cache::BytesCache::Deferred(cache::ReadModeCache::Full),
            stability: cache::StabilityCache::from(attrs.stability),
            version_token: attrs.version_token.clone(),
        }
    }
}

impl From<&wit_types::FileSize> for SizeCache {
    fn from(size: &wit_types::FileSize) -> Self {
        match size {
            wit_types::FileSize::Exact(size) => Self::Exact(*size),
            wit_types::FileSize::NonZero => Self::NonZero,
            wit_types::FileSize::Unknown => Self::Unknown,
        }
    }
}

impl From<&wit_types::ProjBytes> for cache::BytesCache {
    fn from(bytes: &wit_types::ProjBytes) -> Self {
        match bytes {
            wit_types::ProjBytes::Inline(bytes) => Self::Inline(bytes.clone()),
            wit_types::ProjBytes::Deferred(mode) => {
                Self::Deferred(cache::ReadModeCache::from(*mode))
            },
        }
    }
}

impl From<wit_types::ReadMode> for cache::ReadModeCache {
    fn from(mode: wit_types::ReadMode) -> Self {
        match mode {
            wit_types::ReadMode::Full => Self::Full,
            wit_types::ReadMode::Ranged => Self::Ranged,
        }
    }
}

impl From<wit_types::Stability> for cache::StabilityCache {
    fn from(stability: wit_types::Stability) -> Self {
        match stability {
            wit_types::Stability::Immutable => Self::Immutable,
            wit_types::Stability::Mutable => Self::Mutable,
            wit_types::Stability::Volatile => Self::Volatile,
        }
    }
}

impl From<&wit_types::EntryKind> for EntryMeta {
    fn from(kind: &wit_types::EntryKind) -> Self {
        match kind {
            wit_types::EntryKind::Directory => Self::directory(),
            wit_types::EntryKind::File(file) => Self::file(cache::FileAttrsCache::from(file)),
        }
    }
}

impl From<&wit_types::EntryKind> for cache::EntryKindCache {
    fn from(kind: &wit_types::EntryKind) -> Self {
        match kind {
            wit_types::EntryKind::Directory => Self::Directory,
            wit_types::EntryKind::File(_) => Self::File,
        }
    }
}
