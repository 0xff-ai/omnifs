use omnifs_cache::{Record as CacheRecord, RecordKind};
use omnifs_core::path::Path as ProtocolPath;
use omnifs_core::view as view_types;
use omnifs_core::view::{self as cache, EntryMeta, FileAttrsCache, FilePayload};
use omnifs_host::Runtime;
use omnifs_host::wit_protocol;
use omnifs_wit::provider::types::{
    self as wit_types, ByteSource, ErrorKind, ProviderError, ReadFileResult,
};

pub(crate) fn cache_get(
    runtime: &Runtime,
    path: &ProtocolPath,
    kind: RecordKind,
    aux: Option<&str>,
) -> Option<CacheRecord> {
    runtime.cache_get(path.as_str(), kind, aux)
}

pub(crate) fn cache_put(
    runtime: &Runtime,
    path: &ProtocolPath,
    kind: RecordKind,
    aux: Option<&str>,
    record: &CacheRecord,
) {
    runtime.cache_put(path.as_str(), kind, aux, record);
}

pub(crate) struct InvalidationSet {
    paths: Vec<ProtocolPath>,
    prefixes: Vec<ProtocolPath>,
}

impl InvalidationSet {
    pub(crate) fn from_raw(paths: Vec<String>, prefixes: Vec<String>) -> Self {
        Self {
            paths: paths
                .into_iter()
                .filter_map(|path| ProtocolPath::parse(&path).ok())
                .collect(),
            prefixes: prefixes
                .into_iter()
                .filter_map(|prefix| ProtocolPath::parse(&prefix).ok())
                .collect(),
        }
    }

