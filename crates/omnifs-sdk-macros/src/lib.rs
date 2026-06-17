//! Proc macros for the omnifs provider SDK.
//!
//! Five macros, all top-level (there are no per-route attribute macros;
//! routes are registered imperatively in `start`):
//!
//! - [`macro@provider`] lowers one impl block onto the WIT exports.
//! - [`macro@object`] declares an object's static facts (kind, key,
//!   canonical format, stability).
//! - [`macro@path_captures`] turns a struct into a typed multi-segment
//!   route key.
//! - [`derive@Endpoint`] declares an outbound HTTP host.
//! - [`macro@config`] wires serde onto a provider config type.

use proc_macro::TokenStream;
use syn::{DeriveInput, Item, ItemImpl, ItemStruct, parse_macro_input};

mod captures_macro;
mod config_macro;
mod endpoint_macro;
mod object_macro;
mod provider_macro;

/// The provider entrypoint: lowers one impl block onto the full WIT export
/// surface (lifecycle, namespace, continuation, notify).
///
/// The impl block may contain `type Config = ..` (default
/// [`NoConfig`](../omnifs_sdk/struct.NoConfig.html)), `type State = ..`
/// (default `()`), a required synchronous `fn start`, and any helper
/// methods. `start` takes either `(config, &mut Router<State>)` or just
/// `(&mut Router<State>)` and returns `Result<State>`.
///
/// Arguments (all optional unless noted):
///
/// - `metadata = "omnifs.provider.json"`: path relative to the crate root.
///   The manifest is validated at compile time, stamped with SDK/WIT contract
///   evidence, embedded in the `omnifs.provider-metadata.v1` custom section,
///   and supplies the provider name/description (overridable with
///   `name = ".."` / `description = ".."`; `version = ".."` overrides the
///   crate version).
/// - `resources(git = <bool>, memory_mb = <int>, endpoints = [TypeA, ..])`:
///   requested capabilities. `endpoints` is a declared-intent list of
///   [`Endpoint`](../omnifs_sdk/endpoint/trait.Endpoint.html) types,
///   statically asserted to implement the trait.
/// - `events(timer(<Duration expr>, Self::method))`: register a timer
///   handler `async fn method(cx: Cx<State>) -> Result<Effects>`; the
///   interval is exported as the manifest's `refresh-interval-secs`.
///   Provider events without a registered handler warn and return empty
///   effects.
///
/// What the expansion does, so debugging is not archaeology: it defines the
/// provider type, thread-local `STATE`/`ROUTER`/async-runtime/range-handle
/// slots, an `initialize` that deserializes config, runs `start`, and
/// **seals the router** (overlapping route claims fail initialization), the
/// namespace methods that drive your async handlers through the
/// suspend/resume protocol, and the component `export!`.
#[proc_macro_attribute]
pub fn provider(attr: TokenStream, item: TokenStream) -> TokenStream {
    let args = parse_macro_input!(attr as provider_macro::ProviderArgs);
    let input = parse_macro_input!(item as ItemImpl);
    match provider_macro::provider_impl(&args, input) {
        Ok(tokens) => tokens.into(),
        Err(error) => error.to_compile_error().into(),
    }
}

/// Declare an object's static facts by generating its
/// [`Object`](../omnifs_sdk/object/trait.Object.html) impl.
///
/// Arguments:
///
/// - `kind = "provider.noun"` (required): the canonical-store kind tag,
///   e.g. `"github.issue"`. Stable; changing it orphans cached objects.
/// - `key = KeyType` (required): the `#[path_captures]` struct that loads
///   this object (`KeyType: Key<Object = Self>`).
/// - `canonical = Json | Xml | ..` (default `Json`): the upstream payload's
///   content type. Non-JSON canonicals require `parse`.
/// - `parse = path::to::fn`: custom `fn(&[u8]) -> Result<Self>` replacing
///   the default serde-JSON `parse_canonical`.
///
/// Stability is declared in the object builder, not here: call
/// `o.stable()` / `o.dynamic()` / `o.live()` for a constant, or
/// `o.stability(|key| ..)` when it depends on the key.
#[proc_macro_attribute]
pub fn object(attr: TokenStream, item: TokenStream) -> TokenStream {
    let args = parse_macro_input!(attr as object_macro::ObjectArgs);
    let input = parse_macro_input!(item as ItemStruct);
    match object_macro::object_item_impl(&args, &input) {
        Ok(tokens) => tokens.into(),
        Err(error) => error.to_compile_error().into(),
    }
}

/// Turn a named-field struct into a typed route key by generating
/// `FromCaptures`, `IdentityCaptures`, and `FacetMetadata`.
///
/// Field rules (field name must match the template capture name):
///
/// - `field: T` where `T: FromStr + Display`: parses the segment; a parse
///   failure rejects the route candidate (fallthrough, not an error).
///   Identity capture: participates in the object's logical id.
/// - `field: Facet<T>`: parsed and available to the handler, but excluded
///   from identity, so `/issues/open/7` and `/issues/all/7` share one
///   cached object. If `T::choices()` is finite, the facet contributes a
///   view-leaf expansion axis.
/// - `field: Option<T>`: for a key shared across routes with and without
///   the segment; absent parses as `None`, present-but-invalid still
///   rejects.
/// - `#[flatten] field: OtherKey`: splice a nested key's captures and
///   identity (composition, e.g. a repo key inside an issue key).
#[proc_macro_attribute]
pub fn path_captures(_attr: TokenStream, item: TokenStream) -> TokenStream {
    let input = parse_macro_input!(item as ItemStruct);
    match captures_macro::path_captures_impl(&input) {
        Ok(tokens) => tokens.into(),
        Err(error) => error.to_compile_error().into(),
    }
}

/// Derive an outbound HTTP endpoint declaration.
///
/// `#[endpoint(..)]` keys (string-literal values only):
///
/// - `base = "https://api.example.com"` (required)
/// - `default_header = "Name: Value"` (repeatable; e.g. `Accept`,
///   `User-Agent`). Auth headers are NOT declared here: credentials are
///   host-managed and declared in `omnifs.provider.json`; the host
///   materializes them into requests.
/// - `rate_limit = "off"` or `rate_limit = "<seconds>"`: the per-authority
///   429 breaker policy (default cooldown applies when omitted).
///
/// Use via `cx.endpoint::<MyApi>().get("/path")..` after listing the type
/// in the provider's `resources(endpoints = [..])`.
#[proc_macro_derive(Endpoint, attributes(endpoint))]
pub fn endpoint_derive(item: TokenStream) -> TokenStream {
    let input = parse_macro_input!(item as DeriveInput);
    match endpoint_macro::endpoint_derive_impl(&input) {
        Ok(tokens) => tokens.into(),
        Err(error) => error.to_compile_error().into(),
    }
}

/// Wire serde onto a provider config struct or enum.
///
/// Adds `Debug` + `Deserialize` (through the SDK's re-exported serde) and
/// `deny_unknown_fields`, so a typo in mount JSON fails initialization
/// loudly instead of being silently ignored. Mount config arrives as raw
/// JSON bytes; the provider macro deserializes it into `type Config`.
#[proc_macro_attribute]
pub fn config(_attr: TokenStream, item: TokenStream) -> TokenStream {
    let input = parse_macro_input!(item as Item);
    match config_macro::config_item_impl(input) {
        Ok(tokens) => tokens.into(),
        Err(error) => error.to_compile_error().into(),
    }
}

#[allow(non_snake_case)]
#[doc(hidden)]
#[proc_macro_attribute]
pub fn Config(attr: TokenStream, item: TokenStream) -> TokenStream {
    config(attr, item)
}
