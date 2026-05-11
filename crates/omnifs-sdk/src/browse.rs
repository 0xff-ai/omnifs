//! Internal SDK conversion types between the handler-facing `Projection`
//! builder and the generated WIT types. Not part of the public surface:
//! provider authors build `Projection` / `FileContent` from `handler.rs`
//! and the SDK does the mapping.

use crate::file_attrs::{Bytes, FileAttrs, ReadMode, Size, Stability};
use crate::omnifs::provider::types as wit_types;

/// The kind of a filesystem entry.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum EntryKind {
    Directory,
    File,
}

/// A projected file that appears alongside an entry in a directory.
#[derive(Clone, Debug)]
pub struct ProjectedFile {
    pub(crate) name: String,
    pub(crate) attrs: FileAttrs,
}

impl ProjectedFile {
    pub fn new(name: impl Into<String>, attrs: FileAttrs) -> Self {
        Self {
            name: name.into(),
            attrs,
        }
    }

    pub fn inline_immutable(name: impl Into<String>, bytes: impl Into<Vec<u8>>) -> Self {
        Self::new(name, FileAttrs::inline(bytes, Stability::Immutable, None))
    }

    pub fn name(&self) -> &str {
        &self.name
    }

    pub fn attrs(&self) -> &FileAttrs {
        &self.attrs
    }

    pub fn content(&self) -> Option<&[u8]> {
        self.attrs.inline_bytes()
    }
}

impl From<ProjectedFile> for wit_types::ProjectedFile {
    fn from(file: ProjectedFile) -> Self {
        Self {
            name: file.name,
            attrs: file.attrs.into(),
        }
    }
}

/// A filesystem entry representing a file or directory.
#[derive(Clone, Debug)]
pub struct Entry {
    pub(crate) name: String,
    pub(crate) kind: EntryKind,
    pub(crate) attrs: Option<FileAttrs>,
    pub(crate) projected_files: Vec<ProjectedFile>,
}

impl Entry {
    pub fn dir(name: impl Into<String>) -> Self {
        Self {
            name: name.into(),
            kind: EntryKind::Directory,
            attrs: None,
            projected_files: Vec::new(),
        }
    }

    pub fn file(name: impl Into<String>, attrs: FileAttrs) -> Self {
        Self {
            name: name.into(),
            kind: EntryKind::File,
            attrs: Some(attrs),
            projected_files: Vec::new(),
        }
    }

    #[must_use]
    pub fn with_attrs(mut self, attrs: FileAttrs) -> Self {
        self.attrs = Some(attrs);
        self
    }

    #[must_use]
    pub fn projected(mut self, bytes: impl Into<Vec<u8>>) -> Self {
        let bytes = bytes.into();
        if matches!(self.kind, EntryKind::File) {
            self.attrs = Some(FileAttrs::inline(bytes.clone(), Stability::Immutable, None));
        }
        if self.kind == EntryKind::Directory && !bytes.is_empty() {
            let name = self.name.clone();
            self.projected_files
                .push(ProjectedFile::inline_immutable(name, bytes));
        }
        self
    }

    #[must_use]
    pub fn with_projected_files<I: IntoIterator<Item = ProjectedFile>>(mut self, files: I) -> Self {
        self.projected_files.extend(files);
        self
    }

    pub fn name(&self) -> &str {
        &self.name
    }

    pub fn kind(&self) -> EntryKind {
        self.kind
    }

    pub fn attrs(&self) -> Option<&FileAttrs> {
        self.attrs.as_ref()
    }

    pub fn projected_files(&self) -> &[ProjectedFile] {
        &self.projected_files
    }
}

impl From<Entry> for wit_types::DirEntry {
    fn from(entry: Entry) -> Self {
        Self {
            name: entry.name,
            kind: match entry.kind {
                EntryKind::Directory => wit_types::EntryKind::Directory,
                EntryKind::File => wit_types::EntryKind::File(
                    entry.attrs.expect("file entries must carry attrs").into(),
                ),
            },
        }
    }
}

/// A payload the provider has already fetched, to be cached by the host
/// so a later lookup or read of `path` is served without a provider
/// round trip. Carried on listings and on lookup terminals.
#[derive(Clone, Debug)]
pub enum Preload {
    File {
        path: String,
        attrs: FileAttrs,
        content: Vec<u8>,
    },
    Entry {
        path: String,
        kind: EntryKind,
        attrs: Option<FileAttrs>,
    },
}

