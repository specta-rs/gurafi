//! Axum integration tests, using `tower::ServiceExt::oneshot` instead of a socket.

#![cfg(feature = "axum")]

mod support;

use ::axum::{
    Router,
    body::{Body, to_bytes},
    http::{Method, Request, StatusCode, header},
    response::Response,
};
use suku::{Context, GraphQL, HttpError, http::ContextRequest};
use tower::ServiceExt as _;

fn request(
    method: Method,
    uri: &str,
    headers: &[(&str, &str)],
    body: impl Into<Body>,
) -> Request<Body> {
    let mut builder = Request::builder().method(method).uri(uri);
    for (name, value) in headers {
        builder = builder.header(*name, *value);
    }
    builder.body(body.into()).expect("valid request")
}

async fn send(app: &Router, request: Request<Body>) -> (StatusCode, ::http::HeaderMap, Vec<u8>) {
    let response: Response = app.clone().oneshot(request).await.expect("response");
    let status = response.status();
    let headers = response.headers().clone();
    let body = to_bytes(response.into_body(), usize::MAX)
        .await
        .expect("body")
        .to_vec();
    (status, headers, body)
}

fn content_type(headers: &::http::HeaderMap) -> &str {
    headers
        .get(header::CONTENT_TYPE)
        .and_then(|value| value.to_str().ok())
        .unwrap_or_default()
}

fn json_body(body: &[u8]) -> serde_json::Value {
    serde_json::from_slice(body).expect("JSON body")
}

fn app() -> Router {
    GraphQL::new(support::server()).into_router()
}

#[tokio::test]
async fn graphiql_is_served_at_the_default_path() {
    let (status, headers, body) = send(
        &app(),
        request(
            Method::GET,
            "/graphql",
            &[(header::ACCEPT.as_str(), "text/html")],
            Body::empty(),
        ),
    )
    .await;

    assert_eq!(status, StatusCode::OK);
    assert!(content_type(&headers).starts_with("text/html"));
    assert!(String::from_utf8_lossy(&body).contains("GraphiQL"));
}

#[tokio::test]
async fn executes_a_post_request() {
    let (status, headers, body) = send(
        &app(),
        request(
            Method::POST,
            "/graphql",
            &[
                (header::CONTENT_TYPE.as_str(), "application/json"),
                (header::ACCEPT.as_str(), "application/json"),
            ],
            r#"{"query":"{ hello }"}"#,
        ),
    )
    .await;

    assert_eq!(status, StatusCode::OK);
    assert!(content_type(&headers).starts_with("application/json"));
    assert_eq!(json_body(&body)["data"]["hello"], "world");
}

#[tokio::test]
async fn denied_requests_are_401() {
    let app = GraphQL::new(support::server())
        .with_context(|request: ContextRequest| async move {
            match request
                .header("authorization")
                .and_then(|value| value.to_str().ok())
            {
                Some("Bearer secret") => Ok(Context::new(support::User {
                    name: "Ada".to_owned(),
                })),
                _ => Err(HttpError::message(StatusCode::UNAUTHORIZED, "Unauthorized")),
            }
        })
        .into_router();

    let (status, _, body) = send(
        &app,
        request(
            Method::POST,
            "/graphql",
            &[(header::CONTENT_TYPE.as_str(), "application/json")],
            r#"{"query":"{ whoami }"}"#,
        ),
    )
    .await;

    assert_eq!(status, StatusCode::UNAUTHORIZED);
    assert_eq!(json_body(&body)["errors"][0]["message"], "Unauthorized");
}

#[tokio::test]
async fn authenticated_requests_reach_the_context() {
    let app = GraphQL::new(support::server())
        .with_context(|request: ContextRequest| async move {
            match request
                .header("authorization")
                .and_then(|value| value.to_str().ok())
            {
                Some("Bearer secret") => Ok(Context::new(support::User {
                    name: "Ada".to_owned(),
                })),
                _ => Err(HttpError::message(StatusCode::UNAUTHORIZED, "Unauthorized")),
            }
        })
        .into_router();

    let (status, _, body) = send(
        &app,
        request(
            Method::POST,
            "/graphql",
            &[
                (header::CONTENT_TYPE.as_str(), "application/json"),
                (header::AUTHORIZATION.as_str(), "Bearer secret"),
            ],
            r#"{"query":"{ whoami }"}"#,
        ),
    )
    .await;

    assert_eq!(status, StatusCode::OK);
    assert_eq!(json_body(&body)["data"]["whoami"], "Ada");
}

#[tokio::test]
async fn custom_path_is_respected() {
    let app = GraphQL::new(support::server())
        .path("/api/graphql")
        .into_router();

    let (status, _, body) = send(
        &app,
        request(
            Method::POST,
            "/api/graphql",
            &[(header::CONTENT_TYPE.as_str(), "application/json")],
            r#"{"query":"{ hello }"}"#,
        ),
    )
    .await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(json_body(&body)["data"]["hello"], "world");

    let (status, _, _) = send(
        &app,
        request(
            Method::POST,
            "/graphql",
            &[(header::CONTENT_TYPE.as_str(), "application/json")],
            r#"{"query":"{ hello }"}"#,
        ),
    )
    .await;
    assert_eq!(status, StatusCode::NOT_FOUND);
}

#[tokio::test]
async fn body_limit_is_enforced_before_parsing() {
    let app = GraphQL::new(support::server())
        .max_body_bytes(8)
        .into_router();

    let (status, _, _) = send(
        &app,
        request(
            Method::POST,
            "/graphql",
            &[(header::CONTENT_TYPE.as_str(), "application/json")],
            r#"{"query":"{ hello }"}"#,
        ),
    )
    .await;

    assert_eq!(status, StatusCode::PAYLOAD_TOO_LARGE);
}

#[tokio::test]
async fn subscription_requests_stream_sse() {
    let (status, headers, body) = send(
        &app(),
        request(
            Method::POST,
            "/graphql",
            &[
                (header::CONTENT_TYPE.as_str(), "application/json"),
                (header::ACCEPT.as_str(), "text/event-stream"),
            ],
            r#"{"query":"subscription { countdown }"}"#,
        ),
    )
    .await;

    assert_eq!(status, StatusCode::OK);
    assert!(content_type(&headers).starts_with("text/event-stream"));
    let body = String::from_utf8_lossy(&body);
    assert!(body.contains("event: next"));
    assert!(body.contains("event: complete"));
}

#[tokio::test]
async fn unsupported_method_is_405_via_the_router() {
    let (status, headers, _) = send(
        &app(),
        request(Method::DELETE, "/graphql", &[], Body::empty()),
    )
    .await;

    assert_eq!(status, StatusCode::METHOD_NOT_ALLOWED);
    assert_eq!(
        headers
            .get(header::ALLOW)
            .and_then(|value| value.to_str().ok()),
        Some("GET, POST")
    );
}
