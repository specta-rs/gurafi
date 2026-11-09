//! Request JSON parsing and conversion tests.

use serde_json::json;
use suku::{GraphQLRequest, RequestPayload};

#[test]
fn request_serializes_operation_name_as_camel_case() {
    let request = GraphQLRequest {
        query: "{ hello }".to_owned(),
        operation_name: Some("Greeting".to_owned()),
        variables: Some(
            json!({ "name": "Ada" })
                .as_object()
                .cloned()
                .expect("object"),
        ),
        extensions: None,
    };

    let json = serde_json::to_value(&request).expect("serialize");
    assert_eq!(json["operationName"], "Greeting");
    assert!(json.get("operation_name").is_none());
    assert!(json.get("extensions").is_none());
}

#[test]
fn request_deserializes_variables_and_extensions() {
    let request: GraphQLRequest = serde_json::from_value(json!({
        "query": "query Greeting($name: String!) { hello(name: $name) }",
        "operationName": "Greeting",
        "variables": { "name": "Ada" },
        "extensions": { "persisted": false }
    }))
    .expect("valid request");

    assert_eq!(request.operation_name.as_deref(), Some("Greeting"));
    assert_eq!(
        request
            .variables
            .as_ref()
            .expect("variables")
            .get("name")
            .and_then(|value| value.as_str()),
        Some("Ada")
    );
    assert!(request.extensions.is_some());
}

#[test]
fn request_reports_bad_json_instead_of_panicking() {
    let error = serde_json::from_slice::<GraphQLRequest>(br#"{ "query": "#).expect_err("bad json");
    assert!(error.to_string().contains("EOF") || error.to_string().contains("expected"));
}

#[test]
fn variables_convert_to_graphql_values() {
    let request: GraphQLRequest = serde_json::from_value(json!({
        "query": "{ echo }",
        "variables": {
            "int": 42,
            "float": 1.5,
            "bool": true,
            "null": null,
            "list": [1, "two", null],
            "nested": { "value": "x" }
        }
    }))
    .expect("valid request");

    let variables = request.variables_object().expect("convert").expect("some");
    assert_eq!(variables.len(), 6);
    assert_eq!(
        variables.get("int").and_then(|value| value.as_i32()),
        Some(42)
    );
    assert_eq!(
        variables.get("float").and_then(|value| value.as_f64()),
        Some(1.5)
    );
    assert_eq!(
        variables
            .get("nested")
            .and_then(|value| value.as_object())
            .and_then(|object| object.get("value"))
            .and_then(|value| value.as_str()),
        Some("x")
    );
    assert!(
        variables
            .get("list")
            .and_then(|value| value.as_list())
            .is_some()
    );
}

#[test]
fn integers_outside_i32_are_rejected_with_a_clear_error() {
    let request: GraphQLRequest = serde_json::from_value(json!({
        "query": "{ echo }",
        "variables": { "big": 2147483648_i64 }
    }))
    .expect("valid request");

    let error = request.variables_object().expect_err("should reject");
    assert!(error.message().contains("Int range"));
    assert!(error.message().contains("custom scalar"));
}

#[test]
fn batch_payloads_are_parsed_and_limited() {
    let payload =
        RequestPayload::from_json_slice(br#"[{"query":"{ hello }"},{"query":"{ hello }"}]"#, 4)
            .expect("batch");
    assert!(payload.is_batch());
    assert_eq!(payload.requests().len(), 2);

    let unlimited = RequestPayload::from_json_slice(br#"[{"query":"{ hello }"}]"#, 0)
        .expect("zero means unlimited");
    assert_eq!(unlimited.requests().len(), 1);

    let too_many = RequestPayload::from_json_slice(br#"[{"query":"a"},{"query":"b"}]"#, 1)
        .expect_err("too many");
    assert!(too_many.message().contains("at most 1"));

    let empty = RequestPayload::from_json_slice(b"[]", 10).expect_err("empty");
    assert!(empty.message().contains("at least one"));
}

#[test]
fn single_payloads_are_detected() {
    let payload = RequestPayload::from_json_slice(br#"{"query":"{ hello }"}"#, 10).expect("single");
    assert!(!payload.is_batch());
    assert_eq!(payload.requests()[0].query, "{ hello }");
}

#[test]
fn malformed_payloads_report_an_error() {
    let error =
        RequestPayload::from_json_slice(br#"[{"query":"{ hello }"}"#, 10).expect_err("bad json");
    assert!(error.message().contains("Invalid batch request"));
}
