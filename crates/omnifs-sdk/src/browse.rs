//! Internal SDK conversion types between provider-facing browse values
//! and the generated WIT protocol. Provider authors normally build
//! these through `handler.rs`; the SDK maps them to `operation-result`
//! plus terminal `effect`s.

use crate::error::{ProviderError, Result};
use crate::file_attrs::{FileAttrs, FileProj, ReadFileBytes, ReadMode, Size, Stability};
use crate::omnifs::provider::types as wit_types;

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
}

impl Entry {
    pub fn dir(name: impl Into<String>) -> Self {
        Self {
            name: name.into(),
            kind: EntryKind::Directory,
            file: None,
        }
    }

    pub fn file(name: impl Into<String>, file: FileProj) -> Self {
        Self {
            name: name.into(),
            kind: EntryKind::File,
            file: Some(file),
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
        }
    }
}

/// Terminal host mutations staged with an accepted provider return.
#[derive(Clone, Debug, Default)]
pub struct Effects {
    effects: Vec<wit_types::Effect>,
}

impl Effects {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn project_dir(&mut self, path: impl Into<String>) -> Result<&mut Self> {
        self.effects
            .push(wit_types::Effect::Project(wit_types::ProjEntry {
                path: normalize_project_path(path.into())?,
                kind: wit_types::EntryKind::Directory,
            }));
        Ok(self)
    }

    pub fn project_file(&mut self, path: impl Into<String>, file: FileProj) -> Result<&mut Self> {
        file.validate()?;
        self.effects
            .push(wit_types::Effect::Project(wit_types::ProjEntry {
                path: normalize_project_path(path.into())?,
                kind: wit_types::EntryKind::File(file.into()),
            }));
        Ok(self)
    }

    pub fn invalidate_path(&mut self, path: impl Into<String>) -> &mut Self {
        self.effects
            .push(wit_types::Effect::InvalidatePath(normalize_path(
                path.into(),
            )));
        self
    }

    pub fn invalidate_prefix(&mut self, prefix: impl Into<String>) -> &mut Self {
        self.effects
            .push(wit_types::Effect::InvalidatePrefix(normalize_path(
                prefix.into(),
            )));
        self
    }

    #[doc(hidden)]
    pub fn disown_tree(&mut self, path: impl Into<String>, tree: u64) -> &mut Self {
        self.effects
            .push(wit_types::Effect::DisownTree(wit_types::TreeHandoff {
                path: normalize_path(path.into()),
                tree,
            }));
        self
    }

    pub fn extend(&mut self, other: Effects) -> &mut Self {
        self.effects.extend(other.effects);
        self
    }

    pub fn is_empty(&self) -> bool {
        self.effects.is_empty()
    }

    #[doc(hidden)]
    pub fn into_wit(self) -> Vec<wit_types::Effect> {
        self.effects
    }
}

/// A directory listing with entries, exhaustiveness, and accepted-return
/// projection effects for adjacent or nested paths.
#[derive(Clone, Debug)]
pub struct Listing {
    entries: Vec<Entry>,
    exhaustive: bool,
    effects: Effects,
}

impl Listing {
    pub fn complete(entries: impl IntoIterator<Item = Entry>) -> Self {
        Self {
            entries: entries.into_iter().collect(),
            exhaustive: true,
            effects: Effects::new(),
        }
    }

