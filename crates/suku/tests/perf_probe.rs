//! Allocation and latency probe for the WebSocket and multipart transports.
//!
//! The WebSocket numbers are end-to-end: one connection is driven over in-memory channels and
//! every message it emits is drained, so the probe includes parsing, dispatch, serialization and
//! the per-operation task. Per-event and per-operation costs are the difference between two run
//! sizes, which removes the fixed setup cost (connection, schema, channels).
//!
//! ```sh
//! cargo test -q -p suku --release --test perf_probe -- --test-threads=1 --nocapture
//! cargo test -q -p suku --release --test perf_probe -- --test-threads=1 --ignored --nocapture
//! ```
//!
//! The counting allocator is global to this test binary and Tokio's own allocations are included,
//! so every measuring test holds [`MEASUREMENT`].

#![cfg(all(feature = "ws", feature = "multipart"))]

use std::{
    alloc::{GlobalAlloc, Layout, System},
    hint::black_box,
    sync::{
        Mutex, MutexGuard,
        atomic::{AtomicUsize, Ordering},
    },
    time::Instant,
};

use futures::StreamExt as _;
use gql::{Field, InputValue, ObjectType, Schema, TypeRef, Value};
use serde_json::json;
use suku::{
    Context, Server,
    ws::{self, WsConfig, WsInput, WsOutput},
};

/// Serializes the measuring tests: the counters below are process-wide.
static MEASUREMENT: Mutex<()> = Mutex::new(());

fn measuring() -> MutexGuard<'static, ()> {
    MEASUREMENT
        .lock()
        .unwrap_or_else(|error| error.into_inner())
}

struct CountingAllocator;

static ALLOCATIONS: AtomicUsize = AtomicUsize::new(0);

unsafe impl GlobalAlloc for CountingAllocator {
    unsafe fn alloc(&self, layout: Layout) -> *mut u8 {
        ALLOCATIONS.fetch_add(1, Ordering::Relaxed);
        unsafe { System.alloc(layout) }
    }

    unsafe fn alloc_zeroed(&self, layout: Layout) -> *mut u8 {
        ALLOCATIONS.fetch_add(1, Ordering::Relaxed);
        unsafe { System.alloc_zeroed(layout) }
    }

    unsafe fn dealloc(&self, ptr: *mut u8, layout: Layout) {
        unsafe { System.dealloc(ptr, layout) }
    }

    unsafe fn realloc(&self, ptr: *mut u8, layout: Layout, new_size: usize) -> *mut u8 {
        ALLOCATIONS.fetch_add(1, Ordering::Relaxed);
        unsafe { System.realloc(ptr, layout, new_size) }
    }
}

#[global_allocator]
static ALLOCATOR: CountingAllocator = CountingAllocator;

fn schema() -> Schema {
    let hello = Field::new("hello", TypeRef::string().non_null())
        .resolve(|_| Box::pin(async { Ok(Value::from("world")) }));

    let points = ObjectType::new("Point")
        .field(Field::new("x", TypeRef::int().non_null()))
        .field(Field::new("y", TypeRef::int().non_null()));

    let events = Field::new(
        "events",
        TypeRef::named("Point").non_null().list().non_null(),
    )
    .arg(InputValue::new("count", TypeRef::int().non_null()))
    .subscribe(|context| {
        let count = context
            .args
            .get("count")
            .and_then(Value::as_i32)
            .unwrap_or_default()
            .max(0) as usize;
        Box::pin(async move {
            let stream = futures::stream::iter((0..count).map(|index| {
                Ok(Value::Object(std::sync::Arc::new(
                    gql::Object::from_fields([
                        ("x", Value::Int(index as i32)),
                        ("y", Value::Int(index as i32 * 2)),
                    ]),
                )))
            }));
            Ok(stream.boxed())
        })
    })
    .resolve(|context| Box::pin(async move { Ok(context.parent.clone()) }));

    let query = ObjectType::new("Query").field(hello);
    let subscription = ObjectType::new("Subscription").field(events);
    let mut builder = Schema::builder();
    builder.add_type(points.into());
    builder.add_type(query.into());
    builder.add_type(subscription.into());
    builder
        .query("Query")
        .subscription("Subscription")
        .build()
        .expect("probe schema")
}

/// The fixed (single-connection) setup the probes reuse.
struct Harness {
    input: tokio::sync::mpsc::UnboundedSender<WsInput>,
    output: tokio::sync::mpsc::Receiver<WsOutput>,
    task: tokio::task::JoinHandle<()>,
}