impl Preload {
    pub fn file(path: impl Into<String>, content: impl Into<Vec<u8>>) -> Self {
        let content = content.into();
        Self::file_with_attrs(
            path,
            FileAttrs::deferred(
                Size::Exact(u64::try_from(content.len()).unwrap_or(u64::MAX)),
                ReadMode::Full,
                Stability::Immutable,
            ),
            content,
        )
    }

    pub fn file_with_attrs(
        path: impl Into<String>,
        attrs: FileAttrs,
        content: impl Into<Vec<u8>>,
    ) -> Self {
        Self::File {
            path: path.into(),
            attrs,
            content: content.into(),
        }
    }

    pub fn entry(path: impl Into<String>, kind: EntryKind, attrs: Option<FileAttrs>) -> Self {
        Self::Entry {
            path: path.into(),
            kind,
            attrs,
        }
    }

    pub fn path(&self) -> &str {
        match self {
            Self::File { path, .. } | Self::Entry { path, .. } => path,
        }
    }

    fn is_cacheable(&self) -> bool {
        !self.path().is_empty()
    }
}

impl From<Preload> for wit_types::PreloadItem {
    fn from(preload: Preload) -> Self {
        match preload {
            Preload::File {
                path,
                attrs,
                content,
            } => Self::File(wit_types::PreloadedFile {
                path,
                attrs: attrs.into(),
                content,
            }),
            Preload::Entry { path, kind, attrs } => Self::Entry(wit_types::PreloadedEntry {
                path,
                kind: match kind {
                    EntryKind::Directory => wit_types::EntryKind::Directory,
                    EntryKind::File => wit_types::EntryKind::File(
                        attrs
                            .expect("preloaded file entries must carry attrs")
                            .into(),
                    ),
                },
            }),
        }
    }
}

/// A directory listing with entries, exhaustiveness, and preload content.
#[derive(Clone, Debug)]
pub struct Listing {
    pub(crate) entries: Vec<Entry>,
    pub(crate) exhaustive: bool,
    pub(crate) preload: Vec<Preload>,
}

impl Listing {
    pub fn complete(entries: impl IntoIterator<Item = Entry>) -> Self {
        Self {
            entries: entries.into_iter().collect(),
            exhaustive: true,
            preload: Vec::new(),
        }
    }

    pub fn partial(entries: impl IntoIterator<Item = Entry>) -> Self {
        Self {
            entries: entries.into_iter().collect(),
            exhaustive: false,
            preload: Vec::new(),
        }
    }

    pub fn empty_complete() -> Self {
        Self {
            entries: Vec::new(),
            exhaustive: true,
            preload: Vec::new(),
        }
    }

    pub fn empty_partial() -> Self {
        Self {
            entries: Vec::new(),
            exhaustive: false,
            preload: Vec::new(),
        }
    }

    #[must_use]
    pub fn with_preload<I: IntoIterator<Item = Preload>>(mut self, files: I) -> Self {
        self.preload
            .extend(files.into_iter().filter(Preload::is_cacheable));
        self
    }

    pub fn entries(&self) -> &[Entry] {
        &self.entries
    }

    pub fn exhaustive(&self) -> bool {
        self.exhaustive
    }

    pub fn preload(&self) -> &[Preload] {
        &self.preload
    }
}

impl From<Listing> for wit_types::DirListing {
    fn from(listing: Listing) -> Self {
        Self {
            entries: listing.entries.into_iter().map(Into::into).collect(),
            exhaustive: listing.exhaustive,
            preload: listing.preload.into_iter().map(Into::into).collect(),
        }
    }
}

/// A lookup result: either a found entry with cache-adjacent data, a
/// subtree handoff, or a miss. Mirrors the WIT `lookup-result` variant.
#[derive(Clone, Debug)]
pub enum Lookup {
    Entry(LookupEntry),
    Subtree(u64),
    NotFound,
}

