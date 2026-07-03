//! Wire-facing browse results and the [`Effects`] channel.
//!
//! These types sit between the provider-facing projection layer
//! ([`crate::projection::DirProjection`], [`crate::projection::FileProjection`])
//! and the generated WIT protocol; the router lowers handler returns onto
//! them. Most provider code only meets [`Effects`], [`EntryKind`], and
//! [`ReadOutcome`] (re-exported in the prelude); reach for [`Lookup`],
//! [`Listing`], or [`FileContent`] directly only when building custom
//! object leaves or lowering glue.
//!
//! [`Effects`] is the part worth internalizing: it is the only channel
//! through which a provider mutates host state. Everything else here is a
//! terminal answer to the one operation in flight.

use crate::error::{ProviderError, Result};
use crate::file_attrs::{FileAttrs, FileProj, ReadFileBytes, Size, Stability, VersionToken};
use crate::identity::LogicalId;
use omnifs_core::path::Path;
use omnifs_wit::provider::types as wit_types;

/// A host-pushed canonical object on the read path. The host resolves a
/// view miss to its anchor and pushes the cached canonical bytes so the SDK
/// renders without an upstream call. `id` is compared structurally against
/// the route-derived anchor via [`LogicalId::matches_wire`] (no reconstruction).
pub struct CachedCanonical {
    pub id: wit_types::LogicalId,
    pub bytes: Vec<u8>,
    pub validator: Option<VersionToken>,
    pub revalidate: bool,
}

impl CachedCanonical {
    pub fn from_wit(input: wit_types::CanonicalInput) -> Self {
        Self {
            id: input.id,
            bytes: input.bytes,
            validator: input.validator.map(VersionToken::from),
            revalidate: input.revalidate,
        }
    }

    pub fn matches_anchor(&self, anchor: &LogicalId) -> bool {
        anchor.matches_wire(&self.id)
    }
}

/// Lightweight entry classification used by route tables and tests.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum EntryKind {
    Directory,
    File,
}

/// A wire-level dirent: a named file or directory, where file entries carry
/// a [`FileProj`] (lowering to `DirEntry` panics without one). Handlers
/// normally build [`crate::projection::Entry`] instead, which fills in
/// [`FileProj::listing_shape`] for plain file names.
#[derive(Clone, Debug)]
pub struct Entry {
    name: String,
    kind: EntryKind,
    file: Option<FileProj>,
    logical_id: Option<LogicalId>,
}

impl Entry {
    pub fn dir(name: impl Into<String>) -> Self {
        Self {
            name: name.into(),
            kind: EntryKind::Directory,
            file: None,
            logical_id: None,
        }
    }

    pub fn file(name: impl Into<String>, file: FileProj) -> Self {
        Self {
            name: name.into(),
            kind: EntryKind::File,
            file: Some(file),
            logical_id: None,
        }
    }

    pub fn name(&self) -> &str {
        &self.name
    }

    pub fn kind(&self) -> EntryKind {
        self.kind
    }

    pub fn attrs(&self) -> Option<&FileAttrs> {
        self.file.as_ref().map(|file| &file.attrs)
    }
}

impl From<Entry> for wit_types::DirEntry {
    fn from(entry: Entry) -> Self {
        Self {
            name: entry.name,
            kind: match entry.kind {
                EntryKind::Directory => wit_types::EntryKind::Directory,
                EntryKind::File => wit_types::EntryKind::File(
                    entry
                        .file
                        .expect("file entries must carry file projection")
                        .into(),
                ),
            },
            id: entry.logical_id.map(Into::into),
        }
    }
}

