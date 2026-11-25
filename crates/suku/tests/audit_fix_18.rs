//! Regression for audit finding #18: subscription setup error classification.

#![cfg(feature = "sse")]

mod support;

use std::sync::{Arc, Mutex};

use ::http::{Method, StatusCode, header};
use gql::{
    AuthorizationContext, Authorizer, Field, GraphQLError, InputValue, ObjectType, Schema, TypeRef,
    Value,
};
use suku::{
    ErrorKind, GraphQLRequest, HttpConfig, HttpResult, PreparedOperation, PreparedQueries, Server,
    http::handle, mask_internal,
};

const INTERNAL_DETAIL: &str = "backend=dummy-db.internal secret=DUMMY_SECRET";
const SUBSCRIPTION: &str = "subscription { secretSource(limit: 1) }";

fn schema() -> Schema {
    let source = Field::new("secretSource", TypeRef::string())
        .arg(InputValue::new("limit", TypeRef::int().non_null()))
        .subscribe(|_| {
            Box::pin(async {
                Err::<gql::SubscriptionStream, GraphQLError>(GraphQLError::new(INTERNAL_DETAIL))
            })
        });
    let query = ObjectType::new("Query").field(
        Field::new("hello", TypeRef::string().non_null())
            .resolve(|_| Box::pin(async { Ok(Value::from("world")) })),
    );
    let subscription = ObjectType::new("Subscription").field(source);
    let mut builder = Schema::builder();
    builder.add_type(query.into());
    builder.add_type(subscription.into());
    builder
        .query("Query")
        .subscription("Subscription")
        .build()
        .expect("valid schema")
}

fn query_only_schema() -> Schema {
    let query = ObjectType::new("Query").field(
        Field::new("hello", TypeRef::string().non_null())
            .resolve(|_| Box::pin(async { Ok(Value::from("world")) })),
    );
    Schema::builder()
        .object(query)
        .query("Query")
        .build()
        .expect("valid schema")
}

fn missing_subscribe_schema() -> Schema {
    let source = Field::new("secretSource", TypeRef::string());
    let subscription = ObjectType::new("Subscription").field(source);
    let mut builder = Schema::builder();
    builder.add_type(
        ObjectType::new("Query")
            .field(
                Field::new("hello", TypeRef::string().non_null())
                    .resolve(|_| Box::pin(async { Ok(Value::from("world")) })),
            )
            .into(),
    );
    builder.add_type(subscription.into());
    builder
        .query("Query")
        .subscription("Subscription")
        .build()
        .expect("valid schema")
}

struct DenySetup;

impl Authorizer for DenySetup {
    fn authorize(&self, context: &AuthorizationContext<'_>) -> Result<(), GraphQLError> {
        if context.field_name == "secretSource" {
            Err(GraphQLError::new(INTERNAL_DETAIL))
        } else {
            Ok(())
        }
    }
}

fn recording_server(server: Server, kinds: Arc<Mutex<Vec<ErrorKind>>>) -> Server {
    server.format_error(move |error, context| {
        kinds.lock().expect("lock").push(context.kind());
        mask_internal(error, context)
    })
}

async fn post(server: &Server, request: &GraphQLRequest) -> suku::HttpResponse {
    let body = serde_json::to_vec(request).expect("serialize request");
    let parts = support::parts(
        Method::POST,
        "/graphql",
        &[
            (header::CONTENT_TYPE, "application/json"),
            (header::ACCEPT, "text/event-stream"),
        ],
    );
    match handle(
        server,
        &HttpConfig::default(),
        &parts,
        &body,
        support::empty_context,
    )
    .await
    {
        HttpResult::Response(response) => response,
        HttpResult::Stream(_) => panic!("a setup failure must not open a stream"),
    }
}

fn assert_masked_setup_failure(response: suku::HttpResponse) {
    assert_eq!(response.status, StatusCode::INTERNAL_SERVER_ERROR);
    let json: serde_json::Value = serde_json::from_slice(&response.body).expect("JSON body");
    assert_eq!(json["errors"][0]["message"], suku::INTERNAL_ERROR_MESSAGE);
    assert!(
        !String::from_utf8_lossy(&response.body).contains(INTERNAL_DETAIL),
        "internal setup detail leaked"
    );
}

#[tokio::test]
async fn source_setup_failure_is_an_execution_error() {
    let kinds = Arc::new(Mutex::new(Vec::new()));
    let server = recording_server(Server::new(schema()), Arc::clone(&kinds));

    assert_masked_setup_failure(post(&server, &GraphQLRequest::new(SUBSCRIPTION)).await);
    assert_eq!(*kinds.lock().expect("lock"), vec![ErrorKind::Execution]);
}

#[tokio::test]
async fn authorizer_setup_failure_is_an_execution_error() {
    let kinds = Arc::new(Mutex::new(Vec::new()));
    let server = recording_server(
        Server::new(schema()).authorizer(DenySetup),
        Arc::clone(&kinds),
    );

    assert_masked_setup_failure(post(&server, &GraphQLRequest::new(SUBSCRIPTION)).await);
    assert_eq!(*kinds.lock().expect("lock"), vec![ErrorKind::Execution]);
}

