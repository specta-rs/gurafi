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
//! - `#[derive(ErrorUnion)]` and `field_result_union` expose a `Result<T, E>` as a generated
//!   GraphQL union of a success payload and typed error payloads.
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
//! - Object, interface and union names can be computed at runtime with [`Schema::object_ref`],
//!   [`Schema::interface_ref`] and [`Schema::union_ref`]. The reference keeps the model for
//!   resolvers and `is_type_of`, while [`ObjectBuilder::field_ref`] and
//!   [`FieldConfig::as_dynamic`] declare the runtime name in output positions. This is how
//!   Relay-style `UserPostsConnection` and `CreateUserPayload` types are expressed.
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
//!     o.implements(&node);
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
//!
//! # Resolving a whole object at once
//!
//! [`Raw<T>`] returns a prebuilt [`gql::Value`] while taking its schema type from `T`. Declare
//! the object's fields with [`ObjectBuilder::raw_field`] and the executor reads them from the
//! map, so one resolver can produce an entire object:
//!
//! ```
//! # use gurafi::{NoArgs, Object, Raw, RawObject, Schema};
//! # struct Ctx;
//! # #[derive(Object)] struct Giraffe { name: String, height: f32 }
//! # let mut schema = Schema::<Ctx>::new();
//! schema.object::<Giraffe>(|o| {
//!     o.raw_field::<String>("name");
//!     o.raw_field::<f32>("height");
//! });
//! schema.query(|q| {
//!     q.field("giraffe", |_: NoArgs, _: &Ctx| {
//!         Raw::<Giraffe>::object(
//!             RawObject::new()
//!                 .field("name", "Melman")
//!                 .field("height", 3.4)
//!                 .build(),
//!         )
//!     });
//! });
//! # let _ = schema.build().expect("valid schema");
//! ```
//!
//! The caller is responsible for field names, nullability and `__typename`; most mistakes are
//! execution-time field errors with normal null propagation. Custom scalar values are passed
//! through unchanged, and mixing a raw field with a resolver-backed field is a build error. See
//! [`Raw`] and [`RawObject`] for lists, abstract types, reserved `__gql_*` keys and the
//! performance tradeoff.
//!
//! # Plugins
//!
//! [`Schema::plugin`] registers a build-time extension that sees every object, interface and
//! root field. A plugin can change metadata through [`FieldMeta`] and [`Plugin::field`], wrap
//! resolvers with [`Plugin::wrap_resolve`] and subscription factories with
//! [`Plugin::wrap_subscribe`], or add types and directives through [`Plugin::configure`].
//! Plugins are applied once when the schema is built, so fields declared before or after
//! registration are covered equally.
//!
//! ```
//! use gurafi::{NoArgs, ResponseExtensionPlugin, Schema};
//!
//! struct Ctx;
//!
//! let mut schema = Schema::<Ctx>::new();
//! schema.plugin(ResponseExtensionPlugin::new("servedBy", "gurafi").root_only());
//! schema.query(|q| {
//!     q.field("ok", |_: NoArgs, _: &Ctx| true);
//! });
//! let schema = schema.build().expect("valid schema");
//! # let _ = schema;
//! ```
//!
//! [`TracingPlugin`] emits start/finish events without depending on a particular logging crate,
//! and [`MockPlugin`] replaces named fields for tests. See the [`plugin`] module for ordering,
//! coverage and the full hook documentation.
//!
//! # Custom directives
//!
//! A directive is registered once with [`Schema::directive`] and applied where it belongs. The
//! definition is the `gql` model, so descriptions, arguments, repeatability and locations come
//! from the usual builders:
//!
//! ```
//! use gurafi::{NoArgs, Object, Schema};
//!
//! struct Ctx;
//!
//! #[derive(Object)]
//! struct Giraffe {
//!     name: String,
//! }
//!
//! let mut schema = Schema::<Ctx>::new();
//! schema.directive(
//!     gql::DirectiveDefinition::new("tag")
//!         .description("A documentation tag.")
//!         .location(gql::ast::DirectiveLocation::Object)
//!         .location(gql::ast::DirectiveLocation::FieldDefinition)
//!         .repeatable()
//!         .arg(gql::InputValue::new("name", gql::TypeRef::string())),
//! );
//!
//! // Apply by name; the type may be registered before or after this call.
//! schema.type_directive(
//!     "Giraffe",
//!     gql::AppliedDirective::new("tag").argument("name", "animal"),
//! );
//!
//! schema.object::<Giraffe>(|o| {
//!     o.directive(gql::AppliedDirective::new("tag").argument("name", "declared"));
//!     o.field("name", |g: &Giraffe, _: NoArgs, _: &Ctx| g.name.clone())
//!         .directive(gql::AppliedDirective::new("tag").argument("name", "public"));
//! });
//! schema.query(|q| {
//!     q.field("giraffe", |_: NoArgs, _: &Ctx| Giraffe { name: "Melman".into() });
//! });
//!
//! let sdl = schema.build().expect("valid schema").sdl();
//! assert!(sdl.contains("type Giraffe @tag(name: \"declared\") @tag(name: \"animal\")"));
//! assert!(sdl.contains("name: String! @tag(name: \"public\")"));
//! ```
//!
//! Applications attach at the declaration site with [`ObjectBuilder::directive`],
//! [`InterfaceBuilder::directive`], [`UnionBuilder::directive`], [`FieldConfig::directive`] and
//! [`InterfaceFieldConfig::directive`]. Types registered elsewhere use
//! [`Schema::type_directive`], which accepts an object, interface, union, scalar, enum, input
//! object or root operation type and works whether the type already exists or is registered
//! later. Field arguments and input object fields use [`Schema::argument_directive`] and
//! [`Schema::input_field_directive`].
//!
//! Definitions are validated by the same rules as SDL: an unknown directive, a location the
//! definition does not allow, a wrong argument type, a missing required argument and a repeated
//! use of a non-repeatable directive are build errors. Definitions appear in SDL and in
//! introspection's `__schema.directives`; applications appear in SDL. Standard introspection has
//! no field for *applied* directives, so a client that needs them reads the SDL.
//!
//! The `#[graphql(directive = "...")]` derive attribute is not implemented. A directive on a
//! derived type or field is declared on the builder instead, which keeps every application in
//! the schema where the rest of the type's shape is defined.
//!
//! # Batching and per-request caches
//!
//! [`Loader`] collects loads issued in the same scheduler tick into one call of a user-supplied
//! batch function, deduplicates keys that are already in flight, and caches each result for the
//! rest of the request. It is spawn-free and executor-agnostic, so it needs no timer and no
//! runtime-specific API. [`RequestCache`] is a type-keyed store scoped to one request, which is
//! where loaders and other request-wide state belong: put both in the context passed to
//! [`gql::ExecutionOptions::with_context`] and every resolver in that request shares them, while
//! the next request starts empty.
//!
//! # Observability
//!
//! The optional `tracing` feature instruments schema building, plugin application, authorization,
//! validation, resolvers, error-union dispatch, [`Loader`] batching and [`RequestCache`] lookups.
//! It is off by default. Loader spans record the key *type* and cache hit/miss, never key values;
//! per-resolver and cache events are TRACE. See the repository's `docs/plugins/tracing.md`.

