//! Transport-agnostic GraphQL-over-HTTP handling.
//!
//! [`handle`] implements the [GraphQL over HTTP](https://graphql.github.io/graphql-over-http/)
//! draft for one request: content negotiation, GET and POST parameters, request body limits,
//! status codes, GraphiQL and server-sent event subscriptions. It never touches a socket, so the
//! same logic backs the Axum integration and can be embedded in any HTTP framework.
//!
//! The `Authorization` header (and every other request header) is never interpreted here. Pass an
//! [`ExtractContext`] callback to authenticate a request and build the per-request
//! [`Context`].

mod accept;

use core::convert::Infallible;
use std::{fmt, future::Future};

use ::http::{HeaderMap, HeaderValue, Method, StatusCode, header};
use bytes::Bytes;
use futures::{
    StreamExt as _,
    future::BoxFuture,
    stream::{self, BoxStream},
};
use gql::{GraphQLError, Response};
use serde::Deserialize;
use serde_json::{Map as JsonMap, Value as JsonValue};

use crate::{
    Context, Server,
    error::{ErrorKind, ResponseMeta},
    request::{GraphQLRequest, RequestError, RequestPayload},
    response::ResponseExt as _,
};

use self::accept::{Accept, JsonMedia};

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
    /// Override the server's request body limit.
    pub max_body_bytes: Option<usize>,
    /// Override the server's batch size limit.
    pub max_batch_size: Option<usize>,
}

