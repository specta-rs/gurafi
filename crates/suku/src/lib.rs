//! A production GraphQL-over-HTTP server built on [`gql`].
//!
//! `suku` turns a [`gql::Schema`] into an HTTP endpoint. It implements the GraphQL-over-HTTP
//! request/response protocol, including content negotiation, batches, GraphiQL and
//! server-sent-event subscriptions, while keeping the execution engine (`gql`) free of transport
//! concerns.
//!
//! # Layers
//!
//! - [`Server`] executes queries, mutations and subscriptions. It has no HTTP types.
//! - [`http`] is a transport-agnostic request handler. Any framework can call it with request
//!   parts and a body.
//! - [`axum`] mounts a [`Server`] on an Axum router (enabled by the default `axum` feature).
//!
//! # Example
//!
//! ```no_run
//! use suku::{Context, GraphQLRequest, Server};
//!
//! # async fn run(schema: gql::Schema) {
//! let server = Server::new(schema).introspection(true).max_depth(12);
//! let response = server
//!     .execute(&GraphQLRequest::new("{ hello }"), Context::empty())
//!     .await;
//! # let _ = response;
//! # }
//! ```
//!
//! Mounting on Axum:
//!
//! ```no_run
//! # #[cfg(feature = "axum")]
//! # fn run(schema: gql::Schema) {
//! let app = suku::axum::GraphQL::new(suku::Server::new(schema))
//!     .path("/graphql")
//!     .graphiql(true)
//!     .into_router();
//! # let _ = app;
//! # }
//! ```

#![forbid(unsafe_code)]
#![warn(missing_docs, missing_debug_implementations, rust_2018_idioms)]

pub mod http;
mod request;
mod response;
mod server;

pub mod prepared;

#[cfg(feature = "axum")]
pub mod axum;

pub use crate::{
    request::{GraphQLRequest, RequestError, RequestPayload, json_to_graphql},
    response::{
        GraphQLResponse, ResponseExt, error_json_bytes, error_response, to_json, to_json_bytes,
    },
    server::{Context, Server},
};

pub use crate::prepared::{
    PreparedOperation, PreparedQueries, PreparedQueriesError, persisted_query_not_found,
    persisted_query_not_found_error,
};

pub use crate::server::{
    DEFAULT_BATCH_CONCURRENCY, DEFAULT_MAX_BATCH_SIZE, DEFAULT_MAX_BODY_BYTES,
};

pub use crate::http::{
    ContextRequest, ExtractContext, HttpConfig, HttpError, HttpResponse, HttpResult, SseResponse,
    graphiql, graphiql_html, handle,
};

#[cfg(feature = "axum")]
pub use crate::axum::GraphQL;
