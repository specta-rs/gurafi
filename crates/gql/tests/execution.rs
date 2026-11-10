//! Execution behaviour, exercised through the public `gql` API.

use std::sync::{
    Arc, Mutex,
    atomic::{AtomicUsize, Ordering},
};

use futures::StreamExt;
use futures::executor::block_on;
use gql::{
    AuthorizationContext, Authorizer, EnumType, ExecutionOptions, Field, GraphQLError,
    InputObjectType, InputValue, InterfaceType, Object, ObjectType, Request, Response, ScalarType,
    Schema, TypeRef, UnionType, Value,
};

fn run(schema: &Schema, query: &str) -> Response {
    block_on(gql::execute(
        schema,
        Request::new(query),
        &ExecutionOptions::default(),
    ))
}

fn run_with(schema: &Schema, query: &str, options: &ExecutionOptions) -> Response {
    block_on(gql::execute(schema, Request::new(query), options))
}

fn run_vars(schema: &Schema, query: &str, variables: Object) -> Response {
    block_on(gql::execute(
        schema,
        Request::new(query).variables(&variables),
        &ExecutionOptions::default(),
    ))
}

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

fn scalar_schema() -> Schema {
    Schema::builder()
        .object(
            ObjectType::new("Query")
                .field(
                    Field::new("hello", TypeRef::string().non_null())
                        .resolve(|_| Box::pin(async { Ok(Value::from("world")) })),
                )
                .field(
                    Field::new("count", TypeRef::int())
                        .resolve(|_| Box::pin(async { Ok(Value::Int(42)) })),
                )
                .field(
                    Field::new("pi", TypeRef::float())
                        .resolve(|_| Box::pin(async { Ok(Value::Float(3.5)) })),
                )
                .field(
                    Field::new("flag", TypeRef::boolean())
                        .resolve(|_| Box::pin(async { Ok(Value::Boolean(true)) })),
                )
                .field(
                    Field::new("id", TypeRef::id())
                        .resolve(|_| Box::pin(async { Ok(Value::Int(7)) })),
                )
                .field(
                    Field::new("nothing", TypeRef::string())
                        .resolve(|_| Box::pin(async { Ok(Value::Null) })),
                ),
        )
        .query("Query")
        .build()
        .expect("valid schema")
}

#[test]
fn executes_scalars_aliases_and_null() {
    let schema = scalar_schema();
    let response = run(&schema, "{ hello, c: count, pi, flag, id, nothing }");
    assert!(response.errors.is_empty(), "{:?}", response.errors);
    assert_eq!(
        response.data,
        Some(value([
            ("hello", Value::from("world")),
            ("c", Value::Int(42)),
            ("pi", Value::Float(3.5)),
            ("flag", Value::Boolean(true)),
            ("id", Value::from("7")),
            ("nothing", Value::Null),
        ]))
    );
}

#[test]
fn resolves_nested_objects_with_default_resolver() {
    let schema = Schema::builder()
        .object(
            ObjectType::new("Query")
                .field(Field::new("user", TypeRef::named("User")))
                .field(Field::new("maybe", TypeRef::named("User"))),
        )
        .object(
            ObjectType::new("User")
                .field(Field::new("name", TypeRef::string().non_null()))
                .field(Field::new("age", TypeRef::int()))
                .field(Field::new("friend", TypeRef::named("User"))),
        )
        .query("Query")
        .build()
        .expect("valid schema");

    let response = block_on(gql::execute(
        &schema,
        Request::new("{ user { name age friend { name } } maybe { name } }"),
        &ExecutionOptions::default().root_value(value([(
            "user",
            value([
                ("name", Value::from("Alice")),
                ("age", Value::Int(30)),
                ("friend", value([("name", Value::from("Bob"))])),
            ]),
        )])),
    ));

    assert!(response.errors.is_empty(), "{:?}", response.errors);
    assert_eq!(
        response.data,
        Some(value([
            (
                "user",
                value([
                    ("name", Value::from("Alice")),
                    ("age", Value::Int(30)),
                    ("friend", value([("name", Value::from("Bob"))])),
                ])
            ),
            ("maybe", Value::Null),
        ]))
    );
}

#[test]
fn executes_lists_and_nested_lists() {
    let schema = Schema::builder()
        .object(
            ObjectType::new("Query")
                .field(Field::new("numbers", TypeRef::int().list()).resolve(|_| {
                    Box::pin(async {
                        Ok(Value::List(vec![Value::Int(1), Value::Null, Value::Int(3)]))
                    })
                }))
                .field(
                    Field::new("strict", TypeRef::int().non_null().list()).resolve(|_| {
                        Box::pin(async { Ok(Value::List(vec![Value::Int(1), Value::Int(2)])) })
                    }),
                )
                .field(
                    Field::new("required", TypeRef::int().non_null().list().non_null())
                        .resolve(|_| Box::pin(async { Ok(Value::List(vec![Value::Int(1)])) })),
                )
                .field(
                    Field::new("matrix", TypeRef::string().list().list()).resolve(|_| {
                        Box::pin(async {
                            Ok(Value::List(vec![
                                Value::List(vec![Value::from("a"), Value::from("b")]),
                                Value::Null,
                                Value::List(vec![Value::from("c")]),
                            ]))
                        })
                    }),
                ),
        )
        .query("Query")
        .build()
        .expect("valid schema");

    let response = run(&schema, "{ numbers strict required matrix }");
    assert!(response.errors.is_empty(), "{:?}", response.errors);
    assert_eq!(
        response.data,
        Some(value([
            (
                "numbers",
                Value::List(vec![Value::Int(1), Value::Null, Value::Int(3)])
            ),
            ("strict", Value::List(vec![Value::Int(1), Value::Int(2)])),
            ("required", Value::List(vec![Value::Int(1)])),
            (
                "matrix",
                Value::List(vec![
                    Value::List(vec![Value::from("a"), Value::from("b")]),
                    Value::Null,
                    Value::List(vec![Value::from("c")]),
                ])
            ),
        ]))
    );
}

