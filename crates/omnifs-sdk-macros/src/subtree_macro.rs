//! `#[subtree] impl B { ... }` proc macro.
//!
//! Processes an impl block whose `#[dir(...)]` / `#[file(...)]` items
//! describe a typed subtree dispatched via the `Handler<S>` trait.
//! Path templates are *relative to the subtree root* (e.g. `/`,
//! `/paper.pdf`, `/versions/{version}`). The generated code builds a
//! per-type `SubtreeRegistry<S, B>` lazily and routes the trait
//! methods through it.

use omnifs_mount_schema::{PathPattern, PathSegment};
use proc_macro2::TokenStream as TokenStream2;
use quote::{format_ident, quote};
use std::collections::{BTreeMap, BTreeSet};
use std::mem;
use syn::parse::{Parse, ParseStream};
use syn::spanned::Spanned;
use syn::{
    FnArg, GenericArgument, ImplItem, ItemImpl, Pat, PatType, PathArguments, Signature, Type,
};

use crate::handler_macro::{
    HandlerArgs, capture_names, handler_name_from_path_struct, parse_statements, path_struct_name,
    validate_capture_alignment,
};

pub struct SubtreeArgs;

impl Parse for SubtreeArgs {
    fn parse(input: ParseStream<'_>) -> syn::Result<Self> {
        if !input.is_empty() {
            return Err(input.error("#[subtree] takes no arguments"));
        }
        Ok(Self)
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum SubtreeKind {
    Dir,
    File,
}

pub fn expand_subtree(_args: &SubtreeArgs, mut input: ItemImpl) -> syn::Result<TokenStream2> {
    let self_ty = (*input.self_ty).clone();
    let mut state_ty: Option<Type> = None;
    let mut generated_items = Vec::new();
    let mut register_calls = Vec::new();

    let mut methods = Vec::new();
    for item in mem::take(&mut input.items) {
        let ImplItem::Fn(mut method) = item else {
            methods.push(item);
            continue;
        };

        let marked: Vec<(usize, SubtreeKind)> = method
            .attrs
            .iter()
            .enumerate()
            .filter_map(|(index, attr)| {
                let ident = attr.path().segments.last()?.ident.to_string();
                match ident.as_str() {
                    "dir" => Some((index, SubtreeKind::Dir)),
                    "file" => Some((index, SubtreeKind::File)),
                    _ => None,
                }
            })
            .collect();

        if marked.is_empty() {
            methods.push(ImplItem::Fn(method));
            continue;
        }
        if marked.len() > 1 {
            return Err(syn::Error::new(
                method.sig.ident.span(),
                "subtree handlers can only have one path attribute (`dir` or `file`)",
            ));
        }
        let (index, kind) = marked[0];
        let attr = method.attrs.remove(index);
        let handler_args: HandlerArgs = attr.parse_args()?;
        let signature = parse_subtree_signature(&method.sig, &self_ty)?;
        let item_state = signature.state_ty.clone();
        let item_bindings = signature.bindings_ty.clone();

        if !types_equal(&item_bindings, &self_ty) {
            return Err(syn::Error::new(
                method.sig.ident.span(),
                "subtree handler bindings type must match the impl's Self type",
            ));
        }
        if let Some(expected) = state_ty.as_ref() {
            if !types_equal(&item_state, expected) {
                return Err(syn::Error::new(
                    method.sig.ident.span(),
                    "all subtree handler state types must match",
                ));
            }
        } else {
            state_ty = Some(item_state.clone());
        }

        let generated = expand_subtree_handler_items(
            kind,
            &handler_args,
            &method.sig,
            &self_ty,
            &item_state,
            &signature.captures,
        )?;
        generated_items.push(generated);
        register_calls.push(format_ident!(
            "__omnifs_subtree_register_{}",
            method.sig.ident
        ));
        methods.push(ImplItem::Fn(method));
    }

    input.items = methods;

    let state_ty = state_ty.ok_or_else(|| {
        syn::Error::new(
            input.self_ty.span(),
            "#[subtree] impl must declare at least one #[dir] or #[file] handler",
        )
    })?;

    let register_bodies: Vec<TokenStream2> = register_calls
        .iter()
        .map(|name| quote! { #name(&mut registry); })
        .collect();

    let handler_impl = quote! {
        impl #self_ty {
            fn __omnifs_subtree_registry()
                -> &'static omnifs_sdk::__internal::SubtreeRegistry<#state_ty, #self_ty>
            {
                static REGISTRY: std::sync::LazyLock<
                    omnifs_sdk::__internal::SubtreeRegistry<#state_ty, #self_ty>
                > = std::sync::LazyLock::new(|| {
                    let mut registry = omnifs_sdk::__internal::SubtreeRegistry::new();
                    #(#register_bodies)*
                    registry.validate().expect("subtree registry validation failed");
                    registry
                });
                &REGISTRY
            }
        }

        impl omnifs_sdk::handler::Handler<#state_ty> for #self_ty {
            fn lookup_child<'a>(
                &'a self,
                __cx: &'a omnifs_sdk::Cx<#state_ty>,
                __parent: &'a str,
                __name: &'a str,
            ) -> omnifs_sdk::handler::BoxFuture<'a, omnifs_sdk::browse::Lookup> {
                Box::pin(async move {
                    Self::__omnifs_subtree_registry()
                        .lookup_child(__cx, self, __parent, __name)
                        .await
                })
            }

            fn list_children<'a>(
                &'a self,
                __cx: &'a omnifs_sdk::Cx<#state_ty>,
                __path: &'a str,
            ) -> omnifs_sdk::handler::BoxFuture<'a, omnifs_sdk::browse::List> {
                Box::pin(async move {
                    Self::__omnifs_subtree_registry()
                        .list_children(__cx, self, __path)
                        .await
                })
            }

