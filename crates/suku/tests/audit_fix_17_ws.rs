//! WebSocket adapter regressions for audit finding #17: late response serialization failures.

#![cfg(feature = "ws")]

mod support;

use std::{
    sync::{Arc, Mutex},
    time::Duration,
};

use futures::{StreamExt as _, stream};
use gql::{Field, GraphQLError, ObjectType, Schema, TypeRef, Value};
use serde_json::{Value as JsonValue, json};
use suku::{
    Context, ErrorKind, PreparedQueries, Server,
    ws::{self, WsConfig, WsInput, WsOutput},
};
use tokio::{sync::mpsc, task::JoinHandle};

#[derive(Debug, PartialEq, Eq)]
struct Observation {
    kind: ErrorKind,
    operation_name: Option<String>,
    request_id: Option<String>,
}

type Observations = Arc<Mutex<Vec<Observation>>>;

fn opaque_error(message: &str) -> GraphQLError {
    GraphQLError::new(message.to_owned()).extend("opaque", Value::external("DUMMY_MODEL"))
}

fn formatted_server(
    schema: Schema,
    observations: &Observations,
    unserializable_fallback: bool,
) -> Server {
    let observations = Arc::clone(observations);
    Server::new(schema).format_error(move |error, context| {
        observations.lock().expect("lock").push(Observation {
            kind: context.kind(),
            operation_name: context.operation_name().map(str::to_owned),
            request_id: context.request_id().map(str::to_owned),
        });
        if context.is_execution() {
            if unserializable_fallback {
                opaque_error("Internal server error")
            } else {
                GraphQLError::new("redacted")
            }
        } else {
            opaque_error(error.message())
        }
    })
}

fn opaque_schema() -> Schema {
    let query = ObjectType::new("Query").field(
        Field::new("opaque", TypeRef::string().non_null()).resolve(|context| {
            context
                .context
                .set_extension("opaque", Value::external("DUMMY_MODEL"));
            Box::pin(async { Ok(Value::from("query result")) })
        }),
    );
    let failing = Field::new("failing", TypeRef::string().non_null()).subscribe(|_| {
        Box::pin(async {
            Err::<gql::SubscriptionStream, GraphQLError>(opaque_error(
                "private subscription setup detail",
            ))
        })
    });
    let subscription = ObjectType::new("Subscription")
        .field(
            Field::new("opaque", TypeRef::string().non_null())
                .subscribe(|_| {
                    Box::pin(async { Ok(stream::iter(vec![Ok(Value::from("event"))]).boxed()) })
                })
                .resolve(|context| {
                    context
                        .context
                        .set_extension("opaque", Value::external("DUMMY_MODEL"));
                    Box::pin(async move { Ok(context.parent.clone()) })
                }),
        )
        .field(failing);
    let mut builder = Schema::builder();
    builder.add_type(query.into());
    builder.add_type(subscription.into());
    builder
        .query("Query")
        .subscription("Subscription")
        .build()
        .expect("valid schema")
}

struct Connection {
    input: mpsc::UnboundedSender<WsInput>,
    output: mpsc::Receiver<WsOutput>,
    task: JoinHandle<()>,
}

impl Connection {
    async fn start(server: Server) -> Self {
        let (input, mut receiver) = mpsc::unbounded_channel();
        let incoming = stream::poll_fn(move |cx| receiver.poll_recv(cx));
        let (outgoing, output) = mpsc::channel(64);
        let task = tokio::spawn(ws::serve(
            server,
            WsConfig::default(),
            Context::empty(),
            incoming,
            outgoing,
        ));
        let mut connection = Self {
            input,
            output,
            task,
        };
        connection.send(json!({"type": "connection_init"}));
        assert_eq!(connection.next_json().await["type"], "connection_ack");
        connection
    }

    fn send(&self, message: JsonValue) {
        self.input
            .send(WsInput::Text(message.to_string()))
            .expect("send message");
    }

    async fn next_output(&mut self) -> WsOutput {
        tokio::time::timeout(Duration::from_secs(2), self.output.recv())
            .await
            .expect("timed out waiting for WebSocket output")
            .expect("WebSocket output channel closed")
    }

    async fn next_text(&mut self) -> String {
        match self.next_output().await {
            WsOutput::Text(text) => text,
            WsOutput::Close { code, reason } => {
                panic!("unexpected close {code}: {reason}")
            }
        }
    }

    async fn next_json(&mut self) -> JsonValue {
        serde_json::from_str(&self.next_text().await).expect("protocol JSON")
    }
}

impl Drop for Connection {
    fn drop(&mut self) {
        self.task.abort();
    }
}

fn subscribe(id: &str, query: &str, operation_name: &str) -> JsonValue {
    json!({
        "id": id,
        "type": "subscribe",
        "payload": {"query": query, "operationName": operation_name}
    })
}

fn assert_next_fallback(raw: &str, id: &str, expected: &str) -> JsonValue {
    assert!(!raw.contains("Failed to serialize"), "{raw}");
    assert!(!raw.contains("opaque model value"), "{raw}");
    assert!(!raw.contains("DUMMY_MODEL"), "{raw}");
    let message: JsonValue = serde_json::from_str(raw).expect("protocol JSON");
    assert_eq!(message["id"], id);
    assert_eq!(message["type"], "next");
    assert!(message["payload"].get("data").is_none(), "{message}");
    assert_eq!(
        message["payload"]["errors"].as_array().map(Vec::len),
        Some(1)
    );
    assert_eq!(message["payload"]["errors"][0]["message"], expected);
    message
}

