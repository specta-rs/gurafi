//! GraphQL over HTTP.
//!
//! [`handle`] implements the [GraphQL over HTTP specification](https://http-spec.graphql.org)
//! (September 2026 edition) for one request: `Accept` negotiation, `GET` and `POST` parameters,
//! request body limits, the specification's status codes, and GraphiQL.
//!
//! Everything beyond the specification is opt-in and lives behind a feature:
//!
//! * `sse` adds subscription responses as `text/event-stream`.
//! * `multipart` adds GraphQL multipart requests for file uploads.
//!
//! Both are extensions: the specification defines neither subscriptions nor multipart, and says
//! request batching is not standardised yet. This module therefore accepts exactly one
//! GraphQL-over-HTTP request per HTTP request.
//!
//! It never touches a socket, so the same logic backs the Axum integration and can be embedded in
//! any HTTP framework.
//!
//! The `Authorization` header (and every other request header) is never interpreted here. Pass an
//! [`ExtractContext`] callback to authenticate a request and build the per-request [`Context`].
//! Multipart requests are authenticated before their parts are parsed.

mod accept;

use core::convert::Infallible;
use std::{fmt, future::Future};

use ::http::{HeaderMap, HeaderValue, Method, StatusCode, header};
use bytes::Bytes;
use futures::{future::BoxFuture, stream::BoxStream};
use gql::{GraphQLError, Response};
use serde_json::{Map as JsonMap, Value as JsonValue};

use crate::{
    Context, Server,
    error::{ErrorKind, ResponseMeta},
    request::GraphQLRequest,
    response::ResponseExt as _,
    server::{ExecutionOutcome, FailureKind},
};

use self::accept::Accept;

// The default page loads React and GraphiQL from unpkg. With `vendored-graphiql`, `build.rs`
// assembles a self-contained page from the checked-in browser bundles so an offline or
// air-gapped deployment never contacts a CDN.
#[cfg(feature = "vendored-graphiql")]
const GRAPHIQL_HTML: &str = include_str!(concat!(env!("OUT_DIR"), "/graphiql.html"));
#[cfg(not(feature = "vendored-graphiql"))]
const GRAPHIQL_HTML: &str = include_str!("graphiql.html");

/// The static GraphiQL page served for `GET` requests that accept HTML.
///
/// By default the page loads React, ReactDOM and GraphiQL from unpkg. Enable the
/// `vendored-graphiql` feature to serve a self-contained page with every asset inlined and no
/// network requests to a CDN.
#[must_use]
pub fn graphiql_html() -> &'static str {
    GRAPHIQL_HTML
}

/// The static GraphiQL page.
///
/// This is an alias for [`graphiql_html`], matching the name of the Axum builder method.
#[must_use]
pub fn graphiql() -> &'static str {
    GRAPHIQL_HTML
}

/// A cloneable snapshot of the HTTP request parts available to a context extractor.
///
/// Cloning the headers and extensions is cheap relative to a GraphQL execution, and owning the
/// snapshot lets extractors be ordinary `async` closures instead of higher-ranked trait objects.
#[derive(Clone, Debug)]
pub struct ContextRequest {
    /// The request method.
    pub method: Method,
    /// The request URI, including the query string.
    pub uri: ::http::Uri,
    /// All request headers, including `Authorization` and `Cookie`.
    pub headers: HeaderMap,
    /// HTTP request extensions, for values inserted by earlier middleware.
    pub extensions: ::http::Extensions,
}

impl ContextRequest {
    /// Snapshot borrowed request parts.
    #[must_use]
    pub fn from_parts(parts: &::http::request::Parts) -> Self {
        Self {
            method: parts.method.clone(),
            uri: parts.uri.clone(),
            headers: parts.headers.clone(),
            extensions: parts.extensions.clone(),
        }
    }

    /// Look up a header by name.
    #[must_use]
    pub fn header(&self, name: &str) -> Option<&HeaderValue> {
        self.headers.get(name)
    }

    /// Read one cookie value by name.
    #[must_use]
    pub fn cookie(&self, name: &str) -> Option<&str> {
        let cookies = self.headers.get(header::COOKIE)?.to_str().ok()?;
        cookies.split(';').find_map(|cookie| {
            let (key, value) = cookie.split_once('=')?;
            (key.trim() == name).then_some(value.trim())
        })
    }
}

