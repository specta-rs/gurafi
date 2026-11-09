//! `#[derive(Enum)]`.

use std::collections::HashSet;

use proc_macro2::TokenStream;
use quote::quote;
use syn::{Data, DeriveInput, Fields, LitStr};

use crate::{
    attr::{deprecation_reason, parse_field_attrs},
    util::validate_graphql_name,
};

pub fn expand(input: DeriveInput) -> syn::Result<TokenStream> {
    if !input.generics.params.is_empty() {
        return Err(syn::Error::new_spanned(
            &input.generics,
            "`#[derive(Enum)]` does not support generic types",
        ));
    }
    let Data::Enum(data) = &input.data else {
        return Err(syn::Error::new_spanned(
            &input.ident,
            "`#[derive(Enum)]` can only be used on enums",
        ));
    };
    if data.variants.is_empty() {
        return Err(syn::Error::new_spanned(
            &input.ident,
            "a GraphQL enum must define at least one value",
        ));
    }

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

    let mut seen = HashSet::new();
    let mut variants = Vec::new();
    for variant in &data.variants {
        if !matches!(variant.fields, Fields::Unit) {
            return Err(syn::Error::new_spanned(
                &variant.fields,
                "`#[derive(Enum)]` requires unit variants. GraphQL enum values carry no data; \
                 move the data into an object type and expose it as a field.",
            ));
        }
        let variant_attrs = parse_field_attrs(&variant.attrs)?;
        if variant_attrs.default.is_some() || variant_attrs.default_with.is_some() {
            return Err(syn::Error::new_spanned(
                &variant.ident,
                "`default` does not apply to enum variants",
            ));
        }
        let graphql_name = variant_attrs
            .name
            .clone()
            .unwrap_or_else(|| LitStr::new(&variant.ident.to_string(), variant.ident.span()));
        validate_graphql_name(&graphql_name.value(), graphql_name.span())?;
        if !seen.insert(graphql_name.value()) {
            return Err(syn::Error::new_spanned(
                &graphql_name,
                format!(
                    "duplicate GraphQL enum value `{}`; every variant needs a distinct name",
                    graphql_name.value()
                ),
            ));
        }
        variants.push((
            variant.ident.clone(),
            graphql_name,
            variant_attrs.description,
            deprecation_reason(&variant_attrs.deprecated),
        ));
    }

    let type_description = match &attrs.description {
        Some(description) => quote!(Some(#description)),
        None => quote!(None),
    };

    let values = variants.iter().map(|(_, name, description, deprecation)| {
        let description = match description {
            Some(description) => {
                quote!(::core::option::Option::Some(::std::sync::Arc::from(#description)))
            }
            None => quote!(::core::option::Option::None),
        };
        let deprecation = match deprecation {
            Some(reason) => {
                quote!(::core::option::Option::Some(::std::sync::Arc::from(#reason)))
            }
            None => quote!(::core::option::Option::None),
        };
        quote! {
            ::gurafi::gql::EnumValue {
                name: ::gurafi::gql::Name::from(#name),
                description: #description,
                deprecation_reason: #deprecation,
            }
        }
    });

    let value_names = variants
        .iter()
        .map(|(variant, name, _, _)| quote!(Self::#variant => #name));
    let from_names = variants
        .iter()
        .map(|(variant, name, _, _)| quote!(#name => Some(Self::#variant)));

    Ok(quote! {
        const _: () = {
            #[automatically_derived]
            impl ::gurafi::EnumModel for #ident {
                const NAME: &'static str = #type_name_literal;

                fn description() -> ::core::option::Option<&'static str> {
                    #type_description
                }

                fn values() -> ::std::vec::Vec<::gurafi::gql::EnumValue> {
                    ::std::vec![#(#values),*]
                }

                fn value_name(&self) -> &'static str {
                    match self {
                        #(#value_names),*
                    }
                }

                fn from_name(name: &str) -> ::core::option::Option<Self> {
                    match name {
                        #(#from_names,)*
                        _ => ::core::option::Option::None,
                    }
                }
            }

            #[automatically_derived]
            impl ::gurafi::OutputType for #ident {
                fn output_type() -> ::gurafi::gql::TypeRef {
                    ::gurafi::gql::TypeRef::named(<Self as ::gurafi::EnumModel>::NAME)
                }

                fn register_output(registry: &mut ::gurafi::TypeRegistry) {
                    registry.register_enum::<Self>();
                }
            }

            #[automatically_derived]
            impl ::gurafi::InputType for #ident {
                fn input_type() -> ::gurafi::gql::TypeRef {
                    ::gurafi::gql::TypeRef::named(<Self as ::gurafi::EnumModel>::NAME)
                }

                fn register_input(registry: &mut ::gurafi::TypeRegistry) {
                    registry.register_enum::<Self>();
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
                    ::core::result::Result::Ok(::gurafi::gql::Value::Enum(
                        ::std::sync::Arc::from(<Self as ::gurafi::EnumModel>::value_name(&self)),
                    ))
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
                    match value.as_str() {
                        ::core::option::Option::Some(name) => {
                            match <Self as ::gurafi::EnumModel>::from_name(name) {
                                ::core::option::Option::Some(value) => {
                                    ::core::result::Result::Ok(value)
                                }
                                ::core::option::Option::None => {
                                    ::core::result::Result::Err(
                                        ::gurafi::gql::GraphQLError::new(::std::format!(
                                            "`{}` is not a value of enum `{}`.",
                                            name,
                                            <Self as ::gurafi::EnumModel>::NAME,
                                        )),
                                    )
                                }
                            }
                        }
                        ::core::option::Option::None => {
                            ::core::result::Result::Err(
                                ::gurafi::gql::GraphQLError::new(::std::format!(
                                    "Expected a value of enum `{}` but found {}.",
                                    <Self as ::gurafi::EnumModel>::NAME,
                                    value.kind(),
                                )),
                            )
                        }
                    }
                }
            }
        };
    })
}
