//! `#[derive(ErrorUnion)]`.

use std::collections::HashSet;

use proc_macro2::TokenStream;
use quote::{ToTokens, quote};
use syn::{Data, DeriveInput, Fields};

pub fn expand(input: DeriveInput) -> syn::Result<TokenStream> {
    if !input.generics.params.is_empty() {
        return Err(syn::Error::new_spanned(
            &input.generics,
            "`#[derive(ErrorUnion)]` does not support generic types. Use a concrete error enum \
             and, if needed, make the generated error payload a concrete object model.",
        ));
    }
    let Data::Enum(data) = &input.data else {
        return Err(syn::Error::new_spanned(
            &input.ident,
            "`#[derive(ErrorUnion)]` can only be used on an enum whose variants each hold one \
             error payload object.",
        ));
    };
    if data.variants.is_empty() {
        return Err(syn::Error::new_spanned(
            &input.ident,
            "an error union must have at least one variant, so the result union has an error \
             member besides its success payload",
        ));
    }

    let ident = &input.ident;
    let mut seen = HashSet::new();
    let mut variants = Vec::new();
    for variant in &data.variants {
        let payload = match &variant.fields {
            Fields::Unnamed(fields) if fields.unnamed.len() == 1 => &fields.unnamed[0].ty,
            Fields::Unnamed(_) => {
                return Err(syn::Error::new_spanned(
                    &variant.fields,
                    "every `#[derive(ErrorUnion)]` variant must hold exactly one error payload \
                     object, for example `NotFound(NotFoundError)`; tuple variants with zero or \
                     several fields are not supported",
                ));
            }
            Fields::Named(_) => {
                return Err(syn::Error::new_spanned(
                    &variant.fields,
                    "`#[derive(ErrorUnion)]` does not support struct variants. Use a newtype \
                     variant holding one error payload object, for example \
                     `NotFound(NotFoundError)`",
                ));
            }
            Fields::Unit => {
                return Err(syn::Error::new_spanned(
                    &variant.fields,
                    "every `#[derive(ErrorUnion)]` variant must hold an error payload object; \
                     unit variants have no GraphQL object type to use as a union member",
                ));
            }
        };
        let key = payload.to_token_stream().to_string();
        if !seen.insert(key) {
            return Err(syn::Error::new_spanned(
                payload,
                "the same error payload type appears in more than one variant; a GraphQL union \
                 cannot include the same object type twice",
            ));
        }
        variants.push((variant.ident.clone(), payload.clone()));
    }

    let members = variants
        .iter()
        .map(|(_, payload)| quote!(<#payload as ::gurafi::ObjectModel>::NAME));
    let converters = variants.iter().map(
        |(variant, _)| quote!(Self::#variant(value) => ::gurafi::IntoValue::into_value(value)),
    );

    Ok(quote! {
        const _: () = {
            #[automatically_derived]
            impl ::gurafi::ErrorUnionModel for #ident {
                fn members() -> &'static [&'static str] {
                    &[#(#members),*]
                }

                fn into_value(
                    self,
                ) -> ::core::result::Result<
                    ::gurafi::gql::Value,
                    ::gurafi::gql::GraphQLError,
                > {
                    match self {
                        #(#converters),*
                    }
                }
            }
        };
    })
}

#[cfg(test)]
mod tests {
    use syn::parse_quote;

    use super::expand;

    fn message(input: syn::DeriveInput) -> String {
        expand(input)
            .expect_err("macro should reject the input")
            .to_string()
    }

    #[test]
    fn generic_enums_are_rejected() {
        let message = message(parse_quote! {
            enum CreateUserError<T> {
                NotFound(T),
            }
        });
        assert!(
            message.contains("does not support generic types"),
            "{message}"
        );
    }

    #[test]
    fn structs_are_rejected() {
        let message = message(parse_quote! {
            struct CreateUserError;
        });
        assert!(message.contains("only be used on an enum"), "{message}");
    }

    #[test]
    fn unit_variants_are_rejected() {
        let message = message(parse_quote! {
            enum CreateUserError {
                NotFound,
            }
        });
        assert!(message.contains("unit variants"), "{message}");
    }

    #[test]
    fn struct_variants_are_rejected() {
        let message = message(parse_quote! {
            enum CreateUserError {
                NotFound { message: String },
            }
        });
        assert!(message.contains("struct variants"), "{message}");
    }

    #[test]
    fn tuple_variants_must_hold_one_field() {
        let message = message(parse_quote! {
            enum CreateUserError {
                NotFound(String, String),
            }
        });
        assert!(message.contains("exactly one error payload"), "{message}");
    }

    #[test]
    fn duplicate_payloads_are_rejected() {
        let message = message(parse_quote! {
            enum CreateUserError {
                NotFound(NotFoundError),
                AlsoNotFound(NotFoundError),
            }
        });
        assert!(message.contains("same error payload type"), "{message}");
    }

    #[test]
    fn an_empty_enum_is_rejected() {
        let message = message(parse_quote! {
            enum CreateUserError {}
        });
        assert!(message.contains("at least one variant"), "{message}");
    }
}