/// Terminal host mutations staged with an accepted provider return: the
/// only way a provider writes host state.
///
/// Three channels, applied only if the operation succeeds:
///
/// - `canonical` ([`Self::canonical_store`]): store verbatim upstream bytes
///   against a logical object id, durable across restarts.
/// - `fs` ([`Self::project_file`], [`Self::project_dir`]): materialize
///   rendered files and directories into the view cache so later reads are
///   served without re-invoking the provider.
/// - `invalidations` ([`Self::invalidate_object`],
///   [`Self::invalidate_listing_path`], [`Self::invalidate_listing_prefix`]):
///   evict stale state, typically from event handlers. This is the
///   freshness mechanism; there are no TTLs.
///
/// Preload discipline: any resource you already hold beyond the one
/// requested travels here as extra `canonical`/`fs` entries. If a payload
/// in hand contains sibling leaves, project them now instead of forcing a
/// refetch later.
///
/// The three WIT effect blocks are kept as separate typed vecs so the
/// channels never alias; [`Self::into_wit`] assembles the WIT `effects`
/// record.
#[derive(Clone, Debug, Default)]
pub struct Effects {
    canonical: Vec<wit_types::CanonicalStore>,
    fs: Vec<wit_types::FsWrite>,
    invalidations: Vec<wit_types::Invalidation>,
}

impl Effects {
    pub fn new() -> Self {
        Self::default()
    }

    /// Materialize a directory into the view cache without claiming its
    /// listing is complete; later readdirs may still invoke the provider.
    /// Paths here and on every other `project_*` method are
    /// mount-absolute; a missing leading `/` is tolerated and prefixed.
    pub fn project_dir(&mut self, path: impl AsRef<str>) -> Result<&mut Self> {
        self.fs.push(wit_types::FsWrite {
            id: None,
            path: wire_path(path.as_ref())?.into(),
            kind: wit_types::FsKind::Directory(false),
        });
        Ok(self)
    }

    /// Project a directory whose children appearing in the same effects
    /// batch constitute the authoritative listing. The host marks the
    /// resulting dirents record exhaustive so subsequent readdirs serve
    /// from cache without re-invoking `list_children`.
    pub fn project_dir_exhaustive(&mut self, path: impl AsRef<str>) -> Result<&mut Self> {
        self.fs.push(wit_types::FsWrite {
            id: None,
            path: wire_path(path.as_ref())?.into(),
            kind: wit_types::FsKind::Directory(true),
        });
        Ok(self)
    }

    /// Materialize a file into the view cache. Validates the projection
    /// ([`FileProj::validate`]) before staging, so an illegal shape fails
    /// here rather than at the host boundary.
    pub fn project_file(&mut self, path: impl AsRef<str>, file: FileProj) -> Result<&mut Self> {
        file.validate()?;
        self.fs.push(wit_types::FsWrite {
            id: None,
            path: wire_path(path.as_ref())?.into(),
            kind: wit_types::FsKind::File(file.into()),
        });
        Ok(self)
    }

    /// Store verbatim upstream bytes against a logical object id in the
    /// durable object cache.
    ///
    /// `view_leaves` teaches the host the exact full paths that map to this
    /// id: on a later view miss at one of those paths, the host pushes
    /// these bytes back into `read-file` as the cached canonical, and the
    /// SDK re-renders with no upstream call. The host resolves paths by
    /// exact map lookup, never prefix probing, so every leaf path must be
    /// listed explicitly. Overwriting an id evicts its previously derived
    /// view leaves. Object-route registrations emit this effect
    /// automatically from [`crate::object::Key::load`] results; call it
    /// directly only in hand-rolled handlers.
    pub fn canonical_store(
        &mut self,
        id: &LogicalId,
        validator: Option<VersionToken>,
        bytes: Vec<u8>,
        view_leaves: Vec<String>,
    ) -> &mut Self {
        self.canonical.push(wit_types::CanonicalStore {
            id: id.into(),
            validator: validator.map(|v| v.0),
            bytes,
            view_leaves,
        });
        self
    }

    /// Like [`Self::project_file`], with the file tagged as a view leaf of
    /// logical object `id`, so object-level invalidation cascades to it.
    pub fn project_file_with_id(
        &mut self,
        path: impl AsRef<str>,
        id: Option<&LogicalId>,
        file: FileProj,
    ) -> Result<&mut Self> {
        file.validate()?;
        self.fs.push(wit_types::FsWrite {
            id: id.map(Into::into),
            path: wire_path(path.as_ref())?.into(),
            kind: wit_types::FsKind::File(file.into()),
        });
        Ok(self)
    }

