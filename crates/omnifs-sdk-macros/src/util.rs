use proc_macro2::TokenStream as TokenStream2;
use quote::quote;
use syn::{GenericArgument, PathArguments, Type};

pub(crate) struct BytePiece {
    pub length: TokenStream2,
    pub copy: TokenStream2,
}

pub(crate) fn byte_array_tokens(pieces: &[BytePiece], length: &TokenStream2) -> TokenStream2 {
    let copies = pieces.iter().map(|piece| &piece.copy);
    quote! {
        {
            let mut bytes = [0u8; #length];
            let mut offset = 0usize;
            #(#copies)*
            bytes
        }
    }
}

pub(crate) fn generic_type_arg(segment: &syn::PathSegment, index: usize) -> Option<&Type> {
    let PathArguments::AngleBracketed(args) = &segment.arguments else {
        return None;
    };
    let Some(GenericArgument::Type(arg)) = args.args.iter().nth(index) else {
        return None;
    };
    Some(arg)
}

pub(crate) fn has_angle_args(segment: &syn::PathSegment) -> bool {
    matches!(segment.arguments, PathArguments::AngleBracketed(_))
}
