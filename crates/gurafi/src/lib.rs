//! A type-safe, code-first GraphQL schema builder for Rust.
//!
//! `gurafi` sits on top of [`gql`], which implements the GraphQL specification. This crate adds
//! the part a Rust developer actually writes: Rust types drive GraphQL types, field declarations
//! derive their arguments from a Rust `Args` type, and mistakes are compile errors wherever
//! possible.
//!
//! # The idea
//!
//! - `#[derive(Object)]`, `#[derive(Input)]`, `#[derive(Enum)]`, `#[derive(Scalar)]` and
//!   `#[derive(Union)]` register a Rust type as a GraphQL type.
//! - A field's GraphQL output type is inferred from its Rust return type via [`OutputType`].
//! - A field's GraphQL arguments are inferred from its Rust arguments type via [`InputObject`].
//!   There is no second list of argument declarations to keep in sync. An `Input` type is
//!   args-only by default; add `#[graphql(name = "FilterInput")]` when it should also be a
//!   reusable input object nested inside other inputs.
//! - Resolvers always receive `(&Model, Args, &Ctx)` for object fields and `(Args, &Ctx)` for
//!   root fields, with sync, async, infallible and fallible variants. `*_with_info` variants add
//!   the [`gql::ResolveInfo`].
//! - Field options are chained on the returned [`FieldConfig`]: descriptions, deprecation,
//!   complexity estimates, authorization, nullability and checked type overrides.
//! - A type's definition can be split across modules with [`Schema::object_fields`],
//!   [`Schema::interface_fields`] and the root `*_fields` methods; declarations merge.
//!
//! # Example
//!
//! ```
//! use gurafi::{Enum, ID, Input, NoArgs, Object, Result, Schema, expose};
//!
//! struct Ctx {
//!     is_logged_in: bool,
//! }
//!
//! #[derive(Object)]
//! #[graphql(description = "A long-necked animal.")]
//! struct Giraffe {
//!     name: String,
//!     birthday: String,
//!     height: f32,
//! }
//!
//! #[derive(Input)]
//! struct FriendsArgs {
//!     #[graphql(description = "Max results", default = 10)]
//!     limit: i32,
//!     after: Option<String>,
//! }
//!
//! #[derive(Enum)]
//! enum Color {
//!     #[graphql(name = "RED")]
//!     Red,
//!     Blue,
//! }
//!
//! let mut schema = Schema::<Ctx>::new();
//!
//! let node = schema.interface("Node", |i| {
//!     i.description("Anything with an id").field::<ID>("id");
//! });
//!
//! schema.object::<Giraffe>(|o| {
//!     o.description("Long necks.");
//!     o.implements(node);
//!     o.expose(expose!(Giraffe::name));
//!     o.expose(expose!(Giraffe::height)).deprecated("Use heightMeters");
//!     o.field("id", |g: &Giraffe, _: NoArgs, _: &Ctx| ID::new(g.name.clone()));
//!     o.field("age", |g: &Giraffe, _: NoArgs, _: &Ctx| {
//!         g.birthday.parse::<u16>().ok()
//!     })
//!     .description("Age in years");
//!     o.field_result("friends", |_: &Giraffe, _: FriendsArgs, _: &Ctx| {
//!         Result::Ok(Vec::<Giraffe>::new())
//!     });
//!     o.field_async("nameAsync", |g: &Giraffe, _: NoArgs, _: &Ctx| {
//!         Box::pin(async move { g.name.clone() })
//!     });
//! });
//!
//! schema.query(|q| {
//!     q.field("giraffe", |_: NoArgs, _: &Ctx| Giraffe {
//!         name: "Melman".into(),
//!         birthday: "2001-04-12".into(),
//!         height: 3.4,
//!     })
//!     .auth(|ctx: &Ctx| ctx.is_logged_in)
//!     .complexity(2);
//! });
//!
//! let gql_schema: gql::Schema = schema.build().expect("valid schema");
//! assert!(gql_schema.object_type("Giraffe").is_some());
//! ```
//!
//! # Authentication and authorization
//!
//! Authentication is the server's job: it validates a token and puts the resulting user into
//! the context passed to [`gql::ExecutionOptions::with_context`]. Authorization is per field:
//! [`FieldConfig::auth`] runs a predicate against that context before the resolver, and a denial
//! is a normal GraphQL error, so it participates in null propagation. Root and nested fields use
//! the same mechanism.
//!
//! Whole-object rules are declared once with [`ObjectBuilder::auth`]; interface rules with
//! [`InterfaceBuilder::auth`] are inherited by every implementing object, including through
//! another interface. `auth_async` variants await a boxed future, and interface fields can carry
//! their own authorization that objects inherit. Every variant composes with field-level checks.
//!
//! # Asynchronous resolvers
//!
//! An async resolver returns a boxed future:
//!
//! ```
//! # use gurafi::{Object, Schema, NoArgs};
//! # struct Ctx {}
//! # #[derive(Object)] struct Giraffe { name: String }
//! # let mut schema = Schema::<Ctx>::new();
//! # schema.object::<Giraffe>(|o| {
//! o.field_async("name", |g: &Giraffe, _: NoArgs, _: &Ctx| {
//!     Box::pin(async move { g.name.clone() })
//! });
//! # });
//! ```
//!
//! `Box::pin` is required because the closure must return a future that borrows the model, and
//! Rust's `Fn` traits cannot express a return type that borrows from the closure's arguments.
//! The executor boxes resolver futures anyway, so this is not an extra allocation in practice.

