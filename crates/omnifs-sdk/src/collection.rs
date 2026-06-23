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
use crate::error::Result;
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

/// The execution context for a collection list call: callout machinery plus the
/// typed cursor the host echoed back. Derefs to [`Cx<S>`] so the usual context
/// methods (`.http()`, `.endpoint()`, `.state()`) work directly.
pub struct ListCx<C = NoCursor, S = ()> {
    cx: Cx<S>,
    cursor: Option<C>,
}

impl<C, S> ListCx<C, S> {
    pub fn new(cx: Cx<S>, cursor: Option<C>) -> Self {
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
