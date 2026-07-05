//! omnifs provider SDK: build a WASM component that teaches the omnifs
//! filesystem a new region of the world.
//!
//! A provider is a `wasm32-wasip2` component implementing the
//! `omnifs:provider` WIT contract. The host mounts it, sends it browse
//! operations (lookup a child, list a directory, read a file), and runs
//! every side effect (HTTP, git, blobs, archives) on the provider's behalf
//! through a strict request/response callout protocol. Providers never open
//! sockets, never touch credentials, and never cache: the host owns trust,
//! caching, and I/O; the provider owns meaning (what paths exist and what
//! bytes they hold).
//!
//! # Anatomy of a provider
//!
//! One `#[omnifs_sdk::provider(..)]` impl block is the whole entry surface:
//!
//! ```ignore
//! #[omnifs_sdk::config]
//! pub struct Config { api_key: String }
//!
//! pub struct State { /* parsed config, adapters, route policy */ }
//!
//! #[omnifs_sdk::provider(
//!     id = "my-provider",
//!     capabilities(domain("api.example.com", "Fetch API resources.")),
//! )]
//! impl MyProvider {
//!     fn start(config: Config, r: &mut Router<State>) -> Result<State> {
//!         r.dir("/").handler(root_list)?;
//!         r.file("/items/{id}/title").handler(read_title)?;
//!         Ok(State::from(config))
//!     }
//! }
//! ```
//!
//! `start` registers routes imperatively on a [`router::Router`] and returns
//! the provider state. The `#[provider]` macro infers the config and state
//! types from that signature. There are no per-route attribute macros; the
//! route topology lives in `start`, readable top to bottom. After `start`
//! returns, the generated glue seals the router (overlapping routes fail
//! initialization loudly) and wires the WIT exports.
//!
//! # The two provider flavours
//!
//! Pick per route family, not per provider (hybrids are normal):
//!
//! - **Object-oriented** (`r.object::<O>(template, |o| ..)`): use when a path
//!   family has one canonical upstream payload (a GitHub issue, a Linear
//!   ticket) and several computed leaves (`title`, `body`, `item.json`). You
//!   implement [`object::Key::load`] once; the SDK emits the canonical-store
//!   effect, the host caches the verbatim upstream bytes, and later reads
//!   re-render from cache without refetching. Identity comes from the key's
//!   captures; route-context captures that must not affect identity (a list
//!   filter, a version selector) are wrapped in [`identity::Facet`].
//! - **Path-oriented** (`r.dir`/`r.file`/`r.treeref` with plain handlers):
//!   use when the path is a direct operation with no stable canonical object
//!   behind it: a DNS query, a Docker daemon listing, a database row read.
//!   Do not invent fake objects for query results; serving fresh bytes with
//!   honest [`file_attrs::Stability`] is the correct behavior.
//!
//! `r.treeref(..)` is the third, narrower verb: hand a whole subtree to the
//! host (a git clone, an extracted archive) by returning a
//! [`handler::TreeRef`]; the host bind-mounts the resolved tree and provider
//! dispatch stops there.
//!
//! # Routes, captures, and dispatch
//!
//! Templates are absolute paths with literal segments and captures:
//! `/items/{id}`, prefix captures like `/@{resolver}` or `/v{version}`, and
//! a trailing multi-segment rest capture `/{*rest}`. A `#[path_captures]`
//! struct gives a route a typed key; each field parses its segment via
//! `FromStr`, and a parse rejection makes the route a non-candidate (falling
//! through to the next-most-specific route, not to "not found"). Types
//! implementing [`captures::PathSegment::choices`] declare finite segment
//! sets, which tightens validation and feeds facet view-leaf expansion for
//! objects.
//!
//! Dispatch rules worth internalizing (they shape what you must and must
//! not write):
//!
//! - Any registered route's literal prefix is auto-navigable: never write
//!   no-op handlers for intermediate directories.
//! - `lookup` is the authoritative name oracle; `readdir` may be
//!   non-exhaustive. A listing's `exhaustive` flag means "these are all the
//!   names I know," and lookup may still resolve names a listing omitted.
//! - Listings merge your enumeration with literal sibling routes registered
//!   at the same depth.
//!
//! # The async model
//!
//! Handlers are plain `async fn`s. Awaiting an HTTP call (or git, blob,
//! archive callout) awaits a WIT async host import. The component runtime
//! suspends the operation while the host runs the effect, then resumes your
//! future with the result. Your code reads as straight-line async; there is no
//! executor, no `Send` bounds, and state is single-threaded by construction.
//! Use [`cx::join_all`] to issue independent callouts concurrently instead of
//! serially.
//!
//! # Caching and effects: the rules
//!
//! The host owns all caching as plain bytes; providers must not add their
//! own caches, LRUs, or TTLs. What you control is what you emit:
//!
//! - Object loads emit canonical-store effects automatically; the host
//!   pushes cached canonical bytes back into `read-file` so re-renders cost
//!   no upstream call.
//! - **Preload discipline:** if an upstream payload in hand already contains
//!   sibling fields or children the user can read next, emit them now
//!   (eager projections, listing entries with attrs) instead of forcing a
//!   refetch later. If the list payload is not the full leaf contract, emit
//!   a deferred file with honest attrs instead of pretending it is.
//! - Freshness is event-driven, not TTL-driven: emit invalidations from
//!   event handlers, or attach [`file_attrs::VersionToken`]s so conditional
//!   reloads (`Load::Unchanged`) are cheap.
//! - [`file_attrs::Stability::Live`] content (changes mid-read) must use
//!   deferred ranged reads; the projection validator enforces this.
//!
//! # Module map
//!
//! | Module | What it owns |
//! |---|---|
//! | [`router`] | Route registration ([`router::Router`]) and dispatch |
//! | [`captures`] | Typed segment parsing, [`captures::Captures`], choices |
//! | [`object`] / [`identity`] | The object model: [`object::Key`], [`object::Load`], logical ids, facets |
//! | [`projection`] | What handlers return: [`projection::DirListing`] (raw dir listings), [`projection::FileProjection`], [`projection::Entry`]; objects list through [`collection::Collection`] |
//! | [`file_attrs`] | Size, stability, version tokens, projection validation |
//! | [`repr`] | Multi-format object representations (`item.md`, `item.json`) |
//! | [`cx`] | Handler context: state access, callout builders, [`cx::join_all`] |
//! | [`endpoint`] | Declared HTTP endpoints: typed request builder, conditional loads, rate-limit breaker |
//! | [`browse`] | Wire-facing results and [`browse::Effects`] |
//! | [`handler`] | Dir intent/cursor types, ranged-read sessions, [`handler::TreeRef`] |
//! | [`blob`] / [`archives`] / [`git`] | Host-side large bytes, archive trees, git clones |
//! | [`error`] | [`error::ProviderError`]: kinds, retryability, HTTP status mapping |
//!
//! Providers depend only on this crate; `hashbrown`, `serde`, and
//! `serde_json` are re-exported for generated code and provider maps (use
//! `hashbrown::HashMap` for provider-internal maps).

