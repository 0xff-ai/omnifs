use proc_macro2::TokenStream as TokenStream2;
use quote::quote;
use syn::parse::{Parse, ParseStream};
use syn::spanned::Spanned;
use syn::{ImplItem, ImplItemFn, ItemImpl, Token, Type};

struct ClassifiedMethods {
    init: InitMethod,
    capabilities: ImplItemFn,
    config_schema: Option<ImplItemFn>,
    on_event: Option<ImplItemFn>,
    resume_notify: Option<ImplItemFn>,
    cancel_notify: Option<ImplItemFn>,
    helpers: Vec<ImplItemFn>,
}

struct InitMethod {
    func: ImplItemFn,
    config_type: Type,
    fallible: bool,
}

pub struct ProviderArgs {
    mount_modules: Vec<syn::Path>,
}

impl Parse for ProviderArgs {
    fn parse(input: ParseStream<'_>) -> syn::Result<Self> {
        if input.is_empty() {
            return Ok(Self {
                mount_modules: Vec::new(),
            });
        }

        let key: syn::Ident = input.parse()?;
        if key != "mounts" {
            return Err(syn::Error::new(
                key.span(),
                "supported provider argument is `mounts(...)`",
            ));
        }

        let content;
        syn::parenthesized!(content in input);
        let mut mount_modules = Vec::new();
        while !content.is_empty() {
            mount_modules.push(content.parse()?);
            if content.peek(Token![,]) {
                let _: Token![,] = content.parse()?;
            }
        }

        if !input.is_empty() {
            return Err(syn::Error::new(
                input.span(),
                "unexpected tokens after `mounts(...)`",
            ));
        }

        Ok(Self { mount_modules })
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

fn extract_init_types(func: &ImplItemFn) -> syn::Result<(Type, bool)> {
    let config_type = func
        .sig
        .inputs
        .first()
        .and_then(|arg| match arg {
            syn::FnArg::Typed(pat_type) => Some((*pat_type.ty).clone()),
            syn::FnArg::Receiver(_) => None,
        })
        .ok_or_else(|| syn::Error::new(func.sig.span(), "init must have a config parameter"))?;

    let syn::ReturnType::Type(_, ty) = &func.sig.output else {
        return Err(syn::Error::new(
            func.sig.span(),
            "init must return (State, ProviderInfo) or Result<(State, ProviderInfo)>",
        ));
    };

    if let Type::Tuple(tuple) = &**ty
        && tuple.elems.len() == 2
    {
        return Ok((config_type, false));
    }

    if let Type::Path(path) = &**ty
        && let Some(segment) = path.path.segments.last()
        && segment.ident == "Result"
        && let syn::PathArguments::AngleBracketed(args) = &segment.arguments
        && let Some(syn::GenericArgument::Type(Type::Tuple(tuple))) = args.args.first()
        && tuple.elems.len() == 2
    {
        return Ok((config_type, true));
    }

    Err(syn::Error::new(
        ty.span(),
        "init must return (State, ProviderInfo) or Result<(State, ProviderInfo)>",
    ))
}

fn extract_state_type(init: &ImplItemFn) -> syn::Result<Type> {
    let syn::ReturnType::Type(_, ty) = &init.sig.output else {
        return Err(syn::Error::new(
            init.sig.span(),
            "init must return (State, ProviderInfo) or Result<(State, ProviderInfo)>",
        ));
    };

    if let Type::Tuple(tuple) = &**ty {
        return Ok(tuple.elems[0].clone());
    }

    if let Type::Path(path) = &**ty
        && let Some(segment) = path.path.segments.last()
        && segment.ident == "Result"
        && let syn::PathArguments::AngleBracketed(args) = &segment.arguments
        && let Some(syn::GenericArgument::Type(Type::Tuple(tuple))) = args.args.first()
    {
        return Ok(tuple.elems[0].clone());
    }

    Err(syn::Error::new(
        ty.span(),
        "init must return (State, ProviderInfo) or Result<(State, ProviderInfo)>",
    ))
}

fn classify_methods(items: Vec<ImplItem>) -> syn::Result<ClassifiedMethods> {
    let mut init = None;
    let mut capabilities = None;
    let mut config_schema = None;
    let mut on_event = None;
    let mut resume_notify = None;
    let mut cancel_notify = None;
    let mut helpers = Vec::new();

    for item in items {
        match item {
            ImplItem::Fn(func) => match func.sig.ident.to_string().as_str() {
                "init" => {
                    let (config_type, fallible) = extract_init_types(&func)?;
                    init = Some(InitMethod {
                        func,
                        config_type,
                        fallible,
                    });
                },
                "capabilities" => capabilities = Some(func),
                "get_config_schema" => config_schema = Some(func),
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
        capabilities: capabilities.ok_or_else(|| {
            syn::Error::new(
                proc_macro2::Span::call_site(),
                "missing required `capabilities` method",
            )
        })?,
        config_schema,
        on_event,
        resume_notify,
        cancel_notify,
        helpers,
    })
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

fn generate_lifecycle_impl(
    type_name: &syn::Ident,
    init: &InitMethod,
    has_custom_schema: bool,
) -> TokenStream2 {
    let config_type = &init.config_type;
    let init_body = if init.fallible {
        quote! {
            let (state, info) = match #type_name::init(config) {
                Ok(parts) => parts,
                Err(error) => return omnifs_sdk::prelude::err(error),
            };
        }
    } else {
        quote! {
            let (state, info) = #type_name::init(config);
        }
    };
    let schema_body = if has_custom_schema {
        quote! { #type_name::get_config_schema() }
    } else {
        quote! { omnifs_sdk::schema::json_schema_for::<#config_type>() }
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
                    omnifs_sdk::prelude::OperationResult::Initialize(
                        omnifs_sdk::omnifs::provider::types::InitializeResult { info }
                    )
                )
            }

            fn capabilities() -> omnifs_sdk::prelude::RequestedCapabilities {
                #type_name::capabilities()
            }

            fn shutdown() {
                STATE.with(|slot| *slot.borrow_mut() = None);
                ASYNC_RUNTIME.with(|runtime| runtime.clear());
            }

            fn get_config_schema() -> Option<String> {
                #schema_body
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

fn generate_browse_impl(type_name: &syn::Ident, state_type: &Type) -> TokenStream2 {
    quote! {
        thread_local! {
            static RANGE_READERS: ::std::cell::RefCell<
                ::std::collections::BTreeMap<u64, ::std::rc::Rc<dyn omnifs_sdk::handler::RangeReader>>
            > = ::std::cell::RefCell::new(::std::collections::BTreeMap::new());
            static NEXT_RANGE_HANDLE: ::std::cell::Cell<u64> = ::std::cell::Cell::new(1);
        }

        impl omnifs_sdk::exports::omnifs::provider::browse::Guest for #type_name {
            fn lookup_child(
                id: u64,
                parent_path: String,
                name: String,
            ) -> omnifs_sdk::prelude::ProviderStep {
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
                        match registry.lookup_child(&future_cx, &parent_path, &name).await {
                            Ok(lookup) => {
                                let (result, effects) = lookup.into_result_and_effects();
                                omnifs_sdk::prelude::ProviderReturn::with_effects(
                                    omnifs_sdk::prelude::OperationResult::LookupChild(result),
                                    effects,
                                )
                            },
                            Err(error) => omnifs_sdk::prelude::err(error),
                        }
                    });
                ASYNC_RUNTIME.with(|runtime| runtime.start(id, cx, future))
            }

            fn list_children(id: u64, path: String) -> omnifs_sdk::prelude::ProviderStep {
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
                        match registry.list_children(&future_cx, &path).await {
                            Ok(list) => {
                                let (result, effects) = list.into_result_and_effects();
                                omnifs_sdk::prelude::ProviderReturn::with_effects(
                                    omnifs_sdk::prelude::OperationResult::ListChildren(result),
                                    effects,
                                )
                            },
                            Err(error) => omnifs_sdk::prelude::err(error),
                        }
                    });
                ASYNC_RUNTIME.with(|runtime| runtime.start(id, cx, future))
            }

            fn read_file(id: u64, path: String) -> omnifs_sdk::prelude::ProviderStep {
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
                        match registry.read_file(&future_cx, &path).await {
                            Ok(file) => {
                                let (result, effects) = file.into_result_and_effects();
                                omnifs_sdk::prelude::ProviderReturn::with_effects(
                                    omnifs_sdk::prelude::OperationResult::ReadFile(result),
                                    effects,
                                )
                            },
                            Err(error) => omnifs_sdk::prelude::err(error),
                        }
                    });
                ASYNC_RUNTIME.with(|runtime| runtime.start(id, cx, future))
            }

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
                                let mut reader = Some(opened.reader);
                                let handle = RANGE_READERS.with(|readers| {
                                    let mut readers = readers.borrow_mut();
                                    NEXT_RANGE_HANDLE.with(|next| {
                                        let start = next.get().max(1);
                                        let mut handle = start;
                                        loop {
                                            if !readers.contains_key(&handle) {
                                                readers.insert(
                                                    handle,
                                                    reader.take().expect("range reader stored once"),
                                                );
                                                let next_handle = handle.wrapping_add(1);
                                                next.set(if next_handle == 0 { 1 } else { next_handle });
                                                return Some(handle);
                                            }
                                            let next_handle = handle.wrapping_add(1);
                                            handle = if next_handle == 0 { 1 } else { next_handle };
                                            if handle == start {
                                                return None;
                                            }
                                        }
                                    })
                                });
                                let Some(handle) = handle else {
                                    return omnifs_sdk::prelude::err(
                                        omnifs_sdk::error::ProviderError::internal("no free ranged file handles")
                                    );
                                };
                                omnifs_sdk::prelude::ProviderReturn::terminal(
                                    omnifs_sdk::prelude::OperationResult::OpenFile(
                                        omnifs_sdk::omnifs::provider::types::OpenFileResult {
                                            handle,
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
                let Some(reader) = RANGE_READERS.with(|readers| {
                    readers.borrow().get(&handle).cloned()
                }) else {
                    return omnifs_sdk::prelude::err_step(
                        omnifs_sdk::error::ProviderError::not_found(format!("unknown file handle {handle}"))
                    );
                };
                let cx = omnifs_sdk::__internal::Cx::<#state_type>::new(id, state);
                let future: ::std::pin::Pin<Box<dyn ::core::future::Future<Output = omnifs_sdk::prelude::ProviderReturn>>> =
                    Box::pin(async move {
                        match reader.read_chunk(offset, length).await {
                            Ok(chunk) => omnifs_sdk::prelude::ProviderReturn::terminal(
                                omnifs_sdk::prelude::OperationResult::ReadChunk(chunk.into())
                            ),
                            Err(error) => omnifs_sdk::prelude::err(error),
                        }
                    });
                ASYNC_RUNTIME.with(|runtime| runtime.start(id, cx, future))
            }

            fn close_file(handle: u64) {
                RANGE_READERS.with(|readers| {
                    readers.borrow_mut().remove(&handle);
                });
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
                    omnifs_sdk::prelude::OperationResult::OnEvent,
                    effects,
                ),
                Err(error) => omnifs_sdk::prelude::err(error),
            }
        }
    } else {
        quote! {
            let _ = (future_cx, event);
            omnifs_sdk::prelude::ProviderReturn::with_effects(
                omnifs_sdk::prelude::OperationResult::OnEvent,
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

fn generate_reconcile_impl(type_name: &syn::Ident) -> TokenStream2 {
    quote! {
        impl omnifs_sdk::exports::omnifs::provider::reconcile::Guest for #type_name {
            fn plan_mutations(
                _id: u64,
                _changes: Vec<omnifs_sdk::prelude::FileChange>,
            ) -> omnifs_sdk::prelude::ProviderStep {
                omnifs_sdk::prelude::err_step(
                    omnifs_sdk::error::ProviderError::unimplemented(
                        "mutation handlers are reserved but not implemented"
                    )
                )
            }

            fn execute(
                _id: u64,
                _mutation: omnifs_sdk::prelude::PlannedMutation,
            ) -> omnifs_sdk::prelude::ProviderStep {
                omnifs_sdk::prelude::err_step(
                    omnifs_sdk::error::ProviderError::unimplemented(
                        "mutation handlers are reserved but not implemented"
                    )
                )
            }

            fn fetch_resource(
                _id: u64,
                _resource_path: String,
            ) -> omnifs_sdk::prelude::ProviderStep {
                omnifs_sdk::prelude::err_step(
                    omnifs_sdk::error::ProviderError::unimplemented("reconcile not implemented")
                )
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
    let state_type = extract_state_type(&classified.init.func)?;

    let init_func = &classified.init.func;
    let caps_func = &classified.capabilities;
    let config_schema_tokens = classified
        .config_schema
        .iter()
        .map(|func| quote! { #func })
        .collect::<Vec<_>>();
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

    let state_mgmt = generate_state_management(&state_type);
    let registry_builder = generate_registry_builder(&state_type, &args.mount_modules);
    let lifecycle_impl = generate_lifecycle_impl(
        &type_name,
        &classified.init,
        classified.config_schema.is_some(),
    );
    let resume_impl = generate_resume_impl(
        &type_name,
        classified.resume_notify.as_ref(),
        classified.cancel_notify.as_ref(),
    );
    let browse_impl = generate_browse_impl(&type_name, &state_type);
    let notify_impl = generate_notify_impl(&type_name, &state_type, classified.on_event.is_some());
    let reconcile_impl = generate_reconcile_impl(&type_name);

    Ok(quote! {
        struct #type_name;

        #state_mgmt
        #registry_builder

        impl #type_name {
            #init_func
            #caps_func
            #(#config_schema_tokens)*
            #(#on_event_tokens)*
            #(#resume_notify_tokens)*
            #(#cancel_notify_tokens)*
            #(#helper_funcs)*
        }

        #lifecycle_impl
        #resume_impl
        #browse_impl
        #notify_impl
        #reconcile_impl

        #[cfg(target_arch = "wasm32")]
        omnifs_sdk::export!(#type_name with_types_in omnifs_sdk);
    })
}
