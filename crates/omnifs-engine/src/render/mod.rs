//! Whole-file materialization policy shared by the tree read path.
//!
//! `omnifs_engine::namespace::TreeNamespace` owns shared structural `Path` identity,
//! epoch fan-out, and ranged-handle caching. Frontends retain their own protocol
//! identity. This module owns only the materialization cap.

pub mod attrs;

pub use attrs::MATERIALIZE_MAX_BYTES;
