use omnifs_mount_schema::{PathPattern, PathSegment};
use proc_macro2::TokenStream as TokenStream2;
use quote::{format_ident, quote};
use std::collections::{BTreeMap, BTreeSet};
use std::mem;
use syn::parse::{Parse, ParseStream};
use syn::spanned::Spanned;
use syn::{
    Attribute, FnArg, GenericArgument, Ident, ImplItem, ImplItemFn, ItemFn, ItemImpl, LitStr, Pat,
    PatType, PathArguments, Signature, Token, Type, parse_quote,
};

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum HandlerKind {
    Dir,
    File,
    Subtree,
    Mutate,
}

impl HandlerKind {
    fn ctx_ident(self) -> &'static str {
        match self {
            Self::Dir => "DirCx",
            Self::File | Self::Subtree | Self::Mutate => "Cx",
        }
    }

    fn manifest_kind(self) -> Option<omnifs_mount_schema::HandlerKindRecord> {
        match self {
            Self::Dir => Some(omnifs_mount_schema::HandlerKindRecord::Dir),
            Self::File => Some(omnifs_mount_schema::HandlerKindRecord::File),
            Self::Subtree => Some(omnifs_mount_schema::HandlerKindRecord::Subtree),
            Self::Mutate => None,
        }
    }
}

#[derive(Clone)]
pub struct HandlerArgs {
    template: LitStr,
}

impl Parse for HandlerArgs {
    fn parse(input: ParseStream<'_>) -> syn::Result<Self> {
        let template: LitStr = input.parse()?;
        if !input.is_empty() {
            return Err(input.error(
                "unexpected handler argument; capture types now come from the function signature",
            ));
        }
        Ok(Self { template })
    }
}

pub struct HandlersArgs {
    pub state: Type,
    pub explicit_state: bool,
}

impl Parse for HandlersArgs {
    fn parse(input: ParseStream<'_>) -> syn::Result<Self> {
        if input.is_empty() {
            return Ok(Self {
                state: parse_quote!(State),
                explicit_state: false,
            });
        }
        let key: Ident = input.parse()?;
        if key != "state" {
            return Err(syn::Error::new(
                key.span(),
                "supported handlers argument is `state`",
            ));
        }
        let _: Token![=] = input.parse()?;
        let state: Type = input.parse()?;
        if !input.is_empty() {
            return Err(input.error("unexpected handlers argument"));
        }
        Ok(Self {
            state,
            explicit_state: true,
        })
    }
}

#[allow(clippy::large_enum_variant)]
pub enum HandlerTarget {
    Free,
    ImplOf(Type),
}

#[allow(clippy::needless_pass_by_value)]
pub fn expand_handler(
    kind: HandlerKind,
    args: HandlerArgs,
    func: ItemFn,
) -> syn::Result<TokenStream2> {
    let handler_items = expand_handler_items(
        kind,
        &args,
        &func.sig.ident,
        &func.sig,
        &HandlerTarget::Free,
    )?;
    Ok(quote! {
        #func
        #handler_items
    })
}

