//! Child-object collections: typed listings of child objects with typed,
//! host-opaque cursors.
//!
//! A collection face (`o.dir(name).collection::<C>(method)`) lists child
//! objects of type `C`. The list `method` returns a [`Collection<C, Cur>`]; the
//! SDK lowers each [`CollectionEntry`] to a dir entry and, for `fresh` entries,
//! stores the child's canonical bytes at listing time so a later read of the
//! child serves warm. Pagination is a typed [`Cursor`] the host stores and
//! echoes back as opaque bytes.

use crate::cx::Cx;
use crate::error::{ProviderError, Result};
use crate::file_attrs::VersionToken;
use crate::object::{Canonical, Object};
use crate::projection::FileProjection;
use std::ops::Deref;

/// A pagination cursor: typed in provider code, opaque bytes to the host. The
/// host stores the encoded bytes and feeds them back on the next list call; it
/// never inspects their meaning. A cursor resumes a list; it does not prove
/// freshness, existence, or completeness.
pub trait Cursor: Sized {
    /// Encode into the host-opaque token (the wire carries a string). Pack
    /// whatever resumes the list: a page number, an upstream continuation
    /// token, a composite of both.
    fn encode(&self) -> String;
    fn decode(token: &str) -> Result<Self>;
}

/// The absence of a typed cursor for a list call. Does NOT imply the listing is
/// complete; completeness is declared by the [`Collection`] variant.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct NoCursor;

impl Cursor for NoCursor {
    fn encode(&self) -> String {
        String::new()
    }
    fn decode(_token: &str) -> Result<Self> {
        Ok(Self)
    }
}

/// An integer page cursor for offset/page-number pagination, carried
/// host-opaque as its decimal string. The common shape when the upstream pages
/// by a number rather than an opaque continuation token.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct PageCursor(pub u64);

impl Cursor for PageCursor {
    fn encode(&self) -> String {
        self.0.to_string()
    }
    fn decode(token: &str) -> Result<Self> {
        token
            .parse()
            .map(Self)
            .map_err(|_| ProviderError::invalid_input("bad page cursor"))
    }
}

/// The execution context for a collection list call: callout machinery plus the
/// typed cursor the host echoed back. Derefs to [`Cx<S>`] so the usual context
/// methods (`.http()`, `.endpoint()`, `.state()`) work directly.
pub struct ListCx<C = NoCursor, S = ()> {
    cx: Cx<S>,
    cursor: Option<C>,
}

impl<C, S> ListCx<C, S> {
    pub(crate) fn new(cx: Cx<S>, cursor: Option<C>) -> Self {
        Self { cx, cursor }
    }

    /// The host-echoed resume cursor for this list call, or `None` on the first
    /// page.
    pub fn cursor(&self) -> Option<&C> {
        self.cursor.as_ref()
    }
}

impl<C, S> Deref for ListCx<C, S> {
    type Target = Cx<S>;
    fn deref(&self) -> &Cx<S> {
        &self.cx
    }
}

/// One child of a collection.
///
/// - `Fresh`: the list payload satisfies the child object's canonical contract;
///   the child canonical is stored at listing time.
/// - `Derived`: shallow list fields populate eager derived leaves but not the
///   canonical object.
/// - `Key`: discovery only (name; bytes load on the child's own read).
pub enum CollectionEntry<T: Object> {
    Fresh {
        key: T::Key,
        value: T,
        canonical: Canonical,
    },
    Derived {
        key: T::Key,
        files: Vec<(String, FileProjection)>,
    },
    Key {
        key: T::Key,
    },
}

impl<T: Object> CollectionEntry<T> {
    /// Use only when the list payload satisfies the child object's canonical
    /// contract (the stored bytes are exactly what `T::decode` consumes).
    pub fn fresh(key: T::Key, value: T, canonical: Canonical) -> Self {
        Self::Fresh {
            key,
            value,
            canonical,
        }
    }

    /// Shallow list fields that can populate derived leaves but not the
    /// canonical object.
    pub fn derived(key: T::Key, files: Vec<(String, FileProjection)>) -> Self {
        Self::Derived { key, files }
    }

    /// Discovery only: the child name, no bytes.
    pub fn key(key: T::Key) -> Self {
        Self::Key { key }
    }

    pub(crate) fn entry_key(&self) -> &T::Key {
        match self {
            Self::Fresh { key, .. } | Self::Derived { key, .. } | Self::Key { key } => key,
        }
    }
}

/// A listing of child objects.
///
/// - `Complete`: the exhaustive listing the provider knows at this instant.
/// - `Page`: a partial page plus a typed resume cursor; more is reachable.
/// - `Partial`: intentionally open or truncated with no cursor; lookup remains
///   authoritative for navigable names.
/// - `Unchanged`: the host's listing validator matched; serve cached dirents.
pub enum Collection<T: Object, C = NoCursor> {
    Complete {
        entries: Vec<CollectionEntry<T>>,
        validator: Option<VersionToken>,
    },
    Page {
        entries: Vec<CollectionEntry<T>>,
        next: C,
        validator: Option<VersionToken>,
    },
    Partial {
        entries: Vec<CollectionEntry<T>>,
        validator: Option<VersionToken>,
    },
    Unchanged,
}

impl<T: Object, C: Cursor> Collection<T, C> {
    /// The complete listing the provider knows at this instant.
    pub fn complete(entries: impl IntoIterator<Item = CollectionEntry<T>>) -> Self {
        Self::Complete {
            entries: entries.into_iter().collect(),
            validator: None,
        }
    }

