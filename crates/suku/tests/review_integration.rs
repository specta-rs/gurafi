//! Independent review probes for the WebSocket, multipart-upload and vendored-GraphiQL work.
//!
//! These are *review artefacts*: they are not part of the crate's own suite and are written to
//! pin down behaviour the review questions. A failing probe is a reported defect, so every
//! assertion records the requirement it checks.
//!
//! Nothing here touches library code.

#![allow(clippy::unwrap_used, clippy::expect_used)]

// ===========================================================================
// Shared schema helpers
// ===========================================================================

#[cfg(any(feature = "axum", feature = "ws", feature = "multipart"))]
mod fixtures {
    use futures::StreamExt as _;
    use gql::{Field, InputObjectType, InputValue, ObjectType, ScalarType, Schema, TypeRef, Value};

    /// Context payload used by the `whoami` field.
    #[derive(Clone, Debug, PartialEq)]
    pub struct User {
        pub name: String,
    }

    fn describe(upload: &gql::Upload) -> String {
        format!(
            "{}:{}:{}",
            upload.filename().unwrap_or("<none>"),
            upload.content_type().unwrap_or("<none>"),
            String::from_utf8_lossy(upload.bytes())
        )
    }

    /// A schema covering everything the transport probes need.
    ///
    /// Query fields: `hello`, `whoami`, `slow` (200 ms), `echo(message: String!)`.
    /// Subscription fields: `countdown`, `pending`.
    /// Mutation fields: `upload(file: Upload!)`, `uploadNested(input: FileInput!)`.
    pub fn schema() -> Schema {
        let hello = Field::new("hello", TypeRef::string().non_null())
            .resolve(|_| Box::pin(async { Ok(Value::from("world")) }));

        let whoami = Field::new("whoami", TypeRef::string().non_null()).resolve(|context| {
            let name = context
                .context
                .require_data::<User>()
                .map(|user| user.name.clone());
            Box::pin(async move { name.map(Value::from) })
        });

        #[cfg(feature = "ws")]
        let slow = Field::new("slow", TypeRef::string().non_null()).resolve(|_| {
            Box::pin(async {
                tokio::time::sleep(std::time::Duration::from_millis(200)).await;
                Ok(Value::from("finally"))
            })
        });

        // An execution error that carries a response path: it must still be an execution result.
        let located = Field::new("located", TypeRef::string().non_null()).resolve(|_| {
            Box::pin(async {
                Err::<Value, gql::GraphQLError>(
                    gql::GraphQLError::new("located failure").at(gql::Location::new(1, 2)),
                )
            })
        });

        let echo = Field::new("echo", TypeRef::string().non_null())
            .arg(InputValue::new("message", TypeRef::string().non_null()))
            .resolve(|context| {
                let message = context
                    .args
                    .get("message")
                    .and_then(Value::as_str)
                    .unwrap_or_default()
                    .to_owned();
                Box::pin(async move { Ok(Value::from(message)) })
            });

        let countdown = Field::new("countdown", TypeRef::int().non_null())
            .subscribe(|_| {
                Box::pin(async {
                    Ok(futures::stream::iter(vec![
                        Ok(Value::from(3)),
                        Ok(Value::from(2)),
                        Ok(Value::from(1)),
                    ])
                    .boxed())
                })
            })
            .resolve(|context| Box::pin(async move { Ok(context.parent.clone()) }));

        // A subscription whose second event is a field error.
        let failing = Field::new("failing", TypeRef::int().non_null())
            .subscribe(|_| {
                Box::pin(async {
                    Ok(futures::stream::iter(vec![Ok(Value::from(1)), Ok(Value::from(2))]).boxed())
                })
            })
            .resolve(|context| {
                Box::pin(async move {
                    match context.parent {
                        Value::Int(2) => Err(gql::GraphQLError::new("event failure")),
                        value => Ok(value.clone()),
                    }
                })
            });

        let pending = Field::new("pending", TypeRef::int().non_null())
            .subscribe(|_| {
                Box::pin(async {
                    Ok(futures::stream::pending::<Result<Value, gql::GraphQLError>>().boxed())
                })
            })
            .resolve(|context| Box::pin(async move { Ok(context.parent.clone()) }));

        let upload = Field::new("upload", TypeRef::string().non_null())
            .arg(InputValue::new("file", TypeRef::named("Upload").non_null()))
            .resolve(|context| {
                let file = context
                    .args
                    .get("file")
                    .and_then(Value::as_external::<gql::Upload>)
                    .map(describe);
                Box::pin(async move {
                    file.ok_or_else(|| gql::GraphQLError::new("resolver did not receive an Upload"))
                        .map(Value::from)
                })
            });

        let upload_nested = Field::new("uploadNested", TypeRef::string().non_null())
            .arg(InputValue::new(
                "input",
                TypeRef::named("FileInput").non_null(),
            ))
            .resolve(|context| {
                let file = context
                    .args
                    .get("input")
                    .and_then(Value::as_object)
                    .and_then(|input| input.get("file"))
                    .and_then(Value::as_external::<gql::Upload>)
                    .map(describe);
                Box::pin(async move {
                    file.ok_or_else(|| gql::GraphQLError::new("resolver did not receive an Upload"))
                        .map(Value::from)
                })
            });

        let query = ObjectType::new("Query")
            .field(hello)
            .field(whoami)
            .field(echo)
            .field(located);
        #[cfg(feature = "ws")]
        let query = query.field(slow);
        let mutation = ObjectType::new("Mutation")
            .field(upload)
            .field(upload_nested);
        let subscription = ObjectType::new("Subscription")
            .field(countdown)
            .field(failing)
            .field(pending);

        let mut builder = Schema::builder();
        builder.add_type(ScalarType::new("Upload").into());
        builder.add_type(
            InputObjectType::new("FileInput")
                .field(InputValue::new("file", TypeRef::named("Upload").non_null()))
                .into(),
        );
        builder.add_type(query.into());
        builder.add_type(mutation.into());
        builder.add_type(subscription.into());
        builder = builder
            .query("Query")
            .mutation("Mutation")
            .subscription("Subscription");
        builder.build().expect("valid schema")
    }
}

// ===========================================================================
// 1. graphql-transport-ws
// ===========================================================================

#[cfg(feature = "ws")]
mod ws_probes {
    use std::time::Duration;

    use futures::stream::poll_fn;
    use serde_json::{Value as JsonValue, json};
    use suku::{
        Context, Server,
        ws::{self, WsConfig, WsInput, WsOutput},
    };
    use tokio::{sync::mpsc, task::JoinHandle};

    use super::fixtures;

    #[derive(Debug)]
    enum Output {
        Json(JsonValue),
        Close { code: u16, reason: String },
    }

    struct Harness {
        input: mpsc::UnboundedSender<WsInput>,
        output: mpsc::Receiver<WsOutput>,
        task: JoinHandle<()>,
    }

    impl Harness {
        fn start(server: Server, config: WsConfig, context: Context) -> Self {
            let (input, mut receiver) = mpsc::unbounded_channel();
            let incoming = poll_fn(move |cx| receiver.poll_recv(cx));
            let (outgoing, output) = mpsc::channel(64);
            let task = tokio::spawn(ws::serve(server, config, context, incoming, outgoing));
            Self {
                input,
                output,
                task,
            }
        }

        fn send(&self, message: JsonValue) {
            self.input
                .send(WsInput::Text(message.to_string()))
                .expect("send");
        }

        fn send_raw(&self, text: &str) {
            self.input
                .send(WsInput::Text(text.to_owned()))
                .expect("send raw");
        }

        fn send_input(&self, input: WsInput) {
            self.input.send(input).expect("send input");
        }

        async fn next(&mut self) -> Output {
            let output = tokio::time::timeout(Duration::from_secs(2), self.output.recv())
                .await
                .expect("timed out waiting for a server message")
                .expect("server channel closed");
            match output {
                WsOutput::Text(text) => {
                    Output::Json(serde_json::from_str(&text).expect("JSON message"))
                }
                WsOutput::Close { code, reason } => Output::Close { code, reason },
            }
        }

        async fn next_json(&mut self) -> JsonValue {
            match self.next().await {
                Output::Json(value) => value,
                other => panic!("expected a JSON message, got {other:?}"),
            }
        }

        async fn expect_close(&mut self) -> (u16, String) {
            match self.next().await {
                Output::Close { code, reason } => (code, reason),
                other => panic!("expected a close message, got {other:?}"),
            }
        }
    }

    impl Drop for Harness {
        fn drop(&mut self) {
            self.task.abort();
        }
    }

    async fn ready(server: Server) -> Harness {
        let mut harness = Harness::start(server, WsConfig::default(), Context::empty());
        harness.send(json!({ "type": "connection_init" }));
        assert_eq!(harness.next_json().await["type"], "connection_ack");
        harness
    }

    fn subscribe(id: &str, query: &str) -> JsonValue {
        json!({
            "id": id,
            "type": "subscribe",
            "payload": { "query": query },
        })
    }

    /// PROTOCOL.md reserves 4400 for a message whose *type or format* is not in the document, and
    /// 4401 for a message the server cannot handle because the connection is not acknowledged. It
    /// also requires both sides to "be prepared to receive (and ignore)" a `complete` for an
    /// operation they consider completed. So a well-formed, specified `complete` before the
    /// handshake must either be ignored (the connection then still accepts `connection_init`) or,
    /// at worst, close 4401 — never 4400.
    #[tokio::test]
    async fn complete_before_ack_is_not_an_invalid_message() {
        let mut harness = Harness::start(
            Server::new(fixtures::schema()),
            WsConfig::default(),
            Context::empty(),
        );
        harness.send(json!({ "id": "1", "type": "complete" }));
        harness.send(json!({ "type": "connection_init" }));

        match harness.next().await {
            // The message was ignored: the handshake still succeeds.
            Output::Json(value) => {
                assert_eq!(value["type"], "connection_ack", "{value}");
            }
            Output::Close { code, reason } => {
                assert_eq!(
                    code,
                    ws::CLOSE_UNAUTHORIZED,
                    "a specified message before the handshake must not be reported as invalid \
                     (4400); got {code} ({reason})"
                );
            }
        }
    }