#![forbid(unsafe_code)]
#![warn(missing_docs)]

mod error;
mod expose;
mod field;
mod id;
mod registry;
mod schema;
mod traits;
mod value;

pub use gql;

pub use error::SchemaError;
pub use expose::{__expose_name, Expose};
pub use field::{FieldConfig, InterfaceFieldConfig};
pub use gql::{GraphQLError, Value};
pub use gurafi_macros::{Args, Enum, Input, Object, Scalar, Union};
pub use id::ID;
pub use registry::TypeRegistry;
pub use schema::{
    BoxFuture, BuildOptions, InterfaceBuilder, InterfaceRef, ObjectBuilder, RootBuilder, Schema,
    SubscriptionBuilder, UnionBuilder, UnionRef, type_ref,
};
pub use traits::{
    AbstractType, EnumModel, FromValue, InputObject, InputType, IntoValue, NoArgs, ObjectModel,
    OutputType, ScalarValue, UnionModel,
};

/// The result type used by fallible resolvers.
///
/// `E = gql::GraphQLError` is the default, so `Result<Vec<Giraffe>>` is
/// `Result<Vec<Giraffe>, GraphQLError>` and any error type implementing
/// `Into<gql::GraphQLError>` also works.
pub type Result<T, E = gql::GraphQLError> = core::result::Result<T, E>;

/// Implement the conversions needed to use a hand-written [`ScalarValue`] type in a schema.
///
/// [`derive(Scalar)`](macro@Scalar) writes these impls for newtype scalars. A scalar with a
/// custom representation implements [`ScalarValue`] by hand and then invokes this macro:
///
/// ```
/// use gurafi::{GraphQLError, ScalarValue, Value, impl_scalar_traits};
///
/// struct Slug(String);
///
/// impl ScalarValue for Slug {
///     const NAME: &'static str = "Slug";
///
///     fn to_value(&self) -> Result<Value, GraphQLError> {
///         Ok(Value::String(self.0.clone().into()))
///     }
///
///     fn from_value(value: &Value) -> Result<Self, GraphQLError> {
///         value
///             .as_str()
///             .map(|value| Self(value.to_owned()))
///             .ok_or_else(|| GraphQLError::new("Expected a string."))
///     }
/// }
///
/// impl_scalar_traits!(Slug);
/// ```
#[macro_export]
macro_rules! impl_scalar_traits {
    ($ty:ty) => {
        impl $crate::OutputType for $ty {
            fn output_type() -> $crate::gql::TypeRef {
                $crate::gql::TypeRef::named(<Self as $crate::ScalarValue>::NAME)
            }

            fn register_output(registry: &mut $crate::TypeRegistry) {
                registry.register_scalar::<Self>();
            }
        }

        impl $crate::InputType for $ty {
            fn input_type() -> $crate::gql::TypeRef {
                $crate::gql::TypeRef::named(<Self as $crate::ScalarValue>::NAME)
            }

            fn register_input(registry: &mut $crate::TypeRegistry) {
                registry.register_scalar::<Self>();
            }
        }

        impl $crate::IntoValue for $ty {
            fn into_value(
                self,
            ) -> ::core::result::Result<$crate::gql::Value, $crate::gql::GraphQLError> {
                <Self as $crate::ScalarValue>::to_value(&self)
            }
        }

        impl $crate::FromValue for $ty {
            fn from_value(
                value: &$crate::gql::Value,
            ) -> ::core::result::Result<Self, $crate::gql::GraphQLError> {
                <Self as $crate::ScalarValue>::from_value(value)
            }
        }
    };
}
