//! Object model: typed canonical resources addressed by typed keys.
//!
//! An [`Object`] is a value parsed from one canonical upstream payload (a
//! GitHub issue, an arXiv paper); its [`Key`] knows how to fetch that
//! payload and what logical identity it has. Register the pair on an
//! object route and the SDK handles the cache protocol: it emits
//! canonical-store effects on fresh loads, re-renders from host-pushed
//! canonical bytes on warm reads, and answers conditional reloads through
//! the `since` validator. The provider's only upstream-facing code is
//! [`Key::load`].

use crate::browse::{Effects, FileContent};
use crate::captures::FromCaptures;
use crate::cx::Cx;
use crate::error::{ProviderError, Result};
use crate::file_attrs::VersionToken;
use crate::identity::{IdentityCaptures, LogicalId};
use omnifs_core::ContentType;

/// An object kind tag for the canonical store and diagnostics (e.g.
/// `github.issue`). A compile-time constant supplied by the `#[object]` macro.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct ObjectKind(pub &'static str);

impl ObjectKind {
    pub fn as_str(self) -> &'static str {
        self.0
    }
}

/// The conditional-request validator at the object layer (an `ETag`,
/// updated-at, or similar). The same type as a file-level
/// [`VersionToken`].
pub type Validator = VersionToken;

/// The canonical bytes the remote returned, captured verbatim on a load.
///
/// Verbatim means exactly that: the raw response body, not a re-encoding
/// of the parsed value. These bytes are what the host stores and later
/// pushes back through `Object::parse_canonical`, so any normalization
/// here breaks warm re-rendering.
pub struct Canonical {
    pub bytes: Vec<u8>,
    pub validator: Option<Validator>,
}

/// The outcome of a key's [`Key::load`].
///
/// - `Fresh`: a new payload was fetched; carry the parsed value plus the
///   verbatim upstream bytes and their validator (e.g. the response `ETag`).
///   `effects` carries any additional host effects derived from the same
///   response and is empty for the common single-object case; use
///   [`Load::fresh_with_effects`] when one list or range payload contains
///   complete sibling objects to store in the canonical cache alongside the
///   requested object.
/// - `Unchanged`: a conditional request matched the `since` validator
///   (HTTP 304 shape); the host's cached canonical is still current and
///   the SDK serves from it. Return this only when `since` was given.
/// - `NotFound`: the upstream says the object does not exist. Lowered to
///   a not-found terminal, not an error.
pub enum Load<T> {
    Fresh {
        value: T,
        canonical: Canonical,
        effects: Effects,
    },
    Unchanged,
    NotFound,
}

impl<T> Load<T> {
    /// A fresh value with empty canonical bytes and no validator.
    ///
    /// The SDK still emits a canonical-store effect for the empty payload,
    /// so this effectively opts the route out of meaningful durable
    /// caching: a warm read would push empty bytes that
    /// `Object::parse_canonical` cannot parse. Appropriate only when the
    /// value is synthesized rather than fetched and re-rendering from
    /// canonical is never expected; for real upstream loads, return
    /// `Load::Fresh` with the verbatim body (the endpoint helpers
    /// [`crate::endpoint::RequestBuilder::load`] and `load_with` do this
    /// for you).
    pub fn fresh(value: T) -> Self {
        Self::Fresh {
            value,
            canonical: Canonical {
                bytes: Vec::new(),
                validator: None,
            },
            effects: Effects::new(),
        }
    }

    /// A fresh value with its verbatim canonical payload and no extra effects.
    pub fn fresh_from(value: T, canonical: Canonical) -> Self {
        Self::Fresh {
            value,
            canonical,
            effects: Effects::new(),
        }
    }

    /// A fresh value plus host effects derived from the same upstream response
    /// (e.g. sibling objects from one list or range payload).
    pub fn fresh_with_effects(value: T, canonical: Canonical, effects: Effects) -> Self {
        Self::Fresh {
            value,
            canonical,
            effects,
        }
    }
}