/// Builds the per-request [`Context`], usually from an authentication header or cookie.
///
/// Implementations are blanket-provided for closures of the shape
/// `Fn(ContextRequest) -> impl Future<Output = Result<Context, HttpError>> + Send + 'static`.
/// Returning `Err` short-circuits the request with the supplied status and GraphQL response; the
/// transport never runs resolvers for a denied request.
pub trait ExtractContext: Send + Sync + 'static {
    /// Authenticate the request and build its context.
    fn extract(&self, request: ContextRequest) -> BoxFuture<'static, Result<Context, HttpError>>;
}

impl<F, Fut> ExtractContext for F
where
    F: Fn(ContextRequest) -> Fut + Send + Sync + 'static,
    Fut: Future<Output = Result<Context, HttpError>> + Send + 'static,
{
    fn extract(&self, request: ContextRequest) -> BoxFuture<'static, Result<Context, HttpError>> {
        Box::pin(self(request))
    }
}

/// A denial or failure produced while building a request context.
#[derive(Clone, Debug)]
pub struct HttpError {
    /// The HTTP status to send.
    pub status: StatusCode,
    /// The GraphQL response body.
    pub response: Response,
    /// Extra headers, such as `WWW-Authenticate`.
    pub headers: HeaderMap,
}

impl HttpError {
    /// Create an error with an explicit status.
    #[must_use]
    pub fn new(status: StatusCode, response: Response) -> Self {
        Self {
            status,
            response,
            headers: HeaderMap::new(),
        }
    }

    /// Create an error with a message body.
    #[must_use]
    pub fn message(status: StatusCode, message: impl Into<String>) -> Self {
        Self::new(status, crate::response::error_response(message))
    }

    /// A `401 Unauthorized` denial.
    #[must_use]
    pub fn unauthorized(response: Response) -> Self {
        Self::new(StatusCode::UNAUTHORIZED, response)
    }

    /// A `403 Forbidden` denial.
    #[must_use]
    pub fn forbidden(response: Response) -> Self {
        Self::new(StatusCode::FORBIDDEN, response)
    }

    /// An internal error that hides implementation details from the client.
    #[must_use]
    pub fn internal(message: impl Into<String>) -> Self {
        Self::message(StatusCode::INTERNAL_SERVER_ERROR, message)
    }

    /// Add a response header.
    #[must_use]
    pub fn with_header(mut self, name: header::HeaderName, value: HeaderValue) -> Self {
        self.headers.insert(name, value);
        self
    }
}

impl From<Response> for HttpError {
    fn from(value: Response) -> Self {
        Self::unauthorized(value)
    }
}

impl From<GraphQLError> for HttpError {
    fn from(value: GraphQLError) -> Self {
        Self::unauthorized(Response::error(value))
    }
}

/// Configuration for one HTTP mount point.
///
/// This configures transport behaviour only. Error formatting is configured once on [`Server`]
/// with [`Server::format_error`] and applies to every mount, including request errors produced by
/// this module.
#[derive(Clone, Debug)]
pub struct HttpConfig {
    /// The path the GraphQL endpoint is mounted at. Defaults to `/graphql`.
    pub path: String,
    /// Whether to serve GraphiQL for `GET` requests that accept HTML. Defaults to `true`.
    pub graphiql: bool,
    /// The largest request body, or the largest `GET` query string, in bytes.
    ///
    /// `None` uses the server's limit. `0` disables the limit. This is a denial-of-service guard,
    /// not part of the GraphQL over HTTP specification.
    pub max_body_bytes: Option<usize>,
    /// Override the maximum number of multipart fields, including `operations` and `map`.
    ///
    /// Only meaningful with the `multipart` feature. Defaults to
    /// `DEFAULT_MAX_MULTIPART_PARTS` (32). `0` disables the limit.
    pub max_multipart_parts: Option<usize>,
}

impl Default for HttpConfig {
    fn default() -> Self {
        Self {
            path: "/graphql".to_owned(),
            graphiql: true,
            max_body_bytes: None,
            max_multipart_parts: None,
        }
    }
}

impl HttpConfig {
    /// Mount at a different path.
    #[must_use]
    pub fn path(mut self, path: impl Into<String>) -> Self {
        let path = path.into();
        self.path = if path.is_empty() {
            "/".to_owned()
        } else if path.starts_with('/') {
            path
        } else {
            format!("/{path}")
        };
        self
    }

    /// Enable or disable GraphiQL.
    #[must_use]
    pub const fn graphiql(mut self, enabled: bool) -> Self {
        self.graphiql = enabled;
        self
    }

