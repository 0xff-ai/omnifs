use proc_macro2::TokenStream as TokenStream2;
use quote::quote;
use syn::spanned::Spanned;
use syn::{Attribute, Item};

pub(crate) fn config_item_impl(item: Item) -> Result<TokenStream2, syn::Error> {
    match item {
        Item::Struct(mut item_struct) => {
            add_config_attrs(&mut item_struct.attrs);
            let ident = &item_struct.ident;
            // The build-time `manifest_json()` export splices this schema into the
            // embedded manifest's `configSchema`. Everything routes through
            // `omnifs_sdk::schemars`, so a provider needs no direct schemars dep.
            let provides = quote! {
                impl omnifs_sdk::ProvidesConfigSchema for #ident {
                    fn config_schema() -> Option<omnifs_sdk::serde_json::Value> {
                        let schema = omnifs_sdk::schemars::SchemaGenerator::default()
                            .into_root_schema_for::<#ident>();
                        Some(
                            omnifs_sdk::serde_json::to_value(schema)
                                .expect("config schema serializes to JSON"),
                        )
                    }
                }
            };
            Ok(quote! {
                #item_struct
                #provides
            })
        },
        other => Err(syn::Error::new(
            other.span(),
            "#[omnifs_sdk::config] can only be used on a struct",
        )),
    }
}

fn add_config_attrs(attrs: &mut Vec<Attribute>) {
    attrs.push(syn::parse_quote! {
        #[derive(
            std::fmt::Debug,
            omnifs_sdk::serde::Deserialize,
            omnifs_sdk::schemars::JsonSchema,
        )]
    });
    attrs.push(syn::parse_quote! {
        #[serde(crate = "omnifs_sdk::serde")]
    });
    attrs.push(syn::parse_quote! {
        #[schemars(crate = "omnifs_sdk::schemars")]
    });
    attrs.push(syn::parse_quote! {
        #[serde(deny_unknown_fields)]
    });
}