    /// Evict an object's canonical bytes and every view leaf derived from
    /// it.
    pub fn invalidate_object(&mut self, id: &LogicalId) -> &mut Self {
        self.invalidations
            .push(wit_types::Invalidation::Object(id.into()));
        self
    }

    /// Evict the cached listing at exactly `path`. Panics if `path` is not
    /// a valid protocol path; invalidation targets are provider-authored
    /// constants, not user input.
    pub fn invalidate_listing_path(&mut self, path: impl AsRef<str>) -> &mut Self {
        let path = wire_path(path.as_ref()).expect("invalidation path must be a protocol path");
        self.invalidations.push(wit_types::Invalidation::Listing(
            wit_types::PathOrPrefix::Path(path.into()),
        ));
        self
    }

    /// Evict every cached listing under `prefix` (inclusive). Panics on an
    /// invalid protocol path, like [`Self::invalidate_listing_path`].
    pub fn invalidate_listing_prefix(&mut self, prefix: impl AsRef<str>) -> &mut Self {
        let prefix =
            wire_path(prefix.as_ref()).expect("invalidation prefix must be a protocol path");
        self.invalidations.push(wit_types::Invalidation::Listing(
            wit_types::PathOrPrefix::Prefix(prefix.into()),
        ));
        self
    }

    pub fn extend(&mut self, other: Effects) -> &mut Self {
        self.canonical.extend(other.canonical);
        self.fs.extend(other.fs);
        self.invalidations.extend(other.invalidations);
        self
    }

    pub fn is_empty(&self) -> bool {
        self.canonical.is_empty() && self.fs.is_empty() && self.invalidations.is_empty()
    }

    #[doc(hidden)]
    pub fn into_wit(self) -> wit_types::Effects {
        wit_types::Effects {
            canonical: self.canonical,
            fs: self.fs,
            invalidations: self.invalidations,
        }
    }
}

/// A directory listing with entries, exhaustiveness, and accepted-return
/// projection effects for adjacent or nested paths.
///
/// `exhaustive` means "these are all the names I know", not "no other name
/// can resolve": `lookup` remains the authoritative name oracle and may
/// resolve names absent from the latest listing. The host also merges
/// literal sibling routes registered at the same depth into what the user
/// sees, so a handler only enumerates its own dynamic children.
#[derive(Clone, Debug)]
pub struct Listing {
    entries: Vec<Entry>,
    exhaustive: bool,
    effects: Effects,
    validator: Option<String>,
    next_cursor: Option<wit_types::Cursor>,
}

impl Listing {
    /// An exhaustive listing: the host may serve later readdirs from cache
    /// without re-invoking the provider.
    pub fn complete(entries: impl IntoIterator<Item = Entry>) -> Self {
        Self {
            entries: entries.into_iter().collect(),
            exhaustive: true,
            effects: Effects::new(),
            validator: None,
            next_cursor: None,
        }
    }

    /// A non-exhaustive listing: more names may exist than were
    /// enumerated (open namespaces, paged results). Pair with
    /// [`Self::with_cursor`] when the next page is reachable.
    pub fn partial(entries: impl IntoIterator<Item = Entry>) -> Self {
        Self {
            entries: entries.into_iter().collect(),
            exhaustive: false,
            effects: Effects::new(),
            validator: None,
            next_cursor: None,
        }
    }

    #[must_use]
    pub fn with_effects(mut self, effects: Effects) -> Self {
        self.effects.extend(effects);
        self
    }

    /// Carry the opaque listing validator (e.g. an `ETag`) the host echoes as
    /// `cached-validator` on the next `list-children` for a cheap re-list.
    #[must_use]
    pub fn with_validator(mut self, validator: impl Into<String>) -> Self {
        self.validator = Some(validator.into());
        self
    }

    /// Carry the resume cursor for a paged (non-exhaustive) listing; the host
    /// echoes it back as the `cursor` argument to continue.
    #[must_use]
    pub fn with_cursor(mut self, cursor: crate::handler::Cursor) -> Self {
        self.next_cursor = Some(cursor.into());
        self
    }