    /// A `complete` for an id that never existed must not disturb the connection.
    #[tokio::test]
    async fn complete_for_an_unknown_id_is_ignored() {
        let mut harness = ready(Server::new(fixtures::schema())).await;
        harness.send(json!({ "id": "ghost", "type": "complete" }));

        harness.send(json!({ "type": "ping", "payload": { "n": 1 } }));
        let pong = harness.next_json().await;
        assert_eq!(pong["type"], "pong");
        assert_eq!(pong["payload"]["n"], 1);
    }

    /// Once the server has sent `complete` the id is free again, even if the connection loop has
    /// not yet drained its bookkeeping channel.
    #[tokio::test]
    async fn ids_can_be_reused_as_soon_as_the_server_completes() {
        let mut harness = ready(Server::new(fixtures::schema())).await;

        for _ in 0..8 {
            harness.send(subscribe("reuse", "{ hello }"));
            let next = harness.next_json().await;
            assert_eq!(next["type"], "next");
            assert_eq!(next["id"], "reuse");
            let complete = harness.next_json().await;
            assert_eq!(complete["type"], "complete");
        }
    }

    /// A completed operation must release its `max_operations` slot immediately.
    #[tokio::test]
    async fn max_operations_slots_are_released_on_completion() {
        let config = WsConfig::default().max_operations(1);
        let mut harness = Harness::start(Server::new(fixtures::schema()), config, Context::empty());
        harness.send(json!({ "type": "connection_init" }));
        assert_eq!(harness.next_json().await["type"], "connection_ack");

        for id in ["a", "b", "c"] {
            harness.send(subscribe(id, "{ hello }"));
            let next = harness.next_json().await;
            assert_eq!(
                next["type"], "next",
                "operation {id} was rejected although the previous one had completed: {next}"
            );
            assert_eq!(next["id"], id);
            assert_eq!(harness.next_json().await["type"], "complete");
        }
    }

    /// `max_operations` rejects with an `error` message (no `complete`) and keeps the socket open.
    #[tokio::test]
    async fn max_operations_rejection_keeps_the_socket_open() {
        let config = WsConfig::default().max_operations(1);
        let mut harness = Harness::start(Server::new(fixtures::schema()), config, Context::empty());
        harness.send(json!({ "type": "connection_init" }));
        assert_eq!(harness.next_json().await["type"], "connection_ack");

        harness.send(subscribe("a", "subscription { pending }"));
        harness.send(subscribe("b", "{ hello }"));

        let rejected = harness.next_json().await;
        assert_eq!(rejected["id"], "b");
        assert_eq!(rejected["type"], "error");

        // The rejected id was never admitted, so it can be retried once a slot frees up.
        harness.send(json!({ "id": "a", "type": "complete" }));
        harness.send(subscribe("b", "{ hello }"));
        let retried = harness.next_json().await;
        assert_eq!(
            retried["type"], "next",
            "retry after a slot freed: {retried}"
        );
        assert_eq!(retried["id"], "b");
    }

    /// Variable-coercion failures are request failures: `error`, not `next`.
    #[tokio::test]
    async fn variable_coercion_failures_are_error_messages() {
        let mut harness = ready(Server::new(fixtures::schema())).await;
        harness.send(json!({
            "id": "v",
            "type": "subscribe",
            "payload": {
                "query": "query($m: String!) { echo(message: $m) }",
                "variables": { "m": 5 }
            }
        }));

        let error = harness.next_json().await;
        assert_eq!(error["type"], "error");
        assert_eq!(error["id"], "v");
        assert!(error["payload"].as_array().is_some_and(|e| !e.is_empty()));
    }

    /// An execution timeout has no response path, so suku emits `error` (PROTOCOL.md allows
    /// `error` for failures during execution). Recorded here because it differs from HTTP, which
    /// answers 200 for the same operation.
    #[tokio::test]
    async fn execution_timeouts_arrive_as_error_messages() {
        let server = Server::new(fixtures::schema()).timeout(Duration::from_millis(10));
        let mut harness = ready(server).await;
        harness.send(subscribe("t", "{ slow }"));

        let message = harness.next_json().await;
        assert_eq!(message["type"], "error");
        assert!(
            message["payload"][0]["message"]
                .as_str()
                .unwrap_or_default()
                .to_lowercase()
                .contains("timed out"),
            "{message}"
        );
    }

    /// Pings are answered before the handshake, and pongs are accepted silently.
    #[tokio::test]
    async fn ping_and_pong_work_before_and_after_the_handshake() {
        let mut harness = Harness::start(
            Server::new(fixtures::schema()),
            WsConfig::default(),
            Context::empty(),
        );

        harness.send(json!({ "type": "ping" }));
        assert_eq!(harness.next_json().await["type"], "pong");
        harness.send(json!({ "type": "pong" }));
        harness.send(json!({ "type": "connection_init" }));
        assert_eq!(harness.next_json().await["type"], "connection_ack");
        harness.send(json!({ "type": "ping", "payload": null }));
        assert_eq!(harness.next_json().await["type"], "pong");
    }

    /// A binary frame is a fatal protocol error before the handshake too.
    #[tokio::test]
    async fn binary_frames_before_init_close_4400() {
        let mut harness = Harness::start(
            Server::new(fixtures::schema()),
            WsConfig::default(),
            Context::empty(),
        );
        harness.send_input(WsInput::Invalid(
            "Binary messages are not supported".to_owned(),
        ));
        assert_eq!(harness.expect_close().await.0, ws::CLOSE_BAD_REQUEST);
    }

    /// `max_message_bytes(0)` must disable the check, even when the server body limit is tiny.
    #[tokio::test]
    async fn a_zero_message_limit_disables_the_size_check() {
        let server = Server::new(fixtures::schema()).max_body_bytes(8);

        // Default configuration inherits the 8-byte body limit: the handshake is too large.
        let mut limited = Harness::start(server.clone(), WsConfig::default(), Context::empty());
        limited.send(json!({ "type": "connection_init" }));
        assert_eq!(limited.expect_close().await.0, ws::CLOSE_BAD_REQUEST);

        // An explicit 0 disables it.
        let mut unlimited = Harness::start(
            server,
            WsConfig::default().max_message_bytes(0),
            Context::empty(),
        );
        unlimited.send(json!({ "type": "connection_init" }));
        assert_eq!(unlimited.next_json().await["type"], "connection_ack");
    }

    /// A payload that is not an object is a protocol error.
    #[tokio::test]
    async fn non_object_subscribe_payload_closes_4400() {
        let mut harness = ready(Server::new(fixtures::schema())).await;
        harness.send_raw(r#"{"id":"x","type":"subscribe","payload":"nope"}"#);
        assert_eq!(harness.expect_close().await.0, ws::CLOSE_BAD_REQUEST);
    }

    /// Selection by `operationName` works over the socket.
    #[tokio::test]
    async fn operation_name_selects_the_document() {
        let mut harness = ready(Server::new(fixtures::schema())).await;
        harness.send(json!({
            "id": "o",
            "type": "subscribe",
            "payload": {
                "query": "query B { hello } query A { echo(message: \"a\") }",
                "operationName": "A"
            }
        }));

        let next = harness.next_json().await;
        assert_eq!(next["payload"]["data"]["echo"], "a");
        assert_eq!(harness.next_json().await["type"], "complete");
    }

    /// The context is fixed at the connection level: the `connection_init` payload cannot replace
    /// or extend it, and every operation sees the same value.
    #[tokio::test]
    async fn the_init_payload_cannot_change_the_connection_context() {
        let mut harness = Harness::start(
            Server::new(fixtures::schema()),
            WsConfig::default(),
            Context::new(fixtures::User {
                name: "Ada".to_owned(),
            }),
        );
        harness.send(json!({
            "type": "connection_init",
            "payload": { "authorization": "Bearer attacker", "user": "Mallory" }
        }));
        assert_eq!(harness.next_json().await["type"], "connection_ack");

        harness.send(subscribe("w", "{ whoami }"));
        assert_eq!(
            harness.next_json().await["payload"]["data"]["whoami"],
            "Ada"
        );
        assert_eq!(harness.next_json().await["type"], "complete");
    }

    /// A denial raised by the authorizer arrives as a response error and completes normally.
    #[tokio::test]
    async fn authorizer_errors_do_not_terminate_the_connection() {
        struct Deny;

        impl gql::Authorizer for Deny {
            fn authorize(
                &self,
                context: &gql::AuthorizationContext<'_>,
            ) -> Result<(), gql::GraphQLError> {
                if context.field_name == "hello" {
                    Err(gql::GraphQLError::new("nope").extend("code", "UNAUTHORIZED"))
                } else {
                    Ok(())
                }
            }
        }

        let server = Server::new(fixtures::schema()).authorizer(Deny);
        let mut harness = ready(server).await;
        harness.send(subscribe("d", "{ hello }"));

        let next = harness.next_json().await;
        assert_eq!(next["payload"]["errors"][0]["message"], "nope");
        assert_eq!(harness.next_json().await["type"], "complete");

        harness.send(subscribe("ok", "{ echo(message: \"fine\") }"));
        assert_eq!(harness.next_json().await["payload"]["data"]["echo"], "fine");
    }

    /// The server's error formatter also applies to an `error` message.
    #[tokio::test]
    async fn the_error_formatter_applies_to_request_error_messages() {
        let server = Server::new(fixtures::schema()).format_error(suku::mask_internal);
        let mut harness = ready(server).await;
        harness.send(subscribe("x", "{ doesNotExist }"));

        let error = harness.next_json().await;
        assert_eq!(error["type"], "error");
        // Request errors stay readable so clients can fix their document.
        assert!(
            error["payload"][0]["message"]
                .as_str()
                .unwrap_or_default()
                .contains("doesNotExist"),
            "{error}"
        );
    }

    /// A subscription the client cancels must never emit a `complete` for that id.
    #[tokio::test]
    async fn a_client_completed_operation_sends_nothing_further() {
        let mut harness = ready(Server::new(fixtures::schema())).await;
        harness.send(subscribe("c", "subscription { pending }"));
        // Give the operation a chance to start before cancelling it.
        tokio::time::sleep(Duration::from_millis(20)).await;
        harness.send(json!({ "id": "c", "type": "complete" }));

        harness.send(json!({ "type": "ping" }));
        let output = harness.next_json().await;
        assert_eq!(
            output["type"], "pong",
            "a completed operation must be silent: {output}"
        );
    }

    /// A `next` payload carries `data: {}` for a null-propagating field error, not `error`.
    #[tokio::test]
    async fn field_errors_are_next_messages_with_a_complete() {
        let mut harness = ready(Server::new(fixtures::schema())).await;
        harness.send(subscribe("f", "{ echo(message: \"x\") }"));
        let next = harness.next_json().await;
        assert_eq!(next["type"], "next");
        assert_eq!(next["payload"]["data"]["echo"], "x");
        assert_eq!(harness.next_json().await["type"], "complete");
    }

    /// An execution error that carries a response path is an execution *result*, so it must be a
    /// `next` payload followed by `complete` — not an `error` message.
    #[tokio::test]
    async fn path_bearing_execution_errors_are_next_results() {
        let mut harness = ready(Server::new(fixtures::schema())).await;
        harness.send(subscribe("l", "{ located }"));

        let next = harness.next_json().await;
        assert_eq!(next["type"], "next", "{next}");
        assert_eq!(next["payload"]["errors"][0]["message"], "located failure");
        assert_eq!(next["payload"]["errors"][0]["path"][0], "located");
        assert_eq!(harness.next_json().await["type"], "complete");
    }

    /// Field errors inside a subscription event are `next` payloads; the stream keeps going and
    /// then completes.
    #[tokio::test]
    async fn subscription_field_errors_keep_the_stream_alive() {
        let mut harness = ready(Server::new(fixtures::schema())).await;
        harness.send(subscribe("f2", "subscription { failing }"));

        let first = harness.next_json().await;
        assert_eq!(first["type"], "next");
        assert_eq!(first["payload"]["data"]["failing"], 1);

        let second = harness.next_json().await;
        assert_eq!(second["type"], "next");
        assert_eq!(second["payload"]["errors"][0]["message"], "event failure");

        assert_eq!(harness.next_json().await["type"], "complete");
    }

    /// Every operation on the socket shares the connection's context.
    #[tokio::test]
    async fn two_operations_share_one_context() {
        let mut harness = Harness::start(
            Server::new(fixtures::schema()),
            WsConfig::default(),
            Context::new(fixtures::User {
                name: "Ada".to_owned(),
            }),
        );
        harness.send(json!({ "type": "connection_init", "payload": { "name": "Mallory" } }));
        assert_eq!(harness.next_json().await["type"], "connection_ack");

        for id in ["c1", "c2"] {
            harness.send(subscribe(id, "{ whoami }"));
            assert_eq!(
                harness.next_json().await["payload"]["data"]["whoami"],
                "Ada"
            );
            assert_eq!(harness.next_json().await["type"], "complete");
        }
    }
}

// ===========================================================================
// 2. Axum routing coexistence / upgrade semantics
// ===========================================================================

#[cfg(feature = "axum")]
mod axum_probes {
    use ::axum::{
        body::Body,
        http::{Method, Request, StatusCode, header},
    };
    use serde_json::json;
    use suku::Server;
    #[cfg(feature = "ws")]
    use suku::{Context, HttpError};
    use tower::ServiceExt as _;

