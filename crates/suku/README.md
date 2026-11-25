# suku

A GraphQL over HTTP server built on the [`gql`](../gql) reference implementation, implementing the
[GraphQL over HTTP specification](https://http-spec.graphql.org/September2026/) (September 2026
edition).

`suku` owns the HTTP side of GraphQL: request parsing, `Accept` negotiation, the specification's
status codes, GraphiQL, body limits and authentication hooks. It deliberately does not reimplement
execution; every operation is handed to `gql`. Subscriptions (`sse`), WebSocket (`ws`) and file
uploads (`multipart`) are opt-in extensions beyond the specification.

## Layers

| Layer | Purpose |
| --- | --- |
| `Server` | Executes queries, mutations and subscriptions. No HTTP types. |
| `suku::http` | Transport-agnostic `handle` function, status codes, GraphiQL, optional SSE and multipart. |
| `suku::axum` | Axum router and authentication helper (default feature). |
| `suku::ws` | Framework-neutral `graphql-transport-ws` connection driver (opt-in). |

## Feature flags

The specification is always implemented. Everything beyond it is opt-in.

| Feature | Default | Purpose |
| --- | --- | --- |
| `axum` | yes | Axum router and `ExtractContext` integration. |
| `sse` | no | Server-sent-event subscriptions (`text/event-stream`). A transport extension. |
| `ws` | no | `graphql-transport-ws` WebSocket subscriptions; implies `axum`. |
| `multipart` | no | GraphQL multipart requests (file uploads); uses `multer`. |
| `tracing` | no | Request spans, and `gql/tracing`. |

```toml
# The specification's HTTP endpoint.
suku = { version = "0.1.2" }

# Add server-sent-event subscriptions.
suku = { version = "0.1.2", features = ["sse"] }

# Add uploads and WebSocket subscriptions.
suku = { version = "0.1.2", features = ["multipart", "ws"] }
```

At runtime, `GraphQL::websocket(false)` disables upgrades on one mount without rebuilding. With
the `ws` feature off, or the switch off, an upgrade request is handled like any other `GET`
(usually GraphiQL or a `422` request error), never as a socket.

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

## WebSocket subscriptions

