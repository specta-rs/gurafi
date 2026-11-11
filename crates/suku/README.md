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

## Error formatting and masking

Resolver errors often carry details that are useful in logs but unsafe for clients: database
messages, filesystem paths, internal type names. `Server::format_error` installs one formatter
that runs over every error in every response, immediately before the response is returned or
streamed:

```rust,no_run
# fn build(schema: gql::Schema) {
use suku::{Server, mask_internal};

let server = Server::new(schema).format_error(|error, context| {
    if context.is_execution() && context.operation_name() == Some("AdminReport") {
        error.clone().with_message("Internal server error")
    } else {
        error.clone()
    }
});

// Or use the conservative built-in policy.
let server = Server::new(schema).format_error(mask_internal);
# let _ = server;
# }
```

The formatter receives an `ErrorContext` with:

| Accessor | Meaning |
| --- | --- |
| `kind()` / `is_request()` / `is_execution()` | Whether the error happened before execution or during it. |
| `operation_name()` | The requested operation name, or the prepared operation's name. |
| `request_id()` / `prepared_id()` | The persisted-query / prepared-query identifier, when the request carried one. |
| `path()` | The response path attached to the error, when it has one. |

The context never exposes the schema, resolver value, user context or other server internals.
`format_error_with(Arc<dyn ErrorFormatter>)` is available when the formatter is a named type or
shared with other endpoints.

Formatting is applied to single operations, every response in a batch (in request order),
persisted operations, subscription setup errors and every subscription event. It also covers
request errors raised by the HTTP layer, including malformed JSON, missing queries, extractor
denials and content-negotiation/method rejections. Transport-level body-read failures in the Axum
adapter are formatted too.

`mask_internal` has explicit rules:

* errors with `extensions.code` are left untouched (the code marks an intentionally public error);
* request errors are left untouched (parse, validation and coercion messages describe the
  caller's document);
* every other error becomes a fresh `"Internal server error"` with the original source
  locations and response path; extensions are **removed**, because resolvers commonly put stack
  traces and backend details there.

`gurafi` marks its own authorization and argument-validation errors with `UNAUTHORIZED` and
`BAD_USER_INPUT`, so those client-facing messages survive the mask.

A resolver that attaches an `extensions.code` to an internal error opts that message and its
extensions back in, so attach codes only to errors that are safe to expose. Use `format_error`
when the default policy is not enough.

Formatting runs synchronously and deterministically, once per error, in response order.
Formatters must be `Send + Sync + 'static`. A panic is not caught: it unwinds through the
execution future (or fails the subscription stream) exactly like a panic in a resolver.

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

## Persisted operations

Load a manifest once at startup. `PreparedQueries` auto-detects the JSON shapes emitted by Relay,
graphql-codegen, Apollo tooling, Hive, Envelop and urql:

```rust,no_run
use suku::{PreparedQueries, Server};

# fn build(schema: gql::Schema) -> Result<(), Box<dyn std::error::Error>> {
// Relay / graphql-codegen persisted-document map: { "id": "query text" }
let prepared = PreparedQueries::from_json_str(
    &schema,
    r#"{ "hello": "{ hello }" }"#,
)?;

// Hive / Envelop / GraphQL Codegen operation manifest.
let prepared = PreparedQueries::from_json_str(
    &schema,
    r#"{
        "operations": [
            { "id": "hello", "body": "{ hello }", "type": "query" }
        ]
    }"#,
)?;

let server = Server::new(schema).prepared_queries(prepared);
# let _ = server;
# Ok(())
# }
```

A request can then send only an identifier, in any of these shapes:

```json
{ "id": "hello" }
{ "doc_id": "hello" }
{ "documentId": "hello" }
{ "queryId": "hello" }
{ "extensions": { "persistedQuery": { "version": 1, "sha256Hash": "hello" } } }
```

`id`, `doc_id`, `documentId` and `queryId` are also accepted in `extensions`, and
`extensions.persistedQuery` accepts `sha256Hash`, `hash`, `id`, `queryId` or `documentId`.
Identifiers and hashes are opaque keys: a `sha256Hash` entry is not hash-verified. The
`X-Persisted-Query-Id` and `X-Query-Id` headers are a fallback used only when the request carries
no identifier in the body or extensions; for a batch they fill in every item without one. GET
requests may omit `query` when they send an identifier, including an `extensions.persistedQuery`
hash in the query string.

When an identifier matches a registered query, the registered text is executed and any supplied
query text is ignored, so a client cannot substitute a different document. The prepared operation
is fixed: a request `operationName` must match it, and a disagreeing name is an error rather than a
silent switch. An unknown identifier with supplied query text falls back to normal execution by
default; that fallback does not register the operation. A prepared registry is tied to the schema
instance it was prepared with, and installing it on a server for another schema is rejected at
execution. To reject arbitrary query text, make the registry an allowlist:

```rust,no_run
# fn build(schema: gql::Schema) {
let server = suku::Server::new(schema).require_prepared(true);
# let _ = server;
# }
```

In allowlist mode a missing or unknown identifier returns a GraphQL error with message
`PersistedQueryNotFound` and `extensions.code = "PERSISTED_QUERY_NOT_FOUND"`. With
`Accept: application/graphql-response+json` that request error uses HTTP 400, matching the
transport's normal request-error handling; legacy `application/json` keeps HTTP 200.

Prepared subscriptions are supported: setup reuses the parsed document and each source event reuses
it too, so a persisted subscription does not re-parse per event.

`PreparedQueries::from_map`, `from_operations` and `from_json_value` cover programmatic and
custom manifest loading. Invalid manifests fail at startup with the offending id, duplicate id or
query error.

## GraphiQL assets

By default the GraphiQL page loads React 18, ReactDOM 18 and GraphiQL from unpkg, so no assets are
shipped in the binary and the page always follows the upstream releases. Use the
`vendored-graphiql` feature when the server must run without outbound network access or when a
content security policy should not allow a CDN:

```toml
[dependencies]
suku = { version = "0.0.1", features = ["vendored-graphiql"] }
```

With the feature enabled, `suku` builds a self-contained GraphiQL page at compile time from the
pinned browser bundles in `crates/suku/src/http/vendor/` and serves it as one HTML response. There
are no asset routes, so `http::handle` and the Axum adapter behave the same way; the only request
the page makes is to the GraphQL endpoint it was served from. The default feature set is unchanged
and keeps using the CDN.

This is useful for air-gapped deployments, environments behind an egress proxy, and audits where
every script served must come from the application's own build. Scripts and styles are inline, so a
CSP for the page should permit inline script/style (for example `script-src 'unsafe-inline'` and
`style-src 'unsafe-inline'`, or hashes of the served content) while keeping `connect-src 'self'`.
The bundled GraphiQL stylesheet embeds its fonts as `data:` URIs, so `font-src data:` is the only
other source required; no CDN host is needed.

Versions are pinned and checksummed in `crates/suku/src/http/vendor/versions.txt`. To update them,
change the version constants in `crates/suku/src/http/vendor/update.sh`, run it, update
`versions.txt`, and run `cargo test -p suku --features vendored-graphiql`. The vendored files are
the unmodified MIT-licensed upstream production builds; their licenses are next to them. GraphiQL
4.x is used because it is the newest line that still publishes a UMD browser bundle.

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

## Observability

`suku` has an optional `tracing` feature; it also enables `gql/tracing`, so a request span is
followed by the parser/executor tree:

```toml
[dependencies]
suku = { version = "0.0.1", features = ["tracing"] }
```

`suku.request` is INFO and records method, path, body size, operation, request id, batch size and
status. Its children cover parsing, execution, serialization and subscriptions. Transport and
extractor failures are WARN events with a status and code. Bodies, headers, tokens and variable
values are never recorded. See [`docs/plugins/tracing.md`](../../docs/plugins/tracing.md) for the span
tree and the recommended `tracing-subscriber` setup.

## Tests

```sh
cargo test -p suku
```

The transport tests call `suku::http::handle` directly and the Axum tests use
`tower::ServiceExt::oneshot`, so no sockets are opened.
