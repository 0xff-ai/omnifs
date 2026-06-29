use proc_macro2::TokenStream as TokenStream2;
use quote::{quote, quote_spanned};
use syn::spanned::Spanned;
use syn::{Attribute, Expr, Field, Fields, GenericArgument, Item, Lit, Meta, PathArguments, Type};

pub(crate) fn config_item_impl(item: Item) -> Result<TokenStream2, syn::Error> {
    match item {
        Item::Struct(mut item_struct) => {
            if !item_struct.generics.params.is_empty() {
                return Err(syn::Error::new(
                    item_struct.generics.span(),
                    "#[omnifs_sdk::config] does not support generic config structs",
                ));
            }

            let fields = config_fields_from(&mut item_struct.fields)?;
            add_config_attrs(&mut item_struct.attrs);
            let ident = &item_struct.ident;
            let metadata = metadata_tokens(&fields);
            let provides = quote! {
                #[cfg(not(target_arch = "wasm32"))]
                impl omnifs_sdk::ProvidesConfigMetadata for #ident {
                    fn metadata() -> Option<omnifs_sdk::ConfigMetadata> {
                        Some(#metadata)
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
        )]
    });
    attrs.push(syn::parse_quote! {
        #[serde(crate = "omnifs_sdk::serde")]
    });
    attrs.push(syn::parse_quote! {
        #[serde(deny_unknown_fields)]
    });
}

fn config_fields_from(fields: &mut Fields) -> syn::Result<Vec<ConfigField>> {
    let Fields::Named(fields) = fields else {
        return Err(syn::Error::new(
            fields.span(),
            "#[omnifs_sdk::config] requires a named-field struct",
        ));
    };
    fields
        .named
        .iter_mut()
        .map(ConfigField::from_field)
        .collect()
}

fn metadata_tokens(fields: &[ConfigField]) -> TokenStream2 {
    let fields = fields.iter().map(ConfigField::metadata_tokens);
    quote! {
        omnifs_sdk::ConfigMetadata {
            fields: ::std::vec![#(#fields),*],
        }
    }
}

struct ConfigField {
    name: String,
    ty: FieldType,
    required: bool,
    default: Option<Expr>,
    description: Option<String>,
}

impl ConfigField {
    fn from_field(field: &mut Field) -> syn::Result<Self> {
        let ident = field.ident.as_ref().ok_or_else(|| {
            syn::Error::new(field.span(), "#[omnifs_sdk::config] requires named fields")
        })?;
        let serde = SerdeAttrs::from_attrs(&field.attrs)?;
        let omnifs = OmnifsAttrs::from_attrs(&field.attrs)?;
        strip_omnifs_attrs(&mut field.attrs);
        let ty = FieldType::from_type(&field.ty)?;
        let required = !serde.default && omnifs.default.is_none();
        Ok(Self {
            name: serde.rename.unwrap_or_else(|| ident.to_string()),
            ty,
            required,
            default: omnifs.default,
            description: doc_description(&field.attrs),
        })
    }

    fn metadata_tokens(&self) -> TokenStream2 {
        let name = &self.name;
        let value_type = self.ty.type_tokens();
        let required = self.required;
        let default = self.ty.default_tokens(self.default.as_ref());
        let description = self.description.as_ref().map_or_else(
            || quote! { None },
            |description| {
                quote! { Some(#description.to_string()) }
            },
        );
        let binding = self.ty.binding_tokens().unwrap_or_else(|| quote! { None });
        quote! {
            omnifs_sdk::ConfigField {
                name: #name.to_string(),
                value_type: #value_type,
                required: #required,
                default: #default,
                description: #description,
                binding: #binding,
            }
        }
    }
}

enum FieldType {
    String(FieldBinding),
    Boolean,
    Integer,
    Array(Box<FieldType>),
    Map(Box<FieldType>),
    Object(Box<Type>),
}

impl FieldType {
    fn from_type(ty: &Type) -> syn::Result<Self> {
        let Type::Path(type_path) = ty else {
            return Err(syn::Error::new(
                ty.span(),
                "unsupported config field type; use String, bool, an integer, Vec<T>, BTreeMap<String, T>, a nested #[config] struct, HostFile, or HostSocket",
            ));
        };
        let segment = type_path.path.segments.last().ok_or_else(|| {
            syn::Error::new(ty.span(), "unsupported empty config field type path")
        })?;
        match segment.ident.to_string().as_str() {
            "String" | "HostFile" => Ok(Self::String(match segment.ident.to_string().as_str() {
                "HostFile" => FieldBinding::HostFile,
                _ => FieldBinding::None,
            })),
            "HostSocket" => Ok(Self::String(FieldBinding::HostSocket)),
            "bool" => Ok(Self::Boolean),
            "u8" | "u16" | "u32" | "u64" | "usize" | "i8" | "i16" | "i32" | "i64" | "isize" => {
                Ok(Self::Integer)
            },
            "Vec" => Ok(Self::Array(Box::new(type_argument(ty, segment, 0)?))),
            "BTreeMap" => {
                ensure_string_map_key(ty, segment)?;
                Ok(Self::Map(Box::new(type_argument(ty, segment, 1)?)))
            },
            _ => Ok(Self::Object(Box::new(ty.clone()))),
        }
    }

    fn type_tokens(&self) -> TokenStream2 {
        match self {
            Self::String(_) => quote! {
                omnifs_sdk::ConfigType::String
            },
            Self::Boolean => quote! {
                omnifs_sdk::ConfigType::Boolean
            },
            Self::Integer => quote! {
                omnifs_sdk::ConfigType::Integer
            },
            Self::Array(items) => {
                let items = items.type_tokens();
                quote! {
                    omnifs_sdk::ConfigType::Array { items: ::std::boxed::Box::new(#items) }
                }
            },
            Self::Map(values) => {
                let values = values.type_tokens();
                quote! {
                    omnifs_sdk::ConfigType::Map { values: ::std::boxed::Box::new(#values) }
                }
            },
            Self::Object(ty) => quote_spanned! {ty.span()=>
                omnifs_sdk::ConfigType::Object {
                    fields: match <#ty as omnifs_sdk::ProvidesConfigMetadata>::metadata() {
                        Some(metadata) => metadata.fields,
                        None => ::std::vec![],
                    },
                }
            },
        }
    }

    fn default_tokens(&self, default: Option<&Expr>) -> TokenStream2 {
        let Some(default) = default else {
            return quote! { None };
        };
        match self {
            Self::String(_) => quote! {
                Some(omnifs_sdk::serde_json::Value::String((#default).to_string()))
            },
            Self::Boolean => quote! {
                Some(omnifs_sdk::serde_json::Value::Bool(#default))
            },
            Self::Integer => quote! {
                Some(omnifs_sdk::serde_json::Value::Number(
                    omnifs_sdk::serde_json::Number::from((#default) as i64),
                ))
            },
            Self::Array(_) | Self::Map(_) | Self::Object(_) => quote_spanned! {default.span()=>
                compile_error!("#[omnifs(default = ...)] supports string, bool, and integer config fields")
            },
        }
    }

    fn binding_tokens(&self) -> Option<TokenStream2> {
        match self {
            Self::String(binding) => binding.metadata_tokens(),
            _ => None,
        }
    }
}

#[derive(Clone, Copy)]
enum FieldBinding {
    None,
    HostFile,
    HostSocket,
}

impl FieldBinding {
    fn metadata_tokens(self) -> Option<TokenStream2> {
        match self {
            Self::None => None,
            Self::HostFile => Some(quote! {
                Some(omnifs_sdk::HostResourceBinding::File { mode: omnifs_sdk::PreopenMode::default() })
            }),
            Self::HostSocket => Some(quote! {
                Some(omnifs_sdk::HostResourceBinding::Socket)
            }),
        }
    }
}

struct SerdeAttrs {
    default: bool,
    rename: Option<String>,
}

impl SerdeAttrs {
    fn from_attrs(attrs: &[Attribute]) -> syn::Result<Self> {
        let mut out = Self {
            default: false,
            rename: None,
        };
        for attr in attrs.iter().filter(|attr| attr.path().is_ident("serde")) {
            attr.parse_nested_meta(|meta| {
                if meta.path.is_ident("default") {
                    out.default = true;
                    if meta.input.peek(syn::Token![=]) {
                        let _ = meta.value()?.parse::<Expr>()?;
                    }
                    Ok(())
                } else if meta.path.is_ident("rename") {
                    out.rename = Some(meta.value()?.parse::<syn::LitStr>()?.value());
                    Ok(())
                } else {
                    Ok(())
                }
            })?;
        }
        Ok(out)
    }
}

struct OmnifsAttrs {
    default: Option<Expr>,
}

impl OmnifsAttrs {
    fn from_attrs(attrs: &[Attribute]) -> syn::Result<Self> {
        let mut out = Self { default: None };
        for attr in attrs.iter().filter(|attr| attr.path().is_ident("omnifs")) {
            attr.parse_nested_meta(|meta| {
                if meta.path.is_ident("default") {
                    out.default = Some(meta.value()?.parse()?);
                    Ok(())
                } else {
                    Err(meta.error("supported omnifs config attribute is `default = ...`"))
                }
            })?;
        }
        Ok(out)
    }
}

fn strip_omnifs_attrs(attrs: &mut Vec<Attribute>) {
    attrs.retain(|attr| !attr.path().is_ident("omnifs"));
}

fn type_argument(ty: &Type, segment: &syn::PathSegment, index: usize) -> syn::Result<FieldType> {
    let PathArguments::AngleBracketed(arguments) = &segment.arguments else {
        return Err(syn::Error::new(
            ty.span(),
            format!("{} config fields require type arguments", segment.ident),
        ));
    };
    let Some(GenericArgument::Type(arg)) = arguments.args.iter().nth(index) else {
        return Err(syn::Error::new(
            arguments.span(),
            format!(
                "{} config field has an unsupported type argument",
                segment.ident
            ),
        ));
    };
    FieldType::from_type(arg)
}

fn ensure_string_map_key(ty: &Type, segment: &syn::PathSegment) -> syn::Result<()> {
    let PathArguments::AngleBracketed(arguments) = &segment.arguments else {
        return Err(syn::Error::new(
            ty.span(),
            format!(
                "{} config fields require key and value type arguments",
                segment.ident
            ),
        ));
    };
    let Some(GenericArgument::Type(Type::Path(key))) = arguments.args.first() else {
        return Err(syn::Error::new(
            arguments.span(),
            "config map keys must be String",
        ));
    };
    let is_string = key
        .path
        .segments
        .last()
        .is_some_and(|segment| segment.ident == "String");
    if is_string {
        Ok(())
    } else {
        Err(syn::Error::new(
            key.span(),
            "config map keys must be String",
        ))
    }
}

fn doc_description(attrs: &[Attribute]) -> Option<String> {
    let lines = attrs
        .iter()
        .filter(|attr| attr.path().is_ident("doc"))
        .filter_map(|attr| match &attr.meta {
            Meta::NameValue(name_value) => match &name_value.value {
                Expr::Lit(expr) => match &expr.lit {
                    Lit::Str(value) => Some(value.value().trim().to_string()),
                    _ => None,
                },
                _ => None,
            },
            _ => None,
        })
        .filter(|line| !line.is_empty())
        .collect::<Vec<_>>();
    (!lines.is_empty()).then(|| lines.join("\n"))
}