            fn read_file<'a>(
                &'a self,
                __cx: &'a omnifs_sdk::Cx<#state_ty>,
                __path: &'a str,
            ) -> omnifs_sdk::handler::BoxFuture<'a, omnifs_sdk::browse::FileContent> {
                Box::pin(async move {
                    Self::__omnifs_subtree_registry()
                        .read_file(__cx, self, __path)
                        .await
                })
            }

            fn open_file<'a>(
                &'a self,
                __cx: &'a omnifs_sdk::Cx<#state_ty>,
                __path: &'a str,
            ) -> omnifs_sdk::handler::BoxFuture<'a, omnifs_sdk::handler::OpenedFile> {
                Box::pin(async move {
                    Self::__omnifs_subtree_registry()
                        .open_file(__cx, self, __path)
                        .await
                })
            }
        }
    };

    Ok(quote! {
        #(#generated_items)*
        #input
        #handler_impl
    })
}

struct SubtreeSignature {
    state_ty: Type,
    bindings_ty: Type,
    captures: Vec<(String, Type)>,
}

fn parse_subtree_signature(sig: &Signature, expected_self: &Type) -> syn::Result<SubtreeSignature> {
    let mut iter = sig.inputs.iter();
    let Some(first) = iter.next() else {
        return Err(syn::Error::new(
            sig.span(),
            "subtree handler must take a context argument (`&BindCtx<'_, State, Self>`)",
        ));
    };
    let FnArg::Typed(PatType { ty, .. }) = first else {
        return Err(syn::Error::new(
            first.span(),
            "subtree handler context must be `&BindCtx<'_, State, Self>`",
        ));
    };
    let (state_ty, bindings_ty) = extract_bind_ctx_args(ty)?;
    let _ = expected_self; // bindings are checked in the caller against Self.

    let mut captures = Vec::new();
    for arg in iter {
        let FnArg::Typed(PatType { pat, ty, .. }) = arg else {
            return Err(syn::Error::new(
                arg.span(),
                "subtree handler parameters must be typed",
            ));
        };
        let name = match &**pat {
            Pat::Ident(pat_ident) => pat_ident
                .ident
                .to_string()
                .trim_start_matches('_')
                .to_string(),
            _ => {
                return Err(syn::Error::new(
                    pat.span(),
                    "subtree handler capture parameter must be a simple identifier",
                ));
            },
        };
        captures.push((name, (**ty).clone()));
    }

    Ok(SubtreeSignature {
        state_ty,
        bindings_ty,
        captures,
    })
}

