use proc_macro2::TokenStream as TokenStream2;
use quote::{format_ident, quote};
use syn::parse::{Parse, ParseStream};
use syn::spanned::Spanned;
use syn::{ImplItem, ImplItemFn, ItemImpl, LitStr, Token, Type};

struct ClassifiedMethods {
    init: InitReturn,
    on_event: Option<ImplItemFn>,
    resume_notify: Option<ImplItemFn>,
    cancel_notify: Option<ImplItemFn>,
    helpers: Vec<ImplItemFn>,
}

enum InitReturn {
    Infallible {
        func: ImplItemFn,
        config: Type,
        state: Type,
    },
    Fallible {
        func: ImplItemFn,
        config: Type,
        state: Type,
    },
}

impl InitReturn {
    fn from_init_func(func: ImplItemFn) -> syn::Result<Self> {
        let config = func
            .sig
            .inputs
            .first()
            .and_then(|arg| match arg {
                syn::FnArg::Typed(pat_type) => Some((*pat_type.ty).clone()),
                syn::FnArg::Receiver(_) => None,
            })
            .ok_or_else(|| syn::Error::new(func.sig.span(), "init must have a config parameter"))?;

        let syn::ReturnType::Type(_, ty) = func.sig.output.clone() else {
            return Err(syn::Error::new(
                func.sig.span(),
                "init must return (State, ProviderInfo, RequestedCapabilities) or Result<(State, ProviderInfo, RequestedCapabilities)>",
            ));
        };

        if let Type::Tuple(tuple) = &*ty
            && tuple.elems.len() == 3
        {
            return Ok(Self::Infallible {
                func,
                config,
                state: tuple.elems[0].clone(),
            });
        }

        if let Type::Path(path) = &*ty
            && let Some(segment) = path.path.segments.last()
            && segment.ident == "Result"
            && let syn::PathArguments::AngleBracketed(args) = &segment.arguments
            && let Some(syn::GenericArgument::Type(Type::Tuple(tuple))) = args.args.first()
            && tuple.elems.len() == 3
        {
            return Ok(Self::Fallible {
                func,
                config,
                state: tuple.elems[0].clone(),
            });
        }

        Err(syn::Error::new(
            ty.span(),
            "init must return (State, ProviderInfo, RequestedCapabilities) or Result<(State, ProviderInfo, RequestedCapabilities)>",
        ))
    }

    fn func(&self) -> &ImplItemFn {
        match self {
            Self::Infallible { func, .. } | Self::Fallible { func, .. } => func,
        }
    }

    fn config(&self) -> &Type {
        match self {
            Self::Infallible { config, .. } | Self::Fallible { config, .. } => config,
        }
    }

    fn state(&self) -> &Type {
        match self {
            Self::Infallible { state, .. } | Self::Fallible { state, .. } => state,
        }
    }
}

pub struct ProviderArgs {
    metadata_path: Option<LitStr>,
    mount_modules: Vec<syn::Path>,
}

impl Parse for ProviderArgs {
    fn parse(input: ParseStream<'_>) -> syn::Result<Self> {
        if input.is_empty() {
            return Ok(Self {
                metadata_path: None,
                mount_modules: Vec::new(),
            });
        }

        let mut metadata_path = None;
        let mut mount_modules = Vec::new();

        while !input.is_empty() {
            let key: syn::Ident = input.parse()?;
            match key.to_string().as_str() {
                "metadata" => {
                    if metadata_path.is_some() {
                        return Err(syn::Error::new(key.span(), "duplicate `metadata` argument"));
                    }
                    let _: Token![=] = input.parse()?;
                    metadata_path = Some(input.parse()?);
                },
                "mounts" => {
                    let content;
                    syn::parenthesized!(content in input);
                    while !content.is_empty() {
                        mount_modules.push(content.parse()?);
                        if content.peek(Token![,]) {
                            let _: Token![,] = content.parse()?;
                        }
                    }
                },
                _ => {
                    return Err(syn::Error::new(
                        key.span(),
                        "supported provider arguments are `metadata = \"...\"` and `mounts(...)`",
                    ));
                },
            }
            if input.peek(Token![,]) {
                let _: Token![,] = input.parse()?;
            }
        }

        Ok(Self {
            metadata_path,
            mount_modules,
        })
    }
}

