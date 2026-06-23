use proc_macro2::TokenStream as TokenStream2;
use quote::quote;
use syn::spanned::Spanned;

pub(crate) fn endpoint_derive_impl(input: &syn::DeriveInput) -> syn::Result<TokenStream2> {
    let type_name = &input.ident;
    if !input.generics.params.is_empty() {
        return Err(syn::Error::new(
            input.generics.span(),
            "Endpoint can only be derived for a non-generic unit struct",
        ));
    }
    match &input.data {
        syn::Data::Struct(data) if matches!(data.fields, syn::Fields::Unit) => {},
        _ => {
            return Err(syn::Error::new(
                input.span(),
                "Endpoint can only be derived for a unit struct",
            ));
        },
    }
    let (impl_generics, ty_generics, where_clause) = input.generics.split_for_impl();

    let mut base: Option<String> = None;
    let mut base_span = proc_macro2::Span::call_site();
    let mut default_headers: Vec<(String, String)> = Vec::new();
    let mut rate_limit_policy: Option<TokenStream2> = None;

    for attr in input.attrs.iter().filter(|a| a.path().is_ident("endpoint")) {
        let mnv = attr.parse_args::<syn::MetaNameValue>()?;

        let key = mnv
            .path
            .get_ident()
            .ok_or_else(|| syn::Error::new(mnv.path.span(), "expected a simple key"))?
            .to_string();

        // Extract the string-literal value; reject non-literal expressions immediately.
        let value_str = match &mnv.value {
            syn::Expr::Lit(syn::ExprLit {
                lit: syn::Lit::Str(s),
                ..
            }) => s.value(),
            other => {
                return Err(syn::Error::new(
                    other.span(),
                    "#[endpoint(..)] value must be a string literal",
                ));
            },
        };

        match key.as_str() {
            "base" => {
                if base.is_some() {
                    return Err(syn::Error::new(
                        mnv.path.span(),
                        "`base` may only be specified once",
                    ));
                }
                base = Some(value_str);
                base_span = mnv.path.span();
            },
            "default_header" => {
                // Split on the first occurrence of ": " to separate header name from value.
                let sep = value_str.find(": ").ok_or_else(|| {
                    syn::Error::new(mnv.value.span(), "default_header must be \"Name: Value\"")
                })?;
                let name = value_str[..sep].to_string();
                let value = value_str[sep + 2..].to_string();
                default_headers.push((name, value));
            },
            "auth" => {
                return Err(syn::Error::new(
                    mnv.path.span(),
                    "auth is declared in omnifs.provider.json, not #[endpoint]",
                ));
            },
            "rate_limit" => {
                if rate_limit_policy.is_some() {
                    return Err(syn::Error::new(
                        mnv.path.span(),
                        "`rate_limit` may only be specified once",
                    ));
                }
                rate_limit_policy = Some(if value_str == "off" {
                    quote! { omnifs_sdk::endpoint::RateLimitPolicy::Off }
                } else {
                    let seconds = value_str.parse::<u64>().map_err(|_| {
                        syn::Error::new(
                            mnv.value.span(),
                            "rate_limit must be \"off\" or a whole number of seconds",
                        )
                    })?;
                    quote! {
                        omnifs_sdk::endpoint::RateLimitPolicy::Cooldown(
                            core::time::Duration::from_secs(#seconds)
                        )
                    }
                });
            },
            _ => {
                return Err(syn::Error::new(
                    mnv.path.span(),
                    "unknown #[endpoint(..)] key; expected base, default_header, or rate_limit",
                ));
            },
        }
    }

    let base_url =
        base.ok_or_else(|| syn::Error::new(base_span, "#[endpoint(base = \"...\")] is required"))?;

    let headers_tokens = default_headers.iter().map(|(name, value)| {
        quote! { (#name, #value) }
    });
    let rate_limit_policy_method = rate_limit_policy.map(|policy| {
        quote! {
            fn rate_limit_policy() -> omnifs_sdk::endpoint::RateLimitPolicy {
                #policy
            }
        }
    });

    Ok(quote! {
        const _: #type_name = #type_name;

        impl #impl_generics omnifs_sdk::endpoint::Endpoint for #type_name #ty_generics #where_clause {
            fn base() -> &'static str {
                #base_url
            }
            fn default_headers() -> &'static [(&'static str, &'static str)] {
                &[ #(#headers_tokens),* ]
            }
            #rate_limit_policy_method
        }
    })
}
