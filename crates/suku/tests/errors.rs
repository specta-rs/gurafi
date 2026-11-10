//! Error formatter tests: masking, context, batches, prepared queries and subscriptions.

mod support;

use std::{
    sync::{Arc, Mutex},
    time::Duration,
};

use ::http::{Method, StatusCode, header};
use futures::StreamExt as _;
use gql::{Field, GraphQLError, Location, ObjectType, PathSegment, Schema, TypeRef, Value};
use suku::{
    Context, ErrorContext, ErrorFormatter, ErrorKind, GraphQLRequest, HttpConfig, HttpError,
    HttpResult, PreparedOperation, PreparedQueries, Server, http::handle, mask_internal,
};

fn path_of(field: &str) -> Vec<PathSegment> {
    vec![PathSegment::field(field)]
}

async fn execute(server: &Server, request: &GraphQLRequest) -> gql::Response {
    server.execute(request, Context::empty()).await
}

fn buffered(result: HttpResult) -> suku::HttpResponse {
    match result {
        HttpResult::Response(response) => response,
        HttpResult::Stream(_) => panic!("expected a buffered response"),
    }
}

fn json_body(response: &suku::HttpResponse) -> serde_json::Value {
    serde_json::from_slice(&response.body).expect("JSON body")
}

#[tokio::test]
async fn no_formatter_leaves_errors_unchanged() {
    let server = support::server();
    assert!(!server.has_error_formatter());

    let execution = execute(&server, &GraphQLRequest::new("{ boom }")).await;
    assert_eq!(execution.errors[0].message(), "boom");

    let request = execute(&server, &GraphQLRequest::new("{ nope }")).await;
    assert!(
        request.errors[0].message().contains("nope"),
        "{:?}",
        request.errors[0]
    );
}

#[tokio::test]
async fn formatter_sees_execution_context_and_preserves_path() {
    let server = support::server().format_error(|error, context| {
        assert_eq!(context.kind(), ErrorKind::Execution);
        assert!(context.is_execution());
        assert_eq!(context.operation_name(), Some("Boom"));
        assert_eq!(context.request_id(), None);
        assert_eq!(context.path(), error.path());
        error.clone().with_message("redacted")
    });

    let request = GraphQLRequest::new("query Boom { boom }").operation_name("Boom");
    let response = execute(&server, &request).await;

    assert_eq!(response.errors[0].message(), "redacted");
    assert_eq!(response.errors[0].path(), Some(path_of("boom").as_slice()));
}

#[tokio::test]
async fn mask_internal_masks_uncoded_execution_errors() {
    let server = support::server().format_error(mask_internal);
    let response = execute(&server, &GraphQLRequest::new("{ boom }")).await;

    assert_eq!(response.errors[0].message(), "Internal server error");
    assert_eq!(response.errors[0].path(), Some(path_of("boom").as_slice()));
}

#[tokio::test]
async fn mask_internal_preserves_locations_and_path() {
    let server = support::server().format_error(mask_internal);
    let response = execute(&server, &GraphQLRequest::new("{ located }")).await;

    assert_eq!(response.errors[0].message(), "Internal server error");
    assert_eq!(response.errors[0].locations(), &[Location::new(3, 5)]);
    assert_eq!(
        response.errors[0].path(),
        Some(path_of("located").as_slice())
    );
}

#[tokio::test]
async fn mask_internal_keeps_coded_execution_errors() {
    let server = support::server().format_error(mask_internal);
    let response = execute(&server, &GraphQLRequest::new("{ coded }")).await;

    assert_eq!(response.errors[0].message(), "secret database detail");
    assert_eq!(
        response.errors[0]
            .extensions()
            .and_then(|extensions| extensions.get("code")),
        Some(&Value::from("INTERNAL"))
    );
    assert_eq!(response.errors[0].path(), Some(path_of("coded").as_slice()));
}

#[tokio::test]
async fn mask_internal_keeps_request_errors() {
    let kinds = Arc::new(Mutex::new(Vec::new()));
    let seen = Arc::clone(&kinds);
    let server = support::server().format_error(move |error, context| {
        seen.lock().expect("lock").push(context.kind());
        mask_internal(error, context)
    });

    let response = execute(&server, &GraphQLRequest::new("{ nope }")).await;

    assert!(response.errors[0].message().contains("nope"));
    assert_eq!(*kinds.lock().expect("lock"), vec![ErrorKind::Request]);
}

