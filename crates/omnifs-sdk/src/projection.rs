//! Typestate projections (ADR-0001 §7, §10): what handlers return.
//!
//! [`FileProjection`] is the author-facing file projection. Its byte-source
//! constructor fixes a typestate marker (`Inline`/`Body`/`Full`/`Ranged`/
//! `Blob`/`Deferred`) that gates which finishers are legal: `.live()`
//! exists only on a `Ranged` source, and a `Deferred` source cannot `.build()`
//! until its read mode is chosen. The illegal §7 cells (`Live+Inline`,
//! `Live+Full`, deferred-without-read-mode) are therefore unrepresentable.
//!
//! [`DirProjection`] is the author-facing directory listing. It lowers onto
//! [`crate::browse::Listing`]/[`crate::browse::Effects`]; the router applies
//! the carried validator, cursor, and extra-file preloads when it forms the
//! WIT terminal.
//!
//! Projections describe; the host stores. A handler never caches: it attaches
//! preloads ([`FileProjection::preload_file`], [`DirProjection::preload_file`],
//! [`DirProjection::preload_dir`], [`DirProjection::store_canonical`]) and the
//! host decides what to keep, evict, and invalidate.

use crate::browse::{Effects, Entry as BrowseEntry};
use crate::error::{ProviderError, Result};
use crate::file_attrs::{FileAttrs, FileProj, ProjBytes, ReadMode, Size, Stability, VersionToken};
use crate::handler::{Cursor, RangeReader};
use crate::identity::LogicalId;
use crate::repr::Format;
use omnifs_core::ContentType;
use std::rc::Rc;

// ===========================================================================
// File projection
// ===========================================================================

/// A file projection: byte attributes plus an optional SDK-supplied content
/// type and sibling preload files. Built through [`FileProjBuilder`].
pub struct FileProjection {
    source: FileSource,
    attrs: FileAttrs,
    content_type: Option<ContentType>,
    effects: Effects,
    extra_files: Vec<(String, FileProjection)>,
}

/// The byte source backing a [`FileProjection`].
///
/// `Inline` is a capped (<=64 KiB) preload body suitable for a dir entry or a
/// `project` effect. `Body` is the uncapped `read-file` response body (the Q-g
/// split: a read response carries no 64 KiB cap). `Deferred` reads on demand
/// (`Full` or `Ranged`). `Ranged` is served by the SDK's range session, and
/// `Blob` is served by the host from a handle.
pub enum FileSource {
    /// Capped inline preload bytes (validated <=64 KiB at build time).
    Inline(Vec<u8>),
    /// Uncapped `read-file` response body.
    Body(Vec<u8>),
    /// Read on demand with the chosen read mode.
    Deferred(ReadMode),
    /// Served by an SDK range reader.
    Ranged(Rc<dyn RangeReader>),
    /// Served by the host from a blob handle.
    Blob(crate::blob::BlobId),
}

/// How [`FileProjection::text`] treats the trailing newline of its bytes.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum TextFormat {
    /// Emit the bytes verbatim.
    Raw,
    /// Append a trailing newline if absent, so the file ends in one like a
    /// normal POSIX text file. A scalar field rendered as a leaf usually wants
    /// this so `cat` output is not glued to the next prompt.
    Newline,
}

impl FileProjection {
    /// `Bytes::Inline`, capped at 64 KiB. The implied size is the byte length.
    /// Suitable for a dir entry or a `project`-effect preload.
    pub fn inline(bytes: impl Into<Vec<u8>>) -> FileProjBuilder<Inline> {
        let bytes = bytes.into();
        let size = Size::Exact(u64::try_from(bytes.len()).unwrap_or(u64::MAX));
        FileProjBuilder::new(FileSource::Inline(bytes), size)
    }

    /// An inline `text/plain` leaf: [`Self::inline`] with the content type set.
    /// The common shape for projecting a scalar field as a small text file.
    /// `format` controls the trailing newline (see [`TextFormat`]). Returns the
    /// builder so the caller resolves stability with `.build()` (stable) or
    /// `.dynamic().build()` (a mutable upstream field).
    pub fn text(bytes: impl Into<Vec<u8>>, format: TextFormat) -> FileProjBuilder<Inline> {
        let mut bytes = bytes.into();
        if format == TextFormat::Newline && !bytes.ends_with(b"\n") {
            bytes.push(b'\n');
        }
        Self::inline(bytes).content_type(ContentType::Text)
    }

