//! The `#[provider]` attribute macro.
//!
//! It backs `#[omnifs_sdk::provider(id = "..", capabilities(..), limits(..), auth = .., events(..))]`
//! on a provider impl block whose synchronous `start(..)` method defines the
//! provider config, state, and routes.

use proc_macro2::{Span, TokenStream as TokenStream2};
use quote::{format_ident, quote, quote_spanned};
use serde_json::Value;
use syn::parse::{Parse, ParseStream};
use syn::spanned::Spanned;
use syn::{
    FnArg, ImplItem, ImplItemFn, ItemImpl, LitInt, LitStr, Path, PathArguments, Token, Type,
    parse_quote,
};

use crate::util::{BytePiece, byte_array_tokens, generic_type_arg};

/// A `timer(seconds, Self::method)` event declaration.
struct TimerSpec {
    interval_secs: u32,
    handler: Path,
}

enum ParsedAccessNeed {
    Domain {
        value: String,
        why: String,
        dynamic: bool,
    },
    GitRepo {
        value: String,
        why: String,
    },
    UnixSocket {
        why: String,
    },
    PreopenedPath {
        why: String,
    },
}

struct ParsedResourceLimit<T> {
    value: T,
    why: String,
}

#[derive(Default)]
struct ParsedLimitDeclarations {
    max_memory_mb: Option<ParsedResourceLimit<u32>>,
    max_fetch_blob_bytes: Option<ParsedResourceLimit<u64>>,
}

impl ParsedLimitDeclarations {
    fn is_empty(&self) -> bool {
        self.max_memory_mb.is_none() && self.max_fetch_blob_bytes.is_none()
    }
}

pub struct ProviderArgs {
    id: Option<LitStr>,
    display_name: Option<LitStr>,
    description: Option<LitStr>,
    mount: Option<LitStr>,
    capabilities: Vec<ParsedAccessNeed>,
    limits: ParsedLimitDeclarations,
    auth: Option<LitStr>,
    timer: Option<TimerSpec>,
}

