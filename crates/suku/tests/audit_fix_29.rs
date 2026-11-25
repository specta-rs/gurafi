//! Regression for audit finding #29: hard deadlines for subscription events.

#![cfg(all(feature = "sse", feature = "axum"))]

use std::{
    sync::{
        Arc, Mutex,
        atomic::{AtomicUsize, Ordering},
    },
    time::Duration,
};

use futures::{StreamExt as _, future::pending, stream};
use gql::{Field, GraphQLError, ObjectType, Schema, TypeRef, Value};
use http::{Method, Request, header};
use suku::{Context, ErrorKind, HttpConfig, HttpResult, Server, http::handle, mask_internal};

struct ActiveGuard(Arc<AtomicUsize>);

impl Drop for ActiveGuard {
    fn drop(&mut self) {
        self.0.fetch_sub(1, Ordering::SeqCst);
    }
}

fn schema(active: Arc<AtomicUsize>, started: Arc<AtomicUsize>) -> Schema {
    let query = ObjectType::new("Query").field(
        Field::new("hello", TypeRef::string().non_null())
            .resolve(|_| Box::pin(async { Ok(Value::from("world")) })),
    );
    let stall = Field::new("stall", TypeRef::int().non_null())
        .subscribe(|_| Box::pin(async { Ok(stream::iter(vec![Ok(Value::from(1))]).boxed()) }))
        .resolve(move |_| {
            let active = Arc::clone(&active);
            let started = Arc::clone(&started);
            Box::pin(async move {
                active.fetch_add(1, Ordering::SeqCst);
                started.fetch_add(1, Ordering::SeqCst);
                let _guard = ActiveGuard(Arc::clone(&active));
                pending::<Result<Value, GraphQLError>>().await
            })
        });
    let subscription = ObjectType::new("Subscription").field(stall);
    let mut builder = Schema::builder();
    builder.add_type(query.into());
    builder.add_type(subscription.into());
    builder
        .query("Query")
        .subscription("Subscription")
        .build()
        .expect("valid schema")
}

fn idle_schema() -> Schema {
    let query = ObjectType::new("Query").field(
        Field::new("hello", TypeRef::string().non_null())
            .resolve(|_| Box::pin(async { Ok(Value::from("world")) })),
    );
    let delayed = Field::new("stall", TypeRef::int().non_null())
        .subscribe(|_| {
            Box::pin(async {
                Ok::<gql::SubscriptionStream, GraphQLError>(
                    stream::once(async {
                        tokio::time::sleep(Duration::from_millis(60)).await;
                        Ok(Value::from(1))
                    })
                    .boxed(),
                )
            })
        })
        .resolve(|_| Box::pin(async { Ok(Value::from(1)) }));
    let subscription = ObjectType::new("Subscription").field(delayed);
    let mut builder = Schema::builder();
    builder.add_type(query.into());
    builder.add_type(subscription.into());
    builder
        .query("Query")
        .subscription("Subscription")
        .build()
        .expect("valid schema")
}

async fn open_stream(server: &Server) -> suku::http::SseResponse {
    let request = Request::builder()
        .method(Method::POST)
        .uri("/graphql")
        .header(header::CONTENT_TYPE, "application/json")
        .header(header::ACCEPT, "text/event-stream")
        .body(())
        .expect("request");
    let (parts, ()) = request.into_parts();
    match handle(
        server,
        &HttpConfig::default(),
        &parts,
        br#"{"query":"subscription { stall }"}"#,
        |_| async { Ok(Context::empty()) },
    )
    .await
    {
        HttpResult::Stream(response) => response,
        HttpResult::Response(response) => panic!("unexpected buffered response: {response:?}"),
    }
}

