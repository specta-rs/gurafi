//! `graphql-transport-ws` protocol tests.
//!
//! Most tests drive [`suku::ws::serve`] over in-memory channels. Two tests at the end use a real
//! Axum server and `tokio-tungstenite` to verify the upgrade, subprotocol negotiation and framing.

#![cfg(feature = "ws")]

mod support;

use std::time::Duration;

use futures::{SinkExt as _, StreamExt as _};
use gql::{Field, InputValue, ObjectType, Schema, TypeRef, Value};
use serde_json::{Value as JsonValue, json};
use suku::{
    Context, PreparedQueries, Server,
    ws::{self, WsConfig, WsInput, WsOutput},
};
use tokio::{sync::mpsc, task::JoinHandle};

#[derive(Clone, Debug, PartialEq)]
struct User {
    name: String,
}

fn schema() -> Schema {
    let hello = Field::new("hello", TypeRef::string().non_null())
        .resolve(|_| Box::pin(async { Ok(Value::from("world")) }));

    let whoami = Field::new("whoami", TypeRef::string().non_null()).resolve(|context| {
        let name = context
            .context
            .require_data::<User>()
            .map(|user| user.name.clone());
        Box::pin(async move { name.map(Value::from) })
    });

    let boom = Field::new("boom", TypeRef::string().non_null()).resolve(|_| {
        Box::pin(async {
            Err::<Value, gql::GraphQLError>(gql::GraphQLError::new("secret database detail"))
        })
    });

    let set = Field::new("set", TypeRef::string().non_null())
        .arg(InputValue::new("value", TypeRef::string().non_null()))
        .resolve(|context| {
            let value = context
                .args
                .get("value")
                .and_then(Value::as_str)
                .unwrap_or_default()
                .to_owned();
            Box::pin(async move { Ok(Value::from(value)) })
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

    let pending = Field::new("pending", TypeRef::int().non_null())
        .subscribe(|_| {
            Box::pin(async {
                Ok(futures::stream::pending::<Result<Value, gql::GraphQLError>>().boxed())
            })
        })
        .resolve(|context| Box::pin(async move { Ok(context.parent.clone()) }));

    let query = ObjectType::new("Query")
        .field(hello)
        .field(whoami)
        .field(boom);
    let mutation = ObjectType::new("Mutation").field(set);
    let subscription = ObjectType::new("Subscription")
        .field(countdown)
        .field(pending);

    let mut builder = Schema::builder();
    builder.add_type(query.into());
    builder.add_type(mutation.into());
    builder.add_type(subscription.into());
    builder = builder
        .query("Query")
        .mutation("Mutation")
        .subscription("Subscription");
    builder.build().expect("valid schema")
}

fn config() -> WsConfig {
    WsConfig::default()
}

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
        let incoming = futures::stream::poll_fn(move |cx| receiver.poll_recv(cx));
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
            .expect("send message");
    }

    fn send_raw(&self, text: &str) {
        self.input
            .send(WsInput::Text(text.to_owned()))
            .expect("send raw message");
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
}

impl Drop for Harness {
    fn drop(&mut self) {
        self.task.abort();
    }
}

async fn ready(server: Server) -> Harness {
    let mut harness = Harness::start(server, config(), Context::empty());
    harness.send(json!({
        "type": "connection_init",
        "payload": { "token": "ignored-by-the-transport" }
    }));
    let ack = harness.next_json().await;
    assert_eq!(ack["type"], "connection_ack");
    harness
}

fn subscribe(id: &str, query: &str) -> JsonValue {
    json!({
        "id": id,
        "type": "subscribe",
        "payload": { "query": query },
    })
}

#[tokio::test]
async fn handshake_acknowledges_the_connection() {
    let mut harness = ready(Server::new(schema())).await;

    // The server may send pings; a client ping must always get a matching pong.
    harness.send(json!({ "type": "ping", "payload": { "nonce": 7 } }));
    let pong = harness.next_json().await;
    assert_eq!(pong["type"], "pong");
    assert_eq!(pong["payload"]["nonce"], 7);

    // The connection stays open and usable.
    harness.send(subscribe("q1", "{ hello }"));
    assert_eq!(harness.next_json().await["type"], "next");
    assert_eq!(harness.next_json().await["type"], "complete");
}

#[tokio::test]
async fn query_produces_next_then_complete() {
    let mut harness = ready(Server::new(schema())).await;
    harness.send(subscribe("q1", "{ hello }"));

    let next = harness.next_json().await;
    assert_eq!(next["id"], "q1");
    assert_eq!(next["type"], "next");
    assert_eq!(next["payload"]["data"]["hello"], "world");

    let complete = harness.next_json().await;
    assert_eq!(complete["id"], "q1");
    assert_eq!(complete["type"], "complete");
}

#[tokio::test]
async fn mutation_produces_one_result_then_complete() {
    let mut harness = ready(Server::new(schema())).await;
    harness.send(subscribe("m1", r#"mutation { set(value: "done") }"#));

    let next = harness.next_json().await;
    assert_eq!(next["payload"]["data"]["set"], "done");
    assert_eq!(next["type"], "next");
    assert_eq!(harness.next_json().await["type"], "complete");
}

#[tokio::test]
async fn subscription_streams_each_event_then_completes() {
    let mut harness = ready(Server::new(schema())).await;
    harness.send(subscribe("s1", "subscription { countdown }"));

    for value in [3, 2, 1] {
        let next = harness.next_json().await;
        assert_eq!(next["id"], "s1");
        assert_eq!(next["type"], "next");
        assert_eq!(next["payload"]["data"]["countdown"], value);
    }
    let complete = harness.next_json().await;
    assert_eq!(complete["type"], "complete");
    assert_eq!(complete["id"], "s1");
}

#[tokio::test]
async fn client_complete_cancels_a_subscription() {
    let mut harness = ready(Server::new(schema())).await;

    harness.send(subscribe("s1", "subscription { pending }"));
    harness.send(json!({ "id": "s1", "type": "complete" }));

    // Nothing for this operation should be emitted; the next message is the pong from a ping.
    harness.send(json!({ "type": "ping" }));
    let pong = harness.next_json().await;
    assert_eq!(pong["type"], "pong");

    // The id can be reused after the client completes it.
    harness.send(subscribe("s1", "{ hello }"));
    assert_eq!(harness.next_json().await["type"], "next");
    assert_eq!(harness.next_json().await["type"], "complete");
}

#[tokio::test]
async fn duplicate_ids_close_the_socket() {
    let mut harness = ready(Server::new(schema())).await;

    harness.send(subscribe("same", "subscription { pending }"));
    harness.send(subscribe("same", "subscription { pending }"));

    match harness.next().await {
        Output::Close { code, reason } => {
            assert_eq!(code, ws::CLOSE_SUBSCRIBER_EXISTS);
            assert!(reason.contains("already exists"));
        }
        other => panic!("expected a close message, got {other:?}"),
    }
}

#[tokio::test]
async fn unknown_operations_are_reported_as_errors() {
    let mut harness = ready(Server::new(schema())).await;
    harness.send(subscribe("q1", "{ nope }"));

    let error = harness.next_json().await;
    assert_eq!(error["id"], "q1");
    assert_eq!(error["type"], "error");
    assert!(
        error["payload"]
            .as_array()
            .is_some_and(|errors| !errors.is_empty())
    );
    assert!(
        error["payload"][0]["message"]
            .as_str()
            .unwrap_or_default()
            .contains("nope")
    );
}

#[tokio::test]
async fn malformed_json_closes_4400() {
    let mut harness = Harness::start(Server::new(schema()), config(), Context::empty());
    harness.send_raw("this is not JSON");

    match harness.next().await {
        Output::Close { code, .. } => assert_eq!(code, ws::CLOSE_BAD_REQUEST),
        other => panic!("expected a close message, got {other:?}"),
    }
}

#[tokio::test]
async fn unknown_message_types_close_4400() {
    let mut harness = ready(Server::new(schema())).await;
    harness.send(json!({ "type": "nonsense" }));

    match harness.next().await {
        Output::Close { code, .. } => assert_eq!(code, ws::CLOSE_BAD_REQUEST),
        other => panic!("expected a close message, got {other:?}"),
    }
}

#[tokio::test]
async fn subscribe_before_init_closes_4401() {
    let mut harness = Harness::start(Server::new(schema()), config(), Context::empty());
    harness.send(subscribe("q1", "{ hello }"));

    match harness.next().await {
        Output::Close { code, .. } => assert_eq!(code, ws::CLOSE_UNAUTHORIZED),
        other => panic!("expected a close message, got {other:?}"),
    }
}

#[tokio::test]
async fn duplicate_connection_init_closes_4429() {
    let mut harness = ready(Server::new(schema())).await;
    harness.send(json!({ "type": "connection_init" }));

    match harness.next().await {
        Output::Close { code, .. } => assert_eq!(code, ws::CLOSE_TOO_MANY_INIT),
        other => panic!("expected a close message, got {other:?}"),
    }
}

#[tokio::test]
async fn missing_connection_init_times_out() {
    let config = WsConfig::default().connection_init_timeout(Some(Duration::from_millis(20)));
    let mut harness = Harness::start(Server::new(schema()), config, Context::empty());

    match harness.next().await {
        Output::Close { code, .. } => assert_eq!(code, ws::CLOSE_INIT_TIMEOUT),
        other => panic!("expected a close message, got {other:?}"),
    }
}

#[tokio::test]
async fn oversized_messages_close_4400() {
    let config = WsConfig::default().max_message_bytes(16);
    let mut harness = Harness::start(Server::new(schema()), config, Context::empty());
    harness.send(json!({ "type": "connection_init" }));

    match harness.next().await {
        Output::Close { code, .. } => assert_eq!(code, ws::CLOSE_BAD_REQUEST),
        other => panic!("expected a close message, got {other:?}"),
    }
}

#[tokio::test]
async fn context_is_shared_by_every_operation_on_the_connection() {
    let mut harness = Harness::start(
        Server::new(schema()),
        config(),
        Context::new(User {
            name: "Ada".to_owned(),
        }),
    );
    harness.send(json!({ "type": "connection_init" }));
    assert_eq!(harness.next_json().await["type"], "connection_ack");

    for id in ["a", "b"] {
        harness.send(subscribe(id, "{ whoami }"));
        let next = harness.next_json().await;
        assert_eq!(next["payload"]["data"]["whoami"], "Ada");
        assert_eq!(harness.next_json().await["type"], "complete");
    }
}

#[tokio::test]
async fn depth_limit_is_enforced() {
    let server = Server::new(schema()).max_depth(1);
    let mut harness = ready(server).await;
    harness.send(subscribe("q1", "{ __schema { queryType { name } } }"));

    let error = harness.next_json().await;
    assert_eq!(error["type"], "error");
    assert!(
        error["payload"][0]["message"]
            .as_str()
            .unwrap_or_default()
            .to_lowercase()
            .contains("depth")
            || error["payload"][0]["message"]
                .as_str()
                .unwrap_or_default()
                .to_lowercase()
                .contains("deep")
    );
}

#[tokio::test]
async fn complexity_limit_is_enforced() {
    let server = Server::new(schema()).max_complexity(1);
    let mut harness = ready(server).await;
    harness.send(subscribe("q1", "{ a: hello b: hello c: hello d: hello }"));

    let error = harness.next_json().await;
    assert_eq!(error["type"], "error");
    assert!(
        error["payload"][0]["message"]
            .as_str()
            .unwrap_or_default()
            .to_lowercase()
            .contains("complex")
    );
}

#[tokio::test]
async fn prepared_queries_resolve_by_identifier() {
    let schema = schema();
    let prepared = PreparedQueries::from_map(&schema, [("hello", "{ hello }")]).expect("manifest");
    let server = Server::new(schema).prepared_queries(prepared);

    let mut harness = ready(server).await;
    harness.send(json!({
        "id": "p1",
        "type": "subscribe",
        "payload": { "id": "hello" },
    }));

    let next = harness.next_json().await;
    assert_eq!(next["type"], "next");
    assert_eq!(next["payload"]["data"]["hello"], "world");
    assert_eq!(harness.next_json().await["type"], "complete");
}

#[tokio::test]
async fn error_formatter_applies_to_websocket_responses() {
    let server = Server::new(schema()).format_error(suku::mask_internal);
    let mut harness = ready(server).await;
    harness.send(subscribe("q1", "{ boom }"));

    let next = harness.next_json().await;
    assert_eq!(next["type"], "next");
    assert_eq!(
        next["payload"]["errors"][0]["message"],
        "Internal server error"
    );
    assert_eq!(harness.next_json().await["type"], "complete");
}

#[tokio::test]
async fn max_operations_rejects_excess_subscriptions_without_closing() {
    let config = WsConfig::default().max_operations(1);
    let mut harness = Harness::start(Server::new(schema()), config, Context::empty());
    harness.send(json!({ "type": "connection_init" }));
    assert_eq!(harness.next_json().await["type"], "connection_ack");

    harness.send(subscribe("a", "subscription { pending }"));
    harness.send(subscribe("b", "subscription { pending }"));

    let error = harness.next_json().await;
    assert_eq!(error["id"], "b");
    assert_eq!(error["type"], "error");

    // The connection remains usable.
    harness.send(json!({ "type": "ping" }));
    assert_eq!(harness.next_json().await["type"], "pong");
}

#[tokio::test]
async fn binary_frames_are_rejected() {
    let mut harness = ready(Server::new(schema())).await;
    harness
        .input
        .send(WsInput::Invalid(
            "binary messages are not supported".to_owned(),
        ))
        .expect("send invalid frame");

    match harness.next().await {
        Output::Close { code, .. } => assert_eq!(code, ws::CLOSE_BAD_REQUEST),
        other => panic!("expected a close message, got {other:?}"),
    }
}

#[tokio::test]
async fn authorizer_denials_reach_websocket_clients() {
    struct DenyHello;

    impl gql::Authorizer for DenyHello {
        fn authorize(
            &self,
            context: &gql::AuthorizationContext<'_>,
        ) -> Result<(), gql::GraphQLError> {
            if context.field_name == "hello" {
                Err(gql::GraphQLError::new("Not authorized"))
            } else {
                Ok(())
            }
        }
    }

    let server = Server::new(schema()).authorizer(DenyHello);
    let mut harness = ready(server).await;
    harness.send(subscribe("q1", "{ hello }"));

    let next = harness.next_json().await;
    assert_eq!(next["type"], "next");
    assert_eq!(next["payload"]["errors"][0]["message"], "Not authorized");
    assert_eq!(harness.next_json().await["type"], "complete");
}

// ---------------------------------------------------------------------------
// Real Axum/Tokio socket test
// ---------------------------------------------------------------------------

async fn axum_server(app: ::axum::Router) -> (std::net::SocketAddr, JoinHandle<()>) {
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0")
        .await
        .expect("bind");
    let address = listener.local_addr().expect("address");
    let task = tokio::spawn(async move {
        let _ = ::axum::serve(listener, app).await;
    });
    (address, task)
}

async fn connect(
    address: std::net::SocketAddr,
) -> tokio_tungstenite::WebSocketStream<tokio_tungstenite::MaybeTlsStream<tokio::net::TcpStream>> {
    use tokio_tungstenite::tungstenite::client::IntoClientRequest as _;

    let mut request = format!("ws://{address}/graphql")
        .into_client_request()
        .expect("request");
    request.headers_mut().insert(
        "sec-websocket-protocol",
        ::http::HeaderValue::from_static(ws::GRAPHQL_TRANSPORT_WS),
    );
    let (socket, response) = tokio_tungstenite::connect_async(request)
        .await
        .expect("connect");
    assert_eq!(
        response
            .headers()
            .get("sec-websocket-protocol")
            .and_then(|value| value.to_str().ok()),
        Some(ws::GRAPHQL_TRANSPORT_WS)
    );
    socket
}

async fn next_socket_json<S>(socket: &mut S) -> JsonValue
where
    S: futures::Sink<tokio_tungstenite::tungstenite::Message>
        + futures::Stream<
            Item = Result<
                tokio_tungstenite::tungstenite::Message,
                tokio_tungstenite::tungstenite::Error,
            >,
        > + Unpin,
{
    use tokio_tungstenite::tungstenite::Message;

    loop {
        let message = socket.next().await.expect("frame").expect("websocket");
        match message {
            Message::Text(text) => return serde_json::from_str(text.as_str()).expect("JSON"),
            Message::Ping(_) | Message::Pong(_) => continue,
            other => panic!("unexpected frame: {other:?}"),
        }
    }
}

#[tokio::test]
async fn axum_socket_handshake_and_query() {
    use tokio_tungstenite::tungstenite::Message;

    let app = suku::GraphQL::new(support::server()).into_router();
    let (address, server) = axum_server(app).await;
    let mut socket = connect(address).await;

    socket
        .send(Message::Text(
            r#"{"type":"connection_init"}"#.to_owned().into(),
        ))
        .await
        .expect("init");
    assert_eq!(
        next_socket_json(&mut socket).await["type"],
        "connection_ack"
    );

    socket
        .send(Message::Text(
            serde_json::to_string(&subscribe("q1", "{ hello }"))
                .expect("message")
                .into(),
        ))
        .await
        .expect("subscribe");
    let next = next_socket_json(&mut socket).await;
    assert_eq!(next["payload"]["data"]["hello"], "world");
    assert_eq!(next_socket_json(&mut socket).await["type"], "complete");

    server.abort();
}

#[tokio::test]
async fn axum_socket_requires_the_graphql_transport_ws_subprotocol() {
    use tokio_tungstenite::tungstenite::{Error, client::IntoClientRequest as _};

    let app = suku::GraphQL::new(support::server()).into_router();
    let (address, server) = axum_server(app).await;

    let request = format!("ws://{address}/graphql")
        .into_client_request()
        .expect("request");
    match tokio_tungstenite::connect_async(request).await {
        Err(Error::Http(response)) => assert_eq!(response.status(), 400),
        other => panic!("expected an HTTP rejection, got {other:?}"),
    }

    server.abort();
}

#[tokio::test]
async fn axum_rejects_an_unauthorized_upgrade_before_upgrading() {
    use ::axum::{
        body::Body,
        http::{Method, Request, StatusCode, header},
    };
    use tower::ServiceExt as _;

    let app = suku::GraphQL::new(support::server())
        .with_context(|request: suku::http::ContextRequest| async move {
            match request
                .header("authorization")
                .and_then(|value| value.to_str().ok())
            {
                Some("Bearer secret") => Ok(Context::new(User {
                    name: "Ada".to_owned(),
                })),
                _ => Err(suku::HttpError::message(
                    StatusCode::UNAUTHORIZED,
                    "Missing bearer token",
                )),
            }
        })
        .into_router();

    let request = Request::builder()
        .method(Method::GET)
        .uri("/graphql")
        .header(header::CONNECTION, "Upgrade")
        .header(header::UPGRADE, "websocket")
        .header(header::SEC_WEBSOCKET_PROTOCOL, ws::GRAPHQL_TRANSPORT_WS)
        .header(header::SEC_WEBSOCKET_KEY, "dGhlIHNhbXBsZSBub25jZQ==")
        .header(header::SEC_WEBSOCKET_VERSION, "13")
        .body(Body::empty())
        .expect("request");

    let response = app.oneshot(request).await.expect("response");
    assert_eq!(response.status(), StatusCode::UNAUTHORIZED);
}
