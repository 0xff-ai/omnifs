//! Whole-file materialization policy shared by the tree read path.
//!
//! The per-frontend identity, follow-size, and invalidation scaffolding that once
//! lived here was superseded by `omnifs_engine::namespace::TreeNamespace`, which
//! owns its own id table, epoch fan-out, and ranged-handle cache. Only the
//! materialization cap remains.

pub mod attrs;

pub use attrs::MATERIALIZE_MAX_BYTES;
