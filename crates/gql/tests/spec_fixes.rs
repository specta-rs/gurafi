//! Focused regressions for the specification audit (H1/H2/M1/M4/M5/M7/L3/L4/L5/L9 and the
//! breadth / async-authorizer additions).

use std::sync::{
    Arc, Mutex,
    atomic::{AtomicUsize, Ordering},
};

use futures::FutureExt;
use futures::StreamExt;
use futures::executor::block_on;
use futures::future::BoxFuture;
use gql::{
    AuthorizationContext, Authorizer, EnumType, ExecutionOptions, Field, GraphQLError, InputValue,
    Object, ObjectType, Request, Response, ScalarType, Schema, TypeRef, ValidationOptions, Value,
    ast, parser::parse_query, validate,
};

fn value(fields: impl IntoIterator<Item = (&'static str, Value)>) -> Value {
    Value::object(Object::from_fields(fields))
}

fn run_with(schema: &Schema, query: &str, options: &ExecutionOptions) -> Response {
    block_on(gql::execute(schema, Request::new(query), options))
}

fn run(schema: &Schema, query: &str) -> Response {
    run_with(schema, query, &ExecutionOptions::default())
}

fn run_vars(schema: &Schema, query: &str, variables: Object) -> Response {
    block_on(gql::execute(
        schema,
        Request::new(query).variables(&variables),
        &ExecutionOptions::default(),
    ))
}

fn data_field<'a>(response: &'a Response, name: &str) -> &'a Value {
    response
        .data
        .as_ref()
        .and_then(Value::as_object)
        .and_then(|object| object.get(name))
        .unwrap_or_else(|| panic!("response has no field {name}: {response:?}"))
}

fn validation_errors(schema: &Schema, query: &str) -> Vec<GraphQLError> {
    let document = parse_query(query).expect("valid syntax");
    validate(schema, &document, &ValidationOptions::default())
}

// -------------------------------------------------------------------------------------------
// H1 — SameResponseShape applies across mutually exclusive object fragments
// -------------------------------------------------------------------------------------------

#[test]
fn response_shape_conflicts_are_reported_across_disjoint_objects() {
    let schema = Schema::from_sdl(
        r#"
        type Dog { nickname: String }
        type Cat { meowVolume: Int }
        union Pet = Dog | Cat
        type Query { pet: Pet }
        "#,
    )
    .expect("valid schema");

    let errors = validation_errors(
        &schema,
        "{ pet { ... on Dog { x: nickname } ... on Cat { x: meowVolume } } }",
    );
    assert!(
        errors
            .iter()
            .any(|error| error.message().contains("conflicting types")),
        "{errors:?}"
    );

    // `__typename` is a String, so it also conflicts with an Int.
    let errors = validation_errors(
        &schema,
        "{ pet { ... on Dog { x: __typename } ... on Cat { x: meowVolume } } }",
    );
    assert!(
        errors
            .iter()
            .any(|error| error.message().contains("conflicting types")),
        "{errors:?}"
    );

    // Differing field names and arguments remain exempt on disjoint object types.
    let schema = Schema::from_sdl(
        r#"
        type Dog { nickname: String, bark: String }
        type Cat { nickname: String, meow: String }
        union Pet = Dog | Cat
        type Query { pet: Pet }
        "#,
    )
    .expect("valid schema");
    assert!(
        validation_errors(
            &schema,
            "{ pet { ... on Dog { x: nickname } ... on Cat { x: meow } } }"
        )
        .is_empty()
    );
}

// -------------------------------------------------------------------------------------------
// H2 — subscription events are root values
// -------------------------------------------------------------------------------------------

fn subscription_schema() -> Schema {
    Schema::builder()
        .object(
            ObjectType::new("Query")
                .field(Field::new("ping", TypeRef::string()))
                .field(Field::new("never", TypeRef::string())),
        )
        .object(
            ObjectType::new("Subscription").field(
                Field::new("tick", TypeRef::int().non_null())
                    .arg(InputValue::new("step", TypeRef::int()))
                    .subscribe(|_| {
                        Box::pin(async {
                            Ok(futures::stream::iter(vec![
                                Ok::<Value, GraphQLError>(value([("base", Value::Int(10))])),
                                Ok(value([("base", Value::Int(20))])),
                            ])
                            .boxed())
                        })
                    })
                    .resolve(|context| {
                        let base = context
                            .parent
                            .as_object()
                            .and_then(|object| object.get("base"))
                            .and_then(Value::as_i32)
                            .unwrap_or_default();
                        let step = context
                            .args
                            .get("step")
                            .and_then(Value::as_i32)
                            .unwrap_or_default();
                        Box::pin(async move { Ok(Value::Int(base + step)) })
                    }),
            ),
        )
        .query("Query")
        .subscription("Subscription")
        .build()
        .expect("valid schema")
}

