use proc_macro2::{Span, TokenStream as TokenStream2};
use quote::{quote, quote_spanned};
use serde_json::to_string;
use syn::spanned::Spanned;
use syn::{Attribute, Expr, Field, Fields, Item, Lit, Meta, Type};

use crate::util::{BytePiece, byte_array_tokens, generic_type_arg, has_angle_args};

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
            let metadata_bytes = metadata_bytes_tokens(&fields)?;
            Ok(quote! {
                #item_struct
                impl omnifs_sdk::ConfigMetadataBytes for #ident {
                    #metadata_bytes
                }
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

enum JsonPiece {
    Static(String),
    Nested(Type),
}

fn metadata_bytes_tokens(fields: &[ConfigField]) -> syn::Result<TokenStream2> {
    let mut pieces = vec![JsonPiece::Static("[".to_string())];
    for (index, field) in fields.iter().enumerate() {
        if index != 0 {
            pieces.push(JsonPiece::Static(",".to_string()));
        }
        pieces.extend(field.json_pieces()?);
    }
    pieces.push(JsonPiece::Static("]".to_string()));

    let byte_pieces = pieces
        .iter()
        .map(|piece| BytePiece {
            length: piece.length_tokens(),
            copy: piece.copy_tokens(),
        })
        .collect::<Vec<_>>();
    let length_terms = byte_pieces.iter().map(|piece| &piece.length);
    let length = quote! { 0usize #(+ #length_terms)* };
    let bytes = byte_array_tokens(&byte_pieces, &length);
    Ok(quote! {
        const LEN: usize = #length;
        const JSON: &'static [u8] = {
            const BYTES: [u8; #length] = #bytes;
            &BYTES
        };
    })
}

impl JsonPiece {
    fn length_tokens(&self) -> TokenStream2 {
        match self {
            Self::Static(value) => {
                let length = syn::LitInt::new(&value.len().to_string(), Span::call_site());
                quote! { #length }
            },
            Self::Nested(ty) => quote_spanned! {ty.span()=>
                <#ty as omnifs_sdk::ConfigMetadataBytes>::LEN
            },
        }
    }

    fn copy_tokens(&self) -> TokenStream2 {
        match self {
            Self::Static(value) => {
                let value = syn::LitStr::new(value, Span::call_site());
                quote! {
                    omnifs_sdk::__internal::copy_bytes(
                        &mut bytes,
                        &mut offset,
                        (#value).as_bytes(),
                    );
                }
            },
            Self::Nested(ty) => quote_spanned! {ty.span()=>
                omnifs_sdk::__internal::copy_bytes(
                    &mut bytes,
                    &mut offset,
                    <#ty as omnifs_sdk::ConfigMetadataBytes>::JSON,
                );
            },
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

    fn json_pieces(&self) -> syn::Result<Vec<JsonPiece>> {
        let mut pieces = vec![JsonPiece::Static(format!(
            r#"{{"name":{},"type":"#,
            to_string(&self.name).expect("config field name serializes")
        ))];
        pieces.extend(self.ty.json_pieces());
        if self.required {
            pieces.push(JsonPiece::Static(r#","required":true"#.to_string()));
        }
        if let Some(default) = self.default.as_ref() {
            pieces.push(JsonPiece::Static(format!(
                r#","default":{}"#,
                self.ty.default_json(default)?
            )));
        }
        if let Some(description) = self.description.as_ref() {
            pieces.push(JsonPiece::Static(format!(
                r#","description":{}"#,
                to_string(description).expect("config description serializes")
            )));
        }
        if let Some(binding) = self.ty.binding_json() {
            pieces.push(JsonPiece::Static(format!(r#","binding":{binding}"#)));
        }
        pieces.push(JsonPiece::Static("}".to_string()));
        Ok(pieces)
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

    fn json_pieces(&self) -> Vec<JsonPiece> {
        match self {
            Self::String(_) => vec![JsonPiece::Static(r#"{"kind":"string"}"#.to_string())],
            Self::Boolean => vec![JsonPiece::Static(r#"{"kind":"boolean"}"#.to_string())],
            Self::Integer => vec![JsonPiece::Static(r#"{"kind":"integer"}"#.to_string())],
            Self::Array(items) => {
                let mut pieces = vec![JsonPiece::Static(r#"{"kind":"array","items":"#.to_string())];
                pieces.extend(items.json_pieces());
                pieces.push(JsonPiece::Static("}".to_string()));
                pieces
            },
            Self::Map(values) => {
                let mut pieces = vec![JsonPiece::Static(r#"{"kind":"map","values":"#.to_string())];
                pieces.extend(values.json_pieces());
                pieces.push(JsonPiece::Static("}".to_string()));
                pieces
            },
            Self::Object(ty) => vec![
                JsonPiece::Static(r#"{"kind":"object","fields":"#.to_string()),
                JsonPiece::Nested((**ty).clone()),
                JsonPiece::Static("}".to_string()),
            ],
        }
    }

    fn default_json(&self, default: &Expr) -> syn::Result<String> {
        match self {
            Self::String(_) => match default {
                Expr::Lit(expr) => match &expr.lit {
                    Lit::Str(value) => Ok(to_string(&value.value()).expect("string serializes")),
                    _ => Err(syn::Error::new(
                        default.span(),
                        "string config defaults must be string literals for compile-time metadata",
                    )),
                },
                _ => Err(syn::Error::new(
                    default.span(),
                    "string config defaults must be string literals for compile-time metadata",
                )),
            },
            Self::Boolean => match default {
                Expr::Lit(expr) => match &expr.lit {
                    Lit::Bool(value) => Ok(value.value.to_string()),
                    _ => Err(syn::Error::new(
                        default.span(),
                        "boolean config defaults must be boolean literals for compile-time metadata",
                    )),
                },
                _ => Err(syn::Error::new(
                    default.span(),
                    "boolean config defaults must be boolean literals for compile-time metadata",
                )),
            },
            Self::Integer => integer_default_json(default),
            Self::Array(_) | Self::Map(_) | Self::Object(_) => Err(syn::Error::new(
                default.span(),
                "#[omnifs(default = ...)] supports string, bool, and integer config fields",
            )),
        }
    }

    fn binding_json(&self) -> Option<&'static str> {
        match self {
            Self::String(FieldBinding::HostFile) => Some(r#"{"kind":"file","mode":"ro"}"#),
            Self::String(FieldBinding::HostSocket) => Some(r#"{"kind":"socket"}"#),
            _ => None,
        }
    }
}

fn integer_default_json(default: &Expr) -> syn::Result<String> {
    match default {
        Expr::Lit(expr) => match &expr.lit {
            Lit::Int(value) => Ok(value.base10_digits().to_string()),
            _ => Err(syn::Error::new(
                default.span(),
                "integer config defaults must be integer literals for compile-time metadata",
            )),
        },
        Expr::Unary(expr) if matches!(expr.op, syn::UnOp::Neg(_)) => {
            let Expr::Lit(inner) = expr.expr.as_ref() else {
                return Err(syn::Error::new(
                    default.span(),
                    "integer config defaults must be integer literals for compile-time metadata",
                ));
            };
            let Lit::Int(value) = &inner.lit else {
                return Err(syn::Error::new(
                    default.span(),
                    "integer config defaults must be integer literals for compile-time metadata",
                ));
            };
            Ok(format!("-{}", value.base10_digits()))
        },
        _ => Err(syn::Error::new(
            default.span(),
            "integer config defaults must be integer literals for compile-time metadata",
        )),
    }
}

#[derive(Clone, Copy)]
enum FieldBinding {
    None,
    HostFile,
    HostSocket,
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
    if !has_angle_args(segment) {
        return Err(syn::Error::new(
            ty.span(),
            format!("{} config fields require type arguments", segment.ident),
        ));
    }
    let Some(arg) = generic_type_arg(segment, index) else {
        return Err(syn::Error::new(
            segment.arguments.span(),
            format!(
                "{} config field has an unsupported type argument",
                segment.ident
            ),
        ));
    };
    FieldType::from_type(arg)
}

fn ensure_string_map_key(ty: &Type, segment: &syn::PathSegment) -> syn::Result<()> {
    if !has_angle_args(segment) {
        return Err(syn::Error::new(
            ty.span(),
            format!(
                "{} config fields require key and value type arguments",
                segment.ident
            ),
        ));
    }
    let Some(Type::Path(key)) = generic_type_arg(segment, 0) else {
        return Err(syn::Error::new(
            segment.arguments.span(),
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