impl Harness {
    async fn start() -> Self {
        let (input, mut receiver) = tokio::sync::mpsc::unbounded_channel();
        let incoming = futures::stream::poll_fn(move |context| receiver.poll_recv(context));
        let (outgoing, output) = tokio::sync::mpsc::channel(1_024);
        let task = tokio::spawn(ws::serve(
            Server::new(schema()),
            WsConfig::default().max_operations(0),
            Context::empty(),
            incoming,
            outgoing,
        ));
        let mut harness = Self {
            input,
            output,
            task,
        };
        harness.send(json!({ "type": "connection_init" }));
        assert_eq!(harness.next().await["type"], "connection_ack");
        harness
    }

    fn send(&self, message: serde_json::Value) {
        self.input
            .send(WsInput::Text(message.to_string()))
            .expect("send");
    }

    async fn next(&mut self) -> serde_json::Value {
        match self.output.recv().await.expect("server message") {
            WsOutput::Text(text) => serde_json::from_str(&text).expect("JSON"),
            WsOutput::Close { code, reason } => panic!("closed {code}: {reason}"),
        }
    }

    /// Drain one operation's `next` (if any) plus its terminating message.
    async fn drain_operation(&mut self) {
        loop {
            let message = self.next().await;
            match message["type"].as_str() {
                Some("complete" | "error") => return,
                Some("next") => {}
                other => panic!("unexpected message type {other:?}"),
            }
        }
    }
}

impl Drop for Harness {
    fn drop(&mut self) {
        self.task.abort();
    }
}

/// Run `count` independent query operations and drain their output.
async fn query_run(count: usize) {
    let mut harness = Harness::start().await;
    for index in 0..count {
        harness.send(json!({
            "id": format!("q{index}"),
            "type": "subscribe",
            "payload": { "query": "{ hello }" },
        }));
        harness.drain_operation().await;
    }
}

/// Run one subscription operation yielding `events` events.
async fn subscription_run(events: usize) {
    let mut harness = Harness::start().await;
    harness.send(json!({
        "id": "s1",
        "type": "subscribe",
        "payload": {
            "query": "subscription S($count: Int!) { events(count: $count) { x y } }",
            "variables": { "count": events },
        },
    }));
    harness.drain_operation().await;
}

/// Round-trip `count` JSON pings.
async fn ping_run(count: usize) {
    let mut harness = Harness::start().await;
    for _ in 0..count {
        harness.send(json!({ "type": "ping" }));
        assert_eq!(harness.next().await["type"], "pong");
    }
}

fn runtime() -> tokio::runtime::Runtime {
    tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .expect("runtime")
}

/// Per-unit allocations and nanoseconds for a workload measured at two sizes.
///
/// The delta removes the fixed per-connection setup (schema, channels, handshake) so the numbers
/// describe one operation, event or message.
fn per_unit(
    runtime: &tokio::runtime::Runtime,
    small: usize,
    large: usize,
    run: impl Fn(usize) -> futures::future::BoxFuture<'static, ()>,
) -> (u128, f64) {
    // Warm both paths first so one-off allocations are not attributed to the delta.
    runtime.block_on(run(small));
    runtime.block_on(run(large));

    let mut best_ns = u128::MAX;
    let mut allocs = 0f64;
    for _ in 0..3 {
        let before = ALLOCATIONS.load(Ordering::Relaxed);
        let started = Instant::now();
        runtime.block_on(run(large));
        let elapsed = started.elapsed().as_nanos();
        let large_allocs = ALLOCATIONS.load(Ordering::Relaxed) - before;

        let before = ALLOCATIONS.load(Ordering::Relaxed);
        let started = Instant::now();
        runtime.block_on(run(small));
        let small_elapsed = started.elapsed().as_nanos();
        let small_allocs = ALLOCATIONS.load(Ordering::Relaxed) - before;

        let units = (large - small) as u128;
        allocs = ((large_allocs - small_allocs) as f64) / (units as f64);
        best_ns = best_ns.min(elapsed.saturating_sub(small_elapsed) / units);
    }
    (best_ns, allocs)
}