    pub fn entries(&self) -> &[Entry] {
        &self.entries
    }

    pub fn exhaustive(&self) -> bool {
        self.exhaustive
    }

    pub fn effects(&self) -> &Effects {
        &self.effects
    }

    fn into_parts(self) -> (wit_types::DirListing, Effects) {
        (
            wit_types::DirListing {
                entries: self.entries.into_iter().map(Into::into).collect(),
                exhaustive: self.exhaustive,
                validator: self.validator,
                next_cursor: self.next_cursor,
            },
            self.effects,
        )
    }
}

impl From<Listing> for wit_types::DirListing {
    fn from(listing: Listing) -> Self {
        listing.into_parts().0
    }
}

/// A lookup result: a found entry with cache-adjacent sibling data, a
/// subtree handoff, or a miss.
///
/// Lookup is the authoritative name oracle: the host trusts a `NotFound`
/// here as a cacheable negative, so return it only when the name truly
/// does not exist, not on transient failure (return an error for those).
#[allow(clippy::large_enum_variant)]
#[derive(Clone, Debug)]
pub enum Lookup {
    Entry(LookupEntry),
    Subtree { tree: u64 },
    NotFound { id: Option<LogicalId> },
}

/// The non-subtree, non-miss shape of a lookup: the found entry plus
/// cache-adjacent sibling data.
#[derive(Clone, Debug)]
pub struct LookupEntry {
    target: Entry,
    siblings: Vec<Entry>,
    exhaustive: bool,
    effects: Effects,
}

impl Lookup {
    pub fn entry(target: Entry) -> Self {
        Self::Entry(LookupEntry {
            target,
            siblings: Vec::new(),
            exhaustive: true,
            effects: Effects::new(),
        })
    }

    /// Hand the child off as a host-resolved subtree (a git clone, an
    /// extracted archive). `tree` is the handle a tree-opening callout
    /// returned; dispatch below this point belongs to the host.
    pub fn subtree(tree: u64) -> Self {
        Self::Subtree { tree }
    }

    pub fn not_found() -> Self {
        Self::NotFound { id: None }
    }

    pub fn is_found(&self) -> bool {
        !matches!(self, Self::NotFound { .. })
    }

    #[must_use]
    pub fn with_effects(self, effects: Effects) -> Self {
        match self {
            Self::Entry(mut entry) => {
                entry.effects.extend(effects);
                Self::Entry(entry)
            },
            other => other,
        }
    }

    /// Attach sibling entries the answering payload already contained, so
    /// the host caches them alongside the target (preload discipline:
    /// never discard names you already fetched). No-op on subtree and
    /// not-found results.
    #[must_use]
    pub fn with_siblings<I: IntoIterator<Item = Entry>>(self, entries: I) -> Self {
        match self {
            Self::Entry(mut entry) => {
                entry.siblings.extend(entries);
                Self::Entry(entry)
            },
            other => other,
        }
    }

    /// Set whether the sibling set is exhaustive.
    ///
    /// When true (the default), the host treats absence from `siblings`
    /// as authoritative negative. Only meaningful for directory targets;
    /// ignored on subtree and not-found results.
    #[must_use]
    pub fn exhaustive(self, exhaustive: bool) -> Self {
        match self {
            Self::Entry(mut entry) => {
                entry.exhaustive = exhaustive;
                Self::Entry(entry)
            },
            other => other,
        }
    }

    pub fn target(&self) -> Option<&Entry> {
        match self {
            Self::Entry(entry) => Some(&entry.target),
            _ => None,
        }
    }

    #[doc(hidden)]
    pub fn into_result_and_effects(self) -> (wit_types::LookupChildResult, Effects) {
        match self {
            Self::Entry(entry) => (
                wit_types::LookupChildResult::Entry(wit_types::LookupEntry {
                    target: entry.target.into(),
                    siblings: entry.siblings.into_iter().map(Into::into).collect(),
                    exhaustive: entry.exhaustive,
                }),
                entry.effects,
            ),
            Self::Subtree { tree } => (wit_types::LookupChildResult::Subtree(tree), Effects::new()),
            Self::NotFound { id } => (
                wit_types::LookupChildResult::NotFound(id.map(Into::into)),
                Effects::new(),
            ),
        }
    }
}

