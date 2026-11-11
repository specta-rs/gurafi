//! Regression tests for prepared-query security and frontend review findings.

#[path = "support/stack.rs"]
mod stack;

use std::time::Duration;

use futures::executor::block_on;
use gql::{
    ExecutionOptions, Field, InputValue, Object, ObjectType, Request, Schema, TypeRef, Value,
    prepared::PreparedQuery,
};

use crate::stack::stack_size;

fn run(
    schema: &Schema,
    source: &str,
    variables: Option<&Object>,
    options: &ExecutionOptions,
) -> gql::Response {
    block_on(gql::execute(
        schema,
        Request {
            query: source,
            operation_name: None,
            variables,
        },
        options,
    ))
}

fn leaf_estimator_schema() -> Schema {
    let query = ObjectType::new("Query")
        .field(
            Field::new("expensive", TypeRef::string())
                .arg(InputValue::new("n", TypeRef::int().non_null()))
                .complexity(|args| {
                    // A leaf field whose cost depends on the request argument.
                    1 + args.get("n").and_then(Value::as_i32).unwrap_or(0).max(0) as u32
                }),
        )
        .field(Field::new("plain", TypeRef::string()));

    Schema::builder()
        .object(query)
        .query("Query")
        .build()
        .expect("schema")
}

#[test]
fn prepared_bypasses_leaf_complexity_estimator() {
    let schema = leaf_estimator_schema();
    let source = "query Q($n: Int!) { expensive(n: $n) }";
    let variables = Object::from_fields([("n", Value::Int(1_000))]);
    let options = ExecutionOptions::default()
        .root_value(Value::object(Object::from_fields([
            ("expensive", Value::from("x")),
            ("plain", Value::from("p")),
        ])))
        .max_complexity(10);

    let normal = run(&schema, source, Some(&variables), &options);
    let prepared = PreparedQuery::new(&schema, "leaf", source, None).expect("prepared");
    let after = block_on(prepared.execute(Some(&variables), &options));

    assert!(
        normal
            .errors
            .iter()
            .any(|error| error.message().contains("complexity")),
        "normal path should reject: {normal:?}"
    );
    assert!(
        after
            .errors
            .iter()
            .any(|error| error.message().contains("complexity")),
        "prepared path bypassed the estimator: {after:?}"
    );
}

fn identity_schema() -> Schema {
    let query = ObjectType::new("Query")
        .field(Field::new("hello", TypeRef::string().non_null()))
        .field(Field::new("count", TypeRef::int().non_null()));
    Schema::builder()
        .object(query)
        .query("Query")
        .build()
        .expect("schema")
}

#[test]
fn prepared_identity_reuse_skips_deadline() {
    let schema = identity_schema();
    let source = "{ hello count }";
    let root = Value::object(Object::from_fields([
        ("hello", Value::from("world")),
        ("count", Value::Int(3)),
    ]));
    let mut options = ExecutionOptions::default().root_value(root);
    options.timeout = Some(Duration::ZERO);

    let normal = run(&schema, source, None, &options);
    let prepared = PreparedQuery::new(&schema, "identity", source, None).expect("prepared");
    let after = block_on(prepared.execute(None, &options));

    assert!(
        !normal.errors.is_empty(),
        "normal path should time out: {normal:?}"
    );
    assert!(
        !after.errors.is_empty(),
        "prepared path skipped the expired deadline: {after:?}"
    );
}

fn interface_estimator_schema() -> Schema {
    let node = gql::InterfaceType::new("Node")
        .field(Field::new("id", TypeRef::id().non_null()))
        .field(Field::new("items", TypeRef::string().list()));
    let user = ObjectType::new("User")
        .implements("Node")
        .field(Field::new("id", TypeRef::id().non_null()))
        .field(Field::new("items", TypeRef::string().list()).complexity(|_| 1_000));
    let query = ObjectType::new("Query").field(Field::new("node", TypeRef::named("Node")));

    let mut builder = Schema::builder();
    builder.add_type(node.into());
    builder.add_type(user.into());
    builder.add_type(query.into());
    builder.query("Query").build().expect("schema")
}

