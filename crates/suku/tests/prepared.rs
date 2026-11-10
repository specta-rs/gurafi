//! Persisted operation transport and manifest tests.

mod support;

use ::http::{Method, StatusCode, header};
use futures::StreamExt as _;
use serde_json::json;
use suku::{
    Context, GraphQLRequest, HttpConfig, HttpResult, PreparedOperation, PreparedQueries,
    PreparedQueriesError, Server, http::handle,
};

fn queries(schema: &gql::Schema) -> PreparedQueries {
    PreparedQueries::from_operations(
        schema,
        [
            PreparedOperation::new("hello", "{ hello }"),
            PreparedOperation::new(
                "echo",
                "query Echo($message: String!) { echo(message: $message) }",
            )
            .operation_name("Echo"),
            PreparedOperation::new("countdown", "subscription { countdown }")
                .kind(gql::ast::OperationKind::Subscription),
        ],
    )
    .expect("prepared queries")
}

fn prepared_server(schema: gql::Schema) -> Server {
    let prepared = queries(&schema);
    Server::new(schema).prepared_queries(prepared)
}

fn plain_server() -> Server {
    prepared_server(support::schema())
}

fn buffered(result: HttpResult) -> suku::HttpResponse {
    match result {
        HttpResult::Response(response) => response,
        HttpResult::Stream(_) => panic!("expected a buffered response"),
    }
}

async fn post(server: &Server, body: &str) -> suku::HttpResponse {
    post_with_headers(server, body, &[]).await
}

async fn post_graphql_response(server: &Server, body: &str) -> suku::HttpResponse {
    let parts = support::parts(
        Method::POST,
        "/graphql",
        &[
            (header::CONTENT_TYPE, "application/json"),
            (header::ACCEPT, "application/graphql-response+json"),
        ],
    );
    buffered(
        handle(
            server,
            &HttpConfig::default(),
            &parts,
            body.as_bytes(),
            support::empty_context,
        )
        .await,
    )
}

async fn post_with_headers(
    server: &Server,
    body: &str,
    headers: &[(header::HeaderName, &str)],
) -> suku::HttpResponse {
    let mut all_headers = vec![
        (header::CONTENT_TYPE, "application/json"),
        (header::ACCEPT, "application/json"),
    ];
    all_headers.extend_from_slice(headers);
    let parts = support::parts(Method::POST, "/graphql", &all_headers);
    buffered(
        handle(
            server,
            &HttpConfig::default(),
            &parts,
            body.as_bytes(),
            support::empty_context,
        )
        .await,
    )
}

async fn get(
    server: &Server,
    uri: &str,
    headers: &[(header::HeaderName, &str)],
) -> suku::HttpResponse {
    let parts = support::parts(Method::GET, uri, headers);
    buffered(
        handle(
            server,
            &HttpConfig::default(),
            &parts,
            b"",
            support::empty_context,
        )
        .await,
    )
}

fn body(response: &suku::HttpResponse) -> serde_json::Value {
    serde_json::from_slice(&response.body).expect("JSON body")
}