/// A typed canonical resource. Usually implemented via
/// `#[omnifs_sdk::object(kind = "...", key = ...)]` rather than by hand.
///
/// `parse_canonical` must accept the exact bytes [`Key::load`] captured in
/// [`Canonical`]: it runs again on every warm read when the host pushes
/// cached canonical bytes back. The default parses JSON; objects with a
/// non-JSON canonical (e.g. arXiv's Atom) override both
/// `canonical_content_type` and `parse_canonical` (the macro wires this
/// from its `canonical`/`parse` arguments).
pub trait Object: serde::Serialize + serde::de::DeserializeOwned + Sized {
    type Key: Key<Object = Self>;

    /// The stable kind tag; part of every [`LogicalId`] derived for this
    /// object, so renaming it orphans previously cached objects.
    fn kind() -> ObjectKind;

    /// The content type of the verbatim canonical bytes. The identity
    /// representation leaf serves these bytes as-is under this type.
    fn canonical_content_type() -> ContentType {
        ContentType::Json
    }

    /// Parse the verbatim canonical bytes back into a value. Failures
    /// surface as invalid-input; never normalize bytes here to make
    /// parsing easier, fix [`Key::load`] to store the right bytes instead.
    fn parse_canonical(bytes: &[u8]) -> Result<Self> {
        serde_json::from_slice(bytes)
            .map_err(|e| ProviderError::invalid_input(format!("canonical parse: {e}")))
    }
}

/// The identity side of an object: how to fetch it and what logical id it
/// anchors to. Keys are `#[path_captures]` structs; only `load` is written
/// by hand.
pub trait Key: FromCaptures + IdentityCaptures + Sized {
    type Object: Object<Key = Self>;
    type State;

    /// Fetch the object from upstream, honoring `since` as a conditional
    /// validator.
    ///
    /// The contract:
    ///
    /// - return `Load::Fresh` with the parsed value plus the verbatim
    ///   response bytes and their validator in [`Canonical`];
    /// - when `since` is `Some` and the upstream confirms no change (304,
    ///   matching `ETag`), return `Load::Unchanged` instead of refetching
    ///   the body: the host already holds the bytes;
    /// - return `Load::NotFound` for genuine upstream absence; reserve
    ///   `Err` for failures.
    ///
    /// `since` arrives from the host's cached canonical on warm reads and
    /// from the listing validator on re-lists; ignoring it is correct but
    /// wastes a full refetch per read.
    ///
    /// ```ignore
    /// impl Key for PaperVersionKey {
    ///     type Object = Paper;
    ///     type State = ();
    ///
    ///     async fn load(&self, cx: &Cx, since: Option<Validator>) -> Result<Load<Paper>> {
    ///         load_paper(cx, self.paper.decoded(), since).await
    ///     }
    /// }
    /// ```
    fn load(
        &self,
        cx: &Cx<Self::State>,
        since: Option<Validator>,
    ) -> impl core::future::Future<Output = Result<Load<Self::Object>>>;

    /// The logical id this key anchors: the object kind plus the key's
    /// identity captures in declaration order. [`crate::identity::Facet`]
    /// fields are excluded, which is how multiple route aliases share one
    /// cached object.
    fn anchor(&self) -> LogicalId {
        LogicalId::new(Self::Object::kind(), self.identity_captures())
    }
}

/// Route-context capture metadata for multikey view-leaf expansion.
///
/// Emitted by `#[path_captures]` for each `Facet<T>` field whose `T: PathSegment`
/// exposes a finite `choices()` set.
pub trait FacetMetadata {
    fn facet_axes() -> &'static [FacetAxis];
}

/// A finite facet dimension: the template capture name and its segment choices.
#[derive(Clone, Copy, Debug)]
pub struct FacetAxis {
    pub capture_name: &'static str,
    pub choices: &'static [&'static str],
}

impl FacetMetadata for () {
    fn facet_axes() -> &'static [FacetAxis] {
        &[]
    }
}

/// Project a field leaf from a loaded object value and the route key that
/// selected it: a pure function, no callouts. The SDK runs these eagerly after
/// a fresh load and again on warm reads, so the leaf must be derivable from the
/// canonical object plus route context such as a version facet.
pub type ProjectFn<O> = fn(&O, &<O as Object>::Key) -> Result<FileContent>;