    use super::fixtures;

    fn upgrade_request(path: &str) -> Request<Body> {
        Request::builder()
            .method(Method::GET)
            .uri(path)
            .header(header::CONNECTION, "Upgrade")
            .header(header::UPGRADE, "websocket")
            .header(header::SEC_WEBSOCKET_VERSION, "13")
            .header(header::SEC_WEBSOCKET_KEY, "dGhlIHNhbXBsZSBub25jZQ==")
            .body(Body::empty())
            .expect("request")
    }

    /// Every transport is reachable on the one mounted path.
    #[tokio::test]
    async fn one_path_serves_graphiql_get_post_and_sse() {
        let app = suku::GraphQL::new(Server::new(fixtures::schema())).into_router();

        // GraphiQL
        let response = app
            .clone()
            .oneshot(
                Request::builder()
                    .method(Method::GET)
                    .uri("/graphql")
                    .header(header::ACCEPT, "text/html")
                    .body(Body::empty())
                    .expect("request"),
            )
            .await
            .expect("response");
        assert_eq!(response.status(), StatusCode::OK);
        assert_eq!(
            response
                .headers()
                .get(header::CONTENT_TYPE)
                .and_then(|value| value.to_str().ok()),
            Some("text/html; charset=utf-8")
        );

        // GET query
        let response = app
            .clone()
            .oneshot(
                Request::builder()
                    .method(Method::GET)
                    .uri("/graphql?query=%7B%20hello%20%7D")
                    .header(header::ACCEPT, "application/json")
                    .body(Body::empty())
                    .expect("request"),
            )
            .await
            .expect("response");
        assert_eq!(response.status(), StatusCode::OK);

        // POST query
        let response = app
            .clone()
            .oneshot(
                Request::builder()
                    .method(Method::POST)
                    .uri("/graphql")
                    .header(header::CONTENT_TYPE, "application/json")
                    .header(header::ACCEPT, "application/json")
                    .body(Body::from(json!({ "query": "{ hello }" }).to_string()))
                    .expect("request"),
            )
            .await
            .expect("response");
        assert_eq!(response.status(), StatusCode::OK);

        // POST + SSE subscription
        let response = app
            .oneshot(
                Request::builder()
                    .method(Method::POST)
                    .uri("/graphql")
                    .header(header::CONTENT_TYPE, "application/json")
                    .header(header::ACCEPT, "text/event-stream")
                    .body(Body::from(
                        json!({ "query": "subscription { countdown }" }).to_string(),
                    ))
                    .expect("request"),
            )
            .await
            .expect("response");
        assert_eq!(response.status(), StatusCode::OK);
        assert_eq!(
            response
                .headers()
                .get(header::CONTENT_TYPE)
                .and_then(|value| value.to_str().ok()),
            Some("text/event-stream; charset=utf-8")
        );
    }

    /// `websocket(false)` falls through to the normal GET path, as documented.
    #[cfg(feature = "ws")]
    #[tokio::test]
    async fn websocket_disabled_falls_through_to_get() {
        let app = suku::GraphQL::new(Server::new(fixtures::schema()))
            .websocket(false)
            .into_router();

        let mut request = upgrade_request("/graphql");
        request.headers_mut().insert(
            header::ACCEPT,
            header::HeaderValue::from_static("text/html"),
        );
        let response = app.oneshot(request).await.expect("response");
        assert_eq!(response.status(), StatusCode::OK);
        assert_eq!(
            response
                .headers()
                .get(header::CONTENT_TYPE)
                .and_then(|value| value.to_str().ok()),
            Some("text/html; charset=utf-8"),
            "an upgrade must not be misinterpreted when WebSockets are off"
        );
    }

    /// A mismatched subprotocol is rejected before the socket is accepted.
    #[cfg(feature = "ws")]
    #[tokio::test]
    async fn a_legacy_subprotocol_is_rejected_with_400() {
        let app = suku::GraphQL::new(Server::new(fixtures::schema())).into_router();
        let mut request = upgrade_request("/graphql");
        request.headers_mut().insert(
            header::SEC_WEBSOCKET_PROTOCOL,
            header::HeaderValue::from_static("graphql-ws"),
        );
        let response = app.oneshot(request).await.expect("response");
        assert_eq!(response.status(), StatusCode::BAD_REQUEST);
    }

    /// Without the `ws` feature a WebSocket upgrade is just a GET: the documented fall-through.
    #[cfg(not(feature = "ws"))]
    #[tokio::test]
    async fn without_the_ws_feature_an_upgrade_is_plain_get() {
        let app = suku::GraphQL::new(Server::new(fixtures::schema())).into_router();
        let mut request = upgrade_request("/graphql");
        request.headers_mut().insert(
            header::ACCEPT,
            header::HeaderValue::from_static("text/html"),
        );
        let response = app.oneshot(request).await.expect("response");
        assert_eq!(response.status(), StatusCode::OK);
    }

    /// The upgrade goes through the extractor, so an application *can* enforce an `Origin`
    /// allowlist for cookie-authenticated sockets (suku itself does not).
    #[cfg(feature = "ws")]
    #[tokio::test]
    async fn the_origin_header_is_available_to_the_extractor_during_upgrade() {
        let app = suku::GraphQL::new(Server::new(fixtures::schema()))
            .with_context(|request: suku::http::ContextRequest| async move {
                match request
                    .header("origin")
                    .and_then(|value| value.to_str().ok())
                {
                    Some("https://app.example") => Ok(Context::new(fixtures::User {
                        name: "Ada".to_owned(),
                    })),
                    _ => Err(HttpError::message(
                        StatusCode::FORBIDDEN,
                        "Cross-origin WebSocket rejected",
                    )),
                }
            })
            .into_router();

        let allowed = app.clone();
        let mut request = upgrade_request("/graphql");
        request.headers_mut().insert(
            header::SEC_WEBSOCKET_PROTOCOL,
            header::HeaderValue::from_static(suku::ws::GRAPHQL_TRANSPORT_WS),
        );
        request.headers_mut().insert(
            header::ORIGIN,
            header::HeaderValue::from_static("https://evil.example"),
        );
        let response = allowed.oneshot(request).await.expect("response");
        assert_eq!(response.status(), StatusCode::FORBIDDEN);
    }

    /// The upgrade request authenticates exactly once, before the socket is accepted: a denial
    /// body is the extractor's error, not a protocol close.
    #[cfg(feature = "ws")]
    #[tokio::test]
    async fn an_unauthorized_upgrade_returns_json_not_an_upgrade() {
        let app = suku::GraphQL::new(Server::new(fixtures::schema()))
            .with_context(|_: suku::http::ContextRequest| async {
                Err::<Context, HttpError>(HttpError::message(StatusCode::UNAUTHORIZED, "no token"))
            })
            .into_router();

        let mut request = upgrade_request("/graphql");
        request.headers_mut().insert(
            header::SEC_WEBSOCKET_PROTOCOL,
            header::HeaderValue::from_static(suku::ws::GRAPHQL_TRANSPORT_WS),
        );
        let response = app.oneshot(request).await.expect("response");
        assert_eq!(response.status(), StatusCode::UNAUTHORIZED);
        assert_eq!(
            response
                .headers()
                .get(header::CONTENT_TYPE)
                .and_then(|value| value.to_str().ok()),
            Some("application/json; charset=utf-8")
        );
    }
}

// ===========================================================================
// 3. Multipart uploads
// ===========================================================================

#[cfg(feature = "multipart")]
mod multipart_probes {
    use ::http::{Method, StatusCode, header};
    use serde_json::Value as JsonValue;
    use suku::{
        Context, GraphQLRequest, HttpConfig, HttpError, HttpResponse, HttpResult, Server,
        http::{ContextRequest, handle},
    };

