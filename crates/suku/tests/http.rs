//! GraphQL-over-HTTP transport tests.
//!
//! These call `suku::http::handle` directly; no socket is involved.

mod support;

use ::http::{Method, StatusCode, header};
use futures::StreamExt as _;
use serde_json::json;
use suku::{
    Context, HttpConfig, HttpError, HttpResponse, HttpResult, Server,
    http::{ContextRequest, handle},
};

fn buffered(result: HttpResult) -> HttpResponse {
    match result {
        HttpResult::Response(response) => response,
        HttpResult::Stream(_) => panic!("expected a buffered response"),
    }
}

async fn run_default(
    server: &Server,
    config: &HttpConfig,
    parts: &::http::request::Parts,
    body: &[u8],
) -> HttpResult {
    handle(server, config, parts, body, support::empty_context).await
}

fn content_type(response: &HttpResponse) -> &str {
    response
        .headers
        .get(header::CONTENT_TYPE)
        .and_then(|value| value.to_str().ok())
        .unwrap_or_default()
}

#[tokio::test]
async fn get_with_html_accept_returns_graphiql() {
    let server = support::server();
    let config = HttpConfig::default();
    let parts = support::parts(
        Method::GET,
        "/graphql",
        &[(header::ACCEPT, "text/html,application/xhtml+xml")],
    );

    let response = buffered(run_default(&server, &config, &parts, b"").await);
    assert_eq!(response.status, StatusCode::OK);
    assert!(content_type(&response).starts_with("text/html"));
    assert!(String::from_utf8_lossy(&response.body).contains("GraphiQL"));
}

#[tokio::test]
async fn graphiql_can_be_disabled() {
    let server = support::server();
    let config = HttpConfig::default().graphiql(false);
    let parts = support::parts(Method::GET, "/graphql", &[(header::ACCEPT, "text/html")]);

    let response = buffered(run_default(&server, &config, &parts, b"").await);
    assert_eq!(response.status, StatusCode::BAD_REQUEST);
}

#[tokio::test]
async fn get_executes_a_query_from_query_parameters() {
    let server = support::server();
    let config = HttpConfig::default();
    let query = serde_urlencoded::to_string([("query", "{ hello }")]).expect("query string");
    let uri = format!("/graphql?{query}");
    let parts = support::parts(Method::GET, &uri, &[(header::ACCEPT, "application/json")]);

    let response = buffered(run_default(&server, &config, &parts, b"").await);
    assert_eq!(response.status, StatusCode::OK);
    assert!(content_type(&response).starts_with("application/json"));
    let body = support::json_body(&response.body);
    assert_eq!(body["data"]["hello"], "world");
}