#[tokio::test]
async fn prepared_source_setup_failure_is_an_execution_error() {
    let schema = schema();
    let prepared =
        PreparedQueries::from_operations(&schema, [PreparedOperation::new("secret", SUBSCRIPTION)])
            .expect("prepared queries");
    let kinds = Arc::new(Mutex::new(Vec::new()));
    let server = recording_server(
        Server::new(schema).prepared_queries(prepared),
        Arc::clone(&kinds),
    );

    assert_masked_setup_failure(post(&server, &GraphQLRequest::persisted("secret")).await);
    assert_eq!(*kinds.lock().expect("lock"), vec![ErrorKind::Execution]);
}

#[tokio::test]
async fn argument_coercion_failure_remains_a_request_error() {
    let kinds = Arc::new(Mutex::new(Vec::new()));
    let server = recording_server(Server::new(schema()), Arc::clone(&kinds));
    let mut request =
        GraphQLRequest::new("subscription S($limit: Int = 1) { secretSource(limit: $limit) }");
    request = request.variables(
        serde_json::json!({ "limit": null })
            .as_object()
            .expect("variables object")
            .clone(),
    );

    let response = post(&server, &request).await;
    assert_eq!(response.status, StatusCode::UNPROCESSABLE_ENTITY);
    let json: serde_json::Value = serde_json::from_slice(&response.body).expect("JSON body");
    let message = json["errors"][0]["message"].as_str().unwrap_or_default();
    assert!(!message.is_empty());
    assert_ne!(message, suku::INTERNAL_ERROR_MESSAGE);
    assert_eq!(*kinds.lock().expect("lock"), vec![ErrorKind::Request]);
}

#[tokio::test]
async fn prepared_schema_mismatch_is_masked_as_a_server_error() {
    let prepared_schema = schema();
    let prepared = PreparedQueries::from_operations(
        &prepared_schema,
        [PreparedOperation::new("secret", SUBSCRIPTION)],
    )
    .expect("prepared queries");
    let kinds = Arc::new(Mutex::new(Vec::new()));
    let server = recording_server(
        Server::new(schema()).prepared_queries(prepared),
        Arc::clone(&kinds),
    );

    assert_masked_setup_failure(post(&server, &GraphQLRequest::persisted("secret")).await);
    assert_eq!(*kinds.lock().expect("lock"), vec![ErrorKind::Execution]);
}

#[tokio::test]
async fn missing_subscribe_resolver_is_masked_as_a_server_error() {
    let kinds = Arc::new(Mutex::new(Vec::new()));
    let server = recording_server(Server::new(missing_subscribe_schema()), Arc::clone(&kinds));

    assert_masked_setup_failure(
        post(
            &server,
            &GraphQLRequest::new("subscription { secretSource }"),
        )
        .await,
    );
    assert_eq!(*kinds.lock().expect("lock"), vec![ErrorKind::Execution]);
}

#[tokio::test]
async fn missing_subscription_root_is_masked_as_a_server_error() {
    let kinds = Arc::new(Mutex::new(Vec::new()));
    let server = recording_server(Server::new(query_only_schema()), Arc::clone(&kinds));

    assert_masked_setup_failure(post(&server, &GraphQLRequest::new(SUBSCRIPTION)).await);
    assert_eq!(*kinds.lock().expect("lock"), vec![ErrorKind::Execution]);
}

#[tokio::test]
async fn request_diagnostics_remain_visible_with_request_statuses() {
    let kinds = Arc::new(Mutex::new(Vec::new()));
    let server = recording_server(Server::new(schema()), Arc::clone(&kinds));
    let cases = [
        ("subscription {", None, None, StatusCode::BAD_REQUEST),
        (
            "subscription { missing }",
            None,
            None,
            StatusCode::UNPROCESSABLE_ENTITY,
        ),
        (
            "subscription A { secretSource(limit: 1) } subscription B { secretSource(limit: 1) }",
            None,
            None,
            StatusCode::UNPROCESSABLE_ENTITY,
        ),
        (
            SUBSCRIPTION,
            Some("Missing"),
            None,
            StatusCode::UNPROCESSABLE_ENTITY,
        ),
        (
            "subscription($limit: Int!) { secretSource(limit: $limit) }",
            None,
            None,
            StatusCode::UNPROCESSABLE_ENTITY,
        ),
        (
            "subscription { secretSource }",
            None,
            None,
            StatusCode::UNPROCESSABLE_ENTITY,
        ),
    ];

    for (query, operation_name, variables, expected_status) in cases {
        let before = kinds.lock().expect("lock").len();
        let mut request = GraphQLRequest::new(query);
        if let Some(operation_name) = operation_name {
            request = request.operation_name(operation_name);
        }
        if let Some(variables) = variables {
            request = request.variables(variables);
        }
        let response = post(&server, &request).await;
        assert_eq!(response.status, expected_status, "query: {query}");
        let json: serde_json::Value = serde_json::from_slice(&response.body).expect("JSON body");
        let message = json["errors"][0]["message"].as_str().unwrap_or_default();
        assert!(!message.is_empty(), "query: {query}");
        assert_ne!(message, suku::INTERNAL_ERROR_MESSAGE, "query: {query}");
        assert!(
            kinds.lock().expect("lock")[before..]
                .iter()
                .all(|kind| *kind == ErrorKind::Request),
            "query: {query}"
        );
    }
}