#[tokio::test]
async fn custom_formatter_can_redact_request_errors() {
    let server = support::server().format_error(|error, context| {
        assert!(context.is_request());
        assert_eq!(context.path(), None);
        error.clone().with_message("request redacted")
    });

    let response = execute(&server, &GraphQLRequest::new("{ nope }")).await;
    assert_eq!(response.errors[0].message(), "request redacted");
}

#[tokio::test]
async fn batches_format_every_response_in_request_order() {
    let server = support::server().format_error(|error, _| {
        error
            .clone()
            .with_message(format!("masked:{}", error.message()))
    });

    let requests = [
        GraphQLRequest::new("{ hello }"),
        GraphQLRequest::new("{ boom }"),
        GraphQLRequest::new("{ nope }"),
        GraphQLRequest::new("{ coded }"),
    ];
    let responses = server.execute_batch(&requests, Context::empty()).await;

    assert_eq!(responses.len(), 4);
    assert_eq!(
        responses[0]
            .data
            .as_ref()
            .and_then(Value::as_object)
            .and_then(|data| data.get("hello"))
            .and_then(Value::as_str),
        Some("world")
    );
    assert_eq!(responses[1].errors[0].message(), "masked:boom");
    assert!(responses[2].errors[0].message().starts_with("masked:"));
    assert_eq!(
        responses[3].errors[0].message(),
        "masked:secret database detail"
    );
    // The formatter replaced the code-bearing error's message but the code itself survives.
    assert_eq!(
        responses[3].errors[0]
            .extensions()
            .and_then(|extensions| extensions.get("code")),
        Some(&Value::from("INTERNAL"))
    );
}

#[tokio::test]
async fn prepared_execution_exposes_the_prepared_id() {
    let schema = support::schema();
    let prepared =
        PreparedQueries::from_operations(&schema, [PreparedOperation::new("boom", "{ boom }")])
            .expect("prepared queries");
    let server = Server::new(schema)
        .prepared_queries(prepared)
        .format_error(|error, context| {
            assert_eq!(context.request_id(), Some("boom"));
            assert_eq!(context.operation_name(), None);
            assert_eq!(context.kind(), ErrorKind::Execution);
            error.clone().with_message("prepared redacted")
        });

    let response = execute(&server, &GraphQLRequest::persisted("boom")).await;
    assert_eq!(response.errors[0].message(), "prepared redacted");
    assert_eq!(response.errors[0].path(), Some(path_of("boom").as_slice()));
}

#[tokio::test]
async fn prepared_request_errors_expose_the_prepared_id() {
    let schema = support::schema();
    let prepared =
        PreparedQueries::from_operations(&schema, [PreparedOperation::new("hello", "{ hello }")])
            .expect("prepared queries");
    let server = Server::new(schema)
        .prepared_queries(prepared)
        .format_error(|error, context| {
            assert_eq!(context.request_id(), Some("missing"));
            assert!(context.is_request());
            error.clone().with_message("unknown prepared query")
        });

    let response = execute(&server, &GraphQLRequest::persisted("missing")).await;
    assert_eq!(response.errors[0].message(), "unknown prepared query");
}

#[tokio::test]
async fn subscription_setup_errors_are_formatted_as_request_errors() {
    let server = support::server().format_error(|error, context| {
        assert_eq!(context.kind(), ErrorKind::Request);
        error.clone().with_message("setup redacted")
    });

    let error = server
        .subscribe(
            &GraphQLRequest::new("subscription { nope }"),
            Context::empty(),
        )
        .await
        .expect_err("validation error");
    assert_eq!(error.errors[0].message(), "setup redacted");
}

#[tokio::test]
async fn subscription_events_are_formatted_without_reordering() {
    let server = support::server().format_error(|error, context| {
        assert_eq!(context.kind(), ErrorKind::Execution);
        assert_eq!(context.operation_name(), Some("Failing"));
        assert_eq!(context.path(), error.path());
        error.clone().with_message("event redacted")
    });

    let request = GraphQLRequest::new("subscription Failing { failing }").operation_name("Failing");
    let mut subscription = server
        .subscribe(&request, Context::empty())
        .await
        .expect("subscription");

    let first = subscription.stream.next().await.expect("first event");
    assert!(!first.has_errors(), "{:?}", first.errors);
    assert_eq!(
        first
            .data
            .as_ref()
            .and_then(Value::as_object)
            .and_then(|data| data.get("failing")),
        Some(&Value::from(1))
    );

    let second = subscription.stream.next().await.expect("second event");
    assert_eq!(second.errors[0].message(), "event redacted");
    assert_eq!(second.errors[0].path(), Some(path_of("failing").as_slice()));
}

