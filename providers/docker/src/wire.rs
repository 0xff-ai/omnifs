//! Docker daemon wire types, re-exported from `bollard-stubs`.
//!
//! Upgrade discipline: the `bollard-stubs` README explicitly says these
//! data stubs "might change as needed by the parent project. Not
//! intended for direct library consumption." The crate's version is
//! pinned exactly in `Cargo.toml` (`=1.53.1-rc.29.3.1`); a bump is a
//! small migration, not a routine `cargo update`. When you bump:
//!
//! 1. Diff the upstream `bollard-stubs` `lib.rs` between the two
//!    versions, focusing on the types re-exported below and the
//!    nested fields we touch in `system.rs` / `containers.rs` /
//!    `compose.rs` / `events.rs`.
//! 2. Adapt the readers/writers if a field renamed, moved, or changed
//!    optionality. The compiler will flag most of this; semantic
//!    drift (a field's meaning changing under a stable name) is what
//!    you have to read for.
//!
//! We re-export only the handful of types we actually touch so a
//! version bump's blast radius is grep-able: any new model needed
//! gets a deliberate addition here.

pub use bollard_stubs::models::{
    ContainerInspectResponse, ContainerSummary, EventMessage, SystemDataUsageResponse, SystemInfo,
    SystemVersion,
};