    /// Override the request body limit. `0` disables the limit.
    #[must_use]
    pub const fn max_body_bytes(mut self, bytes: usize) -> Self {
        self.max_body_bytes = Some(bytes);
        self
    }

    /// Override the multipart part limit. `0` disables the limit.
    #[must_use]
    pub const fn max_multipart_parts(mut self, parts: usize) -> Self {
        self.max_multipart_parts = Some(parts);
        self
    }
}

/// A fully buffered HTTP response.
#[derive(Clone, Debug)]
pub struct HttpResponse {
    /// The status code.
    pub status: StatusCode,
    /// Response headers, including `Content-Type`.
    pub headers: HeaderMap,
    /// The response body.
    pub body: Vec<u8>,
}

impl HttpResponse {
    /// An empty response with a status.
    #[must_use]
    pub fn new(status: StatusCode) -> Self {
        Self {
            status,
            headers: HeaderMap::new(),
            body: Vec::new(),
        }
    }

    /// A response with a body.
    #[must_use]
    pub fn with_body(
        status: StatusCode,
        content_type: &'static str,
        body: impl Into<Vec<u8>>,
    ) -> Self {
        let mut response = Self::new(status);
        response
            .headers
            .insert(header::CONTENT_TYPE, HeaderValue::from_static(content_type));
        response.body = body.into();
        response
    }
}

/// A streaming server-sent-events response.
///
/// Produced only by the opt-in `sse` feature.
pub struct SseResponse {
    /// The status code, normally `200 OK`.
    pub status: StatusCode,
    /// SSE headers.
    pub headers: HeaderMap,
    /// One byte chunk per SSE event.
    pub stream: BoxStream<'static, Result<Bytes, Infallible>>,
}

impl fmt::Debug for SseResponse {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("SseResponse")
            .field("status", &self.status)
            .field("headers", &self.headers)
            .finish_non_exhaustive()
    }
}

/// The result of [`handle`].
pub enum HttpResult {
    /// A buffered JSON, HTML or error response.
    Response(HttpResponse),
    /// An event stream for a subscription.
    Stream(SseResponse),
}

impl fmt::Debug for HttpResult {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Response(response) => f.debug_tuple("Response").field(response).finish(),
            Self::Stream(response) => f.debug_tuple("Stream").field(response).finish(),
        }
    }
}

impl From<HttpResponse> for HttpResult {
    fn from(value: HttpResponse) -> Self {
        Self::Response(value)
    }
}

/// Handle one GraphQL-over-HTTP request.
///
/// The caller supplies borrowed [`http::request::Parts`] and the already-read body. The extractor
/// is only invoked for requests that actually execute GraphQL (never for GraphiQL or for
/// method/content-type rejections), so it can safely consult `Authorization`, cookies and
/// middleware request extensions.
#[cfg_attr(
    feature = "tracing",
    tracing::instrument(
        skip_all,
        level = "info",
        name = "suku.request",
        fields(
            method = %parts.method,
            path = parts.uri.path(),
            body_bytes = body.len(),
            request_id = tracing::field::Empty,
            status = tracing::field::Empty,
        )
    )
)]
pub async fn handle<F, Fut>(
    server: &Server,
    config: &HttpConfig,
    parts: &::http::request::Parts,
    body: &[u8],
    extract_context: F,
) -> HttpResult
where
    F: FnOnce(ContextRequest) -> Fut + Send,
    Fut: Future<Output = Result<Context, HttpError>> + Send,
{
    let result = dispatch(
        server,
        config,
        parts,
        HttpBody::Borrowed(body),
        extract_context,
    )
    .await;
    record_status(&result);
    result
}

/// [`handle`] for a caller that already owns the body as [`Bytes`].
///
/// The Axum adapter reads the body into `Bytes`, so taking it here instead of a slice lets a
/// multipart request hand the same buffer to the parser; a slice would have to be copied first.
#[cfg(feature = "axum")]
pub(crate) async fn handle_bytes<F, Fut>(
    server: &Server,
    config: &HttpConfig,
    parts: &::http::request::Parts,
    body: Bytes,
    extract_context: F,
) -> HttpResult
where
    F: FnOnce(ContextRequest) -> Fut + Send,
    Fut: Future<Output = Result<Context, HttpError>> + Send,
{
    let result = dispatch(
        server,
        config,
        parts,
        HttpBody::Owned(body),
        extract_context,
    )
    .await;
    record_status(&result);
    result
}