#![doc(
    html_favicon_url = "https://raw.githubusercontent.com/specta-rs/gurafi/main/docs/favicon.svg",
    html_logo_url = "https://raw.githubusercontent.com/specta-rs/gurafi/main/assets/app-icon.svg"
)]
#![forbid(unsafe_code)]
#![warn(missing_docs)]

mod cache;
mod error;
mod error_union;
mod expose;
mod field;
mod id;
mod loader;
pub mod plugin;
mod raw;
mod registry;
mod schema;
mod telemetry;
mod traits;
mod upload;
mod value;

pub use gql;

pub use cache::RequestCache;
pub use error::SchemaError;
pub use error_union::ErrorUnionModel;
pub use expose::{__expose_name, Expose};
pub use field::{FieldConfig, InterfaceFieldConfig};
pub use gql::{GraphQLError, Value};
pub use gurafi_macros::{Args, Enum, ErrorUnion, Input, Object, Scalar, Union};
pub use id::ID;
pub use loader::{Loader, LoaderConfig};
pub use plugin::{
    FieldMeta, MockPlugin, Plugin, ResponseExtensionPlugin, TraceEvent, TracePhase, TracingPlugin,
};
pub use raw::{Raw, RawObject};
pub use registry::TypeRegistry;
pub use schema::{
    BoxFuture, BuildOptions, InterfaceBuilder, InterfaceRef, ObjectBuilder, ObjectRef, RootBuilder,
    Schema, SubscriptionBuilder, UnionBuilder, UnionRef, type_ref,
};
pub use traits::{
    AbstractType, EnumModel, FromValue, InputObject, InputType, IntoValue, NoArgs, ObjectModel,
    OutputType, ScalarValue, UnionModel,
};
pub use upload::Upload;

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
