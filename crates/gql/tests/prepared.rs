//! Prepared query behaviour, exercised through the public `gql` API.

use std::{sync::Arc, thread};

use futures::StreamExt;
use futures::executor::block_on;
use gql::{
    AuthorizationContext, Authorizer, ExecutionOptions, Field, InputValue, Object, ObjectType,
    Request, Response, Schema, TypeRef, Value,
    prepared::{
        PreparedQuery, PreparedQueryError, PreparedQueryRegistry, PreparedQueryRegistryError,
    },
};

fn value(fields: impl IntoIterator<Item = (&'static str, Value)>) -> Value {
    Value::object(Object::from_fields(fields))
}

fn field<'a>(response: &'a Response, key: &str) -> &'a Value {
    response
        .data
        .as_ref()
        .and_then(Value::as_object)
        .and_then(|object| object.get(key))
        .unwrap_or_else(|| panic!("response has no field {key}: {response:?}"))
}

fn schema() -> Schema {
    let user = ObjectType::new("User")
        .field(Field::new("id", TypeRef::id().non_null()))
        .field(Field::new("name", TypeRef::string().non_null()))
        .field(Field::new("age", TypeRef::int()))
        .field(Field::new("friend", TypeRef::named("User")))
        .field(Field::new(
            "tags",
            TypeRef::string().non_null().list().non_null(),
        ));

    let query = ObjectType::new("Query")
        .field(
            Field::new("hello", TypeRef::string().non_null())
                .resolve(|_| Box::pin(async { Ok(Value::from("world")) })),
        )
        .field(
            Field::new("user", TypeRef::named("User"))
                .arg(InputValue::new("id", TypeRef::id().non_null())),
        )
        .field(Field::new("numbers", TypeRef::int().list().non_null()))
        .field(Field::new("required", TypeRef::string().non_null()));

    let mutation = ObjectType::new("Mutation").field(
        Field::new("setName", TypeRef::string().non_null())
            .arg(InputValue::new("name", TypeRef::string().non_null()))
            .resolve(|context| {
                let name = context.args.get("name").cloned().unwrap_or(Value::Null);
                Box::pin(async move { Ok(name) })
            }),
    );

    let subscription = ObjectType::new("Subscription").field(
        Field::new("countdown", TypeRef::int().non_null())
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
            .resolve(|context| Box::pin(async move { Ok(context.parent.clone()) })),
    );

    let mut builder = Schema::builder();
    builder.add_type(user.into());
    builder.add_type(query.into());
    builder.add_type(mutation.into());
    builder.add_type(subscription.into());
    builder = builder
        .query("Query")
        .mutation("Mutation")
        .subscription("Subscription");
    builder.build().expect("valid schema")
}

fn root_value() -> Value {
    let friend = value([
        ("id", Value::from("2")),
        ("name", Value::from("Bob")),
        ("age", Value::Int(31)),
    ]);
    value([
        (
            "user",
            value([
                ("id", Value::from("1")),
                ("name", Value::from("Alice")),
                ("age", Value::Int(30)),
                ("friend", friend),
                (
                    "tags",
                    Value::List(vec![Value::from("a"), Value::from("b")]),
                ),
            ]),
        ),
        (
            "numbers",
            Value::List(vec![Value::Int(1), Value::Int(2), Value::Int(3)]),
        ),
        ("required", Value::Null),
    ])
}

fn options() -> ExecutionOptions {
    ExecutionOptions::default().root_value(root_value())
}

fn run(schema: &Schema, source: &str, variables: Option<&Object>) -> Response {
    block_on(gql::execute(
        schema,
        Request {
            query: source,
            operation_name: None,
            variables,
        },
        &options(),
    ))
}

#[test]
fn prepared_matches_normal_execution_across_language_features() {
    let schema = schema();
    let source = r#"
        query Person($id: ID!, $includeAge: Boolean = true, $skipTags: Boolean = false) {
          hello
          user(id: $id) {
            ...PersonFields
            friend {
              name
              age @include(if: $includeAge)
            }
            tags @skip(if: $skipTags)
          }
          numbers
          renamed: numbers
        }

        fragment PersonFields on User {
          id
          name
          age @include(if: $includeAge)
        }
    "#;
    let variables = Object::from_fields([
        ("id", Value::from("1")),
        ("includeAge", Value::Boolean(true)),
        ("skipTags", Value::Boolean(false)),
    ]);

    let normal = run(&schema, source, Some(&variables));
    let prepared = block_on(
        PreparedQuery::new(&schema, "person", source, None)
            .expect("prepared")
            .execute(Some(&variables), &options()),
    );

    assert!(normal.errors.is_empty(), "{:?}", normal.errors);
    assert_eq!(normal, prepared);
}

