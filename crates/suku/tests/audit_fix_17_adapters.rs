//! Adapter regressions for audit finding #17: late response serialization failures.

#![cfg(feature = "axum")]

mod support;

use std::sync::{Arc, Mutex};

use ::axum::{
    Router,
    body::{Body, to_bytes},
    http::{HeaderMap, HeaderValue, Method, Request, StatusCode, header},
    response::Response as AxumResponse,
};
use gql::{GraphQLError, Schema, Value};
use serde_json::Value as JsonValue;
use suku::{Context, ErrorKind, GraphQL, HttpError, Server, http::ContextRequest, ws};
use tower::ServiceExt as _;

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

async fn send(app: Router, request: Request<Body>) -> AxumResponse {
    app.oneshot(request).await.expect("response")
}

async fn response_body(response: AxumResponse) -> (StatusCode, HeaderMap, Vec<u8>) {
    let status = response.status();
    let headers = response.headers().clone();
    let body = to_bytes(response.into_body(), usize::MAX)
        .await
        .expect("body")
        .to_vec();
    (status, headers, body)
}

fn assert_error_body(body: &[u8], expected: &str) -> JsonValue {
    let raw = String::from_utf8(body.to_vec()).expect("UTF-8 body");
    assert!(!raw.contains("Failed to serialize"), "{raw}");
    assert!(!raw.contains("opaque model value"), "{raw}");
    assert!(!raw.contains("DUMMY_MODEL"), "{raw}");
    let json: JsonValue = serde_json::from_str(&raw).expect("JSON body");
    assert!(json.get("data").is_none(), "{json}");
    assert_eq!(json["errors"].as_array().map(Vec::len), Some(1));
    assert_eq!(json["errors"][0]["message"], expected);
    json
}

#[tokio::test]
async fn axum_direct_error_serialization_failure_forces_http_500() {
    let observations = Observations::default();
    let app = GraphQL::new(formatted_server(support::schema(), &observations, false))
        .max_body_bytes(1)
        .into_router();
    let request = Request::builder()
        .method(Method::POST)
        .uri("/graphql")
        .body(Body::from("{}"))
        .expect("request");

    let (status, headers, body) = response_body(send(app, request).await).await;
    assert_eq!(status, StatusCode::INTERNAL_SERVER_ERROR);
    assert_eq!(
        headers
            .get(header::CONTENT_TYPE)
            .and_then(|value| value.to_str().ok()),
        Some("application/graphql-response+json; charset=utf-8")
    );
    assert_error_body(&body, "redacted");
    assert_eq!(
        *observations.lock().expect("lock"),
        vec![
            Observation {
                kind: ErrorKind::Request,
                operation_name: None,
                request_id: None,
            },
            Observation {
                kind: ErrorKind::Execution,
                operation_name: None,
                request_id: None,
            },
        ]
    );
}
#[tokio::test]
async fn axum_upgrade_error_serialization_failure_forces_http_500() {
    let observations = Observations::default();
    let app = GraphQL::new(formatted_server(support::schema(), &observations, false))
        .with_context(|_: ContextRequest| async move {
            let mut denial = HttpError::new(
                StatusCode::UNAUTHORIZED,
                gql::Response::error(opaque_error("denied")),
            );
            denial
                .headers
                .insert(header::WWW_AUTHENTICATE, HeaderValue::from_static("Bearer"));
            Err::<Context, HttpError>(denial)
        })
        .into_router();
    let request = Request::builder()
        .method(Method::GET)
        .uri("/graphql")
        .header(header::CONNECTION, "Upgrade")
        .header(header::UPGRADE, "websocket")
        .header(header::SEC_WEBSOCKET_PROTOCOL, ws::GRAPHQL_TRANSPORT_WS)
        .body(Body::empty())
        .expect("request");

    let (status, headers, body) = response_body(send(app, request).await).await;
    assert_eq!(status, StatusCode::INTERNAL_SERVER_ERROR);
    assert_eq!(
        headers
            .get(header::WWW_AUTHENTICATE)
            .and_then(|value| value.to_str().ok()),
        Some("Bearer")
    );
    assert_error_body(&body, "redacted");
    assert_eq!(
        *observations.lock().expect("lock"),
        vec![
            Observation {
                kind: ErrorKind::Request,
                operation_name: None,
                request_id: None,
            },
            Observation {
                kind: ErrorKind::Execution,
                operation_name: None,
                request_id: None,
            },
        ]
    );
}
