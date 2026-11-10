//! Axum integration (enabled by the default `axum` feature).
//!
//! [`GraphQL`] is a small builder around [`Server`]. It produces an [`axum::Router`] with one
//! route; authentication is a plain `async` closure that receives a [`ContextRequest`].
//!
//! ```no_run
//! use suku::{Context, axum::GraphQL, http::{ContextRequest, HttpError}};
//!
//! # fn run(schema: gql::Schema) {
//! let app = GraphQL::new(suku::Server::new(schema))
//!     .path("/graphql")
//!     .with_context(|request: ContextRequest| async move {
//!         let Some(value) = request.header("authorization") else {
//!             return Err(HttpError::message(
//!                 ::http::StatusCode::UNAUTHORIZED,
//!                 "Missing bearer token",
//!             ));
//!         };
//!         let value = value.to_str().unwrap_or_default();
//!         Ok(Context::new(value.to_owned()))
//!     })
//!     .into_router();
//! # let _ = app;
//! # }
//! ```
//!
//! The builder does not install CORS or tracing; compose those layers yourself so they match the
//! rest of your application.

use core::future::Future;
use std::sync::Arc;

use ::axum::{
    Router,
    body::{Body, to_bytes},
    extract::{Request, State},
    response::Response as AxumResponse,
    routing::any,
};
use ::http::{HeaderValue, StatusCode, header};
use futures::future::BoxFuture;

use crate::{
    Context, Server,
    http::{
        ContextRequest, ExtractContext, HttpConfig, HttpError, HttpResponse, HttpResult,
        SseResponse,
    },
    response::ResponseExt as _,
};

/// An Axum integration builder for a GraphQL server.
pub struct GraphQL {
    server: Server,
    config: HttpConfig,
    extractor: Arc<dyn ExtractContext>,
}

impl core::fmt::Debug for GraphQL {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        f.debug_struct("GraphQL")
            .field("server", &self.server)
            .field("config", &self.config)
            .finish_non_exhaustive()
    }
}

impl GraphQL {
    /// Start building an endpoint for `server`.
    #[must_use]
    pub fn new(server: Server) -> Self {
        Self {
            server,
            config: HttpConfig::default(),
            extractor: Arc::new(empty_extractor),
        }
    }

    /// Mount at a different path. Defaults to `/graphql`.
    #[must_use]
    pub fn path(mut self, path: impl Into<String>) -> Self {
        self.config = self.config.path(path);
        self
    }

    /// Enable or disable the GraphiQL page. Defaults to enabled.
    #[must_use]
    pub const fn graphiql(mut self, enabled: bool) -> Self {
        self.config.graphiql = enabled;
        self
    }

    /// Override the request body limit for this mount.
    #[must_use]
    pub const fn max_body_bytes(mut self, bytes: usize) -> Self {
        self.config.max_body_bytes = Some(bytes);
        self
    }

    /// Override the batch size limit for this mount.
    #[must_use]
    pub const fn max_batch_size(mut self, size: usize) -> Self {
        self.config.max_batch_size = Some(size);
        self
    }

    /// Authenticate requests and build their context.
    ///
    /// The closure receives a cloneable snapshot of the request parts, so it can read
    /// `Authorization`, cookies and middleware request extensions. Returning `Err` stops the
    /// request before the body is parsed or any resolver runs.
    #[must_use]
    pub fn with_context<F, Fut>(mut self, extractor: F) -> Self
    where
        F: Fn(ContextRequest) -> Fut + Send + Sync + 'static,
        Fut: Future<Output = Result<Context, HttpError>> + Send + 'static,
    {
        self.extractor = Arc::new(extractor);
        self
    }

    /// Use a pre-built extractor, useful when the extractor is shared with other endpoints.
    #[must_use]
    pub fn with_extractor(mut self, extractor: Arc<dyn ExtractContext>) -> Self {
        self.extractor = extractor;
        self
    }

    /// The server this builder will mount.
    #[must_use]
    pub fn server(&self) -> &Server {
        &self.server
    }

    /// The HTTP configuration for this mount.
    #[must_use]
    pub fn config(&self) -> &HttpConfig {
        &self.config
    }

    /// Build an Axum router with the GraphQL endpoint mounted.
    pub fn into_router(self) -> Router {
        let path = self.config.path.clone();
        let state = Arc::new(HandlerState {
            server: self.server,
            config: self.config,
            extractor: self.extractor,
        });

        Router::new().route(&path, any(handler)).with_state(state)
    }

    /// Build an Axum router. Alias for [`GraphQL::into_router`].
    pub fn router(self) -> Router {
        self.into_router()
    }
}

impl From<Server> for GraphQL {
    fn from(value: Server) -> Self {
        Self::new(value)
    }
}

/// Build an Axum endpoint for a server.
#[must_use]
pub fn graphql(server: Server) -> GraphQL {
    GraphQL::new(server)
}

/// Build an Axum router for a server using the default configuration.
///
/// Equivalent to `GraphQL::new(server).into_router()`.
#[must_use]
pub fn router(server: Server) -> Router {
    GraphQL::new(server).into_router()
}

struct HandlerState {
    server: Server,
    config: HttpConfig,
    extractor: Arc<dyn ExtractContext>,
}

async fn handler(State(state): State<Arc<HandlerState>>, request: Request) -> AxumResponse {
    let (parts, body) = request.into_parts();
    let configured_limit = state
        .config
        .max_body_bytes
        .unwrap_or_else(|| state.server.max_body_bytes_limit());
    let limit = if configured_limit == 0 {
        usize::MAX
    } else {
        configured_limit
    };

    let body = match to_bytes(body, limit).await {
        Ok(body) => body,
        Err(_) => {
            return error_response(
                &state.server,
                StatusCode::PAYLOAD_TOO_LARGE,
                "Request body could not be read or exceeded the size limit.",
            );
        }
    };

    let extractor = state.extractor.clone();
    let result = crate::http::handle(
        &state.server,
        &state.config,
        &parts,
        &body,
        move |request| extractor.extract(request),
    )
    .await;

    match result {
        HttpResult::Response(response) => buffered_response(response),
        HttpResult::Stream(response) => stream_response(response),
    }
}

fn buffered_response(response: HttpResponse) -> AxumResponse {
    let mut out = AxumResponse::new(Body::from(response.body));
    *out.status_mut() = response.status;
    *out.headers_mut() = response.headers;
    out
}

fn stream_response(response: SseResponse) -> AxumResponse {
    let mut out = AxumResponse::new(Body::from_stream(response.stream));
    *out.status_mut() = response.status;
    *out.headers_mut() = response.headers;
    out
}

fn error_response(server: &Server, status: StatusCode, message: &str) -> AxumResponse {
    let mut error = crate::response::error_response(message);
    server.format_response(
        &mut error,
        crate::error::ResponseMeta::none(),
        crate::error::ErrorKind::Request,
    );
    let mut response = AxumResponse::new(Body::from(error.to_json_bytes()));
    *response.status_mut() = status;
    response.headers_mut().insert(
        header::CONTENT_TYPE,
        HeaderValue::from_static("application/json; charset=utf-8"),
    );
    response
}

fn empty_extractor(_: ContextRequest) -> BoxFuture<'static, Result<Context, HttpError>> {
    Box::pin(async { Ok(Context::empty()) })
}
