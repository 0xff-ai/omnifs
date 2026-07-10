use proc_macro::TokenStream;
use proc_macro2::TokenStream as TokenStream2;
use quote::quote;
use syn::parse::{Parse, ParseStream};
use syn::punctuated::Punctuated;
use syn::spanned::Spanned;
use syn::{
    Attribute, Fields, Ident, Item, ItemEnum, ItemStruct, LitStr, Path, Token, Type, TypePath,
    Visibility,
};

#[derive(Default)]
struct PathSegmentArgs {
    validate: Option<Path>,
    normalize: Option<Path>,
}

impl Parse for PathSegmentArgs {
    fn parse(input: ParseStream<'_>) -> syn::Result<Self> {
        let mut args = Self::default();
        while !input.is_empty() {
            let key: Ident = input.parse()?;
            input.parse::<Token![=]>()?;
            let value: Path = input.parse()?;
            match key.to_string().as_str() {
                "validate" if args.validate.is_none() => args.validate = Some(value),
                "normalize" if args.normalize.is_none() => args.normalize = Some(value),
                "validate" | "normalize" => {
                    return Err(syn::Error::new(
                        key.span(),
                        "duplicate path_segment argument",
                    ));
                },
                _ => return Err(syn::Error::new(key.span(), "unknown path_segment argument")),
            }
            if input.is_empty() {
                break;
            }
            input.parse::<Token![,]>()?;
        }
        Ok(args)
    }
}

pub(crate) fn path_segment_impl(attr: TokenStream, item: TokenStream) -> syn::Result<TokenStream2> {
    let args = syn::parse::<PathSegmentArgs>(attr)?;
    match syn::parse::<Item>(item)? {
        Item::Enum(item) => enum_impl(&args, item),
        Item::Struct(item) => string_wrapper_impl(args, &item),
        other => Err(syn::Error::new(
            other.span(),
            "#[path_segment] requires an enum or tuple struct",
        )),
    }
}

