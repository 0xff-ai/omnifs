//! The omnifs capability subsystem: the language, data model, invariants, and
//! matching semantics for provider sandboxing.
//!
//! A provider *manifest* declares the capabilities a provider [`Need`]s; a
//! mount *spec* [`Grants`] capabilities; the host resolves grants into an
//! [`Allowlist`] and enforces it on every callout. This crate owns the model
//! and the meaning of "granted", "needed", "allowed", and "satisfies":
//!
//! - [`Need`] / [`Grants`] / [`Grant`]: the data model (manifest needs, spec
//!   grants, and the literal-or-dynamic grant shape).
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
pub use model::{DynamicMarker, Grant, Grants, Missing, Need, PreopenMode, PreopenedPath};
pub use resolve::{EndpointError, endpoint_socket};