#[test]
fn subscription_events_reach_resolvers_with_arguments_and_aliases() {
    let schema = subscription_schema();
    let variables = Object::from_fields([("step", Value::Int(5))]);
    let subscription = block_on(gql::subscribe(
        &schema,
        Request::new("subscription S($step: Int) { total: tick(step: $step) }")
            .operation_name("S")
            .variables(&variables),
        &ExecutionOptions::default(),
    ))
    .expect("subscription starts");

    let responses = block_on(subscription.stream.collect::<Vec<_>>());
    assert_eq!(responses.len(), 2);
    assert!(responses[0].errors.is_empty(), "{:?}", responses[0].errors);
    assert_eq!(responses[0].data, Some(value([("total", Value::Int(15))])));
    assert_eq!(responses[1].data, Some(value([("total", Value::Int(25))])));
}

// -------------------------------------------------------------------------------------------
// M1 — only referenced built-in scalars are part of the schema
// -------------------------------------------------------------------------------------------

#[test]
fn unreferenced_builtin_scalars_are_omitted() {
    let mut builder = Schema::builder();
    builder.add_type(
        ObjectType::new("Query")
            .field(Field::new("hello", TypeRef::string()))
            .into(),
    );
    builder.add_type(ScalarType::new("Unused").into());
    let schema = builder.query("Query").build().expect("valid schema");

    // String and Boolean are reachable through directives/introspection definitions.
    assert!(schema.get_type("String").is_some());
    assert!(schema.get_type("Boolean").is_some());
    assert!(schema.get_type("Int").is_none());
    assert!(schema.get_type("Float").is_none());
    assert!(schema.get_type("ID").is_none());
    // User-registered types are kept even when nothing references them.
    assert!(schema.get_type("Unused").is_some());

    // Introspection must agree: there is no `Float` type to describe.
    let response = run(
        &schema,
        r#"{ float: __type(name: "Float") { name } string: __type(name: "String") { name } }"#,
    );
    assert!(response.errors.is_empty(), "{:?}", response.errors);
    assert_eq!(data_field(&response, "float"), &Value::Null);
    assert_eq!(
        data_field(&response, "string"),
        &value([("name", Value::from("String"))])
    );
}

#[test]
fn referenced_builtin_scalars_are_present() {
    let schema = Schema::builder()
        .object(
            ObjectType::new("Query")
                .field(Field::new("i", TypeRef::int()))
                .field(Field::new("f", TypeRef::float()))
                .field(Field::new("id", TypeRef::id()))
                .field(Field::new("b", TypeRef::boolean())),
        )
        .query("Query")
        .build()
        .expect("valid schema");
    for name in ["String", "Int", "Float", "Boolean", "ID"] {
        assert!(schema.get_type(name).is_some(), "missing {name}");
    }
}

#[test]
fn explicitly_registered_unreferenced_builtin_scalar_is_kept() {
    let schema = Schema::builder()
        .scalar(ScalarType::float())
        .object(ObjectType::new("Query").field(Field::new("hello", TypeRef::string())))
        .query("Query")
        .build()
        .expect("valid schema");
    assert!(schema.get_type("Float").is_some());
}

// -------------------------------------------------------------------------------------------
// M4 — constant @skip/@include on subscription root selections
// -------------------------------------------------------------------------------------------

fn subscription_validation_schema() -> Schema {
    Schema::builder()
        .object(ObjectType::new("Query").field(Field::new("ping", TypeRef::string())))
        .object(
            ObjectType::new("Subscription")
                .field(Field::new("a", TypeRef::string()))
                .field(Field::new("b", TypeRef::string())),
        )
        .query("Query")
        .subscription("Subscription")
        .build()
        .expect("valid schema")
}