#[tokio::test]
async fn event_timeout_cancels_retained_work_for_each_stream() {
    let active = Arc::new(AtomicUsize::new(0));
    let started = Arc::new(AtomicUsize::new(0));
    let calls = Arc::new(AtomicUsize::new(0));
    let kinds = Arc::new(Mutex::new(Vec::new()));
    let seen_calls = Arc::clone(&calls);
    let seen_kinds = Arc::clone(&kinds);
    let server = Server::new(schema(Arc::clone(&active), Arc::clone(&started)))
        .timeout(Duration::from_millis(20))
        .format_error(move |error, context| {
            seen_calls.fetch_add(1, Ordering::SeqCst);
            seen_kinds.lock().expect("lock").push(context.kind());
            mask_internal(error, context)
        });

    let mut first = open_stream(&server).await;
    let mut second = open_stream(&server).await;
    let (events_tx, mut events_rx) = tokio::sync::mpsc::channel(8);
    let first_events = events_tx.clone();
    let first_task = tokio::spawn(async move {
        while let Some(event) = first.stream.next().await {
            if first_events.send(event).await.is_err() {
                return;
            }
        }
    });
    let second_task = tokio::spawn(async move {
        while let Some(event) = second.stream.next().await {
            if events_tx.send(event).await.is_err() {
                return;
            }
        }
    });

    tokio::time::timeout(Duration::from_secs(1), async {
        while started.load(Ordering::SeqCst) < 2 {
            tokio::task::yield_now().await;
        }
    })
    .await
    .expect("both event resolvers started");

    tokio::time::sleep(Duration::from_millis(100)).await;
    assert_eq!(
        active.load(Ordering::SeqCst),
        0,
        "event deadlines must drop pending resolver work without waiting for disconnect"
    );

    let mut next_events = 0;
    let mut complete_events = 0;
    for _ in 0..4 {
        let event = tokio::time::timeout(Duration::from_millis(50), events_rx.recv())
            .await
            .expect("event arrived before the receive deadline")
            .expect("SSE event");
        let text = String::from_utf8(event.expect("SSE event").to_vec()).expect("UTF-8 SSE event");
        if text.contains("event: next") {
            next_events += 1;
            assert!(text.contains(suku::INTERNAL_ERROR_MESSAGE), "{text}");
            assert!(!text.contains("timed out after"), "{text}");
        } else if text.contains("event: complete") {
            complete_events += 1;
        } else {
            panic!("unexpected SSE event: {text}");
        }
    }
    assert_eq!(next_events, 2);
    assert_eq!(complete_events, 2);
    assert_eq!(active.load(Ordering::SeqCst), 0);
    assert_eq!(calls.load(Ordering::SeqCst), 2);
    assert_eq!(*kinds.lock().expect("lock"), vec![ErrorKind::Execution; 2]);

    tokio::time::timeout(Duration::from_millis(50), first_task)
        .await
        .expect("first polling task completed")
        .expect("first polling task did not panic");
    tokio::time::timeout(Duration::from_millis(50), second_task)
        .await
        .expect("second polling task completed")
        .expect("second polling task did not panic");
}

#[tokio::test]
async fn waiting_for_a_source_event_does_not_consume_the_event_timeout() {
    let server = Server::new(idle_schema()).timeout(Duration::from_millis(20));
    let mut stream = open_stream(&server).await.stream;

    let event = tokio::time::timeout(Duration::from_secs(1), stream.next())
        .await
        .expect("delayed source event arrived")
        .expect("SSE event");
    let text = String::from_utf8(event.expect("SSE event").to_vec()).expect("UTF-8 SSE event");
    assert!(text.contains("event: next"), "{text}");
    assert!(text.contains("\"stall\":1"), "{text}");
    assert!(!text.contains("timed out"), "{text}");

    let complete = tokio::time::timeout(Duration::from_secs(1), stream.next())
        .await
        .expect("completion arrived")
        .expect("SSE complete event");
    let text = String::from_utf8(complete.expect("SSE event").to_vec()).expect("UTF-8 SSE event");
    assert!(text.contains("event: complete"), "{text}");
}
