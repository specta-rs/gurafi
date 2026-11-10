//! Core `Server` execution tests.

use futures::StreamExt as _;
use gql::{Field, InputValue, ObjectType, Schema, TypeRef, Value};
use suku::{Context, GraphQLRequest, Server};

#[derive(Clone, Debug, PartialEq)]
struct User {
    name: String,
}

fn schema() -> Schema {
    let hello = Field::new("hello", TypeRef::string().non_null())
        .resolve(|_| Box::pin(async { Ok(Value::from("world")) }));

    let echo = Field::new("echo", TypeRef::string().non_null())
        .arg(InputValue::new("message", TypeRef::string().non_null()))
        .resolve(|context| {
            let message = context
                .args
                .get("message")
                .and_then(Value::as_str)
                .unwrap_or_default()
                .to_owned();
            Box::pin(async move { Ok(Value::from(message)) })
        });

    let echo_int = Field::new("echoInt", TypeRef::int().non_null())
        .arg(InputValue::new("value", TypeRef::int().non_null()))
        .resolve(|context| {
            let value = context.args.get("value").cloned().unwrap_or(Value::Null);
            Box::pin(async move { Ok(value) })
        });

    let whoami = Field::new("whoami", TypeRef::string().non_null()).resolve(|context| {
        let name = context
            .context
            .require_data::<User>()
            .map(|user| user.name.clone());
        Box::pin(async move { name.map(Value::from) })
    });

    let query = ObjectType::new("Query")
        .field(hello)
        .field(echo)
        .field(echo_int)
        .field(whoami);

    let countdown = Field::new("countdown", TypeRef::int().non_null())
        .subscribe(|_| {
            Box::pin(async {
                Ok(futures::stream::iter(vec![
                    Ok(Value::from(3)),
                    Ok(Value::from(2)),
                    Ok(Value::from(1)),
                ])
                .boxed())
            })
        })
        .resolve(|context| Box::pin(async move { Ok(context.parent.clone()) }));

    let subscription = ObjectType::new("Subscription").field(countdown);

    let mut builder = Schema::builder();
    builder.add_type(query.into());
    builder.add_type(subscription.into());
    builder = builder.subscription("Subscription");
    builder.build().expect("valid schema")
}

fn server() -> Server {
    Server::new(schema())
}

fn data(response: &gql::Response) -> &gql::Object {
    response
        .data
        .as_ref()
        .and_then(Value::as_object)
        .expect("data")
}

#[tokio::test]
async fn executes_a_query() {
    let response = server()
        .execute(&GraphQLRequest::new("{ hello }"), Context::empty())
        .await;

    assert!(!response.has_errors(), "{:?}", response.errors);
    assert_eq!(
        data(&response).get("hello").and_then(Value::as_str),
        Some("world")
    );
}

#[tokio::test]
async fn executes_with_variables() {
    let request = GraphQLRequest::new("query Echo($message: String!) { echo(message: $message) }")
        .variables(
            serde_json::json!({ "message": "hello variables" })
                .as_object()
                .cloned()
                .expect("object"),
        );

    let response = server().execute(&request, Context::empty()).await;
    assert!(!response.has_errors(), "{:?}", response.errors);
    assert_eq!(
        data(&response).get("echo").and_then(Value::as_str),
        Some("hello variables")
    );
}

#[tokio::test]
async fn passes_the_context_to_resolvers() {
    let response = server()
        .execute(
            &GraphQLRequest::new("{ whoami }"),
            Context::new(User {
                name: "Ada".to_owned(),
            }),
        )
        .await;

    assert!(!response.has_errors(), "{:?}", response.errors);
    assert_eq!(
        data(&response).get("whoami").and_then(Value::as_str),
        Some("Ada")
    );
}

