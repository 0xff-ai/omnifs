use proc_macro2::TokenStream as TokenStream2;
use quote::quote;
use syn::spanned::Spanned;
use syn::{Fields, GenericArgument, ItemStruct, LitStr, PathArguments, Type, TypePath};

fn option_inner_type(ty: &Type) -> Option<&Type> {
    let Type::Path(type_path) = ty else {
        return None;
    };
    if type_path.qself.is_some() {
        return None;
    }
    let segment = type_path.path.segments.last()?;
    let PathArguments::AngleBracketed(args) = &segment.arguments else {
        return None;
    };
    let Some(GenericArgument::Type(inner)) = args.args.first() else {
        return None;
    };
    let idents: Vec<String> = type_path
        .path
        .segments
        .iter()
        .map(|seg| seg.ident.to_string())
        .collect();
    let path: Vec<&str> = idents.iter().map(String::as_str).collect();
    matches!(
        path.as_slice(),
        ["Option"] | ["std" | "core", "option", "Option"]
    )
    .then_some(inner)
}

fn facet_inner_type(ty: &Type) -> Option<&Type> {
    let Type::Path(TypePath { path, .. }) = ty else {
        return None;
    };
    let segment = path.segments.last()?;
    if segment.ident != "Facet" {
        return None;
    }
    let PathArguments::AngleBracketed(args) = &segment.arguments else {
        return None;
    };
    let Some(GenericArgument::Type(inner)) = args.args.first() else {
        return None;
    };
    Some(inner)
}

struct FieldSpec {
    ident: syn::Ident,
    name_lit: LitStr,
    ty: Type,
    is_option: bool,
    facet_inner_ty: Option<Type>,
}

pub(crate) fn path_captures_impl(item: &ItemStruct) -> syn::Result<TokenStream2> {
    let Fields::Named(named_fields) = &item.fields else {
        return Err(syn::Error::new(
            item.span(),
            "#[path_captures] requires a struct with named fields",
        ));
    };

    let fields: Vec<FieldSpec> = named_fields
        .named
        .iter()
        .map(|field| {
            let ident = field.ident.as_ref().expect("named field has ident").clone();
            let name_lit = LitStr::new(&ident.to_string(), ident.span());
            let facet_inner_ty = facet_inner_type(&field.ty).cloned();
            Ok(FieldSpec {
                ident,
                name_lit,
                ty: field.ty.clone(),
                is_option: option_inner_type(&field.ty).is_some(),
                facet_inner_ty,
            })
        })
        .collect::<syn::Result<Vec<_>>>()?;

    let field_inits = fields.iter().map(|field| {
        let ident = &field.ident;
        let value = field_capture_value(field);
        quote! {
            #ident: #value,
        }
    });

    let present_validations = fields.iter().map(|field| {
        let name_lit = &field.name_lit;
        let ty = &field.ty;
        let value = field_capture_value(field);
        quote! {
            if caps.get(#name_lit).is_some() {
                let _: #ty = #value;
            }
        }
    });

    let identity_fields: Vec<_> = fields
        .iter()
        .filter(|f| f.facet_inner_ty.is_none())
        .collect();
    let identity_captures_body = identity_fields.iter().map(|field| {
        let name_lit = &field.name_lit;
        let ident = &field.ident;
        quote! {
            captures.push((#name_lit, self.#ident.to_string()));
        }
    });

    let facet_fields: Vec<_> = fields
        .iter()
        .filter(|field| field.facet_inner_ty.is_some())
        .collect();
    let facet_axis_pushes = facet_fields.iter().map(|field| {
        let name_lit = &field.name_lit;
        let inner_ty = field
            .facet_inner_ty
            .as_ref()
            .expect("facet field has inner type");
        quote! {
            if let ::core::option::Option::Some(choices) =
                <#inner_ty as omnifs_sdk::captures::PathSegment>::choices()
            {
                axes.push(omnifs_sdk::object::FacetAxis {
                    capture_name: #name_lit,
                    choices,
                });
            }
        }
    });
    let struct_ident = &item.ident;
    let (impl_generics, ty_generics, where_clause) = item.generics.split_for_impl();
    let facet_metadata_impl = if facet_fields.is_empty() {
        quote! {
            impl #impl_generics omnifs_sdk::object::FacetMetadata for #struct_ident #ty_generics
            #where_clause
            {
                fn facet_axes() -> &'static [omnifs_sdk::object::FacetAxis] {
                    &[]
                }
            }
        }
    } else {
        quote! {
            impl #impl_generics omnifs_sdk::object::FacetMetadata for #struct_ident #ty_generics
            #where_clause
            {
                fn facet_axes() -> &'static [omnifs_sdk::object::FacetAxis] {
                    static AXES: ::std::sync::OnceLock<
                        ::std::boxed::Box<[omnifs_sdk::object::FacetAxis]>,
                    > = ::std::sync::OnceLock::new();
                    let axes = AXES.get_or_init(|| {
                        let mut axes = ::std::vec::Vec::new();
                        #(#facet_axis_pushes)*
                        axes.into_boxed_slice()
                    });
                    &**axes
                }
            }
        }
    };

    Ok(quote! {
        #item

        impl #impl_generics omnifs_sdk::captures::FromCaptures for #struct_ident #ty_generics
        #where_clause
        {
            fn from_captures(
                caps: &omnifs_sdk::captures::Captures,
            ) -> omnifs_sdk::error::Result<Self> {
                Ok(Self {
                    #(#field_inits)*
                })
            }

            fn validate_present_captures(caps: &omnifs_sdk::captures::Captures) -> bool {
                (|| -> omnifs_sdk::error::Result<()> {
                    #(#present_validations)*
                    Ok(())
                })()
                .is_ok()
            }
        }

        impl #impl_generics omnifs_sdk::identity::IdentityCaptures for #struct_ident #ty_generics
        #where_clause
        {
            fn identity_captures(&self) -> ::std::vec::Vec<(&'static str, ::std::string::String)> {
                let mut captures = ::std::vec::Vec::new();
                #(#identity_captures_body)*
                captures
            }
        }

        #facet_metadata_impl

        impl #impl_generics omnifs_sdk::object::Key for #struct_ident #ty_generics
        #where_clause
        {
        }
    })
}

fn field_capture_value(field: &FieldSpec) -> TokenStream2 {
    let name_lit = &field.name_lit;
    if field.is_option {
        quote! { caps.parse_optional(#name_lit)? }
    } else if field.facet_inner_ty.is_some() {
        // Construct the Facet newtype by inference: writing `#ty(..)` would emit
        // `Facet<T>(..)`, which parses as a chained comparison in expression
        // position. The inner type is inferred from the field.
        quote! { omnifs_sdk::identity::Facet(caps.parse(#name_lit)?) }
    } else {
        quote! { caps.parse(#name_lit)? }
    }
}
