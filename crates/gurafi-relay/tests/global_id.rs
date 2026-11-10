//! Global ID encoding, decoding and GraphQL coercion.

#![allow(dead_code)]

use futures::executor::block_on;
use gurafi::{Input, NoArgs, Object, Schema, gql};
use gurafi_relay::{GlobalId, decode_global_id};

#[derive(Object)]
struct User {
    id: String,
}

#[derive(Object)]
struct Post {
    id: String,
}

#[derive(Input)]
struct ById {
    id: GlobalId<User>,
}

struct Ctx;

fn execute(schema: &gql::Schema, query: &str) -> gql::Response {
    block_on(gql::execute(
        schema,
        gql::Request::new(query),
        &gql::ExecutionOptions::with_context(Ctx),
    ))
}

fn data(response: &gql::Response) -> &gql::Object {
    response
        .data
        .as_ref()
        .and_then(gql::Value::as_object)
        .unwrap_or_else(|| panic!("missing data: {:?}", response.errors))
}

fn schema() -> gql::Schema {
    let mut schema = Schema::<Ctx>::new();
    schema.query(|q| {
        q.field("encoded", |_: NoArgs, _: &Ctx| GlobalId::<User>::new("42"));
        q.field("local", |args: ById, _: &Ctx| args.id.into_local());
    });
    schema.build().expect("valid schema")
}

#[test]
fn round_trip_and_helpers() {
    let id = GlobalId::<User>::new("42");
    assert_eq!(id.as_local(), "42");
    assert_eq!(id.local_id(), "42");
    assert_eq!(GlobalId::<User>::typename(), "User");
    assert_eq!(id.encode(), "VXNlcjo0Mg==");
    assert_eq!(id.to_string(), "VXNlcjo0Mg==");

    let decoded = GlobalId::<User>::decode("VXNlcjo0Mg==").unwrap();
    assert_eq!(decoded, id);
    assert_eq!(decoded.into_local(), "42");

    let any = decode_global_id("VXNlcjo0Mg==").unwrap();
    assert_eq!(any.typename, "User");
    assert_eq!(any.local_id, "42");
}

#[test]
fn decode_rejects_wrong_type_and_malformed_values() {
    let wrong = GlobalId::<User>::decode(GlobalId::<Post>::new("42").encode().as_str())
        .expect_err("Post ID must not decode as User");
    assert!(wrong.message().contains("Post"));

    for malformed in [
        "",
        "not base64!",
        "VXNlcjo0Mg", // missing padding
        "VXNlcjo0Mg===",
        "VXNlcjo0Mg==extra",
        "Zm9v", // decodes to "foo", no colon
        "OjQy", // decodes to ":42", empty type name
        "//79", // not UTF-8
    ] {
        assert!(
            GlobalId::<User>::decode(malformed).is_err(),
            "{malformed:?} should be rejected"
        );
    }
}

#[test]
fn graphql_uses_id_for_input_and_output() {
    let schema = schema();
    let response = execute(&schema, r#"{ encoded local(id: "VXNlcjo0Mg==") }"#);
    assert!(!response.has_errors(), "{:?}", response.errors);
    let data = data(&response);
    assert_eq!(
        data.get("encoded").and_then(gql::Value::as_str),
        Some("VXNlcjo0Mg==")
    );
    assert_eq!(data.get("local").and_then(gql::Value::as_str), Some("42"));

    let sdl = schema.sdl();
    assert!(sdl.contains("id: ID!"), "SDL was:\n{sdl}");
}

#[test]
fn graphql_rejects_an_id_for_another_type() {
    let schema = schema();
    let response = execute(
        &schema,
        r#"{ local(id: "UG9zdDo0Mg==") }"#, // Post:42
    );
    assert!(response.has_errors());
    assert!(
        response
            .errors
            .iter()
            .any(|error| error.message().contains("Post") && error.message().contains("User")),
        "{:?}",
        response.errors
    );
}

#[test]
fn graphql_rejects_malformed_ids() {
    let schema = schema();
    let response = execute(&schema, r#"{ local(id: "not-base64") }"#);
    assert!(response.has_errors());
    assert!(
        response
            .errors
            .iter()
            .any(|error| error.message().contains("Invalid global ID")),
        "{:?}",
        response.errors
    );
}
