//! Procedural macros for `gurafi`.
//!
//! These macros let Rust types drive the GraphQL type system:
//!
//! - [`macro@Object`] registers a Rust struct as a GraphQL object type.
//! - [`macro@Input`] registers a Rust struct as an input object (and as a field's arguments).
//! - [`macro@Enum`] registers a Rust enum as a GraphQL enum.
//! - [`macro@Scalar`] registers a newtype as a custom scalar delegating to its inner type.
//! - [`macro@Union`] registers a Rust enum as a GraphQL union of object types.
//!
//! Every macro emits all of the trait implementations `gurafi` needs, so a derive is the
//! whole registration: there is no second list of fields to keep in sync. Unsupported shapes
//! (generic types, tuple structs for [`macro@Input`], enums with data for [`macro@Enum`])
//! produce a `compile_error!` with an explanation instead of a confusing trait bound failure.

#![forbid(unsafe_code)]
#![warn(missing_docs)]

mod attr;
mod enums;
mod input;
mod object;
mod scalar;
mod union;
mod util;

use proc_macro::TokenStream;
use syn::{DeriveInput, parse_macro_input};

/// Derive a GraphQL object type.
///
/// The Rust type name becomes the GraphQL type name; override it with
/// `#[graphql(name = "...")]`. A doc comment on the type becomes its description, and
/// `#[graphql(description = "...")]` overrides the comment.
///
/// Fields are *not* derived. An object's GraphQL fields are declared explicitly on the
/// schema builder with `field`, `field_result`, `field_async`, `field_result_async` or
/// `expose`, so every field's resolver and access rules live in one place.
///
/// ```ignore
/// use gurafi::Object;
///
/// #[derive(Object)]
/// #[graphql(description = "A long-necked animal.")]
/// struct Giraffe {
///     name: String,
/// }
/// ```
#[proc_macro_derive(Object, attributes(graphql))]
pub fn derive_object(input: TokenStream) -> TokenStream {
    let input = parse_macro_input!(input as DeriveInput);
    object::expand(input)
        .unwrap_or_else(syn::Error::into_compile_error)
        .into()
}

/// Derive a GraphQL input object.
///
/// The type can be used as a field's arguments (`A: InputObject`) or nested inside another
/// input type. Attribute support:
///
/// - `#[graphql(name = "...")]` renames the type or one of its fields.
/// - `#[graphql(description = "...")]` (or a doc comment) sets a description.
/// - `#[graphql(default = expr)]` sets a default value, typed as the field's Rust type.
/// - `#[graphql(default_with = "path")]` computes the default by calling `path()`.
/// - `#[graphql(deprecated)]` / `#[graphql(deprecated = "reason")]` deprecates a field.
///
/// `Option<T>` fields are optional; every other field without a default is required.
///
/// ```ignore
/// use gurafi::Input;
///
/// #[derive(Input)]
/// struct FriendsArgs {
///     #[graphql(description = "Max results", default = 10)]
///     limit: i32,
///     after: Option<String>,
/// }
/// ```
#[proc_macro_derive(Input, attributes(graphql))]
pub fn derive_input(input: TokenStream) -> TokenStream {
    let input = parse_macro_input!(input as DeriveInput);
    input::expand(input)
        .unwrap_or_else(syn::Error::into_compile_error)
        .into()
}

/// Derive a GraphQL enum.
///
/// GraphQL enum values are usually `SCREAMING_CASE`; the Rust variant name is used verbatim
/// unless `#[graphql(name = "...")]` overrides it. Variants must be unit variants.
///
/// ```ignore
/// use gurafi::Enum;
///
/// #[derive(Enum)]
/// enum Color {
///     #[graphql(name = "RED")]
///     Red,
///     Blue,
/// }
/// ```
#[proc_macro_derive(Enum, attributes(graphql))]
pub fn derive_enum(input: TokenStream) -> TokenStream {
    let input = parse_macro_input!(input as DeriveInput);
    enums::expand(input)
        .unwrap_or_else(syn::Error::into_compile_error)
        .into()
}

/// Derive a custom GraphQL scalar from a newtype.
///
/// The newtype delegates to its inner type's input and value conversions, which is how a
/// validated wrapper (a date, a UUID, an email address) becomes a scalar without writing any
/// conversions by hand.
///
/// ```ignore
/// use gurafi::Scalar;
///
/// #[derive(Scalar, Clone)]
/// #[graphql(name = "Date", description = "An ISO-8601 date.")]
/// struct Date(String);
/// ```
#[proc_macro_derive(Scalar, attributes(graphql))]
pub fn derive_scalar(input: TokenStream) -> TokenStream {
    let input = parse_macro_input!(input as DeriveInput);
    scalar::expand(input)
        .unwrap_or_else(syn::Error::into_compile_error)
        .into()
}

/// Derive a GraphQL union from an enum of object models.
///
/// Each variant must be a newtype variant whose inner type derives [`macro@Object`]. The union
/// itself is still declared on the schema with `schema.union(...)`, so its members and members'
/// fields stay visible where the rest of the schema is defined.
///
/// ```ignore
/// use gurafi::{Object, Union};
///
/// #[derive(Object)]
/// struct User {
///     name: String,
/// }
///
/// #[derive(Object)]
/// struct Post {
///     title: String,
/// }
///
/// #[derive(Union)]
/// enum SearchResult {
///     User(User),
///     Post(Post),
/// }
/// ```
#[proc_macro_derive(Union, attributes(graphql))]
pub fn derive_union(input: TokenStream) -> TokenStream {
    let input = parse_macro_input!(input as DeriveInput);
    union::expand(input)
        .unwrap_or_else(syn::Error::into_compile_error)
        .into()
}