#[cfg(not(target_arch = "wasm32"))]
pub use config_resource::ProvidesConfigMetadata;
pub use config_resource::{HostFile, HostSocket};

// The provider metadata block. These wire types live in omnifs-workspace and
// omnifs-caps; a provider constructs and the harvester serializes them entirely
// host-side, so they are re-exported only for non-wasm targets. The wasm guest
// never references them.
#[cfg(not(target_arch = "wasm32"))]
pub use omnifs_caps::{AccessNeed, PreopenMode, PreopenedPath};
#[cfg(not(target_arch = "wasm32"))]
pub use omnifs_caps::{LimitDeclarations, ResourceLimit};
#[cfg(not(target_arch = "wasm32"))]
pub use omnifs_workspace::authn::{
    AmbientKind, AmbientSource, DevicePollCompat, OauthScheme, SchemeGuidance, StaticTokenScheme,
    TokenValidation,
};
#[cfg(not(target_arch = "wasm32"))]
pub use omnifs_workspace::provider::{
    ConfigField, ConfigMetadata, ConfigType, HostResourceBinding, ProviderAuthManifest,
    ProviderManifest,
};

#[doc(hidden)]
pub use omnifs_wit as __wit;
pub use omnifs_wit::provider::{exports, omnifs};

#[macro_export]
macro_rules! export {
    ($($tokens:tt)*) => {
        $crate::__wit::provider::export!($($tokens)*);
    };
}

extern crate self as omnifs_sdk;

/// Version of the SDK crate used by generated provider metadata.
pub const SDK_VERSION: &str = env!("CARGO_PKG_VERSION");

/// WIT package declaration the SDK-generated exports target.
pub const PROVIDER_WIT_PACKAGE: &str = omnifs_wit::PROVIDER_WIT_PACKAGE;

pub mod archives;
pub mod blob;
pub mod browse;
pub mod captures;
pub mod collection;
pub mod config_resource;
pub mod cx;
pub mod endpoint;
pub mod error;
pub mod file_attrs;
pub mod git;
pub mod handler;
pub mod http;
pub mod identity;
pub mod invalidation;
pub mod object;
pub mod prelude;
pub mod projection;
mod range_handles;
mod rate_limit;
pub mod repr;
pub mod router;

// Re-export proc macros at the crate root so #[omnifs_sdk::provider] works.
pub use file_attrs::{
    FileAttrs, FileProj, ProjBytes, ReadFileBytes, ReadMode, Size, Stability, VersionToken,
};
pub use handler::{FileChunk, MemoryRangeReader, RangeReader};
pub use omnifs_core::ContentType;
pub use omnifs_sdk_macros::Endpoint;
pub use omnifs_sdk_macros::config;
pub use omnifs_sdk_macros::object;
pub use omnifs_sdk_macros::path_captures;
pub use omnifs_sdk_macros::path_segment;
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
    pub use crate::cx::Cx;
    pub use crate::range_handles::RangeReaders;
    pub use crate::rate_limit::clear_breaker;
}

/// Empty provider configuration.
///
/// The host sends `{}` when a mount has no provider-specific config. `()` would
/// deserialize from JSON `null`, so providers with no config use `NoConfig`
/// instead.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct NoConfig;

impl<'de> serde::Deserialize<'de> for NoConfig {
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: serde::Deserializer<'de>,
    {
        match <serde_json::Value as serde::Deserialize>::deserialize(deserializer)? {
            serde_json::Value::Null => Ok(Self),
            serde_json::Value::Object(map) if map.is_empty() => Ok(Self),
            _ => Err(serde::de::Error::custom(
                "expected empty provider config object",
            )),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::NoConfig;

    #[test]
    fn no_config_json_absence() {
        for json in ["{}", "null"] {
            assert_eq!(serde_json::from_str::<NoConfig>(json).unwrap(), NoConfig);
        }
        let err = serde_json::from_str::<NoConfig>(r#"{"endpoint":"x"}"#).unwrap_err();
        assert!(
            err.to_string().contains("empty provider config object"),
            "{err}"
        );
    }
}

#[cfg(doctest)]
mod removed_api_doctests {
    /// ```compile_fail
    /// use omnifs_sdk::capabilities::Capabilities;
    /// ```
    struct CapabilitiesBuilderRemoved;
}
