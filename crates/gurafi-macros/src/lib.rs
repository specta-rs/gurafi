//! Procedural macros for `gurafi`.
//!
//! These macros let Rust types drive the GraphQL type system:
//!
//! - [`macro@Object`] registers a Rust struct as a GraphQL object type.
//! - [`macro@Input`] registers a Rust struct as a field's arguments, or as a named input object
//!   when it carries `#[graphql(name = "...")]`.
//! - [`macro@Args`] is an alias for [`macro@Input`] that says the type is only a field's
//!   arguments.
//! - [`macro@Enum`] registers a Rust enum as a GraphQL enum.
//! - [`macro@Scalar`] registers a newtype as a custom scalar delegating to its inner type.
//! - [`macro@Union`] registers a Rust enum as a GraphQL union of object types.
//! - [`macro@ErrorUnion`] declares the typed errors a `field_result_union` exposes.
//!
//! Every macro emits all of the trait implementations `gurafi` needs, so a derive is the
//! whole registration: there is no second list of fields to keep in sync. Unsupported shapes
//! (generic types, tuple structs for [`macro@Input`], enums with data for [`macro@Enum`])
//! produce a `compile_error!` with an explanation instead of a confusing trait bound failure.

#![doc(
    html_favicon_url = "https://raw.githubusercontent.com/specta-rs/gurafi/main/docs/favicon.svg",
    html_logo_url = "https://raw.githubusercontent.com/specta-rs/gurafi/main/assets/app-icon.svg"
)]
#![forbid(unsafe_code)]
#![warn(missing_docs)]

mod attr;
mod enums;
mod error_union;
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

/// Derive a GraphQL input type.
///
/// # Arguments by default
///
/// Without a name, the type is **args-only**: its fields become a field's arguments
/// (`A: InputObject`) and no input object is registered in the schema. This is what an
/// arguments struct should be — `friends(limit: 10, after: "...")` rather than a leaked
/// `FriendsArgs` input type.
///
/// A type that is also used nested inside another input object must be named:
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
///
/// #[derive(Input)]
/// #[graphql(name = "FilterInput")]
/// struct Filter {
///     prefix: Option<String>,
/// }
///
/// #[derive(Input)]
/// struct SearchArgs {
///     filter: Option<Filter>, // requires FilterInput to exist
/// }
/// ```
///
/// Attribute support:
///
/// - `#[graphql(name = "...")]` makes the type a reusable named input object, or renames one of
///   its fields.
/// - `#[graphql(description = "...")]` (or a doc comment) sets a description.
/// - `#[graphql(default = expr)]` sets a default value, typed as the field's Rust type.
/// - `#[graphql(default_with = "path")]` computes the default by calling `path()`.
/// - `#[graphql(deprecated)]` / `#[graphql(deprecated = "reason")]` deprecates a field.
/// - `#[graphql(validate = "path")]` runs `path(&value)` after coercion. On the type it receives
///   `&Self`; on a field it receives `&FieldType`. The function returns
///   `Result<(), gql::GraphQLError>` and its error becomes a field error.
/// - `#[graphql(one_of)]` marks a named input as a `@oneOf` input. Every field must be
///   `Option<T>` and no field may have a default.
///
/// `Option<T>` fields are optional; every other field without a default is required.
#[proc_macro_derive(Input, attributes(graphql))]
pub fn derive_input(input: TokenStream) -> TokenStream {
    let input = parse_macro_input!(input as DeriveInput);
    input::expand(input)
        .unwrap_or_else(syn::Error::into_compile_error)
        .into()
}

/// Derive a field's arguments type.
///
/// This is an alias for [`macro@Input`] in its default, args-only form. It exists to make the
/// intent obvious at the declaration site; if the type later needs to be a reusable named input
/// object, add `#[graphql(name = "...")]` and the behaviour is identical to [`macro@Input`].
#[proc_macro_derive(Args, attributes(graphql))]
pub fn derive_args(input: TokenStream) -> TokenStream {
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

/// Derive the typed errors of a result-union field.
///
/// The enum is not itself a GraphQL type. It describes how a `field_result_union` field turns
/// each Rust error variant into a GraphQL object payload, and the generated result union uses
/// those payloads as members. Every variant must hold exactly one object model:
///
/// ```ignore
/// use gurafi::{ErrorUnion, Object};
///
/// #[derive(Object)]
/// struct NotFoundError {
///     message: String,
/// }
///
/// #[derive(Object)]
/// struct ConflictError {
///     field: String,
/// }
///
/// #[derive(ErrorUnion)]
/// enum CreateUserError {
///     NotFound(NotFoundError),
///     Conflict(ConflictError),
/// }
/// ```
///
/// The payload objects must still be registered with `schema.object::<M>(...)`. Missing
/// registration is a schema build error that names the missing payload. Generic enums, unit
/// variants, struct variants and tuple variants with zero or several fields are compile errors.
#[proc_macro_derive(ErrorUnion)]
pub fn derive_error_union(input: TokenStream) -> TokenStream {
    let input = parse_macro_input!(input as DeriveInput);
    error_union::expand(input)
        .unwrap_or_else(syn::Error::into_compile_error)
        .into()
}