#[tokio::test]
async fn get_parses_variables() {
    let server = support::server();
    let config = HttpConfig::default();
    let query = serde_urlencoded::to_string([
        (
            "query",
            "query Echo($message: String!) { echo(message: $message) }",
        ),
        ("variables", r#"{"message":"from get"}"#),
    ])
    .expect("query string");
    let parts = support::parts(
        Method::GET,
        &format!("/graphql?{query}"),
        &[(header::ACCEPT, "application/json")],
    );

    let response = buffered(run_default(&server, &config, &parts, b"").await);
    assert_eq!(response.status, StatusCode::OK);
    assert_eq!(
        support::json_body(&response.body)["data"]["echo"],
        "from get"
    );
}

#[tokio::test]
async fn get_rejects_invalid_variables_json() {
    let server = support::server();
    let config = HttpConfig::default();
    let query = serde_urlencoded::to_string([
        (
            "query",
            "query($message: String!) { echo(message: $message) }",
        ),
        ("variables", "{not json"),
    ])
    .expect("query string");
    let parts = support::parts(
        Method::GET,
        &format!("/graphql?{query}"),
        &[(header::ACCEPT, "application/json")],
    );

    let response = buffered(run_default(&server, &config, &parts, b"").await);
    assert_eq!(response.status, StatusCode::BAD_REQUEST);
    assert!(support::json_body(&response.body)["errors"].is_array());
}

#[tokio::test]
async fn post_executes_a_json_request() {
    let server = support::server();
    let config = HttpConfig::default();
    let body = br#"{"query":"{ hello }"}"#;
    let parts = support::parts(
        Method::POST,
        "/graphql",
        &[
            (header::CONTENT_TYPE, "application/json"),
            (header::ACCEPT, "application/json"),
        ],
    );

    let response = buffered(run_default(&server, &config, &parts, body).await);
    assert_eq!(response.status, StatusCode::OK);
    assert!(content_type(&response).starts_with("application/json"));
    assert_eq!(support::json_body(&response.body)["data"]["hello"], "world");
}

#[tokio::test]
async fn post_executes_a_batch_in_order() {
    let server = support::server();
    let config = HttpConfig::default();
    let body = br#"[{"query":"{ hello }"},{"query":"{ echo(message: \"two\") }"}]"#;
    let parts = support::parts(
        Method::POST,
        "/graphql",
        &[(header::CONTENT_TYPE, "application/json")],
    );

    let response = buffered(run_default(&server, &config, &parts, body).await);
    assert_eq!(response.status, StatusCode::OK);
    let body = support::json_body(&response.body);
    let batch = body.as_array().expect("batch array");
    assert_eq!(batch.len(), 2);
    assert_eq!(batch[0]["data"]["hello"], "world");
    assert_eq!(batch[1]["data"]["echo"], "two");
}

#[tokio::test]
async fn rejects_empty_batches() {
    let server = support::server();
    let config = HttpConfig::default();
    let parts = support::parts(
        Method::POST,
        "/graphql",
        &[(header::CONTENT_TYPE, "application/json")],
    );

    let response = buffered(run_default(&server, &config, &parts, b"[]").await);
    assert_eq!(response.status, StatusCode::BAD_REQUEST);
    assert!(support::json_body(&response.body)["errors"].is_array());
}

#[tokio::test]
async fn rejects_batches_over_the_limit() {
    let server = support::server().max_batch_size(1);
    let config = HttpConfig::default();
    let body = br#"[{"query":"{ hello }"},{"query":"{ hello }"}]"#;
    let parts = support::parts(
        Method::POST,
        "/graphql",
        &[(header::CONTENT_TYPE, "application/json")],
    );

    let response = buffered(run_default(&server, &config, &parts, body).await);
    assert_eq!(response.status, StatusCode::BAD_REQUEST);
    assert!(support::json_body(&response.body)["errors"].is_array());
}

#[tokio::test]
async fn malformed_json_is_a_400_with_a_graphql_shaped_body() {
    let server = support::server();
    let config = HttpConfig::default();
    let parts = support::parts(
        Method::POST,
        "/graphql",
        &[
            (header::CONTENT_TYPE, "application/json"),
            (header::ACCEPT, "application/json"),
        ],
    );

    let response = buffered(run_default(&server, &config, &parts, b"{").await);
    assert_eq!(response.status, StatusCode::BAD_REQUEST);
    assert!(support::json_body(&response.body)["errors"][0]["message"].is_string());
}

#[tokio::test]
async fn graphql_response_json_uses_200_for_success() {
    let server = support::server();
    let config = HttpConfig::default();
    let parts = support::parts(
        Method::POST,
        "/graphql",
        &[
            (header::CONTENT_TYPE, "application/json"),
            (header::ACCEPT, "application/graphql-response+json"),
        ],
    );

    let response =
        buffered(run_default(&server, &config, &parts, br#"{"query":"{ hello }"}"#).await);
    assert_eq!(response.status, StatusCode::OK);
    assert!(content_type(&response).starts_with("application/graphql-response+json"));
    assert_eq!(support::json_body(&response.body)["data"]["hello"], "world");
}

#[tokio::test]
async fn graphql_response_json_uses_200_for_execution_errors() {
    let server = support::server();
    let config = HttpConfig::default();
    let parts = support::parts(
        Method::POST,
        "/graphql",
        &[
            (header::CONTENT_TYPE, "application/json"),
            (header::ACCEPT, "application/graphql-response+json"),
        ],
    );

    let response =
        buffered(run_default(&server, &config, &parts, br#"{"query":"{ boom }"}"#).await);
    assert_eq!(response.status, StatusCode::OK);
    assert!(support::json_body(&response.body)["errors"].is_array());
}

#[tokio::test]
async fn the_server_body_limit_is_used_when_the_mount_does_not_override_it() {
    let server = support::server().max_body_bytes(8);
    let config = HttpConfig::default();
    let parts = support::parts(
        Method::POST,
        "/graphql",
        &[(header::CONTENT_TYPE, "application/json")],
    );

    let response =
        buffered(run_default(&server, &config, &parts, br#"{"query":"{ hello }"}"#).await);
    assert_eq!(response.status, StatusCode::PAYLOAD_TOO_LARGE);
}

#[tokio::test]
async fn graphql_response_json_uses_400_for_request_errors() {
    let server = support::server();
    let config = HttpConfig::default();
    let body = br#"{"query":"{"}"#;
    let parts = support::parts(
        Method::POST,
        "/graphql",
        &[
            (header::CONTENT_TYPE, "application/json"),
            (header::ACCEPT, "application/graphql-response+json"),
        ],
    );

    let response = buffered(run_default(&server, &config, &parts, body).await);
    assert_eq!(response.status, StatusCode::BAD_REQUEST);
    assert!(content_type(&response).starts_with("application/graphql-response+json"));
}

#[tokio::test]
async fn legacy_json_uses_200_for_request_errors() {
    let server = support::server();
    let config = HttpConfig::default();
    let body = br#"{"query":"{"}"#;
    let parts = support::parts(
        Method::POST,
        "/graphql",
        &[
            (header::CONTENT_TYPE, "application/json"),
            (header::ACCEPT, "application/json"),
        ],
    );

    let response = buffered(run_default(&server, &config, &parts, body).await);
    assert_eq!(response.status, StatusCode::OK);
    assert!(support::json_body(&response.body)["errors"].is_array());
}

#[tokio::test]
async fn unsupported_method_is_405() {
    let server = support::server();
    let config = HttpConfig::default();
    let parts = support::parts(Method::PUT, "/graphql", &[]);

    let response = buffered(run_default(&server, &config, &parts, b"").await);
    assert_eq!(response.status, StatusCode::METHOD_NOT_ALLOWED);
    assert_eq!(
        response
            .headers
            .get(header::ALLOW)
            .and_then(|value| value.to_str().ok()),
        Some("GET, POST")
    );
}

#[tokio::test]
async fn unsupported_content_type_is_415() {
    let server = support::server();
    let config = HttpConfig::default();
    let parts = support::parts(
        Method::POST,
        "/graphql",
        &[(header::CONTENT_TYPE, "text/plain")],
    );

    let response = buffered(run_default(&server, &config, &parts, b"{ hello }").await);
    assert_eq!(response.status, StatusCode::UNSUPPORTED_MEDIA_TYPE);
}

#[tokio::test]
async fn missing_content_type_is_415() {
    let server = support::server();
    let config = HttpConfig::default();
    let parts = support::parts(Method::POST, "/graphql", &[]);

    let response = buffered(run_default(&server, &config, &parts, b"{}").await);
    assert_eq!(response.status, StatusCode::UNSUPPORTED_MEDIA_TYPE);
}

#[tokio::test]
async fn unacceptable_accept_is_406() {
    let server = support::server();
    let config = HttpConfig::default();
    let parts = support::parts(
        Method::POST,
        "/graphql",
        &[
            (header::CONTENT_TYPE, "application/json"),
            (header::ACCEPT, "text/plain"),
        ],
    );

    let response =
        buffered(run_default(&server, &config, &parts, br#"{"query":"{ hello }"}"#).await);
    assert_eq!(response.status, StatusCode::NOT_ACCEPTABLE);
}

#[tokio::test]
async fn body_limit_is_413() {
    let server = support::server();
    let config = HttpConfig::default().max_body_bytes(8);
    let parts = support::parts(
        Method::POST,
        "/graphql",
        &[(header::CONTENT_TYPE, "application/json")],
    );

    let response =
        buffered(run_default(&server, &config, &parts, br#"{"query":"{ hello }"}"#).await);
    assert_eq!(response.status, StatusCode::PAYLOAD_TOO_LARGE);
}

#[tokio::test]
async fn denied_context_is_a_401_before_execution() {
    let server = support::server();
    let config = HttpConfig::default();
    let parts = support::parts(
        Method::POST,
        "/graphql",
        &[(header::CONTENT_TYPE, "application/json")],
    );

    let result = handle(
        &server,
        &config,
        &parts,
        br#"{"query":"{ whoami }"}"#,
        |_request: ContextRequest| async move {
            Err::<Context, HttpError>(HttpError::message(
                StatusCode::UNAUTHORIZED,
                "Missing bearer token",
            ))
        },
    )
    .await;

    let response = buffered(result);
    assert_eq!(response.status, StatusCode::UNAUTHORIZED);
    assert_eq!(
        support::json_body(&response.body)["errors"][0]["message"],
        "Missing bearer token"
    );
}

#[tokio::test]
async fn the_extractor_can_authenticate_and_build_a_context() {
    let server = support::server();
    let config = HttpConfig::default();
    let parts = support::parts(
        Method::POST,
        "/graphql",
        &[
            (header::CONTENT_TYPE, "application/json"),
            (header::AUTHORIZATION, "Bearer secret"),
        ],
    );

    let result = handle(
        &server,
        &config,
        &parts,
        br#"{"query":"{ whoami }"}"#,
        |request: ContextRequest| async move {
            match request
                .header("authorization")
                .and_then(|value| value.to_str().ok())
            {
                Some("Bearer secret") => Ok(Context::new(support::User {
                    name: "Ada".to_owned(),
                })),
                _ => Err(HttpError::message(StatusCode::UNAUTHORIZED, "Unauthorized")),
            }
        },
    )
    .await;

    let response = buffered(result);
    assert_eq!(response.status, StatusCode::OK);
    assert_eq!(support::json_body(&response.body)["data"]["whoami"], "Ada");
}

#[tokio::test]
async fn cookies_are_visible_to_the_extractor() {
    let server = support::server();
    let config = HttpConfig::default();
    let parts = support::parts(
        Method::POST,
        "/graphql",
        &[
            (header::CONTENT_TYPE, "application/json"),
            (header::COOKIE, "theme=dark; session=abc123"),
        ],
    );

    let result = handle(
        &server,
        &config,
        &parts,
        br#"{"query":"{ hello }"}"#,
        |request: ContextRequest| async move {
            assert_eq!(request.cookie("session"), Some("abc123"));
            Ok(Context::empty())
        },
    )
    .await;

    assert_eq!(buffered(result).status, StatusCode::OK);
}

#[tokio::test]
async fn subscriptions_stream_as_server_sent_events() {
    let server = support::server();
    let config = HttpConfig::default();
    let parts = support::parts(
        Method::POST,
        "/graphql",
        &[
            (header::CONTENT_TYPE, "application/json"),
            (header::ACCEPT, "text/event-stream"),
        ],
    );

    let result = handle(
        &server,
        &config,
        &parts,
        br#"{"query":"subscription { countdown }"}"#,
        support::empty_context,
    )
    .await;

    let HttpResult::Stream(stream) = result else {
        panic!("expected an SSE stream");
    };
    assert!(content_type_from_headers(&stream.headers).starts_with("text/event-stream"));

    let mut body = String::new();
    let mut events = stream.stream;
    while let Some(chunk) = events.next().await {
        body.push_str(&String::from_utf8_lossy(&chunk.expect("chunk")));
    }

    assert!(body.contains("event: next"));
    assert!(body.contains("event: complete"));
    assert!(body.contains("\"countdown\":3"));
}

#[tokio::test]
async fn subscriptions_without_event_stream_get_a_clear_error() {
    let server = support::server();
    let config = HttpConfig::default();
    let parts = support::parts(
        Method::POST,
        "/graphql",
        &[
            (header::CONTENT_TYPE, "application/json"),
            (header::ACCEPT, "application/json"),
        ],
    );

    let response = buffered(
        run_default(
            &server,
            &config,
            &parts,
            br#"{"query":"subscription { countdown }"}"#,
        )
        .await,
    );
    assert_eq!(response.status, StatusCode::OK);
    assert!(
        support::json_body(&response.body)["errors"][0]["message"]
            .as_str()
            .unwrap_or_default()
            .contains("text/event-stream")
    );
}

#[tokio::test]
async fn malformed_subscriptions_return_json_before_streaming() {
    let server = support::server();
    let config = HttpConfig::default();
    let parts = support::parts(
        Method::POST,
        "/graphql",
        &[
            (header::CONTENT_TYPE, "application/json"),
            (header::ACCEPT, "text/event-stream"),
        ],
    );

    let response =
        buffered(run_default(&server, &config, &parts, br#"{"query":"subscription {"}"#).await);
    assert_eq!(response.status, StatusCode::BAD_REQUEST);
    assert!(support::json_body(&response.body)["errors"][0]["message"].is_string());
}

#[tokio::test]
async fn event_stream_is_rejected_for_non_subscriptions() {
    let server = support::server();
    let config = HttpConfig::default();
    let parts = support::parts(
        Method::POST,
        "/graphql",
        &[
            (header::CONTENT_TYPE, "application/json"),
            (header::ACCEPT, "text/event-stream"),
        ],
    );

    let response =
        buffered(run_default(&server, &config, &parts, br#"{"query":"{ hello }"}"#).await);
    assert_eq!(response.status, StatusCode::NOT_ACCEPTABLE);
}

#[tokio::test]
async fn response_includes_vary_and_no_store() {
    let server = support::server();
    let config = HttpConfig::default();
    let parts = support::parts(
        Method::POST,
        "/graphql",
        &[(header::CONTENT_TYPE, "application/json")],
    );

    let response =
        buffered(run_default(&server, &config, &parts, br#"{"query":"{ hello }"}"#).await);
    assert_eq!(
        response
            .headers
            .get(header::VARY)
            .and_then(|value| value.to_str().ok()),
        Some("Accept")
    );
    assert_eq!(
        response
            .headers
            .get(header::CACHE_CONTROL)
            .and_then(|value| value.to_str().ok()),
        Some("no-store")
    );
}

#[tokio::test]
async fn request_extensions_are_accepted() {
    let server = support::server();
    let config = HttpConfig::default();
    let parts = support::parts(
        Method::POST,
        "/graphql",
        &[(header::CONTENT_TYPE, "application/json")],
    );
    let body = json!({
        "query": "{ hello }",
        "extensions": { "persistedQuery": { "version": 1, "sha256Hash": "abc" } }
    });

    let response =
        buffered(run_default(&server, &config, &parts, body.to_string().as_bytes()).await);
    assert_eq!(response.status, StatusCode::OK);
    assert_eq!(support::json_body(&response.body)["data"]["hello"], "world");
}

fn content_type_from_headers(headers: &::http::HeaderMap) -> &str {
    headers
        .get(header::CONTENT_TYPE)
        .and_then(|value| value.to_str().ok())
        .unwrap_or_default()
}
