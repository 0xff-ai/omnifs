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
/// The impl block contains a required synchronous `fn start` and any helper
/// methods. `start` takes either `(config, &mut Router<State>)` or just
/// `(&mut Router<State>)` and returns `Result<State>`. `Config` and `State`
/// are inferred from the `start` signature.
///
/// The manifest (identity, capabilities, config schema, auth) is authored from
/// these arguments and assembled into the `omnifs.provider-metadata.v1` custom
/// section at build: the macro emits a config-less `manifest_json()` lifecycle
/// export that the build tool (`just providers build`) runs and injects. There
/// is no hand-written `omnifs.provider.json`.
///
/// Arguments:
///
/// - `id = ".."` (required): the provider id (mount default and credential
///   namespace). `display_name = ".."` and `mount = ".."` default to `id`. The
///   wasm filename comes from `CARGO_PKG_NAME` and the version from
///   `CARGO_PKG_VERSION`.
/// - `capabilities(domain("v", "why"), git_repo("v", "why"),
///   unix_socket(dynamic, "why"), preopened_path(dynamic, "why"),
///   memory_mb(<int>, "why"))`: the declared capability needs. `unix_socket` and
///   `preopened_path` are dynamic, resolved at mount-start from the
///   [`HostSocket`](../omnifs_sdk/struct.HostSocket.html) /
///   [`HostFile`](../omnifs_sdk/struct.HostFile.html) config field.
/// - `auth = <expr>`: a typed [`omnifs_sdk::auth::Auth`](../omnifs_sdk/auth/struct.Auth.html)
///   value spliced into the manifest's `auth` block.
/// - The config schema is derived from the inferred or explicit config type
///   (via `#[config]`) and spliced in automatically; no argument is needed.
/// - `resources(git = <bool>, memory_mb = <int>)`: requested capabilities
///   exported during initialization.
/// - `events(timer(<Duration expr>, Self::method))`: register a timer
///   handler `async fn method(cx: Cx<State>) -> Result<Invalidation>`; the
///   interval is exported as the manifest's `refresh-interval-secs`. The
///   returned [`Invalidation`](../omnifs_sdk/invalidation/struct.Invalidation.html)
///   lowers to the host invalidation effect channel. Provider events without a
///   registered handler warn and return no invalidations.
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
/// - `key = KeyType` (required): the `#[path_captures]` struct that identifies
///   this object (`KeyType: Key`).
/// - `canonical = Json | Markdown | Atom | Yaml` (default `Json`): the format
///   of the verbatim canonical bytes (`type Canonical`). Non-JSON canonicals
///   require `decode`.
/// - `decode = path::to::fn`: custom `fn(&[u8]) -> Result<Self>` replacing the
///   default `omnifs_sdk::object::decode_json` (which requires the type to be
///   `DeserializeOwned`).
/// - `load = path::to::fn` (default `Self::load`): the provider-written
///   inherent `async fn(cx, key, since) -> Result<Load<Self>>` that
///   `Object::load` forwards to.
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
///   host-managed and declared via `auth = ..` on `#[omnifs_sdk::provider]`;
///   the host materializes them into requests.
/// - `rate_limit = "off"` or `rate_limit = "<seconds>"`: the per-authority
///   429 breaker policy (default cooldown applies when omitted).
///
/// Use via `cx.endpoint::<MyApi>().get("/path")..`.
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
/// JSON bytes; the provider macro deserializes it into the config type inferred
/// from `start`.
#[proc_macro_attribute]
pub fn config(_attr: TokenStream, item: TokenStream) -> TokenStream {
    let input = parse_macro_input!(item as Item);
    match config_macro::config_item_impl(input) {
        Ok(tokens) => tokens.into(),
        Err(error) => error.to_compile_error().into(),
    }
}
