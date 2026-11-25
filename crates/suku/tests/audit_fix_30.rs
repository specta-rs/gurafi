//! Regression for the WebSocket active-id generation race (audit finding #30).

#![cfg(feature = "ws")]

use std::{
    sync::{
        Arc,
        atomic::{AtomicBool, Ordering},
    },
    time::Duration,
};

use futures::stream;
use gql::{Field, ObjectType, Schema, TypeRef, Value};
use serde_json::{Value as JsonValue, json};
use suku::{
    Context, Server,
    ws::{self, WsConfig, WsInput, WsOutput},
};
use tokio::{sync::mpsc, task::JoinHandle};

struct DropSignal(Arc<tokio::sync::Notify>);

impl Drop for DropSignal {
    fn drop(&mut self) {
        self.0.notify_one();
    }
}

fn schema(
    started: mpsc::UnboundedSender<&'static str>,
    first_dropped: Arc<tokio::sync::Notify>,
    release_two: Arc<tokio::sync::Notify>,
    completed_two: Arc<AtomicBool>,
) -> Schema {
    let first = Field::new("first", TypeRef::string().non_null()).resolve({
        let started = started.clone();
        let first_dropped = Arc::clone(&first_dropped);
        move |_| {
            let started = started.clone();
            let first_dropped = Arc::clone(&first_dropped);
            Box::pin(async move {
                let _drop_signal = DropSignal(first_dropped);
                let _ = started.send("first");
                futures::future::pending::<()>().await;
                Ok(Value::from("first"))
            })
        }
    });

    let second = Field::new("second", TypeRef::string().non_null()).resolve({
        let started = started.clone();
        let first_dropped = Arc::clone(&first_dropped);
        let release_two = Arc::clone(&release_two);
        let completed_two = Arc::clone(&completed_two);
        move |_| {
            let started = started.clone();
            let first_dropped = Arc::clone(&first_dropped);
            let release_two = Arc::clone(&release_two);
            let completed_two = Arc::clone(&completed_two);
            Box::pin(async move {
                // Do not start the successor until the aborted predecessor has unwound.
                first_dropped.notified().await;
                let _ = started.send("second");
                release_two.notified().await;
                completed_two.store(true, Ordering::SeqCst);
                Ok(Value::from("second"))
            })
        }
    });

    let third = Field::new("third", TypeRef::string().non_null()).resolve({
        let started = started.clone();
        move |_| {
            let started = started.clone();
            Box::pin(async move {
                let _ = started.send("third");
                futures::future::pending::<()>().await;
                Ok(Value::from("third"))
            })
        }
    });

    let hello = Field::new("hello", TypeRef::string().non_null())
        .resolve(|_| Box::pin(async { Ok(Value::from("world")) }));
    let query = ObjectType::new("Query")
        .field(first)
        .field(second)
        .field(third)
        .field(hello);

    let mut builder = Schema::builder();
    builder.add_type(query.into());
    builder.query("Query").build().expect("valid schema")
}

fn subscribe(id: &str, query: &str) -> JsonValue {
    json!({"id": id, "type": "subscribe", "payload": {"query": query}})
}

async fn recv_output(receiver: &mut mpsc::Receiver<WsOutput>) -> WsOutput {
    tokio::time::timeout(Duration::from_secs(2), receiver.recv())
        .await
        .expect("timed out waiting for WebSocket output")
        .expect("WebSocket output channel closed")
}

async fn recv_json(receiver: &mut mpsc::Receiver<WsOutput>) -> JsonValue {
    match recv_output(receiver).await {
        WsOutput::Text(text) => serde_json::from_str(&text).expect("protocol JSON"),
        WsOutput::Close { code, reason } => panic!("unexpected close {code}: {reason}"),
    }
}

async fn wait_started(
    receiver: &mut mpsc::UnboundedReceiver<&'static str>,
    expected: &'static str,
) {
    let actual = tokio::time::timeout(Duration::from_secs(2), receiver.recv())
        .await
        .expect("timed out waiting for resolver to start")
        .expect("resolver notification closed");
    assert_eq!(actual, expected);
}

#[tokio::test(flavor = "current_thread")]
async fn stale_cleanup_cannot_remove_a_successor_reservation() {
    let (started_tx, mut started_rx) = mpsc::unbounded_channel();
    let first_dropped = Arc::new(tokio::sync::Notify::new());
    let release_two = Arc::new(tokio::sync::Notify::new());
    let completed_two = Arc::new(AtomicBool::new(false));
    let server = Server::new(schema(
        started_tx,
        Arc::clone(&first_dropped),
        Arc::clone(&release_two),
        Arc::clone(&completed_two),
    ));

    let (input_tx, mut input_rx) = mpsc::unbounded_channel::<WsInput>();
    let incoming = stream::poll_fn(move |cx| input_rx.poll_recv(cx));
    let (output_tx, mut output_rx) = mpsc::channel(64);
    let connection: JoinHandle<()> = tokio::spawn(ws::serve(
        server,
        WsConfig::default().max_operations(1),
        Context::empty(),
        incoming,
        output_tx,
    ));

    input_tx
        .send(WsInput::Text(
            json!({"type": "connection_init"}).to_string(),
        ))
        .expect("init");
    assert_eq!(recv_json(&mut output_rx).await["type"], "connection_ack");

    input_tx
        .send(WsInput::Text(subscribe("same", "{ first }").to_string()))
        .expect("first operation");
    wait_started(&mut started_rx, "first").await;

    // Queue all three frames before the aborted task gets another turn on this runtime.
    input_tx
        .send(WsInput::Text(
            json!({"id": "same", "type": "complete"}).to_string(),
        ))
        .expect("complete first operation");
    input_tx
        .send(WsInput::Text(subscribe("same", "{ second }").to_string()))
        .expect("reused id");
    input_tx
        .send(WsInput::Text(
            subscribe("cap-before", "{ hello }").to_string(),
        ))
        .expect("first cap probe");

    let cap_before = recv_json(&mut output_rx).await;
    assert_eq!(cap_before["id"], "cap-before");
    assert_eq!(cap_before["type"], "error");

    // The predecessor's FinishGuard has now dropped. Its cleanup must not remove the
    // successor's reservation, so the same cap still rejects another operation.
    wait_started(&mut started_rx, "second").await;
    input_tx
        .send(WsInput::Text(
            subscribe("cap-after", "{ hello }").to_string(),
        ))
        .expect("second cap probe");
    let cap_after = recv_json(&mut output_rx).await;
    assert_eq!(cap_after["id"], "cap-after");
    assert_eq!(cap_after["type"], "error");

    // The successor still owns both the reservation and its abort handle.
    input_tx
        .send(WsInput::Text(subscribe("same", "{ third }").to_string()))
        .expect("duplicate reused id");
    match recv_output(&mut output_rx).await {
        WsOutput::Close { code, reason } => {
            assert_eq!(code, ws::CLOSE_SUBSCRIBER_EXISTS);
            assert!(reason.contains("already exists"));
        }
        other => panic!("expected duplicate-id close, got {other:?}"),
    }

    tokio::time::timeout(Duration::from_secs(2), connection)
        .await
        .expect("connection did not stop after duplicate-id close")
        .expect("connection task panicked");

    // Every admitted task was aborted. Releasing the successor must not produce output
    // or leave a task retaining the output sender.
    release_two.notify_one();
    let lingering = tokio::time::timeout(Duration::from_secs(2), output_rx.recv())
        .await
        .expect("an operation retained the output channel after disconnect");
    assert!(lingering.is_none(), "unexpected output after disconnect");
    assert!(!completed_two.load(Ordering::SeqCst));
}