#[test]
fn prepared_bypasses_object_estimator_through_interface_scope() {
    let schema = interface_estimator_schema();
    let source = "{ node { ... on User { ... on Node { items } } } }";
    let root = Value::object(Object::from_fields([(
        "node",
        Value::object(Object::from_fields([
            ("id", Value::from("1")),
            ("__typename", Value::from("User")),
            ("items", Value::List(vec![Value::from("a")])),
        ])),
    )]));
    let options = ExecutionOptions::default()
        .root_value(root)
        .max_complexity(10);

    let normal = run(&schema, source, None, &options);
    let prepared = PreparedQuery::new(&schema, "iface", source, None).expect("prepared");
    let after = block_on(prepared.execute(None, &options));

    assert!(
        normal
            .errors
            .iter()
            .any(|error| error.message().contains("complexity")),
        "normal path should reject: {normal:?}"
    );
    assert!(
        after
            .errors
            .iter()
            .any(|error| error.message().contains("complexity")),
        "prepared path bypassed the object estimator: {after:?}"
    );
}

fn recursive_schema() -> Schema {
    // `Query.a` returns `Query`, so a document can nest to the parser's limit.
    let query = ObjectType::new("Query")
        .field(
            Field::new("ping", TypeRef::string())
                .resolve(|_| Box::pin(async { Ok(Value::from("pong")) })),
        )
        .field(
            Field::new("a", TypeRef::named("Query"))
                .resolve(|_| Box::pin(async { Ok(Value::object(Object::new())) })),
        );
    Schema::builder()
        .object(query)
        .query("Query")
        .build()
        .expect("schema")
}

fn deep_source_and_root(depth: usize) -> (String, Value) {
    let mut source = String::new();
    for _ in 0..depth {
        source.push_str("{ a ");
    }
    source.push_str("{ ping }");
    for _ in 0..depth {
        source.push_str(" }");
    }
    (source, Value::object(Object::new()))
}

#[test]
fn prepared_prepare_only_within_the_stack_budget() {
    let depth = gql::parser::MAX_DEPTH - 2;
    let (source, _) = deep_source_and_root(depth);
    std::thread::Builder::new()
        .stack_size(stack_size())
        .spawn(move || {
            let schema = recursive_schema();
            let prepared = PreparedQuery::new(&schema, "deep", &source, None).expect("prepared");
            std::mem::forget(prepared);
        })
        .expect("spawn")
        .join()
        .expect("join");
}

#[test]
fn prepared_execute_within_the_stack_budget() {
    let depth = gql::parser::MAX_DEPTH - 2;
    let (source, root) = deep_source_and_root(depth);
    let schema = recursive_schema();
    let prepared = PreparedQuery::new(&schema, "deep", &source, None).expect("prepared");
    std::thread::Builder::new()
        .stack_size(stack_size())
        .spawn(move || {
            let options = ExecutionOptions::default().root_value(root);
            let response = block_on(prepared.execute(None, &options));
            eprintln!("executed: errors={}", response.errors.len());
            std::mem::forget(prepared);
            assert!(response.errors.is_empty(), "{:?}", response.errors);
        })
        .expect("spawn")
        .join()
        .expect("join");
}

#[test]
fn normal_execute_within_the_stack_budget() {
    let depth = gql::parser::MAX_DEPTH - 2;
    let (source, root) = deep_source_and_root(depth);
    let schema = recursive_schema();
    std::thread::Builder::new()
        .stack_size(stack_size())
        .spawn(move || {
            let options = ExecutionOptions::default().root_value(root);
            let response = block_on(gql::execute(
                &schema,
                Request {
                    query: &source,
                    operation_name: None,
                    variables: None,
                },
                &options,
            ));
            assert!(response.errors.is_empty(), "{:?}", response.errors);
        })
        .expect("spawn")
        .join()
        .expect("join");
}

#[test]
fn prepared_repeated_and_concurrent_execution_is_stable() {
    use std::sync::Arc;
    use std::thread;

    let schema = recursive_schema();
    let prepared = Arc::new(
        PreparedQuery::new(
            &schema,
            "p",
            "query Q($v: Boolean!) { ping @include(if: $v) a { ping } }",
            None,
        )
        .expect("prepared"),
    );
    let handles: Vec<_> = (0..8)
        .map(|_| {
            let prepared = Arc::clone(&prepared);
            thread::spawn(move || {
                for _ in 0..100 {
                    let variables = Object::from_fields([("v", Value::Boolean(true))]);
                    let response =
                        block_on(prepared.execute(Some(&variables), &ExecutionOptions::default()));
                    assert!(response.errors.is_empty(), "{:?}", response.errors);
                }
            })
        })
        .collect();
    for handle in handles {
        handle.join().expect("thread");
    }
}