#[test]
fn subscription_single_root_field_honours_constant_skip_include() {
    let schema = subscription_validation_schema();

    assert!(
        validation_errors(&schema, "subscription { a b @skip(if: true) }").is_empty(),
        "a skipped field must not count"
    );
    assert!(
        validation_errors(&schema, "subscription { a b @include(if: false) }").is_empty(),
        "an excluded field must not count"
    );

    // A variable could be either value at execution time, so it counts as included.
    let errors = validation_errors(
        &schema,
        "subscription S($v: Boolean!) { a b @skip(if: $v) }",
    );
    assert!(
        errors
            .iter()
            .any(|error| error.message().contains("one top-level field")),
        "{errors:?}"
    );

    // A constant that keeps both fields still fails.
    let errors = validation_errors(&schema, "subscription { a b @skip(if: false) }");
    assert!(
        errors
            .iter()
            .any(|error| error.message().contains("one top-level field")),
        "{errors:?}"
    );
}

// -------------------------------------------------------------------------------------------
// M5 — runtime-null @skip/@include behaves as false
// -------------------------------------------------------------------------------------------

fn directive_schema() -> Schema {
    Schema::builder()
        .object(
            ObjectType::new("Query").field(
                Field::new("text", TypeRef::string())
                    .resolve(|_| Box::pin(async { Ok(Value::from("ok")) })),
            ),
        )
        .query("Query")
        .build()
        .expect("valid schema")
}

#[test]
fn null_skip_include_variable_is_false() {
    let schema = directive_schema();
    let query = "query Q($v: Boolean = true) { a: text @skip(if: $v) b: text @include(if: $v) }";

    // `$v` is an explicitly supplied null. Validation allows it because the variable has a
    // non-null default, and execution must treat the null value as false rather than nulling the
    // response.
    let response = run_vars(&schema, query, Object::from_fields([("v", Value::Null)]));
    assert!(response.errors.is_empty(), "{:?}", response.errors);
    assert_eq!(response.data, Some(value([("a", Value::from("ok"))])));

    // Sanity check the non-null cases.
    let response = run_vars(
        &schema,
        query,
        Object::from_fields([("v", Value::Boolean(true))]),
    );
    assert_eq!(response.data, Some(value([("b", Value::from("ok"))])));
    let response = run_vars(
        &schema,
        query,
        Object::from_fields([("v", Value::Boolean(false))]),
    );
    assert_eq!(response.data, Some(value([("a", Value::from("ok"))])));
}

// -------------------------------------------------------------------------------------------
// M7 / L4 — integer and float literal range
// -------------------------------------------------------------------------------------------

fn big_int_schema() -> Schema {
    Schema::builder()
        .scalar(
            ScalarType::new("BigInt")
                .parse_value(|_| Err("runtime values are strings".to_owned()))
                .parse_literal(|value| match value {
                    ast::Value::Int(value) => Ok(Value::String(Arc::from(value.to_string()))),
                    _ => Err("BigInt expects an integer literal".to_owned()),
                }),
        )
        .object(
            ObjectType::new("Query")
                .field(
                    Field::new("big", TypeRef::string())
                        .arg(InputValue::new("v", TypeRef::named("BigInt")))
                        .resolve(|context| {
                            let value = context
                                .args
                                .get("v")
                                .and_then(Value::as_str)
                                .unwrap_or_default()
                                .to_owned();
                            Box::pin(async move { Ok(Value::String(Arc::from(value))) })
                        }),
                )
                .field(
                    Field::new("int", TypeRef::string())
                        .arg(InputValue::new("v", TypeRef::int()))
                        .resolve(|_| Box::pin(async { Ok(Value::from("int")) })),
                ),
        )
        .query("Query")
        .build()
        .expect("valid schema")
}

#[test]
fn custom_scalars_receive_integers_beyond_i64() {
    let schema = big_int_schema();
    let response = run(
        &schema,
        "{ big(v: 170141183460469231731687303715884105727) }",
    );
    assert!(response.errors.is_empty(), "{:?}", response.errors);
    assert_eq!(
        data_field(&response, "big"),
        &Value::from("170141183460469231731687303715884105727")
    );
}