fn assert_error_fallback(raw: &str, id: &str, expected: &str) -> JsonValue {
    assert!(!raw.contains("Failed to serialize"), "{raw}");
    assert!(!raw.contains("opaque model value"), "{raw}");
    assert!(!raw.contains("DUMMY_MODEL"), "{raw}");
    let message: JsonValue = serde_json::from_str(raw).expect("protocol JSON");
    assert_eq!(message["id"], id);
    assert_eq!(message["type"], "error");
    let errors = message["payload"]
        .as_array()
        .expect("error payload must be an array");
    assert!(!errors.is_empty(), "{message}");
    assert_eq!(errors.len(), 1, "{message}");
    assert_eq!(errors[0]["message"], expected);
    message
}

#[tokio::test]
async fn websocket_query_next_serialization_failure_uses_formatted_fallback() {
    let observations = Observations::default();
    let mut connection =
        Connection::start(formatted_server(opaque_schema(), &observations, false)).await;
    connection.send(subscribe(
        "query",
        "query OpaqueQuery { opaque }",
        "OpaqueQuery",
    ));

    assert_next_fallback(&connection.next_text().await, "query", "redacted");
    assert_eq!(connection.next_json().await["type"], "complete");
    assert_eq!(
        *observations.lock().expect("lock"),
        vec![Observation {
            kind: ErrorKind::Execution,
            operation_name: Some("OpaqueQuery".to_owned()),
            request_id: None,
        }]
    );
}

#[tokio::test]
async fn websocket_event_next_serialization_failure_uses_formatted_fallback() {
    let observations = Observations::default();
    let mut connection =
        Connection::start(formatted_server(opaque_schema(), &observations, false)).await;
    connection.send(subscribe(
        "event",
        "subscription OpaqueEvent { opaque }",
        "OpaqueEvent",
    ));

    assert_next_fallback(&connection.next_text().await, "event", "redacted");
    assert_eq!(connection.next_json().await["type"], "complete");
    assert_eq!(
        *observations.lock().expect("lock"),
        vec![Observation {
            kind: ErrorKind::Execution,
            operation_name: Some("OpaqueEvent".to_owned()),
            request_id: None,
        }]
    );
}

#[tokio::test]
async fn websocket_unserializable_formatter_fallback_stays_generic() {
    let observations = Observations::default();
    let mut connection =
        Connection::start(formatted_server(opaque_schema(), &observations, true)).await;
    connection.send(subscribe(
        "query",
        "query OpaqueQuery { opaque }",
        "OpaqueQuery",
    ));

    assert_next_fallback(
        &connection.next_text().await,
        "query",
        suku::INTERNAL_ERROR_MESSAGE,
    );
    assert_eq!(connection.next_json().await["type"], "complete");
    assert_eq!(
        *observations.lock().expect("lock"),
        vec![Observation {
            kind: ErrorKind::Execution,
            operation_name: Some("OpaqueQuery".to_owned()),
            request_id: None,
        }]
    );
}

#[tokio::test]
async fn websocket_request_error_serialization_failure_uses_formatted_fallback() {
    let observations = Observations::default();
    let mut connection =
        Connection::start(formatted_server(opaque_schema(), &observations, false)).await;
    connection.send(subscribe("invalid", "query InvalidQuery {", "InvalidQuery"));

    assert_error_fallback(&connection.next_text().await, "invalid", "redacted");
    assert_eq!(
        *observations.lock().expect("lock"),
        vec![
            Observation {
                kind: ErrorKind::Request,
                operation_name: Some("InvalidQuery".to_owned()),
                request_id: None,
            },
            Observation {
                kind: ErrorKind::Execution,
                operation_name: Some("InvalidQuery".to_owned()),
                request_id: None,
            },
        ]
    );
}

#[tokio::test]
async fn websocket_prepared_schema_mismatch_is_masked_as_a_server_error() {
    let prepared_schema = opaque_schema();
    let prepared = PreparedQueries::from_map(&prepared_schema, [("mismatch", "{ opaque }")])
        .expect("prepared queries");
    let observations = Observations::default();
    let server =
        formatted_server(support::schema(), &observations, false).prepared_queries(prepared);
    let mut connection = Connection::start(server).await;
    connection.send(json!({
        "id": "mismatch",
        "type": "subscribe",
        "payload": {
            "extensions": { "persistedQuery": { "id": "mismatch" } }
        }
    }));

    let raw = connection.next_text().await;
    assert!(!raw.contains("different schema instance"), "{raw}");
    let message: JsonValue = serde_json::from_str(&raw).expect("protocol JSON");
    assert_eq!(message["id"], "mismatch");
    assert_eq!(message["type"], "error");
    assert_eq!(message["payload"].as_array().map(Vec::len), Some(1));
    assert_eq!(message["payload"][0]["message"], "redacted");
    assert_eq!(
        *observations.lock().expect("lock"),
        vec![Observation {
            kind: ErrorKind::Execution,
            operation_name: None,
            request_id: Some("mismatch".to_owned()),
        }]
    );
}

#[tokio::test]
async fn websocket_subscription_setup_error_serialization_failure_stays_generic() {
    let observations = Observations::default();
    let mut connection =
        Connection::start(formatted_server(opaque_schema(), &observations, true)).await;
    connection.send(subscribe(
        "setup",
        "subscription OpaqueSetup { failing }",
        "OpaqueSetup",
    ));

    let raw = connection.next_text().await;
    assert!(!raw.contains("private subscription setup detail"), "{raw}");
    assert_error_fallback(&raw, "setup", suku::INTERNAL_ERROR_MESSAGE);
    assert_eq!(
        *observations.lock().expect("lock"),
        vec![
            Observation {
                kind: ErrorKind::Execution,
                operation_name: Some("OpaqueSetup".to_owned()),
                request_id: None,
            },
            Observation {
                kind: ErrorKind::Execution,
                operation_name: Some("OpaqueSetup".to_owned()),
                request_id: None,
            },
        ]
    );
}
