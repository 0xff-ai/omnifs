//! The `#[provider]` attribute macro.
//!
//! It backs `#[omnifs_sdk::provider(id = "..", capabilities(..), limits(..), auth = .., events(..))]`
//! on a provider impl block whose synchronous `start(..)` method defines the
//! provider config, state, and routes.

use proc_macro2::{Span, TokenStream as TokenStream2};
use quote::{format_ident, quote};
use syn::parse::{Parse, ParseStream};
use syn::spanned::Spanned;
use syn::{
    Expr, FnArg, ImplItem, ImplItemFn, ItemImpl, LitInt, LitStr, Path, PathArguments, Token, Type,
    parse_quote,
};

use crate::util::generic_type_arg;

/// A `timer(Duration, Self::method)` event declaration.
struct TimerSpec {
    interval: Expr,
    handler: Path,
}

pub struct ProviderArgs {
    id: Option<LitStr>,
    display_name: Option<LitStr>,
    description: Option<LitStr>,
    mount: Option<LitStr>,
    capabilities: Vec<omnifs_caps::AccessNeed>,
    limits: omnifs_caps::LimitDeclarations,
    auth: Option<syn::Expr>,
    timer: Option<TimerSpec>,
}

impl ProviderArgs {
    fn requested_capabilities_tokens(&self) -> TokenStream2 {
        let git = self
            .capabilities
            .iter()
            .any(|need| matches!(need, omnifs_caps::AccessNeed::GitRepo { .. }));
        let refresh = if let Some(timer) = &self.timer {
            let interval = &timer.interval;
            quote! { (#interval).as_secs() as u32 }
        } else {
            quote! { 0u32 }
        };
        quote! {
            omnifs_sdk::prelude::RequestedCapabilities {
                needs_git: #git,
                refresh_interval_secs: #refresh,
                ..omnifs_sdk::prelude::RequestedCapabilities::empty()
            }
        }
    }
}

impl Parse for ProviderArgs {
    fn parse(input: ParseStream<'_>) -> syn::Result<Self> {
        let mut args = Self {
            id: None,
            display_name: None,
            description: None,
            mount: None,
            capabilities: Vec::new(),
            limits: omnifs_caps::LimitDeclarations::default(),
            auth: None,
            timer: None,
        };

        while !input.is_empty() {
            let key: syn::Ident = input.parse()?;
            match key.to_string().as_str() {
                "id" => {
                    let _: Token![=] = input.parse()?;
                    args.id = Some(input.parse()?);
                },
                "display_name" => {
                    let _: Token![=] = input.parse()?;
                    args.display_name = Some(input.parse()?);
                },
                "description" => {
                    let _: Token![=] = input.parse()?;
                    if args.description.is_some() {
                        return Err(syn::Error::new(
                            key.span(),
                            "duplicate `description` provider argument",
                        ));
                    }
                    args.description = Some(input.parse()?);
                },
                "mount" => {
                    let _: Token![=] = input.parse()?;
                    args.mount = Some(input.parse()?);
                },
                "capabilities" => {
                    let content;
                    syn::parenthesized!(content in input);
                    args.capabilities = parse_capabilities(&content)?;
                },
                "limits" => {
                    let content;
                    syn::parenthesized!(content in input);
                    args.limits = parse_limits(&content)?;
                },
                "auth" => {
                    let _: Token![=] = input.parse()?;
                    args.auth = Some(input.parse()?);
                },
                "events" => {
                    let content;
                    syn::parenthesized!(content in input);
                    parse_events(&content, &mut args)?;
                },
                _ => {
                    return Err(syn::Error::new(
                        key.span(),
                        "supported provider arguments are `id`/`display_name`/`description`/`mount`, `capabilities(...)`, `limits(...)`, `auth = ...`, and `events(...)`",
                    ));
                },
            }
            if input.peek(Token![,]) {
                let _: Token![,] = input.parse()?;
            }
        }

        Ok(args)
    }
}

/// Placeholder value for a dynamic capability. The concrete value is resolved at
/// mount-start from provider config, so the manifest value is descriptive only
/// and never read.
const DYNAMIC_PLACEHOLDER: &str = "resolved from config at mount-start";

/// Parse `capabilities(domain("v", "why"), domain(dynamic, "why"),
/// unix_socket(dynamic, "why"),
/// preopened_path(dynamic, "why"), ...)` into the manifest's declared
/// `AccessNeed`s. Literal string kinds take `("value", "why")`; dynamic
/// capabilities resolve at mount-start from config fields.
fn parse_capabilities(content: ParseStream<'_>) -> syn::Result<Vec<omnifs_caps::AccessNeed>> {
    let mut needs = Vec::new();
    while !content.is_empty() {
        let kind: syn::Ident = content.parse()?;
        let inner;
        syn::parenthesized!(inner in content);
        let need = match kind.to_string().as_str() {
            "domain" => parse_domain_need(&inner, &kind)?,
            "git_repo" => {
                let (value, why) = parse_literal_need(&inner)?;
                omnifs_caps::AccessNeed::GitRepo {
                    value,
                    why,
                    dynamic: false,
                }
            },
            "unix_socket" => omnifs_caps::AccessNeed::UnixSocket {
                value: DYNAMIC_PLACEHOLDER.to_string(),
                why: parse_dynamic_why(&inner, &kind)?,
                dynamic: true,
            },
            "preopened_path" => omnifs_caps::AccessNeed::PreopenedPath {
                value: omnifs_caps::PreopenedPath {
                    host: DYNAMIC_PLACEHOLDER.to_string(),
                    guest: DYNAMIC_PLACEHOLDER.to_string(),
                    mode: omnifs_caps::PreopenMode::Ro,
                },
                why: parse_dynamic_why(&inner, &kind)?,
                dynamic: true,
            },
            "memory_mb" | "fetch_blob_bytes" | "read_blob_bytes" => {
                return Err(syn::Error::new(
                    kind.span(),
                    format!(
                        "`{kind}` is a scalar resource limit; declare it as `limits({kind}(...))`, not under `capabilities(...)`"
                    ),
                ));
            },
            other => {
                return Err(syn::Error::new(
                    kind.span(),
                    format!(
                        "unsupported capability `{other}`; expected `domain`, `git_repo`, `unix_socket`, or `preopened_path`"
                    ),
                ));
            },
        };
        needs.push(need);
        if content.peek(Token![,]) {
            let _: Token![,] = content.parse()?;
        }
    }
    Ok(needs)
}

/// Parse `limits(memory_mb(32, "why"), fetch_blob_bytes(1048576, "why"), ...)`
/// into provider scalar resource declarations.
fn parse_limits(content: ParseStream<'_>) -> syn::Result<omnifs_caps::LimitDeclarations> {
    let mut limits = omnifs_caps::LimitDeclarations::default();
    while !content.is_empty() {
        let kind: syn::Ident = content.parse()?;
        let inner;
        syn::parenthesized!(inner in content);
        match kind.to_string().as_str() {
            "memory_mb" => {
                reject_duplicate_limit(limits.max_memory_mb.as_ref(), &kind)?;
                let amount: LitInt = inner.parse()?;
                let _: Token![,] = inner.parse()?;
                let why: LitStr = inner.parse()?;
                limits.max_memory_mb = Some(omnifs_caps::ResourceLimit {
                    value: amount.base10_parse::<u32>()?,
                    why: why.value(),
                });
            },
            "fetch_blob_bytes" => {
                reject_duplicate_limit(limits.max_fetch_blob_bytes.as_ref(), &kind)?;
                let amount: LitInt = inner.parse()?;
                let _: Token![,] = inner.parse()?;
                let why: LitStr = inner.parse()?;
                limits.max_fetch_blob_bytes = Some(omnifs_caps::ResourceLimit {
                    value: amount.base10_parse::<u64>()?,
                    why: why.value(),
                });
            },
            "read_blob_bytes" => {
                reject_duplicate_limit(limits.max_read_blob_bytes.as_ref(), &kind)?;
                let amount: LitInt = inner.parse()?;
                let _: Token![,] = inner.parse()?;
                let why: LitStr = inner.parse()?;
                limits.max_read_blob_bytes = Some(omnifs_caps::ResourceLimit {
                    value: amount.base10_parse::<u64>()?,
                    why: why.value(),
                });
            },
            other => {
                return Err(syn::Error::new(
                    kind.span(),
                    format!(
                        "unsupported limit `{other}`; expected `memory_mb`, `fetch_blob_bytes`, or `read_blob_bytes`"
                    ),
                ));
            },
        }
        if content.peek(Token![,]) {
            let _: Token![,] = content.parse()?;
        }
    }
    Ok(limits)
}

fn reject_duplicate_limit<T>(
    field: Option<&omnifs_caps::ResourceLimit<T>>,
    kind: &syn::Ident,
) -> syn::Result<()> {
    if field.is_some() {
        Err(syn::Error::new(
            kind.span(),
            format!("duplicate `{kind}` limit declaration"),
        ))
    } else {
        Ok(())
    }
}

fn parse_literal_need(inner: ParseStream<'_>) -> syn::Result<(String, String)> {
    let value: LitStr = inner.parse()?;
    let _: Token![,] = inner.parse()?;
    let why: LitStr = inner.parse()?;
    Ok((value.value(), why.value()))
}

fn parse_domain_need(
    inner: ParseStream<'_>,
    kind: &syn::Ident,
) -> syn::Result<omnifs_caps::AccessNeed> {
    if inner.peek(syn::Ident) {
        let marker: syn::Ident = inner.parse()?;
        if marker == "dynamic" {
            let _: Token![,] = inner.parse()?;
            let why: LitStr = inner.parse()?;
            return Ok(omnifs_caps::AccessNeed::Domain {
                value: DYNAMIC_PLACEHOLDER.to_string(),
                why: why.value(),
                dynamic: true,
            });
        }
        return Err(syn::Error::new(
            marker.span(),
            format!("unsupported `{kind}` marker `{marker}`; expected `dynamic`"),
        ));
    }

    let (value, why) = parse_literal_need(inner)?;
    Ok(omnifs_caps::AccessNeed::Domain {
        value,
        why,
        dynamic: false,
    })
}

/// Parse a `(dynamic, "why")` capability body. Only the dynamic form is
/// supported for sockets and preopens; the value resolves from a host-resource
/// config field at mount-start.
fn parse_dynamic_why(inner: ParseStream<'_>, kind: &syn::Ident) -> syn::Result<String> {
    let marker: syn::Ident = inner.parse()?;
    if marker != "dynamic" {
        return Err(syn::Error::new(
            marker.span(),
            format!(
                "`{kind}` must be declared `dynamic`; its value resolves at mount-start from the matching host-resource config field"
            ),
        ));
    }
    let _: Token![,] = inner.parse()?;
    let why: LitStr = inner.parse()?;
    Ok(why.value())
}

fn parse_events(content: ParseStream<'_>, args: &mut ProviderArgs) -> syn::Result<()> {
    while !content.is_empty() {
        let key: syn::Ident = content.parse()?;
        match key.to_string().as_str() {
            "timer" => {
                let inner;
                syn::parenthesized!(inner in content);
                let interval: Expr = inner.parse()?;
                let _: Token![,] = inner.parse()?;
                let handler: Path = inner.parse()?;
                args.timer = Some(TimerSpec { interval, handler });
            },
            _ => {
                return Err(syn::Error::new(
                    key.span(),
                    "supported events are `timer(<Duration>, Self::method)`",
                ));
            },
        }
        if content.peek(Token![,]) {
            let _: Token![,] = content.parse()?;
        }
    }
    Ok(())
}

/// The pieces extracted from the provider impl block.
struct ClassifiedImpl {
    config_type: Type,
    state_type: Type,
    start_kind: StartKind,
    methods: Vec<ImplItemFn>,
}

impl ClassifiedImpl {
    fn classify(items: Vec<ImplItem>) -> syn::Result<Self> {
        let mut start_spec = None;
        let mut methods = Vec::new();

        for item in items {
            match item {
                ImplItem::Fn(func) => {
                    if func.sig.ident == "start" {
                        start_spec = Some(StartSpec::classify(&func)?);
                    }
                    methods.push(func);
                },
                other => {
                    return Err(syn::Error::new(
                        other.span(),
                        "unsupported item in #[provider] impl; expected methods only",
                    ));
                },
            }
        }

        let Some(start_spec) = start_spec else {
            return Err(syn::Error::new(
                proc_macro2::Span::call_site(),
                "missing required `fn start(..)`",
            ));
        };

        Ok(Self {
            config_type: start_spec
                .config_type
                .unwrap_or_else(|| parse_quote!(omnifs_sdk::NoConfig)),
            state_type: start_spec.state_type.unwrap_or_else(|| parse_quote!(())),
            start_kind: start_spec.kind,
            methods,
        })
    }
}

struct StartSpec {
    kind: StartKind,
    config_type: Option<Type>,
    state_type: Option<Type>,
}

impl StartSpec {
    fn classify(func: &ImplItemFn) -> syn::Result<Self> {
        let mut inputs = func.sig.inputs.iter().filter_map(|arg| match arg {
            FnArg::Typed(input) => Some(input),
            FnArg::Receiver(_) => None,
        });
        match (inputs.next(), inputs.next(), inputs.next()) {
            (Some(router), None, None) => Ok(Self {
                kind: StartKind::RouterOnly,
                config_type: None,
                state_type: router_state_type(router.ty.as_ref()),
            }),
            (Some(config), Some(router), None) => Ok(Self {
                kind: StartKind::ConfigAndRouter,
                config_type: Some(config.ty.as_ref().clone()),
                state_type: router_state_type(router.ty.as_ref()),
            }),
            _ => Err(syn::Error::new(
                func.sig.span(),
                "`start` must be `fn start(r: &mut Router<..>) -> Result<..>` or `fn start(config, r: &mut Router<..>) -> Result<..>`",
            )),
        }
    }
}

#[derive(Clone, Copy)]
enum StartKind {
    ConfigAndRouter,
    RouterOnly,
}

fn router_state_type(ty: &Type) -> Option<Type> {
    let Type::Reference(reference) = ty else {
        return None;
    };
    let Type::Path(path) = reference.elem.as_ref() else {
        return None;
    };
    let segment = path.path.segments.last()?;
    if segment.ident != "Router" {
        return None;
    }
    match &segment.arguments {
        PathArguments::AngleBracketed(_) => generic_type_arg(segment, 0).cloned(),
        PathArguments::None => Some(parse_quote!(())),
        PathArguments::Parenthesized(_) => None,
    }
}

struct ManifestFacts {
    name: String,
    display_name: String,
    description: Option<String>,
    default_mount: String,
    provider_file: String,
    version: Option<String>,
}

impl ManifestFacts {
    /// Build the manifest base from `#[provider(..)]` annotations.
    fn from_args(args: &ProviderArgs) -> syn::Result<Self> {
        let id = args
            .id
            .as_ref()
            .expect("caller checks `id` is present")
            .value();
        let display_name = args
            .display_name
            .as_ref()
            .map_or_else(|| id.clone(), syn::LitStr::value);
        let default_mount = args
            .mount
            .as_ref()
            .map_or_else(|| id.clone(), syn::LitStr::value);
        let pkg_name = std::env::var("CARGO_PKG_NAME").map_err(|error| {
            syn::Error::new(
                Span::call_site(),
                format!("CARGO_PKG_NAME is not set: {error}"),
            )
        })?;

        Ok(Self {
            name: id,
            display_name,
            description: args.description.as_ref().map(syn::LitStr::value),
            default_mount,
            provider_file: format!("{}.wasm", pkg_name.replace('-', "_")),
            version: std::env::var("CARGO_PKG_VERSION").ok(),
        })
    }

    fn provider_info_tokens(&self) -> TokenStream2 {
        let name = &self.name;
        let display_name = &self.display_name;
        quote! {
            omnifs_sdk::prelude::ProviderInfo {
                name: #name.to_string(),
                version: env!("CARGO_PKG_VERSION").to_string(),
                description: #display_name.to_string(),
            }
        }
    }

    fn metadata_tokens(
        &self,
        config_type: &Type,
        capabilities: &[omnifs_caps::AccessNeed],
        limits: &omnifs_caps::LimitDeclarations,
        auth: Option<&syn::Expr>,
    ) -> TokenStream2 {
        let id = LitStr::new(&self.name, Span::call_site());
        let display_name = LitStr::new(&self.display_name, Span::call_site());
        let description = self.description.as_ref().map_or_else(
            || quote! { None },
            |description| {
                let description = LitStr::new(description, Span::call_site());
                quote! { Some(#description.to_string()) }
            },
        );
        let provider_file = LitStr::new(&self.provider_file, Span::call_site());
        let default_mount = LitStr::new(&self.default_mount, Span::call_site());
        let version = self.version.as_ref().map_or_else(
            || quote! { None },
            |version| {
                let version = LitStr::new(version, Span::call_site());
                quote! { Some(#version.to_string()) }
            },
        );
        let capability_entries = capabilities.iter().map(capability_tokens);
        let limits = limit_declarations_tokens(limits);
        let auth = auth.map_or_else(|| quote! { None }, |auth| quote! { Some(#auth) });
        quote! {
            omnifs_sdk::ProviderManifest {
                id: #id.to_string(),
                display_name: #display_name.to_string(),
                description: #description,
                provider: #provider_file.to_string(),
                default_mount: #default_mount.to_string(),
                version: #version,
                wit_package: Some(omnifs_sdk::PROVIDER_WIT_PACKAGE.to_string()),
                sdk_version: Some(omnifs_sdk::SDK_VERSION.to_string()),
                capabilities: ::std::vec![#(#capability_entries),*],
                limits: #limits,
                auth: #auth,
                config: <#config_type as omnifs_sdk::ProvidesConfigMetadata>::metadata(),
            }
        }
    }
}

fn capability_tokens(need: &omnifs_caps::AccessNeed) -> TokenStream2 {
    // A dynamic socket/preopen need resolves its concrete value from a config
    // field at mount-start; the placeholder mirrors the host's marker.
    let dynamic_placeholder = DYNAMIC_PLACEHOLDER;
    match need {
        omnifs_caps::AccessNeed::Domain {
            value,
            why,
            dynamic,
            ..
        } => {
            let value = LitStr::new(value, Span::call_site());
            let why = LitStr::new(why, Span::call_site());
            quote! { omnifs_sdk::AccessNeed::Domain { value: #value.to_string(), why: #why.to_string(), dynamic: #dynamic } }
        },
        omnifs_caps::AccessNeed::GitRepo { value, why, .. } => {
            let value = LitStr::new(value, Span::call_site());
            let why = LitStr::new(why, Span::call_site());
            quote! { omnifs_sdk::AccessNeed::GitRepo { value: #value.to_string(), why: #why.to_string(), dynamic: false } }
        },
        omnifs_caps::AccessNeed::UnixSocket { why, .. } => {
            let why = LitStr::new(why, Span::call_site());
            quote! { omnifs_sdk::AccessNeed::UnixSocket { value: #dynamic_placeholder.to_string(), why: #why.to_string(), dynamic: true } }
        },
        omnifs_caps::AccessNeed::PreopenedPath { why, .. } => {
            let why = LitStr::new(why, Span::call_site());
            quote! {
                omnifs_sdk::AccessNeed::PreopenedPath {
                    value: omnifs_sdk::PreopenedPath {
                        host: #dynamic_placeholder.to_string(),
                        guest: #dynamic_placeholder.to_string(),
                        mode: omnifs_sdk::PreopenMode::default(),
                    },
                    why: #why.to_string(),
                    dynamic: true,
                }
            }
        },
    }
}

fn limit_declarations_tokens(limits: &omnifs_caps::LimitDeclarations) -> TokenStream2 {
    let max_memory_mb = optional_limit_tokens(limits.max_memory_mb.as_ref());
    let max_fetch_blob_bytes = optional_limit_tokens(limits.max_fetch_blob_bytes.as_ref());
    let max_read_blob_bytes = optional_limit_tokens(limits.max_read_blob_bytes.as_ref());
    quote! {
        omnifs_sdk::LimitDeclarations {
            max_memory_mb: #max_memory_mb,
            max_fetch_blob_bytes: #max_fetch_blob_bytes,
            max_read_blob_bytes: #max_read_blob_bytes,
        }
    }
}

fn optional_limit_tokens<T>(limit: Option<&omnifs_caps::ResourceLimit<T>>) -> TokenStream2
where
    T: quote::ToTokens,
{
    limit.map_or_else(
        || quote! { None },
        |limit| {
            let value = &limit.value;
            let why = LitStr::new(&limit.why, Span::call_site());
            quote! {
                Some(omnifs_sdk::ResourceLimit {
                    value: #value,
                    why: #why.to_string(),
                })
            }
        },
    )
}

fn provider_metadata_impl_tokens(metadata: &TokenStream2) -> TokenStream2 {
    quote! {
        /// Build-time accessor for the provider's metadata. The native metadata
        /// harvester links this crate as a library, calls this, and serializes
        /// the result verbatim into the wasm `omnifs.provider-metadata.v1`
        /// section. Never compiled into the wasm guest.
        #[cfg(not(target_arch = "wasm32"))]
        #[doc(hidden)]
        #[must_use]
        pub fn provider_metadata() -> omnifs_sdk::ProviderManifest {
            #metadata
        }
    }
}

// Codegen aggregator: each argument is a distinct token source for the lifecycle
// export block; bundling them would only move the destructuring elsewhere.
#[allow(clippy::too_many_arguments)]
fn generate_lifecycle(
    type_name: &syn::Ident,
    config_type: &Type,
    state_type: &Type,
    start_kind: StartKind,
    info_tokens: &TokenStream2,
    caps_tokens: &TokenStream2,
) -> TokenStream2 {
    let start_call = match start_kind {
        StartKind::ConfigAndRouter => quote! { #type_name::start(config, &mut router) },
        StartKind::RouterOnly => quote! {
            {
                let _ = config;
                #type_name::start(&mut router)
            }
        },
    };
    quote! {
        impl omnifs_sdk::exports::omnifs::provider::lifecycle::Guest for #type_name {
            fn initialize(config_bytes: Vec<u8>) -> omnifs_sdk::prelude::ProviderReturn {
                let config: #config_type = match omnifs_sdk::serde_json::from_slice(&config_bytes) {
                    Ok(config) => config,
                    Err(error) => {
                        return omnifs_sdk::prelude::ProviderReturn::from(
                            omnifs_sdk::error::ProviderError::invalid_input(format!("config error: {error}"))
                        );
                    },
                };
                let mut router = omnifs_sdk::prelude::Router::<#state_type>::new();
                let state = match #start_call {
                    Ok(state) => state,
                    Err(error) => return omnifs_sdk::prelude::ProviderReturn::from(error),
                };
                let router = match router.compile() {
                    Ok(router) => router,
                    Err(error) => return omnifs_sdk::prelude::ProviderReturn::from(error),
                };
                STATE.with(|slot| {
                    *slot.borrow_mut() = Some(std::rc::Rc::new(core::cell::RefCell::new(state)));
                });
                ROUTER.with(|slot| {
                    *slot.borrow_mut() = Some(std::rc::Rc::new(router));
                });
                let info = #info_tokens;
                let capabilities = #caps_tokens;
                omnifs_sdk::prelude::ProviderReturn::terminal(
                    omnifs_sdk::prelude::OpResult::Initialize(
                        omnifs_sdk::omnifs::provider::types::InitializeResult { info, capabilities }
                    )
                )
            }

            fn shutdown() {
                STATE.with(|slot| *slot.borrow_mut() = None);
                ROUTER.with(|slot| *slot.borrow_mut() = None);
                RANGE_HANDLES.with(|handles| handles.clear());
                omnifs_sdk::__internal::clear_breaker();
            }
        }
    }
}

fn generate_state_management(state_type: &Type) -> TokenStream2 {
    quote! {
        thread_local! {
            static STATE: core::cell::RefCell<Option<std::rc::Rc<core::cell::RefCell<#state_type>>>>
                = const { core::cell::RefCell::new(None) };
            static ROUTER: core::cell::RefCell<
                Option<std::rc::Rc<omnifs_sdk::prelude::CompiledRouter<#state_type>>>
            > = const { core::cell::RefCell::new(None) };
            static RANGE_HANDLES: omnifs_sdk::__internal::RangeReaders =
                const { omnifs_sdk::__internal::RangeReaders::new() };
        }

        fn state_handle() -> core::result::Result<
            std::rc::Rc<core::cell::RefCell<#state_type>>,
            String,
        > {
            STATE.with(|slot| {
                slot.borrow().as_ref().cloned().ok_or_else(|| "provider not initialized".to_string())
            })
        }

        fn router_handle() -> core::result::Result<
            std::rc::Rc<omnifs_sdk::prelude::CompiledRouter<#state_type>>,
            String,
        > {
            ROUTER.with(|slot| {
                slot.borrow().as_ref().cloned().ok_or_else(|| "provider not initialized".to_string())
            })
        }
    }
}

fn generate_namespace(type_name: &syn::Ident, state_type: &Type) -> TokenStream2 {
    quote! {
        impl omnifs_sdk::exports::omnifs::provider::namespace::Guest for #type_name {
            async fn lookup_child(id: u64, parent_path: String, name: String) -> omnifs_sdk::prelude::ProviderReturn {
                let (Ok(state), Ok(router)) = (state_handle(), router_handle()) else {
                    return omnifs_sdk::prelude::ProviderReturn::from(
                        omnifs_sdk::error::ProviderError::internal("provider not initialized")
                    );
                };
                let cx = omnifs_sdk::__internal::Cx::<#state_type>::new(id, state);
                match router.lookup_child(&cx, &parent_path, &name).await {
                    Ok(outcome) => {
                        let (result, effects) = outcome.into_result_and_effects();
                        omnifs_sdk::prelude::ProviderReturn::with_effects(
                            omnifs_sdk::prelude::OpResult::LookupChild(result),
                            effects.into_wit(),
                        )
                    },
                    Err(error) => omnifs_sdk::prelude::ProviderReturn::from(error),
                }
            }

            async fn list_children(
                id: u64,
                path: String,
                cached_validator: Option<String>,
                cursor: Option<omnifs_sdk::omnifs::provider::types::Cursor>,
            ) -> omnifs_sdk::prelude::ProviderReturn {
                let (Ok(state), Ok(router)) = (state_handle(), router_handle()) else {
                    return omnifs_sdk::prelude::ProviderReturn::from(
                        omnifs_sdk::error::ProviderError::internal("provider not initialized")
                    );
                };
                let version = cached_validator
                    .as_ref()
                    .map(|v| omnifs_sdk::file_attrs::VersionToken::from(v.as_str()));
                let sdk_cursor = cursor.map(omnifs_sdk::prelude::Cursor::from);
                let cx = omnifs_sdk::__internal::Cx::<#state_type>::new(id, state).with_version(version);
                match router.list_children(&cx, &path, cached_validator, sdk_cursor).await {
                    Ok(outcome) => {
                        let (result, effects) = outcome.into_result_and_effects();
                        omnifs_sdk::prelude::ProviderReturn::with_effects(
                            omnifs_sdk::prelude::OpResult::ListChildren(result),
                            effects.into_wit(),
                        )
                    },
                    Err(error) => omnifs_sdk::prelude::ProviderReturn::from(error),
                }
            }

            async fn read_file(
                id: u64,
                path: String,
                content_type: String,
                cached_canonical: Option<omnifs_sdk::omnifs::provider::types::CanonicalInput>,
            ) -> omnifs_sdk::prelude::ProviderReturn {
                let (Ok(state), Ok(router)) = (state_handle(), router_handle()) else {
                    return omnifs_sdk::prelude::ProviderReturn::from(
                        omnifs_sdk::error::ProviderError::internal("provider not initialized")
                    );
                };
                let cached = cached_canonical
                    .map(omnifs_sdk::browse::CachedCanonical::from_wit);
                let cx = omnifs_sdk::__internal::Cx::<#state_type>::new(id, state);
                match router.read_file(&cx, &path, &content_type, cached).await {
                    Ok(outcome) => {
                        let (result, effects) = outcome.into_result_and_effects();
                        omnifs_sdk::prelude::ProviderReturn::with_effects(
                            omnifs_sdk::prelude::OpResult::ReadFile(result),
                            effects.into_wit(),
                        )
                    },
                    Err(error) => omnifs_sdk::prelude::ProviderReturn::from(error),
                }
            }

            async fn open_file(id: u64, path: String) -> omnifs_sdk::prelude::ProviderReturn {
                let (Ok(state), Ok(router)) = (state_handle(), router_handle()) else {
                    return omnifs_sdk::prelude::ProviderReturn::from(
                        omnifs_sdk::error::ProviderError::internal("provider not initialized")
                    );
                };
                let cx = omnifs_sdk::__internal::Cx::<#state_type>::new(id, state);
                match router.open_file(&cx, &path).await {
                    Ok(opened) => {
                        let Some(handle) = RANGE_HANDLES.with(|handles| {
                            handles.allocate(opened.reader)
                        }) else {
                            return omnifs_sdk::prelude::ProviderReturn::from(
                                omnifs_sdk::error::ProviderError::internal("no free ranged file handles")
                            );
                        };
                        omnifs_sdk::prelude::ProviderReturn::terminal(
                            omnifs_sdk::prelude::OpResult::OpenFile(
                                omnifs_sdk::omnifs::provider::types::OpenFileResult {
                                    handle: handle.get(),
                                    attrs: opened.attrs.into(),
                                },
                            )
                        )
                    },
                    Err(error) => omnifs_sdk::prelude::ProviderReturn::from(error),
                }
            }

            async fn read_chunk(id: u64, handle: u64, offset: u64, len: u32) -> omnifs_sdk::prelude::ProviderReturn {
                let Ok(state) = state_handle() else {
                    return omnifs_sdk::prelude::ProviderReturn::from(
                        omnifs_sdk::error::ProviderError::internal("provider not initialized")
                    );
                };
                let Some(handle_id) = ::std::num::NonZeroU64::new(handle) else {
                    return omnifs_sdk::prelude::ProviderReturn::from(
                        omnifs_sdk::error::ProviderError::not_found(
                            format!("unknown file handle {handle}")
                        )
                    );
                };
                let Some(reader) = RANGE_HANDLES.with(|handles| handles.get(handle_id)) else {
                    return omnifs_sdk::prelude::ProviderReturn::from(
                        omnifs_sdk::error::ProviderError::not_found(
                            format!("unknown file handle {handle}")
                        )
                    );
                };
                let cx = omnifs_sdk::__internal::Cx::<#state_type>::new(id, state);
                let read_cx = cx.erase_state();
                match reader.read_chunk(&read_cx, offset, len).await {
                    Ok(chunk) => omnifs_sdk::prelude::ProviderReturn::terminal(
                        omnifs_sdk::prelude::OpResult::ReadChunk(chunk.into())
                    ),
                    Err(error) => omnifs_sdk::prelude::ProviderReturn::from(error),
                }
            }

            fn close_file(handle: u64) {
                if let Some(handle_id) = ::std::num::NonZeroU64::new(handle) {
                    RANGE_HANDLES.with(|handles| handles.remove(handle_id));
                }
            }
        }
    }
}

fn generate_notify(
    type_name: &syn::Ident,
    state_type: &Type,
    timer: Option<&TimerSpec>,
) -> TokenStream2 {
    let warn_ignored_event = quote! {
        omnifs_sdk::omnifs::provider::log::log(&omnifs_sdk::omnifs::provider::types::LogEntry {
            level: omnifs_sdk::omnifs::provider::types::LogLevel::Warn,
            message: format!(
                "ignored provider event `{}`: no handler registered",
                event.name()
            ),
        });
    };
    let dispatch = if let Some(timer) = timer {
        let method = timer
            .handler
            .segments
            .last()
            .map(|segment| segment.ident.clone());
        let method = method.unwrap_or_else(|| format_ident!("on_tick"));
        quote! {
            match &event {
                omnifs_sdk::omnifs::provider::types::ProviderEvent::TimerTick => {
                    match #type_name::#method(future_cx).await {
                        Ok(inv) => omnifs_sdk::prelude::ProviderReturn::with_effects(
                            omnifs_sdk::prelude::OpResult::OnEvent,
                            inv.into_effects().into_wit(),
                        ),
                        Err(error) => omnifs_sdk::prelude::ProviderReturn::from(error),
                    }
                },
                _ => {
                    #warn_ignored_event
                    omnifs_sdk::prelude::ProviderReturn::with_effects(
                        omnifs_sdk::prelude::OpResult::OnEvent,
                        omnifs_sdk::prelude::Effects::new().into_wit(),
                    )
                },
            }
        }
    } else {
        quote! {
            let _ = future_cx;
            #warn_ignored_event
            omnifs_sdk::prelude::ProviderReturn::with_effects(
                omnifs_sdk::prelude::OpResult::OnEvent,
                omnifs_sdk::prelude::Effects::new().into_wit(),
            )
        }
    };

    quote! {
        impl omnifs_sdk::exports::omnifs::provider::notify::Guest for #type_name {
            async fn on_event(
                id: u64,
                event: omnifs_sdk::prelude::ProviderEvent,
            ) -> omnifs_sdk::prelude::ProviderReturn {
                let Ok(state) = state_handle() else {
                    return omnifs_sdk::prelude::ProviderReturn::from(
                        omnifs_sdk::error::ProviderError::internal("provider not initialized")
                    );
                };
                let future_cx = omnifs_sdk::__internal::Cx::<#state_type>::new(id, state);
                #dispatch
            }
        }
    }
}

pub(crate) fn provider_impl(args: &ProviderArgs, input: ItemImpl) -> syn::Result<TokenStream2> {
    let type_name = match &*input.self_ty {
        Type::Path(path) => path
            .path
            .segments
            .last()
            .map(|segment| segment.ident.clone()),
        _ => None,
    }
    .ok_or_else(|| syn::Error::new(input.self_ty.span(), "expected a named type"))?;

    let classified = ClassifiedImpl::classify(input.items)?;
    let config_type = &classified.config_type;
    let state_type = &classified.state_type;
    let start_kind = classified.start_kind;
    let methods = &classified.methods;

    if args.id.is_none() {
        return Err(syn::Error::new(
            Span::call_site(),
            "provider needs an `id = \"...\"` annotation",
        ));
    }
    let manifest = ManifestFacts::from_args(args)?;
    let info_tokens = manifest.provider_info_tokens();
    let caps_tokens = args.requested_capabilities_tokens();
    let metadata = manifest.metadata_tokens(
        config_type,
        &args.capabilities,
        &args.limits,
        args.auth.as_ref(),
    );
    let provider_metadata = provider_metadata_impl_tokens(&metadata);

    let state_management = generate_state_management(state_type);
    let lifecycle = generate_lifecycle(
        &type_name,
        config_type,
        state_type,
        start_kind,
        &info_tokens,
        &caps_tokens,
    );
    let namespace = generate_namespace(&type_name, state_type);
    let notify = generate_notify(&type_name, state_type, args.timer.as_ref());

    Ok(quote! {
        struct #type_name;
        #[doc(hidden)]
        pub(crate) type __OmnifsProviderState = #state_type;

        #state_management

        impl #type_name {
            #(#methods)*
        }

        #provider_metadata
        #lifecycle
        #namespace
        #notify

        #[cfg(target_arch = "wasm32")]
        omnifs_sdk::export!(#type_name with_types_in omnifs_sdk);
    })
}
