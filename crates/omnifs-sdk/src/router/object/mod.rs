//! Object route registration, read/list path, and view-leaf expansion.
//!
//! An object route binds a typed [`Object`] to a template: the key parsed
//! from the captures both identifies the resource ([`Key::anchor`]) and
//! loads it ([`Object::load`]). The block passed to `r.object::<O>(..)`
//! declares the anchor's faces ([`ObjectBlock`]):
//!
//! - [`FileFace`] (`o.file(name)`): a `canonical`, `representation`,
//!   `derive`, `object`, `direct`, `blob`, or `stream` leaf.
//! - [`DirFace`] (`o.dir(name)`): a `collection`, `choices`, `children`, or
//!   `tree` child topology.
//!
//! The cache contract (the host owns all caching; the SDK only emits
//! effects): a fresh [`Object::load`] produces a `canonical-store` effect
//! carrying the verbatim upstream bytes, the validator, and the expanded
//! view-leaf paths that map back to the object's logical id. On a later read
//! the host pushes those bytes back as a [`CachedCanonical`] and the SDK
//! re-renders without an upstream call. Facets (identity-neutral captures
//! with finite choices) multiply the view leaves, so loading
//! `/issues/open/7/title.txt` also teaches the host
//! `/issues/all/7/title.txt`.

mod dispatch;
mod serve;
mod spec;

pub use spec::{DirFace, FileFace, ObjectBlock, ObjectHandle, object};

pub(super) use dispatch::{
    AnchorCollection, CollectionTopology, FacetExpansion, ObjectReadTarget, ObjectRouteEntry,
    SourceLeafAttrs, mount_object,
};
pub(crate) use dispatch::{EntryView, ResolvedChildView};
pub(super) use spec::{CollectionHandler, file_object};
