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

#![cfg_attr(docsrs, feature(doc_cfg))]
#![warn(missing_docs, missing_debug_implementations, rust_2018_idioms)]
#![forbid(unsafe_code)]

pub mod ast;
pub mod complexity;
mod error;
pub mod execution;
mod introspection;
pub mod parser;
pub mod prepared;
mod schema;
mod sdl;
mod sdl_schema;
mod types;
pub mod validation;
mod value;

pub use error::{GraphQLError, Location, PathSegment, Response, Result};
pub use execution::{
    AuthorizationContext, Authorizer, ExecutionContext, ExecutionOptions, Request, ResponseStream,
    SubscriptionResponse, execute, subscribe,
};
pub use prepared::{
    PreparedQuery, PreparedQueryError, PreparedQueryRegistry, PreparedQueryRegistryError,
    execute_prepared,
};
pub use schema::{
    Schema, SchemaBuilder, SchemaChange, SchemaDiff, SchemaError, diff, is_output_subtype,
};
pub use sdl_schema::schema_from_sdl;
pub use types::{
    AppliedDirective, ComplexityFn, DirectiveDefinition, EnumType, EnumValue, Field, FieldResult,
    InputObjectType, InputValue, InterfaceType, Name, ObjectType, ResolveFn, ResolveFuture,
    ResolveInfo, ResolveTypeFn, ResolverContext, ScalarType, SubscribeFn, SubscriptionStream, Type,
    TypeKind, TypeRef, UnionType,
};
pub use validation::{ValidationOptions, validate};
pub use value::{External, Object, Value};

/// The version of the GraphQL specification this crate implements.
pub const SPEC_VERSION: &str = "October 2021";
