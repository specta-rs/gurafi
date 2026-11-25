//! Regression for audit finding #17: late response serialization failures.

mod support;

use std::{
    sync::{
        Arc, Mutex,
        atomic::{AtomicUsize, Ordering},
    },
    time::Duration,
};

use ::http::{Method, StatusCode, header};
use gql::{Field, GraphQLError, ObjectType, Schema, TypeRef, Value};
use suku::{ErrorKind, GraphQLRequest, HttpConfig, HttpResult, Server, http::handle};

fn buffered(result: HttpResult) -> suku::HttpResponse {
    match result {
        HttpResult::Response(response) => response,
        HttpResult::Stream(_) => panic!("a query must return a buffered response"),
    }
}

#[cfg(feature = "sse")]
#[tokio::test]
async fn sse_serialization_failure_uses_the_formatted_generic_fallback() {
    use futures::StreamExt as _;

    let event = Field::new("opaque", TypeRef::string().non_null())
        .subscribe(|_| {
            Box::pin(async {
                Ok::<gql::SubscriptionStream, GraphQLError>(
                    futures::stream::iter(vec![Ok(Value::from("event"))]).boxed(),
                )
            })
        })
        .resolve(|context| {
            context
                .context
                .set_extension("opaque", Value::external("DUMMY_MODEL"));
            Box::pin(async move { Ok(context.parent.clone()) })
        });
    let query = ObjectType::new("Query").field(
        Field::new("hello", TypeRef::string().non_null())
            .resolve(|_| Box::pin(async { Ok(Value::from("world")) })),
    );
    let subscription = ObjectType::new("Subscription").field(event);
    let mut builder = Schema::builder();
    builder.add_type(query.into());
    builder.add_type(subscription.into());
    let schema = builder
        .query("Query")
        .subscription("Subscription")
        .build()
        .expect("valid schema");

    let calls = Arc::new(AtomicUsize::new(0));
    let kinds = Arc::new(Mutex::new(Vec::new()));
    let seen_calls = Arc::clone(&calls);
    let seen_kinds = Arc::clone(&kinds);
    let server = Server::new(schema).format_error(move |_, context| {
        seen_calls.fetch_add(1, Ordering::SeqCst);
        seen_kinds.lock().expect("lock").push(context.kind());
        GraphQLError::new("redacted")
    });

    let body = serde_json::to_vec(&GraphQLRequest::new("subscription { opaque }"))
        .expect("serialize request");
    let parts = support::parts(
        Method::POST,
        "/graphql",
        &[
            (header::CONTENT_TYPE, "application/json"),
            (header::ACCEPT, "text/event-stream"),
        ],
    );
    let mut stream = match handle(
        &server,
        &HttpConfig::default(),
        &parts,
        &body,
        support::empty_context,
    )
    .await
    {
        HttpResult::Stream(response) => response.stream,
        HttpResult::Response(response) => panic!("unexpected buffered response: {response:?}"),
    };

    let event = tokio::time::timeout(Duration::from_secs(1), stream.next())
        .await
        .expect("SSE fallback event")
        .expect("SSE event");
    let raw = String::from_utf8(event.expect("SSE bytes").to_vec()).expect("UTF-8 event");
    assert!(raw.contains("event: next"), "{raw}");
    assert!(!raw.contains("Failed to serialize"), "{raw}");
    assert!(!raw.contains("opaque model value"), "{raw}");
    assert!(!raw.contains("DUMMY_MODEL"), "{raw}");
    let data = raw
        .strip_prefix("event: next\ndata: ")
        .and_then(|value| value.strip_suffix("\n\n"))
        .expect("SSE data frame");
    let json: serde_json::Value = serde_json::from_str(data).expect("JSON event");
    assert!(json.get("data").is_none());
    assert_eq!(json["errors"][0]["message"], "redacted");
    assert_eq!(calls.load(Ordering::SeqCst), 1);
    assert_eq!(*kinds.lock().expect("lock"), vec![ErrorKind::Execution]);

    let complete = tokio::time::timeout(Duration::from_secs(1), stream.next())
        .await
        .expect("SSE completion")
        .expect("complete event");
    assert_eq!(
        complete.expect("SSE bytes"),
        bytes::Bytes::from_static(b"event: complete\n\n")
    );
}

#[tokio::test]
async fn serialization_failure_is_a_formatted_generic_http_500() {
    let hello = Field::new("hello", TypeRef::string().non_null()).resolve(|context| {
        context
            .context
            .set_extension("opaque", Value::external("DUMMY_MODEL"));
        Box::pin(async { Ok(Value::from("world")) })
    });
    let query = ObjectType::new("Query").field(hello);
    let mut builder = Schema::builder();
    builder.add_type(query.into());
    let schema = builder.query("Query").build().expect("valid schema");

    let calls = Arc::new(AtomicUsize::new(0));
    let kinds = Arc::new(Mutex::new(Vec::new()));
    let seen_calls = Arc::clone(&calls);
    let seen_kinds = Arc::clone(&kinds);
    let server = Server::new(schema).format_error(move |_, context| {
        seen_calls.fetch_add(1, Ordering::SeqCst);
        seen_kinds.lock().expect("lock").push(context.kind());
        GraphQLError::new("redacted")
    });

    let body = serde_json::to_vec(&GraphQLRequest::new("{ hello }")).expect("serialize request");
    let parts = support::parts(
        Method::POST,
        "/graphql",
        &[
            (header::CONTENT_TYPE, "application/json"),
            (header::ACCEPT, "application/graphql-response+json"),
        ],
    );
    let response = buffered(
        handle(
            &server,
            &HttpConfig::default(),
            &parts,
            &body,
            support::empty_context,
        )
        .await,
    );

    assert_eq!(response.status, StatusCode::INTERNAL_SERVER_ERROR);
    assert_eq!(calls.load(Ordering::SeqCst), 1);
    assert_eq!(*kinds.lock().expect("lock"), vec![ErrorKind::Execution]);
    assert!(
        response
            .headers
            .get(header::CONTENT_TYPE)
            .and_then(|value| value.to_str().ok())
            .is_some_and(|value| value.starts_with("application/graphql-response+json"))
    );

    let raw = String::from_utf8(response.body).expect("UTF-8 body");
    assert!(!raw.contains("Failed to serialize"));
    assert!(!raw.contains("opaque model value"));
    assert!(!raw.contains("DUMMY_MODEL"));
    let json: serde_json::Value = serde_json::from_str(&raw).expect("JSON body");
    assert!(json.get("data").is_none());
    assert_eq!(json["errors"][0]["message"], "redacted");
}
