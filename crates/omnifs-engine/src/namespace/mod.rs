//! Namespace facade re-exports and the in-engine [`EngineNamespace`] implementation.
//!
//! Plain types and [`Namespace`] live in [`omnifs_vfs`]. This module re-exports
//! them for engine call sites and hosts the projection-backed implementation.

pub use omnifs_vfs::{
    Attrs, CachedCursor, DirCursor, DirEntry, DirPage, EntryKind, EventStream, LookupAnswer,
    LookupState, Namespace, NsError, NsEvent, NsRetryClass, ReadAnswer, ReadStyle, Stability,
};

#[cfg(feature = "runtime")]
mod implementation;

#[cfg(feature = "runtime")]
pub use implementation::EngineNamespace;