#[tokio::test]
async fn out_of_range_integer_variables_fail_graphql_coercion_not_the_request() {
    let request = GraphQLRequest::new("query EchoInt($value: Int!) { echoInt(value: $value) }")
        .variables(
            serde_json::json!({ "value": 2_147_483_648_i64 })
                .as_object()
                .cloned()
                .expect("object"),
        );

    // The variable survives JSON conversion as a `BigInt`; the built-in `Int` scalar is what
    // rejects it, at GraphQL coercion time.
    let response = server().execute(&request, Context::empty()).await;
    assert!(response.data.is_none());
    assert!(
        response
            .errors
            .iter()
            .any(|error| error.message().contains("32-bit")),
        "{:?}",
        response.errors
    );
}

#[tokio::test]
async fn reports_parse_errors_in_the_response() {
    let response = server()
        .execute(&GraphQLRequest::new("{"), Context::empty())
        .await;
    assert!(response.data.is_none());
    assert!(!response.errors.is_empty());
}

#[tokio::test]
async fn disabled_introspection_rejects_schema_queries() {
    let response = server()
        .introspection(false)
        .execute(
            &GraphQLRequest::new("{ __schema { queryType { name } } }"),
            Context::empty(),
        )
        .await;

    assert!(!response.errors.is_empty());
}

#[tokio::test]
async fn executes_a_batch_in_order() {
    let requests = vec![
        GraphQLRequest::new("{ hello }"),
        GraphQLRequest::new("{ echo(message: \"second\") }"),
    ];

    let responses = server().execute_batch(&requests, Context::empty()).await;
    assert_eq!(responses.len(), 2);
    assert_eq!(
        data(&responses[0]).get("hello").and_then(Value::as_str),
        Some("world")
    );
    assert_eq!(
        data(&responses[1]).get("echo").and_then(Value::as_str),
        Some("second")
    );
}

#[tokio::test]
async fn subscribes_and_yields_one_response_per_event() {
    let subscription = server()
        .subscribe(
            &GraphQLRequest::new("subscription { countdown }"),
            Context::empty(),
        )
        .await
        .expect("subscription");

    let values: Vec<_> = subscription
        .stream
        .map(|response| {
            data(&response)
                .get("countdown")
                .and_then(Value::as_i32)
                .expect("countdown")
        })
        .collect()
        .await;

    assert_eq!(values, vec![3, 2, 1]);
}

#[tokio::test]
async fn schema_is_accessible() {
    let server = server();
    assert!(server.schema().get_type("Query").is_some());
}

struct DenyHello;

impl gql::Authorizer for DenyHello {
    fn authorize(&self, context: &gql::AuthorizationContext<'_>) -> Result<(), gql::GraphQLError> {
        if context.field_name == "hello" {
            Err(gql::GraphQLError::new("denied by the test authorizer"))
        } else {
            Ok(())
        }
    }
}

#[tokio::test]
async fn a_field_authorizer_is_installed_on_the_execution_options() {
    let response = server()
        .authorizer(DenyHello)
        .execute(&GraphQLRequest::new("{ hello }"), Context::empty())
        .await;

    assert!(response.has_errors());
    assert_eq!(
        response.errors[0].message(),
        "denied by the test authorizer"
    );
}

#[cfg(feature = "axum")]
#[tokio::test]
async fn enforces_a_wall_clock_timeout_around_resolvers() {
    let slow = Field::new("slow", TypeRef::string().non_null()).resolve(|_| {
        Box::pin(async {
            tokio::time::sleep(std::time::Duration::from_millis(200)).await;
            Ok(Value::from("late"))
        })
    });
    let query = ObjectType::new("Query").field(slow);
    let mut builder = Schema::builder();
    builder.add_type(query.into());
    let schema = builder.query("Query").build().expect("valid schema");

    let server = Server::new(schema).timeout(std::time::Duration::from_millis(10));
    let response = server
        .execute(&GraphQLRequest::new("{ slow }"), Context::empty())
        .await;
    assert!(response.has_errors());
    assert!(
        response
            .errors
            .iter()
            .any(|error| error.message().contains("timed out")),
        "{:?}",
        response.errors
    );
}