fn reject_removed_route_surface(items: &[ImplItem]) -> syn::Result<()> {
    for item in items {
        match item {
            ImplItem::Macro(mac) if mac.mac.path.is_ident("routes") => {
                return Err(syn::Error::new(
                    mac.mac.span(),
                    "route macros are removed; use free-function #[dir]/#[file]/#[subtree] handlers and #[omnifs_sdk::provider(mounts(...))]",
                ));
            },
            ImplItem::Fn(func)
                if func.attrs.iter().any(|attr| {
                    attr.path().is_ident("lookup")
                        || attr.path().is_ident("list")
                        || attr.path().is_ident("read")
                }) =>
            {
                return Err(syn::Error::new(
                    func.sig.span(),
                    "#[lookup]/#[list]/#[read] handlers are removed; use free-function #[dir]/#[file]/#[subtree] handlers",
                ));
            },
            _ => {},
        }
    }
    Ok(())
}

fn is_mount_module_macro(path: &syn::Path) -> bool {
    path.segments
        .last()
        .is_some_and(|segment| segment.ident == "mount_module")
}

fn classify_methods(items: Vec<ImplItem>) -> syn::Result<ClassifiedMethods> {
    let mut init = None;
    let mut on_event = None;
    let mut resume_notify = None;
    let mut cancel_notify = None;
    let mut helpers = Vec::new();

    for item in items {
        match item {
            ImplItem::Fn(func) => match func.sig.ident.to_string().as_str() {
                "init" => {
                    init = Some(InitReturn::from_init_func(func)?);
                },
                "capabilities" => {
                    return Err(syn::Error::new(
                        func.sig.span(),
                        "`capabilities` is returned from `init`; include RequestedCapabilities in the init return tuple",
                    ));
                },
                "auth_manifest" => {
                    return Err(syn::Error::new(
                        func.sig.span(),
                        "auth_manifest is removed; declare auth in omnifs.provider.json and use #[provider(metadata = \"...\")]",
                    ));
                },
                "register_scopes" => {
                    return Err(syn::Error::new(
                        func.sig.span(),
                        "register_scopes is removed in the path-first provider SDK",
                    ));
                },
                "on_event" => on_event = Some(func),
                "resume_notify" => resume_notify = Some(func),
                "cancel_notify" => cancel_notify = Some(func),
                _ => helpers.push(func),
            },
            ImplItem::Macro(mac) if is_mount_module_macro(&mac.mac.path) => {
                return Err(syn::Error::new(
                    mac.mac.span(),
                    "mount_module! is removed; declare handler modules in #[omnifs_sdk::provider(mounts(...))]",
                ));
            },
            ImplItem::Macro(mac) => {
                return Err(syn::Error::new(
                    mac.mac.span(),
                    "unsupported macro inside #[omnifs_sdk::provider] impl",
                ));
            },
            _ => {},
        }
    }

    Ok(ClassifiedMethods {
        init: init.ok_or_else(|| {
            syn::Error::new(
                proc_macro2::Span::call_site(),
                "missing required `init` method",
            )
        })?,
        on_event,
        resume_notify,
        cancel_notify,
        helpers,
    })
}

struct ProviderManifestSections {
    metadata_section: TokenStream2,
}

