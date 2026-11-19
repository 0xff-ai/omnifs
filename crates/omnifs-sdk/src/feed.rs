//! Object feeds: an object whose upstream reports its changes keeps the facts
//! of its fed faces in the host projection through one feed.
//!
//! An object declares its feed with `o.feed(method).starts([...])` and its
//! fed faces with `.fed()` (a file face: its lookup and bytes; a dir face: its
//! listing; `o.fed()`: the lookup of the anchor). An operation on a fed path
//! answers `feed` with the claims of the feeds that answer for it, the outer
//! object first, and never runs a request; the host then asks the answering
//! feed for an answer when its facts lack the path, for a lookup only before
//! the end of the first answer of the feed, or are past their deadline, and
//! serves the path from its projection, as not found for a path that it
//! lacks.
//!
//! The host calls the feed `method` once per part of an answer with a
//! [`FeedCx`]: the anchor path and the key of the object, the token the feed
//! returned last, the declared starting path an operation started the answer
//! from, the listings of the projection, and the facts the part writes. The
//! method answers [`Feed::unchanged`] or a [`FeedPart`] with the token after
//! it, whether more parts follow, and the tokens of the inner feeds it brought.

use std::cell::RefCell;
use std::ops::Deref;
use std::rc::Rc;

use crate::browse::{Effects, ListingWrite};
use crate::cx::Cx;
use crate::error::Result;
use crate::identity::IdentityCaptures;
use crate::object::{Key, Object, ObjectKind};
use omnifs_core::path::Path;
use omnifs_wit::provider::types as wit_types;

/// The context of one feed call. Derefs to [`Cx`] for callouts and state.
pub struct FeedCx<K, S = ()> {
    cx: Cx<S>,
    path: Path,
    key: K,
    token: Option<String>,
    start: Option<&'static str>,
    facts: Rc<RefCell<Effects>>,
}

impl<K, S> FeedCx<K, S> {
    pub(crate) fn new(
        cx: Cx<S>,
        path: Path,
        key: K,
        token: Option<String>,
        start: Option<&'static str>,
        facts: Rc<RefCell<Effects>>,
    ) -> Self {
        Self {
            cx,
            path,
            key,
            token,
            start,
            facts,
        }
    }

    /// The anchor path of the feed's object as the host asked for it, which
    /// the paths of its facts extend: a key whose captures normalize names
    /// no path the host serves.
    pub fn path(&self) -> &Path {
        &self.path
    }

    /// The key of the feed's object.
    pub fn key(&self) -> &K {
        &self.key
    }

    /// The token of the last part the host published for this feed, or of
    /// the inner state an outer feed gave it; `None` before its first part.
    pub fn token(&self) -> Option<&str> {
        self.token.as_deref()
    }

    /// The declared starting path, relative to the object, of the operation
    /// that started this answer (`""` for the anchor itself), or `None` for
    /// an answer the host asks for once the feed is past its deadline.
    pub fn start(&self) -> Option<&'static str> {
        self.start
    }

    /// The listing of the mount-absolute `path` in the host projection, as a
    /// count of its entries or their names, with its completeness; `None`
    /// for a path the projection has no listing of. The host answers only
    /// for a path in the subtree of the feed's object.
    pub fn listing(
        &self,
        path: impl AsRef<str>,
        form: ListingForm,
    ) -> Result<Option<ListingFacts>> {
        let facts =
            crate::omnifs::provider::projection::listing(self.cx.id(), path.as_ref(), form.into())?;
        Ok(facts.map(ListingFacts::from))
    }

    /// Write a stable inline file at the mount-absolute `path` as a fact of
    /// this part.
    pub fn file(&self, path: impl AsRef<str>, bytes: impl Into<Vec<u8>>) -> Result<()> {
        self.facts
            .borrow_mut()
            .fed_file(path.as_ref(), bytes.into())?;
        Ok(())
    }

    /// Write the directory at the mount-absolute `path`; the files and
    /// directories this part writes directly under it join its listing as
    /// `write` says.
    pub fn dir(&self, path: impl AsRef<str>, write: ListingWrite) -> Result<()> {
        self.facts.borrow_mut().directory(path.as_ref(), write)?;
        Ok(())
    }

    /// Retract the facts of the mount-absolute `path` and of everything
    /// beneath it, leaving its parent listing as complete as it was.
    pub fn remove(&self, path: impl AsRef<str>) -> Result<()> {
        self.facts.borrow_mut().remove(path)?;
        Ok(())
    }
}