async fn dispatch<F, Fut>(
    server: &Server,
    config: &HttpConfig,
    parts: &::http::request::Parts,
    body: HttpBody<'_>,
    extract_context: F,
) -> HttpResult
where
    F: FnOnce(ContextRequest) -> Fut + Send,
    Fut: Future<Output = Result<Context, HttpError>> + Send,
{
    let accept = accept::parse(&parts.headers);
    let result = match parts.method {
        Method::GET | Method::HEAD => {
            handle_get(server, config, parts, &accept, extract_context).await
        }
        Method::POST => handle_post(server, config, parts, body, &accept, extract_context).await,
        _ => method_not_allowed(server, &accept, "GET, HEAD, POST"),
    };

    // `HEAD` must return the same headers as `GET` with no body. This still executes a query from
    // the URI, which the specification requires to be safe.
    if parts.method == Method::HEAD {
        return without_body(result);
    }
    result
}

/// A request body that is either borrowed from the caller or already owned as [`Bytes`].
///
/// The public [`handle`] receives a slice and never copies it; the Axum adapter already owns the
/// body as `Bytes`. A multipart request needs an owned buffer, so only that path copies (and only
/// when it was borrowed).
pub(crate) enum HttpBody<'a> {
    Borrowed(&'a [u8]),
    #[cfg(feature = "axum")]
    Owned(Bytes),
}

impl HttpBody<'_> {
    fn as_slice(&self) -> &[u8] {
        match self {
            Self::Borrowed(body) => body,
            #[cfg(feature = "axum")]
            Self::Owned(body) => body,
        }
    }

    #[cfg(feature = "multipart")]
    fn into_bytes(self) -> Bytes {
        match self {
            Self::Borrowed(body) => Bytes::copy_from_slice(body),
            #[cfg(feature = "axum")]
            Self::Owned(body) => body,
        }
    }
}

fn without_body(result: HttpResult) -> HttpResult {
    match result {
        HttpResult::Response(mut response) => {
            response.body.clear();
            HttpResult::Response(response)
        }
        HttpResult::Stream(stream) => HttpResult::Response(HttpResponse {
            status: stream.status,
            headers: stream.headers,
            body: Vec::new(),
        }),
    }
}

#[cfg(feature = "tracing")]
fn record_status(result: &HttpResult) {
    let span = tracing::Span::current();
    if !span.is_disabled() {
        let status = match result {
            HttpResult::Response(response) => response.status,
            HttpResult::Stream(_) => StatusCode::OK,
        };
        span.record("status", status.as_u16());
    }
}

#[cfg(not(feature = "tracing"))]
fn record_status(_result: &HttpResult) {}

#[cfg(feature = "tracing")]
fn record_request_id(request: &GraphQLRequest) {
    if let Some(id) = request.prepared_id() {
        let span = tracing::Span::current();
        if !span.is_disabled() {
            span.record("request_id", id);
        }
    }
}

#[cfg(not(feature = "tracing"))]
fn record_request_id(_request: &GraphQLRequest) {}