impl From<Lookup> for wit_types::LookupChildResult {
    fn from(lookup: Lookup) -> Self {
        lookup.into_result_and_effects().0
    }
}

/// A list result: a listing, a subtree handoff, or an unchanged sentinel.
#[derive(Clone, Debug)]
pub enum List {
    Entries(Listing),
    Subtree {
        tree: u64,
    },
    /// The host's `cached-validator` still matched: the host serves its cached
    /// dirents and the provider enumerated nothing (ADR-0001 §6, listings
    /// revalidate like reads). Lowers to `list-children-result::unchanged`.
    Unchanged,
}

impl List {
    pub fn entries(listing: Listing) -> Self {
        Self::Entries(listing)
    }

    pub fn subtree(tree: u64) -> Self {
        Self::Subtree { tree }
    }

    /// The cached-validator-matched sentinel: the host reuses its cached
    /// dirents and the provider enumerated nothing.
    pub fn unchanged() -> Self {
        Self::Unchanged
    }

    #[doc(hidden)]
    pub fn into_result_and_effects(self) -> (wit_types::ListChildrenResult, Effects) {
        match self {
            Self::Entries(listing) => {
                let (listing, effects) = listing.into_parts();
                (wit_types::ListChildrenResult::Entries(listing), effects)
            },
            Self::Subtree { tree } => {
                (wit_types::ListChildrenResult::Subtree(tree), Effects::new())
            },
            Self::Unchanged => (wit_types::ListChildrenResult::Unchanged, Effects::new()),
        }
    }
}

impl From<List> for wit_types::ListChildrenResult {
    fn from(list: List) -> Self {
        list.into_result_and_effects().0
    }
}

impl From<crate::handler::Cursor> for wit_types::Cursor {
    fn from(cursor: crate::handler::Cursor) -> Self {
        match cursor {
            crate::handler::Cursor::Opaque(token) => Self::Opaque(token),
            crate::handler::Cursor::Page(page) => Self::Page(page),
        }
    }
}

impl From<wit_types::Cursor> for crate::handler::Cursor {
    fn from(cursor: wit_types::Cursor) -> Self {
        match cursor {
            wit_types::Cursor::Opaque(token) => Self::Opaque(token),
            wit_types::Cursor::Page(page) => Self::Page(page),
        }
    }
}

/// Full-read file content. Bytes are read data; adjacent projected paths must
/// be staged as effects.
#[derive(Clone, Debug)]
pub struct FileContent {
    content_type: Option<omnifs_core::ContentType>,
    attrs: FileAttrs,
    bytes: ContentBytes,
    effects: Effects,
}

/// The byte source backing a [`FileContent`] read answer. Mirrors the legal
/// `read-file` byte sources: an inline payload, a host-resident blob handle,
/// or a `canonical` reference (serve the canonical store at the read path
/// without copying bytes across the WIT). `deferred` is deliberately absent:
/// a read must answer with concrete bytes.
#[derive(Clone, Debug)]
enum ContentBytes {
    Read(ReadFileBytes),
    Canonical,
}

impl From<ContentBytes> for wit_types::ByteSource {
    fn from(bytes: ContentBytes) -> Self {
        match bytes {
            ContentBytes::Read(read) => read.into(),
            ContentBytes::Canonical => Self::Canonical,
        }
    }
}

impl FileContent {
    /// Inline bytes with default attrs `Size::Exact(len)` plus
    /// `Stability::Stable`. Override with [`Self::with_attrs`] when the
    /// content can change; the stable default licenses the host to
    /// cache it indefinitely.
    pub fn new(content: impl Into<Vec<u8>>) -> Self {
        let content = content.into();
        let size = u64::try_from(content.len()).unwrap_or(u64::MAX);
        Self {
            content_type: None,
            attrs: FileAttrs::new(Size::Exact(size), Stability::Stable),
            bytes: ContentBytes::Read(ReadFileBytes::Inline(content)),
            effects: Effects::new(),
        }
    }

