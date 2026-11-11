//! Transport-level security tests.
//!
//! These call `suku::http::handle` directly, so the behaviour is exactly what
//! `suku::axum::GraphQL` produces without a socket.

use std::sync::{
    Arc, Mutex,
    atomic::{AtomicUsize, Ordering},
};

use ::http::{Method, StatusCode, header};
use futures::StreamExt as _;
use suku::{
    Context, GraphQLRequest, HttpConfig, HttpError, HttpResult, Server,
    http::{ContextRequest, handle},
};

// -------------------------------------------------------------------------------------------
// Fixtures
// -------------------------------------------------------------------------------------------

fn schema(bumps: Arc<AtomicUsize>) -> gql::Schema {
    let read_bumps = Arc::clone(&bumps);
    let write_bumps = Arc::clone(&bumps);
    let query = gql::ObjectType::new("Query")
        .field(
            gql::Field::new("hello", gql::TypeRef::string().non_null())
                .resolve(|_| Box::pin(async { Ok(gql::Value::from("world")) })),
        )
        .field(
            gql::Field::new("bumps", gql::TypeRef::int().non_null()).resolve(move |_| {
                let bumps = Arc::clone(&read_bumps);
                Box::pin(async move {
                    Ok(gql::Value::Int(
                        i32::try_from(bumps.load(Ordering::SeqCst)).unwrap_or(i32::MAX),
                    ))
                })
            }),
        );

    let mutation = gql::ObjectType::new("Mutation").field(
        gql::Field::new("bump", gql::TypeRef::int().non_null()).resolve(move |_| {
            let bumps = Arc::clone(&write_bumps);
            Box::pin(async move {
                let value = bumps.fetch_add(1, Ordering::SeqCst) + 1;
                Ok(gql::Value::Int(i32::try_from(value).unwrap_or(i32::MAX)))
            })
        }),
    );

    let subscription = gql::ObjectType::new("Subscription").field(
        gql::Field::new("countdown", gql::TypeRef::int().non_null())
            .resolve(|context| Box::pin(async move { Ok(context.parent.clone()) }))
            .subscribe(|_| {
                Box::pin(async { Ok(futures::stream::iter(vec![Ok(gql::Value::from(3))]).boxed()) })
            }),
    );

    let mut builder = gql::Schema::builder();
    builder.add_type(query.into());
    builder.add_type(mutation.into());
    builder.add_type(subscription.into());
    let builder = builder.mutation("Mutation").subscription("Subscription");
    builder.build().expect("valid schema")
}

fn server(bumps: Arc<AtomicUsize>) -> Server {
    Server::new(schema(bumps))
}

fn parts(
    method: Method,
    uri: &str,
    headers: &[(header::HeaderName, &str)],
) -> ::http::request::Parts {
    let mut builder = ::http::Request::builder().method(method).uri(uri);
    for (name, value) in headers {
        builder = builder.header(name, *value);
    }
    builder.body(()).expect("valid request").into_parts().0
}

fn get_uri(query: &str) -> String {
    format!(
        "/graphql?{}",
        serde_urlencoded::to_string([("query", query)]).expect("query string")
    )
}

fn buffered(result: HttpResult) -> suku::HttpResponse {
    match result {
        HttpResult::Response(response) => response,
        HttpResult::Stream(_) => panic!("expected a buffered response"),
    }
}

/// An extractor that counts how often it ran and always authenticates.
fn counting_extractor(
    calls: Arc<AtomicUsize>,
) -> impl Fn(ContextRequest) -> futures::future::BoxFuture<'static, Result<Context, HttpError>> {
    move |_request| {
        calls.fetch_add(1, Ordering::SeqCst);
        Box::pin(async { Ok(Context::new("authenticated")) })
    }
}

// -------------------------------------------------------------------------------------------
// Findings
// -------------------------------------------------------------------------------------------

/// GET requests may only execute queries. Allowing mutations here makes every operation
/// reachable from an `<img>`/`<a>`/`window.open` cross-site request when the endpoint
/// authenticates with cookies, because the browser sends cookies on a GET.
#[tokio::test]
async fn get_requests_cannot_execute_mutations() {
    let bumps = Arc::new(AtomicUsize::new(0));
    let server = server(Arc::clone(&bumps));
    let config = HttpConfig::default();
    let request_parts = parts(
        Method::GET,
        &get_uri("mutation { bump }"),
        &[(header::ACCEPT, "application/json")],
    );

    let response = buffered(
        handle(
            &server,
            &config,
            &request_parts,
            b"",
            counting_extractor(Arc::new(AtomicUsize::new(0))),
        )
        .await,
    );

    assert_eq!(response.status, StatusCode::METHOD_NOT_ALLOWED);
    assert_eq!(
        bumps.load(Ordering::SeqCst),
        0,
        "a GET request must never run a mutation"
    );
}