    pub(crate) fn matches(&self, path: &ProtocolPath) -> bool {
        self.paths.iter().any(|invalidated| invalidated == path)
            || self.prefixes.iter().any(|prefix| path.has_prefix(prefix))
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum ProviderFsError {
    NotFound,
    NotDirectory,
    IsDirectory,
    Access,
    InvalidInput,
    TooLarge,
    Retry,
    Io,
}

impl ProviderFsError {
    pub(crate) fn from_provider(error: &ProviderError) -> Self {
        match error.kind {
            ErrorKind::NotFound => Self::NotFound,
            ErrorKind::NotADirectory => Self::NotDirectory,
            ErrorKind::NotAFile => Self::IsDirectory,
            ErrorKind::PermissionDenied | ErrorKind::Denied => Self::Access,
            ErrorKind::InvalidInput => Self::InvalidInput,
            ErrorKind::TooLarge => Self::TooLarge,
            ErrorKind::RateLimited => Self::Retry,
            ErrorKind::Network
            | ErrorKind::Timeout
            | ErrorKind::VersionMismatch
            | ErrorKind::Internal => Self::Io,
        }
    }
}

#[derive(Debug, Clone)]
pub(crate) enum LookupCacheHit {
    Positive(EntryMeta),
    Negative,
}

pub(crate) fn cached_lookup_record(record: &CacheRecord) -> Option<LookupCacheHit> {
    match cache::LookupPayload::deserialize(&record.payload)? {
        cache::LookupPayload::Positive(meta) => Some(LookupCacheHit::Positive(meta)),
        cache::LookupPayload::Negative => Some(LookupCacheHit::Negative),
    }
}

pub(crate) fn cached_dirent_lookup(record: &CacheRecord, name: &str) -> Option<LookupCacheHit> {
    let dirents = cache::DirentsPayload::deserialize(&record.payload)?;
    if let Some(entry) = dirents.entries.iter().find(|entry| entry.name == name) {
        return Some(LookupCacheHit::Positive(entry.meta.clone()));
    }
    dirents.exhaustive.then_some(LookupCacheHit::Negative)
}

pub(crate) fn cached_exhaustive_dirents(record: &CacheRecord) -> Option<cache::DirentsPayload> {
    let dirents = cache::DirentsPayload::deserialize(&record.payload)?;
    dirents.exhaustive.then_some(dirents)
}

pub(crate) fn cached_file_attrs(runtime: &Runtime, path: &ProtocolPath) -> Option<FileAttrsCache> {
    if let Some(record) = cache_get(runtime, path, RecordKind::Lookup, None)
        && let Some(LookupCacheHit::Positive(meta)) = cached_lookup_record(&record)
        && let Some(attrs) = meta.attrs
    {
        return Some(attrs);
    }

    runtime
        .cache_get(path.as_str(), RecordKind::Attr, None)
        .and_then(|record| cache::AttrPayload::deserialize(&record.payload))
        .and_then(|payload| payload.meta.attrs)
}

pub(crate) fn exact_file_attrs(size: u64) -> FileAttrsCache {
    FileAttrsCache {
        size: view_types::FileSize::Exact(size),
        bytes: view_types::ByteSource::Deferred(view_types::ReadMode::Full),
        stability: view_types::Stability::Immutable,
        version_token: None,
    }
}

pub(crate) struct ResolvedRead {
    pub(crate) data: Vec<u8>,
    pub(crate) attrs: FileAttrsCache,
    pub(crate) content_type: Option<String>,
    pub(crate) cache_rendered_file: bool,
}

impl ResolvedRead {
    pub(crate) fn from_provider_result(
        runtime: &Runtime,
        path: &ProtocolPath,
        result: ReadFileResult,
    ) -> Option<Self> {
        let attrs = wit_protocol::file_attrs_from_attrs(&result.attrs);
        let content_type = result.content_type;
        match result.bytes {
            ByteSource::Inline(bytes) => Some(Self {
                data: bytes,
                attrs,
                content_type,
                cache_rendered_file: true,
            }),
            ByteSource::Blob(blob) => runtime.read_blob_full(blob).ok().map(|bytes| Self {
                data: bytes,
                attrs,
                content_type,
                cache_rendered_file: false,
            }),
            ByteSource::Canonical => runtime
                .canonical_bytes_for(path.as_str())
                .map(|bytes| Self {
                    data: bytes,
                    attrs,
                    content_type,
                    cache_rendered_file: false,
                }),
            ByteSource::Deferred(_) => None,
        }
    }
}

pub(crate) fn learned_full_read_attrs(attrs: FileAttrsCache, content_len: usize) -> FileAttrsCache {
    if !can_publish_learned_size(&attrs) {
        return attrs;
    }
    match attrs.size {
        view_types::FileSize::Exact(_) => attrs,
        view_types::FileSize::NonZero | view_types::FileSize::Unknown => {
            attrs.with_exact_size(u64::try_from(content_len).unwrap_or(u64::MAX))
        },
    }
}

pub(crate) fn learned_ranged_eof_attrs(
    attrs: FileAttrsCache,
    eof_size: u64,
) -> Option<FileAttrsCache> {
    if !can_publish_learned_size(&attrs) {
        return None;
    }
    match attrs.size {
        view_types::FileSize::Exact(_) => None,
        view_types::FileSize::NonZero | view_types::FileSize::Unknown => {
            Some(attrs.with_exact_size(eof_size))
        },
    }
}

fn can_publish_learned_size(attrs: &FileAttrsCache) -> bool {
    match attrs.stability {
        view_types::Stability::Immutable | view_types::Stability::Mutable => true,
        view_types::Stability::Volatile => false,
    }
}

pub(crate) fn full_read_matches_attrs(attrs: &FileAttrsCache, content_len: usize) -> bool {
    match attrs.size {
        view_types::FileSize::Exact(size) => {
            u64::try_from(content_len).is_ok_and(|content_len| content_len == size)
        },
        view_types::FileSize::NonZero => content_len > 0,
        view_types::FileSize::Unknown => true,
    }
}

pub(crate) fn file_payload_matches_attrs(attrs: &FileAttrsCache, payload: &FilePayload) -> bool {
    if matches!(attrs.stability, view_types::Stability::Mutable)
        && payload.version_token != attrs.version_token
    {
        return false;
    }
    full_read_matches_attrs(attrs, payload.content.len())
}

pub(crate) fn durable_file_record(
    attrs: &FileAttrsCache,
    data: &[u8],
    content_type: Option<String>,
) -> Option<(Option<String>, CacheRecord)> {
    let aux = attrs.durable_cache_aux()?;
    let payload = FilePayload::new(attrs.version_token.clone(), data.to_vec())
        .with_content_type(content_type);
    let payload = payload.serialize()?;
    Some((aux, CacheRecord::new(RecordKind::File, payload)))
}

pub(crate) fn cache_file_metadata(runtime: &Runtime, path: &ProtocolPath, attrs: FileAttrsCache) {
    let meta = EntryMeta::file(attrs);
    let lookup = cache::LookupPayload::Positive(meta.clone());
    if let Some(payload) = lookup.serialize() {
        cache_put(
            runtime,
            path,
            RecordKind::Lookup,
            None,
            &CacheRecord::new(RecordKind::Lookup, payload),
        );
    }

    let attr = cache::AttrPayload { meta };
    if let Some(payload) = attr.serialize() {
        cache_put(
            runtime,
            path,
            RecordKind::Attr,
            None,
            &CacheRecord::new(RecordKind::Attr, payload),
        );
    }
}

pub(crate) fn opened_file_attrs(
    projected: Option<&FileAttrsCache>,
    opened: &wit_types::FileAttrs,
) -> Result<FileAttrsCache, String> {
    let Some(projected) = projected else {
        return Err("open-file returned without a prior ranged file projection".to_string());
    };
    if !matches!(
        projected.bytes,
        view_types::ByteSource::Deferred(view_types::ReadMode::Ranged)
    ) {
        return Err("open-file requires byte-source::deferred(read-mode::ranged)".to_string());
    }
    let attrs = FileAttrsCache {
        size: wit_protocol::file_size_from_wit(opened.size),
        bytes: projected.bytes.clone(),
        stability: wit_protocol::stability_from_wit(opened.stability),
        version_token: opened.version_token.clone(),
    };
    attrs.validate()?;
    Ok(attrs)
}

pub(crate) fn merge_file_attrs(
    existing: Option<&FileAttrsCache>,
    incoming: Option<FileAttrsCache>,
) -> Option<FileAttrsCache> {
    match (existing, incoming) {
        (Some(existing), Some(incoming))
            if should_preserve_learned_exact_size(existing, &incoming) =>
        {
            Some(existing.clone())
        },
        (_, incoming) => incoming,
    }
}

fn should_preserve_learned_exact_size(
    existing: &FileAttrsCache,
    incoming: &FileAttrsCache,
) -> bool {
    matches!(existing.size, view_types::FileSize::Exact(_))
        && !matches!(incoming.size, view_types::FileSize::Exact(_))
        && existing.bytes == incoming.bytes
        && existing.stability == incoming.stability
        && existing.version_token == incoming.version_token
        && (matches!(existing.stability, view_types::Stability::Immutable)
            || (matches!(existing.stability, view_types::Stability::Mutable)
                && existing.version_token.is_some()))
}