impl Default for HttpConfig {
    fn default() -> Self {
        Self {
            path: "/graphql".to_owned(),
            graphiql: true,
            max_body_bytes: None,
            max_batch_size: None,
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

    /// Override the batch size limit. `0` disables the limit.
    #[must_use]
    pub const fn max_batch_size(mut self, size: usize) -> Self {
        self.max_batch_size = Some(size);
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
            batch_size = tracing::field::Empty,
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
    let accept = accept::parse(&parts.headers);
    let result = match parts.method {
        Method::GET => handle_get(server, config, parts, &accept, extract_context).await,
        Method::POST => handle_post(server, config, parts, body, &accept, extract_context).await,
        _ => HttpResult::Response(method_not_allowed(server)),
    };
    #[cfg(feature = "tracing")]
    {
        let span = tracing::Span::current();
        if !span.is_disabled() {
            let status = match &result {
                HttpResult::Response(response) => response.status,
                HttpResult::Stream(_) => StatusCode::OK,
            };
            span.record("status", status.as_u16());
        }
    }
    result
}

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
    // GET parameters are a request body too. Without this check a client could put an arbitrarily
    // large query in the URL and bypass `max_body_bytes`.
    let max_query_bytes = config
        .max_body_bytes
        .unwrap_or_else(|| server.max_body_bytes_limit());
    if max_query_bytes != 0 && parts.uri.query().map_or(0, str::len) > max_query_bytes {
        return request_error(
            server,
            accept,
            StatusCode::PAYLOAD_TOO_LARGE,
            format!("Request query string exceeds the {max_query_bytes} byte limit."),
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

    if config.graphiql && accept.html {
        return HttpResult::Response(graphiql_response());
    }

    let parameters =
        match serde_urlencoded::from_str::<GetParameters>(parts.uri.query().unwrap_or_default()) {
            Ok(parameters) => parameters,
            Err(error) => {
                return request_error(
                    server,
                    accept,
                    StatusCode::BAD_REQUEST,
                    format!("Invalid query string: {error}"),
                    None,
                );
            }
        };

    let GetParameters {
        query,
        id,
        variables,
        operation_name,
        extensions,
    } = parameters;

    let mut request = match get_request(query, id, variables, operation_name, extensions) {
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
    };
    apply_header_id(&mut request, &parts.headers);

    // The gate runs on the constructed request, not on the raw parameters: an Apollo automatic
    // persisted-query GET carries its identifier only inside `extensions.persistedQuery`, and a
    // header can supply it too. Both are visible through `prepared_id()`.
    if request.query.trim().is_empty() && request.prepared_id().is_none() {
        return request_error(
            server,
            accept,
            StatusCode::BAD_REQUEST,
            "Missing required `query` parameter.",
            Some(&request),
        );
    }

    // GraphQL over HTTP: GET may only execute queries. Allowing mutations here turns every
    // cookie-authenticated endpoint into a CSRF target; subscriptions also open long-lived
    // streams that should require an explicit POST. A persisted identifier is resolved without
    // parsing supplied text; an unknown identifier falls through and becomes an error response.
    if matches!(
        server.operation_kind(&request),
        Ok(Some(
            gql::ast::OperationKind::Mutation | gql::ast::OperationKind::Subscription
        ))
    ) {
        return HttpResult::Response(method_not_allowed(server));
    }

    process_payload(server, accept, RequestPayload::Single(request), context).await
}

async fn handle_post<F, Fut>(
    server: &Server,
    config: &HttpConfig,
    parts: &::http::request::Parts,
    body: &[u8],
    accept: &Accept,
    extract_context: F,
) -> HttpResult
where
    F: FnOnce(ContextRequest) -> Fut + Send,
    Fut: Future<Output = Result<Context, HttpError>> + Send,
{
    if !is_json_content_type(parts.headers.get(header::CONTENT_TYPE)) {
        return request_error(
            server,
            accept,
            StatusCode::UNSUPPORTED_MEDIA_TYPE,
            "POST requests must use `Content-Type: application/json`.",
            None,
        );
    }

    let max_body_bytes = config
        .max_body_bytes
        .unwrap_or_else(|| server.max_body_bytes_limit());
    if max_body_bytes != 0 && body.len() > max_body_bytes {
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

    let max_batch_size = config
        .max_batch_size
        .unwrap_or_else(|| server.max_batch_size_limit());
    let payload = match RequestPayload::from_json_slice(body, max_batch_size) {
        Ok(payload) => payload,
        Err(error) => {
            return request_error(
                server,
                accept,
                StatusCode::BAD_REQUEST,
                error.message(),
                None,
            );
        }
    };
    let mut payload = payload;
    apply_header_id_to_payload(&mut payload, &parts.headers);

    process_payload(server, accept, payload, context).await
}

#[cfg_attr(
    feature = "tracing",
    tracing::instrument(
        skip_all,
        level = "debug",
        name = "suku.process",
        fields(
            batch_size = match &payload {
                RequestPayload::Single(_) => 1usize,
                RequestPayload::Batch(requests) => requests.len(),
            },
            operation = match &payload {
                RequestPayload::Single(request) => request.operation_name.as_deref().unwrap_or("<anonymous>"),
                RequestPayload::Batch(_) => "<batch>",
            },
            request_id = match &payload {
                RequestPayload::Single(request) => request.prepared_id().unwrap_or(""),
                RequestPayload::Batch(_) => "",
            },
        )
    )
)]
async fn process_payload(
    server: &Server,
    accept: &Accept,
    payload: RequestPayload,
    context: Context,
) -> HttpResult {
    match payload {
        RequestPayload::Single(request) => {
            let kind = server.operation_kind(&request);
            if accept.event_stream {
                if matches!(&kind, Ok(Some(gql::ast::OperationKind::Subscription))) {
                    return start_subscription(server, accept, &request, context).await;
                }
                if let (Err(error), None) = (&kind, accept.json) {
                    // `text/event-stream` is the only acceptable type. A malformed document or an
                    // unknown persisted identifier is still a request error, returned as JSON
                    // before any stream is opened.
                    let mut response = Response::error(error.clone());
                    server.format_response(
                        &mut response,
                        ResponseMeta::from_request(&request),
                        ErrorKind::Request,
                    );
                    return HttpResult::Response(json_response(
                        StatusCode::BAD_REQUEST,
                        accept,
                        &response,
                    ));
                }
            }

            if matches!(&kind, Ok(Some(gql::ast::OperationKind::Subscription))) {
                let mut response = crate::response::error_response(
                    "Subscriptions require `Accept: text/event-stream`.",
                );
                server.format_response(
                    &mut response,
                    ResponseMeta::from_request(&request),
                    ErrorKind::Request,
                );
                let status = execution_status(&response, accept.json_or_legacy());
                return HttpResult::Response(json_response(status, accept, &response));
            }

            if accept.json.is_none() {
                return not_acceptable(server);
            }

            let response = server.execute(&request, context).await;
            let status = execution_status(&response, accept.json_or_legacy());
            HttpResult::Response(json_response(status, accept, &response))
        }
        RequestPayload::Batch(requests) => {
            if accept.event_stream
                && requests.iter().any(|request| {
                    matches!(
                        server.operation_kind(request),
                        Ok(Some(gql::ast::OperationKind::Subscription))
                    )
                })
            {
                return request_error(
                    server,
                    accept,
                    StatusCode::BAD_REQUEST,
                    "Subscriptions cannot be batched.",
                    None,
                );
            }

            if accept.json.is_none() {
                return not_acceptable(server);
            }

            let responses = server.execute_batch(&requests, context).await;
            HttpResult::Response(json_batch_response(accept, &responses))
        }
    }
}

async fn start_subscription(
    server: &Server,
    accept: &Accept,
    request: &GraphQLRequest,
    context: Context,
) -> HttpResult {
    match server.subscribe(request, context).await {
        Ok(subscription) => sse_response(subscription),
        Err(response) => {
            HttpResult::Response(json_response(StatusCode::BAD_REQUEST, accept, &response))
        }
    }
}

fn sse_response(subscription: gql::SubscriptionResponse) -> HttpResult {
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

fn execution_status(response: &Response, media: JsonMedia) -> StatusCode {
    // Request errors (parse, validation, variable coercion) have no response path. Field errors
    // always do, even when non-null propagation nulls the entire `data` object; those are
    // execution results and keep status 200.
    if media == JsonMedia::GraphQLResponse
        && response.data.is_none()
        && response.has_errors()
        && response.errors.iter().all(|error| error.path().is_none())
    {
        StatusCode::BAD_REQUEST
    } else {
        StatusCode::OK
    }
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
        HeaderValue::from_static(accept.json_or_legacy().content_type()),
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

#[cfg_attr(
    feature = "tracing",
    tracing::instrument(
        skip_all,
        level = "debug",
        name = "suku.serialize_batch",
        fields(responses = responses.len(), body_bytes = tracing::field::Empty)
    )
)]
fn json_batch_response(accept: &Accept, responses: &[Response]) -> HttpResponse {
    // Serialize each response independently so one malformed value cannot turn the whole batch
    // into an opaque error.
    let values: Vec<serde_json::Value> = responses
        .iter()
        .map(|response| response.to_json())
        .collect();
    let body = match serde_json::to_vec(&values) {
        Ok(body) => body,
        Err(error) => crate::response::error_json_bytes(format!(
            "Failed to serialize the GraphQL batch response: {error}"
        )),
    };

    let mut http = HttpResponse::new(StatusCode::OK);
    http.headers.insert(
        header::CONTENT_TYPE,
        HeaderValue::from_static(accept.json_or_legacy().content_type()),
    );
    http.headers
        .insert(header::VARY, HeaderValue::from_static("Accept"));
    http.headers
        .insert(header::CACHE_CONTROL, HeaderValue::from_static("no-store"));
    http.body = body;
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

fn method_not_allowed(server: &Server) -> HttpResponse {
    #[cfg(feature = "tracing")]
    tracing::warn!(
        target: "suku::http",
        status = StatusCode::METHOD_NOT_ALLOWED.as_u16(),
        code = "METHOD",
        "suku.request.error"
    );
    let mut response = crate::response::error_response("Only GET and POST are supported.");
    server.format_response(&mut response, ResponseMeta::none(), ErrorKind::Request);
    let mut http = HttpResponse::with_body(
        StatusCode::METHOD_NOT_ALLOWED,
        "application/json; charset=utf-8",
        response.to_json_bytes(),
    );
    http.headers
        .insert(header::ALLOW, HeaderValue::from_static("GET, POST"));
    http
}

fn not_acceptable(server: &Server) -> HttpResult {
    #[cfg(feature = "tracing")]
    tracing::warn!(
        target: "suku::http",
        status = StatusCode::NOT_ACCEPTABLE.as_u16(),
        code = "ACCEPT",
        "suku.request.error"
    );
    let mut response = crate::response::error_response(
        "No acceptable response type; use `application/json` or `application/graphql-response+json`.",
    );
    server.format_response(&mut response, ResponseMeta::none(), ErrorKind::Request);
    HttpResult::Response(HttpResponse::with_body(
        StatusCode::NOT_ACCEPTABLE,
        "application/json; charset=utf-8",
        response.to_json_bytes(),
    ))
}

fn graphiql_response() -> HttpResponse {
    let mut http =
        HttpResponse::with_body(StatusCode::OK, "text/html; charset=utf-8", graphiql_html());
    http.headers
        .insert(header::CACHE_CONTROL, HeaderValue::from_static("no-store"));
    http
}

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

fn is_json_content_type(value: Option<&HeaderValue>) -> bool {
    let Some(value) = value else {
        return false;
    };
    let Ok(value) = value.to_str() else {
        return false;
    };
    let media = value.split(';').next().unwrap_or_default().trim();
    media.eq_ignore_ascii_case("application/json")
        || media.eq_ignore_ascii_case("application/graphql-response+json")
}

#[derive(Debug, Deserialize)]
struct GetParameters {
    query: Option<String>,
    #[serde(default, alias = "doc_id", alias = "documentId", alias = "queryId")]
    id: Option<String>,
    variables: Option<String>,
    #[serde(rename = "operationName")]
    operation_name: Option<String>,
    extensions: Option<String>,
}

fn get_request(
    query: Option<String>,
    id: Option<String>,
    variables: Option<String>,
    operation_name: Option<String>,
    extensions: Option<String>,
) -> Result<GraphQLRequest, RequestError> {
    Ok(GraphQLRequest {
        query: query.unwrap_or_default(),
        id,
        operation_name,
        variables: parse_json_object(variables, "variables")?,
        extensions: parse_json_object(extensions, "extensions")?,
    })
}

/// Headers some persisted-query clients use instead of a body field.
const PERSISTED_QUERY_HEADERS: [&str; 2] = ["x-persisted-query-id", "x-query-id"];

fn header_persisted_id(headers: &HeaderMap) -> Option<String> {
    PERSISTED_QUERY_HEADERS.iter().find_map(|name| {
        headers
            .get(*name)
            .and_then(|value| value.to_str().ok())
            .map(str::trim)
            .filter(|value| !value.is_empty())
            .map(str::to_owned)
    })
}

/// Apply a persisted-query header when the request did not carry an identifier anywhere.
///
/// The body's `id` and the extension shapes (`extensions.id`,
/// `extensions.persistedQuery.sha256Hash`, …) both take precedence: the header is a fallback, not
/// an override. Applying it only when [`GraphQLRequest::prepared_id`] is `None` is what prevents a
/// stale or shared header from silently changing which operation a request executes.
fn apply_header_id(request: &mut GraphQLRequest, headers: &HeaderMap) {
    if request.prepared_id().is_none() {
        if let Some(id) = header_persisted_id(headers) {
            request.id = Some(id);
        }
    }
}

fn apply_header_id_to_payload(payload: &mut RequestPayload, headers: &HeaderMap) {
    let Some(id) = header_persisted_id(headers) else {
        return;
    };
    match payload {
        RequestPayload::Single(request) => {
            if request.prepared_id().is_none() {
                request.id = Some(id);
            }
        }
        RequestPayload::Batch(requests) => {
            // A header is shared by the whole batch, so it applies to every item that does not
            // already name its own operation. Batch size is capped by `max_batch_size`, and each
            // item is executed with its own limits, so this fans one identifier out to at most
            // that many operations; send per-item identifiers if that is not desired.
            for request in requests {
                if request.prepared_id().is_none() {
                    request.id = Some(id.clone());
                }
            }
        }
    }
}

fn parse_json_object(
    raw: Option<String>,
    name: &str,
) -> Result<Option<JsonMap<String, JsonValue>>, RequestError> {
    let Some(raw) = raw else {
        return Ok(None);
    };
    if raw.trim().is_empty() {
        return Ok(None);
    }
    let parsed: Option<JsonMap<String, JsonValue>> = serde_json::from_str(&raw)
        .map_err(|error| RequestError::new(format!("Invalid `{name}` parameter: {error}")))?;
    Ok(parsed)
}