async fn handle_get<F, Fut>(
    server: &Server,
    config: &HttpConfig,
    parts: &::http::request::Parts,
    accept: &Accept,
    extract_context: F,
) -> HttpResult
where
    F: FnOnce(ContextRequest) -> Fut + Send,
    Fut: Future<Output = Result<Context, HttpError>> + Send,
{
    if !accept.acceptable {
        return not_acceptable(server, accept);
    }

    // GET parameters are a request body too. Without this check a client could put an arbitrarily
    // large query in the URL and bypass `max_body_bytes`. The specification recommends `414 URI
    // Too Long` when the URI is too large and suggests using `POST` instead.
    let max_query_bytes = config
        .max_body_bytes
        .unwrap_or_else(|| server.max_body_bytes_limit());
    if max_query_bytes != 0 && parts.uri.query().map_or(0, str::len) > max_query_bytes {
        return request_error(
            server,
            accept,
            StatusCode::URI_TOO_LONG,
            format!(
                "Request URI exceeds the {max_query_bytes} byte limit; use POST for large documents."
            ),
            None,
        );
    }

    // Authentication runs before GraphiQL. The page is static, but an operator who protects the
    // endpoint expects the docs and its API surface to be protected too. The context itself is
    // discarded for the HTML response.
    let context = match extract_context(ContextRequest::from_parts(parts)).await {
        Ok(context) => context,
        Err(error) => return context_error_response(server, accept, error),
    };

    if config.graphiql && accept.prefers_html() {
        return HttpResult::Response(graphiql_response());
    }

    // Only HTML was acceptable, but GraphiQL is disabled. There is no representation left.
    if accept.json.is_none() && !accept.event_stream {
        return not_acceptable(server, accept);
    }

    let parameters = match GetParameters::parse(parts.uri.query()) {
        Ok(parameters) => parameters,
        Err(problem) => {
            return request_error(server, accept, problem.status, problem.message, None);
        }
    };
    let request = match parameters.into_request() {
        Ok(request) => request,
        Err(problem) => {
            return request_error(server, accept, problem.status, problem.message, None);
        }
    };

    if request.query.trim().is_empty() && request.prepared_id().is_none() {
        return request_error(
            server,
            accept,
            StatusCode::UNPROCESSABLE_ENTITY,
            "Missing required `query` parameter.",
            Some(&request),
        );
    }
    record_request_id(&request);

    // GraphQL over HTTP: GET may only execute queries. Allowing mutations here turns every
    // cookie-authenticated endpoint into a CSRF target. Subscriptions are a transport extension
    // that only this crate's `POST` + `text/event-stream` path supports.
    if let Ok(Some(kind)) = server.operation_kind(&request) {
        if matches!(
            kind,
            gql::ast::OperationKind::Mutation | gql::ast::OperationKind::Subscription
        ) {
            return method_not_allowed(server, accept, "POST");
        }
    }

    // `text/event-stream` is a POST-only extension.
    if accept.json.is_none() {
        return not_acceptable(server, accept);
    }

    let outcome = server.execute_outcome(&request, context).await;
    let status = outcome_status(&outcome);
    HttpResult::Response(json_response(status, accept, &outcome.response))
}

async fn handle_post<F, Fut>(
    server: &Server,
    config: &HttpConfig,
    parts: &::http::request::Parts,
    body: HttpBody<'_>,
    accept: &Accept,
    extract_context: F,
) -> HttpResult
where
    F: FnOnce(ContextRequest) -> Fut + Send,
    Fut: Future<Output = Result<Context, HttpError>> + Send,
{
    let content_type = parts.headers.get(header::CONTENT_TYPE);
    let media = request_media(content_type);

    #[cfg(feature = "multipart")]
    let multipart = media == RequestMedia::Multipart;
    #[cfg(not(feature = "multipart"))]
    #[allow(unused_variables)]
    let multipart = false;

    if media == RequestMedia::Unsupported {
        #[cfg(feature = "multipart")]
        let message =
            "POST requests must use `Content-Type: application/json` or `multipart/form-data`.";
        #[cfg(not(feature = "multipart"))]
        let message = "POST requests must use `Content-Type: application/json`.";
        return request_error(
            server,
            accept,
            StatusCode::UNSUPPORTED_MEDIA_TYPE,
            message,
            None,
        );
    }

    let max_body_bytes = config
        .max_body_bytes
        .unwrap_or_else(|| server.max_body_bytes_limit());
    if max_body_bytes != 0 && body.as_slice().len() > max_body_bytes {
        return request_error(
            server,
            accept,
            StatusCode::PAYLOAD_TOO_LARGE,
            format!("Request body exceeds the {max_body_bytes} byte limit."),
            None,
        );
    }

    let context = match extract_context(ContextRequest::from_parts(parts)).await {
        Ok(context) => context,
        Err(error) => return context_error_response(server, accept, error),
    };

    #[cfg(feature = "multipart")]
    let request = if multipart {
        let max_parts = config
            .max_multipart_parts
            .unwrap_or(crate::multipart::DEFAULT_MAX_MULTIPART_PARTS);
        let content_type = content_type
            .and_then(|value| value.to_str().ok())
            .unwrap_or_default();
        match crate::multipart::parse_bytes(content_type, body.into_bytes(), max_parts).await {
            Ok(request) => request,
            Err(error) => {
                return request_error(
                    server,
                    accept,
                    StatusCode::BAD_REQUEST,
                    error.message(),
                    None,
                );
            }
        }
    } else {
        match parse_json_request(body.as_slice()) {
            Ok(request) => request,
            Err(problem) => {
                return request_error(server, accept, problem.status, problem.message, None);
            }
        }
    };
    #[cfg(not(feature = "multipart"))]
    let request = match parse_json_request(body.as_slice()) {
        Ok(request) => request,
        Err(problem) => {
            return request_error(server, accept, problem.status, problem.message, None);
        }
    };

    if request.query.trim().is_empty() && request.prepared_id().is_none() {
        return request_error(
            server,
            accept,
            StatusCode::UNPROCESSABLE_ENTITY,
            "Missing required `query` parameter.",
            Some(&request),
        );
    }
    record_request_id(&request);

    let kind = server.operation_kind(&request);
    let subscription = matches!(&kind, Ok(Some(gql::ast::OperationKind::Subscription)));

    // A client that accepts only `text/event-stream` still gets a JSON request error when the
    // document cannot be parsed or validated: no stream is opened for an invalid request.
    #[cfg(feature = "sse")]
    if kind.is_err() && accept.event_stream {
        let outcome = server.execute_outcome(&request, context).await;
        let status = outcome_status(&outcome);
        return HttpResult::Response(json_response(status, accept, &outcome.response));
    }

    #[cfg(feature = "sse")]
    if subscription && accept.event_stream {
        return match server.subscribe_outcome(&request, context).await {
            Ok(subscription) => sse_response(subscription),
            Err(outcome) => {
                let status = outcome_status(&outcome);
                HttpResult::Response(json_response(status, accept, &outcome.response))
            }
        };
    }

    if subscription {
        #[cfg(feature = "sse")]
        let message = "Subscriptions require `Accept: text/event-stream`.";
        #[cfg(not(feature = "sse"))]
        let message =
            "Subscriptions are not supported over HTTP; enable the `sse` feature to serve them.";

        if accept.json.is_none() {
            return not_acceptable(server, accept);
        }
        return request_error(
            server,
            accept,
            StatusCode::UNPROCESSABLE_ENTITY,
            message,
            Some(&request),
        );
    }

    if accept.json.is_none() {
        return not_acceptable(server, accept);
    }

    let outcome = server.execute_outcome(&request, context).await;
    let status = outcome_status(&outcome);
    HttpResult::Response(json_response(status, accept, &outcome.response))
}