    /// The uncapped `read-file` response body. Unlike [`Self::inline`] this is
    /// not subject to the 64 KiB preload cap, because it is the materialized
    /// answer to a read rather than a preload.
    pub fn body(bytes: impl Into<Vec<u8>>) -> FileProjBuilder<Body> {
        let bytes = bytes.into();
        let size = Size::Exact(u64::try_from(bytes.len()).unwrap_or(u64::MAX));
        FileProjBuilder::new(FileSource::Body(bytes), size)
    }

    /// Read on demand; resolve the read mode with `.full()` or `.ranged()`
    /// before `.build()`.
    pub fn deferred(size: Size) -> FileProjBuilder<Deferred> {
        // Read mode is provisional until `.full()`/`.ranged()` resolves it; the
        // `Deferred` marker withholds `Buildable` until then.
        FileProjBuilder::new(FileSource::Deferred(ReadMode::Full), size)
    }

    /// Live or large; served by the ranged session path. The only source that
    /// may be `.live()`.
    pub fn ranged(reader: impl RangeReader + 'static) -> FileProjBuilder<Ranged> {
        FileProjBuilder::new(FileSource::Ranged(Rc::new(reader)), Size::Unknown)
    }

    /// Host-served by handle; the bytes never cross to the provider.
    pub fn blob(blob: crate::blob::BlobId) -> FileProjBuilder<Blob> {
        FileProjBuilder::new(FileSource::Blob(blob), Size::Unknown)
    }

    pub fn source(&self) -> &FileSource {
        &self.source
    }

    pub fn attrs(&self) -> &FileAttrs {
        &self.attrs
    }

    pub fn content_type(&self) -> Option<ContentType> {
        self.content_type
    }

    /// Attach host effects to this file read result. Use this when the file
    /// handler fetched more than the returned bytes and can safely preload or
    /// invalidate related host state in the same successful operation.
    #[must_use]
    pub fn with_effects(mut self, effects: Effects) -> Self {
        self.effects.extend(effects);
        self
    }

    /// Lower the carried sibling preloads onto the browse [`Effects`]
    /// `project_file` channel, mirroring [`DirProjection::project_effects`], so
    /// a `#[file]` handler's [`Self::preload_file`] siblings are cached alongside
    /// the read. A sibling whose source has no `FileProj` lowering (`Body`/
    /// `Ranged`/`Blob`) is skipped; the host serves those through their own read.
    pub fn project_effects(&self) -> Result<Effects> {
        let mut effects = self.effects.clone();
        for (path, file) in &self.extra_files {
            if let Some(proj) = file.as_file_proj() {
                effects.project_file(path, proj)?;
            }
        }
        Ok(effects)
    }

    /// The capped inline/deferred `FileProj` for sources the dir-entry and
    /// `project`-effect channels accept (`Inline`, `Deferred`). `Body`, `Ranged`,
    /// and `Blob` have no `FileProj` lowering and return `None`; the router
    /// serves them through `read-file`/range/blob terminals instead.
    pub(crate) fn as_file_proj(&self) -> Option<FileProj> {
        let bytes = match &self.source {
            FileSource::Inline(bytes) => ProjBytes::Inline(bytes.clone()),
            FileSource::Deferred(read) => ProjBytes::Deferred { read: *read },
            FileSource::Body(_) | FileSource::Ranged(_) | FileSource::Blob(_) => return None,
        };
        Some(FileProj {
            attrs: self.attrs.clone(),
            bytes,
            content_type: self.content_type,
        })
    }

    /// Lower this projection to a browse [`crate::browse::FileContent`] terminal.
    pub(crate) fn to_browse_content(&self) -> Result<crate::browse::FileContent> {
        let ct = self.content_type();
        let attrs = self.attrs().clone();
        let content = match self.source() {
            FileSource::Inline(bytes) | FileSource::Body(bytes) => {
                crate::browse::FileContent::new(bytes.clone()).with_attrs(attrs)
            },
            FileSource::Blob(blob) => crate::browse::FileContent::blob(*blob).with_attrs(attrs),
            FileSource::Deferred(_) => {
                return Err(ProviderError::not_found(
                    "deferred file source cannot answer a read directly",
                ));
            },
            FileSource::Ranged(_) => {
                return Err(ProviderError::unimplemented(
                    "ranged read-file is reserved but not wired through the router",
                ));
            },
        };
        let content = match ct {
            Some(ct) => content.with_content_type(ct),
            None => content,
        };
        Ok(content.with_effects(self.project_effects()?))
    }
}