fn extract_bind_ctx_args(ty: &Type) -> syn::Result<(Type, Type)> {
    let Type::Reference(r) = ty else {
        return Err(syn::Error::new(
            ty.span(),
            "subtree handler context must be `&BindCtx<'_, State, Self>`",
        ));
    };
    let Type::Path(tp) = &*r.elem else {
        return Err(syn::Error::new(
            r.elem.span(),
            "subtree handler context must be `&BindCtx<'_, State, Self>`",
        ));
    };
    let segment = tp.path.segments.last().ok_or_else(|| {
        syn::Error::new(
            tp.span(),
            "subtree handler context must be `&BindCtx<'_, State, Self>`",
        )
    })?;
    if segment.ident != "BindCtx" {
        return Err(syn::Error::new(
            segment.ident.span(),
            "subtree handler context must be `&BindCtx<'_, State, Self>`",
        ));
    }
    let PathArguments::AngleBracketed(args) = &segment.arguments else {
        return Err(syn::Error::new(
            segment.span(),
            "BindCtx must have lifetime + state + bindings type arguments",
        ));
    };
    // Skip the lifetime; collect type arguments.
    let type_args: Vec<&Type> = args
        .args
        .iter()
        .filter_map(|arg| match arg {
            GenericArgument::Type(ty) => Some(ty),
            _ => None,
        })
        .collect();
    if type_args.len() != 2 {
        return Err(syn::Error::new(
            args.span(),
            "BindCtx must have exactly 2 type arguments: state and bindings",
        ));
    }
    Ok((type_args[0].clone(), type_args[1].clone()))
}