    /// Serve from a host-resident blob; no bytes cross the WIT. Attrs
    /// default to `Size::Unknown` plus `Stability::Stable`: set the real
    /// size via [`Self::with_attrs`] when the blob fetch reported one, so
    /// `stat` is honest before the first read.
    pub fn blob(blob: impl Into<crate::blob::BlobId>) -> Self {
        Self {
            content_type: None,
            attrs: FileAttrs::new(Size::Unknown, Stability::Stable),
            bytes: ContentBytes::Read(ReadFileBytes::Blob(blob.into())),
            effects: Effects::new(),
        }
    }

    /// Reference the canonical store at the read path: the host serves the
    /// stored canonical bytes verbatim without copying them across the WIT.
    /// Used when the requested representation IS the canonical (the identity
    /// reference the router returns).
    pub fn canonical(attrs: FileAttrs) -> Self {
        Self {
            content_type: None,
            attrs,
            bytes: ContentBytes::Canonical,
            effects: Effects::new(),
        }
    }

    #[must_use]
    pub fn with_attrs(mut self, attrs: FileAttrs) -> Self {
        self.attrs = attrs;
        self
    }

    #[must_use]
    pub fn with_content_type(mut self, content_type: omnifs_core::ContentType) -> Self {
        self.content_type = Some(content_type);
        self
    }

    #[must_use]
    pub fn with_effects(mut self, effects: Effects) -> Self {
        self.effects.extend(effects);
        self
    }

    pub fn attrs(&self) -> &FileAttrs {
        &self.attrs
    }

    pub fn content_type(&self) -> Option<omnifs_core::ContentType> {
        self.content_type
    }

    pub fn content(&self) -> Option<&[u8]> {
        match &self.bytes {
            ContentBytes::Read(ReadFileBytes::Inline(content)) => Some(content),
            ContentBytes::Read(ReadFileBytes::Blob(_)) | ContentBytes::Canonical => None,
        }
    }

    #[doc(hidden)]
    pub fn into_read_outcome_and_effects(self) -> (wit_types::ReadFileOutcome, Effects) {
        (
            wit_types::ReadFileOutcome::Found(wit_types::ReadFileResult {
                content_type: self.content_type.map(|ct| ct.as_mime().to_string()),
                attrs: self.attrs.into(),
                bytes: self.bytes.into(),
            }),
            self.effects,
        )
    }
}

/// A completed read-file terminal: found content, or a not-found
/// optionally keyed to the logical object whose absence was established
/// (so object invalidation clears the cached miss).
#[derive(Clone, Debug)]
pub enum ReadOutcome {
    Found(FileContent),
    NotFound(Option<LogicalId>),
}

impl ReadOutcome {
    #[doc(hidden)]
    pub fn into_result_and_effects(self) -> (wit_types::ReadFileOutcome, Effects) {
        match self {
            Self::Found(content) => content.into_read_outcome_and_effects(),
            Self::NotFound(id) => (
                wit_types::ReadFileOutcome::NotFound(id.map(Into::into)),
                Effects::new(),
            ),
        }
    }
}

impl From<FileContent> for wit_types::ReadFileResult {
    fn from(result: FileContent) -> Self {
        let (outcome, _) = result.into_read_outcome_and_effects();
        match outcome {
            wit_types::ReadFileOutcome::Found(found) => found,
            wit_types::ReadFileOutcome::NotFound(_) => {
                panic!("FileContent cannot lower to a not-found read outcome")
            },
        }
    }
}

/// Normalize an effect path to protocol form: tolerate a missing leading
/// slash, then validate through `Path::parse` so malformed paths surface
/// as invalid-input here instead of corrupting the view cache.
fn wire_path(path: &str) -> Result<Path> {
    let absolute = if path.starts_with('/') {
        path.to_string()
    } else {
        format!("/{path}")
    };
    Path::parse(&absolute).map_err(|error| ProviderError::invalid_input(error.to_string()))
}
