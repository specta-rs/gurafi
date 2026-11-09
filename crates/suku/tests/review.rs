//! Adversarial review probes for `suku`.

use std::time::Duration;

use gql::{Field, ObjectType, Schema, TypeRef, Value};
use suku::{Context, GraphQLRequest, Server};

fn schema() -> Schema {
    Schema::builder()
        .object(
            ObjectType::new("Query").field(
                Field::new("ping", TypeRef::string().non_null())
                    .resolve(|_| Box::pin(async { Ok(Value::from("pong")) })),
            ),
        )
        .query("Query")
        .build()
        .expect("valid schema")
}

/// PROBE S1: `Server::timeout` is documented as a wall-clock deadline for each operation, but
/// the implementation wraps execution in `tokio::time::timeout`, which panics when the future is
/// driven by any other executor. `Server::execute` itself is not Tokio-specific.
#[test]
fn timeout_outside_a_tokio_runtime() {
    let server = Server::new(schema()).timeout(Duration::from_secs(5));
    let request = GraphQLRequest::new("{ ping }");
    let response = futures::executor::block_on(server.execute(&request, Context::empty()));
    assert!(!response.has_errors(), "{:?}", response.errors);
    assert!(response.data.is_some());
}

/// PROBE S2: with `timeout` unset, `Server::execute` works on any executor.
#[test]
fn no_timeout_outside_a_tokio_runtime() {
    let server = Server::new(schema());
    let request = GraphQLRequest::new("{ ping }");
    let response = futures::executor::block_on(server.execute(&request, Context::empty()));
    assert!(!response.has_errors(), "{:?}", response.errors);
    assert!(response.data.is_some());
}

/// GET query strings are bounded by the same body-size setting as POST.
#[tokio::test]
async fn get_query_string_obeys_max_body_bytes() {
    use suku::{HttpConfig, HttpResult, http::handle};

    let query = "a".repeat(256);
    let uri = format!(
        "/graphql?{}",
        serde_urlencoded::to_string([("query", query.as_str())]).expect("query string")
    );
    let parts = ::http::Request::builder()
        .method(::http::Method::GET)
        .uri(uri)
        .header(::http::header::ACCEPT, "application/json")
        .body(())
        .expect("request")
        .into_parts()
        .0;
    let config = HttpConfig::default().max_body_bytes(64);
    let result = handle(&Server::new(schema()), &config, &parts, b"", |_| async {
        Ok(suku::Context::empty())
    })
    .await;
    let HttpResult::Response(response) = result else {
        panic!("expected a buffered response");
    };
    assert_eq!(response.status, ::http::StatusCode::PAYLOAD_TOO_LARGE);
}

/// The default server limits still allow the standard GraphiQL introspection query.
#[tokio::test]
async fn default_limits_allow_standard_introspection() {
    use suku::{Context, GraphQLRequest};

    let schema = gql::Schema::from_sdl(include_str!("../../gql/tests/fixtures/schema.graphql"))
        .expect("fixture schema");
    let server = Server::new(schema);
    let request = GraphQLRequest::new(include_str!(
        "../../gql/tests/fixtures/introspection.graphql"
    ));
    let response = server.execute(&request, Context::empty()).await;
    assert!(!response.has_errors(), "{:?}", response.errors);
    assert!(response.data.is_some());
}
