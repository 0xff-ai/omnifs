//! Typestate projections (ADR-0001 §7, §10): what handlers return.
//!
//! [`FileProjection`] is the author-facing file projection. Its byte-source
//! constructor fixes a typestate marker (`Inline`/`Body`/`Full`/`Ranged`/
//! `Blob`/`Deferred`) that gates which finishers are legal: `.volatile()`
//! exists only on a `Ranged` source, and a `Deferred` source cannot `.build()`
//! until its read mode is chosen. The illegal §7 cells (`Volatile+Inline`,
//! `Volatile+Full`, deferred-without-read-mode) are therefore unrepresentable.
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

use crate::browse::{Effects, Entry as BrowseEntry, FileContent, Listing};
use crate::error::{ProviderError, Result};
use crate::file_attrs::{FileAttrs, FileProj, ProjBytes, ReadMode, Size, Stability, VersionToken};
use crate::handler::{Cursor, RangeReader};
use crate::identity::LogicalId;
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

impl FileProjection {
    /// `Bytes::Inline`, capped at 64 KiB. The implied size is the byte length.
    /// Suitable for a dir entry or a `project`-effect preload.
    pub fn inline(bytes: impl Into<Vec<u8>>) -> FileProjBuilder<Inline> {
        let bytes = bytes.into();
        let size = Size::Exact(u64::try_from(bytes.len()).unwrap_or(u64::MAX));
        FileProjBuilder::new(FileSource::Inline(bytes), size)
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
    /// may be `.volatile()`.
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

    pub fn extra_files(&self) -> &[(String, FileProjection)] {
        &self.extra_files
    }

    /// Lower the carried sibling preloads onto the browse [`Effects`]
    /// `project_file` channel, mirroring [`DirProjection::project_effects`], so
    /// a `#[file]` handler's [`Self::preload_file`] siblings are cached alongside
    /// the read. A sibling whose source has no `FileProj` lowering (`Body`/
    /// `Ranged`/`Blob`) is skipped; the host serves those through their own read.
    pub fn project_effects(&self) -> Result<Effects> {
        let mut effects = Effects::new();
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
    pub fn as_file_proj(&self) -> Option<FileProj> {
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

    /// Inline projection assembled from a [`FileContent`]'s size, stability,
    /// version evidence, and content type. Errors when the content bytes are
    /// not inline or when the stability is `Volatile` (volatile requires a
    /// ranged source, not an inline one).
    pub fn from_content(content: &FileContent) -> Result<FileProjection> {
        let attrs = content.attrs().clone();
        let content_type = content.content_type();
        let bytes = content
            .content()
            .ok_or_else(|| ProviderError::internal("list preload cannot project non-inline bytes"))?
            .to_vec();
        let mut builder = FileProjection::inline(bytes).size(attrs.size);
        builder = match attrs.stability {
            Stability::Immutable => builder.immutable(),
            Stability::Mutable => builder.mutable(),
            Stability::Volatile => {
                return Err(ProviderError::internal(
                    "list preload cannot project volatile inline bytes",
                ));
            },
        };
        if let Some(version) = attrs.version {
            builder = builder.version(version);
        }
        if let Some(ct) = content_type {
            builder = builder.content_type(ct);
        }
        Ok(builder.build())
    }

    /// Lower this projection to a browse [`crate::browse::FileContent`] terminal.
    pub fn into_browse_content(&self) -> Result<crate::browse::FileContent> {
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

// Byte-source typestate markers. Only `Ranged` is `Volatile`-eligible; only
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
    extra_files: Vec<(String, FileProjection)>,
    _src: core::marker::PhantomData<Src>,
}

impl<Src> FileProjBuilder<Src> {
    fn new(source: FileSource, size: Size) -> Self {
        Self {
            source,
            attrs: FileAttrs::new(size, Stability::Immutable),
            content_type: None,
            extra_files: Vec::new(),
            _src: core::marker::PhantomData,
        }
    }

    fn retag<New>(self) -> FileProjBuilder<New> {
        FileProjBuilder {
            source: self.source,
            attrs: self.attrs,
            content_type: self.content_type,
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
    pub fn immutable(mut self) -> Self {
        self.attrs.stability = Stability::Immutable;
        self
    }

    #[must_use]
    pub fn mutable(mut self) -> Self {
        self.attrs.stability = Stability::Mutable;
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

// §7: `Volatile` requires a ranged source. `inline(..).volatile()` does not
// compile.
impl FileProjBuilder<Ranged> {
    /// Mark the projection live: its bytes may change between reads. Only a
    /// ranged source is `Volatile`-eligible (§7).
    ///
    /// `.volatile()` does not exist on a non-ranged source, so the following
    /// must fail to compile:
    ///
    /// ```compile_fail
    /// use omnifs_sdk::projection::FileProjection;
    /// let _ = FileProjection::inline(b"x".to_vec()).volatile().build();
    /// ```
    #[must_use]
    pub fn volatile(mut self) -> Self {
        self.attrs.stability = Stability::Volatile;
        self
    }
}

impl<Src: Buildable> FileProjBuilder<Src> {
    pub fn build(self) -> FileProjection {
        FileProjection {
            source: self.source,
            attrs: self.attrs,
            content_type: self.content_type,
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
/// The enumerated variants retain the [`Entry`] values rather than
/// pre-lowering to [`Listing`], so the per-entry SDK content type survives for
/// the router. [`Self::to_listing`] performs the browse lowering on demand.
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
pub enum DirOutcome {
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

    pub fn outcome(&self) -> &DirOutcome {
        &self.outcome
    }

    pub fn validator(&self) -> Option<&VersionToken> {
        self.validator.as_ref()
    }

    pub fn extra_files(&self) -> &[(String, FileProjection)] {
        &self.extra_files
    }

    /// Lower the enumerated entries onto a browse [`Listing`]. Returns `None`
    /// for the [`DirOutcome::Unchanged`] sentinel (no entries to lower). The
    /// per-entry SDK content type is dropped in this lowering; the router reads
    /// it from [`DirOutcome::Entries`] to populate `file-proj.content-type`.
    pub fn to_listing(&self) -> Option<Listing> {
        match &self.outcome {
            DirOutcome::Entries {
                entries,
                exhaustive,
                ..
            } => {
                let browse = entries.iter().map(Entry::to_browse_entry);
                Some(if *exhaustive {
                    Listing::complete(browse)
                } else {
                    Listing::partial(browse)
                })
            },
            DirOutcome::Unchanged => None,
        }
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
/// lowers to a default deferred-Full-Immutable file projection. An optional content type types a
/// bare-name leaf the host suffix map cannot.
pub struct Entry {
    name: String,
    kind: EntryKind,
    content_type: Option<ContentType>,
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
            content_type: None,
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
            content_type: None,
        }
    }

    /// Type an SDK-typed leaf so the host echoes its content type opaquely.
    #[must_use]
    pub fn content_type(mut self, ct: ContentType) -> Self {
        self.content_type = Some(ct);
        self
    }

    pub fn name(&self) -> &str {
        &self.name
    }

    pub fn kind(&self) -> EntryKind {
        self.kind
    }

    pub fn declared_content_type(&self) -> Option<ContentType> {
        self.content_type
    }

    /// Lower to a v1 browse [`Entry`]. A file lowers to the default
    /// deferred-Full-Immutable projection; the per-entry SDK content type does
    /// not survive this lowering (the browse [`FileProj`] has no content-type
    /// field), so the router reads it from [`Entry::declared_content_type`].
    pub fn to_browse_entry(&self) -> BrowseEntry {
        match self.kind {
            EntryKind::Dir => BrowseEntry::dir(&self.name),
            EntryKind::File => BrowseEntry::file(&self.name, FileProj::listing_shape()),
        }
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