fn enum_impl(args: &PathSegmentArgs, mut item: ItemEnum) -> syn::Result<TokenStream2> {
    if args.validate.is_some() || args.normalize.is_some() {
        return Err(syn::Error::new(
            item.ident.span(),
            "enum path_segment mode does not accept validate or normalize arguments",
        ));
    }

    let serialize_all = strum_serialize_all(&item.attrs)?;
    let strum_derives = StrumDerives::from_attrs(&item.attrs);
    let preserve_strum_attrs = strum_derives.any_impl() || has_qualified_strum_derive(&item.attrs);

    let mut variants = Vec::new();
    for variant in &item.variants {
        if !matches!(variant.fields, Fields::Unit) {
            return Err(syn::Error::new(
                variant.span(),
                "#[path_segment] enum variants must be fieldless",
            ));
        }
        let serializes = strum_serializes(&variant.attrs)?;
        if serializes.len() > 1 {
            return Err(syn::Error::new(
                variant.span(),
                "#[path_segment] supports one #[strum(serialize = ...)] value per variant",
            ));
        }
        let name = serializes.into_iter().next().map_or_else(
            || segment_name(&variant.ident, serialize_all.as_deref()),
            Ok,
        )?;
        variants.push((
            variant.ident.clone(),
            LitStr::new(&name, variant.ident.span()),
        ));
    }

    if !preserve_strum_attrs {
        strip_strum_attrs(&mut item.attrs);
        for variant in &mut item.variants {
            strip_strum_attrs(&mut variant.attrs);
        }
    }

    let enum_ident = &item.ident;
    let (impl_generics, ty_generics, where_clause) = item.generics.split_for_impl();
    let from_str_impl = (!strum_derives.enum_string).then(|| {
        let match_arms = variants.iter().map(|(ident, name)| {
            quote! { #name => ::core::result::Result::Ok(Self::#ident), }
        });
        quote! {
            impl #impl_generics ::core::str::FromStr for #enum_ident #ty_generics
            #where_clause
            {
                type Err = ();

                fn from_str(segment: &str) -> ::core::result::Result<Self, Self::Err> {
                    match segment {
                        #(#match_arms)*
                        _ => ::core::result::Result::Err(()),
                    }
                }
            }
        }
    });
    let display_impl = (!strum_derives.display).then(|| {
        let match_arms = variants.iter().map(|(ident, name)| {
            quote! { Self::#ident => #name, }
        });
        quote! {
            impl #impl_generics ::core::fmt::Display for #enum_ident #ty_generics
            #where_clause
            {
                fn fmt(&self, f: &mut ::core::fmt::Formatter<'_>) -> ::core::fmt::Result {
                    f.write_str(match self {
                        #(#match_arms)*
                    })
                }
            }
        }
    });
    let as_ref_impl = (!strum_derives.as_ref_str).then(|| {
        let match_arms = variants.iter().map(|(ident, name)| {
            quote! { Self::#ident => #name, }
        });
        quote! {
            impl #impl_generics ::core::convert::AsRef<str> for #enum_ident #ty_generics
            #where_clause
            {
                fn as_ref(&self) -> &str {
                    match self {
                        #(#match_arms)*
                    }
                }
            }
        }
    });
    let choices = variants.iter().map(|(_, name)| name);

    Ok(quote! {
        #item

        #from_str_impl
        #display_impl
        #as_ref_impl

        impl #impl_generics omnifs_sdk::captures::PathSegment for #enum_ident #ty_generics
        #where_clause
        {
            fn choices() -> ::core::option::Option<&'static [&'static str]> {
                ::core::option::Option::Some(&[#(#choices),*])
            }
        }
    })
}

fn string_wrapper_impl(args: PathSegmentArgs, item: &ItemStruct) -> syn::Result<TokenStream2> {
    let validate = args.validate.ok_or_else(|| {
        syn::Error::new(
            item.ident.span(),
            "string-wrapper path_segment mode requires validate = path::to::predicate",
        )
    })?;
    let field = match &item.fields {
        Fields::Unnamed(fields) if fields.unnamed.len() == 1 => &fields.unnamed[0],
        Fields::Unnamed(_) => {
            return Err(syn::Error::new(
                item.fields.span(),
                "#[path_segment] tuple structs must have exactly one field",
            ));
        },
        _ => {
            return Err(syn::Error::new(
                item.fields.span(),
                "#[path_segment] string-wrapper mode requires a tuple struct",
            ));
        },
    };
    if !is_string_type(&field.ty) {
        return Err(syn::Error::new(
            field.ty.span(),
            "#[path_segment] string-wrapper mode requires a String field",
        ));
    }

    let ident = &item.ident;
    let vis = method_vis(&item.vis);
    let normalize_expr = args.normalize.map_or_else(
        || quote! { segment.to_string() },
        |normalize| quote! { #normalize(segment) },
    );
    let (impl_generics, ty_generics, where_clause) = item.generics.split_for_impl();

    Ok(quote! {
        #item

        impl #impl_generics #ident #ty_generics
        #where_clause
        {
            #vis fn as_str(&self) -> &str {
                &self.0
            }
        }

        impl #impl_generics ::core::str::FromStr for #ident #ty_generics
        #where_clause
        {
            type Err = ();

            fn from_str(segment: &str) -> ::core::result::Result<Self, Self::Err> {
                if #validate(segment) {
                    ::core::result::Result::Ok(Self(#normalize_expr))
                } else {
                    ::core::result::Result::Err(())
                }
            }
        }

        impl #impl_generics ::core::fmt::Display for #ident #ty_generics
        #where_clause
        {
            fn fmt(&self, f: &mut ::core::fmt::Formatter<'_>) -> ::core::fmt::Result {
                f.write_str(self.as_str())
            }
        }

        impl #impl_generics ::core::convert::AsRef<str> for #ident #ty_generics
        #where_clause
        {
            fn as_ref(&self) -> &str {
                self.as_str()
            }
        }

        impl #impl_generics omnifs_sdk::captures::PathSegment for #ident #ty_generics
        #where_clause
        {
        }
    })
}

#[derive(Default)]
struct StrumDerives {
    enum_string: bool,
    display: bool,
    as_ref_str: bool,
}

impl StrumDerives {
    fn any_impl(&self) -> bool {
        self.enum_string || self.display || self.as_ref_str
    }

    fn from_attrs(attrs: &[Attribute]) -> Self {
        let mut out = Self::default();
        for attr in attrs.iter().filter(|attr| attr.path().is_ident("derive")) {
            let Ok(paths) = attr.parse_args_with(Punctuated::<Path, Token![,]>::parse_terminated)
            else {
                continue;
            };
            for path in paths {
                let Some(ident) = path.segments.last().map(|segment| &segment.ident) else {
                    continue;
                };
                if ident == "EnumString" {
                    out.enum_string = true;
                } else if ident == "Display" {
                    out.display = true;
                } else if ident == "AsRefStr" {
                    out.as_ref_str = true;
                }
            }
        }
        out
    }
}

fn has_qualified_strum_derive(attrs: &[Attribute]) -> bool {
    attrs
        .iter()
        .filter(|attr| attr.path().is_ident("derive"))
        .filter_map(|attr| {
            attr.parse_args_with(Punctuated::<Path, Token![,]>::parse_terminated)
                .ok()
        })
        .flatten()
        .any(|path| path.segments.iter().any(|segment| segment.ident == "strum"))
}

fn strum_serialize_all(attrs: &[Attribute]) -> syn::Result<Option<String>> {
    let mut out = None;
    for attr in attrs.iter().filter(|attr| attr.path().is_ident("strum")) {
        attr.parse_nested_meta(|meta| {
            if meta.path.is_ident("serialize_all") {
                if out.is_some() {
                    return Err(meta.error("duplicate serialize_all"));
                }
                let value = meta.value()?;
                let lit: LitStr = value.parse()?;
                out = Some(lit.value());
            } else {
                consume_ignored_strum_meta(meta)?;
            }
            Ok(())
        })?;
    }
    Ok(out)
}

fn strum_serializes(attrs: &[Attribute]) -> syn::Result<Vec<String>> {
    let mut out = Vec::new();
    for attr in attrs.iter().filter(|attr| attr.path().is_ident("strum")) {
        attr.parse_nested_meta(|meta| {
            if meta.path.is_ident("serialize") {
                let value = meta.value()?;
                let lit: LitStr = value.parse()?;
                out.push(lit.value());
            } else {
                consume_ignored_strum_meta(meta)?;
            }
            Ok(())
        })?;
    }
    Ok(out)
}

#[allow(clippy::needless_pass_by_value)]
fn consume_ignored_strum_meta(meta: syn::meta::ParseNestedMeta<'_>) -> syn::Result<()> {
    if meta.input.peek(Token![=]) {
        let value = meta.value()?;
        let _: syn::Expr = value.parse()?;
    } else if meta.input.peek(syn::token::Paren) {
        meta.parse_nested_meta(consume_ignored_strum_meta)?;
    }
    Ok(())
}

fn strip_strum_attrs(attrs: &mut Vec<Attribute>) {
    attrs.retain(|attr| !attr.path().is_ident("strum"));
}

fn segment_name(ident: &Ident, serialize_all: Option<&str>) -> syn::Result<String> {
    let name = ident.to_string();
    match serialize_all {
        None => Ok(name),
        Some("snake_case") => Ok(words(&name).join("_")),
        Some(other) => Err(syn::Error::new(
            ident.span(),
            format!("unsupported strum serialize_all value {other:?}"),
        )),
    }
}

fn words(name: &str) -> Vec<String> {
    let mut out = Vec::new();
    let mut current = String::new();
    let mut chars = name.chars().peekable();
    let mut previous: Option<char> = None;
    while let Some(ch) = chars.next() {
        if let Some(prev) = previous
            && ch.is_ascii_uppercase()
        {
            let next = chars.peek().copied();
            if (prev.is_ascii_lowercase()
                || prev.is_ascii_digit()
                || next.is_some_and(|next| next.is_ascii_lowercase()))
                && !current.is_empty()
            {
                out.push(current.to_ascii_lowercase());
                current.clear();
            }
        }
        current.push(ch);
        previous = Some(ch);
    }
    if !current.is_empty() {
        out.push(current.to_ascii_lowercase());
    }
    out
}

fn is_string_type(ty: &Type) -> bool {
    let Type::Path(TypePath { qself: None, path }) = ty else {
        return false;
    };
    let mut segments = path.segments.iter();
    match (
        segments.next(),
        segments.next(),
        segments.next(),
        segments.next(),
    ) {
        (Some(string), None, None, None) => string.ident == "String",
        (Some(root), Some(module), Some(string), None) => {
            (root.ident == "std" || root.ident == "alloc")
                && module.ident == "string"
                && string.ident == "String"
        },
        _ => false,
    }
}

fn method_vis(struct_vis: &Visibility) -> TokenStream2 {
    match struct_vis {
        Visibility::Public(_) | Visibility::Restricted(_) => quote! { #struct_vis },
        Visibility::Inherited => TokenStream2::new(),
    }
}
