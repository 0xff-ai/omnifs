//! NFS-renderer cache helpers that remain after the read/list/lookup decision
//! logic moved into `omnifs-tree`.
//!
//! These are the thin pieces the NFS adapter still drives directly because they
//! are flatten-renderer concerns the projection core does not model: the
//! cached-dirents positive-lookup probe (so a probe name seen in a partial
//! listing beats the expected-negative shortcut), the inline-projection read
//! path (bytes that live in a cached projection with no provider file route),
//! and the learned-attrs slot survival on the NFS inode table.

use omnifs_cache::{Record as CacheRecord, RecordKind};
use omnifs_core::path::Path;
use omnifs_core::view as view_types;
use omnifs_core::view::{self as cache, EntryMeta, FileAttrsCache};
use omnifs_host::Runtime;

pub(crate) fn cache_get(
    runtime: &Runtime,
    path: &Path,
    kind: RecordKind,
    aux: Option<&str>,
) -> Option<CacheRecord> {
    runtime.cache_get(path, kind, aux)
}

fn cache_put(
    runtime: &Runtime,
    path: &Path,
    kind: RecordKind,
    aux: Option<&str>,
    record: &CacheRecord,
) {
    runtime.cache_put(path, kind, aux, record);
}

#[derive(Debug, Clone)]
pub(crate) enum LookupCacheHit {
    Positive(EntryMeta),
    Negative,
}

fn cached_lookup_record(record: &CacheRecord) -> Option<LookupCacheHit> {
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

pub(crate) fn cached_file_attrs(runtime: &Runtime, path: &Path) -> Option<FileAttrsCache> {
    if let Some(record) = cache_get(runtime, path, RecordKind::Lookup, None)
        && let Some(LookupCacheHit::Positive(meta)) = cached_lookup_record(&record)
        && let Some(attrs) = meta.attrs
    {
        return Some(attrs);
    }

    runtime
        .cache_get(path, RecordKind::Attr, None)
        .and_then(|record| cache::AttrPayload::deserialize(&record.payload))
        .and_then(|payload| payload.meta.attrs)
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

fn can_publish_learned_size(attrs: &FileAttrsCache) -> bool {
    match attrs.stability {
        view_types::Stability::Stable | view_types::Stability::Dynamic => true,
        view_types::Stability::Live => false,
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

pub(crate) fn cache_file_metadata(runtime: &Runtime, path: &Path, attrs: FileAttrsCache) {
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

/// Keep a learned exact size on the NFS inode across an origin-agnostic refresh:
/// a re-listing that projects a kind-derived placeholder must not erase a size
/// learned from a complete read. Returns the attrs the inode should hold after
/// merging `incoming` over `existing`. NFS keeps this renderer-side, exactly as
/// the FUSE inode does.
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
        && (matches!(existing.stability, view_types::Stability::Stable)
            || (matches!(existing.stability, view_types::Stability::Dynamic)
                && existing.version_token.is_some()))
}
