//! GraphQL-over-HTTP transport tests.
//!
//! These call `suku::http::handle` directly; no socket is involved. The expected status codes
//! follow the GraphQL over HTTP specification (September 2026).

mod support;

use ::http::{Method, StatusCode, header};
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

fn allow(response: &HttpResponse) -> Option<&str> {
    response
        .headers
        .get(header::ALLOW)
        .and_then(|value| value.to_str().ok())
}

fn post(content_type: &'static str, accept: &'static str) -> ::http::request::Parts {
    support::parts(
        Method::POST,
        "/graphql",
        &[
            (header::CONTENT_TYPE, content_type),
            (header::ACCEPT, accept),
        ],
    )
}

fn get(query: &str, accept: &'static str) -> ::http::request::Parts {
    support::parts(
        Method::GET,
        &format!("/graphql?{query}"),
        &[(header::ACCEPT, accept)],
    )
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
    assert_eq!(response.status, StatusCode::NOT_ACCEPTABLE);
}

#[tokio::test]
async fn json_is_preferred_over_graphiql_when_a_client_lists_both() {
    let server = support::server();
    let config = HttpConfig::default();
    let query = serde_urlencoded::to_string([("query", "{ hello }")]).expect("query string");
    let parts = get(
        &query,
        "text/html, application/graphql-response+json, application/json",
    );

    let response = buffered(run_default(&server, &config, &parts, b"").await);
    assert_eq!(response.status, StatusCode::OK);
    assert!(content_type(&response).starts_with("application/graphql-response+json"));
}

#[tokio::test]
async fn get_executes_a_query_from_query_parameters() {
    let server = support::server();
    let config = HttpConfig::default();
    let query = serde_urlencoded::to_string([("query", "{ hello }")]).expect("query string");
    let parts = get(&query, "application/graphql-response+json");

    let response = buffered(run_default(&server, &config, &parts, b"").await);
    assert_eq!(response.status, StatusCode::OK);
    assert!(content_type(&response).starts_with("application/graphql-response+json"));
    assert_eq!(support::json_body(&response.body)["data"]["hello"], "world");
}

#[tokio::test]
async fn a_missing_accept_header_uses_the_specification_media_type() {
    let server = support::server();
    let config = HttpConfig::default();
    let query = serde_urlencoded::to_string([("query", "{ hello }")]).expect("query string");
    let parts = support::parts(Method::GET, &format!("/graphql?{query}"), &[]);

    let response = buffered(run_default(&server, &config, &parts, b"").await);
    assert_eq!(response.status, StatusCode::OK);
    assert!(content_type(&response).starts_with("application/graphql-response+json"));
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
    let parts = get(&query, "application/graphql-response+json");

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
    let parts = get(&query, "application/graphql-response+json");

    let response = buffered(run_default(&server, &config, &parts, b"").await);
    assert_eq!(response.status, StatusCode::BAD_REQUEST);
    assert!(support::json_body(&response.body)["errors"].is_array());
}

#[tokio::test]
async fn get_rejects_variables_that_are_not_an_object() {
    let server = support::server();
    let config = HttpConfig::default();
    let query = serde_urlencoded::to_string([("query", "{ hello }"), ("variables", "[1,2,3]")])
        .expect("query string");
    let parts = get(&query, "application/graphql-response+json");

    let response = buffered(run_default(&server, &config, &parts, b"").await);
    assert_eq!(response.status, StatusCode::UNPROCESSABLE_ENTITY);
}

#[tokio::test]
async fn get_without_a_query_is_422() {
    let server = support::server();
    let config = HttpConfig::default();
    let parts = get("", "application/graphql-response+json");

    let response = buffered(run_default(&server, &config, &parts, b"").await);
    assert_eq!(response.status, StatusCode::UNPROCESSABLE_ENTITY);
    assert!(support::json_body(&response.body)["errors"].is_array());
}

#[tokio::test]
async fn get_rejects_mutations_with_405_and_allow_post() {
    let server = support::server();
    let config = HttpConfig::default();
    let query = serde_urlencoded::to_string([("query", "mutation { hello }")]).expect("query");
    let parts = get(&query, "application/graphql-response+json");

    let response = buffered(run_default(&server, &config, &parts, b"").await);
    assert_eq!(response.status, StatusCode::METHOD_NOT_ALLOWED);
    assert_eq!(allow(&response), Some("POST"));
}