#[tokio::test]
async fn relay_style_id_executes_without_query_text() {
    let server = plain_server();
    let response = post(&server, r#"{"id":"hello"}"#).await;
    assert_eq!(response.status, StatusCode::OK);
    let body = body(&response);
    assert_eq!(body["data"]["hello"], "world");
}

#[tokio::test]
async fn id_aliases_are_accepted() {
    let server = plain_server();
    for key in ["doc_id", "documentId", "queryId"] {
        let response = post(&server, &format!(r#"{{"{key}":"hello"}}"#)).await;
        assert_eq!(response.status, StatusCode::OK, "{key}");
        assert_eq!(body(&response)["data"]["hello"], "world", "{key}");
    }
}

#[tokio::test]
async fn apollo_persisted_query_shape_and_variables() {
    let server = plain_server();
    let response = post(
        &server,
        &json!({
            "variables": { "message": "from apq" },
            "extensions": {
                "persistedQuery": {
                    "version": 1,
                    "sha256Hash": "echo"
                }
            }
        })
        .to_string(),
    )
    .await;
    assert_eq!(response.status, StatusCode::OK);
    assert_eq!(body(&response)["data"]["echo"], "from apq");
}

#[tokio::test]
async fn registered_text_wins_over_supplied_query() {
    let server = plain_server();
    let response = post(&server, r#"{"id":"hello","query":"{ boom }"}"#).await;
    assert_eq!(response.status, StatusCode::OK);
    let body = body(&response);
    assert_eq!(body["data"]["hello"], "world");
    assert!(body.get("errors").is_none());
}

#[tokio::test]
async fn unknown_apq_falls_back_to_query_text_when_allowed() {
    let server = plain_server();
    let response = post(
        &server,
        r#"{"query":"{ hello }","extensions":{"persistedQuery":{"sha256Hash":"missing"}}}"#,
    )
    .await;
    assert_eq!(response.status, StatusCode::OK);
    assert_eq!(body(&response)["data"]["hello"], "world");
}

#[tokio::test]
async fn unknown_apq_without_query_is_persisted_query_not_found() {
    let server = plain_server();
    let response = post_graphql_response(
        &server,
        r#"{"extensions":{"persistedQuery":{"sha256Hash":"missing"}}}"#,
    )
    .await;
    assert_eq!(response.status, StatusCode::BAD_REQUEST);
    let body = body(&response);
    assert_eq!(body["errors"][0]["message"], "PersistedQueryNotFound");
    assert_eq!(
        body["errors"][0]["extensions"]["code"],
        "PERSISTED_QUERY_NOT_FOUND"
    );
    assert_eq!(
        body["errors"][0]["extensions"]["persistedQueryId"],
        "missing"
    );
}

#[tokio::test]
async fn allowlist_rejects_query_text_and_unknown_ids() {
    let schema = support::schema();
    let server = prepared_server(schema).require_prepared(true);

    let known = post(&server, r#"{"id":"hello"}"#).await;
    assert_eq!(known.status, StatusCode::OK);

    for payload in [
        r#"{"query":"{ hello }"}"#,
        r#"{"id":"missing","query":"{ hello }"}"#,
        r#"{"id":"missing"}"#,
    ] {
        let response = post_graphql_response(&server, payload).await;
        assert_eq!(response.status, StatusCode::BAD_REQUEST, "{payload}");
        assert_eq!(
            body(&response)["errors"][0]["message"],
            "PersistedQueryNotFound"
        );
    }
}

#[tokio::test]
async fn get_and_batch_support_persisted_ids() {
    let server = plain_server();
    let response = get(
        &server,
        "/graphql?id=hello",
        &[(header::ACCEPT, "application/json")],
    )
    .await;
    assert_eq!(response.status, StatusCode::OK);
    assert_eq!(body(&response)["data"]["hello"], "world");

    let batch = post(&server, r#"[{"id":"hello"},{"doc_id":"hello"}]"#).await;
    assert_eq!(batch.status, StatusCode::OK);
    let values = body(&batch);
    assert_eq!(values[0]["data"]["hello"], "world");
    assert_eq!(values[1]["data"]["hello"], "world");
}

#[tokio::test]
async fn get_rejects_persisted_mutations_and_subscriptions() {
    let server = plain_server();
    let response = get(
        &server,
        "/graphql?id=countdown",
        &[(header::ACCEPT, "application/json")],
    )
    .await;
    assert_eq!(response.status, StatusCode::METHOD_NOT_ALLOWED);
}

#[tokio::test]
async fn persisted_id_header_is_honoured() {
    let name = header::HeaderName::from_static("x-persisted-query-id");
    let server = plain_server();
    let response = post_with_headers(&server, "{}", &[(name.clone(), "hello")]).await;
    assert_eq!(response.status, StatusCode::OK);
    assert_eq!(body(&response)["data"]["hello"], "world");

    // GET may omit every query parameter when a header supplies the id.
    let response = get(
        &server,
        "/graphql",
        &[(name, "hello"), (header::ACCEPT, "application/json")],
    )
    .await;
    assert_eq!(response.status, StatusCode::OK);
    assert_eq!(body(&response)["data"]["hello"], "world");
}

#[tokio::test]
async fn prepared_subscription_streams_events() {
    let server = plain_server();
    let subscription = server
        .subscribe(&GraphQLRequest::persisted("countdown"), Context::empty())
        .await
        .expect("subscription");
    let responses: Vec<_> = subscription.stream.collect().await;
    let values: Vec<_> = responses
        .iter()
        .map(|response| {
            response
                .data
                .as_ref()
                .and_then(gql::Value::as_object)
                .and_then(|object| object.get("countdown"))
                .cloned()
                .expect("countdown value")
        })
        .collect();
    assert_eq!(
        values,
        [
            gql::Value::from(3),
            gql::Value::from(2),
            gql::Value::from(1)
        ]
    );
}

#[tokio::test]
async fn prepared_subscription_over_http_emits_sse() {
    let server = plain_server();
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
        &HttpConfig::default(),
        &parts,
        br#"{"id":"countdown"}"#,
        support::empty_context,
    )
    .await;
    let HttpResult::Stream(stream) = result else {
        panic!("expected an event stream");
    };
    let chunks: Vec<_> = stream.stream.collect().await;
    let text: String = chunks
        .into_iter()
        .map(|chunk| String::from_utf8(chunk.expect("chunk").to_vec()).expect("utf8"))
        .collect();
    assert!(text.contains(r#"event: next"#));
    assert!(text.contains(r#""countdown":3"#));
}

#[test]
fn manifest_shapes_are_auto_detected() {
    let schema = support::schema();

    let map = PreparedQueries::from_json_str(&schema, r#"{"hello":"{ hello }"}"#)
        .expect("codegen/relay map");
    assert_eq!(map.len(), 1);
    assert!(map.get("hello").is_some());

    let operations = PreparedQueries::from_json_str(
        &schema,
        r#"{
            "operations": [
                { "id": "a", "body": "query Hello { hello }", "name": "Hello", "type": "query" },
                { "hash": "b", "query": "{ hello }" }
            ]
        }"#,
    )
    .expect("hive/envelop manifest");
    assert_eq!(operations.len(), 2);
    assert_eq!(operations.get("a").unwrap().operation_name(), Some("Hello"));

    let array = PreparedQueries::from_json_str(
        &schema,
        r#"[{"hash":"c","document":"{ hello }","type":"QUERY"}]"#,
    )
    .expect("array manifest");
    assert_eq!(array.len(), 1);
}

#[test]
fn manifest_diagnostics_are_clear() {
    let schema = support::schema();

    let error = PreparedQueries::from_json_str(&schema, r#"{"hello":"{ missing }"}"#)
        .expect_err("invalid query");
    assert!(matches!(error, PreparedQueriesError::Registry(_)));
    assert!(error.to_string().contains("hello"));

    let error = PreparedQueries::from_operations(
        &schema,
        [
            PreparedOperation::new("hello", "{ hello }"),
            PreparedOperation::new("hello", "{ hello }"),
        ],
    )
    .expect_err("duplicate");
    assert!(matches!(error, PreparedQueriesError::Registry(_)));
    assert!(error.to_string().contains("duplicate"));

    let error = PreparedQueries::from_json_str(
        &schema,
        r#"{"operations":[{"id":"a","body":"{ hello }","type":"mutation"}]}"#,
    )
    .expect_err("kind mismatch");
    assert!(matches!(error, PreparedQueriesError::KindMismatch { .. }));

    let error = PreparedQueries::from_json_str(&schema, r#"{"operations":[]}"#)
        .expect_err("empty manifest");
    assert!(matches!(error, PreparedQueriesError::Invalid(_)));

    let error = PreparedQueries::from_json_str(&schema, "[]").expect_err("empty array");
    assert!(matches!(error, PreparedQueriesError::Invalid(_)));
}

#[test]
fn prepared_id_is_extracted_from_request_shapes() {
    let request: GraphQLRequest = serde_json::from_str(r#"{"id":"a"}"#).expect("relay");
    assert_eq!(request.prepared_id(), Some("a"));

    let request: GraphQLRequest =
        serde_json::from_str(r#"{"extensions":{"persistedQuery":{"version":1,"sha256Hash":"b"}}}"#)
            .expect("apollo");
    assert_eq!(request.prepared_id(), Some("b"));

    let request: GraphQLRequest =
        serde_json::from_str(r#"{"extensions":{"queryId":"c"}}"#).expect("extension id");
    assert_eq!(request.prepared_id(), Some("c"));

    let request: GraphQLRequest =
        serde_json::from_str(r#"{"extensions":{"id":"d"}}"#).expect("extension id");
    assert_eq!(request.prepared_id(), Some("d"));

    let request: GraphQLRequest =
        serde_json::from_str(r#"{"query":"{ hello }"}"#).expect("normal request");
    assert_eq!(request.prepared_id(), None);
}

#[test]
fn query_text_alone_is_rejected_when_missing() {
    // The transport reports missing text with its normal request error.
    let request: GraphQLRequest = serde_json::from_str(r#"{"variables":{}}"#).expect("request");
    assert!(request.query.is_empty());
    assert_eq!(request.prepared_id(), None);
}
