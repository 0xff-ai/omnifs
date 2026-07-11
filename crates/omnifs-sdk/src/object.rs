//! Object model: typed canonical resources addressed by typed keys.
//!
//! An [`Object`] is a value assembled from one replayable canonical payload (a
//! GitHub issue, an arXiv paper). It owns identity context ([`Key`]), how to
//! fetch itself ([`Object::load`]), how to read its stored bytes back
//! ([`Object::decode`]), and what format those bytes are in
//! ([`Object::Canonical`]). Register it on an object route and the SDK handles
//! the cache protocol: it emits canonical-store effects on fresh loads,
//! re-renders from host-pushed canonical bytes on warm reads, and answers
//! conditional reloads through the `since` validator.
//!
//! `load` and `decode` live on `Object` because the key is pure identity and
//! route context. The canonical content type is an associated [`Format`], and a
//! private transport representation may live inside `decode` without imposing a
//! `serde` bound on the object.

use crate::cx::Cx;
use crate::error::{ProviderError, Result};
use crate::file_attrs::VersionToken;
use crate::identity::{IdentityCaptures, LogicalId};
use crate::projection::FileProjection;
use crate::repr::Format;
use omnifs_core::ContentType;
use std::future::Future;

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
/// Verbatim means exactly that: the raw response body, or a deterministic
/// logical-object assembly that [`Object::decode`] consumes byte-for-byte, not
/// a convenience re-encoding of the parsed value. These bytes are what the host
/// stores and later pushes back through `Object::decode`, so any normalization
/// here breaks warm re-rendering.
pub struct Canonical {
    pub bytes: Vec<u8>,
    pub validator: Option<Validator>,
}

impl Canonical {
    pub fn new(bytes: impl Into<Vec<u8>>, validator: Option<Validator>) -> Self {
        Self {
            bytes: bytes.into(),
            validator,
        }
    }
}

/// The outcome of an object's [`Object::load`].
///
/// - `Fresh`: a new payload was fetched; carry the parsed value plus the
///   verbatim canonical bytes and their validator. `preloads` carries any
///   sibling objects or files derivable from the SAME payload (see
///   [`Load::preload_object`] / [`Load::preload_file`]); it is empty for the
///   common single-object case.
/// - `Unchanged`: a conditional request matched the `since` validator
///   (HTTP 304 shape); the host's cached canonical is still current and the SDK
///   serves from it. Return this only when `since` was given.
/// - `NotFound`: the upstream says the object does not exist. Lowered to a
///   not-found terminal, not an error.
pub enum Load<T> {
    Fresh {
        value: T,
        canonical: Canonical,
        preloads: Preloads,
    },
    Unchanged,
    NotFound,
}

impl<T> Load<T> {
    /// A fresh value with its verbatim canonical payload and no preloads.
    pub fn fresh(value: T, canonical: Canonical) -> Self {
        Self::Fresh {
            value,
            canonical,
            preloads: Preloads::default(),
        }
    }

    /// Preload a sibling file derivable from the same payload (the `project`
    /// effect). Only inline/deferred sources lower; a `Body`/`Ranged`/`Blob`
    /// source is rejected when the preload is lowered (serve those through
    /// their own face).
    #[must_use]
    pub fn preload_file(mut self, path: impl Into<String>, file: FileProjection) -> Self {
        if let Self::Fresh { preloads, .. } = &mut self {
            preloads.files.push((path.into(), file));
        }
        self
    }
}

impl<O: Object> Load<O> {
    /// Preload a SAME-TYPE sibling object the load payload already contained
    /// (Oura's date-range window: one fetch materializes neighboring days).
    /// The sibling's canonical is stored against its own logical id with the
    /// sibling object's faces expanded as view leaves. Cross-type preloads are
    /// not supported in this pass.
    #[must_use]
    pub fn preload_object(mut self, entry: ObjectEntry<O>) -> Self {
        if let Self::Fresh { preloads, .. } = &mut self {
            preloads.objects.push(ObjectPreload {
                identity_captures: entry.key.identity_captures(),
                canonical: entry.canonical,
            });
        }
        self
    }
}