pub fn expand_handler_items(
    kind: HandlerKind,
    args: &HandlerArgs,
    fn_name: &syn::Ident,
    sig: &Signature,
    target: &HandlerTarget,
) -> syn::Result<TokenStream2> {
    let template = args.template.clone();
    let HandlerSignature { state_ty, captures } = parse_signature(kind, sig)?;
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

    validate_capture_alignment(&captures, &template_captures, &template, &rest_captures)?;

    let path_struct = path_struct_name(fn_name);
    let register_name = format_ident!("__omnifs_mount_{}", fn_name);
    let parse_name = format_ident!("__omnifs_parse_{}", fn_name);
    let call_name = format_ident!("__omnifs_call_{}", fn_name);
    let manifest_ident = format_ident!("__OMNIFS_MANIFEST_{}", fn_name.to_string().to_uppercase());

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
    let manifest_bytes = if let Some(handler_kind) = kind.manifest_kind() {
        let handler_name = path_struct
            .to_string()
            .strip_suffix("Path")
            .map_or_else(|| path_struct.to_string(), str::to_string);
        let record = omnifs_mount_schema::HandlerRecord {
            path_template: template.value(),
            handler_name,
            handler_kind,
            capture_schema: manifest_captures,
        };
        omnifs_mount_schema::encode_handler(&record).map_err(|error| {
            syn::Error::new(
                fn_name.span(),
                format!("failed to encode handler manifest record: {error}"),
            )
        })?
    } else {
        let record = omnifs_mount_schema::MutationRecord {
            path_template: template.value(),
            capture_schema: manifest_captures,
        };
        omnifs_mount_schema::encode_mutation(&record).map_err(|error| {
            syn::Error::new(
                fn_name.span(),
                format!("failed to encode mutation manifest record: {error}"),
            )
        })?
    };
    let manifest_len = manifest_bytes.len();
    let manifest_lits = manifest_bytes;

    let register_body = match kind {
        HandlerKind::Dir => quote! {
            pub(crate) fn #register_name(
                registry: &mut omnifs_sdk::__internal::MountRegistry<#state_ty>,
            ) {
                registry
                    .add_dir(#template, #parse_name, #call_name)
                    .expect("register dir handler");
            }
        },
        HandlerKind::File => quote! {
            pub(crate) fn #register_name(
                registry: &mut omnifs_sdk::__internal::MountRegistry<#state_ty>,
            ) {
                registry
                    .add_file(#template, #parse_name, #call_name)
                    .expect("register file handler");
            }
        },
        HandlerKind::Subtree => quote! {
            pub(crate) fn #register_name(
                registry: &mut omnifs_sdk::__internal::MountRegistry<#state_ty>,
            ) {
                registry
                    .add_subtree(#template, #parse_name, #call_name)
                    .expect("register subtree handler");
            }
        },
        HandlerKind::Mutate => TokenStream2::new(),
    };

    let call_target = match target {
        HandlerTarget::Free => quote! { #fn_name },
        HandlerTarget::ImplOf(ty) => quote! { <#ty>::#fn_name },
    };
    let call_body = match kind {
        HandlerKind::Dir => quote! {
            fn #call_name<'a>(
                __omnifs_cx: &'a omnifs_sdk::Cx<#state_ty>,
                __omnifs_path: Box<dyn std::any::Any>,
                __omnifs_intent: omnifs_sdk::handler::DirIntent<'a>,
            ) -> omnifs_sdk::handler::BoxFuture<'a, omnifs_sdk::handler::Projection> {
                let __omnifs_path: Box<#path_struct> = __omnifs_path
                    .downcast()
                    .unwrap_or_else(|_| panic!("dir handler path type mismatch for {}", stringify!(#fn_name)));
                let #path_struct { #(#destructure_fields,)* .. } = *__omnifs_path;
                Box::pin(async move {
                    let __omnifs_dir_cx = omnifs_sdk::handler::DirCx::new(__omnifs_cx, __omnifs_intent);
                    #call_target(&__omnifs_dir_cx, #(#source_order_idents,)*) #await_tokens
                })
            }
        },
        HandlerKind::File => quote! {
            fn #call_name<'a>(
                __omnifs_cx: &'a omnifs_sdk::Cx<#state_ty>,
                __omnifs_path: Box<dyn std::any::Any>,
            ) -> omnifs_sdk::handler::BoxFuture<'a, omnifs_sdk::handler::FileContent> {
                let __omnifs_path: Box<#path_struct> = __omnifs_path
                    .downcast()
                    .unwrap_or_else(|_| panic!("file handler path type mismatch for {}", stringify!(#fn_name)));
                let #path_struct { #(#destructure_fields,)* .. } = *__omnifs_path;
                Box::pin(async move {
                    #call_target(__omnifs_cx, #(#source_order_idents,)*) #await_tokens
                })
            }
        },
        HandlerKind::Subtree => quote! {
            fn #call_name<'a>(
                __omnifs_cx: &'a omnifs_sdk::Cx<#state_ty>,
                __omnifs_path: Box<dyn std::any::Any>,
            ) -> omnifs_sdk::handler::BoxFuture<'a, omnifs_sdk::handler::SubtreeRef> {
                let __omnifs_path: Box<#path_struct> = __omnifs_path
                    .downcast()
                    .unwrap_or_else(|_| panic!("subtree handler path type mismatch for {}", stringify!(#fn_name)));
                let #path_struct { #(#destructure_fields,)* .. } = *__omnifs_path;
                Box::pin(async move {
                    #call_target(__omnifs_cx, #(#source_order_idents,)*) #await_tokens
                })
            }
        },
        HandlerKind::Mutate => quote! {},
    };

    Ok(quote! {
        #[cfg(target_arch = "wasm32")]
        #[unsafe(link_section = "omnifs.provider-manifest.v1")]
        #[used]
        static #manifest_ident: [u8; #manifest_len] = [ #(#manifest_lits),* ];

        #[derive(Clone, Debug)]
        pub struct #path_struct {
            #(#path_struct_fields,)*
        }

        impl #path_struct {
            pub const MOUNT_ID: &'static str = #template;
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

fn extract_handler_attr_kind(attr: &Attribute) -> Option<HandlerKind> {
    let ident = attr.path().segments.last().map(|segment| &segment.ident)?;
    match ident.to_string().as_str() {
        "dir" => Some(HandlerKind::Dir),
        "file" => Some(HandlerKind::File),
        "subtree" => Some(HandlerKind::Subtree),
        _ => None,
    }
}

pub fn expand_handlers(args: &HandlersArgs, mut input: ItemImpl) -> syn::Result<TokenStream2> {
    let self_ty = (*input.self_ty).clone();
    let explicit_state = args.state.clone();
    let has_explicit_state = args.explicit_state;
    let mut generated_items = Vec::new();
    let mut register_calls = Vec::new();
    let mut state_ty: Option<Type> = None;

    let mut methods = Vec::new();
    for item in mem::take(&mut input.items) {
        let ImplItem::Fn(mut method) = item else {
            methods.push(item);
            continue;
        };

        let marked_attrs = method
            .attrs
            .iter()
            .enumerate()
            .filter_map(|(index, attr)| extract_handler_attr_kind(attr).map(|kind| (index, kind)))
            .collect::<Vec<_>>();
        if marked_attrs.is_empty() {
            methods.push(ImplItem::Fn(method));
            continue;
        }
        if marked_attrs.len() > 1 {
            return Err(syn::Error::new(
                method.sig.ident.span(),
                "handlers can only have one path attribute (`dir`, `file`, or `subtree`)",
            ));
        }
        let (index, kind) = marked_attrs[0];
        let attr = method.attrs.remove(index);
        let handler_args: HandlerArgs = attr.parse_args()?;
        let signature = parse_signature(kind, &method.sig)?;
        let item_state = signature.state_ty.clone();
        if has_explicit_state && item_state != explicit_state {
            return Err(syn::Error::new(
                method.sig.ident.span(),
                format!(
                    "handler state `{}` does not match `#[handlers(state = ...)]` `{}`",
                    quote!(#item_state),
                    quote!(#explicit_state),
                ),
            ));
        }
        if let Some(expected_state) = state_ty.as_ref() {
            if item_state != *expected_state {
                return Err(syn::Error::new(
                    method.sig.ident.span(),
                    format!(
                        "all handler state types must match; expected `{}`",
                        quote!(#expected_state),
                    ),
                ));
            }
        } else {
            state_ty = Some(item_state.clone());
        }
        let generated = expand_handler_items(
            kind,
            &handler_args,
            &method.sig.ident,
            &method.sig,
            &HandlerTarget::ImplOf(self_ty.clone()),
        )?;
        generated_items.push(generated);
        register_calls.push(format_ident!("__omnifs_mount_{}", method.sig.ident));
        methods.push(ImplItem::Fn(method));
    }

    input.items = methods;
    let mount_state = state_ty.unwrap_or(explicit_state);
    let register_bodies = register_calls
        .iter()
        .map(|register| quote! { #register(registry); })
        .collect::<Vec<_>>();

    let mount_method: ImplItemFn = parse_quote! {
        pub(crate) fn mount(registry: &mut omnifs_sdk::__internal::MountRegistry<#mount_state>) {
            #(#register_bodies)*
        }
    };
    input.items.push(ImplItem::Fn(mount_method));

    Ok(quote! {
        #(#generated_items)*
        #input
    })
}

struct HandlerSignature {
    state_ty: Type,
    captures: Vec<(String, Type)>,
}

fn parse_signature(kind: HandlerKind, sig: &Signature) -> syn::Result<HandlerSignature> {
    let mut iter = sig.inputs.iter();
    let Some(first) = iter.next() else {
        return Err(syn::Error::new(
            sig.span(),
            format!(
                "handler must take a context argument (`&{}<State>`)",
                kind.ctx_ident()
            ),
        ));
    };
    let FnArg::Typed(PatType { ty, .. }) = first else {
        return Err(syn::Error::new(
            first.span(),
            format!("handler context must be `&{}<State>`", kind.ctx_ident()),
        ));
    };
    let state_ty = extract_state_ty(kind, ty)?;

    let mut captures = Vec::new();
    for arg in iter {
        let FnArg::Typed(PatType { pat, ty, .. }) = arg else {
            return Err(syn::Error::new(
                arg.span(),
                "handler parameters must be typed",
            ));
        };
        let name = match &**pat {
            Pat::Ident(pat_ident) => {
                let raw = pat_ident.ident.to_string();
                raw.trim_start_matches('_').to_string()
            },
            _ => {
                return Err(syn::Error::new(
                    pat.span(),
                    "handler capture parameter must be a simple identifier",
                ));
            },
        };
        captures.push((name, (**ty).clone()));
    }

    Ok(HandlerSignature { state_ty, captures })
}

fn extract_state_ty(kind: HandlerKind, ty: &Type) -> syn::Result<Type> {
    let expected = kind.ctx_ident();
    let Type::Reference(r) = ty else {
        return Err(syn::Error::new(
            ty.span(),
            format!("handler context must be `&{expected}<State>`"),
        ));
    };
    let Type::Path(tp) = &*r.elem else {
        return Err(syn::Error::new(
            r.elem.span(),
            format!("handler context must be `&{expected}<State>`"),
        ));
    };
    let segment = tp.path.segments.last().ok_or_else(|| {
        syn::Error::new(
            tp.span(),
            format!("handler context must be `&{expected}<State>`"),
        )
    })?;
    if segment.ident != expected {
        return Err(syn::Error::new(
            segment.ident.span(),
            format!("handler context must be `&{expected}<State>`"),
        ));
    }
    let PathArguments::AngleBracketed(args) = &segment.arguments else {
        return Err(syn::Error::new(
            segment.span(),
            "handler context must name a state type",
        ));
    };
    let state_ty = args
        .args
        .iter()
        .find_map(|arg| match arg {
            GenericArgument::Type(ty) => Some(ty.clone()),
            _ => None,
        })
        .ok_or_else(|| syn::Error::new(args.span(), "handler context must name a state type"))?;
    Ok(state_ty)
}

fn path_struct_name(fn_name: &Ident) -> Ident {
    let raw = fn_name.to_string();
    let mut pascal = String::with_capacity(raw.len());
    let mut cap_next = true;
    for ch in raw.chars() {
        if ch == '_' {
            cap_next = true;
            continue;
        }
        if cap_next {
            pascal.extend(ch.to_uppercase());
            cap_next = false;
        } else {
            pascal.push(ch);
        }
    }
    pascal.push_str("Path");
    Ident::new(&pascal, fn_name.span())
}

fn validate_capture_alignment(
    sig_captures: &[(String, Type)],
    template_captures: &[String],
    template: &LitStr,
    rest_captures: &BTreeSet<String>,
) -> syn::Result<()> {
    let sig_names: BTreeSet<_> = sig_captures.iter().map(|(n, _)| n.clone()).collect();
    if sig_names.len() != sig_captures.len() {
        return Err(syn::Error::new(
            template.span(),
            "duplicate capture parameter names in handler signature",
        ));
    }
    let tpl_names: BTreeSet<_> = template_captures.iter().cloned().collect();
    for name in &tpl_names {
        if !sig_names.contains(name) {
            return Err(syn::Error::new(
                template.span(),
                format!("template capture `{{{name}}}` has no matching function parameter"),
            ));
        }
    }
    for (name, ty) in sig_captures {
        if !tpl_names.contains(name) {
            return Err(syn::Error::new(
                template.span(),
                format!("parameter `{name}` does not match any capture in the template"),
            ));
        }
        if rest_captures.contains(name) && !is_string_type(ty) {
            return Err(syn::Error::new(
                ty.span(),
                format!(
                    "rest capture `{{*{name}}}` must decode to `String` because it joins multiple segments"
                ),
            ));
        }
    }
    Ok(())
}

fn capture_names(segments: &[PathSegment]) -> Vec<String> {
    let mut names = Vec::new();
    let mut seen = BTreeSet::new();
    for segment in segments {
        let name = match segment {
            PathSegment::Capture { name, .. } | PathSegment::Rest { name } => name,
            PathSegment::Literal(_) => continue,
        };
        if seen.insert(name.clone()) {
            names.push(name.clone());
        }
    }
    names
}

fn is_string_type(ty: &Type) -> bool {
    let Type::Path(tp) = ty else {
        return false;
    };
    if tp.qself.is_some() {
        return false;
    }
    let Some(segment) = tp.path.segments.last() else {
        return false;
    };
    segment.ident == "String" && matches!(segment.arguments, PathArguments::None)
}

fn parse_statements(
    segments: &[PathSegment],
    capture_type_map: &BTreeMap<String, Type>,
) -> (TokenStream2, Vec<TokenStream2>) {
    if segments.is_empty() {
        return (
            quote! {
                if __omnifs_path != "/" {
                    return None;
                }
            },
            Vec::new(),
        );
    }

    let has_rest = matches!(segments.last(), Some(PathSegment::Rest { .. }));
    let mut stmts = Vec::new();
    let len = segments.len();
    if has_rest {
        let fixed = len - 1;
        stmts.push(quote! {
            let __omnifs_segments: Vec<&str> = __omnifs_path.trim_start_matches('/').split('/').collect();
            if __omnifs_segments.len() < #fixed {
                return None;
            }
        });
    } else {
        stmts.push(quote! {
            let __omnifs_segments: Vec<&str> = __omnifs_path.trim_start_matches('/').split('/').collect();
            if __omnifs_segments.len() != #len {
                return None;
            }
        });
    }

    for (index, segment) in segments.iter().enumerate() {
        match segment {
            PathSegment::Literal(literal) => stmts.push(quote! {
                if __omnifs_segments[#index] != #literal {
                    return None;
                }
            }),
            PathSegment::Capture { name, prefix: None } => {
                let ident = format_ident!("{name}");
                let ty = &capture_type_map[name];
                stmts.push(quote! {
                    let #ident: #ty = __omnifs_segments[#index].parse().ok()?;
                });
            },
            PathSegment::Capture {
                name,
                prefix: Some(prefix),
            } => {
                let ident = format_ident!("{name}");
                let ty = &capture_type_map[name];
                stmts.push(quote! {
                    let #ident: #ty = __omnifs_segments[#index]
                        .strip_prefix(#prefix)?
                        .parse()
                        .ok()?;
                });
            },
            PathSegment::Rest { name } => {
                // Join the trailing segments with '/'. An empty tail yields
                // an empty string, which is the contract documented on the
                // rest segment in omnifs-mount-schema.
                let ident = format_ident!("{name}");
                stmts.push(quote! {
                    let #ident: String = __omnifs_segments[#index..].join("/");
                });
            },
        }
    }

    (quote! {}, stmts)
}
