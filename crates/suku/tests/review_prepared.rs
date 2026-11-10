//! Regression tests for persisted-query security and frontend review findings.

mod support;

use ::http::{Method, StatusCode, header};
use gql::{Field, ObjectType, Schema, TypeRef, Value};
use suku::{HttpConfig, HttpResult, PreparedOperation, PreparedQueries, Server, http::handle};

fn buffered(result: HttpResult) -> suku::HttpResponse {
    match result {
        HttpResult::Response(response) => response,
        HttpResult::Stream(_) => panic!("expected a buffered response"),
    }
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
async fn get_with_only_apq_extensions_is_rejected_before_resolution() {
    let schema = support::schema();
    let prepared = PreparedQueries::from_map(&schema, [("hello", "{ hello }")]).expect("manifest");
    let server = Server::new(schema).prepared_queries(prepared);

    // Apollo's persisted-query link sends only the hash (no `query`) on a cache hit, and clients
    // configured for GET send that as a query-string `extensions` parameter.
    let response = get(
        &server,
        "/graphql?extensions=%7B%22persistedQuery%22%3A%7B%22version%22%3A1%2C%22sha256Hash%22%3A%22hello%22%7D%7D",
        &[(header::ACCEPT, "application/json")],
    )
    .await;

    assert_eq!(response.status, StatusCode::OK, "{:?}", body(&response));
    assert_eq!(body(&response)["data"]["hello"], "world");
}

#[tokio::test]
async fn get_with_only_extension_id_is_rejected_before_resolution() {
    let schema = support::schema();
    let prepared = PreparedQueries::from_map(&schema, [("hello", "{ hello }")]).expect("manifest");
    let server = Server::new(schema).prepared_queries(prepared);

    let response = get(
        &server,
        "/graphql?extensions=%7B%22id%22%3A%22hello%22%7D",
        &[(header::ACCEPT, "application/json")],
    )
    .await;

    assert_eq!(response.status, StatusCode::OK, "{:?}", body(&response));
    assert_eq!(body(&response)["data"]["hello"], "world");
}

fn secret_schema() -> Schema {
    let query = ObjectType::new("Query").field(
        Field::new("secret", TypeRef::string().non_null())
            .resolve(|_| Box::pin(async { Ok(Value::from("leaked")) })),
    );
    Schema::builder()
        .object(query)
        .query("Query")
        .build()
        .expect("schema")
}

async fn post(server: &Server, body: &str) -> suku::HttpResponse {
    let parts = support::parts(
        Method::POST,
        "/graphql",
        &[
            (header::CONTENT_TYPE, "application/json"),
            (header::ACCEPT, "application/json"),
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

#[tokio::test]
async fn registry_prepared_against_another_schema_runs_against_that_schema() {
    let prepared_schema = secret_schema();
    let prepared =
        PreparedQueries::from_map(&prepared_schema, [("secret", "{ secret }")]).expect("manifest");

    // The server's schema has no `secret` field, so the request must not be able to reach it.
    let server = Server::new(support::schema()).prepared_queries(prepared);
    let response = post(&server, r#"{"id":"secret"}"#).await;

    let body = body(&response);
    assert!(
        body.get("data")
            .and_then(|data| data.get("secret"))
            .is_none(),
        "server executed a prepared query against a schema it was not built for: {body}"
    );
}

#[tokio::test]
async fn header_id_overrides_extension_id_even_though_body_has_one() {
    let schema = support::schema();
    let prepared = PreparedQueries::from_map(
        &schema,
        [
            ("hello", "{ hello }"),
            ("other", "{ echo(message: \"other\") }"),
        ],
    )
    .expect("manifest");
    let server = Server::new(schema).prepared_queries(prepared);

    let name = header::HeaderName::from_static("x-persisted-query-id");
    let response = post_with_headers(
        &server,
        r#"{"extensions":{"persistedQuery":{"sha256Hash":"other"}}}"#,
        &[(name, "hello")],
    )
    .await;

    let body = body(&response);
    assert!(
        body["data"].get("echo").is_some(),
        "the header should only fill in a missing id, but it overrode the extension id: {body}"
    );
}

async fn post_with_headers(
    server: &Server,
    body: &str,
    headers: &[(header::HeaderName, &str)],
) -> suku::HttpResponse {
    let mut all = vec![
        (header::CONTENT_TYPE, "application/json"),
        (header::ACCEPT, "application/json"),
    ];
    all.extend_from_slice(headers);
    let parts = support::parts(Method::POST, "/graphql", &all);
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

#[test]
fn duplicate_ids_in_a_map_manifest_are_rejected() {
    let schema = support::schema();
    let prepared =
        PreparedQueries::from_json_str(&schema, r#"{"same":"{ hello }","same":"{ boom }"}"#);
    assert!(
        prepared.is_err(),
        "duplicate manifest ids were silently accepted"
    );
}

#[tokio::test]
async fn persisted_request_operation_name_must_match_the_prepared_operation() {
    let schema = support::schema();
    let prepared = PreparedQueries::from_operations(
        &schema,
        [
            PreparedOperation::new(
                "echo",
                "query Echo($message: String!) { echo(message: $message) }",
            )
            .operation_name("Echo"),
            PreparedOperation::new("hello", "{ hello }"),
        ],
    )
    .expect("manifest");
    let server = Server::new(schema).prepared_queries(prepared);

    let response = post(
        &server,
        r#"{"id":"echo","operationName":"Other","variables":{"message":"hi"}}"#,
    )
    .await;
    let first = body(&response);
    let message = first["errors"][0]["message"]
        .as_str()
        .unwrap_or_else(|| panic!("expected an error: {first}"));
    assert!(
        message.contains("Other"),
        "operationName mismatch should be reported, got: {message}"
    );
    assert!(first.get("data").is_none() || first["data"].is_null());

    let response = post(
        &server,
        r#"{"id":"echo","operationName":"Echo","variables":{"message":"hi"}}"#,
    )
    .await;
    let body = body(&response);
    assert_eq!(body["data"]["echo"], "hi", "{body}");
}

#[tokio::test]
async fn header_id_is_a_per_item_fallback_for_batches() {
    let schema = support::schema();
    let prepared = PreparedQueries::from_map(
        &schema,
        [
            ("hello", "{ hello }"),
            ("other", "{ echo(message: \"other\") }"),
        ],
    )
    .expect("manifest");
    let server = Server::new(schema).prepared_queries(prepared);

    let name = header::HeaderName::from_static("x-persisted-query-id");
    let response = post_with_headers(
        &server,
        r#"[
            {"extensions":{"persistedQuery":{"sha256Hash":"other"}}},
            {}
        ]"#,
        &[(name, "hello")],
    )
    .await;

    let body = body(&response);
    assert_eq!(
        body[0]["data"]["echo"], "other",
        "first item's extension id must win over the header: {body}"
    );
    assert_eq!(
        body[1]["data"]["hello"], "world",
        "the header must still fill in an item with no identifier: {body}"
    );
}
