//! A production GraphQL-over-HTTP server built on [`gql`].
//!
//! `suku` turns a [`gql::Schema`] into an HTTP endpoint. It implements the GraphQL-over-HTTP
//! request/response protocol, including content negotiation, batches, GraphiQL,
//! server-sent-event subscriptions, the `graphql-transport-ws` WebSocket protocol and GraphQL
//! multipart requests, while keeping the execution engine (`gql`) free of transport concerns.
//!
//! # Layers
//!
//! - [`Server`] executes queries, mutations and subscriptions. It has no HTTP types. It also owns
//!   the optional error formatter that redacts messages before a client sees them.
//! - [`http`] is a transport-agnostic request handler. Any framework can call it with request
//!   parts and a body.
//! - [`axum`] mounts a [`Server`] on an Axum router (enabled by the default `axum` feature).
//! - `ws` drives one `graphql-transport-ws` connection over any text-message stream (enabled by
//!   the default `ws` feature, which implies `axum`).
//!
//! # Feature flags
//!
//! | Feature | Default | Purpose |
//! | --- | --- | --- |
//! | `axum` | yes | The [`axum`] router and `ExtractContext` integration. |
//! | `ws` | yes | `graphql-transport-ws` WebSocket subscriptions. Enables `axum`. |
//! | `multipart` | yes | GraphQL multipart requests; uses `multer`. |
//! | `vendored-graphiql` | no | Self-contained GraphiQL page with pinned browser assets. |
//! | `tracing` | no | Request spans, and `gql/tracing`. |
//!
//! To build without WebSocket support while keeping Axum:
//!
//! ```toml
//! suku = { version = "0.0.1", default-features = false, features = ["axum", "multipart"] }
//! ```
//!
//! A WebSocket upgrade then receives no special handling and falls through to the normal GET
//! path. To drop file uploads, omit `multipart`; a `multipart/form-data` POST is then rejected with
//! `415 Unsupported Media Type` exactly like any other non-JSON body.
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
//!
//! # Transports
//!
//! * **HTTP** — one request or batch per `GET`/`POST`, JSON responses and GraphiQL.
//! * **SSE** — `POST` + `Accept: text/event-stream` streams subscription events as
//!   `event: next`/`data:` frames.
//! * **WebSocket** — `graphql-transport-ws` over an upgrade at the same path. It multiplexes
//!   queries, mutations and subscriptions, and supports client cancellation. Authentication runs
//!   once per connection against the upgrade request; every operation reuses that [`Context`].
//! * **Multipart** — the GraphQL multipart request shape, with files inserted as
//!   [`gql::Upload`] values into request variables.
//!
//! See the module documentation of `ws` and `multipart` for the protocol details, and the
//! crate README for deployment notes.
//!
//! # GraphiQL assets
//!
//! The GraphiQL page served for `GET` requests that accept HTML loads React, ReactDOM and
//! GraphiQL from unpkg by default. Enable the `vendored-graphiql` feature to compile the pinned
//! browser assets into the crate and serve one self-contained page with no CDN requests:
//!
//! ```toml
//! [dependencies]
//! suku = { version = "0.0.1", features = ["vendored-graphiql"] }
//! ```
//!
//! The inline page works with both [`http::handle`] and the Axum adapter because it needs no asset
//! routes. Scripts and styles are inline, so an accompanying CSP should allow inline script/style
//! while restricting `connect-src` to the application itself. Versions, checksums, licenses and
//! the update procedure live in `crates/suku/src/http/vendor/`.
//!
//! # Observability
//!
//! The optional `tracing` feature (which also enables `gql/tracing`) emits a `suku.request` span
//! with method, path, body size, operation, request id, batch size and status, plus children for
//! parsing, execution, serialization and subscriptions. Transport and extractor failures are WARN
//! events with a status and code. Request bodies, headers, tokens and variable values are never
//! recorded. See the repository's `docs/plugins/tracing.md`.

#![forbid(unsafe_code)]
#![warn(missing_docs, missing_debug_implementations, rust_2018_idioms)]

mod error;
pub mod http;
#[cfg(feature = "multipart")]
pub mod multipart;
mod request;
mod response;
mod server;

pub mod prepared;

#[cfg(feature = "axum")]
pub mod axum;

#[cfg(feature = "ws")]
pub mod ws;

pub use crate::{
    error::{ErrorContext, ErrorFormatter, ErrorKind, INTERNAL_ERROR_MESSAGE, mask_internal},
    request::{GraphQLRequest, RequestError, RequestPayload, UploadBinding, json_to_graphql},
    response::{
        GraphQLResponse, ResponseExt, error_json_bytes, error_response, to_json, to_json_bytes,
    },
    server::{Context, Server},
};

pub use gql::Upload;

#[cfg(feature = "multipart")]
pub use crate::multipart::DEFAULT_MAX_MULTIPART_PARTS;

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
