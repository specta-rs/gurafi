//! `#[derive(Scalar)]`.

use proc_macro2::TokenStream;
use quote::quote;
use syn::{Data, DeriveInput, Fields, LitStr};

use crate::util::validate_graphql_name;

pub fn expand(input: DeriveInput) -> syn::Result<TokenStream> {
    if !input.generics.params.is_empty() {
        return Err(syn::Error::new_spanned(
            &input.generics,
            "`#[derive(Scalar)]` does not support generic types",
        ));
    }
    let Data::Struct(data) = &input.data else {
        return Err(syn::Error::new_spanned(
            &input.ident,
            "`#[derive(Scalar)]` can only be used on a newtype struct",
        ));
    };
    let Fields::Unnamed(fields) = &data.fields else {
        return Err(syn::Error::new_spanned(
            &data.fields,
            "`#[derive(Scalar)]` requires a newtype struct, for example `struct Date(String)`",
        ));
    };
    if fields.unnamed.len() != 1 {
        return Err(syn::Error::new_spanned(
            &data.fields,
            "`#[derive(Scalar)]` requires exactly one field; a custom scalar wraps one \
             validated value",
        ));
    }
    let inner = &fields.unnamed[0].ty;

    let attrs = crate::attr::parse_type_attrs(&input.attrs)?;
    let ident = &input.ident;
    let graphql_type_name = attrs
        .name
        .as_ref()
        .map_or_else(|| ident.to_string(), LitStr::value);
    let type_name_literal = attrs
        .name
        .clone()
        .unwrap_or_else(|| LitStr::new(&graphql_type_name, ident.span()));
    validate_graphql_name(&graphql_type_name, type_name_literal.span())?;

    let description = match &attrs.description {
        Some(description) => quote!(Some(#description)),
        None => quote!(None),
    };
    let specified_by = match &attrs.specified_by {
        Some(url) => quote!(Some(#url)),
        None => quote!(None),
    };

    Ok(quote! {
        const _: () = {
            #[automatically_derived]
            impl ::gurafi::ScalarValue for #ident {
                const NAME: &'static str = #type_name_literal;

                fn description() -> ::core::option::Option<&'static str> {
                    #description
                }

                fn specified_by() -> ::core::option::Option<&'static str> {
                    #specified_by
                }

                fn to_value(
                    &self,
                ) -> ::core::result::Result<
                    ::gurafi::gql::Value,
                    ::gurafi::gql::GraphQLError,
                > {
                    <#inner as ::gurafi::IntoValue>::into_value(
                        ::core::clone::Clone::clone(&self.0),
                    )
                }

                fn from_value(
                    value: &::gurafi::gql::Value,
                ) -> ::core::result::Result<
                    Self,
                    ::gurafi::gql::GraphQLError,
                > {
                    ::core::result::Result::Ok(Self(
                        <#inner as ::gurafi::FromValue>::from_value(value)?,
                    ))
                }
            }

            #[automatically_derived]
            impl ::gurafi::OutputType for #ident {
                fn output_type() -> ::gurafi::gql::TypeRef {
                    ::gurafi::gql::TypeRef::named(<Self as ::gurafi::ScalarValue>::NAME)
                }

                fn register_output(registry: &mut ::gurafi::TypeRegistry) {
                    registry.register_scalar::<Self>();
                }
            }

            #[automatically_derived]
            impl ::gurafi::InputType for #ident {
                fn input_type() -> ::gurafi::gql::TypeRef {
                    ::gurafi::gql::TypeRef::named(<Self as ::gurafi::ScalarValue>::NAME)
                }

                fn register_input(registry: &mut ::gurafi::TypeRegistry) {
                    registry.register_scalar::<Self>();
                }
            }

            #[automatically_derived]
            impl ::gurafi::IntoValue for #ident {
                fn into_value(
                    self,
                ) -> ::core::result::Result<
                    ::gurafi::gql::Value,
                    ::gurafi::gql::GraphQLError,
                > {
                    <Self as ::gurafi::ScalarValue>::to_value(&self)
                }
            }

            #[automatically_derived]
            impl ::gurafi::FromValue for #ident {
                fn from_value(
                    value: &::gurafi::gql::Value,
                ) -> ::core::result::Result<
                    Self,
                    ::gurafi::gql::GraphQLError,
                > {
                    <Self as ::gurafi::ScalarValue>::from_value(value)
                }
            }
        };
    })
}