#[tokio::test]
async fn get_rejects_subscriptions_with_405_and_allow_post() {
    let server = support::server();
    let config = HttpConfig::default();
    let query =
        serde_urlencoded::to_string([("query", "subscription { countdown }")]).expect("query");
    let parts = get(&query, "text/event-stream");

    let response = buffered(run_default(&server, &config, &parts, b"").await);
    assert_eq!(response.status, StatusCode::METHOD_NOT_ALLOWED);
    assert_eq!(allow(&response), Some("POST"));
}

#[tokio::test]
async fn post_executes_a_json_request() {
    let server = support::server();
    let config = HttpConfig::default();
    let parts = post("application/json", "application/graphql-response+json");

    let response =
        buffered(run_default(&server, &config, &parts, br#"{"query":"{ hello }"}"#).await);
    assert_eq!(response.status, StatusCode::OK);
    assert!(content_type(&response).starts_with("application/graphql-response+json"));
    assert_eq!(support::json_body(&response.body)["data"]["hello"], "world");
}

#[tokio::test]
async fn post_accepts_a_utf8_charset_parameter() {
    let server = support::server();
    let config = HttpConfig::default();
    let parts = post(
        "application/json; charset=UTF-8",
        "application/graphql-response+json",
    );

    let response =
        buffered(run_default(&server, &config, &parts, br#"{"query":"{ hello }"}"#).await);
    assert_eq!(response.status, StatusCode::OK);
}

#[tokio::test]
async fn post_rejects_a_non_utf8_charset_with_415() {
    let server = support::server();
    let config = HttpConfig::default();
    let parts = post(
        "application/json; charset=utf-16",
        "application/graphql-response+json",
    );

    let response = buffered(run_default(&server, &config, &parts, b"{}").await);
    assert_eq!(response.status, StatusCode::UNSUPPORTED_MEDIA_TYPE);
}

#[tokio::test]
async fn post_rejects_json_arrays_beacuse_batching_is_not_specified() {
    let server = support::server();
    let config = HttpConfig::default();
    let parts = post("application/json", "application/graphql-response+json");
    let body = br#"[{"query":"{ hello }"},{"query":"{ hello }"}]"#;

    let response = buffered(run_default(&server, &config, &parts, body).await);
    assert_eq!(response.status, StatusCode::UNPROCESSABLE_ENTITY);
    assert!(support::json_body(&response.body)["errors"].is_array());
}

#[tokio::test]
async fn malformed_json_is_a_400_with_a_graphql_shaped_body() {
    let server = support::server();
    let config = HttpConfig::default();
    let parts = post("application/json", "application/graphql-response+json");

    let response = buffered(run_default(&server, &config, &parts, b"{").await);
    assert_eq!(response.status, StatusCode::BAD_REQUEST);
    assert!(support::json_body(&response.body)["errors"][0]["message"].is_string());
}

#[tokio::test]
async fn a_wrongly_shaped_request_is_422() {
    let server = support::server();
    let config = HttpConfig::default();
    let parts = post("application/json", "application/graphql-response+json");
    let body = br#"{"query":"{ hello }","variables":[7]}"#;

    let response = buffered(run_default(&server, &config, &parts, body).await);
    assert_eq!(response.status, StatusCode::UNPROCESSABLE_ENTITY);
}

#[tokio::test]
async fn unknown_request_members_are_ignored() {
    let server = support::server();
    let config = HttpConfig::default();
    let parts = post("application/json", "application/graphql-response+json");
    let body = br#"{"query":"{ hello }","somethingNew":{"nested":true}}"#;

    let response = buffered(run_default(&server, &config, &parts, body).await);
    assert_eq!(response.status, StatusCode::OK);
    assert_eq!(support::json_body(&response.body)["data"]["hello"], "world");
}

#[tokio::test]
async fn a_document_that_cannot_be_parsed_is_400() {
    let server = support::server();
    let config = HttpConfig::default();
    let parts = post("application/json", "application/graphql-response+json");

    let response = buffered(run_default(&server, &config, &parts, br#"{"query":"{"}"#).await);
    assert_eq!(response.status, StatusCode::BAD_REQUEST);
    assert!(content_type(&response).starts_with("application/graphql-response+json"));
}

#[tokio::test]
async fn a_document_that_fails_validation_is_422() {
    let server = support::server();
    let config = HttpConfig::default();
    let parts = post("application/json", "application/graphql-response+json");
    let body = br#"{"query":"{ unknownField }"}"#;

    let response = buffered(run_default(&server, &config, &parts, body).await);
    assert_eq!(response.status, StatusCode::UNPROCESSABLE_ENTITY);
}

#[tokio::test]
async fn an_operation_that_cannot_be_selected_is_422() {
    let server = support::server();
    let config = HttpConfig::default();
    let parts = post("application/json", "application/graphql-response+json");
    let body = br#"{"query":"query A { hello } query B { hello }"}"#;

    let response = buffered(run_default(&server, &config, &parts, body).await);
    assert_eq!(response.status, StatusCode::UNPROCESSABLE_ENTITY);
}

#[tokio::test]
async fn variable_coercion_failure_is_422() {
    let server = support::server();
    let config = HttpConfig::default();
    let parts = post("application/json", "application/graphql-response+json");
    let body =
        br#"{"query":"query($message: String!) { echo(message: $message) }","variables":{}}"#;

    let response = buffered(run_default(&server, &config, &parts, body).await);
    assert_eq!(response.status, StatusCode::UNPROCESSABLE_ENTITY);
}

#[tokio::test]
async fn a_field_error_with_data_is_294_partial_success() {
    let server = support::server();
    let config = HttpConfig::default();
    let parts = post("application/json", "application/graphql-response+json");

    let response =
        buffered(run_default(&server, &config, &parts, br#"{"query":"{ boom }"}"#).await);
    assert_eq!(response.status.as_u16(), 294);
    let body = support::json_body(&response.body);
    assert!(body["data"].is_object() || body["data"].is_null());
    assert!(body["errors"].is_array());
}

#[tokio::test]
async fn legacy_json_uses_application_json_for_successes() {
    let server = support::server();
    let config = HttpConfig::default();
    let parts = post("application/json", "application/json");

    let response =
        buffered(run_default(&server, &config, &parts, br#"{"query":"{ hello }"}"#).await);
    assert_eq!(response.status, StatusCode::OK);
    assert_eq!(content_type(&response), "application/json; charset=utf-8");
}

#[tokio::test]
async fn legacy_json_still_uses_specification_status_codes_and_media_type_for_errors() {
    let server = support::server();
    let config = HttpConfig::default();
    let parts = post("application/json", "application/json");

    let response = buffered(run_default(&server, &config, &parts, br#"{"query":"{"}"#).await);
    assert_eq!(response.status, StatusCode::BAD_REQUEST);
    assert!(content_type(&response).starts_with("application/graphql-response+json"));
}

#[tokio::test]
async fn legacy_json_uses_application_json_for_294() {
    let server = support::server();
    let config = HttpConfig::default();
    let parts = post("application/json", "application/json");

    let response =
        buffered(run_default(&server, &config, &parts, br#"{"query":"{ boom }"}"#).await);
    assert_eq!(response.status.as_u16(), 294);
    assert_eq!(content_type(&response), "application/json; charset=utf-8");
}

#[tokio::test]
async fn unsupported_method_is_405_with_allow() {
    let server = support::server();
    let config = HttpConfig::default();
    let parts = support::parts(Method::PUT, "/graphql", &[]);

    let response = buffered(run_default(&server, &config, &parts, b"").await);
    assert_eq!(response.status, StatusCode::METHOD_NOT_ALLOWED);
    assert_eq!(allow(&response), Some("GET, HEAD, POST"));
}

#[tokio::test]
async fn head_runs_a_query_and_strips_the_body() {
    let server = support::server();
    let config = HttpConfig::default();
    let query = serde_urlencoded::to_string([("query", "{ hello }")]).expect("query string");
    let parts = support::parts(
        Method::HEAD,
        &format!("/graphql?{query}"),
        &[(header::ACCEPT, "application/graphql-response+json")],
    );

    let response = buffered(run_default(&server, &config, &parts, b"").await);
    assert_eq!(response.status, StatusCode::OK);
    assert!(response.body.is_empty());
    assert!(content_type(&response).starts_with("application/graphql-response+json"));
}

#[tokio::test]
async fn unsupported_content_type_is_415() {
    let server = support::server();
    let config = HttpConfig::default();
    let parts = post("text/plain", "application/graphql-response+json");

    let response = buffered(run_default(&server, &config, &parts, b"{ hello }").await);
    assert_eq!(response.status, StatusCode::UNSUPPORTED_MEDIA_TYPE);
}

#[tokio::test]
async fn the_response_media_type_is_not_accepted_as_a_request_type() {
    let server = support::server();
    let config = HttpConfig::default();
    let parts = post("application/graphql-response+json", "application/json");

    let response =
        buffered(run_default(&server, &config, &parts, br#"{"query":"{ hello }"}"#).await);
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
    let parts = post("application/json", "text/plain");

    let response =
        buffered(run_default(&server, &config, &parts, br#"{"query":"{ hello }"}"#).await);
    assert_eq!(response.status, StatusCode::NOT_ACCEPTABLE);
}

#[tokio::test]
async fn body_limit_is_413() {
    let server = support::server();
    let config = HttpConfig::default().max_body_bytes(8);
    let parts = post("application/json", "application/graphql-response+json");

    let response =
        buffered(run_default(&server, &config, &parts, br#"{"query":"{ hello }"}"#).await);
    assert_eq!(response.status, StatusCode::PAYLOAD_TOO_LARGE);
}

#[tokio::test]
async fn an_oversized_uri_is_414() {
    let server = support::server();
    let config = HttpConfig::default().max_body_bytes(8);
    let query = serde_urlencoded::to_string([("query", "{ hello }")]).expect("query string");
    let parts = get(&query, "application/graphql-response+json");

    let response = buffered(run_default(&server, &config, &parts, b"").await);
    assert_eq!(response.status, StatusCode::URI_TOO_LONG);
}

#[tokio::test]
async fn denied_context_is_a_401_before_execution() {
    let server = support::server();
    let config = HttpConfig::default();
    let parts = post("application/json", "application/graphql-response+json");

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
async fn a_subscription_without_event_stream_is_422_when_json_is_acceptable() {
    let server = support::server();
    let config = HttpConfig::default();
    let parts = post("application/json", "application/graphql-response+json");

    let response = buffered(
        run_default(
            &server,
            &config,
            &parts,
            br#"{"query":"subscription { countdown }"}"#,
        )
        .await,
    );
    assert_eq!(response.status, StatusCode::UNPROCESSABLE_ENTITY);
    assert!(
        support::json_body(&response.body)["errors"][0]["message"]
            .as_str()
            .unwrap_or_default()
            .contains("Subscriptions")
    );
}

#[cfg(feature = "sse")]
#[tokio::test]
async fn subscriptions_stream_as_server_sent_events() {
    use futures::StreamExt as _;

    let server = support::server();
    let config = HttpConfig::default();
    let parts = post("application/json", "text/event-stream");

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

#[cfg(feature = "sse")]
#[tokio::test]
async fn malformed_subscriptions_return_json_before_streaming() {
    let server = support::server();
    let config = HttpConfig::default();
    let parts = post("application/json", "text/event-stream");

    let response =
        buffered(run_default(&server, &config, &parts, br#"{"query":"subscription {"}"#).await);
    assert_eq!(response.status, StatusCode::BAD_REQUEST);
    assert!(support::json_body(&response.body)["errors"][0]["message"].is_string());
}

#[cfg(not(feature = "sse"))]
#[tokio::test]
async fn event_stream_only_is_406_without_the_sse_feature() {
    let server = support::server();
    let config = HttpConfig::default();
    let parts = post("application/json", "text/event-stream");

    let response = buffered(
        run_default(
            &server,
            &config,
            &parts,
            br#"{"query":"subscription { countdown }"}"#,
        )
        .await,
    );
    assert_eq!(response.status, StatusCode::NOT_ACCEPTABLE);
}

#[tokio::test]
async fn event_stream_is_rejected_for_non_subscriptions() {
    let server = support::server();
    let config = HttpConfig::default();
    let parts = post("application/json", "text/event-stream");

    let response =
        buffered(run_default(&server, &config, &parts, br#"{"query":"{ hello }"}"#).await);
    assert_eq!(response.status, StatusCode::NOT_ACCEPTABLE);
}

#[tokio::test]
async fn response_includes_vary_and_no_store() {
    let server = support::server();
    let config = HttpConfig::default();
    let parts = post("application/json", "application/graphql-response+json");

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
    let parts = post("application/json", "application/graphql-response+json");
    let body = json!({
        "query": "{ hello }",
        "extensions": { "trace": true }
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
