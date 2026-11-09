//! `#[derive(Input)]`.

use proc_macro2::TokenStream;
use quote::quote;
use syn::{Data, DeriveInput, Fields, LitStr, Type};

use crate::{
    attr::{deprecation_reason, parse_field_attrs},
    util::validate_graphql_name,
};

struct InputField {
    ident: syn::Ident,
    ty: Type,
    graphql_name: LitStr,
    description: Option<LitStr>,
    default: TokenStream,
    missing: TokenStream,
    deprecation: Option<LitStr>,
}

pub fn expand(input: DeriveInput) -> syn::Result<TokenStream> {
    if !input.generics.params.is_empty() {
        return Err(syn::Error::new_spanned(
            &input.generics,
            "`#[derive(Input)]` does not support generic types; an input object's fields must \
             have concrete GraphQL types",
        ));
    }
    let ident = &input.ident;
    let fields = match &input.data {
        Data::Struct(data) => match &data.fields {
            Fields::Named(named) => named,
            Fields::Unnamed(_) => {
                return Err(syn::Error::new_spanned(
                    &data.fields,
                    "`#[derive(Input)]` requires a struct with named fields; a tuple struct \
                     has no field names to expose as GraphQL input fields",
                ));
            }
            Fields::Unit => {
                return Err(syn::Error::new_spanned(
                    &input.ident,
                    "`#[derive(Input)]` cannot be used on a unit struct; use `gurafi::NoArgs` \
                     when a field takes no arguments",
                ));
            }
        },
        Data::Enum(_) => {
            return Err(syn::Error::new_spanned(
                &input.ident,
                "`#[derive(Input)]` can only be used on structs; use `#[derive(Enum)]` for an \
                 enum input type",
            ));
        }
        Data::Union(_) => {
            return Err(syn::Error::new_spanned(
                &input.ident,
                "`#[derive(Input)]` can only be used on structs",
            ));
        }
    };

    let attrs = crate::attr::parse_type_attrs(&input.attrs)?;
    let graphql_type_name = attrs
        .name
        .as_ref()
        .map_or_else(|| ident.to_string(), LitStr::value);
    let type_name_literal = attrs
        .name
        .clone()
        .unwrap_or_else(|| LitStr::new(&graphql_type_name, ident.span()));
    validate_graphql_name(&graphql_type_name, type_name_literal.span())?;

    let mut parsed = Vec::new();
    for field in &fields.named {
        let Some(field_ident) = field.ident.clone() else {
            return Err(syn::Error::new_spanned(field, "expected a named field"));
        };
        let field_attrs = parse_field_attrs(&field.attrs)?;
        let graphql_name = field_attrs
            .name
            .clone()
            .unwrap_or_else(|| LitStr::new(&field_ident.to_string(), field_ident.span()));
        validate_graphql_name(&graphql_name.value(), graphql_name.span())?;
        if field_attrs.default.is_some() && field_attrs.default_with.is_some() {
            return Err(syn::Error::new_spanned(
                &field_ident,
                "an input field can have `default` or `default_with`, not both",
            ));
        }

        let ty = field.ty.clone();
        let field_name = graphql_name.value();

        let default = match (&field_attrs.default, &field_attrs.default_with) {
            (Some(expr), _) => quote! {
                {
                    let value: #ty = #expr;
                    let value = <#ty as ::gurafi::IntoValue>::into_value(value)
                        .map_err(|error| ::gurafi::gql::GraphQLError::new(::std::format!(
                            "Failed to serialize the default value of input field `{}.{}`: {}",
                            #graphql_type_name,
                            #field_name,
                            error.message(),
                        )))?;
                    field = field.default_value(value);
                }
            },
            (_, Some(path)) => quote! {
                {
                    let value: #ty = #path();
                    let value = <#ty as ::gurafi::IntoValue>::into_value(value)
                        .map_err(|error| ::gurafi::gql::GraphQLError::new(::std::format!(
                            "Failed to serialize the default value of input field `{}.{}`: {}",
                            #graphql_type_name,
                            #field_name,
                            error.message(),
                        )))?;
                    field = field.default_value(value);
                }
            },
            (None, None) => quote! {},
        };

        let missing = if field_attrs.default.is_some() || field_attrs.default_with.is_some() {
            let value = match (&field_attrs.default, &field_attrs.default_with) {
                (Some(expr), _) => quote!(#expr),
                (_, Some(path)) => quote!(#path()),
                (None, None) => unreachable!(),
            };
            quote! {
                {
                    let value: #ty = #value;
                    let value = <#ty as ::gurafi::IntoValue>::into_value(value)
                        .map_err(|error| ::gurafi::gql::GraphQLError::new(::std::format!(
                            "Failed to serialize the default value of input field `{}.{}`: {}",
                            #graphql_type_name,
                            #field_name,
                            error.message(),
                        )))?;
                    <#ty as ::gurafi::FromValue>::from_value(&value)
                        .map_err(|error| ::gurafi::gql::GraphQLError::new(::std::format!(
                            "Invalid value for input field `{}.{}`: {}",
                            #graphql_type_name,
                            #field_name,
                            error.message(),
                        )))?
                }
            }
        } else {
            quote! {
                <#ty as ::gurafi::FromValue>::from_value(&::gurafi::gql::Value::Null)
                    .map_err(|error| ::gurafi::gql::GraphQLError::new(::std::format!(
                        "Invalid value for input field `{}.{}`: {}",
                        #graphql_type_name,
                        #field_name,
                        error.message(),
                    )))?
            }
        };

        parsed.push(InputField {
            ident: field_ident,
            ty,
            graphql_name,
            description: field_attrs.description,
            default,
            missing,
            deprecation: deprecation_reason(&field_attrs.deprecated),
        });
    }

    let type_description = match &attrs.description {
        Some(description) => quote!(Some(#description)),
        None => quote!(None),
    };

    let field_docs = parsed.iter().map(|field| {
        let description = match &field.description {
            Some(description) => quote!(field = field.description(#description);),
            None => quote!(),
        };
        let deprecation = match &field.deprecation {
            Some(reason) => quote!(field = field.deprecation_reason(#reason);),
            None => quote!(),
        };
        quote! {
            #description
            #deprecation
        }
    });
    let field_definitions = parsed.iter().zip(field_docs).map(|(field, docs)| {
        let name = &field.graphql_name;
        let ty = &field.ty;
        let default = &field.default;
        quote! {
            {
                let mut field = ::gurafi::gql::InputValue::new(
                    #name,
                    <#ty as ::gurafi::InputType>::input_type(),
                );
                #docs
                #default
                fields.push(field);
            }
        }
    });

    let constructor_fields = parsed.iter().map(|field| {
        let ident = &field.ident;
        let name = &field.graphql_name;
        let ty = &field.ty;
        let missing = &field.missing;
        quote! {
            #ident: match input.get(#name) {
                Some(value) => <#ty as ::gurafi::FromValue>::from_value(value)
                    .map_err(|error| ::gurafi::gql::GraphQLError::new(::std::format!(
                        "Invalid value for input field `{}.{}`: {}",
                        #graphql_type_name,
                        #name,
                        error.message(),
                    )))?,
                None => #missing,
            },
        }
    });

    let nested_registrations: Vec<_> = parsed
        .iter()
        .map(|field| {
            let ty = &field.ty;
            quote! {
                <#ty as ::gurafi::InputType>::register_input(registry);
            }
        })
        .collect();

    Ok(quote! {
        const _: () = {
            #[automatically_derived]
            impl ::gurafi::InputObject for #ident {
                const NAME: ::core::option::Option<&'static str> =
                    ::core::option::Option::Some(#type_name_literal);

                fn description() -> ::core::option::Option<&'static str> {
                    #type_description
                }

                fn fields() -> ::core::result::Result<
                    ::std::vec::Vec<::gurafi::gql::InputValue>,
                    ::gurafi::gql::GraphQLError,
                > {
                    let mut fields = ::std::vec::Vec::new();
                    #(#field_definitions)*
                    ::core::result::Result::Ok(fields)
                }

                fn from_object(
                    input: &::gurafi::gql::Object,
                ) -> ::core::result::Result<
                    Self,
                    ::gurafi::gql::GraphQLError,
                > {
                    ::core::result::Result::Ok(Self {
                        #(#constructor_fields)*
                    })
                }

                fn register_args(registry: &mut ::gurafi::TypeRegistry) {
                    registry.register_input_object::<Self>();
                    #(#nested_registrations)*
                }
            }

            #[automatically_derived]
            impl ::gurafi::InputType for #ident {
                fn input_type() -> ::gurafi::gql::TypeRef {
                    match <Self as ::gurafi::InputObject>::NAME {
                        ::core::option::Option::Some(name) => {
                            ::gurafi::gql::TypeRef::named(name)
                        }
                        ::core::option::Option::None => {
                            ::gurafi::gql::TypeRef::named("__GurafiUnnamedInput")
                        }
                    }
                }

                fn register_input(registry: &mut ::gurafi::TypeRegistry) {
                    registry.register_input_object::<Self>();
                    #(#nested_registrations)*
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
                    match value {
                        ::gurafi::gql::Value::Object(object) => {
                            <Self as ::gurafi::InputObject>::from_object(object)
                        }
                        other => ::core::result::Result::Err(
                            ::gurafi::gql::GraphQLError::new(::std::format!(
                                "Expected an object for input type `{}` but found {}.",
                                #graphql_type_name,
                                other.kind(),
                            )),
                        ),
                    }
                }
            }
        };
    })
}
