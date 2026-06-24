//! Projected file attributes: declared size, stability, and version
//! evidence, plus the validation rules that keep projections honest.
//!
//! These attributes are the contract the host derives filesystem behavior
//! from: the `st_size` reported before any read, kernel attribute/content
//! caching policy, direct I/O and the ranged live path for live files,
//! and version-keyed durable content caching. Declare only what you actually
//! know; the host learns the rest (for example the real size after the
//! first read). Lying here breaks standard tools: an inflated size breaks
//! `tail -c`, a false `Stable` serves stale bytes forever.

use crate::error::{ProviderError, Result};
use omnifs_core::ContentType;
use omnifs_wit::provider::types as wit_types;

/// Per-projection inline byte cap (64 KiB), enforced by
/// [`FileProj::validate`]. Content larger than this must be a deferred
/// projection or a blob; it cannot ride inline in a terminal.
pub const MAX_PROJECTED_BYTES: usize = 64 * 1024;
/// Maximum [`VersionToken`] length in bytes, enforced by
/// [`VersionToken::validate`].
pub const MAX_VERSION_TOKEN_BYTES: usize = 256;

/// Declared metadata for a projected file: size, stability, and optional
/// version evidence.
///
/// A version token lets the host key durable content by version and lets
/// conditional reloads answer cheaply ([`crate::object::Load::Unchanged`]).
/// It carries the most weight on `Dynamic` content, where it is the proof
/// that cached bytes are still current.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct FileAttrs {
    pub size: Size,
    pub stability: Stability,
    pub version: Option<VersionToken>,
}

impl FileAttrs {
    pub fn new(size: Size, stability: Stability) -> Self {
        Self {
            size,
            stability,
            version: None,
        }
    }

    #[must_use]
    pub fn with_version(mut self, version: impl Into<VersionToken>) -> Self {
        self.version = Some(version.into());
        self
    }

    /// The `st_size` value implied by the declared size; see
    /// [`Size::st_size`] for the placeholder rule.
    pub fn st_size(&self) -> u64 {
        self.size.st_size()
    }

    pub fn validate(&self) -> Result<()> {
        if let Some(version) = &self.version {
            version.validate()?;
        }

        Ok(())
    }
}

/// Opaque version evidence for a file or object: an `ETag`, commit SHA,
/// updated-at timestamp, or any string that changes when the content does.
///
/// Must be non-empty and at most [`MAX_VERSION_TOKEN_BYTES`] bytes
/// ([`Self::validate`]); the same type doubles as the object-layer
/// conditional-request validator ([`crate::object::Validator`]).
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct VersionToken(pub String);

impl VersionToken {
    pub fn as_str(&self) -> &str {
        &self.0
    }

    pub fn validate(&self) -> Result<()> {
        if self.0.is_empty() {
            return Err(ProviderError::invalid_input(
                "version token must not be empty",
            ));
        }
        if self.0.len() > MAX_VERSION_TOKEN_BYTES {
            return Err(ProviderError::invalid_input(format!(
                "version token exceeds {MAX_VERSION_TOKEN_BYTES} bytes"
            )));
        }
        Ok(())
    }
}

impl From<String> for VersionToken {
    fn from(value: String) -> Self {
        Self(value)
    }
}

impl From<&str> for VersionToken {
    fn from(value: &str) -> Self {
        Self(value.to_string())
    }
}

/// Declared file size: the truthful byte length, "non-empty but length
/// unknown", or nothing known at all.
///
/// Declare `Exact` only when you know the precise length without an extra
/// upstream call; `NonZero` when you know content exists (a field is
/// present) but not its size; `Unknown` otherwise. Do not fabricate exact
/// sizes: stat-driven tools (`tail -c`, `rsync --size-only`, `wc -c` on
/// stat-only paths) trust them.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Size {
    Exact(u64),
    NonZero,
    Unknown,
}

impl Size {
    /// The `st_size` to report before any read: the exact length, or the
    /// placeholder `1` for `NonZero` and `Unknown`.
    ///
    /// The placeholder is deliberately tiny and non-zero: zero would make
    /// size-checking tools skip the file as empty, and a large fake value
    /// (the old 256 MiB placeholder) broke `tail -n` and friends. The host
    /// replaces it with the learned size once content has been
    /// materialized.
    pub fn st_size(&self) -> u64 {
        match self {
            Self::Exact(size) => *size,
            Self::NonZero | Self::Unknown => 1,
        }
    }
}