/// Typed preloads accumulated on a [`Load::Fresh`], lowered onto
/// [`crate::browse::Effects`] by the object dispatch path (which holds the
/// route template, faces, and facet expansion needed to compute sibling paths
/// and view leaves).
#[derive(Default)]
pub struct Preloads {
    pub(crate) objects: Vec<ObjectPreload>,
    pub(crate) files: Vec<(String, FileProjection)>,
}

impl Preloads {
    /// Decompose into the typed object and file preloads, for the object
    /// dispatch path that lowers them onto `Effects`.
    pub(crate) fn into_parts(self) -> (Vec<ObjectPreload>, Vec<(String, FileProjection)>) {
        (self.objects, self.files)
    }
}

/// A same-type sibling object preload, type-erased to its identity captures
/// plus canonical bytes. Dispatch reconstructs its anchor id and view-leaf
/// paths from the requested object's `O::kind()`, route template, and faces.
pub(crate) struct ObjectPreload {
    pub(crate) identity_captures: Vec<(&'static str, String)>,
    pub(crate) canonical: Canonical,
}

/// A sibling object entry for [`Load::preload_object`]: the sibling key and its
/// canonical bytes.
pub struct ObjectEntry<O: Object> {
    pub(crate) key: O::Key,
    pub(crate) canonical: Canonical,
}

impl<O: Object> ObjectEntry<O> {
    pub fn fresh(key: O::Key, canonical: Canonical) -> Self {
        Self { key, canonical }
    }
}

/// A typed canonical resource. Usually paired with
/// `#[omnifs_sdk::object(kind = "...", key = ...)]`, which emits everything
/// except [`Self::load`] (a hand-written async fn the macro forwards to).
///
/// [`Self::decode`] must accept the exact bytes [`Self::load`] stored in
/// [`Canonical`]: it runs again on every warm read when the host pushes cached
/// canonical bytes back. A private transport DTO may live inside `decode`; it
/// must not be part of the public object API.
pub trait Object: Sized {
    /// Identity + route context. Loads do not live here.
    type Key: Key;
    /// Provider state threaded through [`Self::load`].
    type State;
    /// The format of the verbatim canonical bytes. The canonical face serves
    /// these bytes as-is under [`Format::CT`].
    type Canonical: Format;

    /// Fetch the object from upstream, honoring `since` as a conditional
    /// validator (map it to `If-None-Match`). Return `Load::Fresh` with the
    /// parsed value plus the verbatim canonical bytes and validator;
    /// `Load::Unchanged` when `since` matched (304); `Load::NotFound` for
    /// genuine absence. Reserve `Err` for failures.
    fn load(
        cx: &Cx<Self::State>,
        key: &Self::Key,
        since: Option<Validator>,
    ) -> impl Future<Output = Result<Load<Self>>>;

    /// Parse the verbatim canonical bytes back into a value. Failures surface
    /// as invalid-input; never normalize bytes here, fix [`Self::load`] to
    /// store the right bytes instead.
    fn decode(bytes: &[u8]) -> Result<Self>;

    /// The stable kind tag; part of every [`LogicalId`] derived for this
    /// object, so renaming it orphans previously cached objects.
    fn kind() -> ObjectKind;

    /// The canonical bytes' content type, from [`Self::Canonical`].
    fn canonical_ct() -> ContentType {
        <Self::Canonical as Format>::CT
    }
}

/// The identity side of an object: route-context captures and the logical id
/// they anchor. Keys are `#[path_captures]` structs (the macro emits an empty
/// `impl Key`); only the captures and facets are declared, never behavior.
pub trait Key: FromCaptures + IdentityCaptures + FacetMetadata + Sized {
    /// The logical id this key anchors under `kind`: the object kind plus the
    /// key's identity captures in declaration order. [`crate::identity::Facet`]
    /// fields are excluded, which is how multiple route aliases share one
    /// cached object.
    fn anchor(&self, kind: ObjectKind) -> LogicalId {
        LogicalId::new(kind, self.identity_captures())
    }
}

use crate::captures::FromCaptures;

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

/// Decode helper for the default JSON canonical: providers whose canonical is
/// `Json` and whose type is `DeserializeOwned` get this as their `decode` from
/// the `#[object]` macro.
pub fn decode_json<T: serde::de::DeserializeOwned>(bytes: &[u8]) -> Result<T> {
    serde_json::from_slice(bytes)
        .map_err(|e| ProviderError::invalid_input(format!("canonical decode: {e}")))
}
