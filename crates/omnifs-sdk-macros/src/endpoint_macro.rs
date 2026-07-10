use proc_macro2::TokenStream as TokenStream2;
use quote::quote;
use syn::punctuated::Punctuated;
use syn::spanned::Spanned;
use syn::{Attribute, Meta, Token};

pub(crate) fn endpoint_derive_impl(input: &syn::DeriveInput) -> syn::Result<TokenStream2> {
    EndpointArgs::from_attrs(&input.attrs)?.expand(input)
}

#[derive(Default)]
struct EndpointArgs {
    base: Option<String>,
    default_headers: Vec<(String, String)>,
    rate_limit: Option<RateLimitArg>,
    hooks: bool,
}

enum RateLimitArg {
    Off,
    Cooldown(u64),
}

impl RateLimitArg {
    fn tokens(&self) -> TokenStream2 {
        match self {
            Self::Off => quote! { omnifs_sdk::endpoint::RateLimitPolicy::Off },
            Self::Cooldown(seconds) => quote! {
                omnifs_sdk::endpoint::RateLimitPolicy::Cooldown(
                    core::time::Duration::from_secs(#seconds)
                )
            },
        }
    }
}

impl EndpointArgs {
    fn from_attrs(attrs: &[Attribute]) -> syn::Result<Self> {
        let mut args = Self::default();

        // One or more `#[endpoint(..)]` attributes, each a comma-separated
        // list of `key = "value"` pairs plus the bare `hooks` flag.
        for attr in attrs.iter().filter(|a| a.path().is_ident("endpoint")) {
            let metas = attr.parse_args_with(Punctuated::<Meta, Token![,]>::parse_terminated)?;
            for meta in metas {
                args.parse_meta(meta)?;
            }
        }

        Ok(args)
    }

    fn parse_meta(&mut self, meta: Meta) -> syn::Result<()> {
        // `hooks` is a bare flag: the provider supplies its own
        // `impl EndpointHooks`, so the derive must not emit the default.
        if let Meta::Path(path) = &meta {
            if path.is_ident("hooks") {
                if self.hooks {
                    return Err(syn::Error::new(
                        path.span(),
                        "`hooks` may only be specified once",
                    ));
                }
                self.hooks = true;
                return Ok(());
            }
            return Err(syn::Error::new(
                path.span(),
                "unknown #[endpoint(..)] flag; expected `hooks`",
            ));
        }

        let mnv = match meta {
            Meta::NameValue(mnv) => mnv,
            other => {
                return Err(syn::Error::new(
                    other.span(),
                    "expected `key = \"value\"` or the `hooks` flag",
                ));
            },
        };
        let key = mnv
            .path
            .get_ident()
            .ok_or_else(|| syn::Error::new(mnv.path.span(), "expected a simple key"))?
            .to_string();
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
                if self.base.is_some() {
                    return Err(syn::Error::new(
                        mnv.path.span(),
                        "`base` may only be specified once",
                    ));
                }
                self.base = Some(value_str);
            },
            "default_header" => {
                // Split on the first occurrence of ": " to separate header
                // name from value.
                let sep = value_str.find(": ").ok_or_else(|| {
                    syn::Error::new(mnv.value.span(), "default_header must be \"Name: Value\"")
                })?;
                let name = value_str[..sep].to_string();
                let value = value_str[sep + 2..].to_string();
                self.default_headers.push((name, value));
            },
            "auth" => {
                return Err(syn::Error::new(
                    mnv.path.span(),
                    "auth is declared via `auth = ..` on `#[omnifs_sdk::provider]`, not #[endpoint]",
                ));
            },
            "rate_limit" => {
                if self.rate_limit.is_some() {
                    return Err(syn::Error::new(
                        mnv.path.span(),
                        "`rate_limit` may only be specified once",
                    ));
                }
                self.rate_limit = Some(if value_str == "off" {
                    RateLimitArg::Off
                } else {
                    RateLimitArg::Cooldown(value_str.parse::<u64>().map_err(|_| {
                        syn::Error::new(
                            mnv.value.span(),
                            "rate_limit must be \"off\" or a whole number of seconds",
                        )
                    })?)
                });
            },
            _ => {
                return Err(syn::Error::new(
                    mnv.path.span(),
                    "unknown #[endpoint(..)] key; expected base, default_header, rate_limit, or hooks",
                ));
            },
        }

        Ok(())
    }

    fn expand(&self, input: &syn::DeriveInput) -> syn::Result<TokenStream2> {
        let base_url = self.base.as_deref().ok_or_else(|| {
            syn::Error::new(
                proc_macro2::Span::call_site(),
                "#[endpoint(base = \"...\")] is required",
            )
        })?;

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

        let headers_tokens = self.default_headers.iter().map(|(name, value)| {
            quote! { (#name, #value) }
        });
        let rate_limit_method = self.rate_limit.as_ref().map(|policy| {
            let policy = policy.tokens();
            quote! {
                fn rate_limit(&self) -> omnifs_sdk::endpoint::RateLimitPolicy {
                    #policy
                }
            }
        });
        // The easy case gets the default hooks; `hooks` lets the provider write
        // its own `impl EndpointHooks` without a coherence clash.
        let hooks_impl = if self.hooks {
            quote! {}
        } else {
            quote! {
                impl #impl_generics omnifs_sdk::endpoint::EndpointHooks for #type_name #ty_generics #where_clause {}
            }
        };

        Ok(quote! {
            const _: #type_name = #type_name;

            impl #impl_generics omnifs_sdk::endpoint::Endpoint for #type_name #ty_generics #where_clause {
                fn base(&self) -> &str {
                    #base_url
                }
                fn default_headers(&self) -> &[(&str, &str)] {
                    &[ #(#headers_tokens),* ]
                }
                #rate_limit_method
            }

            #hooks_impl
        })
    }
}
