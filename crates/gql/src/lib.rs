//! A GraphQL reference implementation for Rust.
//!
//! `gql` implements the GraphQL specification itself: the type system, the executable document
//! language, validation, execution, introspection, subscriptions and schema compatibility
//! checking. It deliberately knows nothing about HTTP, frameworks, databases or authentication.
//!
//! Higher-level crates build on the types in this crate. [`gurafi`](https://docs.rs/gurafi) adds
//! a type-safe, code-first schema builder, and [`suku`](https://docs.rs/suku) turns a [`Schema`]
//! into an HTTP server.
//!
//! # Example
//!
//! ```
//! use gql::{Field, ObjectType, ScalarType, Schema, TypeRef, Value};
//!
//! let mut builder = Schema::builder();
//! builder.add_type(ObjectType::new("Query").field(
//!     Field::new("hello", TypeRef::string().non_null()).resolve(|_| {
//!         Box::pin(async { Ok(Value::from("world")) })
//!     }),
//! ).into());
//! builder = builder.query("Query");
//! let schema = builder.build().expect("valid schema");
//!
//! let response = futures::executor::block_on(gql::execute(
//!     &schema,
//!     gql::Request::new("{ hello }"),
//!     &gql::ExecutionOptions::default(),
//! ));
//! assert_eq!(response.data.unwrap().as_object().unwrap().get("hello").unwrap().as_str(), Some("world"));
//! ```
//!
//! # Resolver panics
//!
//! Resolvers are caller code, so every call into one is isolated. If a `resolve` resolver, a
//! subscription's `subscribe` resolver, an interface or union `resolve_type` hook or an object
//! `is_type_of` check panics, the panic is caught at that boundary and reported as an ordinary
//! error instead of unwinding the request:
//!
//! * a field resolver panic becomes a field error with the message `"Internal server error"` and
//!   `extensions.code` of `"INTERNAL_SERVER_ERROR"`, with the usual response path and source
//!   location attached by field completion. Sibling fields still complete, and null propagation
//!   applies as for any other field error;
//! * a subscription setup panic makes [`subscribe`] return an error response, while a panic in a
//!   per-event resolver yields an error response for that event and leaves the stream running;
//! * the panic payload and backtrace are never copied into the error, and the error is built
//!   without formatting any caught value.
//!
//! Isolation does not change the panic hook: the process-wide hook still runs, so existing
//! logging or crash reporting sees the panic. Only the unwind is caught.
//!
//! This relies on [`std::panic::catch_unwind`], which cannot intercept a panic in a build
//! configured with `panic = "abort"`; such a profile aborts the process on the first panic before
//! any isolation can run.
//!
//! # Observability
//!
//! The optional `tracing` feature instruments parsing, validation, variable coercion, operation
//! execution, per-field completion, subscriptions and schema construction. It is off by default,
//! so a normal build does not compile `tracing` and contains none of the instrumentation. With
//! the feature enabled, spans record counts, byte lengths and short identifier prefixes; variable
//! values, arguments and document text are never recorded. Operation summaries are INFO, field
//! spans are DEBUG and collection spans are TRACE. See the repository's
//! `docs/plugins/tracing.md` for the span tree.

#![doc(
    html_favicon_url = "https://raw.githubusercontent.com/specta-rs/gurafi/main/docs/favicon.svg"
)]
#![cfg_attr(docsrs, feature(doc_cfg))]
#![warn(missing_docs, missing_debug_implementations, rust_2018_idioms)]
#![forbid(unsafe_code)]

pub mod ast;
pub mod complexity;
mod error;
pub mod execution;
mod fragments;
mod introspection;
pub mod parser;
pub mod prepared;
mod schema;
mod sdl;
mod sdl_schema;
mod telemetry;
mod types;
mod upload;
pub mod validation;
mod value;

pub use ast::{FieldNodes, FieldNodesIter};
pub use error::{GraphQLError, Location, PathSegment, Response, Result};
pub use execution::{
    AuthorizationContext, Authorizer, ExecutionContext, ExecutionOptions, Request, ResponseStream,
    SubscriptionResponse, execute, subscribe,
};
pub use fragments::Fragments;
pub use prepared::{
    PreparedQuery, PreparedQueryError, PreparedQueryRegistry, PreparedQueryRegistryError,
    execute_prepared,
};
pub use schema::{
    FieldTransform, Schema, SchemaBuilder, SchemaChange, SchemaDiff, SchemaError, SchemaTransform,
    diff, is_output_subtype,
};
pub use sdl_schema::schema_from_sdl;
pub use types::{
    AppliedDirective, ComplexityFn, DirectiveDefinition, EnumType, EnumValue, Field, FieldResult,
    InputObjectType, InputValue, InterfaceType, Name, ObjectType, PropertyContext,
    PropertyResolveFn, ResolveFn, ResolveFuture, ResolveInfo, ResolveTypeFn, ResolverContext,
    ScalarType, SubscribeFn, SubscriptionStream, Type, TypeKind, TypeRef, UnionType,
};
pub use upload::Upload;
pub use validation::{ValidationOptions, validate};
pub use value::{External, Object, Value};

/// The version of the GraphQL specification this crate implements.
pub const SPEC_VERSION: &str = "October 2021";