fn null_propagation_schema() -> Schema {
    Schema::builder()
        .object(
            ObjectType::new("Query")
                .field(Field::new("user", TypeRef::named("User")).resolve(|_| {
                    Box::pin(async {
                        Ok(value([("name", Value::from("x")), ("age", Value::Int(1))]))
                    })
                }))
                .field(
                    Field::new("required", TypeRef::string().non_null())
                        .resolve(|_| Box::pin(async { Err(GraphQLError::new("root boom")) })),
                ),
        )
        .object(
            ObjectType::new("User")
                .field(
                    Field::new("name", TypeRef::string().non_null())
                        .resolve(|_| Box::pin(async { Err(GraphQLError::new("name boom")) })),
                )
                .field(
                    Field::new("age", TypeRef::int())
                        .resolve(|_| Box::pin(async { Err(GraphQLError::new("age boom")) })),
                ),
        )
        .query("Query")
        .build()
        .expect("valid schema")
}

#[test]
fn non_null_field_bubbles_to_nullable_parent() {
    let schema = null_propagation_schema();
    let response = run(&schema, "{ user { name } }");

    assert_eq!(response.data, Some(value([("user", Value::Null)])));
    assert_eq!(response.errors.len(), 1);
    assert_eq!(response.errors[0].message(), "name boom");
    assert_eq!(
        response.errors[0].path(),
        Some(
            &[
                gql::PathSegment::field("user"),
                gql::PathSegment::field("name")
            ][..]
        )
    );
    assert_eq!(response.errors[0].locations().len(), 1);
}

#[test]
fn nullable_field_error_becomes_null() {
    let schema = null_propagation_schema();
    let response = run(&schema, "{ user { age } }");

    assert_eq!(
        response.data,
        Some(value([("user", value([("age", Value::Null)]))]))
    );
    assert_eq!(response.errors.len(), 1);
    assert_eq!(response.errors[0].message(), "age boom");
    assert_eq!(
        response.errors[0].path(),
        Some(
            &[
                gql::PathSegment::field("user"),
                gql::PathSegment::field("age")
            ][..]
        )
    );
}

#[test]
fn root_non_null_failure_nulls_data() {
    let schema = null_propagation_schema();
    let response = run(&schema, "{ required }");

    assert_eq!(response.data, Some(Value::Null));
    assert_eq!(response.errors.len(), 1);
    assert_eq!(response.errors[0].message(), "root boom");
    assert_eq!(
        response.errors[0].path(),
        Some(&[gql::PathSegment::field("required")][..])
    );
}

#[test]
fn null_item_in_non_null_list_nulls_the_list() {
    let schema = Schema::builder()
        .object(
            ObjectType::new("Query")
                .field(
                    Field::new("strict", TypeRef::int().non_null().list()).resolve(|_| {
                        Box::pin(async {
                            Ok(Value::List(vec![Value::Int(1), Value::Null, Value::Int(3)]))
                        })
                    }),
                )
                .field(
                    Field::new("hard", TypeRef::int().non_null().list().non_null()).resolve(|_| {
                        Box::pin(async {
                            Ok(Value::List(vec![Value::Int(1), Value::Null, Value::Int(3)]))
                        })
                    }),
                ),
        )
        .query("Query")
        .build()
        .expect("valid schema");

    let response = run(&schema, "{ strict }");
    assert_eq!(response.data, Some(value([("strict", Value::Null)])));
    assert_eq!(response.errors.len(), 1);
    assert_eq!(
        response.errors[0].path(),
        Some(
            &[
                gql::PathSegment::field("strict"),
                gql::PathSegment::Index(1)
            ][..]
        )
    );

    let response = run(&schema, "{ hard }");
    assert_eq!(response.data, Some(Value::Null));
    assert_eq!(response.errors.len(), 1);
}

#[test]
fn reports_multiple_errors_and_respects_max_errors() {
    let schema = Schema::builder()
        .object(
            ObjectType::new("Query")
                .field(
                    Field::new("a", TypeRef::string())
                        .resolve(|_| Box::pin(async { Err(GraphQLError::new("a failed")) })),
                )
                .field(
                    Field::new("b", TypeRef::string())
                        .resolve(|_| Box::pin(async { Err(GraphQLError::new("b failed")) })),
                )
                .field(
                    Field::new("c", TypeRef::string())
                        .resolve(|_| Box::pin(async { Err(GraphQLError::new("c failed")) })),
                ),
        )
        .query("Query")
        .build()
        .expect("valid schema");

    let response = run(&schema, "{ a b c }");
    assert_eq!(response.errors.len(), 3);
    assert_eq!(
        response.data,
        Some(value([
            ("a", Value::Null),
            ("b", Value::Null),
            ("c", Value::Null),
        ]))
    );

    let response = run_with(
        &schema,
        "{ a b c }",
        &ExecutionOptions::default().max_errors(1),
    );
    assert_eq!(response.errors.len(), 1);
}

fn argument_schema() -> Schema {
    Schema::builder()
        .object(
            ObjectType::new("Query").field(
                Field::new("greet", TypeRef::string().non_null())
                    .arg(InputValue::new("name", TypeRef::string().non_null()))
                    .arg(InputValue::new("times", TypeRef::int()).default_value(2))
                    .resolve(|context| {
                        Box::pin(async move {
                            let name = context
                                .args
                                .get("name")
                                .and_then(Value::as_str)
                                .unwrap_or("")
                                .to_owned();
                            let times = context
                                .args
                                .get("times")
                                .and_then(Value::as_i32)
                                .unwrap_or(0);
                            Ok(Value::from(format!("{name}:{times}")))
                        })
                    }),
            ),
        )
        .query("Query")
        .build()
        .expect("valid schema")
}