impl<K, S> Deref for FeedCx<K, S> {
    type Target = Cx<S>;

    fn deref(&self) -> &Cx<S> {
        &self.cx
    }
}

/// What [`FeedCx::listing`] returns of a listing.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ListingForm {
    Count,
    Names,
}

impl From<ListingForm> for wit_types::ListingForm {
    fn from(form: ListingForm) -> Self {
        match form {
            ListingForm::Count => Self::Count,
            ListingForm::Names => Self::Names,
        }
    }
}

/// A listing of the host projection with its completeness.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ListingFacts {
    pub complete: bool,
    pub entries: ListingEntries,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum ListingEntries {
    Count(u64),
    Names(Vec<String>),
}

impl From<wit_types::ListingFacts> for ListingFacts {
    fn from(facts: wit_types::ListingFacts) -> Self {
        Self {
            complete: facts.complete,
            entries: match facts.entries {
                wit_types::ListingEntries::Count(count) => ListingEntries::Count(count),
                wit_types::ListingEntries::Names(names) => ListingEntries::Names(names),
            },
        }
    }
}

/// The answer of one feed call.
pub enum Feed {
    /// The facts of the feed stand.
    Unchanged,
    /// One part of the answer; its facts are the writes of the [`FeedCx`].
    Part(FeedPart),
}

impl Feed {
    /// The facts of the feed stand; valid for a call with a token.
    pub fn unchanged() -> Self {
        Self::Unchanged
    }

    /// One part of the answer, with the token after it. The answer ends with
    /// it unless [`FeedPart::more`] says more parts follow.
    pub fn part(token: impl Into<String>) -> FeedPart {
        FeedPart {
            token: token.into(),
            more: false,
            inner: Vec::new(),
        }
    }
}

impl From<FeedPart> for Feed {
    fn from(part: FeedPart) -> Self {
        Self::Part(part)
    }
}

/// One part of a feed answer.
pub struct FeedPart {
    token: String,
    more: bool,
    inner: Vec<InnerState>,
}

impl FeedPart {
    /// Whether more parts follow in the same answer.
    #[must_use]
    pub fn more(mut self, more: bool) -> Self {
        self.more = more;
        self
    }

    /// The token of the inner feed of the object of type `O` with `key`, as
    /// an item of a page of rows: the host keeps it as that feed's state.
    #[must_use]
    pub fn inner<O: Object>(mut self, key: &O::Key, token: impl Into<String>) -> Self {
        self.inner.push(InnerState {
            kind: O::kind(),
            identity: key.identity_captures(),
            token: token.into(),
        });
        self
    }

    pub(crate) fn into_parts(self) -> (String, bool, Vec<InnerState>) {
        (self.token, self.more, self.inner)
    }
}

/// An inner feed state, type-erased to the kind and the identity of the
/// logical id of its object.
pub(crate) struct InnerState {
    pub(crate) kind: ObjectKind,
    pub(crate) identity: Vec<(&'static str, String)>,
    pub(crate) token: String,
}

/// The feed method of an object, erased over its future.
pub(crate) type FeedFuture = std::pin::Pin<Box<dyn std::future::Future<Output = Result<Feed>>>>;

/// Run a typed feed method: parse the key and hand it the context.
pub(crate) fn feed_handler<O, Fut>(
    method: fn(FeedCx<O::Key, O::State>) -> Fut,
) -> std::rc::Rc<FeedHandlerFn<O::State>>
where
    O: Object + 'static,
    O::Key: Key + 'static,
    O::State: 'static,
    Fut: std::future::Future<Output = Result<Feed>> + 'static,
{
    std::rc::Rc::new(
        move |cx: Cx<O::State>,
              path: Path,
              caps: &crate::captures::Captures,
              token: Option<String>,
              start: Option<&'static str>,
              facts: Rc<RefCell<Effects>>| {
            match <O::Key as crate::captures::FromCaptures>::from_captures(caps) {
                Ok(key) => {
                    Box::pin(method(FeedCx::new(cx, path, key, token, start, facts))) as FeedFuture
                },
                Err(error) => Box::pin(async move { Err(error) }),
            }
        },
    )
}

/// The `dyn Fn` an object's feed boxes: the context of the call, the anchor
/// path and its captures, the token, the declared starting path, and the
/// facts it writes.
pub(crate) type FeedHandlerFn<S> = dyn Fn(
    Cx<S>,
    Path,
    &crate::captures::Captures,
    Option<String>,
    Option<&'static str>,
    Rc<RefCell<Effects>>,
) -> FeedFuture;
