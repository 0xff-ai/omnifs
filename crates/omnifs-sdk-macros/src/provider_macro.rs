//! The `#[provider]` attribute macro.
//!
//! It backs `#[omnifs_sdk::provider(resources(..), events(..), metadata = "..")]`
//! on a provider impl block whose optional associated `type Config/State`
//! aliases and a synchronous `start(..)` method define the provider.

use proc_macro2::TokenStream as TokenStream2;
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
    git: bool,
    memory_mb: Option<u32>,
    endpoints: Vec<Path>,
    timer: Option<TimerSpec>,
    name: Option<LitStr>,
    version: Option<LitStr>,
    description: Option<LitStr>,
}

impl Parse for ProviderArgs {
    fn parse(input: ParseStream<'_>) -> syn::Result<Self> {
        let mut args = Self {
            metadata_path: None,
            git: false,
            memory_mb: None,
            endpoints: Vec::new(),
            timer: None,
            name: None,
            version: None,
            description: None,
        };

        while !input.is_empty() {
            let key: syn::Ident = input.parse()?;
            match key.to_string().as_str() {
                "metadata" => {
                    let _: Token![=] = input.parse()?;
                    args.metadata_path = Some(input.parse()?);
                },
                "name" => {
                    let _: Token![=] = input.parse()?;
                    args.name = Some(input.parse()?);
                },
                "version" => {
                    let _: Token![=] = input.parse()?;
                    args.version = Some(input.parse()?);
                },
                "description" => {
                    let _: Token![=] = input.parse()?;
                    args.description = Some(input.parse()?);
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
                        "supported provider arguments are `metadata = \"...\"`, `resources(...)`, `events(...)`, and `name`/`version`/`description` overrides",
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

fn parse_resources(content: ParseStream<'_>, args: &mut ProviderArgs) -> syn::Result<()> {
    while !content.is_empty() {
        let key: syn::Ident = content.parse()?;
        match key.to_string().as_str() {
            "git" => {
                let _: Token![=] = content.parse()?;
                let value: LitBool = content.parse()?;
                args.git = value.value;
            },
            "memory_mb" => {
                let _: Token![=] = content.parse()?;
                let value: LitInt = content.parse()?;
                args.memory_mb = Some(value.base10_parse::<u32>()?);
            },
            "endpoints" => {
                let _: Token![=] = content.parse()?;
                let list;
                syn::bracketed!(list in content);
                while !list.is_empty() {
                    args.endpoints.push(list.parse()?);
                    if list.peek(Token![,]) {
                        let _: Token![,] = list.parse()?;
                    }
                }
            },
            _ => {
                return Err(syn::Error::new(
                    key.span(),
                    "supported resources are `git = <bool>`, `memory_mb = <int>`, and `endpoints = [..]`",
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
struct ManifestFacts {
    metadata_section: TokenStream2,
    name: Option<String>,
    description: Option<String>,
}

fn read_manifest_facts(
    type_name: &syn::Ident,
    metadata_path: Option<&LitStr>,
) -> syn::Result<ManifestFacts> {
    let Some(metadata_path) = metadata_path else {
        return Ok(ManifestFacts {
            metadata_section: TokenStream2::new(),
            name: None,
            description: None,
        });
    };
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
    manifest.contract = Some(omnifs_provider::ContractEvidence::current(env!(
        "CARGO_PKG_VERSION"
    )));
    let name = manifest.id.clone();
    let description = manifest.display_name.clone();
    let metadata_bytes = serde_json::to_vec(&manifest).map_err(|error| {
        syn::Error::new(
            metadata_path.span(),
            format!("failed to encode provider metadata custom section: {error}"),
        )
    })?;
    let metadata_len = metadata_bytes.len();
    let metadata_ident = format_ident!(
        "__OMNIFS_PROVIDER_METADATA_{}",
        type_name.to_string().to_uppercase()
    );

    // Force Cargo to track the manifest as a build input; without it an edit to
    // `omnifs.provider.json` alone leaves a stale custom section in incremental
    // and Docker-layer-cached builds. `include_bytes!` is a tracked compile-time
    // dep at zero runtime cost (the const is dropped by the linker).
    let path_lit = syn::LitStr::new(&path.display().to_string(), metadata_path.span());
    let metadata_section = quote! {
        const _: &[u8] = include_bytes!(#path_lit);

        #[cfg(all(target_arch = "wasm32", not(test)))]
        #[unsafe(link_section = "omnifs.provider-metadata.v1")]
        #[used]
        static #metadata_ident: [u8; #metadata_len] = [ #(#metadata_bytes),* ];

        #[cfg(test)]
        #[allow(non_upper_case_globals)]
        pub(crate) const #metadata_ident: [u8; #metadata_len] = [ #(#metadata_bytes),* ];
    };
    Ok(ManifestFacts {
        metadata_section,
        name: Some(name),
        description: Some(description),
    })
}

fn provider_info_tokens(
    type_name: &syn::Ident,
    args: &ProviderArgs,
    manifest: &ManifestFacts,
) -> TokenStream2 {
    let name = match (&args.name, &manifest.name) {
        (Some(lit), _) => quote! { #lit.to_string() },
        (None, Some(name)) => quote! { #name.to_string() },
        (None, None) => quote! { stringify!(#type_name).to_string() },
    };
    let version = if let Some(lit) = &args.version {
        quote! { #lit.to_string() }
    } else {
        quote! { env!("CARGO_PKG_VERSION").to_string() }
    };
    let description = match (&args.description, &manifest.description) {
        (Some(lit), _) => quote! { #lit.to_string() },
        (None, Some(desc)) => quote! { #desc.to_string() },
        (None, None) => quote! { String::new() },
    };
    quote! {
        omnifs_sdk::prelude::ProviderInfo {
            name: #name,
            version: #version,
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
    let memory = if let Some(mb) = args.memory_mb {
        quote! { #mb }
    } else {
        quote! { 0u32 }
    };
    quote! {
        omnifs_sdk::prelude::RequestedCapabilities {
            needs_git: #git,
            refresh_interval_secs: #refresh,
            max_memory_mb: #memory,
            ..omnifs_sdk::prelude::RequestedCapabilities::empty()
        }
    }
}

fn endpoint_assert_tokens(endpoints: &[Path]) -> TokenStream2 {
    if endpoints.is_empty() {
        return TokenStream2::new();
    }
    let asserts = endpoints
        .iter()
        .map(|path| quote! { assert_endpoint::<#path>(); });
    quote! {
        // `endpoints = [..]` is a declared-intent list; keep it honest by
        // statically asserting each names a real `Endpoint`, without any
        // runtime registration (`cx.endpoint::<E>()` needs none).
        const _: fn() = || {
            fn assert_endpoint<E: omnifs_sdk::endpoint::Endpoint>() {}
            #(#asserts)*
        };
    }
}

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
                let future: ::std::pin::Pin<Box<dyn ::core::future::Future<Output = omnifs_sdk::prelude::ProviderReturn>>> =
                    Box::pin(async move {
                        match reader.read_chunk(offset, len).await {
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
                        Ok(effects) => omnifs_sdk::prelude::ProviderReturn::with_effects(
                            omnifs_sdk::prelude::OpResult::OnEvent,
                            effects.into_wit(),
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

    let manifest = read_manifest_facts(&type_name, args.metadata_path.as_ref())?;
    let info_tokens = provider_info_tokens(&type_name, args, &manifest);
    let caps_tokens = requested_capabilities_tokens(args);
    let endpoint_assert = endpoint_assert_tokens(&args.endpoints);

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
    let continuation = generate_continuation(&type_name);
    let notify = generate_notify(&type_name, state_type, args.timer.as_ref());
    let metadata_section = manifest.metadata_section;

    Ok(quote! {
        struct #type_name;

        #state_management
        #metadata_section
        #endpoint_assert

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
