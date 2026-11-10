# suku

`suku` is the HTTP server crate for [gurafi](../../README.md). It takes a `gql::Schema` and exposes
it over HTTP according to the [GraphQL over HTTP](https://graphql.github.io/graphql-over-http/)
draft, without depending on `gurafi` or any high-level schema builder.

## Design

The crate is split into three layers so that HTTP frameworks never leak into execution:

1. **`Server`** — owns the schema and per-operation defaults. `execute`, `execute_batch` and
   `subscribe` return `gql` values only.
2. **`suku::http`** — a single `handle` function that receives `http::request::Parts`, the body
   bytes and a context extractor. It produces either a buffered `HttpResponse` or an `SseResponse`
   with a byte stream. It has no dependency on Axum, Tokio or a socket.
3. **`suku::axum`** — a builder that turns a `Server` into an `axum::Router`, reads the body with a
   limit and adapts `HttpResult` to an Axum response.

This is also why `suku` can be tested without a live server: the transport tests call `handle`
directly, and the Axum tests use `tower::ServiceExt::oneshot`.

## Building a server

```rust,no_run
use suku::Server;

# fn build(schema: gql::Schema) {
let server = Server::new(schema)
    .introspection(true)
    .max_depth(15)
    .max_complexity(1_000)
    .max_errors(50)
    .max_body_bytes(512 * 1024)
    .max_batch_size(10)
    .batch_concurrency(8);
# let _ = server;
# }
```

| Option | Default | Meaning |
| --- | --- | --- |
| `introspection` | `true` | Allow `__schema` and `__type`. |
| `max_depth` | `32` | Maximum selection-set depth (parser hard cap is 96). |
| `max_complexity` | `100_000` | Maximum estimated operation cost. |
| `max_breadth` | `1_000` | Maximum distinct response keys in one selection set. |
| `max_errors` | `100` | Errors reported before execution stops adding them. |
| `max_body_bytes` | 1 MiB | HTTP body limit; `0` disables it. |
| `timeout` | `30s` | Wall-clock timeout for one operation or subscription setup. |
| `max_concurrency` | `16` | Sibling fields resolved concurrently per operation; `0` is unlimited. Mutations stay serial. |
| `max_batch_size` | `16` | Maximum operations in a batch; `0` disables it. |
| `batch_concurrency` | `16` | Operations executed concurrently in a batch. |

`Server` clones cheaply (one `Arc`), so it can be mounted in several routers or shared with
`Arc<Server>` without changing behaviour.

## Executing

```rust,no_run
use suku::{Context, GraphQLRequest, Server};

# async fn run(server: Server) {
let request = GraphQLRequest::new("query Greeting($name: String!) { hello(name: $name) }")
    .operation_name("Greeting")
    .variables(serde_json::json!({ "name": "Ada" }).as_object().cloned().expect("object"));

let response = server.execute(&request, Context::new("user-123")).await;
# let _ = response;
# }
```

`Context` wraps `Arc<dyn Any + Send + Sync>`. Use `Context::new(value)` for a typed context,
`Context::empty()` when a request has no user data, and `Context::from_arc` when a framework
already produces `Arc<dyn Any + Send + Sync>`.

`subscribe` returns `Result<gql::SubscriptionResponse, gql::Response>`. Setup errors arrive as a
response, exactly like `gql::subscribe`.

## HTTP behaviour

### Methods and content types

- `GET` reads `query`, `variables`, `operationName` and `extensions` from the query string.
  `variables` and `extensions` are JSON-encoded objects.
- `POST` requires `Content-Type: application/json` (the `charset` parameter is ignored). The body
  is either one request object or a non-empty JSON array for a batch.
- Any other method returns `405 Method Not Allowed` with `Allow: GET, POST`.
- A missing or non-JSON `Content-Type` returns `415 Unsupported Media Type`.

### Content negotiation

- `application/json` is the legacy response type: execution and request errors both use status
  200, except malformed requests (JSON, parameters, batches), which are 400.
- `application/graphql-response+json` is the modern response type. A response with `errors` and no
  `data` is a request error and gets status 400.
- If both are acceptable, the higher quality value wins; a tie prefers
  `application/graphql-response+json`.
- If nothing acceptable is listed, the server returns 406 with a legacy JSON error body.

Every GraphQL response carries `Vary: Accept` and `Cache-Control: no-store`.

### GraphiQL

`GET` with `Accept: text/html` serves a static GraphiQL page after the context extractor accepts
the request, so a protected endpoint does not expose its API surface anonymously. It uses the
request path as its endpoint, so it works at any mount point. Disable it with `.graphiql(false)`
(Axum builder) or `HttpConfig::graphiql(false)`.

`GET` may only execute **queries**. Mutations and subscriptions over GET are rejected with
`405 Method Not Allowed`, which prevents cross-site request forgery against cookie-authenticated
endpoints. Use `POST` for mutations and `POST` + `Accept: text/event-stream` for subscriptions.

GraphiQL itself makes requests with `Accept: */*`, so the page and the API can share one URL.

### Subscriptions

A subscription request with `Accept: text/event-stream` is executed with `gql::subscribe` and
streamed as server-sent events:

```
event: next
data: {"data":{"countdown":3}}

event: next
data: {"data":{"countdown":2}}

event: complete
```

Validation or setup errors are returned as a JSON response before the stream starts (status 400),
never as a broken stream. A subscription requested without `text/event-stream` receives a clear
GraphQL error; subscriptions cannot be batched.

### Limits

The transport enforces `max_body_bytes` before authentication and parsing. The Axum adapter also
passes the limit to `to_bytes`, so an oversized body is rejected with 413 without being buffered in
full.

Batch execution runs in chunks of `batch_concurrency` with `futures::future::join_all`, preserving
request order while bounding concurrency.

## Authentication

The transport never interprets `Authorization`, `Cookie` or any other header. Authentication is a
closure that receives a `ContextRequest`:

```rust,no_run
use suku::{Context, GraphQL, http::{ContextRequest, HttpError}};

# fn build(server: suku::Server) {
let app = GraphQL::new(server)
    .with_context(|request: ContextRequest| async move {
        let token = request
            .header("authorization")
            .and_then(|value| value.to_str().ok());

        match token {
            Some(value) if value.starts_with("Bearer ") => {
                Ok(Context::new(value.trim_start_matches("Bearer ").to_owned()))
            }
            _ => Err(HttpError::message(
                ::http::StatusCode::UNAUTHORIZED,
                "Missing bearer token",
            )),
        }
    })
    .into_router();
# let _ = app;
# }
```

`ContextRequest` is a cloneable snapshot containing the method, URI, headers, `http::Extensions`
and a `cookie(name)` helper. Returning `Err(HttpError)` short-circuits the request before the body
is parsed or a resolver runs; use `HttpError::unauthorized`, `forbidden`, `internal` or `new` to
choose the status.

### Authorization inside execution

Authentication answers *who* the caller is. For field-level authorization, install a
`gql::Authorizer` with `Server::authorizer`. It runs inside `gql` immediately before each resolver
and reports denied fields as GraphQL errors, so partial responses stay correct.

## Batches

A batch is a JSON array of requests. Empty arrays are request errors, arrays larger than the
configured limit are rejected, and each operation still gets its own response (including its own
variable-conversion errors). Batching subscriptions is not supported.

## Production notes

- The crate forbids `unsafe` and its request paths avoid `unwrap`/`expect`/panics.
- SSE is streamed chunk by chunk; the server never buffers an entire subscription.
- Errors exposed to clients are GraphQL messages, not Rust error chains.
- CORS, tracing, compression and TLS are intentionally left to the application's HTTP framework.
