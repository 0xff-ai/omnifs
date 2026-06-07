//! Proc macros for the omnifs provider SDK .
//!
//! `#[provider(resources(..), events(..), metadata = "..")]` lowers a provider
//! impl block (its `type Config/State/Routes` aliases plus a synchronous
//! `start`) onto the WIT exports, building the `Router` once. `#[object(..)]`
//! generates the `Object` impl metadata; `#[path_captures]` generates capture,
//! identity, and facet metadata impls; `#[derive(Endpoint)]` generates the
//! outbound-endpoint impl; `#[config]` wires serde onto a config struct.

use proc_macro::TokenStream;
use syn::{DeriveInput, Item, ItemImpl, ItemStruct, parse_macro_input};

mod captures_macro;
mod config_macro;
mod endpoint_macro;
mod object_macro;
mod provider_macro;

/// Attribute macro for an omnifs provider impl block.
#[proc_macro_attribute]
pub fn provider(attr: TokenStream, item: TokenStream) -> TokenStream {
    let args = parse_macro_input!(attr as provider_macro::ProviderArgs);
    let input = parse_macro_input!(item as ItemImpl);
    match provider_macro::provider_impl(&args, input) {
        Ok(tokens) => tokens.into(),
        Err(error) => error.to_compile_error().into(),
    }
}

/// Attribute macro generating the `Object` metadata impl for an object struct.
#[proc_macro_attribute]
pub fn object(attr: TokenStream, item: TokenStream) -> TokenStream {
    let args = parse_macro_input!(attr as object_macro::ObjectArgs);
    let input = parse_macro_input!(item as ItemStruct);
    match object_macro::object_item_impl(&args, &input) {
        Ok(tokens) => tokens.into(),
        Err(error) => error.to_compile_error().into(),
    }
}

/// Attribute macro generating `FromCaptures` for a path-key struct.
#[proc_macro_attribute]
pub fn path_captures(_attr: TokenStream, item: TokenStream) -> TokenStream {
    let input = parse_macro_input!(item as ItemStruct);
    match captures_macro::path_captures_impl(&input) {
        Ok(tokens) => tokens.into(),
        Err(error) => error.to_compile_error().into(),
    }
}

/// Derive macro generating the `Endpoint` impl from `#[endpoint(..)]` attributes.
#[proc_macro_derive(Endpoint, attributes(endpoint))]
pub fn endpoint_derive(item: TokenStream) -> TokenStream {
    let input = parse_macro_input!(item as DeriveInput);
    match endpoint_macro::endpoint_derive_impl(&input) {
        Ok(tokens) => tokens.into(),
        Err(error) => error.to_compile_error().into(),
    }
}

/// Attribute macro wiring serde derives onto a provider config struct or enum.
#[proc_macro_attribute]
pub fn config(_attr: TokenStream, item: TokenStream) -> TokenStream {
    let input = parse_macro_input!(item as Item);
    match config_macro::config_item_impl(input) {
        Ok(tokens) => tokens.into(),
        Err(error) => error.to_compile_error().into(),
    }
}

#[allow(non_snake_case)]
#[proc_macro_attribute]
pub fn Config(attr: TokenStream, item: TokenStream) -> TokenStream {
    config(attr, item)
}