    /// A page; finish with [`CollectionPage::next`] to attach the resume cursor.
    pub fn page(entries: impl IntoIterator<Item = CollectionEntry<T>>) -> CollectionPage<T, C> {
        CollectionPage {
            entries: entries.into_iter().collect(),
            validator: None,
            _cursor: std::marker::PhantomData,
        }
    }

    /// Intentionally open or truncated, with no cursor.
    pub fn partial(entries: impl IntoIterator<Item = CollectionEntry<T>>) -> Self {
        Self::Partial {
            entries: entries.into_iter().collect(),
            validator: None,
        }
    }

    /// The listing validator matched; serve cached dirents.
    pub fn unchanged() -> Self {
        Self::Unchanged
    }

    /// Record the validator the host echoes on the next list for a cheap re-list.
    #[must_use]
    pub fn with_validator(mut self, validator: impl Into<VersionToken>) -> Self {
        let v = validator.into();
        match &mut self {
            Self::Complete { validator, .. }
            | Self::Page { validator, .. }
            | Self::Partial { validator, .. } => *validator = Some(v),
            Self::Unchanged => {},
        }
        self
    }
}

/// A pending paged collection awaiting its resume cursor.
pub struct CollectionPage<T: Object, C> {
    entries: Vec<CollectionEntry<T>>,
    validator: Option<VersionToken>,
    _cursor: std::marker::PhantomData<C>,
}

impl<T: Object, C: Cursor> CollectionPage<T, C> {
    /// Attach the typed resume cursor. `None` next is not a thing — use
    /// [`Collection::complete`] or [`Collection::partial`] when no cursor exists.
    pub fn next(self, cursor: C) -> Collection<T, C> {
        Collection::Page {
            entries: self.entries,
            next: cursor,
            validator: self.validator,
        }
    }

    #[must_use]
    pub fn with_validator(mut self, validator: impl Into<VersionToken>) -> Self {
        self.validator = Some(validator.into());
        self
    }
}

/// Lower a typed [`Collection`] to the [`crate::projection::DirProjection`] the
/// SDK-generated collection list handler returns.
///
/// Each entry becomes a child directory named by the child object anchor's
/// segment(s) beyond the collection dir, computed from the entry key against
/// the CHILD's registered template (not the parent's captures). A `Fresh`
/// entry also stores the child canonical against its own logical id with the
/// child's canonical-view leaf paths (canonical/representation/derived,
/// facet-expanded) as view leaves, so a later read of any child leaf serves
/// warm. A `Derived` entry projects its shallow leaves under the child anchor.
/// The completeness variant selects exhaustive / open / paged.
pub(crate) fn collection_to_dir_projection<T, C>(
    child_view: &crate::router::ResolvedChildView,
    collection: Collection<T, C>,
) -> Result<crate::projection::DirProjection>
where
    T: Object,
    T::Key: crate::object::Key,
    C: Cursor,
{
    use crate::identity::IdentityCaptures;
    use crate::object::FacetMetadata as _;
    use crate::projection::{DirProjection, Entry};

    let (entries, cursor, validator, complete) = match collection {
        Collection::Complete { entries, validator } => (entries, None, validator, true),
        Collection::Partial { entries, validator } => (entries, None, validator, false),
        Collection::Page {
            entries,
            next,
            validator,
        } => (entries, Some(encode_cursor(&next)), validator, false),
        Collection::Unchanged => return Ok(DirProjection::unchanged()),
    };

    // The child view resolution plus the canonical bytes a fresh entry stores,
    // and the shallow derived leaves a derived entry projects.
    let mut fresh_stores: Vec<(crate::router::EntryView, Canonical)> = Vec::new();
    let mut derived_files: Vec<(String, crate::projection::FileProjection)> = Vec::new();
    let mut dir_entries = Vec::with_capacity(entries.len());

    for entry in entries {
        let key = entry.entry_key();
        let view = child_view.entry_view(&key.identity_captures(), T::Key::facet_axes())?;
        dir_entries.push(Entry::dir(view.child_name.clone()));

        match entry {
            CollectionEntry::Fresh { canonical, .. } => fresh_stores.push((view, canonical)),
            CollectionEntry::Derived { files, .. } => {
                for (leaf, file) in files {
                    derived_files.push((format!("{}/{leaf}", view.anchor_base), file));
                }
            },
            CollectionEntry::Key { .. } => {},
        }
    }

    let mut projection = if complete {
        DirProjection::exhaustive(dir_entries)
    } else if let Some(cursor) = cursor {
        DirProjection::paged(dir_entries, cursor)
    } else {
        DirProjection::open(dir_entries)
    };
    if let Some(validator) = validator {
        projection = projection.with_validator(validator);
    }
    for (view, canonical) in fresh_stores {
        projection = projection.store_canonical(
            view.id,
            canonical.validator,
            canonical.bytes,
            view.view_leaves,
        );
    }
    for (path, file) in derived_files {
        projection = projection.preload_file(path, file);
    }
    Ok(projection)
}

/// Encode a typed cursor into the host-opaque token carried by the wire.
pub(crate) fn encode_cursor<C: Cursor>(cursor: &C) -> crate::handler::Cursor {
    crate::handler::Cursor::Opaque(cursor.encode())
}

/// Decode a host-echoed opaque token back into a typed cursor.
pub(crate) fn decode_cursor<C: Cursor>(cursor: &crate::handler::Cursor) -> Result<Option<C>> {
    match cursor {
        crate::handler::Cursor::Opaque(token) => Ok(Some(C::decode(token)?)),
        crate::handler::Cursor::Page(page) => Ok(Some(C::decode(&page.to_string())?)),
    }
}
