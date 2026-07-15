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

#[cfg(test)]
mod tests {
    use super::*;
    use syn::parse_quote;

    #[test]
    fn generic_type_arg_returns_only_type_arguments() {
        let ty: syn::Type = parse_quote!(Router<State>);
        let syn::Type::Path(path) = ty else {
            panic!("expected type path");
        };
        let segment = path.path.segments.last().unwrap();

        assert!(has_angle_args(segment));
        assert!(matches!(
            generic_type_arg(segment, 0),
            Some(syn::Type::Path(_))
        ));
        assert!(generic_type_arg(segment, 1).is_none());
    }
}
