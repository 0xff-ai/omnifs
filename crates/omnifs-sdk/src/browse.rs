//! Internal SDK conversion types between provider-facing browse values
//! and the generated WIT protocol. Provider authors normally build
//! these through `handler.rs`; the SDK maps them to `operation-result`
//! plus terminal `effect`s.

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
}

impl CachedCanonical {
    pub fn from_wit(input: wit_types::CanonicalInput) -> Self {
        Self {
            id: input.id,
            bytes: input.bytes,
            validator: input.validator.map(VersionToken::from),
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

/// A filesystem entry representing a file or directory.
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

    pub fn file_proj(&self) -> Option<&FileProj> {
        self.file.as_ref()
    }

    pub fn attrs(&self) -> Option<&FileAttrs> {
        self.file.as_ref().map(|file| &file.attrs)
    }
}

impl Entry {
    #[must_use]
    pub fn with_id(mut self, id: LogicalId) -> Self {
        self.logical_id = Some(id);
        self
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

/// Terminal host mutations staged with an accepted provider return.
///
/// The three WIT effect blocks are kept as separate typed vecs so the
/// `fs`/`canonical`/`invalidations` channels never alias.
/// [`Self::into_wit`] assembles them into the WIT `effects` record.
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

    pub fn project_dir(&mut self, path: impl AsRef<str>) -> Result<&mut Self> {
        self.fs.push(wit_types::FsWrite {
            id: None,
            path: wire_path(path.as_ref())?,
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
            path: wire_path(path.as_ref())?,
            kind: wit_types::FsKind::Directory(true),
        });
        Ok(self)
    }

    pub fn project_file(&mut self, path: impl AsRef<str>, file: FileProj) -> Result<&mut Self> {
        file.validate()?;
        self.fs.push(wit_types::FsWrite {
            id: None,
            path: wire_path(path.as_ref())?,
            kind: wit_types::FsKind::File(file.into()),
        });
        Ok(self)
    }

    /// Store raw upstream bytes against a logical object id.
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

    pub fn project_file_with_id(
        &mut self,
        path: impl AsRef<str>,
        id: Option<&LogicalId>,
        file: FileProj,
    ) -> Result<&mut Self> {
        file.validate()?;
        self.fs.push(wit_types::FsWrite {
            id: id.map(Into::into),
            path: wire_path(path.as_ref())?,
            kind: wit_types::FsKind::File(file.into()),
        });
        Ok(self)
    }

    pub fn invalidate_object(&mut self, id: &LogicalId) -> &mut Self {
        self.invalidations
            .push(wit_types::Invalidation::Object(id.into()));
        self
    }

    pub fn invalidate_listing_path(&mut self, path: impl AsRef<str>) -> &mut Self {
        let path = wire_path(path.as_ref()).expect("invalidation path must be a protocol path");
        self.invalidations.push(wit_types::Invalidation::Listing(
            wit_types::PathOrPrefix::Path(path),
        ));
        self
    }

    pub fn invalidate_listing_prefix(&mut self, prefix: impl AsRef<str>) -> &mut Self {
        let prefix =
            wire_path(prefix.as_ref()).expect("invalidation prefix must be a protocol path");
        self.invalidations.push(wit_types::Invalidation::Listing(
            wit_types::PathOrPrefix::Prefix(prefix),
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
#[derive(Clone, Debug)]
pub struct Listing {
    entries: Vec<Entry>,
    exhaustive: bool,
    effects: Effects,
    validator: Option<String>,
    next_cursor: Option<wit_types::Cursor>,
}

impl Listing {
    pub fn complete(entries: impl IntoIterator<Item = Entry>) -> Self {
        Self {
            entries: entries.into_iter().collect(),
            exhaustive: true,
            effects: Effects::new(),
            validator: None,
            next_cursor: None,
        }
    }

    pub fn partial(entries: impl IntoIterator<Item = Entry>) -> Self {
        Self {
            entries: entries.into_iter().collect(),
            exhaustive: false,
            effects: Effects::new(),
            validator: None,
            next_cursor: None,
        }
    }

    pub fn empty_complete() -> Self {
        Self::complete([])
    }

    pub fn empty_partial() -> Self {
        Self::partial([])
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

/// A lookup result: either a found entry with cache-adjacent data, a
/// subtree handoff, or a miss.
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

impl LookupEntry {
    pub fn target(&self) -> &Entry {
        &self.target
    }

    pub fn siblings(&self) -> &[Entry] {
        &self.siblings
    }

    pub fn is_exhaustive(&self) -> bool {
        self.exhaustive
    }

    pub fn effects(&self) -> &Effects {
        &self.effects
    }
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

    pub fn file(name: impl Into<String>, content: impl Into<Vec<u8>>) -> Self {
        let content = content.into();
        Self::Entry(LookupEntry {
            target: Entry::file(name, FileProj::inline(content, Stability::Immutable, None)),
            siblings: Vec::new(),
            exhaustive: true,
            effects: Effects::new(),
        })
    }

    pub fn dir(name: impl Into<String>) -> Self {
        Self::entry(Entry::dir(name))
    }

    pub fn subtree(tree: u64) -> Self {
        Self::Subtree { tree }
    }

    pub fn not_found() -> Self {
        Self::NotFound { id: None }
    }

    pub fn not_found_id(id: LogicalId) -> Self {
        Self::NotFound { id: Some(id) }
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
    pub fn new(content: impl Into<Vec<u8>>) -> Self {
        let content = content.into();
        let size = u64::try_from(content.len()).unwrap_or(u64::MAX);
        Self {
            content_type: None,
            attrs: FileAttrs::new(Size::Exact(size), Stability::Immutable),
            bytes: ContentBytes::Read(ReadFileBytes::Inline(content)),
            effects: Effects::new(),
        }
    }

    /// Serve from a host-resident blob. No bytes cross the WIT.
    pub fn blob(blob: impl Into<crate::blob::BlobId>) -> Self {
        Self {
            content_type: None,
            attrs: FileAttrs::new(Size::Unknown, Stability::Immutable),
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

/// A completed read-file terminal: found bytes or an id-bearing not-found.
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

fn wire_path(path: &str) -> Result<String> {
    let absolute = if path.starts_with('/') {
        path.to_string()
    } else {
        format!("/{path}")
    };
    Path::parse(&absolute)
        .map(|path| path.as_str().to_string())
        .map_err(|error| ProviderError::invalid_input(error.to_string()))
}
