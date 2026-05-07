//! Proc macros for the omnifs provider SDK.
//!
//! `#[provider]` processes a provider lifecycle impl block and stitches
//! together handler modules declared in `#[provider(mounts(...))]`.
//!
//! Inside `#[handlers] impl T { ... }`, `#[dir]`, `#[file]`,
//! `#[treeref]`, `#[bind]`, and reserved `#[mutate]` annotate methods
//! that become path handlers. `#[bind]` dispatches into a typed subtree
//! defined by `#[subtree] impl B { ... }`.

use proc_macro::TokenStream;
use syn::{Item, ItemFn, ItemImpl, parse_macro_input};

const HANDLERS_ATTRIBUTE_SCOPE_ERROR: &str =
    "must be used inside an `impl` block annotated with #[omnifs_sdk::handlers]";

mod config_macro;
mod handler_macro;
mod provider_macro;
mod subtree_macro;

/// Attribute macro for omnifs provider impl blocks.
#[proc_macro_attribute]
pub fn provider(attr: TokenStream, item: TokenStream) -> TokenStream {
    let args = parse_macro_input!(attr as provider_macro::ProviderArgs);
    let input = parse_macro_input!(item as ItemImpl);
    match provider_macro::provider_impl(&args, input) {
        Ok(tokens) => tokens.into(),
        Err(error) => error.to_compile_error().into(),
    }
}

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

fn handlers_attr_scope_error() -> TokenStream {
    syn::Error::new(
        proc_macro2::Span::call_site(),
        HANDLERS_ATTRIBUTE_SCOPE_ERROR,
    )
    .to_compile_error()
    .into()
}

#[proc_macro_attribute]
pub fn dir(_attr: TokenStream, item: TokenStream) -> TokenStream {
    let _func = parse_macro_input!(item as ItemFn);
    handlers_attr_scope_error()
}

#[proc_macro_attribute]
pub fn file(_attr: TokenStream, item: TokenStream) -> TokenStream {
    let _func = parse_macro_input!(item as ItemFn);
    handlers_attr_scope_error()
}

#[proc_macro_attribute]
pub fn treeref(_attr: TokenStream, item: TokenStream) -> TokenStream {
    let _func = parse_macro_input!(item as ItemFn);
    handlers_attr_scope_error()
}

#[proc_macro_attribute]
pub fn bind(_attr: TokenStream, item: TokenStream) -> TokenStream {
    let _func = parse_macro_input!(item as ItemFn);
    handlers_attr_scope_error()
}

#[proc_macro_attribute]
pub fn handlers(attr: TokenStream, item: TokenStream) -> TokenStream {
    let args = parse_macro_input!(attr as handler_macro::HandlersArgs);
    let input = parse_macro_input!(item as ItemImpl);
    match handler_macro::expand_handlers(&args, input) {
        Ok(tokens) => tokens.into(),
        Err(error) => error.to_compile_error().into(),
    }
}

#[proc_macro_attribute]
pub fn subtree(attr: TokenStream, item: TokenStream) -> TokenStream {
    let args = parse_macro_input!(attr as subtree_macro::SubtreeArgs);
    let input = parse_macro_input!(item as ItemImpl);
    match subtree_macro::expand_subtree(&args, input) {
        Ok(tokens) => tokens.into(),
        Err(error) => error.to_compile_error().into(),
    }
}

#[proc_macro_attribute]
pub fn mutate(attr: TokenStream, item: TokenStream) -> TokenStream {
    let func = parse_macro_input!(item as ItemFn);
    match handler_macro::expand_handler(
        handler_macro::HandlerKind::Mutate,
        parse_macro_input!(attr as handler_macro::HandlerArgs),
        func,
    ) {
        Ok(tokens) => tokens.into(),
        Err(error) => error.to_compile_error().into(),
    }
}

#[proc_macro]
pub fn mounts(item: TokenStream) -> TokenStream {
    let _ = item;
    syn::Error::new(
        proc_macro2::Span::call_site(),
        "mounts! has been removed; declare free-function handlers with #[dir]/#[file]/#[subtree] and mount modules from #[omnifs_sdk::provider]",
    )
    .to_compile_error()
    .into()
}