#[test]
#[ignore = "timing probe; run alone with --ignored"]
fn websocket_operations() {
    let _measuring = measuring();
    let runtime = runtime();

    let (query_ns, query_allocs) = per_unit(&runtime, 20, 120, |count| Box::pin(query_run(count)));
    let (event_ns, event_allocs) = per_unit(&runtime, 20, 1_020, |count| {
        Box::pin(subscription_run(count))
    });
    let (ping_ns, ping_allocs) = per_unit(&runtime, 20, 120, |count| Box::pin(ping_run(count)));

    eprintln!(
        "ws query_ns={query_ns} query_allocs={query_allocs} event_ns={event_ns} event_allocs={event_allocs} ping_ns={ping_ns} ping_allocs={ping_allocs}"
    );
}

// ---- multipart -------------------------------------------------------------------------------

const BOUNDARY: &str = "probe-boundary";

fn multipart_body(files: usize, file_bytes: usize) -> (String, Vec<u8>) {
    let mut body = Vec::new();
    let mut operations = String::from(
        r#"{"query":"mutation($f: Upload!) { upload(file: $f) }","variables":{"f":null}}"#,
    );
    let mut map = String::from(r#"{"0":["variables.f"]}"#);
    if files > 1 {
        operations = format!(
            r#"{{"query":"mutation($f: [Upload!]!) {{ uploadMany(files: $f) }}","variables":{{"f":[{}]}}}}"#,
            std::iter::repeat_n("null", files)
                .collect::<Vec<_>>()
                .join(",")
        );
        map = format!(
            r#"{{{}}}"#,
            (0..files)
                .map(|index| format!(r#""{index}":["variables.f.{index}"]"#))
                .collect::<Vec<_>>()
                .join(",")
        );
    }

    push_part(&mut body, "operations", None, None, operations.as_bytes());
    push_part(&mut body, "map", None, None, map.as_bytes());
    for index in 0..files {
        push_part(
            &mut body,
            &index.to_string(),
            Some("file.bin"),
            Some("application/octet-stream"),
            &vec![b'x'; file_bytes],
        );
    }
    body.extend_from_slice(format!("--{BOUNDARY}--\r\n").as_bytes());
    (format!("multipart/form-data; boundary={BOUNDARY}"), body)
}

fn push_part(
    body: &mut Vec<u8>,
    name: &str,
    filename: Option<&str>,
    content_type: Option<&str>,
    bytes: &[u8],
) {
    body.extend_from_slice(b"--");
    body.extend_from_slice(BOUNDARY.as_bytes());
    body.extend_from_slice(b"\r\nContent-Disposition: form-data; name=\"");
    body.extend_from_slice(name.as_bytes());
    body.extend_from_slice(b"\"");
    if let Some(filename) = filename {
        body.extend_from_slice(b"; filename=\"");
        body.extend_from_slice(filename.as_bytes());
        body.extend_from_slice(b"\"");
    }
    body.extend_from_slice(b"\r\n");
    if let Some(content_type) = content_type {
        body.extend_from_slice(b"Content-Type: ");
        body.extend_from_slice(content_type.as_bytes());
        body.extend_from_slice(b"\r\n");
    }
    body.extend_from_slice(b"\r\n");
    body.extend_from_slice(bytes);
    body.extend_from_slice(b"\r\n");
}

async fn parse_multipart(content_type: String, body: Vec<u8>) {
    let payload = suku::multipart::parse(&content_type, &body, 32, 10)
        .await
        .expect("parse");
    black_box(payload);
}

#[test]
#[ignore = "timing probe; run alone with --ignored"]
fn multipart_parsing() {
    let _measuring = measuring();
    let runtime = runtime();

    for (label, files, bytes) in [
        ("one-small", 1, 64),
        ("one-1mib", 1, 1 << 20),
        ("four", 4, 4_096),
    ] {
        let (content_type, body) = multipart_body(files, bytes);
        let before = ALLOCATIONS.load(Ordering::Relaxed);
        let started = Instant::now();
        runtime.block_on(parse_multipart(content_type.clone(), body.clone()));
        let elapsed = started.elapsed();
        let allocs = ALLOCATIONS.load(Ordering::Relaxed) - before;

        // Time the parse alone, with the body already in hand.
        let mut best = u128::MAX;
        for _ in 0..20 {
            let started = Instant::now();
            runtime.block_on(parse_multipart(content_type.clone(), body.clone()));
            best = best.min(started.elapsed().as_nanos());
        }
        eprintln!(
            "multipart {label} allocs={allocs} ns={best} body_bytes={} first_us={}",
            body.len(),
            elapsed.as_micros()
        );
    }
}

#[test]
fn multipart_allocations_are_reported() {
    let _measuring = measuring();
    let runtime = runtime();
    let (content_type, body) = multipart_body(1, 64);
    let before = ALLOCATIONS.load(Ordering::Relaxed);
    runtime.block_on(parse_multipart(content_type, body));
    let allocs = ALLOCATIONS.load(Ordering::Relaxed) - before;
    eprintln!("multipart one-small allocs={allocs}");
}

// ---- Axum request path -----------------------------------------------------------------------

#[test]
#[ignore = "timing probe; run alone with --ignored"]
fn axum_multipart_upload() {
    use ::http::{Request, header};
    use axum::body::Body;
    use tower::ServiceExt as _;

    let _measuring = measuring();
    let runtime = runtime();
    let app = suku::axum::GraphQL::new(Server::new(schema())).into_router();
    let (content_type, body) = multipart_body(1, 1 << 19);

    let request = || {
        Request::builder()
            .method("POST")
            .uri("/graphql")
            .header(header::CONTENT_TYPE, &content_type)
            .header(header::ACCEPT, "application/json")
            .body(Body::from(body.clone()))
            .expect("request")
    };

    for _ in 0..3 {
        let response = runtime
            .block_on(app.clone().oneshot(request()))
            .expect("response");
        assert_eq!(response.status(), 200);
    }

    let before = ALLOCATIONS.load(Ordering::Relaxed);
    let started = Instant::now();
    let response = runtime
        .block_on(app.clone().oneshot(request()))
        .expect("response");
    let elapsed = started.elapsed().as_nanos();
    let allocs = ALLOCATIONS.load(Ordering::Relaxed) - before;
    assert_eq!(response.status(), 200);
    eprintln!(
        "axum multipart-512kib allocs={allocs} ns={elapsed} body_bytes={}",
        body.len()
    );
}

// ---- JSON variable conversion ----------------------------------------------------------------

/// A request whose variables exercise the recursive JSON-to-value conversion.
fn variable_request(count: usize) -> suku::GraphQLRequest {
    use serde_json::Map;

    let mut variables = Map::new();
    for index in 0..count {
        variables.insert(
            format!("v{index}"),
            json!({
                "name": format!("item {index}"),
                "count": index,
                "flags": [true, false],
                "nested": { "value": index as f64 + 0.5, "empty": null },
            }),
        );
    }
    suku::GraphQLRequest {
        query: String::from("query Q { hello }"),
        id: None,
        operation_name: None,
        variables: Some(variables),
        extensions: None,
        uploads: Vec::new(),
    }
}

#[test]
fn variables_conversion_allocations() {
    let _measuring = measuring();
    for count in [1usize, 100] {
        let request = variable_request(count);
        let before = ALLOCATIONS.load(Ordering::Relaxed);
        let object = request.variables_object().expect("variables");
        let allocs = ALLOCATIONS.load(Ordering::Relaxed) - before;
        assert_eq!(object.map(|object| object.len()), Some(count));
        eprintln!("request_variables variables={count} allocs={allocs}");
    }
}

// ---- HTTP batch ------------------------------------------------------------------------------

#[test]
fn http_batch_allocations() {
    use ::http::Request;

    let _measuring = measuring();
    let runtime = runtime();
    let server = Server::new(schema());

    for count in [1usize, 100] {
        let operations = std::iter::repeat_n(r#"{"query":"{ hello }"}"#, count)
            .collect::<Vec<_>>()
            .join(",");
        let body = format!("[{operations}]");
        let parts = {
            let request = Request::builder()
                .method("POST")
                .uri("/graphql")
                .header(::http::header::CONTENT_TYPE, "application/json")
                .header(::http::header::ACCEPT, "application/json")
                .body(())
                .expect("request");
            request.into_parts().0
        };

        let before = ALLOCATIONS.load(Ordering::Relaxed);
        let result = runtime.block_on(suku::http::handle(
            &server,
            &suku::HttpConfig::default().max_batch_size(1_000),
            &parts,
            body.as_bytes(),
            |_request| async { Ok(Context::empty()) },
        ));
        let allocs = ALLOCATIONS.load(Ordering::Relaxed) - before;
        match result {
            suku::HttpResult::Response(response) => assert_eq!(response.status, 200),
            suku::HttpResult::Stream(_) => panic!("expected a buffered response"),
        }
        eprintln!("http_batch operations={count} allocs={allocs}");
    }
}
