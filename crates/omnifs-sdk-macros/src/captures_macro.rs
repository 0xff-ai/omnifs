use proc_macro2::TokenStream as TokenStream2;
use quote::quote;
use syn::spanned::Spanned;
use syn::{Fields, ItemStruct, LitStr, Type, TypePath};

use crate::util::generic_type_arg;

fn option_inner_type(ty: &Type) -> Option<&Type> {
    let Type::Path(type_path) = ty else {
        return None;
    };
    if type_path.qself.is_some() {
        return None;
    }
    let segment = type_path.path.segments.last()?;
    let inner = generic_type_arg(segment, 0)?;
    let mut segments = type_path.path.segments.iter();
    let is_option = match (
        segments.next(),
        segments.next(),
        segments.next(),
        segments.next(),
    ) {
        (Some(option), None, None, None) => option.ident == "Option",
        (Some(root), Some(module), Some(option), None) => {
            (root.ident == "std" || root.ident == "core")
                && module.ident == "option"
                && option.ident == "Option"
        },
        _ => false,
    };
    is_option.then_some(inner)
}

fn facet_inner_type(ty: &Type) -> Option<&Type> {
    let Type::Path(TypePath { path, .. }) = ty else {
        return None;
    };
    let segment = path.segments.last()?;
    if segment.ident != "Facet" {
        return None;
    }
    generic_type_arg(segment, 0)
}

struct FieldSpec {
    ident: syn::Ident,
    name_lit: LitStr,
    ty: Type,
    facet_inner_ty: Option<Type>,
}

impl FieldSpec {
    fn capture_value(&self) -> TokenStream2 {
        let name = &self.name_lit;
        if option_inner_type(&self.ty).is_some() {
            quote! { caps.parse_optional(#name)? }
        } else if self.facet_inner_ty.is_some() {
            // The inner type is inferred from the field. Emitting `Facet<T>(..)`
            // would parse as a chained comparison in expression position.
            quote! { omnifs_sdk::identity::Facet(caps.parse(#name)?) }
        } else {
            quote! { caps.parse(#name)? }
        }
    }
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
                facet_inner_ty,
            })
        })
        .collect::<syn::Result<Vec<_>>>()?;

    let field_inits = fields.iter().map(|field| {
        let ident = &field.ident;
        let value = field.capture_value();
        quote! {
            #ident: #value,
        }
    });

    let present_validations = fields.iter().map(|field| {
        let name_lit = &field.name_lit;
        let ty = &field.ty;
        let value = field.capture_value();
        quote! {
            if caps.get(#name_lit).is_some() {
                let _: #ty = #value;
            }
        }
    });
    let capture_descriptors = fields.iter().map(|field| {
        let name_lit = &field.name_lit;
        let descriptor_ty = field
            .facet_inner_ty
            .as_ref()
            .or_else(|| option_inner_type(&field.ty))
            .unwrap_or(&field.ty);
        let choices = field.facet_inner_ty.as_ref().map_or_else(
            || quote! { None },
            |inner_ty| {
                quote! {
                    <#inner_ty as omnifs_sdk::captures::PathSegment>::choices().map(|choices| {
                        choices
                            .iter()
                            .map(|choice| (*choice).to_string())
                            .collect::<::std::vec::Vec<_>>()
                    })
                }
            },
        );
        quote! {
            omnifs_sdk::captures::CaptureDescriptor {
                name: #name_lit.to_string(),
                type_name: stringify!(#descriptor_ty).to_string(),
                choices: #choices,
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

            fn capture_descriptors() -> ::std::vec::Vec<omnifs_sdk::captures::CaptureDescriptor> {
                ::std::vec![#(#capture_descriptors),*]
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