// Byte-source typestate markers. Only `Ranged` is `Live`-eligible; only
// `Buildable` states may `.build()`, so a bare `Deferred` cannot.
pub struct Inline;
pub struct Body;
pub struct Full;
pub struct Ranged;
pub struct Blob;
pub struct Deferred;

/// Source states that may be finished with `.build()`. `Deferred` is absent: a
/// deferred source must resolve its read mode first.
pub trait Buildable {}
impl Buildable for Inline {}
impl Buildable for Body {}
impl Buildable for Full {}
impl Buildable for Ranged {}
impl Buildable for Blob {}

/// Typestate builder for a [`FileProjection`]. `Src` fixes the byte-source
/// state and thereby the legal finishers.
pub struct FileProjBuilder<Src> {
    source: FileSource,
    attrs: FileAttrs,
    content_type: Option<ContentType>,
    effects: Effects,
    extra_files: Vec<(String, FileProjection)>,
    _src: core::marker::PhantomData<Src>,
}

impl<Src> FileProjBuilder<Src> {
    fn new(source: FileSource, size: Size) -> Self {
        Self {
            source,
            attrs: FileAttrs::new(size, Stability::Stable),
            content_type: None,
            effects: Effects::new(),
            extra_files: Vec::new(),
            _src: core::marker::PhantomData,
        }
    }

    fn retag<New>(self) -> FileProjBuilder<New> {
        FileProjBuilder {
            source: self.source,
            attrs: self.attrs,
            content_type: self.content_type,
            effects: self.effects,
            extra_files: self.extra_files,
            _src: core::marker::PhantomData,
        }
    }

    #[must_use]
    pub fn size(mut self, size: Size) -> Self {
        self.attrs.size = size;
        self
    }

    #[must_use]
    pub fn stable(mut self) -> Self {
        self.attrs.stability = Stability::Stable;
        self
    }

    #[must_use]
    pub fn dynamic(mut self) -> Self {
        self.attrs.stability = Stability::Dynamic;
        self
    }

    #[must_use]
    pub fn version(mut self, v: impl Into<VersionToken>) -> Self {
        self.attrs.version = Some(v.into());
        self
    }

    /// SDK-supplied content type for a bare-name leaf the host suffix map
    /// cannot type.
    #[must_use]
    pub fn content_type(mut self, ct: ContentType) -> Self {
        self.content_type = Some(ct);
        self
    }

    /// Preload a sibling file alongside this projection (the `project` effect).
    #[must_use]
    pub fn preload_file(mut self, path: impl Into<String>, file: FileProjection) -> Self {
        self.extra_files.push((path.into(), file));
        self
    }

    /// Attach host effects to the eventual file read result.
    #[must_use]
    pub fn with_effects(mut self, effects: Effects) -> Self {
        self.effects.extend(effects);
        self
    }
}

// A `Deferred` source must resolve its read mode before it is `Buildable`.
impl FileProjBuilder<Deferred> {
    pub fn full(mut self) -> FileProjBuilder<Full> {
        self.source = FileSource::Deferred(ReadMode::Full);
        self.retag()
    }

    pub fn ranged(mut self) -> FileProjBuilder<Ranged> {
        self.source = FileSource::Deferred(ReadMode::Ranged);
        self.retag()
    }
}

// §7: `Live` requires a ranged source. `inline(..).live()` does not
// compile.
impl FileProjBuilder<Ranged> {
    /// Mark the projection live: its bytes may change between reads. Only a
    /// ranged source is `Live`-eligible (§7).
    ///
    /// `.live()` does not exist on a non-ranged source, so the following
    /// must fail to compile:
    ///
    /// ```compile_fail
    /// use omnifs_sdk::projection::FileProjection;
    /// let _ = FileProjection::inline(b"x".to_vec()).live().build();
    /// ```
    #[must_use]
    pub fn live(mut self) -> Self {
        self.attrs.stability = Stability::Live;
        self
    }
}

impl<Src: Buildable> FileProjBuilder<Src> {
    pub fn build(self) -> FileProjection {
        FileProjection {
            source: self.source,
            attrs: self.attrs,
            content_type: self.content_type,
            effects: self.effects,
            extra_files: self.extra_files,
        }
    }
}

// ===========================================================================
// Directory projection
// ===========================================================================

