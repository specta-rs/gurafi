//! `#[derive(Object)]`.

use proc_macro2::TokenStream;
use quote::quote;
use syn::{Data, DeriveInput};

use crate::{attr::parse_type_attrs, util::validate_type_name};

pub fn expand(input: DeriveInput) -> syn::Result<TokenStream> {
    if !input.generics.params.is_empty() {
        return Err(syn::Error::new_spanned(
            &input.generics,
            "`#[derive(Object)]` does not support generic types. Register a concrete model, or \
             expose a concrete type that carries the same data.",
        ));
    }
    if !matches!(input.data, Data::Struct(_)) {
        return Err(syn::Error::new_spanned(
            &input.ident,
            "`#[derive(Object)]` can only be used on structs. Model enums with \
             `#[derive(Enum)]` or `#[derive(Union)]`.",
        ));
    }

    let attrs = parse_type_attrs(&input.attrs)?;
    crate::attr::reject_input_only(&attrs, &input.ident)?;
    let ident = &input.ident;
    let name = attrs
        .name
        .as_ref()
        .map_or_else(|| ident.to_string(), syn::LitStr::value);
    let name_literal = attrs
        .name
        .clone()
        .unwrap_or_else(|| syn::LitStr::new(&name, ident.span()));
    validate_type_name(&name, name_literal.span())?;
    if let Some(specified_by) = &attrs.specified_by {
        return Err(syn::Error::new_spanned(
            specified_by,
            "`specified_by` only applies to `#[derive(Scalar)]`",
        ));
    }
    // `parse_type_attrs` merged doc comments into `description`, so this is the final value.
    let description = match &attrs.description {
        Some(description) => quote!(Some(#description)),
        None => quote!(None),
    };

    Ok(quote! {
        const _: () = {
            #[automatically_derived]
            impl ::gurafi::ObjectModel for #ident {
                const NAME: &'static str = #name_literal;

                fn description() -> ::core::option::Option<&'static str> {
                    #description
                }
            }

            #[automatically_derived]
            impl ::gurafi::OutputType for #ident {
                fn output_type() -> ::gurafi::gql::TypeRef {
                    ::gurafi::gql::TypeRef::named(
                        <Self as ::gurafi::ObjectModel>::NAME,
                    )
                }

                fn register_output(registry: &mut ::gurafi::TypeRegistry) {
                    registry.reference_object(<Self as ::gurafi::ObjectModel>::NAME);
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
                    ::core::result::Result::Ok(::gurafi::gql::Value::external(self))
                }
            }
        };
    })
}
