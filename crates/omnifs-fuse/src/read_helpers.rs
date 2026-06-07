//! Read-path helpers: payload resolution, slicing, learned sizes.

use fuser::Errno;
use omnifs_cache::Record as CacheRecord;
use omnifs_core::view as view_types;
use omnifs_core::view::{FileAttrsCache, FilePayload};
use omnifs_host::Runtime;
use omnifs_wit::provider::types::{ByteSource, ReadFileResult};
use tracing::warn;

/// Materialize a `read-file` terminal into the bytes the FUSE response
/// will return. Inline content travels in the WIT; blob content gets
/// pulled from the host's blob cache; `canonical` is served from the
/// anchor-keyed canonical store without copying across the WIT (ADR-0001
/// §5.1). Returns `None` when the byte source can't be resolved (logged at
/// warn for diagnostics).
pub(super) fn resolve_read_payload(
    runtime: &Runtime,
    path: &str,
    result: ReadFileResult,
) -> Option<(Vec<u8>, FileAttrsCache, Option<String>)> {
    let attrs = omnifs_host::wit_protocol::file_attrs_from_attrs(&result.attrs);
    let content_type = result.content_type;
    match result.bytes {
        ByteSource::Inline(bytes) => Some((bytes, attrs, content_type)),
        ByteSource::Blob(blob) => match runtime.read_blob_full(blob) {
            Ok(bytes) => Some((bytes, attrs, content_type)),
            Err(e) => {
                warn!(path, error = %e, "blob-backed read failed");
                None
            },
        },
        ByteSource::Canonical => {
            if let Some(bytes) = runtime.canonical_bytes_for(path) {
                Some((bytes, attrs, content_type))
            } else {
                warn!(
                    path,
                    "read answered byte-source::canonical but no canonical covers the path"
                );
                None
            }
        },
        // The validator rejects a `deferred` read answer before FUSE is
        // reached; a read must produce bytes.
        ByteSource::Deferred(_) => {
            warn!(
                path,
                "read answered byte-source::deferred, which is not a valid read answer"
            );
            None
        },
    }
}

/// Slice `data` at the given FUSE `offset` and `size`, returning the relevant
/// byte range. Returns an empty slice when `offset` is past the end.
#[allow(clippy::cast_possible_truncation)]
pub(super) fn data_slice(data: &[u8], offset: u64, size: u32) -> &[u8] {
    let start = offset as usize;
    let end = (start + size as usize).min(data.len());
    data.get(start..end).unwrap_or(&[])
}

pub(super) fn should_prefetch_full_on_open(attrs: &FileAttrsCache) -> bool {
    matches!(
        attrs.bytes,
        view_types::ByteSource::Deferred(view_types::ReadMode::Full)
    ) && !matches!(attrs.size, view_types::FileSize::Exact(_))
}

pub(super) fn learned_full_read_attrs(attrs: FileAttrsCache, content_len: usize) -> FileAttrsCache {
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

pub(super) fn learned_ranged_eof_attrs(
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

pub(super) fn opened_file_attrs(
    path: &str,
    projected: Option<&FileAttrsCache>,
    opened: &omnifs_wit::provider::types::FileAttrs,
) -> Result<FileAttrsCache, Errno> {
    let Some(projected) = projected else {
        warn!(
            path,
            "open-file returned without a prior ranged file projection"
        );
        return Err(Errno::EIO);
    };
    if !matches!(
        projected.bytes,
        view_types::ByteSource::Deferred(view_types::ReadMode::Ranged)
    ) {
        warn!(
            path,
            "open-file requires byte-source::deferred(read-mode::ranged)"
        );
        return Err(Errno::EIO);
    }
    Ok(FileAttrsCache {
        size: omnifs_host::wit_protocol::file_size_from_wit(opened.size),
        bytes: projected.bytes.clone(),
        stability: omnifs_host::wit_protocol::stability_from_wit(opened.stability),
        version_token: opened.version_token.clone(),
    })
}

pub(super) fn can_publish_learned_size(attrs: &FileAttrsCache) -> bool {
    match attrs.stability {
        view_types::Stability::Immutable | view_types::Stability::Mutable => true,
        view_types::Stability::Volatile => false,
    }
}

pub(super) fn full_read_matches_attrs(attrs: &FileAttrsCache, content_len: usize) -> bool {
    match attrs.size {
        view_types::FileSize::Exact(size) => {
            u64::try_from(content_len).is_ok_and(|content_len| content_len == size)
        },
        view_types::FileSize::NonZero => content_len > 0,
        view_types::FileSize::Unknown => true,
    }
}

pub(super) fn file_payload_for_attrs(
    record: &CacheRecord,
    attrs: Option<&FileAttrsCache>,
) -> Option<FilePayload> {
    let payload = FilePayload::deserialize(&record.payload)?;
    let attrs = attrs?;
    if matches!(attrs.stability, view_types::Stability::Mutable)
        && payload.version_token != attrs.version_token
    {
        return None;
    }
    if !full_read_matches_attrs(attrs, payload.content.len()) {
        return None;
    }
    Some(payload)
}
