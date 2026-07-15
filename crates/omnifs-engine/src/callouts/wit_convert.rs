//! WIT bindgen ↔ host view conversions.
//!
//! Single hub for wire-to-view translation at the host boundary.

use crate::view::{
    BodyId, ByteSource, CachedCursor, EntryKind, EntryMeta, FileAttrsCache, FileSize, ReadMode,
    Stability,
};

use omnifs_wit::provider::types as wit_types;

pub fn file_size_from_wit(size: wit_types::FileSize) -> FileSize {
    match size {
        wit_types::FileSize::Exact(size) => FileSize::Exact(size),
        wit_types::FileSize::NonZero => FileSize::NonZero,
        wit_types::FileSize::Unknown => FileSize::Unknown,
    }
}

pub fn stability_from_wit(stability: wit_types::Stability) -> Stability {
    match stability {
        wit_types::Stability::Stable => Stability::Stable,
        wit_types::Stability::Dynamic => Stability::Dynamic,
        wit_types::Stability::Live => Stability::Live,
    }
}

pub(crate) fn read_mode_from_wit(mode: wit_types::ReadMode) -> ReadMode {
    match mode {
        wit_types::ReadMode::Full => ReadMode::Full,
        wit_types::ReadMode::Ranged => ReadMode::Ranged,
    }
}

pub(crate) fn byte_source_from_wit(source: &wit_types::ByteSource) -> Result<ByteSource, String> {
    match source {
        wit_types::ByteSource::Inline(bytes) => Ok(ByteSource::Inline(bytes.clone())),
        wit_types::ByteSource::Deferred(mode) => {
            Ok(ByteSource::Deferred(read_mode_from_wit(*mode)))
        },
        wit_types::ByteSource::Canonical => Ok(ByteSource::Canonical),
        wit_types::ByteSource::Blob(_) => {
            Err("runtime blob handle requires mount resolution".into())
        },
    }
}

pub(crate) fn try_file_attrs_from_file_out(
    file: &wit_types::FileOut,
    resolve_blob: impl Fn(u64) -> Result<(BodyId, u64), String>,
) -> Result<FileAttrsCache, String> {
    let declared = file_size_from_wit(file.attrs.size);
    let (bytes, size) = match &file.bytes {
        wit_types::ByteSource::Blob(blob) => {
            let (body, length) = resolve_blob(*blob)?;
            validate_trusted_size(declared, length)?;
            (ByteSource::Body(body), crate::view::FileSize::Exact(length))
        },
        source => (byte_source_from_wit(source)?, declared),
    };
    FileAttrsCache::from_parts(
        size,
        bytes,
        stability_from_wit(file.attrs.stability),
        file.attrs.version_token.clone(),
    )
}

pub fn try_file_attrs_from_attrs(attrs: &wit_types::FileAttrs) -> Result<FileAttrsCache, String> {
    FileAttrsCache::deferred(
        file_size_from_wit(attrs.size),
        ReadMode::Full,
        stability_from_wit(attrs.stability),
        attrs.version_token.clone(),
    )
}

pub fn file_attrs_from_attrs(attrs: &wit_types::FileAttrs) -> FileAttrsCache {
    try_file_attrs_from_attrs(attrs)
        .expect("provider file attrs are validated before view conversion")
}

pub fn entry_meta_from_kind(
    kind: &wit_types::EntryKind,
    resolve_blob: impl Fn(u64) -> Result<(BodyId, u64), String>,
) -> Result<EntryMeta, String> {
    match kind {
        wit_types::EntryKind::Directory => Ok(EntryMeta::directory()),
        wit_types::EntryKind::File(file) => Ok(EntryMeta::file(try_file_attrs_from_file_out(
            file,
            resolve_blob,
        )?)),
    }
}

fn validate_trusted_size(size: crate::view::FileSize, length: u64) -> Result<(), String> {
    match size {
        crate::view::FileSize::Exact(expected) if expected != length => Err(format!(
            "blob body length {length} disagrees with declared size {expected}"
        )),
        crate::view::FileSize::NonZero if length == 0 => {
            Err("blob body declared NonZero but is empty".into())
        },
        _ => Ok(()),
    }
}

pub fn cached_cursor_from_wit(cursor: wit_types::Cursor) -> CachedCursor {
    match cursor {
        wit_types::Cursor::Opaque(token) => CachedCursor::Opaque(token),
        wit_types::Cursor::Page(page) => CachedCursor::Page(page),
    }
}

pub fn cached_cursor_to_wit(cursor: CachedCursor) -> wit_types::Cursor {
    match cursor {
        CachedCursor::Opaque(token) => wit_types::Cursor::Opaque(token),
        CachedCursor::Page(page) => wit_types::Cursor::Page(page),
    }
}

pub fn entry_kind_to_wit(kind: &EntryKind) -> wit_types::EntryKind {
    match kind {
        EntryKind::Directory => wit_types::EntryKind::Directory,
        EntryKind::File => wit_types::EntryKind::File(wit_types::FileOut {
            attrs: wit_types::FileAttrs {
                size: wit_types::FileSize::Unknown,
                stability: wit_types::Stability::Dynamic,
                version_token: None,
            },
            bytes: wit_types::ByteSource::Deferred(wit_types::ReadMode::Full),
            content_type: None,
        }),
    }
}