#[test]
fn prepared_handles_variable_defaults_and_aliases() {
    let schema = schema();
    let prepared = PreparedQuery::new(
        &schema,
        "defaults",
        "query($includeAge: Boolean = true) { user(id: \"1\") { name age @include(if: $includeAge) } }",
        None,
    )
    .expect("prepared");
    let response = block_on(prepared.execute(None, &options()));
    assert!(response.errors.is_empty(), "{:?}", response.errors);
    let user = field(&response, "user");
    assert_eq!(user.as_object().unwrap().get("age"), Some(&Value::Int(30)));
}

#[test]
fn prepared_applies_null_propagation() {
    let schema = schema();
    let prepared = PreparedQuery::new(&schema, "nulls", "{ required }", None).expect("prepared");
    let response = block_on(prepared.execute(None, &options()));
    // `required` is non-null and the root value is null, so the whole data entry is null.
    assert_eq!(response.data, Some(Value::Null));
    assert_eq!(response.errors.len(), 1);
}

#[test]
fn prepared_selects_named_operation() {
    let schema = schema();
    let source = "query A { hello } query B { numbers }";
    let prepared = PreparedQuery::new(&schema, "b", source, Some("B")).expect("prepared");
    assert_eq!(prepared.operation_name(), Some("B"));
    assert_eq!(prepared.kind(), gql::ast::OperationKind::Query);
    let response = block_on(prepared.execute(None, &options()));
    assert!(response.errors.is_empty(), "{:?}", response.errors);
    assert!(
        response
            .data
            .unwrap()
            .as_object()
            .unwrap()
            .contains_key("numbers")
    );

    let error = PreparedQuery::new(&schema, "x", source, Some("Missing")).expect_err("unknown");
    assert!(
        matches!(error, PreparedQueryError::Operation(error) if error.message().contains("Unknown operation"))
    );

    let error = PreparedQuery::new(&schema, "x", source, None).expect_err("ambiguous");
    assert!(
        matches!(error, PreparedQueryError::Operation(error) if error.message().contains("operation name"))
    );
}

#[test]
fn prepared_executes_mutations() {
    let schema = schema();
    let prepared = PreparedQuery::new(
        &schema,
        "set-name",
        "mutation SetName { setName(name: \"Ada\") }",
        Some("SetName"),
    )
    .expect("prepared");
    assert_eq!(prepared.kind(), gql::ast::OperationKind::Mutation);
    let response = block_on(prepared.execute(None, &options()));
    assert!(response.errors.is_empty(), "{:?}", response.errors);
    assert_eq!(field(&response, "setName"), &Value::from("Ada"));
}

#[test]
fn rejects_parse_and_validation_errors() {
    let schema = schema();
    let error = PreparedQuery::new(&schema, "bad", "{ hello", None).expect_err("parse");
    assert!(matches!(error, PreparedQueryError::Parse(_)));
    assert!(!error.errors().is_empty());

    let error = PreparedQuery::new(&schema, "bad", "{ missing }", None).expect_err("validation");
    assert!(matches!(error, PreparedQueryError::Validation(_)));
    assert!(!error.errors().is_empty());
}

#[test]
fn owns_its_source() {
    let schema = schema();
    let source = String::from("{ hello }");
    let prepared = PreparedQuery::new(&schema, "owned", &source, None).expect("prepared");
    drop(source);
    assert_eq!(prepared.source(), "{ hello }");
    let response = block_on(prepared.execute(None, &options()));
    assert_eq!(field(&response, "hello"), &Value::from("world"));
}

#[test]
fn registry_validates_ids_and_executes() {
    let schema = schema();
    let registry = PreparedQueryRegistry::new(
        &schema,
        [
            ("hello", "{ hello }", None),
            ("numbers", "query N { numbers }", Some("N")),
        ],
    )
    .expect("registry");
    assert_eq!(registry.len(), 2);
    assert_eq!(registry.ids().collect::<Vec<_>>(), ["hello", "numbers"]);
    assert!(registry.get("hello").is_some());
    assert!(registry.get("missing").is_none());

    let response = block_on(registry.execute("hello", None, &options())).expect("known id");
    assert_eq!(field(&response, "hello"), &Value::from("world"));
    assert!(block_on(registry.execute("missing", None, &options())).is_none());

    let error = PreparedQueryRegistry::new(
        &schema,
        [("hello", "{ hello }", None), ("hello", "{ numbers }", None)],
    )
    .expect_err("duplicate");
    assert!(matches!(
        error,
        PreparedQueryRegistryError::DuplicateId { .. }
    ));

    let error =
        PreparedQueryRegistry::new(&schema, [("", "{ hello }", None)]).expect_err("empty id");
    assert!(matches!(error, PreparedQueryRegistryError::EmptyId));

    let error = PreparedQueryRegistry::new(&schema, [("bad", "{ missing }", None)])
        .expect_err("invalid query");
    assert!(matches!(error, PreparedQueryRegistryError::Query { .. }));
}

