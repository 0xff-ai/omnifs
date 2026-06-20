//! Host-side support for the omnifs virtual filesystem.
//!
//! This crate owns the provider runtime and host data structures that are
//! shared by runtime adapters, including cache records, mount configuration
//! compatibility, path-key indexing, and provider registry wiring.
//!
//! - `registry`: Provider loading and lifecycle management
//! - `mounts`: mount spec loading and runtime resolution
//! - `wit_protocol`: Host view and WIT boundary conversions
//! - `Runtime`, `Instance`, `Namespace`: WASM provider execution and namespace
//!   operation handles

pub mod path_key;
pub mod registry;

mod archive;
pub mod auth;
pub mod blob;
mod blob_cache;
pub(crate) mod callouts;
pub mod capability;
pub mod clock;
pub mod cloner;
mod git;
pub mod http;
mod inflight;
pub mod inspector;
mod instance;
mod invalidation;
pub(crate) mod log_redaction;
mod manifest;
pub(crate) mod materialize;
mod namespace;
mod object_id;
mod op;
mod op_lifecycle;
mod op_validate;
mod operation_ids;
pub mod pagination;
mod projection;
mod runtime;
pub(crate) mod sandbox;
pub mod tools;
mod tree_refs;
mod wasi;
mod wasm;
pub mod wit_protocol;

pub use omnifs_wit::provider::Provider;

pub use instance::Instance;
pub use manifest::Artifact;
pub use materialize::{LookupEntry, LookupOutcome};
pub use op::Op;
pub use runtime::{BuildError, Dirs, Error, Namespace, Runtime, TestOp};
pub use wasm::{component_engine, provider_compiler_strategy};

#[doc(hidden)]
pub use runtime::__test_support;
