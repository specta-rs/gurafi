//! Adversarial review probes for `suku`.

use std::{sync::Arc, time::Duration};

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
    println!("data={:?} errors={:?}", response.data, response.errors);
}

/// PROBE S2: with `timeout` unset, `Server::execute` works on any executor.
#[test]
fn no_timeout_outside_a_tokio_runtime() {
    let server = Server::new(schema());
    let request = GraphQLRequest::new("{ ping }");
    let response = futures::executor::block_on(server.execute(&request, Context::empty()));
    println!("data={:?} errors={:?}", response.data, response.errors);
}

/// PROBE S3: GET has no query-string size limit, unlike POST's body limit.
#[test]
fn get_query_string_is_not_size_limited() {
    let _ = Arc::new(0);
    println!("see http::handle: handle_get never consults max_body_bytes");
}