fn generate_provider_manifest_sections(
    type_name: &syn::Ident,
    metadata_path: Option<&LitStr>,
) -> syn::Result<ProviderManifestSections> {
    let Some(metadata_path) = metadata_path else {
        return Ok(ProviderManifestSections {
            metadata_section: TokenStream2::new(),
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
    let manifest = omnifs_mount_schema::ProviderManifest::from_bytes(&bytes).map_err(|error| {
        syn::Error::new(
            metadata_path.span(),
            format!("invalid provider manifest {}: {error}", path.display()),
        )
    })?;
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

    // Force Cargo to track the manifest file as a build input. Without
    // this, changing `omnifs.provider.json` without touching any `.rs`
    // file leaves Cargo unable to see the input changed; incremental
    // rebuilds (and Docker layer caches) then reuse a wasm with stale
    // metadata embedded in its custom section. `include_bytes!` makes
    // the manifest a tracked compile-time dep at zero runtime cost (the
    // const is dropped by the linker).
    let path_lit = syn::LitStr::new(&path.display().to_string(), metadata_path.span());
    let dep_section = quote! {
        const _: &[u8] = include_bytes!(#path_lit);
    };

    let metadata_section = quote! {
        #dep_section

        #[cfg(all(target_arch = "wasm32", not(test)))]
        #[unsafe(link_section = "omnifs.provider-metadata.v1")]
        #[used]
        static #metadata_ident: [u8; #metadata_len] = [ #(#metadata_bytes),* ];

        #[cfg(test)]
        #[allow(non_upper_case_globals)]
        pub(crate) const #metadata_ident: [u8; #metadata_len] = [ #(#metadata_bytes),* ];
    };
    Ok(ProviderManifestSections { metadata_section })
}

fn generate_state_management(state_type: &Type) -> TokenStream2 {
    quote! {
        thread_local! {
            static STATE: core::cell::RefCell<Option<std::rc::Rc<core::cell::RefCell<#state_type>>>>
                = const { core::cell::RefCell::new(None) };
            static ASYNC_RUNTIME: omnifs_sdk::__internal::AsyncRuntime<#state_type> =
                omnifs_sdk::__internal::AsyncRuntime::new();
            static MOUNT_REGISTRY: std::cell::OnceCell<
                omnifs_sdk::error::Result<std::rc::Rc<omnifs_sdk::__internal::MountRegistry<#state_type>>>
            > = const { std::cell::OnceCell::new() };
        }

        pub(crate) fn state_handle() -> core::result::Result<
            std::rc::Rc<core::cell::RefCell<#state_type>>,
            String,
        > {
            STATE.with(|slot| {
                slot.borrow()
                    .as_ref()
                    .cloned()
                    .ok_or_else(|| "provider not initialized".to_string())
            })
        }

        pub(crate) fn mount_registry() -> omnifs_sdk::error::Result<
            std::rc::Rc<omnifs_sdk::__internal::MountRegistry<#state_type>>,
        > {
            MOUNT_REGISTRY.with(|slot| {
                slot.get_or_init(|| __mount_registry().map(std::rc::Rc::new))
                    .as_ref()
                    .map(std::rc::Rc::clone)
                    .map_err(Clone::clone)
            })
        }
    }
}

fn generate_registry_builder(state_type: &Type, modules: &[syn::Path]) -> TokenStream2 {
    let mount_calls = modules
        .iter()
        .map(|module| quote! { #module::mount(&mut registry); });
    quote! {
        fn __mount_registry() -> omnifs_sdk::error::Result<omnifs_sdk::__internal::MountRegistry<#state_type>> {
            let mut registry = omnifs_sdk::__internal::MountRegistry::new();
            #(#mount_calls)*
            registry.validate()?;
            Ok(registry)
        }
    }
}

fn generate_lifecycle_impl(type_name: &syn::Ident, init: &InitReturn) -> TokenStream2 {
    let config_type = init.config();
    let init_body = match init {
        InitReturn::Fallible { .. } => quote! {
            let (state, info, capabilities) = match #type_name::init(config) {
                Ok(parts) => parts,
                Err(error) => return omnifs_sdk::prelude::err(error),
            };
        },
        InitReturn::Infallible { .. } => quote! {
            let (state, info, capabilities) = #type_name::init(config);
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
                    }
                };
                #init_body
                STATE.with(|slot| {
                    *slot.borrow_mut() = Some(std::rc::Rc::new(core::cell::RefCell::new(state)));
                });
                omnifs_sdk::prelude::ProviderReturn::terminal(
                    omnifs_sdk::prelude::OpResult::Initialize(
                        omnifs_sdk::omnifs::provider::types::InitializeResult { info, capabilities }
                    )
                )
            }

            fn shutdown() {
                STATE.with(|slot| *slot.borrow_mut() = None);
                ASYNC_RUNTIME.with(|runtime| runtime.clear());
            }
        }
    }
}

fn generate_resume_impl(
    type_name: &syn::Ident,
    resume_notify: Option<&ImplItemFn>,
    cancel_notify: Option<&ImplItemFn>,
) -> TokenStream2 {
    let resume_notify_body = resume_notify.map_or_else(TokenStream2::new, |_| {
        quote! {
            if let Some(response) = #type_name::resume_notify(id, outcome) {
                return response;
            }
        }
    });
    let cancel_notify_body = cancel_notify.map_or_else(
        TokenStream2::new,
        |_| quote! { #type_name::cancel_notify(id); },
    );

    quote! {
        impl omnifs_sdk::exports::omnifs::provider::continuation::Guest for #type_name {
            fn resume(
                id: u64,
                outcome: omnifs_sdk::prelude::CalloutResults,
            ) -> omnifs_sdk::prelude::ProviderStep {
                if let Some(response) = ASYNC_RUNTIME.with(|runtime| runtime.resume(id, outcome.clone())) {
                    return response;
                }
                #resume_notify_body
                omnifs_sdk::prelude::err_step(
                    omnifs_sdk::error::ProviderError::internal(format!("no pending future for id {id}"))
                )
            }

            fn cancel(id: u64) {
                ASYNC_RUNTIME.with(|runtime| runtime.cancel(id));
                #cancel_notify_body
            }
        }
    }
}