    pub fn partial(entries: impl IntoIterator<Item = Entry>) -> Self {
        Self {
            entries: entries.into_iter().collect(),
            exhaustive: false,
            effects: Effects::new(),
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
#[derive(Clone, Debug)]
pub enum Lookup {
    Entry(LookupEntry),
    Subtree { path: String, tree: u64 },
    NotFound,
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

    pub fn subtree(path: impl Into<String>, tree: u64) -> Self {
        Self::Subtree {
            path: normalize_path(path.into()),
            tree,
        }
    }

    pub fn not_found() -> Self {
        Self::NotFound
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
            Self::Subtree { path, tree } => {
                let mut effects = Effects::new();
                effects.disown_tree(path, tree);
                (wit_types::LookupChildResult::Subtree(tree), effects)
            },
            Self::NotFound => (wit_types::LookupChildResult::NotFound, Effects::new()),
        }
    }
}

impl From<Lookup> for wit_types::LookupChildResult {
    fn from(lookup: Lookup) -> Self {
        lookup.into_result_and_effects().0
    }
}

/// A list result: either a listing or a subtree handoff.
#[derive(Clone, Debug)]
pub enum List {
    Entries(Listing),
    Subtree { path: String, tree: u64 },
}

impl List {
    pub fn entries(listing: Listing) -> Self {
        Self::Entries(listing)
    }

    pub fn subtree(path: impl Into<String>, tree: u64) -> Self {
        Self::Subtree {
            path: normalize_path(path.into()),
            tree,
        }
    }

    #[doc(hidden)]
    pub fn into_result_and_effects(self) -> (wit_types::ListChildrenResult, Effects) {
        match self {
            Self::Entries(listing) => {
                let (listing, effects) = listing.into_parts();
                (wit_types::ListChildrenResult::Entries(listing), effects)
            },
            Self::Subtree { path, tree } => {
                let mut effects = Effects::new();
                effects.disown_tree(path, tree);
                (wit_types::ListChildrenResult::Subtree(tree), effects)
            },
        }
    }
}

impl From<List> for wit_types::ListChildrenResult {
    fn from(list: List) -> Self {
        list.into_result_and_effects().0
    }
}

/// Full-read file content. Bytes are read data; adjacent projected paths must
/// be staged as effects.
#[derive(Clone, Debug)]
pub struct FileContent {
    attrs: FileAttrs,
    bytes: ReadFileBytes,
    effects: Effects,
}

impl FileContent {
    pub fn new(content: impl Into<Vec<u8>>) -> Self {
        let content = content.into();
        let size = u64::try_from(content.len()).unwrap_or(u64::MAX);
        Self {
            attrs: FileAttrs::new(Size::Exact(size), Stability::Immutable),
            bytes: ReadFileBytes::Inline(content),
            effects: Effects::new(),
        }
    }

    /// Serve from a host-resident blob. No bytes cross the WIT.
    pub fn blob(blob: impl Into<crate::blob::BlobId>) -> Self {
        Self {
            attrs: FileAttrs::new(Size::Unknown, Stability::Immutable),
            bytes: ReadFileBytes::Blob(blob.into()),
            effects: Effects::new(),
        }
    }

    #[must_use]
    pub fn with_attrs(mut self, attrs: FileAttrs) -> Self {
        self.attrs = attrs;
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

    pub fn content(&self) -> Option<&[u8]> {
        match &self.bytes {
            ReadFileBytes::Inline(content) => Some(content),
            ReadFileBytes::Blob(_) => None,
        }
    }

    #[doc(hidden)]
    pub fn into_result_and_effects(self) -> (wit_types::ReadFileResult, Effects) {
        (
            wit_types::ReadFileResult {
                attrs: self.attrs.into(),
                bytes: self.bytes.into(),
            },
            self.effects,
        )
    }
}

impl From<FileContent> for wit_types::ReadFileResult {
    fn from(result: FileContent) -> Self {
        result.into_result_and_effects().0
    }
}

fn normalize_path(path: impl AsRef<str>) -> String {
    path.as_ref().trim_matches('/').to_string()
}

fn normalize_project_path(path: impl AsRef<str>) -> Result<String> {
    let path = normalize_path(path);
    if path.is_empty() {
        return Err(ProviderError::invalid_input(
            "project effect path must not be empty",
        ));
    }
    if path
        .split('/')
        .any(|segment| segment.is_empty() || segment == "." || segment == "..")
    {
        return Err(ProviderError::invalid_input(format!(
            "project effect path {path:?} must be a normalized provider path"
        )));
    }
    Ok(path)
}

pub(crate) fn deferred_full_attrs_for_read(file: &FileProj) -> FileAttrs {
    match file.bytes {
        crate::file_attrs::ProjBytes::Inline(_) => FileAttrs {
            size: file.attrs.size.clone(),
            stability: file.attrs.stability,
            version: file.attrs.version.clone(),
        },
        crate::file_attrs::ProjBytes::Deferred {
            read: ReadMode::Full | ReadMode::Ranged,
        } => file.attrs.clone(),
    }
}
