//! `#[derive(Union)]`.

use std::collections::HashSet;

use proc_macro2::TokenStream;
use quote::{ToTokens, quote};
use syn::{Data, DeriveInput, Fields, LitStr};

use crate::util::validate_type_name;

pub fn expand(input: DeriveInput) -> syn::Result<TokenStream> {
    if !input.generics.params.is_empty() {
        return Err(syn::Error::new_spanned(
            &input.generics,
            "`#[derive(Union)]` does not support generic types",
        ));
    }
    let Data::Enum(data) = &input.data else {
        return Err(syn::Error::new_spanned(
            &input.ident,
            "`#[derive(Union)]` can only be used on enums, one unit-like variant per member",
        ));
    };
    if data.variants.is_empty() {
        return Err(syn::Error::new_spanned(
            &input.ident,
            "a GraphQL union must have at least one member",
        ));
    }

    let attrs = crate::attr::parse_type_attrs(&input.attrs)?;
    crate::attr::reject_input_only(&attrs, &input.ident)?;
    let ident = &input.ident;
    let graphql_type_name = attrs
        .name
        .as_ref()
        .map_or_else(|| ident.to_string(), LitStr::value);
    let type_name_literal = attrs
        .name
        .clone()
        .unwrap_or_else(|| LitStr::new(&graphql_type_name, ident.span()));
    validate_type_name(&graphql_type_name, type_name_literal.span())?;

    let mut seen = HashSet::new();
    let mut variants = Vec::new();
    for variant in &data.variants {
        let Fields::Unnamed(fields) = &variant.fields else {
            return Err(syn::Error::new_spanned(
                &variant.fields,
                "every `#[derive(Union)]` variant must hold exactly one object model, for \
                 example `User(User)`",
            ));
        };
        if fields.unnamed.len() != 1 {
            return Err(syn::Error::new_spanned(
                &variant.fields,
                "every `#[derive(Union)]` variant must hold exactly one object model",
            ));
        }
        let member = &fields.unnamed[0].ty;
        let key = member.to_token_stream().to_string();
        if !seen.insert(key) {
            return Err(syn::Error::new_spanned(
                member,
                "the same object type appears in more than one union variant",
            ));
        }
        variants.push((variant.ident.clone(), member.clone()));
    }

    let description = match &attrs.description {
        Some(description) => quote!(Some(#description)),
        None => quote!(None),
    };

    let into_value_arms = variants.iter().map(
        |(variant, _)| quote!(Self::#variant(value) => ::gurafi::IntoValue::into_value(value)),
    );
    let member_registrations = variants
        .iter()
        .map(|(_, member)| quote!(<#member as ::gurafi::OutputType>::register_output(registry);));

    Ok(quote! {
        const _: () = {
            #[automatically_derived]
            impl ::gurafi::UnionModel for #ident {
                const NAME: &'static str = #type_name_literal;

                fn description() -> ::core::option::Option<&'static str> {
                    #description
                }
            }

            #[automatically_derived]
            impl ::gurafi::OutputType for #ident {
                fn output_type() -> ::gurafi::gql::TypeRef {
                    ::gurafi::gql::TypeRef::named(<Self as ::gurafi::UnionModel>::NAME)
                }

                fn register_output(registry: &mut ::gurafi::TypeRegistry) {
                    registry.reference_union(<Self as ::gurafi::UnionModel>::NAME);
                    #(#member_registrations)*
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
                    match self {
                        #(#into_value_arms),*
                    }
                }
            }
        };
    })
}
