//! The omnifs capability subsystem: the language, data model, invariants, and
//! matching semantics for provider sandboxing, plus the scalar resource-limit
//! model that travels beside capability grants.
//!
//! A provider *manifest* declares the access capabilities a provider
//! [`AccessNeed`]s; a mount *spec* [`Grants`] capabilities; the host resolves
//! grants into an [`Allowlist`] and enforces it on every callout. Scalar
//! resource ceilings are declared as [`LimitDeclarations`] and baked into mount
//! [`Limits`], but they are not capability grants. This crate owns the model and
//! the meaning of "granted", "needed", "allowed", "limited", and "satisfies":
//!
//! - [`AccessNeed`] / [`Grants`] / [`Grant`]: the access data model (manifest
//!   needs, spec grants, and the literal-or-dynamic grant shape).
//! - [`LimitDeclarations`] / [`Limits`]: provider-declared and mount-owned
//!   scalar resource ceilings.
//! - [`Grants::satisfies`]: the invariant that a mount grants at least what its
//!   provider needs, checked at provider start.
//! - [`Allowlist`]: the resolved runtime allowlist and the per-callout decision.
//!
//! Enforcement, resolving a mount's grants into an [`Allowlist`] and calling the
//! decision before each callout, lives in the host, not here.

#![forbid(unsafe_code)]

mod allowlist;
mod check;
mod matching;
mod model;
mod resolve;

pub use allowlist::{Allowlist, Error};
pub use matching::{domain_matches, glob_covers};
pub use model::{
    AccessNeed, DynamicMarker, Grant, Grants, LimitDeclarations, Limits, Missing, PreopenMode,
    PreopenedPath, ResourceLimit,
};
pub use resolve::{EndpointError, endpoint_socket};