A `GET` with `Upgrade: websocket` at the GraphQL path is served as a
[`graphql-transport-ws`](https://github.com/enisdenjo/graphql-ws/blob/master/PROTOCOL.md)
connection, alongside the normal `GET`/`POST` handling on the same path. The client must offer the
`graphql-transport-ws` subprotocol; the legacy `subscriptions-transport-ws` protocol is not
implemented, so a mismatched client fails the upgrade with `400` instead of silently
misinterpreting messages.

```text
client → connection_init
server → connection_ack
client → subscribe { id: "1", payload: { query: "subscription { giraffeBorn }" } }
server → next      { id: "1", payload: { data: … } }   (once per event)
server → complete  { id: "1" }
```

Queries and mutations are supported on the socket too: they emit one `next` and then `complete`.
Parse, validation, coercion, authorization and limit failures emit `error` (an array of GraphQL
errors) and no `complete`. A client may send `complete` to cancel; the server stops the stream and
sends nothing further for that id. JSON `ping`/`pong` messages are echoed.

**Choose WebSocket or SSE.** SSE (`Accept: text/event-stream` on `POST`) is a plain HTTP response:
it traverses proxies and CDNs that block upgrades, reconnects with ordinary HTTP semantics, and is
the simplest choice for stream-only clients. WebSocket multiplexes many operations over one
connection, supports client cancellation and is bidirectional, but requires the proxy to forward
`Upgrade`/`Connection` headers and to allow long-lived idle connections. Both transports execute
through the same `Server`, so limits and error formatting cannot drift between them.

**Authentication is per connection.** The extractor runs once against the upgrade request
(`Authorization`, cookies, middleware extensions) before the socket is accepted. Every operation
on that connection uses the same `Context`; nothing in `connection_init` can widen it. Use a
`gql::Authorizer` or check expiry inside resolvers when credentials can change mid-connection.

WebSocket upgrades are not subject to a CORS preflight, and browsers attach cookies to the
handshake. If cookie authentication is enabled, validate the `Origin` header (or require a token)
in the extractor; otherwise a hostile page can open a socket and issue mutations with the user's
cookies. The HTTP-only query restriction for `GET` does not protect a WebSocket connection.

Depth, complexity, breadth, timeout and concurrency limits apply exactly as over HTTP.
`WsConfig` adds `connection_init_timeout` (default 10s), `max_operations` per connection (default
32) and `max_message_bytes` (defaults to `max_body_bytes`):

```rust,no_run
# fn build(server: suku::Server) {
use std::time::Duration;
use suku::GraphQL;

let app = GraphQL::new(server)
    .ws_config(
        suku::ws::WsConfig::default()
            .connection_init_timeout(Some(Duration::from_secs(5)))
            .max_operations(16),
    )
    .into_router();
# let _ = app;
# }
```

## Multipart uploads

With the `multipart` feature, `POST multipart/form-data` follows the
[GraphQL multipart request specification](https://github.com/jaydenseric/graphql-multipart-request-spec).
The fields must appear in order: `operations`, `map`, then one file part per map entry.

```sh
curl http://localhost:3000/graphql \
  -F operations='{ "query": "mutation ($file: Upload!) { upload(file: $file) { id } }", "variables": { "file": null } }' \
  -F map='{ "0": ["variables.file"] }' \
  -F 0=@a.txt
```

`operations` is a single request object; batching is not part of the specification and is not
supported. One file may be mapped to several paths; the bytes are shared, not copied. Every mapped
destination must already exist in `variables` and be `null`, which is what prevents arbitrary JSON
writes. Only paths rooted at `variables` are accepted.

Files become `gql::Upload` values (re-exported as `suku::Upload`) inserted as
`gql::Value::External`. Execution stays transport-free; a custom scalar owns the bytes. In raw
`gql`:

```rust,no_run
# fn build(builder: &mut gql::SchemaBuilder) {
let upload = gql::ScalarType::new("Upload")
    .description("A file from a multipart request.")
    .parse_value(|value| {
        if value.as_external::<gql::Upload>().is_some() {
            Ok(value.clone())
        } else {
            Err("Upload must come from a multipart request".to_owned())
        }
    });
builder.add_type(upload.into());
# }
```

A resolver downcasts the argument:

```rust,no_run
use gql::{GraphQLError, ResolverContext, Upload, Value};

# fn resolve(context: &ResolverContext<'_>) -> Result<Value, GraphQLError> {
let upload = context
    .args
    .get("file")
    .and_then(Value::as_external::<Upload>)
    .ok_or_else(|| GraphQLError::new("Expected an upload"))?;
# let _ = (upload.filename(), upload.content_type(), upload.bytes());
# Ok(Value::Null)
# }
```

In `gurafi`, wrap it in a newtype scalar so the type can appear in derived input objects:

```rust
use gurafi::gql::{GraphQLError, Upload, Value};
use gurafi::{ScalarValue, impl_scalar_traits};

#[derive(Clone)]
pub struct UploadFile(pub Upload);

impl ScalarValue for UploadFile {
    const NAME: &'static str = "Upload";

    fn to_value(&self) -> Result<Value, GraphQLError> {
        Err(GraphQLError::new("Upload is input-only"))
    }

    fn from_value(value: &Value) -> Result<Self, GraphQLError> {
        value
            .as_external::<Upload>()
            .cloned()
            .map(Self)
            .ok_or_else(|| GraphQLError::new("Expected a multipart file upload"))
    }
}

impl_scalar_traits!(UploadFile);
```

Then use `UploadFile` in an input struct or `schema.scalar::<UploadFile>()`. The resolver receives
the typed value; `filename()`, `content_type()` and `bytes()` are on `gql::Upload`. The client
supplied filename and content type are metadata, never trusted as a filesystem path.

The whole body is buffered, so uploads are bounded by `max_body_bytes` (1 MiB by default) and
`max_multipart_parts` (32 by default, counting `operations` and `map`). Raise both for real file
uploads. Authentication runs before the parts are parsed. A multipart body that exceeds the limit
is `413`; malformed JSON, a bad `map`, an unknown or missing file, or a non-`null` destination is
`400`. Ordinary `application/json` POSTs are unaffected.

## Deployment notes

- **WebSocket proxies** must forward `Upgrade` and `Connection` and allow idle connections at
  least as long as the longest subscription. If a proxy closes after 60 seconds, long
  subscriptions die; use its WebSocket/idle timeout rather than a request timeout.
- **SSE** should bypass response buffering. `suku` already sends `X-Accel-Buffering: no`; some CDNs
  still need the route configured as streaming.
- **Uploads** are read into memory, so set the application limit *and* the reverse proxy limit
  (`client_max_body_size` in nginx, equivalent elsewhere) to the same value. Leave headroom for
  the JSON fields and multipart boundaries.
- **Timeouts**: `Server::timeout` bounds each operation, subscription setup and each subscription
  event. Waiting for the next source event does not consume the event timeout, so a long or quiet
  subscription can outlive it; a proxy read timeout should close idle sockets.
- Every connection is independent. Authenticate at the upgrade, then rely on
  `Server::authorizer` for field-level checks that must run per operation.

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

Formatting is applied to single operations, every response from `execute_batch` (in request
order), persisted operations, subscription setup errors and every subscription event. It also covers
request errors raised by the HTTP layer, including malformed JSON, multipart fields, missing
queries, extractor denials and content-negotiation/method rejections, and WebSocket `error`
messages and `next` payloads. Transport-level body-read failures in the Axum adapter are formatted
too.

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

A request can then send only an identifier. The specification reserves `extensions` for
implementer-defined data, so the identifier lives there:

```json
{ "extensions": { "persistedQuery": { "id": "hello" } } }
{ "extensions": { "persistedQuery": { "version": 1, "sha256Hash": "hello" } } }
```

`id` is this crate's shape; `sha256Hash` is Apollo's automatic persisted query spelling. Both are
opaque keys: a `sha256Hash` entry is not hash-verified. There is no custom header fallback. GET
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
`PersistedQueryNotFound` and `extensions.code = "PERSISTED_QUERY_NOT_FOUND"`. An unresolved
identifier is a request error and uses HTTP `422 Unprocessable Content`.

Prepared subscriptions are supported: setup reuses the parsed document and each source event reuses
it too, so a persisted subscription does not re-parse per event.

`PreparedQueries::from_map`, `from_operations` and `from_json_value` cover programmatic and
custom manifest loading. Invalid manifests fail at startup with the offending id, duplicate id or
query error.

## GraphiQL assets

By default the GraphiQL page loads pinned React 18.3.1, ReactDOM 18.3.1 and GraphiQL 4.1.2 from unpkg, so no assets are
shipped in the binary and the page always follows the upstream releases. Use the
`vendored-graphiql` feature when the server must run without outbound network access or when a
content security policy should not allow a CDN:

```toml
[dependencies]
suku = { version = "0.1.2", features = ["vendored-graphiql"] }
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

| Request / result | Response |
| --- | --- |
| `POST application/json` with a request object | `2xx` with a GraphQL response |
| `POST application/json` with an array | `422`; batching is not specified |
| `GET` with `query`, `variables`, `operationName`, `extensions` | `2xx` with a GraphQL response |
| `GET`/`HEAD` + `Accept: text/html` | GraphiQL (after authentication) |
| `GET` + mutation or subscription | `405` with `Allow: POST` |
| `data` + no `errors` | `200 OK` |
| `data` + `errors` | `294 Partial Success` |
| Parse failure / validation, coercion or operation-selection failure | `400` / `422` |
| Unresolved persisted identifier | `422` |
| `Accept: text/event-stream` + subscription (`sse`) | SSE: `event: next`, `data: …`, then `event: complete` |
| `GET` + `Upgrade: websocket` (`ws`) | `graphql-transport-ws`; queries, mutations and subscriptions over one socket |
| `POST multipart/form-data` (`multipart`) | GraphQL multipart request; files become `suku::Upload` values |
| Unsupported method | `405` with `Allow: GET, HEAD, POST` |
| Missing/unsupported `Content-Type` | `415` |
| No acceptable `Accept` | `406` |
| Body over the limit / URI too long | `413` / `414` |
| Malformed JSON | `400` with `{"errors":[…]}` |

A request that accepts only `application/json` receives `Content-Type: application/json` on `2xx`
responses, while non-`2xx` responses keep `application/graphql-response+json`, as the
specification recommends.

The `Authorization` header is never read by the transport. Authentication is always an
`ExtractContext` closure that receives a cloneable `ContextRequest` with the request method, URI,
headers, cookies and HTTP extensions.

## Limits

The HTTP endpoint accepts one operation per request. `max_body_bytes` bounds the `POST` body and
the `GET` query string; oversized requests use `413` and `414` respectively. `Server::execute_batch`
remains available for in-process callers and bounds concurrency with `batch_concurrency`.

## Observability

`suku` has an optional `tracing` feature; it also enables `gql/tracing`, so a request span is
followed by the parser/executor tree:

```toml
[dependencies]
suku = { version = "0.1.2", features = ["tracing"] }
```

`suku.request` is INFO and records method, path, body size, request id and status. Its children cover parsing, execution, serialization and subscriptions. Transport and
extractor failures are WARN events with a status and code. Bodies, headers, tokens and variable
values are never recorded. See [`docs/plugins/tracing.md`](../../docs/plugins/tracing.md) for the span
tree and the recommended `tracing-subscriber` setup.

## Tests

```sh
cargo test -p suku --all-features
```

The transport tests call `suku::http::handle` directly, the WebSocket protocol tests drive
`suku::ws::serve` over in-memory channels, and the Axum tests use `tower::ServiceExt::oneshot`.
One WebSocket test opens a real local socket to verify the upgrade and subprotocol negotiation.
