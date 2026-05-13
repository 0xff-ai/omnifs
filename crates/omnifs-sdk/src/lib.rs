//! omnifs provider SDK.
//!
//! Provides WIT bindings, helper types, and proc macros for building
//! omnifs providers. Providers depend only on this crate.
//!
//! Usage: `#[omnifs_sdk::config]` on config types, `#[omnifs_sdk::provider]`
//! on a provider lifecycle impl, and `#[dir("...")]`, `#[file("...")]`, or
//! `#[treeref("...")]` on path handlers.

// Generate WIT bindings once; providers import from here.
wit_bindgen::generate!({
    world: "provider",
    path: "../../wit",
    pub_export_macro: true,
});

pub mod archives;
mod async_runtime;
pub mod blob;
pub mod browse;

pub mod cx;
pub mod error;
pub mod file_attrs;
pub mod git;
pub mod handler;
pub mod helpers;
pub mod http;
pub mod init;
pub mod prelude;
pub mod schema;

// Re-export proc macros at the crate root so #[omnifs_sdk::provider] works.
pub use file_attrs::{
    FileAttrs, FileProj, ProjBytes, ReadFileBytes, ReadMode, Size, Stability, VersionToken,
};
pub use handler::{FileChunk, MemoryRangeReader, RangeReader};
pub use omnifs_sdk_macros::Config;
pub use omnifs_sdk_macros::config;
pub use omnifs_sdk_macros::dir;
pub use omnifs_sdk_macros::file;
pub use omnifs_sdk_macros::handlers;
pub use omnifs_sdk_macros::mutate;
pub use omnifs_sdk_macros::provider;
pub use omnifs_sdk_macros::subtree;
pub use omnifs_sdk_macros::treeref;

// Re-export deps that generated code references, so providers don't need
// direct dependencies on them.
pub use hashbrown;
pub use schemars;
pub use serde;
pub use serde_json;

// Re-export Cx at the top level for user convenience.
pub use crate::cx::Cx;

/// Internal types used by generated code. Not part of the public API.
pub mod __internal {
    pub use crate::async_runtime::AsyncRuntime;
    pub use crate::cx::Cx;
    pub use crate::handler::{MountRegistry, SubtreeRegistry};
}

#[cfg(doctest)]
mod removed_api_doctests {
    /// ```compile_fail
    /// use omnifs_sdk::capabilities::Capabilities;
    /// ```
    struct CapabilitiesBuilderRemoved;
}