/// A directory listing. `exhaustive` enumerates every child; `open` is
/// capped/non-exhaustive with no cursor; `paged` is resumable; `unchanged` is
/// the listing analog of [`crate::object::Load::Unchanged`] (the cached
/// validator matched, so the host serves its cached dirents).
///
pub struct DirProjection {
    outcome: DirOutcome,
    validator: Option<VersionToken>,
    extra_files: Vec<(String, FileProjection)>,
    /// Preloaded directory entries: `(path, exhaustive)`. Lowered to the
    /// `project_dir`/`project_dir_exhaustive` effect so a listing can declare
    /// that a child is a directory (and optionally that the files written
    /// beneath it in the same batch are its authoritative listing).
    extra_dirs: Vec<(String, bool)>,
    /// Canonical bytes to store at object anchors alongside a parent listing.
    extra_canonical: Vec<ExtraCanonical>,
}

/// A canonical-store entry a directory listing carries for an object it
/// preloaded: the logical id, validator, canonical bytes, and full view leaves.
struct ExtraCanonical {
    id: LogicalId,
    validator: Option<VersionToken>,
    bytes: Vec<u8>,
    leaves: Vec<String>,
}

/// The listing outcome a [`DirProjection`] carries. `Unchanged` is a sentinel
/// the router maps to the WIT `list-children-result::unchanged`; the
/// `Entries` variant carries the entries, an exhaustiveness flag, and an
/// optional resume cursor.
pub(crate) enum DirOutcome {
    Entries {
        entries: Vec<Entry>,
        exhaustive: bool,
        cursor: Option<Cursor>,
    },
    Unchanged,
}

impl DirProjection {
    /// Every child enumerated: "these are all the names I am aware of."
    /// Even so, lookup remains the authoritative name oracle and may
    /// resolve names this listing omitted; exhaustive is a claim about the
    /// enumeration, not a promise of future misses.
    pub fn exhaustive(entries: impl IntoIterator<Item = Entry>) -> Self {
        Self::from_entries(entries.into_iter().collect(), true, None)
    }

    /// Deliberately partial with no cursor: the directory is unbounded or
    /// expensive to enumerate (reverse-DNS, an unbounded id space) and the
    /// caller navigates by lookup rather than by listing. Use [`Self::paged`]
    /// instead when the rest is reachable and worth fetching.
    pub fn open(entries: impl IntoIterator<Item = Entry>) -> Self {
        Self::from_entries(entries.into_iter().collect(), false, None)
    }

    /// Resumable: a partial page plus an opaque cursor. The host echoes the
    /// cursor back as the `cursor` argument of the next `list_children` on
    /// this path ([`crate::handler::DirIntent::List`]); the handler decodes
    /// it and continues until a page comes back without one.
    pub fn paged(entries: impl IntoIterator<Item = Entry>, cursor: Cursor) -> Self {
        Self::from_entries(entries.into_iter().collect(), false, Some(cursor))
    }

    /// The cached validator matched: the host serves its cached dirents and the
    /// handler enumerated nothing.
    pub fn unchanged() -> Self {
        Self {
            outcome: DirOutcome::Unchanged,
            validator: None,
            extra_files: Vec::new(),
            extra_dirs: Vec::new(),
            extra_canonical: Vec::new(),
        }
    }

    fn from_entries(entries: Vec<Entry>, exhaustive: bool, cursor: Option<Cursor>) -> Self {
        Self {
            outcome: DirOutcome::Entries {
                entries,
                exhaustive,
                cursor,
            },
            validator: None,
            extra_files: Vec::new(),
            extra_dirs: Vec::new(),
            extra_canonical: Vec::new(),
        }
    }

    /// Record the validator the host echoes on the next `list-children` for a
    /// cheap re-list.
    #[must_use]
    pub fn with_validator(mut self, validator: impl Into<VersionToken>) -> Self {
        self.validator = Some(validator.into());
        self
    }

    /// Preload a child file alongside the listing (the `project` effect).
    /// `path` may be mount-relative or absolute; see [`join_preload_path`].
    #[must_use]
    pub fn preload_file(mut self, path: impl Into<String>, file: FileProjection) -> Self {
        self.extra_files.push((path.into(), file));
        self
    }