#[tokio::test]
async fn trait_object_formatter_is_supported() {
    struct Redact;

    impl ErrorFormatter for Redact {
        fn format(&self, error: &GraphQLError, _: &ErrorContext<'_>) -> GraphQLError {
            error.clone().with_message("trait object")
        }
    }

    let server = support::server().format_error_with(Arc::new(Redact));
    assert!(server.has_error_formatter());
    let response = execute(&server, &GraphQLRequest::new("{ boom }")).await;
    assert_eq!(response.errors[0].message(), "trait object");
}

#[tokio::test]
async fn http_request_errors_are_formatted() {
    let server = support::server().format_error(|error, context| {
        assert!(context.is_request());
        assert_eq!(context.request_id(), None);
        error.clone().with_message("transport redacted")
    });

    let parts = support::parts(
        Method::POST,
        "/graphql",
        &[
            (header::CONTENT_TYPE, "application/json"),
            (header::ACCEPT, "application/json"),
        ],
    );
    let response = buffered(
        handle(
            &server,
            &HttpConfig::default(),
            &parts,
            b"{not json",
            support::empty_context,
        )
        .await,
    );

    assert_eq!(response.status, StatusCode::BAD_REQUEST);
    assert_eq!(
        json_body(&response)["errors"][0]["message"],
        "transport redacted"
    );
}

#[tokio::test]
async fn extractor_denials_are_formatted() {
    let server = support::server()
        .format_error(|error, _| error.clone().with_message("denied and redacted"));

    let parts = support::parts(
        Method::POST,
        "/graphql",
        &[
            (header::CONTENT_TYPE, "application/json"),
            (header::ACCEPT, "application/json"),
        ],
    );
    let response = buffered(
        handle(&server, &HttpConfig::default(), &parts, b"{}", |_| async {
            Err(HttpError::unauthorized(suku::error_response(
                "denied detail",
            )))
        })
        .await,
    );

    assert_eq!(response.status, StatusCode::UNAUTHORIZED);
    assert_eq!(
        json_body(&response)["errors"][0]["message"],
        "denied and redacted"
    );
}

#[tokio::test]
async fn transport_rejections_are_formatted() {
    let server = support::server().format_error(|error, context| {
        assert!(context.is_request());
        error
            .clone()
            .with_message(format!("{} (formatted)", error.message()))
    });

    let put = support::parts(
        Method::PUT,
        "/graphql",
        &[(header::ACCEPT, "application/json")],
    );
    let response = buffered(
        handle(
            &server,
            &HttpConfig::default(),
            &put,
            b"{}",
            support::empty_context,
        )
        .await,
    );
    assert_eq!(response.status, StatusCode::METHOD_NOT_ALLOWED);
    assert_eq!(
        json_body(&response)["errors"][0]["message"],
        "Only GET and POST are supported. (formatted)"
    );

    let post = support::parts(
        Method::POST,
        "/graphql",
        &[
            (header::CONTENT_TYPE, "application/json"),
            (header::ACCEPT, "text/html"),
        ],
    );
    let response = buffered(
        handle(
            &server,
            &HttpConfig::default(),
            &post,
            b"{\"query\":\"{ hello }\"}",
            support::empty_context,
        )
        .await,
    );
    assert_eq!(response.status, StatusCode::NOT_ACCEPTABLE);
    assert_eq!(
        json_body(&response)["errors"][0]["message"],
        "No acceptable response type; use `application/json` or `application/graphql-response+json`. (formatted)"
    );
}

#[cfg(feature = "axum")]
#[tokio::test]
async fn timeouts_are_formatted_as_execution_errors() {
    let slow = Field::new("slow", TypeRef::string().non_null()).resolve(|_| {
        Box::pin(async {
            tokio::time::sleep(Duration::from_millis(200)).await;
            Ok(Value::from("late"))
        })
    });
    let query = ObjectType::new("Query").field(slow);
    let mut builder = Schema::builder();
    builder.add_type(query.into());
    let schema = builder.query("Query").build().expect("valid schema");

    let server = Server::new(schema)
        .timeout(Duration::from_millis(10))
        .format_error(|error, context| {
            assert_eq!(context.kind(), ErrorKind::Execution);
            error.clone().with_message("timed out and redacted")
        });

    let response = execute(&server, &GraphQLRequest::new("{ slow }")).await;
    assert_eq!(response.errors[0].message(), "timed out and redacted");
}

#[tokio::test]
#[should_panic(expected = "formatter panic")]
async fn formatter_panics_are_not_caught() {
    let server = support::server().format_error(|_, _| panic!("formatter panic"));
    let _ = execute(&server, &GraphQLRequest::new("{ boom }")).await;
}