#[allow(clippy::needless_pass_by_value)]
fn expand_subtree_handler_items(
    kind: SubtreeKind,
    args: &HandlerArgs,
    sig: &Signature,
    self_ty: &Type,
    state_ty: &Type,
    captures: &[(String, Type)],
) -> syn::Result<TokenStream2> {
    let template = args.template();
    let fn_name = &sig.ident;
    let pattern = PathPattern::parse(&template.value())
        .map_err(|error| syn::Error::new(template.span(), error.message()))?;
    let template_captures = capture_names(pattern.segments());
    let rest_captures: BTreeSet<String> = pattern
        .segments()
        .iter()
        .filter_map(|segment| match segment {
            PathSegment::Rest { name } => Some(name.clone()),
            _ => None,
        })
        .collect();
    validate_capture_alignment(captures, &template_captures, &template, &rest_captures)?;

    let path_struct = path_struct_name(fn_name);
    let register_name = format_ident!("__omnifs_subtree_register_{}", fn_name);
    let parse_name = format_ident!("__omnifs_subtree_parse_{}", fn_name);
    let call_name = format_ident!("__omnifs_subtree_call_{}", fn_name);

    let capture_type_map: BTreeMap<String, Type> = captures
        .iter()
        .map(|(name, ty)| (name.clone(), ty.clone()))
        .collect();

    let path_struct_fields = capture_type_map
        .iter()
        .map(|(name, ty)| {
            let ident = format_ident!("{name}");
            quote! { pub #ident: #ty }
        })
        .collect::<Vec<_>>();
    let path_struct_inits = capture_type_map
        .keys()
        .map(|name| {
            let ident = format_ident!("{name}");
            quote! { #ident }
        })
        .collect::<Vec<_>>();

    let (len_check, parse_stmts) = parse_statements(pattern.segments(), &capture_type_map);

    let await_tokens = if sig.asyncness.is_some() {
        quote! { .await }
    } else {
        quote! {}
    };

    let source_order_idents = captures
        .iter()
        .map(|(name, _)| format_ident!("__omnifs_cap_{name}"))
        .collect::<Vec<_>>();
    let destructure_fields = captures
        .iter()
        .zip(&source_order_idents)
        .map(|((name, _), local)| {
            let field = format_ident!("{name}");
            quote! { #field: #local }
        })
        .collect::<Vec<_>>();

    let call_target = quote! { <#self_ty>::#fn_name };

    let call_body = match kind {
        SubtreeKind::Dir => quote! {
            fn #call_name<'a>(
                __omnifs_cx: &'a omnifs_sdk::Cx<#state_ty>,
                __omnifs_bindings: &'a #self_ty,
                __omnifs_path: Box<dyn std::any::Any>,
                __omnifs_intent: omnifs_sdk::handler::DirIntent,
            ) -> omnifs_sdk::handler::BoxFuture<'a, omnifs_sdk::handler::Projection> {
                let __omnifs_path: Box<#path_struct> = __omnifs_path
                    .downcast()
                    .unwrap_or_else(|_| panic!("subtree dir handler path type mismatch for {}", stringify!(#fn_name)));
                let #path_struct { #(#destructure_fields,)* .. } = *__omnifs_path;
                Box::pin(async move {
                    let __omnifs_handoff_ctx = omnifs_sdk::handler::BindCtx::new(
                        __omnifs_cx,
                        __omnifs_bindings,
                        Some(__omnifs_intent),
                    );
                    #call_target(&__omnifs_handoff_ctx, #(#source_order_idents,)*) #await_tokens
                })
            }
        },
        SubtreeKind::File => quote! {
            fn #call_name<'a>(
                __omnifs_cx: &'a omnifs_sdk::Cx<#state_ty>,
                __omnifs_bindings: &'a #self_ty,
                __omnifs_path: Box<dyn std::any::Any>,
            ) -> omnifs_sdk::handler::BoxFuture<'a, omnifs_sdk::handler::FileContent> {
                let __omnifs_path: Box<#path_struct> = __omnifs_path
                    .downcast()
                    .unwrap_or_else(|_| panic!("subtree file handler path type mismatch for {}", stringify!(#fn_name)));
                let #path_struct { #(#destructure_fields,)* .. } = *__omnifs_path;
                Box::pin(async move {
                    let __omnifs_handoff_ctx = omnifs_sdk::handler::BindCtx::new(
                        __omnifs_cx,
                        __omnifs_bindings,
                        None,
                    );
                    #call_target(&__omnifs_handoff_ctx, #(#source_order_idents,)*) #await_tokens
                })
            }
        },
    };

    let register_body = match kind {
        SubtreeKind::Dir => quote! {
            fn #register_name(
                registry: &mut omnifs_sdk::__internal::SubtreeRegistry<#state_ty, #self_ty>,
            ) {
                registry
                    .add_dir(#template, #parse_name, #call_name)
                    .expect("register subtree dir handler");
            }
        },
        SubtreeKind::File => quote! {
            fn #register_name(
                registry: &mut omnifs_sdk::__internal::SubtreeRegistry<#state_ty, #self_ty>,
            ) {
                registry
                    .add_file(#template, #parse_name, #call_name)
                    .expect("register subtree file handler");
            }
        },
    };

    let manifest_captures = template_captures
        .iter()
        .map(|name| {
            let ty = &capture_type_map[name];
            omnifs_mount_schema::ManifestCaptureRecord {
                name: name.clone(),
                type_name: quote!(#ty).to_string(),
            }
        })
        .collect::<Vec<_>>();
    let handler_name_str = handler_name_from_path_struct(&path_struct);
    let subtree_kind_record = match kind {
        SubtreeKind::Dir => omnifs_mount_schema::HandlerKindRecord::Dir,
        SubtreeKind::File => omnifs_mount_schema::HandlerKindRecord::File,
    };
    let manifest_bytes =
        omnifs_mount_schema::encode_subtree_route(&omnifs_mount_schema::SubtreeRouteRecord {
            subtree_type: quote!(#self_ty).to_string(),
            path_template: template.value(),
            handler_name: handler_name_str,
            handler_kind: subtree_kind_record,
            capture_schema: manifest_captures,
        })
        .map_err(|error| {
            syn::Error::new(
                fn_name.span(),
                format!("failed to encode subtree route manifest record: {error}"),
            )
        })?;
    let manifest_len = manifest_bytes.len();
    let manifest_ident = format_ident!(
        "__OMNIFS_SUBTREE_MANIFEST_{}",
        fn_name.to_string().to_uppercase()
    );

    Ok(quote! {
        #[cfg(target_arch = "wasm32")]
        #[unsafe(link_section = "omnifs.provider-manifest.v1")]
        #[used]
        static #manifest_ident: [u8; #manifest_len] = [ #(#manifest_bytes),* ];

        #[derive(Clone, Debug)]
        pub struct #path_struct {
            #(#path_struct_fields,)*
        }

        fn #parse_name(__omnifs_path: &str) -> Option<Box<dyn std::any::Any>> {
            #len_check
            #(#parse_stmts)*
            Some(Box::new(#path_struct {
                #(#path_struct_inits,)*
            }) as Box<dyn std::any::Any>)
        }

        #call_body
        #register_body
    })
}

fn types_equal(a: &Type, b: &Type) -> bool {
    fn norm(ty: &Type) -> String {
        quote::quote!(#ty).to_string()
    }
    norm(a) == norm(b)
}