#[test]
fn registry_insert_and_free_function_execution() {
    let schema = schema();
    let query = PreparedQuery::new(&schema, "hello", "{ hello }", None).expect("prepared");
    let response = block_on(gql::execute_prepared(&query, None, &options()));
    assert_eq!(field(&response, "hello"), &Value::from("world"));

    let mut registry = PreparedQueryRegistry::empty();
    registry.insert(query).expect("insert");
    assert!(matches!(
        registry.insert(PreparedQuery::new(&schema, "hello", "{ hello }", None).expect("prepared")),
        Err(PreparedQueryRegistryError::DuplicateId { .. })
    ));
    assert!(matches!(
        registry.insert(PreparedQuery::new(&schema, "", "{ hello }", None).expect("prepared")),
        Err(PreparedQueryRegistryError::EmptyId)
    ));
}

#[test]
fn prepared_query_is_send_and_sync() {
    fn assert_send_sync<T: Send + Sync>() {}
    assert_send_sync::<PreparedQuery>();
    assert_send_sync::<PreparedQueryRegistry>();

    let schema = schema();
    let prepared =
        Arc::new(PreparedQuery::new(&schema, "shared", "{ hello }", None).expect("prepared"));
    let handles: Vec<_> = (0..4)
        .map(|_| {
            let prepared = Arc::clone(&prepared);
            thread::spawn(move || {
                let options = ExecutionOptions::default();
                let response = block_on(prepared.execute(None, &options));
                assert_eq!(field(&response, "hello"), &Value::from("world"));
            })
        })
        .collect();
    for handle in handles {
        handle.join().expect("thread");
    }
}

#[test]
fn prepared_still_applies_authorization() {
    struct Deny;
    impl Authorizer for Deny {
        fn authorize(&self, _: &AuthorizationContext<'_>) -> Result<(), gql::GraphQLError> {
            Err(gql::GraphQLError::new("denied"))
        }
    }

    let schema = schema();
    let prepared = PreparedQuery::new(&schema, "hello", "{ hello }", None).expect("prepared");
    let response = block_on(prepared.execute(None, &ExecutionOptions::default().authorizer(Deny)));
    assert_eq!(response.data, Some(Value::Null));
    assert!(
        response
            .errors
            .iter()
            .any(|error| error.message().contains("denied"))
    );
}

#[test]
fn prepared_subscription_reuses_document_for_every_event() {
    let schema = schema();
    let prepared = PreparedQuery::new(&schema, "countdown", "subscription { countdown }", None)
        .expect("prepared");
    assert_eq!(prepared.kind(), gql::ast::OperationKind::Subscription);

    let response = block_on(async {
        let subscription = prepared
            .subscribe(None, &options())
            .await
            .expect("subscription");
        subscription.stream.collect::<Vec<_>>().await
    });
    let values: Vec<_> = response
        .iter()
        .map(|response| field(response, "countdown").clone())
        .collect();
    assert_eq!(values, [Value::from(3), Value::from(2), Value::from(1)]);
}

#[test]
fn prepared_execute_rejects_subscriptions_and_vice_versa() {
    let schema = schema();
    let prepared = PreparedQuery::new(&schema, "countdown", "subscription { countdown }", None)
        .expect("prepared");
    let response = block_on(prepared.execute(None, &options()));
    assert!(
        response
            .errors
            .iter()
            .any(|error| error.message().contains("subscribe"))
    );

    let query = PreparedQuery::new(&schema, "hello", "{ hello }", None).expect("prepared");
    let error = block_on(query.subscribe(None, &options())).expect_err("not a subscription");
    assert!(
        error
            .errors
            .iter()
            .any(|error| error.message().contains("not a subscription"))
    );
}

#[test]
fn prepared_respects_introspection_option() {
    let schema = schema();
    let source = "{ __schema { queryType { name } } }";
    let prepared = PreparedQuery::new(&schema, "intro", source, None).expect("prepared");

    let enabled = block_on(prepared.execute(None, &ExecutionOptions::default()));
    assert!(enabled.errors.is_empty(), "{:?}", enabled.errors);

    let disabled =
        block_on(prepared.execute(None, &ExecutionOptions::default().introspection(false)));
    assert_eq!(disabled.data, None);
    assert_eq!(disabled.errors.len(), 1);
    assert!(disabled.errors[0].message().contains("__schema"));
}

#[test]
fn prepared_reports_operation_root_errors() {
    let schema = Schema::builder()
        .object(ObjectType::new("Query").field(Field::new("hello", TypeRef::string())))
        .query("Query")
        .build()
        .expect("schema");

    let error =
        PreparedQuery::new(&schema, "mutation", "mutation { hello }", None).expect_err("no root");
    assert!(matches!(error, PreparedQueryError::Operation(_)));
    assert!(error.to_string().contains("mutation"));

    // A parse error is not a GraphQL response; the diagnostic carries the message.
    let error = PreparedQuery::new(&schema, "parse", "{", None).expect_err("parse");
    assert!(matches!(error, PreparedQueryError::Parse(_)));
    assert!(!matches!(error, PreparedQueryError::Validation(errors) if errors.is_empty()));
}