    /// Preload a child directory and merge `child`'s listing-time effects under
    /// `path`. Relative paths on `child` are joined to `path`; absolute paths
    /// are left unchanged. The child's [`DirOutcome::Entries::exhaustive`]
    /// flag selects `project_dir` vs `project_dir_exhaustive` for `path`.
    /// Enumerated entries on `child` are not merged into this listing.
    #[must_use]
    pub fn preload_dir(mut self, path: impl Into<String>, child: DirProjection) -> Self {
        let path = path.into();
        let exhaustive = match child.outcome() {
            DirOutcome::Entries { exhaustive, .. } => *exhaustive,
            DirOutcome::Unchanged => false,
        };
        self.extra_dirs.push((path.clone(), exhaustive));
        for (subdir, subdir_exhaustive) in child.extra_dirs {
            self.extra_dirs
                .push((join_preload_path(&path, &subdir), subdir_exhaustive));
        }
        for (file_path, file) in child.extra_files {
            self.extra_files
                .push((join_preload_path(&path, &file_path), file));
        }
        self.extra_canonical.extend(child.extra_canonical);
        self
    }

    /// Store canonical bytes at an object anchor so later reads of identity or
    /// rendered representations can reuse the listing fetch. `leaves` are the
    /// anchor-relative remainders the host will index for this anchor.
    #[must_use]
    pub fn store_canonical(
        mut self,
        id: LogicalId,
        validator: Option<VersionToken>,
        bytes: Vec<u8>,
        leaves: Vec<String>,
    ) -> Self {
        self.extra_canonical.push(ExtraCanonical {
            id,
            validator,
            bytes,
            leaves,
        });
        self
    }

    pub(crate) fn outcome(&self) -> &DirOutcome {
        &self.outcome
    }

    pub fn validator(&self) -> Option<&VersionToken> {
        self.validator.as_ref()
    }

    /// Lower the carried extra-file preloads onto the browse [`Effects`]
    /// `project_file` channel. Sources without a `FileProj` lowering (`Body`,
    /// `Ranged`, `Blob`) are skipped; the router serves them through their own
    /// terminals.
    pub fn project_effects(&self) -> Result<Effects> {
        let mut effects = Effects::new();
        for (path, exhaustive) in &self.extra_dirs {
            if *exhaustive {
                effects.project_dir_exhaustive(path)?;
            } else {
                effects.project_dir(path)?;
            }
        }
        for (path, file) in &self.extra_files {
            if let Some(proj) = file.as_file_proj() {
                effects.project_file(path, proj)?;
            }
        }
        for ec in &self.extra_canonical {
            effects.canonical_store(
                &ec.id,
                ec.validator.clone(),
                ec.bytes.clone(),
                ec.leaves.clone(),
            );
        }
        Ok(effects)
    }
}

/// A directory entry: a bare name plus a kind. Unlike the v1
/// [`crate::browse::Entry`], a file entry carries no [`FileProj`] argument; it
/// lowers to a default deferred-Full-Stable file projection.
pub struct Entry {
    name: String,
    kind: EntryKind,
}

/// The kind of a [`Entry`].
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum EntryKind {
    Dir,
    File,
}

impl Entry {
    /// A directory child; its contents come from whatever route matches the
    /// child path when it is later listed.
    pub fn dir(name: impl Into<String>) -> Self {
        Self {
            name: name.into(),
            kind: EntryKind::Dir,
        }
    }

    /// A file child with the default deferred projection: `stat` works
    /// immediately, bytes load on the first read through the child's own
    /// route. To ship the bytes along with the listing, pair the entry with
    /// [`DirProjection::preload_file`] for the same name.
    pub fn file(name: impl Into<String>) -> Self {
        Self {
            name: name.into(),
            kind: EntryKind::File,
        }
    }

    pub fn name(&self) -> &str {
        &self.name
    }

    pub fn kind(&self) -> EntryKind {
        self.kind
    }

    /// Lower to a v1 browse [`Entry`]. A file lowers to the default
    /// deferred-Full-Stable projection.
    pub(crate) fn to_browse_entry(&self) -> BrowseEntry {
        match self.kind {
            EntryKind::Dir => BrowseEntry::dir(&self.name),
            EntryKind::File => BrowseEntry::file(&self.name, FileProj::listing_shape()),
        }
    }
}

// ===========================================================================
// Object face value types (blob / stream)
// ===========================================================================

/// A host-resident blob file, returned by an object `blob` face. The bytes
/// stay host-side; only a [`crate::blob::BlobId`] handle crosses back. The
/// content type defaults to `F::CT` and can be overridden.
pub struct BlobFile<F: Format> {
    id: crate::blob::BlobId,
    size: Size,
    stability: Stability,
    version: Option<VersionToken>,
    content_type: Option<ContentType>,
    _format: core::marker::PhantomData<F>,
}

