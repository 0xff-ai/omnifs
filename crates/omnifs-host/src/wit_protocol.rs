//! WIT bindgen ↔ host view conversions.
//!
//! Single hub for wire-to-view translation at the host boundary.

use omnifs_core::view::{
    ByteSource, CachedCursor, EntryKind, EntryMeta, FileAttrsCache, FileSize, ReadMode, Stability,
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
        wit_types::Stability::Immutable => Stability::Immutable,
        wit_types::Stability::Mutable => Stability::Mutable,
        wit_types::Stability::Volatile => Stability::Volatile,
    }
}

pub(crate) fn read_mode_from_wit(mode: wit_types::ReadMode) -> ReadMode {
    match mode {
        wit_types::ReadMode::Full => ReadMode::Full,
        wit_types::ReadMode::Ranged => ReadMode::Ranged,
    }
}

pub(crate) fn byte_source_from_wit(source: &wit_types::ByteSource) -> ByteSource {
    match source {
        wit_types::ByteSource::Inline(bytes) => ByteSource::Inline(bytes.clone()),
        wit_types::ByteSource::Deferred(mode) => ByteSource::Deferred(read_mode_from_wit(*mode)),
        wit_types::ByteSource::Canonical => ByteSource::Canonical,
        wit_types::ByteSource::Blob(blob) => ByteSource::Blob(*blob),
    }
}

pub(crate) fn file_attrs_from_file_out(file: &wit_types::FileOut) -> FileAttrsCache {
    FileAttrsCache {
        size: file_size_from_wit(file.attrs.size),
        bytes: byte_source_from_wit(&file.bytes),
        stability: stability_from_wit(file.attrs.stability),
        version_token: file.attrs.version_token.clone(),
    }
}

pub fn file_attrs_from_attrs(attrs: &wit_types::FileAttrs) -> FileAttrsCache {
    FileAttrsCache {
        size: file_size_from_wit(attrs.size),
        bytes: ByteSource::Deferred(ReadMode::Full),
        stability: stability_from_wit(attrs.stability),
        version_token: attrs.version_token.clone(),
    }
}

pub fn entry_meta_from_kind(kind: &wit_types::EntryKind) -> EntryMeta {
    match kind {
        wit_types::EntryKind::Directory => EntryMeta::directory(),
        wit_types::EntryKind::File(file) => EntryMeta::file(file_attrs_from_file_out(file)),
    }
}

pub fn cached_cursor_from_wit(cursor: wit_types::Cursor) -> CachedCursor {
    match cursor {
        wit_types::Cursor::Opaque(token) => CachedCursor::Opaque(token),
        wit_types::Cursor::Page(page) => CachedCursor::Page(page),
    }
}

pub(crate) fn cached_cursor_to_wit(cursor: CachedCursor) -> wit_types::Cursor {
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
                stability: wit_types::Stability::Mutable,
                version_token: None,
            },
            bytes: wit_types::ByteSource::Deferred(wit_types::ReadMode::Full),
            content_type: None,
        }),
    }
}