/// A GET request with `Accept: text/event-stream` must not open a subscription stream.
#[tokio::test]
async fn get_requests_cannot_start_subscriptions() {
    let server = server(Arc::new(AtomicUsize::new(0)));
    let config = HttpConfig::default();
    let request_parts = parts(
        Method::GET,
        &get_uri("subscription { countdown }"),
        &[(header::ACCEPT, "text/event-stream")],
    );

    let result = handle(
        &server,
        &config,
        &request_parts,
        b"",
        counting_extractor(Arc::new(AtomicUsize::new(0))),
    )
    .await;

    let response = buffered(result);
    assert_eq!(response.status, StatusCode::METHOD_NOT_ALLOWED);
}

/// GraphiQL is served only after the context extractor accepts the request, so a protected
/// endpoint does not expose its API surface to anonymous callers.
#[tokio::test]
async fn graphiql_requires_authentication() {
    let calls = Arc::new(AtomicUsize::new(0));
    let extractor_calls = Arc::clone(&calls);
    let server = server(Arc::new(AtomicUsize::new(0)));
    let config = HttpConfig::default();
    let request_parts = parts(Method::GET, "/graphql", &[(header::ACCEPT, "text/html")]);

    let response = buffered(
        handle(&server, &config, &request_parts, b"", move |_| {
            extractor_calls.fetch_add(1, Ordering::SeqCst);
            Box::pin(async {
                Err(HttpError::message(
                    StatusCode::UNAUTHORIZED,
                    "missing bearer token",
                ))
            })
        })
        .await,
    );

    assert_eq!(response.status, StatusCode::UNAUTHORIZED);
    assert!(
        !String::from_utf8_lossy(&response.body).contains("GraphiQL"),
        "GraphiQL HTML must not be served without authentication"
    );
    assert_eq!(calls.load(Ordering::SeqCst), 1, "extractor must run");
}

/// Control: a POST that the extractor rejects never reaches a resolver.
#[tokio::test]
async fn rejected_requests_do_not_execute() {
    let bumps = Arc::new(AtomicUsize::new(0));
    let server = server(Arc::clone(&bumps));
    let config = HttpConfig::default();
    let request_parts = parts(
        Method::POST,
        "/graphql",
        &[(header::CONTENT_TYPE, "application/json")],
    );
    let body = serde_json::to_vec(&GraphQLRequest::new("mutation { bump }")).expect("body");

    let response = buffered(
        handle(&server, &config, &request_parts, &body, |_| {
            Box::pin(async {
                Err(HttpError::message(
                    StatusCode::UNAUTHORIZED,
                    "missing bearer token",
                ))
            })
        })
        .await,
    );

    assert_eq!(response.status, StatusCode::UNAUTHORIZED);
    assert_eq!(bumps.load(Ordering::SeqCst), 0);
}

/// Control: a batch shares one context but each operation is executed (and therefore
/// authorized) individually against it.
#[tokio::test]
async fn batch_operations_are_authorized_by_the_shared_context() {
    let authorizer = Arc::new(Mutex::new(Vec::new()));

    struct Record(Arc<Mutex<Vec<String>>>);
    impl gql::Authorizer for Record {
        fn authorize(
            &self,
            context: &gql::AuthorizationContext<'_>,
        ) -> Result<(), gql::GraphQLError> {
            self.0
                .lock()
                .expect("log")
                .push(context.field_name.to_owned());
            Ok(())
        }
    }

    let server = Server::new(schema(Arc::new(AtomicUsize::new(0))))
        .authorizer(Record(Arc::clone(&authorizer)));
    let config = HttpConfig::default();
    let request_parts = parts(
        Method::POST,
        "/graphql",
        &[(header::CONTENT_TYPE, "application/json")],
    );
    let body = serde_json::to_vec(&vec![
        GraphQLRequest::new("{ hello }"),
        GraphQLRequest::new("{ bumps }"),
    ])
    .expect("body");

    let response = buffered(
        handle(
            &server,
            &config,
            &request_parts,
            &body,
            counting_extractor(Arc::new(AtomicUsize::new(0))),
        )
        .await,
    );
    assert_eq!(response.status, StatusCode::OK);
    let mut seen = authorizer.lock().expect("log").clone();
    seen.sort();
    assert_eq!(seen, vec!["bumps".to_owned(), "hello".to_owned()]);
}