#[test]
fn integers_beyond_i128_are_reported_without_panicking() {
    let error = parse_query("{ big(v: 999999999999999999999999999999999999999999999999) }")
        .expect_err("too large for the AST");
    assert!(error.message().contains("too large"), "{error:?}");
}

#[test]
fn builtin_int_still_rejects_non_32_bit_literals() {
    let schema = big_int_schema();
    let response = run(&schema, "{ int(v: 2147483648) }");
    assert!(
        response
            .errors
            .iter()
            .any(|error| error.message().contains("32-bit")),
        "{:?}",
        response.errors
    );
}

#[test]
fn float_overflow_is_grammar_valid_but_rejected_by_the_float_scalar() {
    assert!(
        parse_query("{ f(v: 1e999) }").is_ok(),
        "a non-finite float literal is grammatically valid"
    );

    let schema = Schema::builder()
        .object(
            ObjectType::new("Query").field(
                Field::new("f", TypeRef::string())
                    .arg(InputValue::new("v", TypeRef::float()))
                    .resolve(|_| Box::pin(async { Ok(Value::from("float")) })),
            ),
        )
        .query("Query")
        .build()
        .expect("valid schema");
    let response = run(&schema, "{ f(v: 1e999) }");
    assert!(
        response
            .errors
            .iter()
            .any(|error| error.message().contains("non-finite")),
        "{:?}",
        response.errors
    );
}

#[test]
fn custom_scalars_may_accept_non_finite_float_literals() {
    let schema = Schema::builder()
        .scalar(
            ScalarType::new("LooseFloat")
                .parse_value(|_| Err("runtime values are strings".to_owned()))
                .parse_literal(|value| match value {
                    ast::Value::Float(value) if !value.is_finite() => Ok(Value::from("non-finite")),
                    ast::Value::Float(value) => Ok(Value::from(value.to_string())),
                    _ => Err("LooseFloat expects a float literal".to_owned()),
                }),
        )
        .object(
            ObjectType::new("Query").field(
                Field::new("f", TypeRef::string())
                    .arg(InputValue::new("v", TypeRef::named("LooseFloat")))
                    .resolve(|context| {
                        let value = context
                            .args
                            .get("v")
                            .and_then(Value::as_str)
                            .unwrap_or_default()
                            .to_owned();
                        Box::pin(async move { Ok(Value::String(Arc::from(value))) })
                    }),
            ),
        )
        .query("Query")
        .build()
        .expect("valid schema");

    let response = run(&schema, "{ f(v: 1e999) }");
    assert!(response.errors.is_empty(), "{:?}", response.errors);
    assert_eq!(data_field(&response, "f"), &Value::from("non-finite"));
}

// -------------------------------------------------------------------------------------------
// L3 — block strings reject C0 controls
// -------------------------------------------------------------------------------------------

#[test]
fn block_strings_reject_control_characters() {
    assert!(parse_query("query { f(a: \"\"\"tab\tand\nnewline\"\"\") }").is_ok());
    let error = parse_query("query { f(a: \"\"\"bell\u{0007}\"\"\") }")
        .expect_err("C0 control inside block string");
    assert!(
        error.message().contains("Invalid character within String"),
        "{error:?}"
    );
    let error = parse_query("query { f(a: \"\"\"nul\u{0000}\"\"\") }")
        .expect_err("NUL inside block string");
    assert!(
        error.message().contains("Invalid character within String"),
        "{error:?}"
    );
}

// -------------------------------------------------------------------------------------------
// L5 — enum values named true/false/null are rejected for code-first schemas
// -------------------------------------------------------------------------------------------

#[test]
fn enum_values_may_not_be_true_false_or_null() {
    for reserved in ["true", "false", "null"] {
        let error = Schema::builder()
            .enumeration(EnumType::new("Flag").value(reserved).value("OK"))
            .object(ObjectType::new("Query").field(Field::new("flag", TypeRef::named("Flag"))))
            .query("Query")
            .build()
            .expect_err("reserved enum value must be rejected");
        assert!(
            error
                .into_errors()
                .iter()
                .any(|error| error.message().contains(reserved)),
            "missing error for {reserved}"
        );
    }
}

// -------------------------------------------------------------------------------------------
// Breadth limit and async authorizer
// -------------------------------------------------------------------------------------------