/// A projected file: attributes plus a byte source, as it appears in
/// directory entries and `fs` effects.
///
/// Provider authors usually build the higher-level
/// [`crate::projection::FileProjection`] instead; `FileProj` is what it
/// lowers onto. Every construction path runs [`Self::validate`] before
/// crossing the WIT, so an illegal combination fails the operation rather
/// than reaching the host.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct FileProj {
    pub attrs: FileAttrs,
    pub bytes: ProjBytes,
    pub content_type: Option<ContentType>,
}

impl FileProj {
    /// Carry the bytes now. Size is set to the exact byte length, which is
    /// the only legal size for inline content.
    pub fn inline(
        bytes: impl Into<Vec<u8>>,
        stability: Stability,
        version: Option<VersionToken>,
    ) -> Self {
        let bytes = bytes.into();
        Self {
            attrs: FileAttrs {
                size: Size::Exact(u64::try_from(bytes.len()).unwrap_or(u64::MAX)),
                stability,
                version,
            },
            bytes: ProjBytes::Inline(bytes),
            content_type: None,
        }
    }

    /// Declare the file without carrying bytes; content is served later
    /// through `read-file` (`ReadMode::Full`) or the
    /// `open-file`/`read-chunk` session (`ReadMode::Ranged`). Declare
    /// honest attrs: this is the correct shape when a listing payload does
    /// not contain the leaf's full content.
    pub fn deferred(size: Size, read: ReadMode, stability: Stability) -> Self {
        Self {
            attrs: FileAttrs::new(size, stability),
            bytes: ProjBytes::Deferred { read },
            content_type: None,
        }
    }

    /// Directory listing entry: attrs for `stat`/`ls` without inline bytes; content
    /// is loaded on `read-file` or via an object/canonical path.
    pub fn listing_shape() -> Self {
        Self::deferred(Size::Unknown, ReadMode::Full, Stability::Stable)
    }

    /// Listing entry for a route declared `ranged`: like [`Self::listing_shape`]
    /// but with `ReadMode::Ranged`, so a lookup/listing placeholder tells the
    /// host the file streams through `open-file`/`read-chunk` without running
    /// the handler. The real size and stability are learned at `open-file`.
    pub fn ranged_listing_shape() -> Self {
        Self::deferred(Size::Unknown, ReadMode::Ranged, Stability::Stable)
    }

    #[must_use]
    pub fn with_version(mut self, version: impl Into<VersionToken>) -> Self {
        self.attrs.version = Some(version.into());
        self
    }

    #[must_use]
    pub fn with_content_type(mut self, content_type: ContentType) -> Self {
        self.content_type = Some(content_type);
        self
    }

    /// Enforce the structural legality rules:
    ///
    /// - `Stability::Live` requires `ProjBytes::Deferred { read:
    ///   ReadMode::Ranged }`: bytes that can change mid-read may only be
    ///   served through the ranged live path, never inline or full-read.
    /// - Inline bytes require `Size::Exact` equal to the actual byte
    ///   length, and at most [`MAX_PROJECTED_BYTES`].
    /// - A version token, when present, must be non-empty and within
    ///   [`MAX_VERSION_TOKEN_BYTES`].
    pub fn validate(&self) -> Result<()> {
        self.attrs.validate()?;

        if self.attrs.stability == Stability::Live
            && !matches!(
                self.bytes,
                ProjBytes::Deferred {
                    read: ReadMode::Ranged
                }
            )
        {
            return Err(ProviderError::invalid_input(
                "Stability::Live requires ProjBytes::Deferred { read: ReadMode::Ranged }",
            ));
        }

        match (&self.bytes, &self.attrs.size) {
            (ProjBytes::Inline(bytes), Size::Exact(size)) => {
                let len = u64::try_from(bytes.len())
                    .map_err(|_| ProviderError::too_large("inline file length does not fit u64"))?;
                if *size != len {
                    return Err(ProviderError::invalid_input(format!(
                        "inline projection declares size {size} but carries {len} bytes"
                    )));
                }
                if bytes.len() > MAX_PROJECTED_BYTES {
                    return Err(ProviderError::too_large(format!(
                        "inline projection exceeds eager byte limit of {MAX_PROJECTED_BYTES} bytes"
                    )));
                }
            },
            (ProjBytes::Inline(_), Size::NonZero | Size::Unknown) => {
                return Err(ProviderError::invalid_input(
                    "inline projection bytes require Size::Exact(bytes.len())",
                ));
            },
            (ProjBytes::Deferred { .. }, _) => {},
        }

        Ok(())
    }
}

