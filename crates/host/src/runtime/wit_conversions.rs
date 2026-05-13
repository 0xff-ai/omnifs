use crate::cache::{self, EntryMeta};
use crate::omnifs::provider::types as wit_types;

impl From<&wit_types::FileProj> for cache::FileAttrsCache {
    fn from(file: &wit_types::FileProj) -> Self {
        Self {
            size: file.attrs.size,
            bytes: file.bytes.clone(),
            stability: file.attrs.stability,
            version_token: file.attrs.version_token.clone(),
        }
    }
}

impl From<&wit_types::FileAttrs> for cache::FileAttrsCache {
    fn from(attrs: &wit_types::FileAttrs) -> Self {
        Self {
            size: attrs.size,
            bytes: wit_types::ProjBytes::Deferred(wit_types::ReadMode::Full),
            stability: attrs.stability,
            version_token: attrs.version_token.clone(),
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