impl<F: Format> BlobFile<F> {
    /// A blob file with content type `F::CT`, unknown size, and
    /// `Stability::Stable`.
    pub fn new(id: crate::blob::BlobId) -> Self {
        Self {
            id,
            size: Size::Unknown,
            stability: Stability::Stable,
            version: None,
            content_type: None,
            _format: core::marker::PhantomData,
        }
    }

    /// The exact blob size, so `stat` is honest before the first read.
    #[must_use]
    pub fn size(mut self, size: Size) -> Self {
        self.size = size;
        self
    }

    /// Declare the blob's [`Stability`] (defaults to `Stable`). A blob whose
    /// upstream bytes can change under a stable handle (a `@latest`-style alias)
    /// is `Dynamic`.
    #[must_use]
    pub fn stability(mut self, stability: Stability) -> Self {
        self.stability = stability;
        self
    }

    /// Shorthand for `stability(Stability::Stable)`.
    #[must_use]
    pub fn stable(self) -> Self {
        self.stability(Stability::Stable)
    }

    /// Shorthand for `stability(Stability::Dynamic)`.
    #[must_use]
    pub fn dynamic(self) -> Self {
        self.stability(Stability::Dynamic)
    }

    #[must_use]
    pub fn version(mut self, version: impl Into<VersionToken>) -> Self {
        self.version = Some(version.into());
        self
    }

    /// Override the content type (defaults to `F::CT`).
    #[must_use]
    pub fn content_type(mut self, content_type: ContentType) -> Self {
        self.content_type = Some(content_type);
        self
    }

    /// Lower to the [`FileProjection`] the router serves through the blob
    /// terminal.
    pub(crate) fn into_projection(self) -> FileProjection {
        let ct = self.content_type.unwrap_or(F::CT);
        let mut builder = FileProjection::blob(self.id)
            .size(self.size)
            .content_type(ct);
        // A blob is host-served (not a ranged source), so it cannot be `Live`;
        // `BlobFile` exposes only stable/dynamic. Anything that is not stable
        // lowers to dynamic.
        builder = match self.stability {
            Stability::Stable => builder.stable(),
            Stability::Dynamic | Stability::Live => builder.dynamic(),
        };
        if let Some(version) = self.version {
            builder = builder.version(version);
        }
        builder.build()
    }
}

/// A ranged byte stream, returned by an object `stream` face: a range reader
/// plus declared size, stability, and content type. The only face that may be
/// `Live`.
pub struct StreamFile {
    reader: Rc<dyn RangeReader>,
    size: Size,
    stability: Stability,
    content_type: Option<ContentType>,
    version: Option<VersionToken>,
}

impl StreamFile {
    /// A stream over `reader` with unknown size, `Stability::Dynamic`, and no
    /// content type.
    pub fn new(reader: impl RangeReader + 'static) -> Self {
        Self {
            reader: Rc::new(reader),
            size: Size::Unknown,
            stability: Stability::Dynamic,
            content_type: None,
            version: None,
        }
    }

    #[must_use]
    pub fn size(mut self, size: Size) -> Self {
        self.size = size;
        self
    }

    /// Mark the stream live: its bytes may change while observed (`tail -f`).
    #[must_use]
    pub fn live(mut self) -> Self {
        self.stability = Stability::Live;
        self
    }

    #[must_use]
    pub fn stability(mut self, stability: Stability) -> Self {
        self.stability = stability;
        self
    }

    #[must_use]
    pub fn content_type(mut self, content_type: ContentType) -> Self {
        self.content_type = Some(content_type);
        self
    }

    #[must_use]
    pub fn version(mut self, version: impl Into<VersionToken>) -> Self {
        self.version = Some(version.into());
        self
    }

    /// The attrs an open session reports.
    pub fn attrs(&self) -> FileAttrs {
        let attrs = FileAttrs::new(self.size.clone(), self.stability);
        match &self.version {
            Some(version) => attrs.with_version(version.clone()),
            None => attrs,
        }
    }

    /// The reader serving chunks.
    pub fn reader(&self) -> Rc<dyn RangeReader> {
        self.reader.clone()
    }
}

impl<R: RangeReader + 'static> From<R> for StreamFile {
    fn from(reader: R) -> Self {
        Self::new(reader)
    }
}

/// Join a parent preload path with a child path. Absolute child paths pass through.
fn join_preload_path(base: &str, leaf: &str) -> String {
    if leaf.starts_with('/') {
        return leaf.to_string();
    }
    let base = base.trim_end_matches('/');
    format!("{base}/{leaf}")
}