#[test]
fn coerces_arguments_and_variables() {
    let schema = argument_schema();

    let response = run(&schema, r#"{ greet(name: "Bob") }"#);
    assert!(response.errors.is_empty(), "{:?}", response.errors);
    assert_eq!(field(&response, "greet"), &Value::from("Bob:2"));

    let query = "query Q($n: String!, $t: Int) { greet(name: $n, times: $t) }";
    let response = run_vars(
        &schema,
        query,
        Object::from_fields([("n", Value::from("Alice")), ("t", Value::Int(5))]),
    );
    assert!(response.errors.is_empty(), "{:?}", response.errors);
    assert_eq!(field(&response, "greet"), &Value::from("Alice:5"));

    // A variable that is absent but has a document default uses the default.
    let response = run(
        &schema,
        r#"query Q($n: String = "Def") { greet(name: $n) }"#,
    );
    assert!(response.errors.is_empty(), "{:?}", response.errors);
    assert_eq!(field(&response, "greet"), &Value::from("Def:2"));

    // A missing argument default is applied.
    let response = run(&schema, r#"{ greet(name: "x", times: 9) }"#);
    assert_eq!(field(&response, "greet"), &Value::from("x:9"));

    // A variable's own default is used when the variable is not supplied.
    let response = run_vars(
        &schema,
        r#"query Q($n: String!, $t: Int = 4) { greet(name: $n, times: $t) }"#,
        Object::from_fields([("n", Value::from("Zoe"))]),
    );
    assert!(response.errors.is_empty(), "{:?}", response.errors);
    assert_eq!(field(&response, "greet"), &Value::from("Zoe:4"));
}

#[test]
fn reports_variable_errors_before_execution() {
    let schema = argument_schema();

    let response = run(&schema, "query Q($n: String!) { greet(name: $n) }");
    assert_eq!(response.data, None);
    assert_eq!(response.errors.len(), 1);
    assert!(response.errors[0].message().contains("was not provided"));

    let query = "query Q($t: Int) { greet(name: \"x\", times: $t) }";
    let response = run_vars(
        &schema,
        query,
        Object::from_fields([("t", Value::from("nope"))]),
    );
    assert_eq!(response.data, None);
    assert_eq!(response.errors.len(), 1);
    assert!(response.errors[0].message().contains("Variable \"$t\""));

    let query = "query Q($t: Int!) { greet(name: \"x\", times: $t) }";
    let response = run_vars(&schema, query, Object::from_fields([("t", Value::Null)]));
    assert_eq!(response.data, None);
    assert!(response.errors[0].message().contains("must not be null"));

    // Out-of-range Int literals and unknown arguments are rejected by validation before
    // execution; see `tests/coercion.rs` for the coercion-level checks.
    let response = run(&schema, r#"{ greet(name: "x") }"#);
    assert!(response.errors.is_empty(), "{:?}", response.errors);
    assert_eq!(field(&response, "greet"), &Value::from("x:2"));
}

fn input_schema() -> Schema {
    let filter = InputObjectType::new("Filter")
        .field(InputValue::new("limit", TypeRef::int().non_null()))
        .field(
            InputValue::new("color", TypeRef::named("Color"))
                .default_value(Value::enumeration("RED")),
        )
        .field(InputValue::new("tags", TypeRef::string().non_null().list()));

    Schema::builder()
        .enumeration(
            EnumType::new("Color")
                .value("RED")
                .value("GREEN")
                .value("BLUE"),
        )
        .input_object(filter)
        .object(
            ObjectType::new("Query")
                .field(
                    Field::new("search", TypeRef::string().non_null())
                        .arg(InputValue::new(
                            "filter",
                            TypeRef::named("Filter").non_null(),
                        ))
                        .resolve(|context| {
                            Box::pin(async move {
                                let filter = context
                                    .args
                                    .get("filter")
                                    .and_then(Value::as_object)
                                    .cloned()
                                    .unwrap_or_default();
                                let color = filter
                                    .get("color")
                                    .and_then(Value::as_str)
                                    .unwrap_or("?")
                                    .to_owned();
                                let limit =
                                    filter.get("limit").and_then(Value::as_i32).unwrap_or(0);
                                let tags = filter
                                    .get("tags")
                                    .and_then(Value::as_list)
                                    .map_or(0, <[Value]>::len);
                                Ok(Value::from(format!("{color}:{limit}:{tags}")))
                            })
                        }),
                )
                .field(
                    Field::new("echo", TypeRef::named("Color").non_null())
                        .arg(InputValue::new("color", TypeRef::named("Color").non_null()))
                        .resolve(|context| {
                            Box::pin(async move {
                                Ok(context.args.get("color").cloned().unwrap_or(Value::Null))
                            })
                        }),
                ),
        )
        .query("Query")
        .build()
        .expect("valid schema")
}

#[test]
fn coerces_input_objects_and_enums() {
    let schema = input_schema();

    let response = run(
        &schema,
        r#"{ search(filter: { limit: 5, tags: ["x", "y"] }) }"#,
    );
    assert!(response.errors.is_empty(), "{:?}", response.errors);
    assert_eq!(field(&response, "search"), &Value::from("RED:5:2"));

    let query = "query Q($f: Filter!) { search(filter: $f) }";
    let response = run_vars(
        &schema,
        query,
        Object::from_fields([(
            "f",
            value([
                ("limit", Value::Int(10)),
                ("color", Value::enumeration("GREEN")),
                ("tags", Value::List(vec![Value::from("a")])),
            ]),
        )]),
    );
    assert!(response.errors.is_empty(), "{:?}", response.errors);
    assert_eq!(field(&response, "search"), &Value::from("GREEN:10:1"));

    let response = run(&schema, r#"{ echo(color: BLUE) }"#);
    assert!(response.errors.is_empty(), "{:?}", response.errors);
    assert_eq!(field(&response, "echo"), &Value::Enum("BLUE".into()));
}

#[test]
fn custom_scalars_round_trip() {
    let date = ScalarType::new("Date")
        .serialize(|value| match value {
            Value::String(value) => Ok(Value::String(value.clone())),
            _ => Err("Date must be a string".to_owned()),
        })
        .parse_value(|value| match value {
            Value::String(value) => Ok(Value::String(value.clone())),
            _ => Err("Date must be a string".to_owned()),
        })
        .parse_literal(|value| match value {
            gql::ast::Value::String(value) => Ok(Value::String(Arc::from(value.as_ref()))),
            _ => Err("Date must be a string literal".to_owned()),
        });

    let schema = Schema::builder()
        .scalar(date)
        .object(
            ObjectType::new("Query")
                .field(
                    Field::new("today", TypeRef::named("Date").non_null())
                        .resolve(|_| Box::pin(async { Ok(Value::from("2026-01-01")) })),
                )
                .field(
                    Field::new("echo", TypeRef::named("Date").non_null())
                        .arg(InputValue::new("date", TypeRef::named("Date").non_null()))
                        .resolve(|context| {
                            Box::pin(async move {
                                Ok(context.args.get("date").cloned().unwrap_or(Value::Null))
                            })
                        }),
                )
                .field(
                    Field::new("broken", TypeRef::named("Date"))
                        .resolve(|_| Box::pin(async { Ok(Value::Int(1)) })),
                ),
        )
        .query("Query")
        .build()
        .expect("valid schema");

    let response = run(&schema, r#"{ today echo(date: "2026-02-03") }"#);
    assert!(response.errors.is_empty(), "{:?}", response.errors);
    assert_eq!(field(&response, "today"), &Value::from("2026-01-01"));
    assert_eq!(field(&response, "echo"), &Value::from("2026-02-03"));

    let query = "query Q($d: Date!) { echo(date: $d) }";
    let response = run_vars(
        &schema,
        query,
        Object::from_fields([("d", Value::from("2026-03-04"))]),
    );
    assert!(response.errors.is_empty(), "{:?}", response.errors);
    assert_eq!(field(&response, "echo"), &Value::from("2026-03-04"));

    let response = run(&schema, "{ broken }");
    assert_eq!(response.data, Some(value([("broken", Value::Null)])));
    assert_eq!(response.errors.len(), 1);
}

fn fragment_schema() -> Schema {
    Schema::builder()
        .object(
            ObjectType::new("Query").field(Field::new("user", TypeRef::named("User")).resolve(
                |_| {
                    Box::pin(async {
                        Ok(value([
                            ("id", Value::from("1")),
                            ("name", Value::from("Alice")),
                            ("email", Value::from("alice@example.com")),
                        ]))
                    })
                },
            )),
        )
        .object(
            ObjectType::new("User")
                .field(Field::new("id", TypeRef::id().non_null()))
                .field(Field::new("name", TypeRef::string().non_null()))
                .field(Field::new("email", TypeRef::string())),
        )
        .query("Query")
        .build()
        .expect("valid schema")
}

#[test]
fn expands_fragments_and_honours_directives() {
    let schema = fragment_schema();

    let query = r#"
        query Q($includeEmail: Boolean!, $skipName: Boolean!) {
            user {
                ...UserFields
                id
                email @include(if: $includeEmail)
                name @skip(if: $skipName)
            }
        }
        fragment UserFields on User { id name }
    "#;
    let response = run_vars(
        &schema,
        query,
        Object::from_fields([
            ("includeEmail", Value::Boolean(true)),
            ("skipName", Value::Boolean(false)),
        ]),
    );
    assert!(response.errors.is_empty(), "{:?}", response.errors);
    assert_eq!(
        response.data,
        Some(value([(
            "user",
            value([
                ("id", Value::from("1")),
                ("name", Value::from("Alice")),
                ("email", Value::from("alice@example.com")),
            ])
        )]))
    );

    let response = run_vars(
        &schema,
        query,
        Object::from_fields([
            ("includeEmail", Value::Boolean(false)),
            ("skipName", Value::Boolean(true)),
        ]),
    );
    assert!(response.errors.is_empty(), "{:?}", response.errors);
    // The fragment still selects `name`, so `@skip` on the direct occurrence does not remove it.
    assert_eq!(
        response.data,
        Some(value([(
            "user",
            value([("id", Value::from("1")), ("name", Value::from("Alice"))])
        )]))
    );
}

#[test]
fn merges_fields_with_the_same_response_key() {
    let schema = fragment_schema();
    let response = run(&schema, "{ user { name } user { id } }");
    assert!(response.errors.is_empty(), "{:?}", response.errors);
    assert_eq!(
        response.data,
        Some(value([(
            "user",
            value([("name", Value::from("Alice")), ("id", Value::from("1"))])
        )]))
    );
}

#[test]
fn executes_mutations_in_document_order() {
    let log: Arc<Mutex<Vec<&'static str>>> = Arc::new(Mutex::new(Vec::new()));

    fn mutation_field(name: &'static str, log: Arc<Mutex<Vec<&'static str>>>) -> Field {
        Field::new(name, TypeRef::string().non_null()).resolve(move |_| {
            let log = Arc::clone(&log);
            Box::pin(async move {
                if let Ok(mut log) = log.lock() {
                    log.push(name);
                }
                Ok(Value::from(name))
            })
        })
    }

    let schema = Schema::builder()
        .object(ObjectType::new("Query").field(Field::new("noop", TypeRef::boolean())))
        .object(
            ObjectType::new("Mutation")
                .field(mutation_field("first", Arc::clone(&log)))
                .field(mutation_field("second", Arc::clone(&log)))
                .field(mutation_field("third", Arc::clone(&log))),
        )
        .query("Query")
        .mutation("Mutation")
        .build()
        .expect("valid schema");

    let response = run(&schema, "mutation { third second first }");
    assert!(response.errors.is_empty(), "{:?}", response.errors);
    assert_eq!(
        response.data,
        Some(value([
            ("third", Value::from("third")),
            ("second", Value::from("second")),
            ("first", Value::from("first")),
        ]))
    );
    assert_eq!(*log.lock().expect("lock"), vec!["third", "second", "first"]);
}

#[derive(Clone)]
struct Model {
    name: String,
    age: i32,
}

#[test]
fn supports_root_values_and_opaque_model_parents() {
    let schema = Schema::builder()
        .object(
            ObjectType::new("Query")
                .field(
                    Field::new("name", TypeRef::string().non_null()).resolve(|context| {
                        Box::pin(async move {
                            let model = context
                                .parent
                                .as_external::<Model>()
                                .ok_or_else(|| GraphQLError::new("expected a Model"))?;
                            Ok(Value::from(model.name.clone()))
                        })
                    }),
                )
                .field(
                    Field::new("age", TypeRef::int().non_null()).resolve(|context| {
                        Box::pin(async move {
                            let model = context
                                .parent
                                .as_external::<Model>()
                                .ok_or_else(|| GraphQLError::new("expected a Model"))?;
                            Ok(Value::Int(model.age))
                        })
                    }),
                )
                .field(Field::new("unresolved", TypeRef::string())),
        )
        .query("Query")
        .build()
        .expect("valid schema");

    let response = block_on(gql::execute(
        &schema,
        Request::new("{ name age }"),
        &ExecutionOptions::default().root_value(Value::external(Model {
            name: "Alice".to_owned(),
            age: 30,
        })),
    ));
    assert!(response.errors.is_empty(), "{:?}", response.errors);
    assert_eq!(
        response.data,
        Some(value([
            ("name", Value::from("Alice")),
            ("age", Value::Int(30)),
        ]))
    );

    // A field without a resolver cannot read a property from an opaque model value.
    let response = block_on(gql::execute(
        &schema,
        Request::new("{ unresolved }"),
        &ExecutionOptions::default().root_value(Value::external(Model {
            name: "Alice".to_owned(),
            age: 30,
        })),
    ));
    assert_eq!(response.data, Some(value([("unresolved", Value::Null)])));
    assert_eq!(response.errors.len(), 1);
    assert!(response.errors[0].message().contains("No resolver"));
}

#[test]
fn root_object_value_backs_the_default_resolver() {
    let schema = Schema::builder()
        .object(ObjectType::new("Query").field(Field::new("hello", TypeRef::string())))
        .query("Query")
        .build()
        .expect("valid schema");

    let response = block_on(gql::execute(
        &schema,
        Request::new("{ hello }"),
        &ExecutionOptions::default().root_value(value([("hello", Value::from("root"))])),
    ));
    assert!(response.errors.is_empty(), "{:?}", response.errors);
    assert_eq!(response.data, Some(value([("hello", Value::from("root"))])));
}

#[derive(Clone)]
struct Person {
    name: String,
    age: i32,
}

#[derive(Clone)]
struct Team {
    name: String,
    members: i32,
}

#[test]
fn resolves_interfaces_and_unions() {
    let schema = Schema::builder()
        .interface(
            InterfaceType::new("Named")
                .field(Field::new("name", TypeRef::string().non_null()))
                .resolve_type(|value, _context| {
                    if value.as_external::<Person>().is_some() {
                        Ok("Person".into())
                    } else if value.as_external::<Team>().is_some() {
                        Ok("Team".into())
                    } else {
                        Err(GraphQLError::new("unknown runtime type"))
                    }
                }),
        )
        .union(UnionType::new("Search").member("Person").member("Team"))
        .object(
            ObjectType::new("Person")
                .implements("Named")
                .is_type_of(|value| value.as_external::<Person>().is_some())
                .field(
                    Field::new("name", TypeRef::string().non_null()).resolve(|context| {
                        Box::pin(async move {
                            Ok(Value::from(
                                context
                                    .parent
                                    .as_external::<Person>()
                                    .map(|person| person.name.clone())
                                    .unwrap_or_default(),
                            ))
                        })
                    }),
                )
                .field(
                    Field::new("age", TypeRef::int().non_null()).resolve(|context| {
                        Box::pin(async move {
                            Ok(Value::Int(
                                context
                                    .parent
                                    .as_external::<Person>()
                                    .map_or(0, |person| person.age),
                            ))
                        })
                    }),
                ),
        )
        .object(
            ObjectType::new("Team")
                .implements("Named")
                .is_type_of(|value| value.as_external::<Team>().is_some())
                .field(
                    Field::new("name", TypeRef::string().non_null()).resolve(|context| {
                        Box::pin(async move {
                            Ok(Value::from(
                                context
                                    .parent
                                    .as_external::<Team>()
                                    .map(|team| team.name.clone())
                                    .unwrap_or_default(),
                            ))
                        })
                    }),
                )
                .field(
                    Field::new("members", TypeRef::int().non_null()).resolve(|context| {
                        Box::pin(async move {
                            Ok(Value::Int(
                                context
                                    .parent
                                    .as_external::<Team>()
                                    .map_or(0, |team| team.members),
                            ))
                        })
                    }),
                ),
        )
        .object(
            ObjectType::new("Query")
                .field(
                    Field::new("named", TypeRef::named("Named").non_null()).resolve(|_| {
                        Box::pin(async {
                            Ok(Value::external(Person {
                                name: "Alice".to_owned(),
                                age: 30,
                            }))
                        })
                    }),
                )
                .field(
                    Field::new("search", TypeRef::named("Search").non_null().list()).resolve(
                        |_| {
                            Box::pin(async {
                                Ok(Value::List(vec![
                                    Value::external(Person {
                                        name: "Alice".to_owned(),
                                        age: 30,
                                    }),
                                    Value::external(Team {
                                        name: "Core".to_owned(),
                                        members: 4,
                                    }),
                                ]))
                            })
                        },
                    ),
                ),
        )
        .query("Query")
        .build()
        .expect("valid schema");

    let query = r#"
        {
            named {
                __typename
                name
                ... on Person { age }
            }
            search {
                __typename
                ... on Person { name age }
                ... on Team { name members }
            }
        }
    "#;
    let response = run(&schema, query);
    assert!(response.errors.is_empty(), "{:?}", response.errors);
    assert_eq!(
        response.data,
        Some(value([
            (
                "named",
                value([
                    ("__typename", Value::from("Person")),
                    ("name", Value::from("Alice")),
                    ("age", Value::Int(30)),
                ])
            ),
            (
                "search",
                Value::List(vec![
                    value([
                        ("__typename", Value::from("Person")),
                        ("name", Value::from("Alice")),
                        ("age", Value::Int(30)),
                    ]),
                    value([
                        ("__typename", Value::from("Team")),
                        ("name", Value::from("Core")),
                        ("members", Value::Int(4)),
                    ]),
                ])
            ),
        ]))
    );
}

#[test]
fn reports_abstract_type_resolution_failures() {
    let schema =
        Schema::builder()
            .union(UnionType::new("Search").member("Person"))
            .object(
                ObjectType::new("Person")
                    .field(Field::new("name", TypeRef::string()))
                    .is_type_of(|value| {
                        value
                            .as_object()
                            .and_then(|object| object.get("isPerson"))
                            .and_then(Value::as_bool)
                            == Some(true)
                    }),
            )
            .object(ObjectType::new("Query").field(
                Field::new("search", TypeRef::named("Search")).resolve(|_| {
                    Box::pin(async { Ok(value([("isPerson", Value::Boolean(false))])) })
                }),
            ))
            .query("Query")
            .build()
            .expect("valid schema");

    let response = run(&schema, "{ search { ... on Person { name } } }");
    assert_eq!(response.data, Some(value([("search", Value::Null)])));
    assert_eq!(response.errors.len(), 1);
    assert!(response.errors[0].message().contains("could not determine"));
}

#[test]
fn executes_subscriptions() {
    let schema = Schema::builder()
        .object(ObjectType::new("Query").field(Field::new("ping", TypeRef::string())))
        .object(ObjectType::new("Subscription").field(
            Field::new("counter", TypeRef::named("Counter").non_null()).subscribe(|_| {
                Box::pin(async {
                    Ok(futures::stream::iter(vec![
                        Ok(value([("counter", value([("value", Value::Int(1))]))])),
                        Ok(value([("counter", value([("value", Value::Int(2))]))])),
                    ])
                    .boxed())
                })
            }),
        ))
        .object(ObjectType::new("Counter").field(Field::new("value", TypeRef::int().non_null())))
        .query("Query")
        .subscription("Subscription")
        .build()
        .expect("valid schema");

    let subscription = block_on(gql::subscribe(
        &schema,
        Request::new("subscription { counter { value } }"),
        &ExecutionOptions::default(),
    ))
    .expect("subscription starts");

    let responses = block_on(subscription.stream.collect::<Vec<_>>());
    assert_eq!(responses.len(), 2);
    assert!(responses[0].errors.is_empty(), "{:?}", responses[0].errors);
    assert_eq!(
        responses[0].data,
        Some(value([("counter", value([("value", Value::Int(1))]))]))
    );
    assert_eq!(
        responses[1].data,
        Some(value([("counter", value([("value", Value::Int(2))]))]))
    );
}

#[test]
fn subscription_source_errors_follow_nullability() {
    let schema = Schema::builder()
        .object(ObjectType::new("Query").field(Field::new("ping", TypeRef::string())))
        .object(ObjectType::new("Subscription").field(
            Field::new("broken", TypeRef::named("Counter")).subscribe(|_| {
                Box::pin(async {
                    Ok(futures::stream::iter(vec![Err(GraphQLError::new("source boom"))]).boxed())
                })
            }),
        ))
        .object(ObjectType::new("Counter").field(Field::new("value", TypeRef::int().non_null())))
        .query("Query")
        .subscription("Subscription")
        .build()
        .expect("valid schema");

    let subscription = block_on(gql::subscribe(
        &schema,
        Request::new("subscription { broken { value } }"),
        &ExecutionOptions::default(),
    ))
    .expect("subscription starts");

    let responses = block_on(subscription.stream.collect::<Vec<_>>());
    assert_eq!(responses.len(), 1);
    assert_eq!(responses[0].data, Some(value([("broken", Value::Null)])));
    assert_eq!(responses[0].errors.len(), 1);
    assert_eq!(responses[0].errors[0].message(), "source boom");
    assert_eq!(
        responses[0].errors[0].path(),
        Some(&[gql::PathSegment::field("broken")][..])
    );
}

#[test]
fn subscription_events_are_root_values() {
    let subscribe_calls = Arc::new(AtomicUsize::new(0));
    let resolve_calls = Arc::new(AtomicUsize::new(0));

    let scalar_subscribe_calls = Arc::clone(&subscribe_calls);
    let scalar_resolve_calls = Arc::clone(&resolve_calls);

    let schema = Schema::builder()
        .object(ObjectType::new("Query").field(Field::new("ping", TypeRef::string())))
        .object(
            ObjectType::new("Subscription")
                .field(
                    Field::new("scalar", TypeRef::int().non_null())
                        .subscribe(move |_| {
                            let calls = Arc::clone(&scalar_subscribe_calls);
                            Box::pin(async move {
                                calls.fetch_add(1, Ordering::SeqCst);
                                Ok(futures::stream::iter(vec![
                                    Ok::<Value, GraphQLError>(Value::Int(1)),
                                    Ok(Value::Int(2)),
                                ])
                                .boxed())
                            })
                        })
                        .resolve(move |_| {
                            let calls = Arc::clone(&scalar_resolve_calls);
                            Box::pin(async move {
                                calls.fetch_add(1, Ordering::SeqCst);
                                Ok(Value::Int(99))
                            })
                        }),
                )
                .field(
                    Field::new("widget", TypeRef::named("Widget")).subscribe(|_| {
                        Box::pin(async {
                            Ok(
                                futures::stream::iter(vec![Ok::<Value, GraphQLError>(value([(
                                    "widget",
                                    value([("name", Value::from("a")), ("count", Value::Int(1))]),
                                )]))])
                                .boxed(),
                            )
                        })
                    }),
                )
                .field(
                    Field::new(
                        "widgets",
                        TypeRef::named("Widget").non_null().list().non_null(),
                    )
                    .subscribe(|_| {
                        Box::pin(async {
                            Ok(
                                futures::stream::iter(vec![Ok::<Value, GraphQLError>(value([(
                                    "widgets",
                                    Value::List(vec![
                                        value([
                                            ("name", Value::from("a")),
                                            ("count", Value::Int(1)),
                                        ]),
                                        value([
                                            ("name", Value::from("b")),
                                            ("count", Value::Int(2)),
                                        ]),
                                    ]),
                                )]))])
                                .boxed(),
                            )
                        })
                    }),
                )
                .field(Field::new("nullable", TypeRef::string()).subscribe(|_| {
                    Box::pin(async {
                        Ok(
                            futures::stream::iter(vec![Ok::<Value, GraphQLError>(value([(
                                "nullable",
                                Value::Null,
                            )]))])
                            .boxed(),
                        )
                    })
                }))
                .field(
                    Field::new("strict", TypeRef::string().non_null()).subscribe(|_| {
                        Box::pin(async {
                            Ok(
                                futures::stream::iter(vec![Ok::<Value, GraphQLError>(value([(
                                    "strict",
                                    Value::Null,
                                )]))])
                                .boxed(),
                            )
                        })
                    }),
                ),
        )
        .object(
            ObjectType::new("Widget")
                .field(Field::new("name", TypeRef::string().non_null()))
                .field(Field::new("count", TypeRef::int())),
        )
        .query("Query")
        .subscription("Subscription")
        .build()
        .expect("valid schema");

    fn responses(schema: &Schema, query: &str) -> Vec<Response> {
        let subscription = block_on(gql::subscribe(
            schema,
            Request::new(query),
            &ExecutionOptions::default(),
        ))
        .expect("subscription starts");
        block_on(subscription.stream.collect::<Vec<_>>())
    }

    // Each event is the operation's root value, so the root field's `resolve` runs for every
    // event instead of the event being used as the field value directly.
    let scalar = responses(&schema, "subscription { scalar }");
    assert_eq!(scalar.len(), 2);
    assert!(scalar[0].errors.is_empty(), "{:?}", scalar[0].errors);
    assert_eq!(scalar[0].data, Some(value([("scalar", Value::Int(99))])));
    assert_eq!(scalar[1].data, Some(value([("scalar", Value::Int(99))])));
    assert_eq!(subscribe_calls.load(Ordering::SeqCst), 1);
    assert_eq!(resolve_calls.load(Ordering::SeqCst), 2);

    // Without a `resolve` resolver the default property resolver reads the root field's name from
    // the event object.
    let widget = responses(&schema, "subscription { widget { name count } }");
    assert_eq!(widget.len(), 1);
    assert!(widget[0].errors.is_empty(), "{:?}", widget[0].errors);
    assert_eq!(
        widget[0].data,
        Some(value([(
            "widget",
            value([("name", Value::from("a")), ("count", Value::Int(1))]),
        )]))
    );

    // A list event completes every item.
    let widgets = responses(&schema, "subscription { widgets { name } }");
    assert_eq!(widgets.len(), 1);
    assert!(widgets[0].errors.is_empty(), "{:?}", widgets[0].errors);
    assert_eq!(
        widgets[0].data,
        Some(value([(
            "widgets",
            Value::List(vec![
                value([("name", Value::from("a"))]),
                value([("name", Value::from("b"))]),
            ])
        )]))
    );

    // A null root-field value is a legal event for a nullable field.
    let nullable = responses(&schema, "subscription { nullable }");
    assert_eq!(nullable.len(), 1);
    assert!(nullable[0].errors.is_empty(), "{:?}", nullable[0].errors);
    assert_eq!(nullable[0].data, Some(value([("nullable", Value::Null)])));

    // A null event for a non-null root field bubbles to a null data entry.
    let strict = responses(&schema, "subscription { strict }");
    assert_eq!(strict.len(), 1);
    assert_eq!(strict[0].data, Some(Value::Null));
    assert_eq!(strict[0].errors.len(), 1);
    assert!(
        strict[0].errors[0].message().contains("Cannot return null"),
        "{:?}",
        strict[0].errors[0]
    );
    assert_eq!(
        strict[0].errors[0].path(),
        Some(&[gql::PathSegment::field("strict")][..])
    );

    // `subscribe` ran exactly once and `resolve` ran once per scalar event.
    assert_eq!(resolve_calls.load(Ordering::SeqCst), 2);
    assert_eq!(subscribe_calls.load(Ordering::SeqCst), 1);
}

#[test]
fn subscription_requires_a_subscribe_resolver() {
    let schema = Schema::builder()
        .object(ObjectType::new("Query").field(Field::new("ping", TypeRef::string())))
        .object(ObjectType::new("Subscription").field(Field::new("broken", TypeRef::string())))
        .query("Query")
        .subscription("Subscription")
        .build()
        .expect("valid schema");

    let error = block_on(gql::subscribe(
        &schema,
        Request::new("subscription { broken }"),
        &ExecutionOptions::default(),
    ))
    .expect_err("setup must fail");
    assert!(error.errors[0].message().contains("subscribe resolver"));
}

#[test]
fn execute_rejects_subscription_operations() {
    let schema = Schema::builder()
        .object(ObjectType::new("Query").field(Field::new("ping", TypeRef::string())))
        .object(ObjectType::new("Subscription").field(Field::new("broken", TypeRef::string())))
        .query("Query")
        .subscription("Subscription")
        .build()
        .expect("valid schema");

    let response = run(&schema, "subscription { broken }");
    assert_eq!(response.data, None);
    assert!(response.errors[0].message().contains("subscribe"));
}

fn multi_operation_schema() -> Schema {
    scalar_schema()
}

#[test]
fn selects_operations_by_name() {
    let schema = multi_operation_schema();
    let document = "query A { hello } query B { count }";

    let response = run(&schema, document);
    assert_eq!(response.data, None);
    assert!(response.errors[0].message().contains("operation name"));

    let response = block_on(gql::execute(
        &schema,
        Request::new(document).operation_name("B"),
        &ExecutionOptions::default(),
    ));
    assert!(response.errors.is_empty(), "{:?}", response.errors);
    assert_eq!(response.data, Some(value([("count", Value::Int(42))])));

    let response = block_on(gql::execute(
        &schema,
        Request::new(document).operation_name("Nope"),
        &ExecutionOptions::default(),
    ));
    assert_eq!(response.data, None);
    assert!(response.errors[0].message().contains("Unknown operation"));
}

struct DenySecret;

impl Authorizer for DenySecret {
    fn authorize(&self, context: &AuthorizationContext<'_>) -> Result<(), GraphQLError> {
        if context.field_name == "secret" {
            Err(GraphQLError::new("denied"))
        } else {
            Ok(())
        }
    }
}

#[test]
fn authorizer_denial_is_a_field_error() {
    let schema = Schema::builder()
        .object(
            ObjectType::new("Query")
                .field(
                    Field::new("public", TypeRef::string())
                        .resolve(|_| Box::pin(async { Ok(Value::from("ok")) })),
                )
                .field(
                    Field::new("secret", TypeRef::string())
                        .resolve(|_| Box::pin(async { Ok(Value::from("hidden")) })),
                ),
        )
        .query("Query")
        .build()
        .expect("valid schema");

    let response = run_with(
        &schema,
        "{ public secret }",
        &ExecutionOptions::default().authorizer(DenySecret),
    );
    assert_eq!(
        response.data,
        Some(value([
            ("public", Value::from("ok")),
            ("secret", Value::Null),
        ]))
    );
    assert_eq!(response.errors.len(), 1);
    assert_eq!(response.errors[0].message(), "denied");
    assert_eq!(
        response.errors[0].path(),
        Some(&[gql::PathSegment::field("secret")][..])
    );
}

#[test]
fn resolves_typename_meta_field() {
    let schema = fragment_schema();
    let response = run(&schema, "{ __typename user { __typename } }");
    assert!(response.errors.is_empty(), "{:?}", response.errors);
    assert_eq!(
        response.data,
        Some(value([
            ("__typename", Value::from("Query")),
            ("user", value([("__typename", Value::from("User"))])),
        ]))
    );
}

#[test]
fn typename_uses_descriptor_metadata() {
    let schema = Schema::builder()
        .object(ObjectType::new("Query").field(Field::new("user", TypeRef::named("User"))))
        .object(ObjectType::new("User").field(Field::new("name", TypeRef::string())))
        .query("Query")
        .build()
        .expect("valid schema");

    let response = block_on(gql::execute(
        &schema,
        Request::new("{ user { __typename } }"),
        &ExecutionOptions::default().root_value(value([(
            "user",
            value([
                ("__gql_typename", Value::from("SpecialUser")),
                ("name", Value::from("Alice")),
            ]),
        )])),
    ));
    assert!(response.errors.is_empty(), "{:?}", response.errors);
    assert_eq!(
        response.data,
        Some(value([(
            "user",
            value([("__typename", Value::from("SpecialUser"))])
        )]))
    );
}

#[test]
fn directives_apply_to_spreads_and_inline_fragments() {
    let schema = fragment_schema();
    let query = r#"
        query Q($includeFragment: Boolean!, $skipInline: Boolean!) {
            user {
                id
                ...UserFields @include(if: $includeFragment)
                ... on User @skip(if: $skipInline) { email }
            }
        }
        fragment UserFields on User { name }
    "#;

    let response = run_vars(
        &schema,
        query,
        Object::from_fields([
            ("includeFragment", Value::Boolean(true)),
            ("skipInline", Value::Boolean(false)),
        ]),
    );
    assert!(response.errors.is_empty(), "{:?}", response.errors);
    assert_eq!(
        response.data,
        Some(value([(
            "user",
            value([
                ("id", Value::from("1")),
                ("name", Value::from("Alice")),
                ("email", Value::from("alice@example.com")),
            ])
        )]))
    );

    let response = run_vars(
        &schema,
        query,
        Object::from_fields([
            ("includeFragment", Value::Boolean(false)),
            ("skipInline", Value::Boolean(true)),
        ]),
    );
    assert!(response.errors.is_empty(), "{:?}", response.errors);
    assert_eq!(
        response.data,
        Some(value([("user", value([("id", Value::from("1"))]))]))
    );
}

#[test]
fn response_extensions_are_reported() {
    let schema = Schema::builder()
        .object(ObjectType::new("Query").field(
            Field::new("traced", TypeRef::string().non_null()).resolve(|context| {
                context.context.set_extension("traceId", "abc");
                Box::pin(async { Ok(Value::from("ok")) })
            }),
        ))
        .query("Query")
        .build()
        .expect("valid schema");

    let response = run(&schema, "{ traced }");
    assert!(response.errors.is_empty(), "{:?}", response.errors);
    assert_eq!(
        response
            .extensions
            .and_then(|extensions| extensions.get("traceId").cloned()),
        Some(Value::from("abc"))
    );
}

#[test]
fn typename_is_available_without_introspection() {
    let schema = fragment_schema();
    let response = run_with(
        &schema,
        "{ __typename user { __typename } }",
        &ExecutionOptions::default().introspection(false),
    );
    assert!(response.errors.is_empty(), "{:?}", response.errors);
    assert_eq!(
        response.data,
        Some(value([
            ("__typename", Value::from("Query")),
            ("user", value([("__typename", Value::from("User"))])),
        ]))
    );
}

#[test]
fn execute_future_is_send() {
    fn assert_send<T: Send>(value: T) -> T {
        value
    }

    let schema = scalar_schema();
    let options = ExecutionOptions::default();
    let future = assert_send(gql::execute(&schema, Request::new("{ hello }"), &options));
    let response = block_on(future);
    assert!(response.errors.is_empty(), "{:?}", response.errors);
    assert_eq!(field(&response, "hello"), &Value::from("world"));
}

#[test]
fn reports_unknown_fields_defensively() {
    // Validation normally rejects this, but execution must still fail safely if it does not.
    let schema = fragment_schema();
    let response = run(&schema, "{ user { nope } }");
    assert!(!response.errors.is_empty());
}

#[test]
fn query_fields_resolve_concurrently_within_the_concurrency_limit() {
    use std::sync::{
        Arc,
        atomic::{AtomicUsize, Ordering},
    };

    let started = Arc::new(AtomicUsize::new(0));
    let a_started = Arc::clone(&started);
    let b_started = Arc::clone(&started);

    let a = gql::Field::new("a", gql::TypeRef::string().non_null()).resolve(move |_| {
        let started = Arc::clone(&a_started);
        Box::pin(async move {
            started.fetch_add(1, Ordering::SeqCst);
            // If fields run serially this yields forever; cap the wait and fail instead.
            for _ in 0..10_000 {
                if started.load(Ordering::SeqCst) >= 2 {
                    return Ok(gql::Value::from("a"));
                }
                struct YieldOnce(bool);
                impl std::future::Future for YieldOnce {
                    type Output = ();
                    fn poll(
                        mut self: std::pin::Pin<&mut Self>,
                        cx: &mut std::task::Context<'_>,
                    ) -> std::task::Poll<()> {
                        if self.0 {
                            std::task::Poll::Ready(())
                        } else {
                            self.0 = true;
                            cx.waker().wake_by_ref();
                            std::task::Poll::Pending
                        }
                    }
                }
                YieldOnce(false).await;
            }
            Ok(gql::Value::from("serial"))
        })
    });
    let b = gql::Field::new("b", gql::TypeRef::string().non_null()).resolve(move |_| {
        let started = Arc::clone(&b_started);
        Box::pin(async move {
            started.fetch_add(1, Ordering::SeqCst);
            Ok(gql::Value::from("b"))
        })
    });
    let mut builder = gql::Schema::builder();
    builder.add_type(gql::ObjectType::new("Query").field(a).field(b).into());
    let schema = builder.query("Query").build().expect("valid schema");

    let response = futures::executor::block_on(gql::execute(
        &schema,
        gql::Request::new("{ a b }"),
        &gql::ExecutionOptions::default().max_concurrency(2),
    ));
    let data = response
        .data
        .as_ref()
        .and_then(gql::Value::as_object)
        .expect("data");
    assert_eq!(data.get("a").and_then(gql::Value::as_str), Some("a"));
    assert_eq!(data.get("b").and_then(gql::Value::as_str), Some("b"));
}
