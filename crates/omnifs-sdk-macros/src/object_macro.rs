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
}

impl Parse for ObjectArgs {
    fn parse(input: ParseStream<'_>) -> syn::Result<Self> {
        let mut kind: Option<LitStr> = None;
        let mut object_key: Option<Path> = None;
        let mut canonical: Option<Ident> = None;
        let mut parse: Option<Path> = None;
        let mut stability: Option<Ident> = None;

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
                _ => {
                    return Err(syn::Error::new(
                        key.span(),
                        "supported object arguments are `kind = \"...\"`, `key = Type`, `canonical = Ident`, `parse = path::to::function`, and `stability = Ident`",
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

    let stability_tokens = if let Some(stability) = &args.stability {
        quote! { omnifs_sdk::file_attrs::Stability::#stability }
    } else {
        quote! { omnifs_sdk::file_attrs::Stability::Mutable }
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

            fn default_stability() -> omnifs_sdk::file_attrs::Stability {
                #stability_tokens
            }

            #parse_tokens
        }
    })
}