#[test]
fn execution_rejects_queries_wider_than_max_breadth() {
    let schema = Schema::builder()
        .object(
            ObjectType::new("Query")
                .field(Field::new("a", TypeRef::string()))
                .field(Field::new("b", TypeRef::string()))
                .field(Field::new("c", TypeRef::string())),
        )
        .query("Query")
        .build()
        .expect("valid schema");

    let response = run_with(
        &schema,
        "{ a b c }",
        &ExecutionOptions::default().max_breadth(2),
    );
    assert_eq!(response.data, None);
    assert!(
        response
            .errors
            .iter()
            .any(|error| error.message().contains("breadth")),
        "{:?}",
        response.errors
    );

    // Duplicate response keys merge, so the same query under two aliases is only breadth two.
    let response = run_with(
        &schema,
        "{ x: a x: a y: b }",
        &ExecutionOptions::default().max_breadth(2),
    );
    assert!(response.errors.is_empty(), "{:?}", response.errors);
}

#[derive(Clone)]
struct AsyncRecordingAuthorizer {
    calls: Arc<AtomicUsize>,
    seen: Arc<Mutex<Vec<String>>>,
}

impl Authorizer for AsyncRecordingAuthorizer {
    // Deliberately does not implement the synchronous method: the default async adapter must
    // still work for a sync-only authorizer, while this one overrides the async path.
    fn authorize_async<'a>(
        &'a self,
        context: &'a AuthorizationContext<'a>,
    ) -> BoxFuture<'a, Result<(), GraphQLError>> {
        async move {
            self.calls.fetch_add(1, Ordering::SeqCst);
            self.seen
                .lock()
                .expect("authorizer log")
                .push(format!("{}.{}", context.parent_type, context.field_name));
            Err(GraphQLError::new("async denied"))
        }
        .boxed()
    }
}

#[test]
fn async_authorizer_denials_are_field_errors() {
    let schema = Schema::builder()
        .object(
            ObjectType::new("Query").field(
                Field::new("hello", TypeRef::string())
                    .resolve(|_| Box::pin(async { Ok(Value::from("world")) })),
            ),
        )
        .query("Query")
        .build()
        .expect("valid schema");

    let authorizer = AsyncRecordingAuthorizer {
        calls: Arc::default(),
        seen: Arc::default(),
    };
    let response = run_with(
        &schema,
        "{ hello }",
        &ExecutionOptions::default().authorizer(authorizer.clone()),
    );
    assert_eq!(response.data, Some(value([("hello", Value::Null)])));
    assert_eq!(response.errors[0].message(), "async denied");
    assert_eq!(authorizer.calls.load(Ordering::SeqCst), 1);
    assert_eq!(
        authorizer.seen.lock().expect("authorizer log").as_slice(),
        ["Query.hello"]
    );
}

#[test]
fn async_authorizer_is_awaited_during_subscription_setup() {
    let schema = subscription_schema();
    let authorizer = AsyncRecordingAuthorizer {
        calls: Arc::default(),
        seen: Arc::default(),
    };
    let error = block_on(gql::subscribe(
        &schema,
        Request::new("subscription { tick }"),
        &ExecutionOptions::default().authorizer(authorizer.clone()),
    ))
    .expect_err("the root field must be authorized before streaming");
    assert_eq!(error.errors[0].message(), "async denied");
    assert_eq!(authorizer.calls.load(Ordering::SeqCst), 1);
    assert_eq!(
        authorizer.seen.lock().expect("authorizer log").as_slice(),
        ["Subscription.tick"]
    );
}

#[test]
fn sync_authorizer_still_works_through_the_default_async_adapter() {
    struct Deny;
    impl Authorizer for Deny {
        fn authorize(&self, _: &AuthorizationContext<'_>) -> Result<(), GraphQLError> {
            Err(GraphQLError::new("sync denied"))
        }
    }

    let schema = Schema::builder()
        .object(
            ObjectType::new("Query").field(
                Field::new("hello", TypeRef::string())
                    .resolve(|_| Box::pin(async { Ok(Value::from("world")) })),
            ),
        )
        .query("Query")
        .build()
        .expect("valid schema");

    let response = run_with(
        &schema,
        "{ hello }",
        &ExecutionOptions::default().authorizer(Deny),
    );
    assert_eq!(response.errors[0].message(), "sync denied");
}