/// The non-subtree, non-miss shape of a lookup: the found entry plus
/// cache-adjacent data.
#[derive(Clone, Debug)]
pub struct LookupEntry {
    pub(crate) target: Entry,
    pub(crate) siblings: Vec<Entry>,
    pub(crate) sibling_files: Vec<ProjectedFile>,
    pub(crate) exhaustive: bool,
    pub(crate) preload: Vec<Preload>,
}

impl LookupEntry {
    pub fn target(&self) -> &Entry {
        &self.target
    }

    pub fn siblings(&self) -> &[Entry] {
        &self.siblings
    }

    pub fn sibling_files(&self) -> &[ProjectedFile] {
        &self.sibling_files
    }

    pub fn is_exhaustive(&self) -> bool {
        self.exhaustive
    }

    pub fn preload(&self) -> &[Preload] {
        &self.preload
    }
}

impl Lookup {
    pub fn entry(target: Entry) -> Self {
        Self::Entry(LookupEntry {
            target,
            siblings: Vec::new(),
            sibling_files: Vec::new(),
            exhaustive: true,
            preload: Vec::new(),
        })
    }

    pub fn file(name: impl Into<String>, content: impl Into<Vec<u8>>) -> Self {
        let name = name.into();
        let content = content.into();
        let attrs = FileAttrs::inline(content, Stability::Immutable, None);
        Self::Entry(LookupEntry {
            target: Entry::file(&name, attrs),
            siblings: Vec::new(),
            sibling_files: Vec::new(),
            exhaustive: true,
            preload: Vec::new(),
        })
    }

    pub fn dir(name: impl Into<String>) -> Self {
        Self::Entry(LookupEntry {
            target: Entry::dir(name),
            siblings: Vec::new(),
            sibling_files: Vec::new(),
            exhaustive: true,
            preload: Vec::new(),
        })
    }

    pub fn subtree(tree_ref: u64) -> Self {
        Self::Subtree(tree_ref)
    }

    pub fn not_found() -> Self {
        Self::NotFound
    }

    #[must_use]
    pub fn with_sibling_files<I: IntoIterator<Item = ProjectedFile>>(self, files: I) -> Self {
        match self {
            Self::Entry(mut entry) => {
                entry.sibling_files.extend(files);
                Self::Entry(entry)
            },
            other => other,
        }
    }