struct BrowseDispatchOp {
    method_name: syn::Ident,
    extra_params: TokenStream2,
    registry_call: TokenStream2,
    ok_binding: syn::Ident,
    op_result: TokenStream2,
}

fn generate_browse_dispatch_method(state_type: &Type, op: BrowseDispatchOp) -> TokenStream2 {
    let BrowseDispatchOp {
        method_name,
        extra_params,
        registry_call,
        ok_binding,
        op_result,
    } = op;
    quote! {
        fn #method_name(id: u64, #extra_params) -> omnifs_sdk::prelude::ProviderStep {
            let Ok(state) = state_handle() else {
                return omnifs_sdk::prelude::err_step(
                    omnifs_sdk::error::ProviderError::internal("provider not initialized")
                );
            };
            let cx = omnifs_sdk::__internal::Cx::<#state_type>::new(id, state);
            let future_cx = cx.clone();
            let future: ::std::pin::Pin<Box<dyn ::core::future::Future<Output = omnifs_sdk::prelude::ProviderReturn>>> =
                Box::pin(async move {
                    let registry = match mount_registry() {
                        Ok(registry) => registry,
                        Err(error) => return omnifs_sdk::prelude::err(error),
                    };
                    match #registry_call.await {
                        Ok(#ok_binding) => {
                            let (result, effects) = #ok_binding.into_result_and_effects();
                            omnifs_sdk::prelude::ProviderReturn::with_effects(
                                #op_result,
                                effects,
                            )
                        },
                        Err(error) => omnifs_sdk::prelude::err(error),
                    }
                });
            ASYNC_RUNTIME.with(|runtime| runtime.start(id, cx, future))
        }
    }
}