/// Byte source for a projected file: bytes carried now, or a promise to
/// serve them on demand in the given [`ReadMode`].
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum ProjBytes {
    Inline(Vec<u8>),
    Deferred { read: ReadMode },
}

/// Byte source for a completed read answer: inline bytes, or a handle to a
/// host-resident blob (the bytes never cross the WIT). Deferred is
/// deliberately not an option here: a read must answer with concrete bytes.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum ReadFileBytes {
    Inline(Vec<u8>),
    Blob(crate::blob::BlobId),
}

/// How deferred content is read: one whole-file `read-file`, or arbitrary
/// `(offset, length)` chunks through an `open-file`/`read-chunk` session
/// backed by a [`crate::handler::RangeReader`]. This declares provider
/// capability, not cache policy; the host may still read a ranged file in
/// full.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ReadMode {
    Full,
    Ranged,
}

/// How the bytes behave over time for one logical identity; the host
/// derives its caching policy from this.
///
/// - `Stable`: the bytes never change for this path identity (a pinned
///   version, a content-addressed artifact). The host may cache content and
///   learned size indefinitely.
/// - `Dynamic`: bytes may change between reads but not during one. Durable
///   caching is tied to version evidence and invalidations.
/// - `Live`: bytes may change while being observed (`tail -f` shapes).
///   The host serves it through direct I/O and the ranged live path and
///   never caches content or learned size. Structurally requires a
///   deferred ranged projection ([`FileProj::validate`]).
///
/// Declaring `Dynamic` when unsure is safe; declaring `Stable` when the
/// content can change pins stale bytes in caches with no expiry.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Stability {
    Stable,
    Dynamic,
    Live,
}

impl From<Size> for wit_types::FileSize {
    fn from(size: Size) -> Self {
        match size {
            Size::Exact(size) => Self::Exact(size),
            Size::NonZero => Self::NonZero,
            Size::Unknown => Self::Unknown,
        }
    }
}

impl From<ReadMode> for wit_types::ReadMode {
    fn from(mode: ReadMode) -> Self {
        match mode {
            ReadMode::Full => Self::Full,
            ReadMode::Ranged => Self::Ranged,
        }
    }
}

impl From<ProjBytes> for wit_types::ByteSource {
    fn from(bytes: ProjBytes) -> Self {
        match bytes {
            ProjBytes::Inline(bytes) => Self::Inline(bytes),
            ProjBytes::Deferred { read } => Self::Deferred(read.into()),
        }
    }
}

impl From<ReadFileBytes> for wit_types::ByteSource {
    fn from(bytes: ReadFileBytes) -> Self {
        match bytes {
            ReadFileBytes::Inline(bytes) => Self::Inline(bytes),
            ReadFileBytes::Blob(blob) => Self::Blob(blob.raw()),
        }
    }
}

impl From<Stability> for wit_types::Stability {
    fn from(stability: Stability) -> Self {
        match stability {
            Stability::Stable => Self::Stable,
            Stability::Dynamic => Self::Dynamic,
            Stability::Live => Self::Live,
        }
    }
}

impl From<FileAttrs> for wit_types::FileAttrs {
    fn from(attrs: FileAttrs) -> Self {
        Self {
            size: attrs.size.into(),
            stability: attrs.stability.into(),
            version_token: attrs.version.map(|version| version.0),
        }
    }
}

impl From<FileProj> for wit_types::FileOut {
    fn from(file: FileProj) -> Self {
        Self {
            content_type: file
                .content_type
                .map(|content_type| content_type.as_mime().to_string()),
            attrs: file.attrs.into(),
            bytes: file.bytes.into(),
        }
    }
}