    #[must_use]
    pub fn with_preload<I: IntoIterator<Item = Preload>>(self, files: I) -> Self {
        match self {
            Self::Entry(mut entry) => {
                entry
                    .preload
                    .extend(files.into_iter().filter(Preload::is_cacheable));
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
}

impl From<Lookup> for wit_types::LookupResult {
    fn from(lookup: Lookup) -> Self {
        match lookup {
            Lookup::Entry(entry) => Self::Entry(wit_types::LookupEntry {
                target: entry.target.into(),
                siblings: entry.siblings.into_iter().map(Into::into).collect(),
                sibling_files: entry.sibling_files.into_iter().map(Into::into).collect(),
                exhaustive: entry.exhaustive,
                preload: entry.preload.into_iter().map(Into::into).collect(),
            }),
            Lookup::Subtree(tree_ref) => Self::Subtree(tree_ref),
            Lookup::NotFound => Self::NotFound,
        }
    }
}

/// A list result: either a listing or a subtree handoff. Mirrors the
/// WIT `list-result` variant.
#[derive(Clone, Debug)]
pub enum List {
    Entries(Listing),
    Subtree(u64),
}

impl List {
    pub fn entries(listing: Listing) -> Self {
        Self::Entries(listing)
    }

    pub fn subtree(tree_ref: u64) -> Self {
        Self::Subtree(tree_ref)
    }
}

impl From<List> for wit_types::ListResult {
    fn from(list: List) -> Self {
        match list {
            List::Entries(listing) => Self::Entries(listing.into()),
            List::Subtree(tree_ref) => Self::Subtree(tree_ref),
        }
    }
}

/// File content with optional projected siblings.
///
/// Two flavours: inline bytes that travel through the WIT, or a
/// blob-backed reference whose bytes live in the host's blob cache and
/// are streamed straight to FUSE without crossing the boundary.
#[derive(Clone, Debug)]
pub enum FileContent {
    Inline {
        attrs: FileAttrs,
        content: Vec<u8>,
        sibling_files: Vec<ProjectedFile>,
    },
    Blob {
        attrs: FileAttrs,
        blob: crate::blob::BlobId,
        sibling_files: Vec<ProjectedFile>,
    },
}

impl FileContent {
    pub fn new(content: impl Into<Vec<u8>>) -> Self {
        let content = content.into();
        let size = u64::try_from(content.len()).unwrap_or(u64::MAX);
        Self::Inline {
            attrs: FileAttrs {
                size: Size::Exact(size),
                bytes: Bytes::Deferred {
                    read: ReadMode::Full,
                },
                stability: Stability::Immutable,
                version: None,
            },
            content,
            sibling_files: Vec::new(),
        }
    }

    /// Serve from a host-resident blob — no bytes cross the WIT.
    pub fn blob(blob: impl Into<crate::blob::BlobId>) -> Self {
        Self::Blob {
            attrs: FileAttrs {
                size: Size::Unknown,
                bytes: Bytes::Deferred {
                    read: ReadMode::Full,
                },
                stability: Stability::Immutable,
                version: None,
            },
            blob: blob.into(),
            sibling_files: Vec::new(),
        }
    }

    #[must_use]
    pub fn with_attrs(mut self, attrs: FileAttrs) -> Self {
        match &mut self {
            Self::Inline { attrs: current, .. } | Self::Blob { attrs: current, .. } => {
                *current = attrs;
            },
        }
        self
    }

    #[must_use]
    pub fn with_sibling_files<I: IntoIterator<Item = ProjectedFile>>(mut self, files: I) -> Self {
        let (Self::Inline { sibling_files, .. } | Self::Blob { sibling_files, .. }) = &mut self;
        sibling_files.extend(files);
        self
    }

    pub fn attrs(&self) -> &FileAttrs {
        match self {
            Self::Inline { attrs, .. } | Self::Blob { attrs, .. } => attrs,
        }
    }

    pub fn content(&self) -> Option<&[u8]> {
        match self {
            Self::Inline { content, .. } => Some(content.as_slice()),
            Self::Blob { .. } => None,
        }
    }

    pub fn sibling_files(&self) -> &[ProjectedFile] {
        match self {
            Self::Inline { sibling_files, .. } | Self::Blob { sibling_files, .. } => sibling_files,
        }
    }
}

impl From<FileContent> for wit_types::FileContentResult {
    fn from(result: FileContent) -> Self {
        match result {
            FileContent::Inline {
                attrs,
                content,
                sibling_files,
            } => Self::Inline(wit_types::InlineFileContent {
                attrs: attrs.into(),
                content,
                sibling_files: sibling_files.into_iter().map(Into::into).collect(),
            }),
            FileContent::Blob {
                attrs,
                blob,
                sibling_files,
            } => Self::Blob(wit_types::BlobFileContent {
                attrs: attrs.into(),
                blob: blob.raw(),
                sibling_files: sibling_files.into_iter().map(Into::into).collect(),
            }),
        }
    }
}

/// Event outcome for `on-event` handlers. Carries invalidations the host
/// must apply at the response boundary.
#[derive(Clone, Debug, Default)]
pub struct EventOutcome {
    pub(crate) invalidate_paths: Vec<String>,
    pub(crate) invalidate_prefixes: Vec<String>,
}

impl EventOutcome {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn invalidate_path(&mut self, path: impl Into<String>) -> &mut Self {
        self.invalidate_paths.push(normalize_path(path.into()));
        self
    }

    pub fn invalidate_prefix(&mut self, prefix: impl Into<String>) -> &mut Self {
        self.invalidate_prefixes.push(normalize_path(prefix.into()));
        self
    }

    pub fn invalidate_paths(&self) -> &[String] {
        &self.invalidate_paths
    }

    pub fn invalidate_prefixes(&self) -> &[String] {
        &self.invalidate_prefixes
    }
}

impl From<EventOutcome> for wit_types::EventOutcome {
    fn from(outcome: EventOutcome) -> Self {
        Self {
            invalidate_paths: outcome.invalidate_paths,
            invalidate_prefixes: outcome.invalidate_prefixes,
        }
    }
}

fn normalize_path(path: String) -> String {
    match path.strip_prefix('/') {
        Some(stripped) => stripped.to_string(),
        None => path,
    }
}