fn generate_browse_impl(type_name: &syn::Ident, state_type: &Type) -> TokenStream2 {
    let lookup_child = generate_browse_dispatch_method(
        state_type,
        BrowseDispatchOp {
            method_name: format_ident!("lookup_child"),
            extra_params: quote! { parent_path: String, name: String },
            registry_call: quote! { registry.lookup_child(&future_cx, &parent_path, &name) },
            ok_binding: format_ident!("lookup"),
            op_result: quote! { omnifs_sdk::prelude::OpResult::LookupChild(result) },
        },
    );
    let list_children = generate_browse_dispatch_method(
        state_type,
        BrowseDispatchOp {
            method_name: format_ident!("list_children"),
            extra_params: quote! { path: String },
            registry_call: quote! { registry.list_children(&future_cx, &path) },
            ok_binding: format_ident!("list"),
            op_result: quote! { omnifs_sdk::prelude::OpResult::ListChildren(result) },
        },
    );
    let read_file = generate_browse_dispatch_method(
        state_type,
        BrowseDispatchOp {
            method_name: format_ident!("read_file"),
            extra_params: quote! { path: String },
            registry_call: quote! { registry.read_file(&future_cx, &path) },
            ok_binding: format_ident!("file"),
            op_result: quote! { omnifs_sdk::prelude::OpResult::ReadFile(result) },
        },
    );

    quote! {
        thread_local! {
            static RANGE_READERS: omnifs_sdk::__internal::RangeReaders =
                omnifs_sdk::__internal::RangeReaders::new();
        }

        impl omnifs_sdk::exports::omnifs::provider::browse::Guest for #type_name {
            #lookup_child

            #list_children

            #read_file

            fn open_file(id: u64, path: String) -> omnifs_sdk::prelude::ProviderStep {
                let Ok(state) = state_handle() else {
                    return omnifs_sdk::prelude::err_step(
                        omnifs_sdk::error::ProviderError::internal("provider not initialized")
                    );
                };
                let cx = omnifs_sdk::__internal::Cx::<#state_type>::new(id, state);
                let future_cx = cx.clone();
                let future: ::std::pin::Pin<Box<dyn ::core::future::Future<Output = omnifs_sdk::prelude::ProviderReturn>>> =
                    Box::pin(async move {
                        let registry = match mount_registry() {
                            Ok(registry) => registry,
                            Err(error) => return omnifs_sdk::prelude::err(error),
                        };
                        match registry.open_file(&future_cx, &path).await {
                            Ok(opened) => {
                                let Some(handle) = RANGE_READERS.with(|readers| {
                                    readers.allocate(opened.reader)
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

            fn read_chunk(
                id: u64,
                handle: u64,
                offset: u64,
                length: u32,
            ) -> omnifs_sdk::prelude::ProviderStep {
                let Ok(state) = state_handle() else {
                    return omnifs_sdk::prelude::err_step(
                        omnifs_sdk::error::ProviderError::internal("provider not initialized")
                    );
                };
                let Some(handle_id) = ::std::num::NonZeroU64::new(handle) else {
                    return omnifs_sdk::prelude::err_step(
                        omnifs_sdk::error::ProviderError::not_found(format!("unknown file handle {handle}"))
                    );
                };
                let Some(reader) = RANGE_READERS.with(|readers| readers.get(handle_id)) else {
                    return omnifs_sdk::prelude::err_step(
                        omnifs_sdk::error::ProviderError::not_found(format!("unknown file handle {handle}"))
                    );
                };
                let cx = omnifs_sdk::__internal::Cx::<#state_type>::new(id, state);
                let future: ::std::pin::Pin<Box<dyn ::core::future::Future<Output = omnifs_sdk::prelude::ProviderReturn>>> =
                    Box::pin(async move {
                        match reader.read_chunk(offset, length).await {
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
                    RANGE_READERS.with(|readers| readers.remove(handle_id));
                }
            }
        }
    }
}

fn generate_notify_impl(
    type_name: &syn::Ident,
    state_type: &Type,
    has_on_event: bool,
) -> TokenStream2 {
    let dispatch_body = if has_on_event {
        quote! {
            match #type_name::on_event(future_cx, event).await {
                Ok(effects) => omnifs_sdk::prelude::ProviderReturn::with_effects(
                    omnifs_sdk::prelude::OpResult::OnEvent,
                    effects,
                ),
                Err(error) => omnifs_sdk::prelude::err(error),
            }
        }
    } else {
        quote! {
            let _ = (future_cx, event);
            omnifs_sdk::prelude::ProviderReturn::with_effects(
                omnifs_sdk::prelude::OpResult::OnEvent,
                omnifs_sdk::prelude::Effects::new(),
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
                let cx = omnifs_sdk::__internal::Cx::<#state_type>::from_event(id, state, &event);
                let future_cx = cx.clone();
                let future: ::std::pin::Pin<Box<dyn ::core::future::Future<Output = omnifs_sdk::prelude::ProviderReturn>>> =
                    Box::pin(async move { #dispatch_body });
                ASYNC_RUNTIME.with(|runtime| runtime.start(id, cx, future))
            }
        }
    }
}

pub(crate) fn provider_impl(args: &ProviderArgs, input: ItemImpl) -> syn::Result<TokenStream2> {
    reject_removed_route_surface(&input.items)?;

    let type_name = match &*input.self_ty {
        Type::Path(path) => path
            .path
            .segments
            .last()
            .map(|segment| segment.ident.clone()),
        _ => None,
    }
    .ok_or_else(|| syn::Error::new(input.self_ty.span(), "expected a named type"))?;

    let classified = classify_methods(input.items)?;
    let state_type = classified.init.state();

    let init_func = classified.init.func();
    let on_event_tokens = classified
        .on_event
        .iter()
        .map(|func| quote! { #func })
        .collect::<Vec<_>>();
    let resume_notify_tokens = classified
        .resume_notify
        .iter()
        .map(|func| quote! { #func })
        .collect::<Vec<_>>();
    let cancel_notify_tokens = classified
        .cancel_notify
        .iter()
        .map(|func| quote! { #func })
        .collect::<Vec<_>>();
    let helper_funcs = &classified.helpers;

    let state_mgmt = generate_state_management(state_type);
    let registry_builder = generate_registry_builder(state_type, &args.mount_modules);
    let lifecycle_impl = generate_lifecycle_impl(&type_name, &classified.init);
    let resume_impl = generate_resume_impl(
        &type_name,
        classified.resume_notify.as_ref(),
        classified.cancel_notify.as_ref(),
    );
    let browse_impl = generate_browse_impl(&type_name, state_type);
    let notify_impl = generate_notify_impl(&type_name, state_type, classified.on_event.is_some());
    let provider_manifest_sections =
        generate_provider_manifest_sections(&type_name, args.metadata_path.as_ref())?;
    let provider_metadata_section = provider_manifest_sections.metadata_section;

    Ok(quote! {
        struct #type_name;

        #state_mgmt
        #registry_builder
        #provider_metadata_section

        impl #type_name {
            #init_func
            #(#on_event_tokens)*
            #(#resume_notify_tokens)*
            #(#cancel_notify_tokens)*
            #(#helper_funcs)*
        }

        #lifecycle_impl
        #resume_impl
        #browse_impl
        #notify_impl

        #[cfg(target_arch = "wasm32")]
        omnifs_sdk::export!(#type_name with_types_in omnifs_sdk);
    })
}