/// Map an execution outcome to the status code the specification recommends.
fn outcome_status(outcome: &ExecutionOutcome) -> StatusCode {
    let Some(failure) = outcome.failure else {
        return match outcome.response.data {
            Some(_) if outcome.response.has_errors() => partial_success_status(),
            Some(_) => StatusCode::OK,
            // A response with neither data nor a classified failure is a server bug; never
            // present it as success.
            None => StatusCode::INTERNAL_SERVER_ERROR,
        };
    };
    match failure {
        FailureKind::Parse => StatusCode::BAD_REQUEST,
        FailureKind::Request => StatusCode::UNPROCESSABLE_ENTITY,
        FailureKind::Timeout => StatusCode::GATEWAY_TIMEOUT,
        FailureKind::Server => StatusCode::INTERNAL_SERVER_ERROR,
    }
}

/// `294 Partial Success`, recommended when a response has both `data` and `errors`.
///
/// The specification notes that clients which do not recognise this code must treat it as the
/// `x00` of its class, i.e. like `200 OK`.
fn partial_success_status() -> StatusCode {
    StatusCode::from_u16(294).unwrap_or(StatusCode::OK)
}

#[cfg(feature = "sse")]
fn sse_response(subscription: gql::SubscriptionResponse) -> HttpResult {
    use futures::{StreamExt as _, stream};

    let events = subscription
        .stream
        .map(|response| -> Result<Bytes, Infallible> {
            #[cfg(feature = "tracing")]
            if tracing::enabled!(target: "suku::subscription", tracing::Level::TRACE) {
                tracing::trace!(
                    target: "suku::subscription",
                    errors = response.errors.len(),
                    has_data = response.data.is_some(),
                    "suku.subscription.event"
                );
            }
            let json = response.to_json_string();
            Ok(Bytes::from(format!("event: next\ndata: {json}\n\n")))
        })
        .chain(stream::once(async {
            Ok(Bytes::from_static(b"event: complete\n\n"))
        }));

    HttpResult::Stream(SseResponse {
        status: StatusCode::OK,
        headers: sse_headers(),
        stream: Box::pin(events),
    })
}