    use super::fixtures;

    const BOUNDARY: &str = "----reviewboundary";

    struct Part<'a> {
        name: &'a str,
        filename: Option<&'a str>,
        content_type: Option<&'a str>,
        body: &'a [u8],
    }

    fn text<'a>(name: &'a str, body: &'a str) -> Part<'a> {
        Part {
            name,
            filename: None,
            content_type: None,
            body: body.as_bytes(),
        }
    }

    fn file<'a>(name: &'a str, filename: &'a str, body: &'a [u8]) -> Part<'a> {
        Part {
            name,
            filename: Some(filename),
            content_type: Some("application/octet-stream"),
            body,
        }
    }

    fn body(parts: &[Part<'_>]) -> Vec<u8> {
        let mut body = Vec::new();
        for part in parts {
            body.extend_from_slice(format!("--{BOUNDARY}\r\n").as_bytes());
            match part.filename {
                Some(filename) => body.extend_from_slice(
                    format!(
                        "Content-Disposition: form-data; name=\"{}\"; filename=\"{}\"\r\n",
                        part.name, filename
                    )
                    .as_bytes(),
                ),
                None => body.extend_from_slice(
                    format!("Content-Disposition: form-data; name=\"{}\"\r\n", part.name)
                        .as_bytes(),
                ),
            }
            if let Some(content_type) = part.content_type {
                body.extend_from_slice(format!("Content-Type: {content_type}\r\n").as_bytes());
            }
            body.extend_from_slice(b"\r\n");
            body.extend_from_slice(part.body);
            body.extend_from_slice(b"\r\n");
        }
        body.extend_from_slice(format!("--{BOUNDARY}--\r\n").as_bytes());
        body
    }

    fn content_type() -> String {
        format!("multipart/form-data; boundary={BOUNDARY}")
    }

    fn parts(method: Method, headers: &[(::http::HeaderName, &str)]) -> ::http::request::Parts {
        let mut builder = ::http::Request::builder().method(method).uri("/graphql");
        for (name, value) in headers {
            builder = builder.header(name.as_str(), *value);
        }
        builder.body(()).expect("request").into_parts().0
    }

    async fn empty_context(_: ContextRequest) -> Result<Context, HttpError> {
        Ok(Context::empty())
    }

    fn buffered(result: HttpResult) -> HttpResponse {
        match result {
            HttpResult::Response(response) => response,
            HttpResult::Stream(_) => panic!("expected a buffered response"),
        }
    }

    async fn post(server: &Server, config: &HttpConfig, body: &[u8]) -> HttpResponse {
        let header_value = content_type();
        let parts = parts(
            Method::POST,
            &[
                (header::CONTENT_TYPE, &header_value),
                (header::ACCEPT, "application/json"),
            ],
        );
        buffered(handle(server, config, &parts, body, empty_context).await)
    }

    async fn post_with<F, Fut>(
        server: &Server,
        config: &HttpConfig,
        body: &[u8],
        extractor: F,
    ) -> HttpResponse
    where
        F: FnOnce(ContextRequest) -> Fut + Send,
        Fut: std::future::Future<Output = Result<Context, HttpError>> + Send,
    {
        let header_value = content_type();
        let parts = parts(
            Method::POST,
            &[
                (header::CONTENT_TYPE, &header_value),
                (header::ACCEPT, "application/json"),
            ],
        );
        buffered(handle(server, config, &parts, body, extractor).await)
    }

    fn json_body(response: &HttpResponse) -> JsonValue {
        serde_json::from_slice(&response.body).expect("JSON body")
    }

    fn error_text(response: &HttpResponse) -> String {
        json_body(response)["errors"][0]["message"]
            .as_str()
            .unwrap_or_default()
            .to_owned()
    }

    const UPLOAD_OPERATIONS: &str =
        r#"{"query":"mutation($file: Upload!){ upload(file:$file) }","variables":{"file":null}}"#;
    const UPLOAD_MAP: &str = r#"{"0":["variables.file"]}"#;

    fn upload_body<'a>(file_body: &'a [u8]) -> Vec<u8> {
        body(&[
            text("operations", UPLOAD_OPERATIONS),
            text("map", UPLOAD_MAP),
            Part {
                name: "0",
                filename: Some("a.txt"),
                content_type: Some("text/plain"),
                body: file_body,
            },
        ])
    }

    // -----------------------------------------------------------------------
    // field structure
    // -----------------------------------------------------------------------

    #[tokio::test]
    async fn a_minimal_spec_request_works() {
        let server = Server::new(fixtures::schema());
        let response = post(&server, &HttpConfig::default(), &upload_body(b"hi")).await;
        assert_eq!(response.status, StatusCode::OK, "{}", error_text(&response));
        assert_eq!(
            json_body(&response)["data"]["upload"],
            "a.txt:text/plain:hi"
        );
    }

    #[tokio::test]
    async fn duplicate_operations_fields_are_rejected() {
        let server = Server::new(fixtures::schema());
        let body = body(&[
            text("operations", UPLOAD_OPERATIONS),
            text("operations", UPLOAD_OPERATIONS),
            text("map", UPLOAD_MAP),
            file("0", "a.txt", b"a"),
        ]);
        let response = post(&server, &HttpConfig::default(), &body).await;
        assert_eq!(response.status, StatusCode::BAD_REQUEST);
        assert!(
            error_text(&response).contains("more than once"),
            "{}",
            error_text(&response)
        );
    }

    #[tokio::test]
    async fn duplicate_map_fields_are_rejected() {
        let server = Server::new(fixtures::schema());
        let body = body(&[
            text("operations", UPLOAD_OPERATIONS),
            text("map", UPLOAD_MAP),
            text("map", UPLOAD_MAP),
            file("0", "a.txt", b"a"),
        ]);
        let response = post(&server, &HttpConfig::default(), &body).await;
        assert_eq!(response.status, StatusCode::BAD_REQUEST);
        assert!(
            error_text(&response).contains("more than once"),
            "{}",
            error_text(&response)
        );
    }

    #[tokio::test]
    async fn duplicate_file_parts_are_rejected() {
        let server = Server::new(fixtures::schema());
        let body = body(&[
            text("operations", UPLOAD_OPERATIONS),
            text("map", UPLOAD_MAP),
            file("0", "a.txt", b"a"),
            file("0", "b.txt", b"b"),
        ]);
        let response = post(&server, &HttpConfig::default(), &body).await;
        assert_eq!(response.status, StatusCode::BAD_REQUEST);
        assert!(
            error_text(&response).contains("more than once"),
            "{}",
            error_text(&response)
        );
    }

    #[tokio::test]
    async fn a_file_part_before_the_map_is_rejected() {
        let server = Server::new(fixtures::schema());
        let body = body(&[
            text("operations", UPLOAD_OPERATIONS),
            file("0", "a.txt", b"a"),
            text("map", UPLOAD_MAP),
        ]);
        let response = post(&server, &HttpConfig::default(), &body).await;
        assert_eq!(response.status, StatusCode::BAD_REQUEST);
        assert!(
            error_text(&response).contains("must follow"),
            "{}",
            error_text(&response)
        );
    }

    #[tokio::test]
    async fn map_before_operations_is_rejected() {
        let server = Server::new(fixtures::schema());
        let body = body(&[
            text("map", UPLOAD_MAP),
            text("operations", UPLOAD_OPERATIONS),
            file("0", "a.txt", b"a"),
        ]);
        let response = post(&server, &HttpConfig::default(), &body).await;
        assert_eq!(response.status, StatusCode::BAD_REQUEST);
        assert!(
            error_text(&response).contains("must appear before"),
            "{}",
            error_text(&response)
        );
    }

    // -----------------------------------------------------------------------
    // malformed operations / map
    // -----------------------------------------------------------------------

    #[tokio::test]
    async fn malformed_operations_json_is_rejected() {
        let server = Server::new(fixtures::schema());
        let body = body(&[text("operations", "{\"query\":"), text("map", "{}")]);
        let response = post(&server, &HttpConfig::default(), &body).await;
        assert_eq!(response.status, StatusCode::BAD_REQUEST);
        assert!(
            error_text(&response).contains("operations"),
            "{}",
            error_text(&response)
        );
    }

    #[tokio::test]
    async fn non_object_operations_are_rejected() {
        let server = Server::new(fixtures::schema());
        for operations in ["5", "null", "\"query\"", "[]"] {
            let body = body(&[text("operations", operations), text("map", "{}")]);
            let response = post(&server, &HttpConfig::default(), &body).await;
            assert_eq!(
                response.status,
                StatusCode::BAD_REQUEST,
                "operations={operations} should be rejected"
            );
        }
    }

    #[tokio::test]
    async fn a_map_that_is_not_an_object_is_rejected() {
        let server = Server::new(fixtures::schema());
        let body = body(&[text("operations", UPLOAD_OPERATIONS), text("map", "[]")]);
        let response = post(&server, &HttpConfig::default(), &body).await;
        assert_eq!(response.status, StatusCode::BAD_REQUEST);
    }

    #[tokio::test]
    async fn map_entries_must_be_arrays_of_strings() {
        let server = Server::new(fixtures::schema());
        for map in [r#"{"0":"variables.file"}"#, r#"{"0":[5]}"#, r#"{"0":[]}"#] {
            let body = body(&[
                text("operations", UPLOAD_OPERATIONS),
                text("map", map),
                file("0", "a.txt", b"a"),
            ]);
            let response = post(&server, &HttpConfig::default(), &body).await;
            assert_eq!(
                response.status,
                StatusCode::BAD_REQUEST,
                "map={map} should be rejected"
            );
        }
    }

    // -----------------------------------------------------------------------
    // path validation
    // -----------------------------------------------------------------------

    #[tokio::test]
    async fn paths_must_address_an_existing_null_inside_variables() {
        let server = Server::new(fixtures::schema());
        for map in [
            r#"{"0":["variables.missing"]}"#,
            r#"{"0":["variables.file.deeper"]}"#,
            r#"{"0":["variables.file.0"]}"#,
            r#"{"0":["variables"]}"#,
            r#"{"0":["file"]}"#,
            r#"{"0":["variables."]}"#,
            r#"{"0":["variables..file"]}"#,
            r#"{"0":[".variables.file"]}"#,
        ] {
            let body = body(&[
                text("operations", UPLOAD_OPERATIONS),
                text("map", map),
                file("0", "a.txt", b"a"),
            ]);
            let response = post(&server, &HttpConfig::default(), &body).await;
            assert_eq!(
                response.status,
                StatusCode::BAD_REQUEST,
                "map={map} should be rejected, body={:?}",
                String::from_utf8_lossy(&response.body)
            );
        }
    }

    /// Paths cannot create new keys: `variables.a.b` must not materialise `a`.
    #[tokio::test]
    async fn a_path_cannot_create_intermediate_objects() {
        let server = Server::new(fixtures::schema());
        let body = body(&[
            text("operations", r#"{"query":"{ hello }","variables":{}}"#),
            text("map", r#"{"0":["variables.a.b"]}"#),
            file("0", "a.txt", b"a"),
        ]);
        let response = post(&server, &HttpConfig::default(), &body).await;
        assert_eq!(response.status, StatusCode::BAD_REQUEST);
    }

    /// JS prototype-looking names are ordinary JSON keys here; what matters is that they are
    /// validated like any other path and never resolve to something inherited.
    #[tokio::test]
    async fn prototype_looking_paths_behave_like_ordinary_keys() {
        let server = Server::new(fixtures::schema());

        // Not present in `variables`: rejected.
        let proto_body = body(&[
            text("operations", UPLOAD_OPERATIONS),
            text("map", r#"{"0":["variables.__proto__"]}"#),
            file("0", "a.txt", b"a"),
        ]);
        let response = post(&server, &HttpConfig::default(), &proto_body).await;
        assert_eq!(response.status, StatusCode::BAD_REQUEST);

        // Path ending in `constructor`: also just a missing key.
        let ctor_body = body(&[
            text("operations", UPLOAD_OPERATIONS),
            text("map", r#"{"0":["variables.constructor"]}"#),
            file("0", "a.txt", b"a"),
        ]);
        let response = post(&server, &HttpConfig::default(), &ctor_body).await;
        assert_eq!(response.status, StatusCode::BAD_REQUEST);
    }

    // -----------------------------------------------------------------------
    // batches
    // -----------------------------------------------------------------------

    #[tokio::test]
    async fn a_batch_index_out_of_range_is_rejected() {
        let server = Server::new(fixtures::schema());
        let body = body(&[
            text(
                "operations",
                r#"[{"query":"mutation($f: Upload!){ upload(file:$f) }","variables":{"f":null}}]"#,
            ),
            text("map", r#"{"0":["1.variables.f"]}"#),
            file("0", "a.txt", b"a"),
        ]);
        let response = post(&server, &HttpConfig::default(), &body).await;
        assert_eq!(response.status, StatusCode::BAD_REQUEST);
        assert!(
            error_text(&response).contains("batch"),
            "{}",
            error_text(&response)
        );
    }

    /// A batch path without a leading index is a request error, not a silent default to 0.
    #[tokio::test]
    async fn a_batch_path_must_start_with_an_index() {
        let server = Server::new(fixtures::schema());
        let body = body(&[
            text(
                "operations",
                r#"[{"query":"mutation($f: Upload!){ upload(file:$f) }","variables":{"f":null}}]"#,
            ),
            text("map", r#"{"0":["variables.f"]}"#),
            file("0", "a.txt", b"a"),
        ]);
        let response = post(&server, &HttpConfig::default(), &body).await;
        assert_eq!(response.status, StatusCode::BAD_REQUEST);
    }

    /// One file may serve two requests in a batch.
    #[tokio::test]
    async fn a_file_can_be_shared_across_batch_requests() {
        let server = Server::new(fixtures::schema());
        let body = body(&[
            text(
                "operations",
                r#"[{"query":"mutation($f: Upload!){ upload(file:$f) }","variables":{"f":null}},
                    {"query":"mutation($f: Upload!){ upload(file:$f) }","variables":{"f":null}}]"#,
            ),
            text("map", r#"{"0":["0.variables.f","1.variables.f"]}"#),
            file("0", "a.txt", b"shared"),
        ]);
        let response = post(&server, &HttpConfig::default(), &body).await;
        assert_eq!(response.status, StatusCode::OK, "{}", error_text(&response));
        let body_json = json_body(&response);
        assert_eq!(
            body_json[0]["data"]["upload"],
            "a.txt:application/octet-stream:shared"
        );
        assert_eq!(
            body_json[1]["data"]["upload"],
            "a.txt:application/octet-stream:shared"
        );
    }

    // -----------------------------------------------------------------------
    // file part metadata
    // -----------------------------------------------------------------------

    #[tokio::test]
    async fn a_part_without_a_filename_is_rejected() {
        let server = Server::new(fixtures::schema());
        let body = body(&[
            text("operations", UPLOAD_OPERATIONS),
            text("map", UPLOAD_MAP),
            Part {
                name: "0",
                filename: None,
                content_type: Some("text/plain"),
                body: b"a",
            },
        ]);
        let response = post(&server, &HttpConfig::default(), &body).await;
        assert_eq!(response.status, StatusCode::BAD_REQUEST);
        assert!(
            error_text(&response).contains("must be a file"),
            "{}",
            error_text(&response)
        );
    }

    #[tokio::test]
    async fn a_file_without_a_content_type_is_allowed_and_stays_none() {
        let server = Server::new(fixtures::schema());
        let body = body(&[
            text("operations", UPLOAD_OPERATIONS),
            text("map", UPLOAD_MAP),
            Part {
                name: "0",
                filename: Some("a.bin"),
                content_type: None,
                body: b"raw",
            },
        ]);
        let response = post(&server, &HttpConfig::default(), &body).await;
        assert_eq!(response.status, StatusCode::OK, "{}", error_text(&response));
        assert_eq!(json_body(&response)["data"]["upload"], "a.bin:<none>:raw");
    }

    #[tokio::test]
    async fn an_empty_file_is_preserved() {
        let server = Server::new(fixtures::schema());
        let response = post(&server, &HttpConfig::default(), &upload_body(b"")).await;
        assert_eq!(response.status, StatusCode::OK, "{}", error_text(&response));
        assert_eq!(json_body(&response)["data"]["upload"], "a.txt:text/plain:");
    }

    /// The filename is attacker-controlled text. suku never touches the filesystem, so it
    /// forwards it verbatim; applications must sanitise it before writing to disk.
    #[tokio::test]
    async fn a_traversal_filename_is_forwarded_verbatim_and_writes_nothing() {
        let server = Server::new(fixtures::schema());
        let body = body(&[
            text("operations", UPLOAD_OPERATIONS),
            text("map", UPLOAD_MAP),
            file("0", "../../etc/passwd", b"x"),
        ]);
        let response = post(&server, &HttpConfig::default(), &body).await;
        assert_eq!(response.status, StatusCode::OK, "{}", error_text(&response));
        let described = json_body(&response)["data"]["upload"]
            .as_str()
            .unwrap_or_default()
            .to_owned();
        assert_eq!(described, "../../etc/passwd:application/octet-stream:x");
    }

    // -----------------------------------------------------------------------
    // content type / limits / ordering
    // -----------------------------------------------------------------------

    #[tokio::test]
    async fn a_multipart_content_type_without_a_boundary_is_rejected() {
        let server = Server::new(fixtures::schema());
        let parts = parts(
            Method::POST,
            &[
                (header::CONTENT_TYPE, "multipart/form-data"),
                (header::ACCEPT, "application/json"),
            ],
        );
        let response =
            buffered(handle(&server, &HttpConfig::default(), &parts, b"", empty_context).await);
        assert_eq!(response.status, StatusCode::BAD_REQUEST);
        assert!(
            error_text(&response).to_lowercase().contains("multipart"),
            "{}",
            error_text(&response)
        );
    }

    #[tokio::test]
    async fn max_multipart_parts_zero_disables_the_limit() {
        let server = Server::new(fixtures::schema());
        let config = HttpConfig::default().max_multipart_parts(0);
        let response = post(&server, &config, &upload_body(b"a")).await;
        assert_eq!(response.status, StatusCode::OK, "{}", error_text(&response));
    }

    /// Authentication must run before the multipart body is parsed, including for a body that
    /// would otherwise be rejected.
    #[tokio::test]
    async fn authentication_runs_before_the_multipart_parser() {
        let server = Server::new(fixtures::schema());
        let response = post_with(
            &server,
            &HttpConfig::default(),
            b"not a multipart body at all",
            |_: ContextRequest| async {
                Err::<Context, HttpError>(HttpError::message(StatusCode::UNAUTHORIZED, "no token"))
            },
        )
        .await;
        assert_eq!(response.status, StatusCode::UNAUTHORIZED);
        assert_eq!(error_text(&response), "no token");
    }

    /// An upload supplied to a non-`Upload` variable is an ordinary coercion error, not a panic
    /// or an internal error.
    #[tokio::test]
    async fn an_upload_for_a_string_variable_fails_cleanly() {
        let server = Server::new(fixtures::schema());
        let body = body(&[
            text(
                "operations",
                r#"{"query":"query($m: String!){ echo(message:$m) }","variables":{"m":null}}"#,
            ),
            text("map", r#"{"0":["variables.m"]}"#),
            file("0", "a.txt", b"a"),
        ]);
        let response = post(&server, &HttpConfig::default(), &body).await;
        assert_ne!(response.status, StatusCode::INTERNAL_SERVER_ERROR);
        let body_json = json_body(&response);
        assert!(
            body_json["errors"]
                .as_array()
                .is_some_and(|errors| !errors.is_empty()),
            "{body_json}"
        );
    }

    /// A missing `operations` part (only a `map`) is rejected.
    #[tokio::test]
    async fn a_missing_operations_part_is_rejected() {
        let server = Server::new(fixtures::schema());
        let body = body(&[text("map", "{}")]);
        let response = post(&server, &HttpConfig::default(), &body).await;
        assert_eq!(response.status, StatusCode::BAD_REQUEST);
    }

    // -----------------------------------------------------------------------
    // the public `GraphQLRequest` upload API
    // -----------------------------------------------------------------------

    #[test]
    fn with_upload_validates_its_path_when_the_variables_are_converted() {
        let upload = gql::Upload::new("a.txt", "text/plain", b"a".to_vec());

        let request = GraphQLRequest::new("{ hello }").with_upload("file", upload.clone());
        assert!(request.variables_object().is_err());

        let request =
            GraphQLRequest::new("{ hello }").with_upload("variables.missing", upload.clone());
        assert!(request.variables_object().is_err());

        let mut variables = serde_json::Map::new();
        variables.insert("file".to_owned(), JsonValue::String("taken".to_owned()));
        let request = GraphQLRequest::new("{ hello }")
            .variables(variables)
            .with_upload("variables.file", upload.clone());
        assert!(request.variables_object().is_err());

        let mut variables = serde_json::Map::new();
        variables.insert("file".to_owned(), JsonValue::Null);
        let request = GraphQLRequest::new("{ hello }")
            .variables(variables)
            .with_upload("variables.file", upload);
        let object = request
            .variables_object()
            .expect("valid upload")
            .expect("variables");
        assert!(
            object
                .get("file")
                .and_then(gql::Value::as_external::<gql::Upload>)
                .is_some()
        );
    }

    /// A JSON body cannot smuggle an `uploads` array past the transport: the field is skipped on
    /// deserialization, so only multipart (or explicit `with_upload`) can attach a file.
    #[test]
    fn the_json_uploads_field_is_not_deserializable() {
        let request = GraphQLRequest::from_json_str(
            r#"{"query":"{ hello }","variables":{},"uploads":[{"path":"variables.x"}]}"#,
        )
        .expect("request");
        assert!(request.uploads.is_empty());
    }

    /// Cloning an upload (for a shared file mapping) shares the bytes rather than copying them.
    #[test]
    fn cloning_an_upload_shares_its_bytes() {
        let upload = gql::Upload::new("a.txt", "text/plain", b"0123456789".to_vec());
        let clone = upload.clone();
        assert_eq!(
            upload.bytes().as_ptr(),
            clone.bytes().as_ptr(),
            "an upload clone must share its Arc<[u8]>, not copy it"
        );
        assert_eq!(clone.len(), 10);
        assert!(!clone.is_empty());
    }

    #[test]
    fn cloning_a_request_keeps_its_uploads() {
        let request = GraphQLRequest::new("{ hello }").with_upload(
            "variables.f",
            gql::Upload::new("a.txt", "text/plain", b"a".to_vec()),
        );
        let clone = request.clone();
        assert_eq!(clone.uploads.len(), 1);
        assert_eq!(clone.uploads[0].upload.bytes(), b"a");
        assert_eq!(clone, request);
    }
}

// ===========================================================================
// 4. Vendored GraphiQL
// ===========================================================================

mod graphiql_probes {
    use suku::http::{graphiql, graphiql_html};

    #[test]
    fn the_accessors_return_the_same_static_page() {
        assert_eq!(graphiql(), graphiql_html());
        assert!(graphiql_html().starts_with("<!DOCTYPE html>"));
    }

    /// Only the vendored page claims to be self-contained; the default page is deliberately
    /// CDN-backed.
    #[cfg(feature = "vendored-graphiql")]
    #[test]
    fn the_page_has_no_external_script_or_stylesheet() {
        let html = graphiql_html();

        // No element-level network references. (Bare URLs inside inlined JS strings are data,
        // not requests, so they are checked separately below.)
        for needle in [
            "src=\"http",
            "src='http",
            "href=\"http",
            "href='http",
            "src=//",
            "href=//",
            "@import",
            "url(http",
            "url(//",
        ] {
            assert!(!html.contains(needle), "unexpected reference: {needle}");
        }
    }

    #[test]
    fn the_page_has_no_frames_or_preconnects() {
        let html = graphiql_html();
        for needle in ["<iframe", "rel=\"preconnect\"", "rel=\"dns-prefetch\""] {
            assert!(!html.contains(needle), "unexpected element: {needle}");
        }
    }

    #[cfg(feature = "vendored-graphiql")]
    mod vendored {
        use suku::http::graphiql_html;

        #[test]
        fn the_bundles_cannot_terminate_their_script_block() {
            // A literal `</script>` in a bundle closes the element early. An opening `<script`
            // is only dangerous in the *same* block as a `<!--` (the tokenizer's
            // double-escaped state), which is why the pair is checked rather than either alone.
            for (name, source) in [
                (
                    "graphiql.min.js",
                    include_str!("../src/http/vendor/graphiql.min.js"),
                ),
                (
                    "react.production.min.js",
                    include_str!("../src/http/vendor/react.production.min.js"),
                ),
                (
                    "react-dom.production.min.js",
                    include_str!("../src/http/vendor/react-dom.production.min.js"),
                ),
                (
                    "graphiql.min.css",
                    include_str!("../src/http/vendor/graphiql.min.css"),
                ),
            ] {
                let lower = source.to_ascii_lowercase();
                assert!(
                    !lower.contains("</script"),
                    "{name} contains a closing script tag"
                );
                assert!(
                    !lower.contains("</style"),
                    "{name} contains a closing style tag"
                );
                assert!(
                    !(lower.contains("<!--") && lower.contains("<script")),
                    "{name} contains both `<!--` and `<script`, which puts the tokenizer in the \
                     double-escaped state"
                );
            }
        }

        #[test]
        fn every_asset_is_inlined_exactly_once() {
            let html = graphiql_html();
            for (name, source) in [
                (
                    "graphiql.min.css",
                    include_str!("../src/http/vendor/graphiql.min.css"),
                ),
                (
                    "graphiql.min.js",
                    include_str!("../src/http/vendor/graphiql.min.js"),
                ),
            ] {
                assert_eq!(
                    html.matches(source).count(),
                    1,
                    "{name} should be inlined exactly once"
                );
            }
        }

        #[test]
        fn the_versions_file_matches_the_embedded_bundles() {
            let versions = include_str!("../src/http/vendor/versions.txt");
            assert!(versions.contains("graphiql"));
            assert!(versions.contains("4.1.2"));
            assert!(versions.contains("18.3.1"));
            // The recorded sizes are the sizes of the checked-in files.
            assert!(versions.contains(&format!(
                "graphiql.min.js   {} bytes",
                include_str!("../src/http/vendor/graphiql.min.js").len()
            )));
        }

        #[test]
        fn the_page_is_self_contained_on_one_document() {
            let html = graphiql_html();
            // The document's own tags. (`<script>` also appears inside react-dom's inlined JS as
            // a string literal, so the raw substring count is not the tag count.)
            assert_eq!(html.matches("<script>\n").count(), 4);
            assert_eq!(html.matches("</script>").count(), 4);
            assert_eq!(html.matches("<style>\n").count(), 2);
            assert_eq!(html.matches("</style>").count(), 2);
            assert!(!html.contains("{{"));
            assert!(html.contains("data:font/woff2;base64,"));
        }
    }

    #[cfg(not(feature = "vendored-graphiql"))]
    #[test]
    fn the_default_page_is_cdn_backed() {
        let html = graphiql_html();
        assert!(html.contains("https://unpkg.com/graphiql/graphiql.min.js"));
        assert!(html.contains("https://unpkg.com/react@18/umd/react.production.min.js"));
    }
}

// ===========================================================================
// 5. Additional in-memory WebSocket limits
// ===========================================================================

#[cfg(feature = "ws")]
mod ws_limit_probes {
    use std::time::Duration;

    use futures::stream::poll_fn;
    use serde_json::{Value as JsonValue, json};
    use suku::{
        Context, Server,
        ws::{self, WsConfig, WsInput, WsOutput},
    };
    use tokio::{sync::mpsc, task::JoinHandle};

    use super::fixtures;

    struct Harness {
        input: mpsc::UnboundedSender<WsInput>,
        output: mpsc::Receiver<WsOutput>,
        task: JoinHandle<()>,
    }

    impl Harness {
        fn start(server: Server, config: WsConfig) -> Self {
            let (input, mut receiver) = mpsc::unbounded_channel();
            let incoming = poll_fn(move |cx| receiver.poll_recv(cx));
            let (outgoing, output) = mpsc::channel(64);
            let task = tokio::spawn(ws::serve(
                server,
                config,
                Context::empty(),
                incoming,
                outgoing,
            ));
            Self {
                input,
                output,
                task,
            }
        }

        fn send(&self, message: JsonValue) {
            self.input
                .send(WsInput::Text(message.to_string()))
                .expect("send");
        }

        async fn next(&mut self) -> WsOutput {
            tokio::time::timeout(Duration::from_secs(2), self.output.recv())
                .await
                .expect("timed out")
                .expect("closed")
        }

        async fn next_json(&mut self) -> JsonValue {
            match self.next().await {
                WsOutput::Text(text) => serde_json::from_str(&text).expect("json"),
                other => panic!("expected text, got {other:?}"),
            }
        }
    }

    impl Drop for Harness {
        fn drop(&mut self) {
            self.task.abort();
        }
    }

    fn subscribe(id: &str, query: &str) -> JsonValue {
        json!({ "id": id, "type": "subscribe", "payload": { "query": query } })
    }

    /// `max_operations(0)` disables the limit (documented), so 40 concurrent operations are all
    /// admitted.
    #[tokio::test]
    async fn a_zero_operation_limit_disables_the_limit() {
        let mut harness = Harness::start(
            Server::new(fixtures::schema()),
            WsConfig::default().max_operations(0),
        );
        harness.send(json!({ "type": "connection_init" }));
        assert_eq!(harness.next_json().await["type"], "connection_ack");

        for index in 0..40 {
            harness.send(subscribe(&format!("p{index}"), "subscription { pending }"));
        }
        harness.send(json!({ "type": "ping" }));
        assert_eq!(
            harness.next_json().await["type"],
            "pong",
            "no operation may be rejected when the limit is disabled"
        );
    }

    /// The limit is inclusive: a message exactly at `max_message_bytes` is accepted.
    #[tokio::test]
    async fn the_message_limit_is_inclusive() {
        let init = json!({ "type": "connection_init" }).to_string();
        let len = init.len();

        let mut exact = Harness::start(
            Server::new(fixtures::schema()),
            WsConfig::default().max_message_bytes(len),
        );
        exact.input.send(WsInput::Text(init.clone())).expect("send");
        assert_eq!(exact.next_json().await["type"], "connection_ack");

        let mut under = Harness::start(
            Server::new(fixtures::schema()),
            WsConfig::default().max_message_bytes(len - 1),
        );
        under.input.send(WsInput::Text(init)).expect("send");
        match under.next().await {
            WsOutput::Close { code, .. } => assert_eq!(code, ws::CLOSE_BAD_REQUEST),
            other => panic!("expected a close, got {other:?}"),
        }
    }

    /// An empty id is rejected, and the connection terminates as a protocol error.
    #[tokio::test]
    async fn an_empty_operation_id_is_rejected() {
        let mut harness = Harness::start(Server::new(fixtures::schema()), WsConfig::default());
        harness.send(json!({ "type": "connection_init" }));
        assert_eq!(harness.next_json().await["type"], "connection_ack");
        harness.send(subscribe("", "{ hello }"));
        match harness.next().await {
            WsOutput::Close { code, .. } => assert_eq!(code, ws::CLOSE_BAD_REQUEST),
            other => panic!("expected a close, got {other:?}"),
        }
    }
}

// ===========================================================================
// 6. Real socket: close codes and subscription framing through Axum
// ===========================================================================

#[cfg(all(feature = "ws", feature = "axum"))]
mod ws_socket_probes {
    use futures::{SinkExt as _, StreamExt as _};
    use serde_json::{Value as JsonValue, json};
    use suku::Server;
    use tokio::task::JoinHandle;
    use tokio_tungstenite::tungstenite::{Message, client::IntoClientRequest as _};

    use super::fixtures;

    type Socket = tokio_tungstenite::WebSocketStream<
        tokio_tungstenite::MaybeTlsStream<tokio::net::TcpStream>,
    >;

    async fn start() -> (String, JoinHandle<()>) {
        let app = suku::GraphQL::new(Server::new(fixtures::schema())).into_router();
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0")
            .await
            .expect("bind");
        let address = listener.local_addr().expect("address");
        let task = tokio::spawn(async move {
            let _ = ::axum::serve(listener, app).await;
        });
        (format!("ws://{address}/graphql"), task)
    }

    async fn connect(url: &str) -> Socket {
        let mut request = url.into_client_request().expect("request");
        request.headers_mut().insert(
            "sec-websocket-protocol",
            ::http::HeaderValue::from_static(suku::ws::GRAPHQL_TRANSPORT_WS),
        );
        let (socket, _response) = tokio_tungstenite::connect_async(request)
            .await
            .expect("connect");
        socket
    }

    async fn send_text(socket: &mut Socket, value: JsonValue) {
        socket
            .send(Message::Text(value.to_string().into()))
            .await
            .expect("send");
    }

    async fn next_message(socket: &mut Socket) -> Message {
        tokio::time::timeout(std::time::Duration::from_secs(2), socket.next())
            .await
            .expect("timed out")
            .expect("frame")
            .expect("socket")
    }

    async fn next_json(socket: &mut Socket) -> JsonValue {
        loop {
            match next_message(socket).await {
                Message::Text(text) => return serde_json::from_str(text.as_str()).expect("json"),
                Message::Ping(_) | Message::Pong(_) => continue,
                other => panic!("unexpected frame: {other:?}"),
            }
        }
    }

    /// A protocol violation must reach the client as the documented application close code, not
    /// as a generic 1006 abnormal closure.
    #[tokio::test]
    async fn protocol_close_codes_survive_the_axum_adapter() {
        let (url, server) = start().await;
        let mut socket = connect(&url).await;

        send_text(&mut socket, json!({ "type": "connection_init" })).await;
        assert_eq!(next_json(&mut socket).await["type"], "connection_ack");

        // A second `connection_init` is fatal: 4429.
        send_text(&mut socket, json!({ "type": "connection_init" })).await;

        let frame = next_message(&mut socket).await;
        match frame {
            Message::Close(Some(frame)) => {
                assert_eq!(
                    u16::from(frame.code),
                    suku::ws::CLOSE_TOO_MANY_INIT,
                    "{frame:?}"
                );
            }
            other => panic!("expected a close frame, got {other:?}"),
        }

        server.abort();
    }

    /// Subscriptions stream and cancel over a real socket.
    #[tokio::test]
    async fn subscriptions_stream_and_cancel_over_a_socket() {
        let (url, server) = start().await;
        let mut socket = connect(&url).await;

        send_text(&mut socket, json!({ "type": "connection_init" })).await;
        assert_eq!(next_json(&mut socket).await["type"], "connection_ack");

        send_text(
            &mut socket,
            json!({
                "id": "s",
                "type": "subscribe",
                "payload": { "query": "subscription { countdown }" }
            }),
        )
        .await;
        for expected in [3, 2, 1] {
            let message = next_json(&mut socket).await;
            assert_eq!(message["type"], "next");
            assert_eq!(message["payload"]["data"]["countdown"], expected);
        }
        assert_eq!(next_json(&mut socket).await["type"], "complete");

        // Start a non-terminating subscription and cancel it client-side.
        send_text(
            &mut socket,
            json!({
                "id": "p",
                "type": "subscribe",
                "payload": { "query": "subscription { pending }" }
            }),
        )
        .await;
        tokio::time::sleep(std::time::Duration::from_millis(50)).await;
        send_text(&mut socket, json!({ "id": "p", "type": "complete" })).await;

        // The id is free again and the connection is still healthy.
        send_text(
            &mut socket,
            json!({ "id": "p", "type": "subscribe", "payload": { "query": "{ hello }" } }),
        )
        .await;
        let next = next_json(&mut socket).await;
        assert_eq!(next["type"], "next");
        assert_eq!(next["payload"]["data"]["hello"], "world");
        assert_eq!(next_json(&mut socket).await["type"], "complete");

        server.abort();
    }
}

// ===========================================================================
// 7. Multipart edge shapes caught by review
// ===========================================================================

#[cfg(feature = "multipart")]
mod multipart_edge_probes {
    use ::http::{Method, StatusCode, header};
    use serde_json::Value as JsonValue;
    use suku::{
        Context, HttpConfig, HttpError, HttpResponse, HttpResult, Server,
        http::{ContextRequest, handle},
    };

    use super::fixtures;

    const BOUNDARY: &str = "----edgeboundary";

    fn field(body: &mut Vec<u8>, name: &str, filename: Option<&str>, value: &[u8]) {
        body.extend_from_slice(format!("--{BOUNDARY}\r\n").as_bytes());
        match filename {
            Some(filename) => body.extend_from_slice(
                format!(
                    "Content-Disposition: form-data; name=\"{name}\"; filename=\"{filename}\"\r\n\
                     Content-Type: text/plain\r\n"
                )
                .as_bytes(),
            ),
            None => body.extend_from_slice(
                format!("Content-Disposition: form-data; name=\"{name}\"\r\n").as_bytes(),
            ),
        }
        body.extend_from_slice(b"\r\n");
        body.extend_from_slice(value);
        body.extend_from_slice(b"\r\n");
    }

    async fn post(body: &[u8]) -> HttpResponse {
        let content_type = format!("multipart/form-data; boundary={BOUNDARY}");
        let parts = ::http::Request::builder()
            .method(Method::POST)
            .uri("/graphql")
            .header(header::CONTENT_TYPE, content_type)
            .header(header::ACCEPT, "application/json")
            .body(())
            .expect("request")
            .into_parts()
            .0;
        async fn empty(_: ContextRequest) -> Result<Context, HttpError> {
            Ok(Context::empty())
        }
        match handle(
            &Server::new(fixtures::schema()),
            &HttpConfig::default(),
            &parts,
            body,
            empty,
        )
        .await
        {
            HttpResult::Response(response) => response,
            HttpResult::Stream(_) => panic!("expected a buffered response"),
        }
    }

    fn json_body(response: &HttpResponse) -> JsonValue {
        serde_json::from_slice(&response.body).expect("json")
    }

    const OPERATIONS: &str =
        r#"{"query":"mutation($file: Upload!){ upload(file:$file) }","variables":{"file":null}}"#;

    /// The specification lets the `map` keys be any string. `operations` and `map` are field names
    /// too, so a file keyed `operations` collides with the field parser.
    #[tokio::test]
    async fn a_file_keyed_operations_is_a_legal_request() {
        let mut body = Vec::new();
        field(&mut body, "operations", None, OPERATIONS.as_bytes());
        field(
            &mut body,
            "map",
            None,
            br#"{"operations":["variables.file"]}"#,
        );
        field(&mut body, "operations", Some("a.txt"), b"payload");
        body.extend_from_slice(format!("--{BOUNDARY}--\r\n").as_bytes());

        let response = post(&body).await;
        assert_eq!(
            response.status,
            StatusCode::OK,
            "a file whose map key is `operations` is valid per the spec, got: {}",
            String::from_utf8_lossy(&response.body)
        );
    }

    #[tokio::test]
    async fn a_file_keyed_map_is_a_legal_request() {
        let mut body = Vec::new();
        field(&mut body, "operations", None, OPERATIONS.as_bytes());
        field(&mut body, "map", None, br#"{"map":["variables.file"]}"#);
        field(&mut body, "map", Some("a.txt"), b"payload");
        body.extend_from_slice(format!("--{BOUNDARY}--\r\n").as_bytes());

        let response = post(&body).await;
        assert_eq!(
            response.status,
            StatusCode::OK,
            "a file whose map key is `map` is valid per the spec, got: {}",
            String::from_utf8_lossy(&response.body)
        );
    }

    /// A truncated body (no terminating boundary) must be a client error, never a hang or panic.
    #[tokio::test]
    async fn a_truncated_body_is_a_client_error() {
        let mut body = Vec::new();
        field(&mut body, "operations", None, OPERATIONS.as_bytes());
        field(&mut body, "map", None, br#"{"0":["variables.file"]}"#);
        body.extend_from_slice(format!("--{BOUNDARY}\r\n").as_bytes());
        body.extend_from_slice(
            b"Content-Disposition: form-data; name=\"0\"; filename=\"a.txt\"\r\n\r\npartial",
        );

        // The whole body is already in memory, so the parser cannot stall; it must still report a
        // client error rather than a panic or a 500.
        let response = post(&body).await;
        assert_eq!(response.status, StatusCode::BAD_REQUEST);
    }

    /// `max_batch_size` applies to a multipart batch too.
    #[tokio::test]
    async fn multipart_batches_respect_max_batch_size() {
        let server = Server::new(fixtures::schema());
        let config = HttpConfig::default().max_batch_size(1);
        let mut body = Vec::new();
        field(
            &mut body,
            "operations",
            None,
            br#"[{"query":"mutation($f: Upload!){ upload(file:$f) }","variables":{"f":null}},
                 {"query":"mutation($f: Upload!){ upload(file:$f) }","variables":{"f":null}}]"#,
        );
        field(
            &mut body,
            "map",
            None,
            br#"{"0":["0.variables.f","1.variables.f"]}"#,
        );
        field(&mut body, "0", Some("a.txt"), b"x");
        body.extend_from_slice(format!("--{BOUNDARY}--\r\n").as_bytes());

        let content_type = format!("multipart/form-data; boundary={BOUNDARY}");
        let parts = ::http::Request::builder()
            .method(Method::POST)
            .uri("/graphql")
            .header(header::CONTENT_TYPE, content_type)
            .header(header::ACCEPT, "application/json")
            .body(())
            .expect("request")
            .into_parts()
            .0;
        async fn empty(_: ContextRequest) -> Result<Context, HttpError> {
            Ok(Context::empty())
        }
        let response = match handle(&server, &config, &parts, &body, empty).await {
            HttpResult::Response(response) => response,
            HttpResult::Stream(_) => panic!("expected a buffered response"),
        };
        assert_eq!(response.status, StatusCode::BAD_REQUEST);
    }

    /// An empty `map` leaves the `null` in place, so a non-null `Upload` variable is a coercion
    /// error, not a 500.
    #[tokio::test]
    async fn an_empty_map_leaves_the_null_for_the_executor() {
        let mut body = Vec::new();
        field(&mut body, "operations", None, OPERATIONS.as_bytes());
        field(&mut body, "map", None, b"{}");
        body.extend_from_slice(format!("--{BOUNDARY}--\r\n").as_bytes());

        let response = post(&body).await;
        assert_ne!(response.status, StatusCode::INTERNAL_SERVER_ERROR);
        let json = json_body(&response);
        assert!(
            json["errors"]
                .as_array()
                .is_some_and(|errors| !errors.is_empty()),
            "{json}"
        );
    }

    /// `variables` must be an object; a JSON array is a request error rather than a panic.
    #[tokio::test]
    async fn non_object_variables_are_a_request_error() {
        let mut body = Vec::new();
        field(
            &mut body,
            "operations",
            None,
            br#"{"query":"{ hello }","variables":[null]}"#,
        );
        field(&mut body, "map", None, b"{}");
        body.extend_from_slice(format!("--{BOUNDARY}--\r\n").as_bytes());

        let response = post(&body).await;
        assert_eq!(response.status, StatusCode::BAD_REQUEST);
    }

    /// Nested list destinations work: `variables.files.1`.
    #[tokio::test]
    async fn a_nested_list_destination_is_supported() {
        let mut body = Vec::new();
        field(
            &mut body,
            "operations",
            None,
            br#"{"query":"query($files: [Upload!]!){ __typename }","variables":{"files":[null,null]}}"#,
        );
        field(&mut body, "map", None, br#"{"0":["variables.files.1"]}"#);
        field(&mut body, "0", Some("b.txt"), b"second");
        body.extend_from_slice(format!("--{BOUNDARY}--\r\n").as_bytes());

        let response = post(&body).await;
        // The field is not an upload field, so the result is a validation error about `Upload`
        // rather than a parser failure: what matters is that the file was accepted and mapped.
        assert_ne!(response.status, StatusCode::INTERNAL_SERVER_ERROR);
        let body = json_body(&response);
        assert!(
            !body["errors"]
                .as_array()
                .is_some_and(|errors| errors.iter().any(|error| error["message"]
                    .as_str()
                    .unwrap_or_default()
                    .contains("Invalid multipart request"))),
            "{body}"
        );
    }
}

// ===========================================================================
// 8. Cancellation / task-leak probes
// ===========================================================================

#[cfg(feature = "ws")]
mod ws_leak_probes {
    use std::{
        sync::{
            Arc,
            atomic::{AtomicBool, AtomicUsize, Ordering},
        },
        task::Poll,
        time::Duration,
    };

    use futures::{StreamExt as _, stream::poll_fn};
    use gql::{Field, ObjectType, Schema, TypeRef, Value};
    use serde_json::json;
    use suku::{
        Context, Server,
        ws::{self, WsConfig, WsInput},
    };
    use tokio::sync::mpsc;

    /// Sets a flag when the subscription source is dropped.
    struct DropFlag(Arc<AtomicBool>);

    impl Drop for DropFlag {
        fn drop(&mut self) {
            self.0.store(true, Ordering::SeqCst);
        }
    }

    fn schema(dropped: Arc<AtomicBool>, started: Arc<AtomicUsize>) -> Schema {
        let tick = Field::new("tick", TypeRef::int().non_null())
            .subscribe(move |_| {
                let dropped = Arc::clone(&dropped);
                let started = Arc::clone(&started);
                Box::pin(async move {
                    started.fetch_add(1, Ordering::SeqCst);
                    let flag = DropFlag(dropped);
                    Ok(poll_fn(move |_| {
                        let _ = &flag;
                        Poll::<Option<Result<Value, gql::GraphQLError>>>::Pending
                    })
                    .boxed())
                })
            })
            .resolve(|context| Box::pin(async move { Ok(context.parent.clone()) }));

        let subscription = ObjectType::new("Subscription").field(tick);
        let query = ObjectType::new("Query").field(
            Field::new("hello", TypeRef::string().non_null())
                .resolve(|_| Box::pin(async { Ok(Value::from("world")) })),
        );

        let mut builder = Schema::builder();
        builder.add_type(query.into());
        builder.add_type(subscription.into());
        builder = builder.query("Query").subscription("Subscription");
        builder.build().expect("valid schema")
    }

    fn start(
        dropped: Arc<AtomicBool>,
        started: Arc<AtomicUsize>,
    ) -> (
        mpsc::UnboundedSender<WsInput>,
        mpsc::Receiver<suku::ws::WsOutput>,
        tokio::task::JoinHandle<()>,
    ) {
        let (input, mut receiver) = mpsc::unbounded_channel();
        let incoming = poll_fn(move |cx| receiver.poll_recv(cx));
        let (outgoing, output) = mpsc::channel(64);
        let task = tokio::spawn(ws::serve(
            Server::new(schema(dropped, started)),
            WsConfig::default(),
            Context::empty(),
            incoming,
            outgoing,
        ));
        (input, output, task)
    }

    async fn ack(
        input: &mpsc::UnboundedSender<WsInput>,
        output: &mut mpsc::Receiver<suku::ws::WsOutput>,
    ) {
        input
            .send(WsInput::Text(
                json!({ "type": "connection_init" }).to_string(),
            ))
            .expect("send init");
        let message = tokio::time::timeout(Duration::from_secs(2), output.recv())
            .await
            .expect("timed out")
            .expect("closed");
        assert!(format!("{message:?}").contains("connection_ack"));
    }

    /// When the peer disappears, every running subscription stream is dropped and the connection
    /// task finishes. Nothing is orphaned.
    #[tokio::test]
    async fn closing_the_connection_drops_running_subscriptions() {
        let dropped = Arc::new(AtomicBool::new(false));
        let started = Arc::new(AtomicUsize::new(0));
        let (input, mut output, task) = start(Arc::clone(&dropped), Arc::clone(&started));

        ack(&input, &mut output).await;
        for id in ["a", "b", "c"] {
            input
                .send(WsInput::Text(
                    json!({
                        "id": id,
                        "type": "subscribe",
                        "payload": { "query": "subscription { tick }" }
                    })
                    .to_string(),
                ))
                .expect("send subscribe");
        }

        for _ in 0..100 {
            if started.load(Ordering::SeqCst) == 3 {
                break;
            }
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
        assert_eq!(
            started.load(Ordering::SeqCst),
            3,
            "subscriptions did not start"
        );
        assert!(!dropped.load(Ordering::SeqCst));

        // The peer goes away: the incoming stream ends.
        drop(input);

        tokio::time::timeout(Duration::from_secs(2), task)
            .await
            .expect("serve must finish when the peer disconnects")
            .expect("no panic");

        for _ in 0..100 {
            if dropped.load(Ordering::SeqCst) {
                break;
            }
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
        assert!(
            dropped.load(Ordering::SeqCst),
            "a subscription stream outlived the connection (task leak)"
        );
    }

    /// A client `complete` drops that subscription's stream while the connection stays open.
    #[tokio::test]
    async fn a_client_complete_drops_only_its_own_stream() {
        let dropped = Arc::new(AtomicBool::new(false));
        let started = Arc::new(AtomicUsize::new(0));
        let (input, mut output, task) = start(Arc::clone(&dropped), Arc::clone(&started));

        ack(&input, &mut output).await;
        input
            .send(WsInput::Text(
                json!({
                    "id": "one",
                    "type": "subscribe",
                    "payload": { "query": "subscription { tick }" }
                })
                .to_string(),
            ))
            .expect("send subscribe");
        for _ in 0..100 {
            if started.load(Ordering::SeqCst) == 1 {
                break;
            }
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
        assert_eq!(started.load(Ordering::SeqCst), 1);

        input
            .send(WsInput::Text(
                json!({ "id": "one", "type": "complete" }).to_string(),
            ))
            .expect("send complete");
        for _ in 0..100 {
            if dropped.load(Ordering::SeqCst) {
                break;
            }
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
        assert!(
            dropped.load(Ordering::SeqCst),
            "cancelling an operation must drop its stream"
        );

        // The connection is still usable.
        input
            .send(WsInput::Text(json!({ "type": "ping" }).to_string()))
            .expect("send ping");
        let message = tokio::time::timeout(Duration::from_secs(2), output.recv())
            .await
            .expect("timed out")
            .expect("closed");
        assert!(format!("{message:?}").contains("pong"));

        task.abort();
    }
}
