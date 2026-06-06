//! omnifs provider SDK.
//!
//! Provides WIT bindings, helper types, and proc macros for building
//! omnifs providers. Providers depend only on this crate.
//!
//! Usage: `#[omnifs_sdk::config]` on config types, `#[omnifs_sdk::provider]`
//! on a provider lifecycle impl, and `#[dir("...")]`, `#[file("...")]`, or
//! `#[treeref("...")]` on path handlers.

#[doc(hidden)]
pub use omnifs_wit as __wit;
pub use omnifs_wit::provider::{exports, omnifs};

#[macro_export]
macro_rules! export {
    ($($tokens:tt)*) => {
        $crate::__wit::provider::export!($($tokens)*);
    };
}

pub mod archives;
mod async_runtime;
pub mod blob;
pub mod browse;
pub mod captures;

pub mod cx;
pub mod endpoint;
pub mod error;
pub mod file_attrs;
pub mod git;
pub mod handler;
pub mod helpers;
pub mod http;
pub mod identity;
pub mod init;
pub mod object;
pub mod prelude;
pub mod projection;
mod range_handles;
mod rate_limit;
pub mod repr;
pub mod router;

// Re-export proc macros at the crate root so #[omnifs_sdk::provider] works.
pub use crate::rate_limit::note_rate_limited;
pub use file_attrs::{
    FileAttrs, FileProj, ProjBytes, ReadFileBytes, ReadMode, Size, Stability, VersionToken,
};
pub use handler::{FileChunk, MemoryRangeReader, RangeReader};
pub use omnifs_core::ContentType;
pub use omnifs_sdk_macros::Config;
pub use omnifs_sdk_macros::Endpoint;
pub use omnifs_sdk_macros::config;
pub use omnifs_sdk_macros::object;
pub use omnifs_sdk_macros::path_captures;
pub use omnifs_sdk_macros::provider;

// Re-export deps that generated code references, so providers don't need
// direct dependencies on them.
pub use hashbrown;
pub use serde;
pub use serde_json;

// Re-export Cx at the top level for user convenience.
pub use crate::cx::Cx;

/// Internal types used by generated code. Not part of the public API.
pub mod __internal {
    pub use crate::async_runtime::AsyncRuntime;
    pub use crate::cx::Cx;
    pub use crate::range_handles::RangeReaders;
    pub use crate::rate_limit::clear_breaker;
}

#[cfg(doctest)]
mod removed_api_doctests {
    /// ```compile_fail
    /// use omnifs_sdk::capabilities::Capabilities;
    /// ```
    struct CapabilitiesBuilderRemoved;
}