fn authorizer_schema() -> Schema {
    let user = ObjectType::new("User")
        .field(Field::new("id", TypeRef::id().non_null()))
        .field(Field::new("name", TypeRef::string()))
        .field(Field::new("secret", TypeRef::string()))
        .field(Field::new(
            "friends",
            TypeRef::named("User").non_null().list().non_null(),
        ));
    let query = ObjectType::new("Query").field(
        Field::new("viewer", TypeRef::named("User")).resolve(|_| {
            Box::pin(async {
                Ok(Value::object(Object::from_fields([
                    ("id", Value::from("1")),
                    ("name", Value::from("Alice")),
                    ("secret", Value::from("s")),
                    (
                        "friends",
                        Value::List(vec![Value::object(Object::from_fields([
                            ("id", Value::from("2")),
                            ("name", Value::from("Bob")),
                            ("secret", Value::from("t")),
                            ("friends", Value::List(Vec::new())),
                        ]))]),
                    ),
                ])))
            })
        }),
    );
    let mut builder = Schema::builder();
    builder.add_type(user.into());
    builder.add_type(query.into());
    builder.query("Query").build().expect("schema")
}

#[derive(Default)]
struct Recorder {
    calls: std::sync::Mutex<Vec<String>>,
}

impl gql::Authorizer for Recorder {
    fn authorize(&self, context: &gql::AuthorizationContext<'_>) -> Result<(), gql::GraphQLError> {
        self.calls.lock().unwrap().push(format!(
            "{}:{}:{}:{:?}",
            context.parent_type,
            context.field_name,
            context
                .path
                .iter()
                .map(|segment| format!("{segment:?}"))
                .collect::<Vec<_>>()
                .join("/"),
            context.args.len()
        ));
        Ok(())
    }
}

#[test]
fn prepared_authorization_trace_matches_normal() {
    use std::sync::Arc;

    let schema = authorizer_schema();
    let source = r#"
        query {
          viewer {
            ...F
            alias: name
            friends { id name secret }
          }
        }
        fragment F on User { id name secret }
    "#;

    let normal_recorder = Arc::new(Recorder::default());
    let mut normal_options = ExecutionOptions::default().max_concurrency(1);
    normal_options.authorizer = Some(normal_recorder.clone());
    let normal = run(&schema, source, None, &normal_options);

    let prepared_recorder = Arc::new(Recorder::default());
    let prepared = PreparedQuery::new(&schema, "auth", source, None).expect("prepared");
    let mut prepared_options = ExecutionOptions::default().max_concurrency(1);
    prepared_options.authorizer = Some(prepared_recorder.clone());
    let after = block_on(prepared.execute(None, &prepared_options));

    let mut normal_calls = normal_recorder.calls.lock().unwrap().clone();
    let mut prepared_calls = prepared_recorder.calls.lock().unwrap().clone();
    normal_calls.sort();
    prepared_calls.sort();
    assert_eq!(normal_calls, prepared_calls);
    assert_eq!(normal.data, after.data);
    assert_eq!(normal.errors, after.errors);
}

#[test]
fn prepared_introspection_error_count_matches_normal() {
    let schema = leaf_estimator_schema();
    let source = r#"{ __schema { queryType { name } } __type(name: "Query") { name } }"#;
    let options = ExecutionOptions::default().introspection(false);
    let normal = run(&schema, source, None, &options);
    let prepared = PreparedQuery::new(&schema, "intro", source, None).expect("prepared");
    let after = block_on(prepared.execute(None, &options));
    assert_eq!(normal.errors, after.errors);
}

#[test]
fn prepared_introspection_errors_respect_max_errors() {
    let schema = leaf_estimator_schema();
    let source = r#"{ __schema { queryType { name } } __type(name: "Query") { name } }"#;
    let options = ExecutionOptions::default()
        .introspection(false)
        .max_errors(1);
    let normal = run(&schema, source, None, &options);
    let prepared = PreparedQuery::new(&schema, "intro-limit", source, None).expect("prepared");
    let after = block_on(prepared.execute(None, &options));

    assert_eq!(normal.errors.len(), 1, "{normal:?}");
    assert_eq!(normal.errors, after.errors);
}

#[test]
fn schema_instance_identity_is_observable() {
    let first = identity_schema();
    let clone = first.clone();
    let second = identity_schema();

    assert!(
        first.ptr_eq(&clone),
        "a clone must share the schema instance"
    );
    assert!(
        !first.ptr_eq(&second),
        "separately built schemas must not share an instance"
    );

    let prepared = PreparedQuery::new(&first, "identity", "{ hello }", None).expect("prepared");
    assert!(prepared.schema().ptr_eq(&first));
    assert!(!prepared.schema().ptr_eq(&second));
}
