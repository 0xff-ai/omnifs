//! The `#[provider]` attribute macro.
//!
//! It backs `#[omnifs_sdk::provider(resources(..), events(..), metadata = "..")]`
//! on a provider impl block whose optional associated `type Config/State`
//! aliases and a synchronous `start(..)` method define the provider.

use proc_macro2::{Span, TokenStream as TokenStream2};
use quote::{format_ident, quote};
use syn::parse::{Parse, ParseStream};
use syn::spanned::Spanned;
use syn::{
    Expr, FnArg, ImplItem, ImplItemFn, ItemImpl, LitBool, LitInt, LitStr, Path, Token, Type,
    parse_quote,
};

/// A `timer(Duration, Self::method)` event declaration.
struct TimerSpec {
    interval: Expr,
    handler: Path,
}

pub struct ProviderArgs {
    metadata_path: Option<LitStr>,
    id: Option<LitStr>,
    display_name: Option<LitStr>,
    mount: Option<LitStr>,
    capabilities: Vec<omnifs_caps::Need>,
    git: bool,
    timer: Option<TimerSpec>,
}

impl Parse for ProviderArgs {
    fn parse(input: ParseStream<'_>) -> syn::Result<Self> {
        let mut args = Self {
            metadata_path: None,
            id: None,
            display_name: None,
            mount: None,
            capabilities: Vec::new(),
            git: false,
            timer: None,
        };

        while !input.is_empty() {
            let key: syn::Ident = input.parse()?;
            match key.to_string().as_str() {
                "metadata" => {
                    let _: Token![=] = input.parse()?;
                    args.metadata_path = Some(input.parse()?);
                },
                "id" => {
                    let _: Token![=] = input.parse()?;
                    args.id = Some(input.parse()?);
                },
                "display_name" => {
                    let _: Token![=] = input.parse()?;
                    args.display_name = Some(input.parse()?);
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
                "resources" => {
                    let content;
                    syn::parenthesized!(content in input);
                    parse_resources(&content, &mut args)?;
                },
                "events" => {
                    let content;
                    syn::parenthesized!(content in input);
                    parse_events(&content, &mut args)?;
                },
                _ => {
                    return Err(syn::Error::new(
                        key.span(),
                        "supported provider arguments are `id`/`display_name`/`mount`, `capabilities(...)`, `resources(...)`, `events(...)`, and (transitional) `metadata = \"...\"`",
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

/// Parse `capabilities(domain("v", "why"), memory_mb(32, "why"), ...)` into the
/// manifest's declared `Need`s. String-valued kinds take `("value", "why")`;
/// scalar kinds take `(<int>, "why")`. All are static (non-dynamic) here; a
/// dynamic value (e.g. a docker socket resolved at init) is declared by the
/// provider through config, not the manifest annotation.
fn parse_capabilities(content: ParseStream<'_>) -> syn::Result<Vec<omnifs_caps::Need>> {
    let mut needs = Vec::new();
    while !content.is_empty() {
        let kind: syn::Ident = content.parse()?;
        let inner;
        syn::parenthesized!(inner in content);
        let need = match kind.to_string().as_str() {
            "domain" | "git_repo" | "unix_socket" => {
                let value: LitStr = inner.parse()?;
                let _: Token![,] = inner.parse()?;
                let why: LitStr = inner.parse()?;
                let (value, why) = (value.value(), why.value());
                match kind.to_string().as_str() {
                    "domain" => omnifs_caps::Need::Domain {
                        value,
                        why,
                        dynamic: false,
                    },
                    "git_repo" => omnifs_caps::Need::GitRepo {
                        value,
                        why,
                        dynamic: false,
                    },
                    _ => omnifs_caps::Need::UnixSocket {
                        value,
                        why,
                        dynamic: false,
                    },
                }
            },
            "memory_mb" => {
                let amount: LitInt = inner.parse()?;
                let _: Token![,] = inner.parse()?;
                let why: LitStr = inner.parse()?;
                omnifs_caps::Need::MemoryMb {
                    value: amount.base10_parse::<u32>()?,
                    why: why.value(),
                    dynamic: false,
                }
            },
            other => {
                return Err(syn::Error::new(
                    kind.span(),
                    format!(
                        "unsupported capability `{other}`; expected `domain`, `git_repo`, `unix_socket`, or `memory_mb`"
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

fn parse_resources(content: ParseStream<'_>, args: &mut ProviderArgs) -> syn::Result<()> {
    while !content.is_empty() {
        let key: syn::Ident = content.parse()?;
        match key.to_string().as_str() {
            "git" => {
                let _: Token![=] = content.parse()?;
                let value: LitBool = content.parse()?;
                args.git = value.value;
            },
            _ => {
                return Err(syn::Error::new(
                    key.span(),
                    "supported resources are `git = <bool>`",
                ));
            },
        }
        if content.peek(Token![,]) {
            let _: Token![,] = content.parse()?;
        }
    }
    Ok(())
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

#[derive(Clone, Copy)]
enum StartKind {
    ConfigAndRouter,
    RouterOnly,
}

fn classify_impl(items: Vec<ImplItem>) -> syn::Result<ClassifiedImpl> {
    let mut config_type = None;
    let mut state_type = None;
    let mut start_kind = None;
    let mut methods = Vec::new();

    for item in items {
        match item {
            ImplItem::Type(ty) => match ty.ident.to_string().as_str() {
                "Config" => config_type = Some(ty.ty),
                "State" => state_type = Some(ty.ty),
                _ => {
                    return Err(syn::Error::new(
                        ty.ident.span(),
                        "only `type Config` and `type State` are recognized",
                    ));
                },
            },
            ImplItem::Fn(func) => {
                if func.sig.ident == "start" {
                    start_kind = Some(classify_start(&func)?);
                }
                methods.push(func);
            },
            other => {
                return Err(syn::Error::new(
                    other.span(),
                    "unsupported item in #[provider] impl; expected `type` aliases and methods",
                ));
            },
        }
    }

    let Some(start_kind) = start_kind else {
        return Err(syn::Error::new(
            proc_macro2::Span::call_site(),
            "missing required `fn start(..)`",
        ));
    };

    Ok(ClassifiedImpl {
        config_type: config_type.unwrap_or_else(|| parse_quote!(omnifs_sdk::NoConfig)),
        state_type: state_type.unwrap_or_else(|| parse_quote!(())),
        start_kind,
        methods,
    })
}

fn classify_start(func: &ImplItemFn) -> syn::Result<StartKind> {
    let inputs = func
        .sig
        .inputs
        .iter()
        .filter(|arg| matches!(arg, FnArg::Typed(_)))
        .count();
    match inputs {
        1 => Ok(StartKind::RouterOnly),
        2 => Ok(StartKind::ConfigAndRouter),
        _ => Err(syn::Error::new(
            func.sig.span(),
            "`start` must be `fn start(r: &mut Router<..>) -> Result<..>` or `fn start(config, r: &mut Router<..>) -> Result<..>`",
        )),
    }
}

/// The compile-time manifest facts: the embedded custom section plus the
/// provider name/description used to derive [`ProviderInfo`].
/// The facts the macro needs to emit a provider's `manifest_json()` export. The
/// manifest is a JSON string built at compile time except for `config_schema`
/// (and, later, `auth`), which the proc-macro cannot evaluate; `splice_config`
/// asks the generated export to splice the runtime `ProvidesConfigSchema` value
/// into `configSchema` before returning. `cargo_tracking` is an
/// `include_bytes!` of the source `omnifs.provider.json` so an edit invalidates
/// the build (annotation-authored providers have no such file).
struct ManifestFacts {
    base_json: String,
    splice_config: bool,
    cargo_tracking: TokenStream2,
    name: Option<String>,
    description: Option<String>,
}

fn read_manifest_facts(
    type_name: &syn::Ident,
    metadata_path: &LitStr,
) -> syn::Result<ManifestFacts> {
    let _ = type_name;
    let manifest_dir = std::env::var("CARGO_MANIFEST_DIR").map_err(|error| {
        syn::Error::new(
            metadata_path.span(),
            format!("CARGO_MANIFEST_DIR is not set: {error}"),
        )
    })?;
    let path = std::path::PathBuf::from(manifest_dir).join(metadata_path.value());
    let bytes = std::fs::read(&path).map_err(|error| {
        syn::Error::new(
            metadata_path.span(),
            format!(
                "failed to read provider metadata {}: {error}",
                path.display()
            ),
        )
    })?;
    let mut manifest = omnifs_provider::ProviderManifest::from_bytes(&bytes).map_err(|error| {
        syn::Error::new(
            metadata_path.span(),
            format!("invalid provider manifest {}: {error}", path.display()),
        )
    })?;
    // `env!` here captures the *sdk-macros* crate version at the time this macro
    // was compiled, which is correctly `BuildEvidence.sdk`.
    manifest.build_evidence = Some(omnifs_provider::BuildEvidence::current(env!(
        "CARGO_PKG_VERSION"
    )));
    // `std::env::var` reads the cargo-set env at *expansion* time, i.e. the
    // version of the provider crate currently being compiled. Do not switch this
    // to `env!`: that would capture sdk-macros' version instead.
    manifest.version = std::env::var("CARGO_PKG_VERSION").ok();
    let name = manifest.id.clone();
    let description = manifest.display_name.clone();
    let base_json = serde_json::to_string(&manifest).map_err(|error| {
        syn::Error::new(
            metadata_path.span(),
            format!("failed to encode provider manifest: {error}"),
        )
    })?;

    // Force Cargo to track the manifest as a build input; without it an edit to
    // `omnifs.provider.json` alone leaves a stale `manifest_json()` in
    // incremental and Docker-layer-cached builds. `include_bytes!` is a tracked
    // compile-time dep at zero runtime cost (the const is dropped by the linker).
    let path_lit = syn::LitStr::new(&path.display().to_string(), metadata_path.span());
    Ok(ManifestFacts {
        base_json,
        // The hand-authored JSON already carries `configSchema`/`auth`.
        splice_config: false,
        cargo_tracking: quote! { const _: &[u8] = include_bytes!(#path_lit); },
        name: Some(name),
        description: Some(description),
    })
}

/// Build the embedded manifest from `#[provider(..)]` annotations rather than a
/// hand-authored `omnifs.provider.json`. The full manifest is known at macro
/// expansion for a provider with no config and no auth (`config_schema` and
/// `auth` are `None`); config/auth providers will fill those at build time.
fn build_manifest_facts_from_args(
    type_name: &syn::Ident,
    args: &ProviderArgs,
) -> syn::Result<ManifestFacts> {
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

    let manifest = omnifs_provider::ProviderManifest {
        id: id.clone(),
        display_name: display_name.clone(),
        provider: format!("{}.wasm", pkg_name.replace('-', "_")),
        default_mount,
        version: std::env::var("CARGO_PKG_VERSION").ok(),
        build_evidence: Some(omnifs_provider::BuildEvidence::current(env!(
            "CARGO_PKG_VERSION"
        ))),
        capabilities: args.capabilities.clone(),
        auth: None,
        config_schema: None,
    };
    let _ = type_name;
    let base_json = serde_json::to_string(&manifest).map_err(|error| {
        syn::Error::new(
            Span::call_site(),
            format!("failed to encode provider manifest: {error}"),
        )
    })?;
    Ok(ManifestFacts {
        base_json,
        // `config_schema` (and later `auth`) are runtime values the macro cannot
        // evaluate; the generated `manifest_json()` splices them in.
        splice_config: true,
        cargo_tracking: TokenStream2::new(),
        name: Some(id),
        description: Some(display_name),
    })
}

fn provider_info_tokens(type_name: &syn::Ident, manifest: &ManifestFacts) -> TokenStream2 {
    let name = if let Some(name) = &manifest.name {
        quote! { #name.to_string() }
    } else {
        quote! { stringify!(#type_name).to_string() }
    };
    let description = if let Some(desc) = &manifest.description {
        quote! { #desc.to_string() }
    } else {
        quote! { String::new() }
    };
    quote! {
        omnifs_sdk::prelude::ProviderInfo {
            name: #name,
            version: env!("CARGO_PKG_VERSION").to_string(),
            description: #description,
        }
    }
}

fn requested_capabilities_tokens(args: &ProviderArgs) -> TokenStream2 {
    let git = args.git;
    let refresh = if let Some(timer) = &args.timer {
        let interval = &timer.interval;
        quote! { (#interval).as_secs() as u32 }
    } else {
        quote! { 0u32 }
    };
    // The sandbox memory cap comes from the mount's granted capabilities
    // (seeded from the manifest's `memoryMb` need), not this field, so it stays
    // at the `empty()` default.
    quote! {
        omnifs_sdk::prelude::RequestedCapabilities {
            needs_git: #git,
            refresh_interval_secs: #refresh,
            ..omnifs_sdk::prelude::RequestedCapabilities::empty()
        }
    }
}

fn generate_lifecycle(
    type_name: &syn::Ident,
    config_type: &Type,
    state_type: &Type,
    start_kind: StartKind,
    info_tokens: &TokenStream2,
    caps_tokens: &TokenStream2,
    base_json: &str,
    splice_config: bool,
) -> TokenStream2 {
    // The config-less manifest export the build tool calls to harvest the full
    // manifest (incl. `config_schema`) and inject the custom section.
    let manifest_json_fn = if splice_config {
        quote! {
            fn manifest_json() -> String {
                const BASE: &str = #base_json;
                let mut value: omnifs_sdk::serde_json::Value =
                    omnifs_sdk::serde_json::from_str(BASE)
                        .expect("provider manifest base is valid JSON");
                if let Some(schema) =
                    <#config_type as omnifs_sdk::ProvidesConfigSchema>::config_schema()
                {
                    if let Some(object) = value.as_object_mut() {
                        object.insert("configSchema".to_string(), schema);
                    }
                }
                omnifs_sdk::serde_json::to_string(&value)
                    .expect("provider manifest serializes")
            }
        }
    } else {
        quote! {
            fn manifest_json() -> String {
                #base_json.to_string()
            }
        }
    };
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
                        return omnifs_sdk::prelude::err(
                            omnifs_sdk::error::ProviderError::invalid_input(format!("config error: {error}"))
                        );
                    },
                };
                let mut router = omnifs_sdk::prelude::Router::<#state_type>::new();
                let state = match #start_call {
                    Ok(state) => state,
                    Err(error) => return omnifs_sdk::prelude::err(error),
                };
                if let Err(error) = router.seal() {
                    return omnifs_sdk::prelude::err(error);
                }
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
                ASYNC_RUNTIME.with(|runtime| runtime.clear());
                RANGE_HANDLES.with(|handles| handles.clear());
                omnifs_sdk::__internal::clear_breaker();
            }

            #manifest_json_fn
        }
    }
}

fn generate_state_management(state_type: &Type) -> TokenStream2 {
    quote! {
        thread_local! {
            static STATE: core::cell::RefCell<Option<std::rc::Rc<core::cell::RefCell<#state_type>>>>
                = const { core::cell::RefCell::new(None) };
            static ASYNC_RUNTIME: omnifs_sdk::__internal::AsyncRuntime<#state_type> =
                omnifs_sdk::__internal::AsyncRuntime::new();
            static ROUTER: core::cell::RefCell<
                Option<std::rc::Rc<omnifs_sdk::prelude::Router<#state_type>>>
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
            std::rc::Rc<omnifs_sdk::prelude::Router<#state_type>>,
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
            fn lookup_child(id: u64, parent_path: String, name: String) -> omnifs_sdk::prelude::ProviderStep {
                let (Ok(state), Ok(router)) = (state_handle(), router_handle()) else {
                    return omnifs_sdk::prelude::err_step(
                        omnifs_sdk::error::ProviderError::internal("provider not initialized")
                    );
                };
                let cx = omnifs_sdk::__internal::Cx::<#state_type>::new(id, state);
                let future_cx = cx.clone();
                let future: ::std::pin::Pin<Box<dyn ::core::future::Future<Output = omnifs_sdk::prelude::ProviderReturn>>> =
                    Box::pin(async move {
                        match router.lookup_child(&future_cx, &parent_path, &name).await {
                            Ok(outcome) => {
                                let (result, effects) = outcome.into_result_and_effects();
                                omnifs_sdk::prelude::ProviderReturn::with_effects(
                                    omnifs_sdk::prelude::OpResult::LookupChild(result),
                                    effects.into_wit(),
                                )
                            },
                            Err(error) => omnifs_sdk::prelude::err(error),
                        }
                    });
                ASYNC_RUNTIME.with(|runtime| runtime.start(id, cx, future))
            }

            fn list_children(
                id: u64,
                path: String,
                cached_validator: Option<String>,
                cursor: Option<omnifs_sdk::omnifs::provider::types::Cursor>,
            ) -> omnifs_sdk::prelude::ProviderStep {
                let (Ok(state), Ok(router)) = (state_handle(), router_handle()) else {
                    return omnifs_sdk::prelude::err_step(
                        omnifs_sdk::error::ProviderError::internal("provider not initialized")
                    );
                };
                let version = cached_validator
                    .as_ref()
                    .map(|v| omnifs_sdk::file_attrs::VersionToken::from(v.as_str()));
                let sdk_cursor = cursor.map(omnifs_sdk::prelude::Cursor::from);
                let cx = omnifs_sdk::__internal::Cx::<#state_type>::new(id, state).with_version(version);
                let future_cx = cx.clone();
                let future: ::std::pin::Pin<Box<dyn ::core::future::Future<Output = omnifs_sdk::prelude::ProviderReturn>>> =
                    Box::pin(async move {
                        match router.list_children(&future_cx, &path, cached_validator, sdk_cursor).await {
                            Ok(outcome) => {
                                let (result, effects) = outcome.into_result_and_effects();
                                omnifs_sdk::prelude::ProviderReturn::with_effects(
                                    omnifs_sdk::prelude::OpResult::ListChildren(result),
                                    effects.into_wit(),
                                )
                            },
                            Err(error) => omnifs_sdk::prelude::err(error),
                        }
                    });
                ASYNC_RUNTIME.with(|runtime| runtime.start(id, cx, future))
            }

            fn read_file(
                id: u64,
                path: String,
                content_type: String,
                cached_canonical: Option<omnifs_sdk::omnifs::provider::types::CanonicalInput>,
            ) -> omnifs_sdk::prelude::ProviderStep {
                let (Ok(state), Ok(router)) = (state_handle(), router_handle()) else {
                    return omnifs_sdk::prelude::err_step(
                        omnifs_sdk::error::ProviderError::internal("provider not initialized")
                    );
                };
                let cached = cached_canonical
                    .map(omnifs_sdk::browse::CachedCanonical::from_wit);
                let version = cached.as_ref().and_then(|c| c.validator.clone());
                let cx = omnifs_sdk::__internal::Cx::<#state_type>::new(id, state).with_version(version);
                let future_cx = cx.clone();
                let future: ::std::pin::Pin<Box<dyn ::core::future::Future<Output = omnifs_sdk::prelude::ProviderReturn>>> =
                    Box::pin(async move {
                        match router.read_file(&future_cx, &path, &content_type, cached).await {
                            Ok(outcome) => {
                                let (result, effects) = outcome.into_result_and_effects();
                                omnifs_sdk::prelude::ProviderReturn::with_effects(
                                    omnifs_sdk::prelude::OpResult::ReadFile(result),
                                    effects.into_wit(),
                                )
                            },
                            Err(error) => omnifs_sdk::prelude::err(error),
                        }
                    });
                ASYNC_RUNTIME.with(|runtime| runtime.start(id, cx, future))
            }

            fn open_file(id: u64, path: String) -> omnifs_sdk::prelude::ProviderStep {
                let (Ok(state), Ok(router)) = (state_handle(), router_handle()) else {
                    return omnifs_sdk::prelude::err_step(
                        omnifs_sdk::error::ProviderError::internal("provider not initialized")
                    );
                };
                let cx = omnifs_sdk::__internal::Cx::<#state_type>::new(id, state);
                let future_cx = cx.clone();
                let future: ::std::pin::Pin<Box<dyn ::core::future::Future<Output = omnifs_sdk::prelude::ProviderReturn>>> =
                    Box::pin(async move {
                        match router.open_file(&future_cx, &path).await {
                            Ok(opened) => {
                                let Some(handle) = RANGE_HANDLES.with(|handles| {
                                    handles.allocate(opened.reader)
                                }) else {
                                    return omnifs_sdk::prelude::err(
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
                            Err(error) => omnifs_sdk::prelude::err(error),
                        }
                    });
                ASYNC_RUNTIME.with(|runtime| runtime.start(id, cx, future))
            }

            fn read_chunk(id: u64, handle: u64, offset: u64, len: u32) -> omnifs_sdk::prelude::ProviderStep {
                let Ok(state) = state_handle() else {
                    return omnifs_sdk::prelude::err_step(
                        omnifs_sdk::error::ProviderError::internal("provider not initialized")
                    );
                };
                let Some(handle_id) = ::std::num::NonZeroU64::new(handle) else {
                    return omnifs_sdk::prelude::err_step(
                        omnifs_sdk::error::ProviderError::not_found(
                            format!("unknown file handle {handle}")
                        )
                    );
                };
                let Some(reader) = RANGE_HANDLES.with(|handles| handles.get(handle_id)) else {
                    return omnifs_sdk::prelude::err_step(
                        omnifs_sdk::error::ProviderError::not_found(
                            format!("unknown file handle {handle}")
                        )
                    );
                };
                let cx = omnifs_sdk::__internal::Cx::<#state_type>::new(id, state);
                let read_cx = cx.erase_state();
                let future: ::std::pin::Pin<Box<dyn ::core::future::Future<Output = omnifs_sdk::prelude::ProviderReturn>>> =
                    Box::pin(async move {
                        match reader.read_chunk(&read_cx, offset, len).await {
                            Ok(chunk) => omnifs_sdk::prelude::ProviderReturn::terminal(
                                omnifs_sdk::prelude::OpResult::ReadChunk(chunk.into())
                            ),
                            Err(error) => omnifs_sdk::prelude::err(error),
                        }
                    });
                ASYNC_RUNTIME.with(|runtime| runtime.start(id, cx, future))
            }

            fn close_file(handle: u64) {
                if let Some(handle_id) = ::std::num::NonZeroU64::new(handle) {
                    RANGE_HANDLES.with(|handles| handles.remove(handle_id));
                }
            }
        }
    }
}

fn generate_continuation(type_name: &syn::Ident) -> TokenStream2 {
    quote! {
        impl omnifs_sdk::exports::omnifs::provider::continuation::Guest for #type_name {
            fn resume(
                id: u64,
                outcome: omnifs_sdk::prelude::CalloutResults,
            ) -> omnifs_sdk::prelude::ProviderStep {
                if let Some(response) = ASYNC_RUNTIME.with(|runtime| runtime.resume(id, outcome.clone())) {
                    return response;
                }
                omnifs_sdk::prelude::err_step(
                    omnifs_sdk::error::ProviderError::internal(format!("no pending future for id {id}"))
                )
            }

            fn cancel(id: u64) {
                ASYNC_RUNTIME.with(|runtime| runtime.cancel(id));
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
                        Err(error) => omnifs_sdk::prelude::err(error),
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
            fn on_event(
                id: u64,
                event: omnifs_sdk::prelude::ProviderEvent,
            ) -> omnifs_sdk::prelude::ProviderStep {
                let Ok(state) = state_handle() else {
                    return omnifs_sdk::prelude::err_step(
                        omnifs_sdk::error::ProviderError::internal("provider not initialized")
                    );
                };
                let cx = omnifs_sdk::__internal::Cx::<#state_type>::new(id, state);
                let future_cx = cx.clone();
                let future: ::std::pin::Pin<Box<dyn ::core::future::Future<Output = omnifs_sdk::prelude::ProviderReturn>>> =
                    Box::pin(async move { #dispatch });
                ASYNC_RUNTIME.with(|runtime| runtime.start(id, cx, future))
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

    let classified = classify_impl(input.items)?;
    let config_type = &classified.config_type;
    let state_type = &classified.state_type;
    let start_kind = classified.start_kind;
    let methods = &classified.methods;

    // Two manifest sources: annotation-authored (`id = ".."` + `capabilities(..)`)
    // or the transitional hand-authored `metadata = "omnifs.provider.json"`.
    let manifest = if let Some(metadata_path) = args.metadata_path.as_ref() {
        read_manifest_facts(&type_name, metadata_path)?
    } else if args.id.is_some() {
        build_manifest_facts_from_args(&type_name, args)?
    } else {
        return Err(syn::Error::new(
            Span::call_site(),
            "provider needs an `id = \"...\"` annotation (or the transitional `metadata = \"...\"`)",
        ));
    };
    let info_tokens = provider_info_tokens(&type_name, &manifest);
    let caps_tokens = requested_capabilities_tokens(args);

    let state_management = generate_state_management(state_type);
    let lifecycle = generate_lifecycle(
        &type_name,
        config_type,
        state_type,
        start_kind,
        &info_tokens,
        &caps_tokens,
        &manifest.base_json,
        manifest.splice_config,
    );
    let namespace = generate_namespace(&type_name, state_type);
    let continuation = generate_continuation(&type_name);
    let notify = generate_notify(&type_name, state_type, args.timer.as_ref());
    let cargo_tracking = manifest.cargo_tracking;

    Ok(quote! {
        struct #type_name;

        #state_management
        #cargo_tracking

        impl #type_name {
            #(#methods)*
        }

        #lifecycle
        #namespace
        #continuation
        #notify

        #[cfg(target_arch = "wasm32")]
        omnifs_sdk::export!(#type_name with_types_in omnifs_sdk);
    })
}
