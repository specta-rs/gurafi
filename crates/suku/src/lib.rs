//! A GraphQL-over-HTTP server built on [`gql`].
//!
//! `suku` turns a [`gql::Schema`] into an HTTP endpoint implementing the
//! [GraphQL over HTTP specification](https://http-spec.graphql.org) (September 2026 edition):
//! `GET`/`POST` requests, `Accept` negotiation between `application/graphql-response+json` and
//! legacy `application/json`, and the specification's status codes (including `294 Partial
//! Success`). The execution engine (`gql`) stays free of transport concerns.
//!
//! # Layers
//!
//! - [`Server`] executes queries, mutations and subscriptions. It has no HTTP types. It also owns
//!   the optional error formatter that redacts messages before a client sees them.
//! - [`http`] is a transport-agnostic request handler. Any framework can call it with request
//!   parts and a body.
//! - [`axum`] mounts a [`Server`] on an Axum router (enabled by the `axum` feature).
//! - `ws` drives one `graphql-transport-ws` connection over any text-message stream (enabled by
//!   the `ws` feature, which implies `axum`).
//!
//! # Feature flags
//!
//! Only `axum` is enabled by default. Everything beyond the GraphQL over HTTP specification is
//! opt-in.
//!
//! | Feature | Default | Purpose |
//! | --- | --- | --- |
//! | `axum` | yes | The [`axum`] router and `ExtractContext` integration. |
//! | `sse` | no | Server-sent-event subscriptions (`text/event-stream`). A transport extension. |
//! | `ws` | no | `graphql-transport-ws` WebSocket subscriptions. A non-HTTP transport. Enables `axum`. |
//! | `multipart` | no | GraphQL multipart requests (file uploads). A community specification. Uses `multer`. |
//! | `vendored-graphiql` | no | Self-contained GraphiQL page with pinned browser assets. |
//! | `tracing` | no | Request spans, and `gql/tracing`. |
//!
//! To serve subscriptions over server-sent events:
//!
//! ```toml
//! suku = { version = "0.1.2", features = ["sse"] }
//! ```
//!
//! Without `sse`, a subscription request receives a `406 Not Acceptable` when only
//! `text/event-stream` is acceptable, or a clear GraphQL error when JSON is acceptable.
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
//! * **HTTP** — one `GET` or `POST` per request, with `application/graphql-response+json`
//!   responses and GraphiQL. This is the specification's scope.
//! * **SSE** (feature `sse`) — `POST` + `Accept: text/event-stream` streams subscription events
//!   as `event: next`/`data:` frames. The specification does not define subscriptions.
//! * **WebSocket** (feature `ws`) — `graphql-transport-ws` over an upgrade at the same path. It
//!   multiplexes queries, mutations and subscriptions, and supports client cancellation.
//!   Authentication runs once per connection against the upgrade request; every operation reuses
//!   that [`Context`].
//! * **Multipart** (feature `multipart`) — the GraphQL multipart request shape, with files
//!   inserted as [`gql::Upload`] values into request variables.
//!
//! Request batching is not part of the specification and is not supported over HTTP. Use one
//! request per operation; [`Server::execute_batch`] remains available for callers that execute
//! several operations in-process.
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
//! suku = { version = "0.1.2", features = ["vendored-graphiql"] }
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
//! with method, path, body size, operation, request id and status, plus children for parsing,
//! execution, serialization and subscriptions. Transport and extractor failures are WARN events
//! with a status and code. Request bodies, headers, tokens and variable values are never
//! recorded. See the repository's `docs/plugins/tracing.md`.

#![doc(
    html_favicon_url = "https://raw.githubusercontent.com/specta-rs/gurafi/main/docs/favicon.svg",
    html_logo_url = "https://raw.githubusercontent.com/specta-rs/gurafi/main/assets/app-icon.svg"
)]
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
    request::{GraphQLRequest, RequestError, UploadBinding, json_to_graphql},
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

pub use crate::server::{DEFAULT_BATCH_CONCURRENCY, DEFAULT_MAX_BODY_BYTES};

pub use crate::http::{
    ContextRequest, ExtractContext, HttpConfig, HttpError, HttpResponse, HttpResult, SseResponse,
    graphiql, graphiql_html, handle,
};

#[cfg(feature = "axum")]
pub use crate::axum::GraphQL;
