use proc_macro2::TokenStream as TokenStream2;
use quote::quote;
use syn::spanned::Spanned;
use syn::{Attribute, Item, ItemEnum, ItemStruct};

pub(crate) fn config_item_impl(item: Item) -> Result<TokenStream2, syn::Error> {
    match item {
        Item::Struct(mut item_struct) => {
            add_config_attrs_to_struct(&mut item_struct);
            Ok(quote! { #item_struct })
        },
        Item::Enum(mut item_enum) => {
            add_config_attrs_to_enum(&mut item_enum);
            Ok(quote! { #item_enum })
        },
        other => Err(syn::Error::new(
            other.span(),
            "#[omnifs_sdk::config] can only be used on structs or enums",
        )),
    }
}

fn add_config_attrs(attrs: &mut Vec<Attribute>) {
    attrs.push(syn::parse_quote! {
        #[derive(
            std::fmt::Debug,
            omnifs_sdk::serde::Deserialize,
        )]
    });
    attrs.push(syn::parse_quote! {
        #[serde(crate = "omnifs_sdk::serde")]
    });
    attrs.push(syn::parse_quote! {
        #[serde(deny_unknown_fields)]
    });
}

fn add_config_attrs_to_struct(item: &mut ItemStruct) {
    add_config_attrs(&mut item.attrs);
}

fn add_config_attrs_to_enum(item: &mut ItemEnum) {
    add_config_attrs(&mut item.attrs);
}