impl Parse for ProviderArgs {
    fn parse(input: ParseStream<'_>) -> syn::Result<Self> {
        let mut args = Self {
            id: None,
            display_name: None,
            description: None,
            mount: None,
            capabilities: Vec::new(),
            limits: ParsedLimitDeclarations::default(),
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
                    args.auth = Some(parse_auth_literal(input)?);
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
fn parse_capabilities(content: ParseStream<'_>) -> syn::Result<Vec<ParsedAccessNeed>> {
    let mut needs = Vec::new();
    while !content.is_empty() {
        let kind: syn::Ident = content.parse()?;
        let inner;
        syn::parenthesized!(inner in content);
        let need = match kind.to_string().as_str() {
            "domain" => parse_domain_need(&inner, &kind)?,
            "git_repo" => {
                let (value, why) = parse_literal_need(&inner)?;
                ParsedAccessNeed::GitRepo { value, why }
            },
            "unix_socket" => ParsedAccessNeed::UnixSocket {
                why: parse_dynamic_why(&inner, &kind)?,
            },
            "preopened_path" => ParsedAccessNeed::PreopenedPath {
                why: parse_dynamic_why(&inner, &kind)?,
            },
            "memory_mb" | "fetch_blob_bytes" => {
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
fn parse_limits(content: ParseStream<'_>) -> syn::Result<ParsedLimitDeclarations> {
    let mut limits = ParsedLimitDeclarations::default();
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
                limits.max_memory_mb = Some(ParsedResourceLimit {
                    value: amount.base10_parse::<u32>()?,
                    why: why.value(),
                });
            },
            "fetch_blob_bytes" => {
                reject_duplicate_limit(limits.max_fetch_blob_bytes.as_ref(), &kind)?;
                let amount: LitInt = inner.parse()?;
                let _: Token![,] = inner.parse()?;
                let why: LitStr = inner.parse()?;
                limits.max_fetch_blob_bytes = Some(ParsedResourceLimit {
                    value: amount.base10_parse::<u64>()?,
                    why: why.value(),
                });
            },
            other => {
                return Err(syn::Error::new(
                    kind.span(),
                    format!(
                        "unsupported limit `{other}`; expected `memory_mb` or `fetch_blob_bytes`"
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
    field: Option<&ParsedResourceLimit<T>>,
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

fn parse_domain_need(inner: ParseStream<'_>, kind: &syn::Ident) -> syn::Result<ParsedAccessNeed> {
    if inner.peek(syn::Ident) {
        let marker: syn::Ident = inner.parse()?;
        if marker == "dynamic" {
            let _: Token![,] = inner.parse()?;
            let why: LitStr = inner.parse()?;
            return Ok(ParsedAccessNeed::Domain {
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
    Ok(ParsedAccessNeed::Domain {
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
                let interval: LitInt = inner.parse()?;
                let _: Token![,] = inner.parse()?;
                let handler: Path = inner.parse()?;
                args.timer = Some(TimerSpec {
                    interval_secs: interval.base10_parse()?,
                    handler,
                });
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

fn parse_auth_literal(input: ParseStream<'_>) -> syn::Result<LitStr> {
    let literal: LitStr = input.parse()?;
    let value: Value = serde_json::from_str(&literal.value()).map_err(|error| {
        syn::Error::new(
            literal.span(),
            format!("auth declaration must be valid JSON: {error}"),
        )
    })?;
    let Value::Object(object) = &value else {
        return Err(syn::Error::new(
            literal.span(),
            "auth declaration must be a JSON object",
        ));
    };
    if !matches!(object.get("default"), Some(Value::String(_))) {
        return Err(syn::Error::new(
            literal.span(),
            "auth declaration requires a string default",
        ));
    }
    if !matches!(object.get("schemes"), Some(Value::Array(schemes)) if !schemes.is_empty()) {
        return Err(syn::Error::new(
            literal.span(),
            "auth declaration requires a non-empty schemes array",
        ));
    }
    Ok(literal)
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

    fn metadata_tokens(
        &self,
        config_type: &Type,
        capabilities: &[ParsedAccessNeed],
        limits: &ParsedLimitDeclarations,
        auth: Option<&LitStr>,
        refresh_interval_secs: u32,
        has_config: bool,
    ) -> syn::Result<TokenStream2> {
        let mut fields = Vec::new();
        fields.push(("id", json_string(&self.name)));
        fields.push(("displayName", json_string(&self.display_name)));
        if let Some(description) = &self.description {
            fields.push(("description", json_string(description)));
        }
        fields.push(("provider", json_string(&self.provider_file)));
        fields.push(("defaultMount", json_string(&self.default_mount)));
        if let Some(version) = &self.version {
            fields.push(("version", json_string(version)));
        }
        fields.push(("witPackage", json_string("package omnifs:provider@0.6.0;")));
        fields.push(("sdkVersion", json_string(env!("CARGO_PKG_VERSION"))));
        fields.push(("refreshIntervalSecs", refresh_interval_secs.to_string()));
        if !capabilities.is_empty() {
            fields.push(("capabilities", capabilities_json(capabilities)));
        }
        if !limits.is_empty() {
            fields.push(("limits", limits_json(limits)));
        }
        if let Some(auth) = auth {
            let value: Value = serde_json::from_str(&auth.value()).map_err(|error| {
                syn::Error::new(auth.span(), format!("invalid auth declaration: {error}"))
            })?;
            fields.push((
                "auth",
                serde_json::to_string(&value).expect("auth JSON serializes"),
            ));
        }

        let mut pieces = vec![MetadataPiece::Static("{".to_string())];
        for (index, (key, value)) in fields.iter().enumerate() {
            if index != 0 {
                pieces.push(MetadataPiece::Static(",".to_string()));
            }
            pieces.push(MetadataPiece::Static(format!(
                "{}:{}",
                json_string(key),
                value
            )));
        }
        if has_config {
            if !fields.is_empty() {
                pieces.push(MetadataPiece::Static(",".to_string()));
            }
            pieces.push(MetadataPiece::Static(r#""config":{"fields":"#.to_string()));
            pieces.push(MetadataPiece::Config(config_type.clone()));
            pieces.push(MetadataPiece::Static("}}".to_string()));
        } else {
            pieces.push(MetadataPiece::Static("}".to_string()));
        }
        Ok(metadata_bytes_tokens(&pieces))
    }
}

enum MetadataPiece {
    Static(String),
    Config(Type),
}

fn metadata_bytes_tokens(pieces: &[MetadataPiece]) -> TokenStream2 {
    let byte_pieces = pieces
        .iter()
        .map(|piece| match piece {
            MetadataPiece::Static(value) => {
                let length = syn::LitInt::new(&value.len().to_string(), Span::call_site());
                let value = LitStr::new(value, Span::call_site());
                BytePiece {
                    length: quote! { #length },
                    copy: quote! {
                        omnifs_sdk::__internal::copy_bytes(
                            &mut bytes,
                            &mut offset,
                            (#value).as_bytes(),
                        );
                    },
                }
            },
            MetadataPiece::Config(ty) => BytePiece {
                length: quote_spanned! {ty.span()=>
                    <#ty as omnifs_sdk::ConfigMetadataBytes>::LEN
                },
                copy: quote_spanned! {ty.span()=>
                    omnifs_sdk::__internal::copy_bytes(
                        &mut bytes,
                        &mut offset,
                        <#ty as omnifs_sdk::ConfigMetadataBytes>::JSON,
                    );
                },
            },
        })
        .collect::<Vec<_>>();
    let length_terms = byte_pieces.iter().map(|piece| &piece.length);
    let length = quote! { 0usize #(+ #length_terms)* };
    let bytes = byte_array_tokens(&byte_pieces, &length);
    quote! {
        const __OMNIFS_PROVIDER_METADATA_LEN: usize = #length;
        const __OMNIFS_PROVIDER_METADATA_BYTES: [u8; __OMNIFS_PROVIDER_METADATA_LEN] = #bytes;
        #[cfg(target_arch = "wasm32")]
        #[used]
        #[unsafe(link_section = "omnifs.provider-metadata.v1")]
        static __OMNIFS_PROVIDER_METADATA_SECTION: [u8; __OMNIFS_PROVIDER_METADATA_LEN] =
            __OMNIFS_PROVIDER_METADATA_BYTES;
    }
}

fn json_string(value: &str) -> String {
    serde_json::to_string(value).expect("metadata string serializes")
}

fn capabilities_json(capabilities: &[ParsedAccessNeed]) -> String {
    let values = capabilities
        .iter()
        .map(capability_value)
        .collect::<Vec<_>>();
    serde_json::to_string(&values).expect("capabilities serialize")
}

fn capability_value(need: &ParsedAccessNeed) -> Value {
    let dynamic_placeholder = DYNAMIC_PLACEHOLDER;
    match need {
        ParsedAccessNeed::Domain {
            value,
            why,
            dynamic,
        } => serde_json::json!({
            "kind": "domain",
            "value": value,
            "why": why,
            "dynamic": dynamic,
        }),
        ParsedAccessNeed::GitRepo { value, why } => serde_json::json!({
            "kind": "gitRepo",
            "value": value,
            "why": why,
            "dynamic": false,
        }),
        ParsedAccessNeed::UnixSocket { why } => serde_json::json!({
            "kind": "unixSocket",
            "value": dynamic_placeholder,
            "why": why,
            "dynamic": true,
        }),
        ParsedAccessNeed::PreopenedPath { why } => serde_json::json!({
            "kind": "preopenedPath",
            "value": {
                "host": dynamic_placeholder,
                "guest": dynamic_placeholder,
                "mode": "ro",
            },
            "why": why,
            "dynamic": true,
        }),
    }
}

fn limits_json(limits: &ParsedLimitDeclarations) -> String {
    let mut fields = Vec::new();
    if let Some(limit) = &limits.max_memory_mb {
        fields.push(format!(
            "{}:{{\"value\":{},\"why\":{}}}",
            json_string("maxMemoryMb"),
            limit.value,
            json_string(&limit.why)
        ));
    }
    if let Some(limit) = &limits.max_fetch_blob_bytes {
        fields.push(format!(
            "{}:{{\"value\":{},\"why\":{}}}",
            json_string("maxFetchBlobBytes"),
            limit.value,
            json_string(&limit.why)
        ));
    }
    format!("{{{}}}", fields.join(","))
}

// Codegen aggregator: each argument is a distinct token source for the lifecycle
// export block; bundling them would only move the destructuring elsewhere.
#[allow(clippy::too_many_arguments)]
fn generate_lifecycle(
    type_name: &syn::Ident,
    config_type: &Type,
    state_type: &Type,
    start_kind: StartKind,
) -> TokenStream2 {
    let start_call = match start_kind {
        StartKind::ConfigAndRouter => quote! { #type_name::start(config, &mut builder) },
        StartKind::RouterOnly => quote! {
            {
                let _ = config;
                #type_name::start(&mut builder)
            }
        },
    };
    quote! {
        impl omnifs_sdk::exports::omnifs::provider::lifecycle::Guest for #type_name {
            fn initialize(
                config_bytes: Vec<u8>,
            ) -> (
                core::result::Result<
                    (),
                    omnifs_sdk::omnifs::provider::types::ProviderError,
                >,
                omnifs_sdk::omnifs::provider::types::Effects,
            ) {
                let config: #config_type = match omnifs_sdk::serde_json::from_slice(&config_bytes) {
                    Ok(config) => config,
                    Err(error) => {
                        return (
                            Err(omnifs_sdk::error::ProviderError::invalid_input(
                                format!("config error: {error}"),
                            ).into()),
                            omnifs_sdk::prelude::Effects::new().into_wit(),
                        );
                    },
                };
                let mut builder = omnifs_sdk::prelude::Router::<#state_type>::new();
                let state = match #start_call {
                    Ok(state) => state,
                    Err(error) => {
                        return (
                            Err(error.into()),
                            omnifs_sdk::prelude::Effects::new().into_wit(),
                        );
                    },
                };
                let router = match builder.compile() {
                    Ok(router) => router,
                    Err(error) => {
                        return (
                            Err(error.into()),
                            omnifs_sdk::prelude::Effects::new().into_wit(),
                        );
                    },
                };
                STATE.with(|slot| {
                    *slot.borrow_mut() = Some(std::rc::Rc::new(core::cell::RefCell::new(state)));
                });
                ROUTER.with(|slot| {
                    *slot.borrow_mut() = Some(std::rc::Rc::new(router));
                });
                (
                    Ok(()),
                    omnifs_sdk::prelude::Effects::new().into_wit(),
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
            async fn lookup_child(
                id: u64,
                parent_path: String,
                name: String,
            ) -> (
                core::result::Result<
                    omnifs_sdk::omnifs::provider::types::LookupChildResult,
                    omnifs_sdk::omnifs::provider::types::ProviderError,
                >,
                omnifs_sdk::omnifs::provider::types::Effects,
            ) {
                let (Ok(state), Ok(router)) = (state_handle(), router_handle()) else {
                    return (
                        Err(omnifs_sdk::error::ProviderError::internal(
                            "provider not initialized",
                        ).into()),
                        omnifs_sdk::prelude::Effects::new().into_wit(),
                    );
                };
                let cx = omnifs_sdk::__internal::Cx::<#state_type>::new(id, state);
                match router.lookup_child(&cx, &parent_path, &name).await {
                    Ok(outcome) => {
                        let (result, effects) = outcome.into_result_and_effects();
                        (Ok(result), effects.into_wit())
                    },
                    Err(error) => (
                        Err(error.into()),
                        omnifs_sdk::prelude::Effects::new().into_wit(),
                    ),
                }
            }

            async fn list_children(
                id: u64,
                path: String,
                cached_validator: Option<String>,
                cursor: Option<omnifs_sdk::omnifs::provider::types::Cursor>,
            ) -> (
                core::result::Result<
                    omnifs_sdk::omnifs::provider::types::ListChildrenResult,
                    omnifs_sdk::omnifs::provider::types::ProviderError,
                >,
                omnifs_sdk::omnifs::provider::types::Effects,
            ) {
                let (Ok(state), Ok(router)) = (state_handle(), router_handle()) else {
                    return (
                        Err(omnifs_sdk::error::ProviderError::internal(
                            "provider not initialized",
                        ).into()),
                        omnifs_sdk::prelude::Effects::new().into_wit(),
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
                        (Ok(result), effects.into_wit())
                    },
                    Err(error) => (
                        Err(error.into()),
                        omnifs_sdk::prelude::Effects::new().into_wit(),
                    ),
                }
            }

            async fn read_file(
                id: u64,
                path: String,
                content_type: String,
                cached_canonical: Option<omnifs_sdk::omnifs::provider::types::CanonicalInput>,
            ) -> (
                core::result::Result<
                    omnifs_sdk::omnifs::provider::types::ReadFileOutcome,
                    omnifs_sdk::omnifs::provider::types::ProviderError,
                >,
                omnifs_sdk::omnifs::provider::types::Effects,
            ) {
                let (Ok(state), Ok(router)) = (state_handle(), router_handle()) else {
                    return (
                        Err(omnifs_sdk::error::ProviderError::internal(
                            "provider not initialized",
                        ).into()),
                        omnifs_sdk::prelude::Effects::new().into_wit(),
                    );
                };
                let cached = cached_canonical
                    .map(omnifs_sdk::browse::CachedCanonical::from_wit);
                let cx = omnifs_sdk::__internal::Cx::<#state_type>::new(id, state);
                match router.read_file(&cx, &path, &content_type, cached).await {
                    Ok(outcome) => {
                        let (result, effects) = outcome.into_result_and_effects();
                        (Ok(result), effects.into_wit())
                    },
                    Err(error) => (
                        Err(error.into()),
                        omnifs_sdk::prelude::Effects::new().into_wit(),
                    ),
                }
            }

            async fn open_file(
                id: u64,
                path: String,
            ) -> (
                core::result::Result<
                    omnifs_sdk::omnifs::provider::types::OpenFileResult,
                    omnifs_sdk::omnifs::provider::types::ProviderError,
                >,
                omnifs_sdk::omnifs::provider::types::Effects,
            ) {
                let (Ok(state), Ok(router)) = (state_handle(), router_handle()) else {
                    return (
                        Err(omnifs_sdk::error::ProviderError::internal(
                            "provider not initialized",
                        ).into()),
                        omnifs_sdk::prelude::Effects::new().into_wit(),
                    );
                };
                let cx = omnifs_sdk::__internal::Cx::<#state_type>::new(id, state);
                match router.open_file(&cx, &path).await {
                    Ok(opened) => {
                        let Some(handle) = RANGE_HANDLES.with(|handles| {
                            handles.allocate(opened.reader)
                        }) else {
                            return (
                                Err(omnifs_sdk::error::ProviderError::internal(
                                    "no free ranged file handles",
                                ).into()),
                                omnifs_sdk::prelude::Effects::new().into_wit(),
                            );
                        };
                        (
                            Ok(omnifs_sdk::omnifs::provider::types::OpenFileResult {
                                handle: handle.get(),
                                attrs: opened.attrs.into(),
                            }),
                            omnifs_sdk::prelude::Effects::new().into_wit(),
                        )
                    },
                    Err(error) => (
                        Err(error.into()),
                        omnifs_sdk::prelude::Effects::new().into_wit(),
                    ),
                }
            }

            async fn read_chunk(
                id: u64,
                handle: u64,
                offset: u64,
                len: u32,
            ) -> (
                core::result::Result<
                    omnifs_sdk::omnifs::provider::types::ReadChunkResult,
                    omnifs_sdk::omnifs::provider::types::ProviderError,
                >,
                omnifs_sdk::omnifs::provider::types::Effects,
            ) {
                let Ok(state) = state_handle() else {
                    return (
                        Err(omnifs_sdk::error::ProviderError::internal(
                            "provider not initialized",
                        ).into()),
                        omnifs_sdk::prelude::Effects::new().into_wit(),
                    );
                };
                let Some(handle_id) = ::std::num::NonZeroU64::new(handle) else {
                    return (
                        Err(omnifs_sdk::error::ProviderError::not_found(
                            format!("unknown file handle {handle}"),
                        ).into()),
                        omnifs_sdk::prelude::Effects::new().into_wit(),
                    );
                };
                let Some(reader) = RANGE_HANDLES.with(|handles| handles.get(handle_id)) else {
                    return (
                        Err(omnifs_sdk::error::ProviderError::not_found(
                            format!("unknown file handle {handle}"),
                        ).into()),
                        omnifs_sdk::prelude::Effects::new().into_wit(),
                    );
                };
                let cx = omnifs_sdk::__internal::Cx::<#state_type>::new(id, state);
                let read_cx = cx.erase_state();
                match reader.read_chunk(&read_cx, offset, len).await {
                    Ok(chunk) => (
                        Ok(chunk.into()),
                        omnifs_sdk::prelude::Effects::new().into_wit(),
                    ),
                    Err(error) => (
                        Err(error.into()),
                        omnifs_sdk::prelude::Effects::new().into_wit(),
                    ),
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
                        Ok(inv) => (
                            Ok(()),
                            inv.into_effects().into_wit(),
                        ),
                        Err(error) => (
                            Err(error.into()),
                            omnifs_sdk::prelude::Effects::new().into_wit(),
                        ),
                    }
                },
                _ => {
                    #warn_ignored_event
                    (Ok(()), omnifs_sdk::prelude::Effects::new().into_wit())
                },
            }
        }
    } else {
        quote! {
            let _ = future_cx;
            #warn_ignored_event
            (Ok(()), omnifs_sdk::prelude::Effects::new().into_wit())
        }
    };

    quote! {
        impl omnifs_sdk::exports::omnifs::provider::notify::Guest for #type_name {
            async fn on_event(
                id: u64,
                event: omnifs_sdk::prelude::ProviderEvent,
            ) -> (
                core::result::Result<(), omnifs_sdk::omnifs::provider::types::ProviderError>,
                omnifs_sdk::omnifs::provider::types::Effects,
            ) {
                let Ok(state) = state_handle() else {
                    return (
                        Err(omnifs_sdk::error::ProviderError::internal(
                            "provider not initialized",
                        ).into()),
                        omnifs_sdk::prelude::Effects::new().into_wit(),
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
    let refresh_interval_secs = args.timer.as_ref().map_or(0, |timer| timer.interval_secs);
    let metadata = manifest.metadata_tokens(
        config_type,
        &args.capabilities,
        &args.limits,
        args.auth.as_ref(),
        refresh_interval_secs,
        matches!(start_kind, StartKind::ConfigAndRouter),
    )?;

    let state_management = generate_state_management(state_type);
    let lifecycle = generate_lifecycle(&type_name, config_type, state_type, start_kind);
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

        #metadata
        #lifecycle
        #namespace
        #notify

        #[cfg(target_arch = "wasm32")]
        omnifs_sdk::export!(#type_name with_types_in omnifs_sdk);
    })
}