#[cfg_attr(
    feature = "tracing",
    tracing::instrument(
        skip_all,
        level = "debug",
        name = "suku.serialize",
        fields(status = status.as_u16(), errors = response.errors.len(), body_bytes = tracing::field::Empty)
    )
)]
fn json_response(status: StatusCode, accept: &Accept, response: &Response) -> HttpResponse {
    let mut http = HttpResponse::new(status);
    http.headers.insert(
        header::CONTENT_TYPE,
        HeaderValue::from_static(accept.json_content_type(status)),
    );
    http.headers
        .insert(header::VARY, HeaderValue::from_static("Accept"));
    http.headers
        .insert(header::CACHE_CONTROL, HeaderValue::from_static("no-store"));
    http.body = response.to_json_bytes();
    #[cfg(feature = "tracing")]
    {
        let span = tracing::Span::current();
        if !span.is_disabled() {
            span.record("body_bytes", http.body.len());
        }
    }
    http
}

fn request_error(
    server: &Server,
    accept: &Accept,
    status: StatusCode,
    message: impl Into<String>,
    request: Option<&GraphQLRequest>,
) -> HttpResult {
    #[cfg(feature = "tracing")]
    tracing::warn!(
        target: "suku::http",
        status = status.as_u16(),
        code = "REQUEST",
        "suku.request.error"
    );
    let mut response = crate::response::error_response(message);
    let meta = request.map_or_else(ResponseMeta::none, ResponseMeta::from_request);
    server.format_response(&mut response, meta, ErrorKind::Request);
    HttpResult::Response(json_response(status, accept, &response))
}

fn context_error_response(server: &Server, accept: &Accept, mut error: HttpError) -> HttpResult {
    #[cfg(feature = "tracing")]
    tracing::warn!(
        target: "suku::http",
        status = error.status.as_u16(),
        code = "CONTEXT",
        "suku.request.error"
    );
    server.format_response(
        &mut error.response,
        ResponseMeta::none(),
        ErrorKind::Request,
    );
    let mut http = json_response(error.status, accept, &error.response);
    http.headers.extend(error.headers);
    HttpResult::Response(http)
}

fn method_not_allowed(server: &Server, accept: &Accept, allow: &'static str) -> HttpResult {
    #[cfg(feature = "tracing")]
    tracing::warn!(
        target: "suku::http",
        status = StatusCode::METHOD_NOT_ALLOWED.as_u16(),
        code = "METHOD",
        "suku.request.error"
    );
    let mut response =
        crate::response::error_response(format!("Method not allowed. Supported methods: {allow}."));
    server.format_response(&mut response, ResponseMeta::none(), ErrorKind::Request);
    let mut http = json_response(StatusCode::METHOD_NOT_ALLOWED, accept, &response);
    http.headers
        .insert(header::ALLOW, HeaderValue::from_static(allow));
    HttpResult::Response(http)
}

fn not_acceptable(server: &Server, accept: &Accept) -> HttpResult {
    #[cfg(feature = "tracing")]
    tracing::warn!(
        target: "suku::http",
        status = StatusCode::NOT_ACCEPTABLE.as_u16(),
        code = "ACCEPT",
        "suku.request.error"
    );
    let mut response = crate::response::error_response(
        "No acceptable response type; use `application/graphql-response+json` or `application/json`.",
    );
    server.format_response(&mut response, ResponseMeta::none(), ErrorKind::Request);
    HttpResult::Response(json_response(StatusCode::NOT_ACCEPTABLE, accept, &response))
}

fn graphiql_response() -> HttpResponse {
    let mut http =
        HttpResponse::with_body(StatusCode::OK, "text/html; charset=utf-8", graphiql_html());
    http.headers
        .insert(header::VARY, HeaderValue::from_static("Accept"));
    http.headers
        .insert(header::CACHE_CONTROL, HeaderValue::from_static("no-store"));
    http
}

#[cfg(feature = "sse")]
fn sse_headers() -> HeaderMap {
    let mut headers = HeaderMap::new();
    headers.insert(
        header::CONTENT_TYPE,
        HeaderValue::from_static("text/event-stream; charset=utf-8"),
    );
    headers.insert(header::CACHE_CONTROL, HeaderValue::from_static("no-cache"));
    headers.insert(header::VARY, HeaderValue::from_static("Accept"));
    headers.insert(
        header::HeaderName::from_static("x-accel-buffering"),
        HeaderValue::from_static("no"),
    );
    headers
}

/// The media type of a `POST` body.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum RequestMedia {
    Json,
    #[cfg(feature = "multipart")]
    Multipart,
    Unsupported,
}

