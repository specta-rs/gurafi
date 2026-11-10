# suku

A production GraphQL-over-HTTP server built on the [`gql`](../gql) reference implementation.

`suku` owns the HTTP side of GraphQL: request parsing, content negotiation, batches, GraphiQL,
server-sent-event subscriptions, body limits and authentication hooks. It deliberately does not
reimplement execution; every operation is handed to `gql`.

## Layers

| Layer | Purpose |
| --- | --- |
| `Server` | Executes queries, mutations and subscriptions. No HTTP types. |
| `suku::http` | Transport-agnostic `handle` function, status codes, SSE, GraphiQL. |
| `suku::axum` | Optional (default) Axum router and authentication helper. |

## Mounting on Axum

```rust,no_run
use suku::{Context, GraphQL, http::{ContextRequest, HttpError}};

# fn build(schema: gql::Schema) {
let app = GraphQL::new(suku::Server::new(schema))
    .path("/graphql")
    .graphiql(true)
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

CORS, tracing and compression are ordinary Axum layers; compose them around the router.

## Server configuration

```rust,no_run
# fn build(schema: gql::Schema) {
use std::time::Duration;

let server = suku::Server::new(schema)
    .introspection(true)
    .max_depth(15)
    .max_complexity(1_000)
    .max_breadth(1_000)
    .max_errors(50)
    .max_body_bytes(512 * 1024)
    .timeout(Duration::from_secs(10))
    .max_concurrency(16)
    .max_batch_size(10)
    .batch_concurrency(8);
# let _ = server;
# }
```

`Server` is cheap to clone and safe to share. Wrap it in `Arc` only if that suits your
application; cloning is equivalent.

## Executing without HTTP

```rust,no_run
use suku::{Context, GraphQLRequest, Server};

# async fn run(server: Server) {
let variables = serde_json::json!({ "name": "Ada" });
let response = server
    .execute(
        &GraphQLRequest::new("query Greeting($name: String!) { hello(name: $name) }")
            .operation_name("Greeting")
            .variables(variables.as_object().cloned().unwrap_or_default()),
        Context::new("authenticated user"),
    )
    .await;
# let _ = response;
# }
```

`GraphQLRequest` converts JSON variables into `gql::Value`. An integer outside GraphQL's 32-bit
`Int` range becomes `gql::Value::BigInt` instead of failing the request, so a custom scalar can
receive it exactly and `Float`/`ID` can accept it. The built-in `Int` scalar still rejects it
during GraphQL variable coercion.

## Transport behaviour

| Request | Result |
| --- | --- |
| `POST application/json` (single object) | 200 with a GraphQL response |
| `POST application/json` (array) | 200 with an array of responses, executed with bounded concurrency |
| `GET` with `query`, `variables`, `operationName`, `extensions` | 200 with a GraphQL response |
| `GET` + `Accept: text/html` | GraphiQL (after authentication) |
| `GET` + mutation/subscription | 405; GET is query-only |
| `Accept: application/graphql-response+json` | Request errors use 400; `application/json` keeps 200 |
| `Accept: text/event-stream` + subscription | SSE: `event: next`, `data: …`, then `event: complete` |
| Unsupported method | 405 with `Allow: GET, POST` |
| Missing/unsupported `Content-Type` | 415 |
| No acceptable `Accept` | 406 |
| Body over the limit | 413 |
| Malformed JSON or invalid batch | 400 with `{"errors":[…]}` |

The `Authorization` header is never read by the transport. Authentication is always an
`ExtractContext` closure that receives a cloneable `ContextRequest` with the request method, URI,
headers, cookies and HTTP extensions.

## Empty batches and limits

Batches must be non-empty. `Server::max_batch_size` (or `HttpConfig::max_batch_size`) rejects
oversized arrays, and `Server::batch_concurrency` bounds how many operations run at once.

## Tests

```sh
cargo test -p suku
```

The transport tests call `suku::http::handle` directly and the Axum tests use
`tower::ServiceExt::oneshot`, so no sockets are opened.
