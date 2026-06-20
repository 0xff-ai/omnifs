//! Path resolution for the omnifs CLI.
//!
//! The canonical layout and resolution logic live in `omnifs_home`; this
//! module re-exports those types for command code. CLI-specific factories for
//! config, catalogs, mount enumeration, and daemon clients live in
//! `crate::workspace`.

pub use omnifs_home::{PathOverrides, Paths, ResolveError};