/// Parse the `Content-Type` header of a `POST` request.
///
/// The specification requires `application/json`; `application/graphql-response+json` is a
/// *response* media type and is not accepted as a request type. A missing header is unsupported
/// (the specification recommends rejecting it rather than guessing).
fn request_media(value: Option<&HeaderValue>) -> RequestMedia {
    let Some(value) = value.and_then(|value| value.to_str().ok()) else {
        return RequestMedia::Unsupported;
    };
    let mut parts = value.split(';');
    let media = parts.next().unwrap_or_default().trim();

    // An officially recognised GraphQL media type without an encoding is UTF-8. An explicit
    // non-UTF-8 charset is not something this server can decode.
    let mut utf8 = true;
    for parameter in parts {
        let Some((name, value)) = parameter.split_once('=') else {
            continue;
        };
        if name.trim().eq_ignore_ascii_case("charset") {
            let charset = value.trim().trim_matches('"');
            utf8 = charset.eq_ignore_ascii_case("utf-8")
                || charset.eq_ignore_ascii_case("utf8")
                || charset.eq_ignore_ascii_case("us-ascii");
        }
    }
    if !utf8 {
        return RequestMedia::Unsupported;
    }

    if media.eq_ignore_ascii_case("application/json") {
        RequestMedia::Json
    } else {
        #[cfg(feature = "multipart")]
        if media.eq_ignore_ascii_case("multipart/form-data") {
            return RequestMedia::Multipart;
        }
        RequestMedia::Unsupported
    }
}

#[cfg_attr(
    feature = "tracing",
    tracing::instrument(
        skip_all,
        level = "debug",
        name = "suku.parse",
        fields(body_bytes = body.len(), status = tracing::field::Empty)
    )
)]
fn parse_json_request(body: &[u8]) -> Result<GraphQLRequest, Problem> {
    serde_json::from_slice(body).map_err(|error| {
        // Malformed JSON is a `400`; valid JSON whose shape is not a GraphQL-over-HTTP request is
        // a `422`. This also rejects a top-level array: batching is not part of the specification.
        let status = match error.classify() {
            serde_json::error::Category::Data => StatusCode::UNPROCESSABLE_ENTITY,
            _ => StatusCode::BAD_REQUEST,
        };
        Problem {
            status,
            message: format!("Invalid GraphQL request: {error}"),
        }
    })
}

/// The `GET` parameters defined by the specification.
#[derive(Debug, Default)]
struct GetParameters {
    query: Option<String>,
    operation_name: Option<String>,
    variables: Option<String>,
    extensions: Option<String>,
}

impl GetParameters {
    /// Parse the query component as `application/x-www-form-urlencoded`.
    ///
    /// Unknown parameters are ignored, matching the rule that servers must ignore members they
    /// do not understand. Duplicate parameters keep the first value, like `URLSearchParams.get`.
    fn parse(query: Option<&str>) -> Result<Self, Problem> {
        let mut parameters = Self::default();
        for (key, value) in form_urlencoded::parse(query.unwrap_or_default().as_bytes()) {
            let slot = match &*key {
                "query" => &mut parameters.query,
                "operationName" => &mut parameters.operation_name,
                "variables" => &mut parameters.variables,
                "extensions" => &mut parameters.extensions,
                _ => continue,
            };
            slot.get_or_insert_with(|| value.into_owned());
        }
        Ok(parameters)
    }

    fn into_request(self) -> Result<GraphQLRequest, Problem> {
        Ok(GraphQLRequest {
            query: self.query.unwrap_or_default(),
            operation_name: self.operation_name.filter(|name| !name.is_empty()),
            variables: parse_json_map(self.variables.as_deref(), "variables")?,
            extensions: parse_json_map(self.extensions.as_deref(), "extensions")?,
            uploads: Vec::new(),
        })
    }
}

fn parse_json_map(
    raw: Option<&str>,
    name: &str,
) -> Result<Option<JsonMap<String, JsonValue>>, Problem> {
    let Some(raw) = raw else {
        return Ok(None);
    };
    // The specification says an empty optional parameter is equivalent to omitting it.
    if raw.is_empty() {
        return Ok(None);
    }
    let value: JsonValue = serde_json::from_str(raw).map_err(|error| Problem {
        status: StatusCode::BAD_REQUEST,
        message: format!("Invalid `{name}` parameter: {error}"),
    })?;
    match value {
        JsonValue::Null => Ok(None),
        JsonValue::Object(map) => Ok(Some(map)),
        _ => Err(Problem {
            status: StatusCode::UNPROCESSABLE_ENTITY,
            message: format!("`{name}` must be a JSON object."),
        }),
    }
}

/// A request problem with the status code the specification recommends for it.
struct Problem {
    status: StatusCode,
    message: String,
}
