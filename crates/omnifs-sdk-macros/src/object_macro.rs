//! Attribute macro generating `Object` metadata for an object struct.

use proc_macro2::TokenStream as TokenStream2;
use quote::quote;
use syn::parse::{Parse, ParseStream};
use syn::{Ident, ItemStruct, LitStr, Path, Token};

pub(crate) struct ObjectArgs {
    pub kind: LitStr,
    pub key: Path,
    pub canonical: Option<Ident>,
    pub parse: Option<Path>,
    pub stability: Option<Ident>,
    pub stability_fn: Option<Path>,
}

impl Parse for ObjectArgs {
    fn parse(input: ParseStream<'_>) -> syn::Result<Self> {
        let mut kind: Option<LitStr> = None;
        let mut object_key: Option<Path> = None;
        let mut canonical: Option<Ident> = None;
        let mut parse: Option<Path> = None;
        let mut stability: Option<Ident> = None;
        let mut stability_fn: Option<Path> = None;

        while !input.is_empty() {
            let key: Ident = input.parse()?;
            match key.to_string().as_str() {
                "kind" => {
                    if kind.is_some() {
                        return Err(syn::Error::new(key.span(), "duplicate `kind` argument"));
                    }
                    let _: Token![=] = input.parse()?;
                    kind = Some(input.parse()?);
                },
                "key" => {
                    if object_key.is_some() {
                        return Err(syn::Error::new(key.span(), "duplicate `key` argument"));
                    }
                    let _: Token![=] = input.parse()?;
                    object_key = Some(input.parse()?);
                },
                "canonical" => {
                    if canonical.is_some() {
                        return Err(syn::Error::new(
                            key.span(),
                            "duplicate `canonical` argument",
                        ));
                    }
                    let _: Token![=] = input.parse()?;
                    canonical = Some(input.parse()?);
                },
                "parse" => {
                    if parse.is_some() {
                        return Err(syn::Error::new(key.span(), "duplicate `parse` argument"));
                    }
                    let _: Token![=] = input.parse()?;
                    parse = Some(input.parse()?);
                },
                "stability" => {
                    if stability.is_some() {
                        return Err(syn::Error::new(
                            key.span(),
                            "duplicate `stability` argument",
                        ));
                    }
                    let _: Token![=] = input.parse()?;
                    stability = Some(input.parse()?);
                },
                "stability_fn" => {
                    if stability_fn.is_some() {
                        return Err(syn::Error::new(
                            key.span(),
                            "duplicate `stability_fn` argument",
                        ));
                    }
                    let _: Token![=] = input.parse()?;
                    stability_fn = Some(input.parse()?);
                },
                _ => {
                    return Err(syn::Error::new(
                        key.span(),
                        "supported object arguments are `kind = \"...\"`, `key = Type`, `canonical = Ident`, `parse = path::to::function`, `stability = Variant`, and `stability_fn = path::to::function`",
                    ));
                },
            }
            if input.peek(Token![,]) {
                let _: Token![,] = input.parse()?;
            }
        }

        let kind = kind.ok_or_else(|| {
            syn::Error::new(
                proc_macro2::Span::call_site(),
                "missing required `kind = \"...\"` argument",
            )
        })?;
        let object_key = object_key.ok_or_else(|| {
            syn::Error::new(
                proc_macro2::Span::call_site(),
                "missing required `key = Type` argument",
            )
        })?;

        Ok(Self {
            kind,
            key: object_key,
            canonical,
            parse,
            stability,
            stability_fn,
        })
    }
}

#[allow(clippy::unnecessary_wraps)]
pub(crate) fn object_item_impl(args: &ObjectArgs, item: &ItemStruct) -> syn::Result<TokenStream2> {
    let struct_ident = &item.ident;
    let (impl_generics, ty_generics, where_clause) = item.generics.split_for_impl();
    let kind_val = &args.kind;
    let object_key = &args.key;

    let canonical_ident = args
        .canonical
        .clone()
        .unwrap_or_else(|| Ident::new("Json", proc_macro2::Span::call_site()));
    if args.parse.is_none() && canonical_ident != "Json" {
        return Err(syn::Error::new(
            canonical_ident.span(),
            "non-JSON canonical content requires `parse = path::to::function`",
        ));
    }

    // Stability is required and has no default: a constant `stability = Variant`
    // (the same for every key) or a key-conditional `stability_fn = path` that
    // maps `&Self::Key` to a `Stability` (e.g. a pinned version is `Stable`
    // while a "latest" alias is `Dynamic`). Exactly one must be given.
    let stability_method = match (&args.stability, &args.stability_fn) {
        (Some(_), Some(path)) => {
            return Err(syn::Error::new_spanned(
                path,
                "object takes either `stability = Variant` or `stability_fn = path`, not both",
            ));
        },
        (Some(variant), None) => quote! {
            fn stability(_key: &Self::Key) -> omnifs_sdk::file_attrs::Stability {
                omnifs_sdk::file_attrs::Stability::#variant
            }
        },
        (None, Some(path)) => quote! {
            fn stability(key: &Self::Key) -> omnifs_sdk::file_attrs::Stability {
                #path(key)
            }
        },
        (None, None) => {
            return Err(syn::Error::new(
                proc_macro2::Span::call_site(),
                "object requires `stability = <Stable|Dynamic|Live>` for a constant stability, or `stability_fn = path` for one that depends on the key",
            ));
        },
    };

    let parse_tokens = args.parse.as_ref().map(|parse| {
        quote! {
            fn parse_canonical(bytes: &[u8]) -> omnifs_sdk::error::Result<Self> {
                #parse(bytes)
            }
        }
    });

    Ok(quote! {
        #item

        impl #impl_generics omnifs_sdk::object::Object for #struct_ident #ty_generics #where_clause {
            type Key = #object_key;

            fn kind() -> omnifs_sdk::object::ObjectKind {
                omnifs_sdk::object::ObjectKind(#kind_val)
            }

            fn canonical_content_type() -> omnifs_sdk::ContentType {
                omnifs_sdk::ContentType::#canonical_ident
            }

            #stability_method

            #parse_tokens
        }
    })
}
