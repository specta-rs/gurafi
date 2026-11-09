//! Independent, black-box conformance audit against the GraphQL October 2021 specification.
//!
//! Every test here uses only the crate's public API. Tests whose names start with `spec_`
//! assert a specific normative requirement; several of them are expected to expose deviations
//! found while auditing this branch. `coverage_` tests record behaviour that already conforms.

use std::sync::{
    Arc,
    atomic::{AtomicUsize, Ordering},
};

use futures::StreamExt;
use futures::executor::block_on;
use gql::{
    DirectiveDefinition, EnumType, ExecutionOptions, Field, GraphQLError, InputObjectType,
    InputValue, InterfaceType, Object, ObjectType, Request, Response, ScalarType, Schema, TypeRef,
    UnionType, ValidationOptions, Value,
    ast::{self, DirectiveLocation},
};

// ---------------------------------------------------------------------------------------------
// Helpers
// ---------------------------------------------------------------------------------------------

fn run(schema: &Schema, query: &str) -> Response {
    block_on(gql::execute(
        schema,
        Request::new(query),
        &ExecutionOptions::default(),
    ))
}

fn run_vars(schema: &Schema, query: &str, variables: Object) -> Response {
    block_on(gql::execute(
        schema,
        Request::new(query).variables(&variables),
        &ExecutionOptions::default(),
    ))
}

fn run_named(schema: &Schema, query: &str, name: &str) -> Response {
    block_on(gql::execute(
        schema,
        Request::new(query).operation_name(name),
        &ExecutionOptions::default(),
    ))
}

fn object(fields: impl IntoIterator<Item = (&'static str, Value)>) -> Value {
    Value::object(Object::from_fields(fields))
}

fn result_field<'a>(response: &'a Response, key: &str) -> &'a Value {
    response
        .data
        .as_ref()
        .and_then(Value::as_object)
        .and_then(|object| object.get(key))
        .unwrap_or_else(|| panic!("response has no field {key}: {response:?}"))
}

fn get<'a>(value: &'a Value, key: &str) -> &'a Value {
    value
        .as_object()
        .and_then(|object| object.get(key))
        .unwrap_or_else(|| panic!("value has no field {key}: {value:?}"))
}

#[track_caller]
fn expect_no_errors(response: &Response) {
    assert!(response.errors.is_empty(), "{:?}", response.errors);
}

fn path_of(error: &GraphQLError) -> Vec<String> {
    error
        .path()
        .map(|path| path.iter().map(ToString::to_string).collect())
        .unwrap_or_default()
}

fn validate_errors(schema: &Schema, query: &str) -> Vec<GraphQLError> {
    let document = gql::parser::parse_query(query).expect("audit document parses");
    gql::validate(schema, &document, &ValidationOptions::default())
}

#[track_caller]
fn assert_valid(schema: &Schema, query: &str) {
    let errors = validate_errors(schema, query);
    assert!(
        errors.is_empty(),
        "expected a valid document, got {errors:?}"
    );
}

#[track_caller]
fn assert_invalid(schema: &Schema, query: &str, needle: &str) {
    let errors = validate_errors(schema, query);
    assert!(
        errors.iter().any(|error| error.message().contains(needle)),
        "expected an error containing {needle:?}, got {errors:?}"
    );
}

/// A deterministic, type-preserving rendering of a `Value`, used to observe coerced arguments.
fn describe(value: &Value) -> String {
    match value {
        Value::Null => "null".to_owned(),
        Value::Boolean(value) => format!("bool:{value}"),
        Value::Int(value) => format!("i32:{value}"),
        Value::Float(value) => format!("f64:{value:?}"),
        Value::String(value) => format!("str:{value}"),
        Value::Enum(value) => format!("enum:{value}"),
        Value::List(items) => format!(
            "[{}]",
            items.iter().map(describe).collect::<Vec<_>>().join(",")
        ),
        Value::Object(object) => format!(
            "{{{}}}",
            object
                .iter()
                .map(|(key, value)| format!("{key}:{}", describe(value)))
                .collect::<Vec<_>>()
                .join(",")
        ),
        Value::External(_) => "external".to_owned(),
    }
}

/// A `String!` field that echoes its coerced arguments back as text.
fn echo_field(name: &'static str, args: impl IntoIterator<Item = InputValue>) -> Field {
    let mut field = Field::new(name, TypeRef::string().non_null());
    for argument in args {
        field = field.arg(argument);
    }
    field.resolve(|context| {
        let output = Value::from(describe(&Value::Object(Arc::new(context.args.clone()))));
        Box::pin(async move { Ok(output) })
    })
}

fn sdl(source: &str) -> Schema {
    Schema::from_sdl(source).unwrap_or_else(|error| panic!("audit schema is invalid: {error}"))
}

// ---------------------------------------------------------------------------------------------
// 1. Execution: field collection, fragments, aliases and directives
// ---------------------------------------------------------------------------------------------

#[test]
fn coverage_fragments_inline_fragments_and_duplicate_response_keys_merge() {
    let calls = Arc::new(AtomicUsize::new(0));

    let counter = Arc::clone(&calls);
    let schema =
        Schema::builder()
            .object(ObjectType::new("Query").field(
                Field::new("user", TypeRef::named("User")).resolve(move |_| {
                    counter.fetch_add(1, Ordering::SeqCst);
                    Box::pin(async {
                        Ok(object([
                            ("id", Value::from("1")),
                            ("name", Value::from("Ada")),
                        ]))
                    })
                }),
            ))
            .object(
                ObjectType::new("User")
                    .field(Field::new("id", TypeRef::id().non_null()))
                    .field(Field::new("name", TypeRef::string())),
            )
            .query("Query")
            .build()
            .expect("valid schema");

    let response = run(
        &schema,
        r#"
        query {
          u: user { id }
          u: user { ...NameFields }
          v: user { ... on User { id } ... on User { name } }
        }
        fragment NameFields on User { name }
        "#,
    );

    expect_no_errors(&response);
    assert_eq!(
        result_field(&response, "u"),
        &object([("id", Value::from("1")), ("name", Value::from("Ada"))])
    );
    assert_eq!(
        result_field(&response, "v"),
        &object([("id", Value::from("1")), ("name", Value::from("Ada"))])
    );
    // The two `u` selections share a response key, so they must be resolved once.
    assert_eq!(calls.load(Ordering::SeqCst), 2);
}

#[test]
fn coverage_skip_and_include_apply_to_fields_spreads_and_inline_fragments() {
    let schema = Schema::builder()
        .object(
            ObjectType::new("Query")
                .field(
                    Field::new("a", TypeRef::string().non_null())
                        .resolve(|_| Box::pin(async { Ok(Value::from("a")) })),
                )
                .field(
                    Field::new("b", TypeRef::string().non_null())
                        .resolve(|_| Box::pin(async { Ok(Value::from("b")) })),
                )
                .field(
                    Field::new("c", TypeRef::string().non_null())
                        .resolve(|_| Box::pin(async { Ok(Value::from("c")) })),
                )
                .field(
                    Field::new("d", TypeRef::string().non_null())
                        .resolve(|_| Box::pin(async { Ok(Value::from("d")) })),
                ),
        )
        .query("Query")
        .build()
        .expect("valid schema");

    let query = r#"
        query Q($skipA: Boolean!, $includeB: Boolean!, $includeFragment: Boolean!) {
          a @skip(if: $skipA)
          b @include(if: $includeB)
          ...F @include(if: $includeFragment)
          ... on Query @skip(if: $includeFragment) { c }
        }
        fragment F on Query { d }
    "#;

    let excluded = run_vars(
        &schema,
        query,
        Object::from_fields([
            ("skipA", Value::Boolean(true)),
            ("includeB", Value::Boolean(false)),
            ("includeFragment", Value::Boolean(false)),
        ]),
    );
    expect_no_errors(&excluded);
    assert_eq!(excluded.data, Some(object([("c", Value::from("c"))])));

    let included = run_vars(
        &schema,
        query,
        Object::from_fields([
            ("skipA", Value::Boolean(false)),
            ("includeB", Value::Boolean(true)),
            ("includeFragment", Value::Boolean(true)),
        ]),
    );
    expect_no_errors(&included);
    assert_eq!(
        included.data,
        Some(object([
            ("a", Value::from("a")),
            ("b", Value::from("b")),
            ("d", Value::from("d")),
        ]))
    );
}

#[test]
fn coverage_default_resolver_reads_parent_object_fields() {
    let schema = Schema::builder()
        .object(ObjectType::new("Query").field(Field::new("user", TypeRef::named("User"))))
        .object(
            ObjectType::new("User")
                .field(Field::new("name", TypeRef::string()))
                .field(Field::new("age", TypeRef::int())),
        )
        .query("Query")
        .build()
        .expect("valid schema");

    let response = block_on(gql::execute(
        &schema,
        Request::new("{ user { name age } }"),
        &ExecutionOptions::default().root_value(object([(
            "user",
            object([("name", Value::from("Ada")), ("age", Value::Int(36))]),
        )])),
    ));

    expect_no_errors(&response);
    assert_eq!(
        result_field(&response, "user"),
        &object([("name", Value::from("Ada")), ("age", Value::Int(36))])
    );
}

// ---------------------------------------------------------------------------------------------
// 2. Execution: null propagation, error paths and partial data
// ---------------------------------------------------------------------------------------------

#[test]
fn coverage_nullable_field_error_becomes_null_and_execution_continues() {
    let schema = Schema::builder()
        .object(
            ObjectType::new("Query")
                .field(
                    Field::new("ok", TypeRef::string())
                        .resolve(|_| Box::pin(async { Ok(Value::from("ok")) })),
                )
                .field(
                    Field::new("boom", TypeRef::string())
                        .resolve(|_| Box::pin(async { Err(GraphQLError::new("resolver failed")) })),
                )
                .field(
                    Field::new("after", TypeRef::string())
                        .resolve(|_| Box::pin(async { Ok(Value::from("after")) })),
                ),
        )
        .query("Query")
        .build()
        .expect("valid schema");

    let response = run(&schema, "{ ok boom after }");
    assert_eq!(response.errors.len(), 1, "{:?}", response.errors);
    assert_eq!(response.errors[0].message(), "resolver failed");
    assert_eq!(path_of(&response.errors[0]), vec!["boom"]);
    assert_eq!(
        response.data,
        Some(object([
            ("ok", Value::from("ok")),
            ("boom", Value::Null),
            ("after", Value::from("after")),
        ]))
    );
}

#[test]
fn coverage_non_null_field_error_nulls_the_nullable_parent() {
    let schema = Schema::builder()
        .object(
            ObjectType::new("Query").field(
                Field::new("parent", TypeRef::named("Parent"))
                    .resolve(|_| Box::pin(async { Ok(object([("sibling", Value::from("s"))])) })),
            ),
        )
        .object(
            ObjectType::new("Parent")
                .field(
                    Field::new("required", TypeRef::string().non_null())
                        .resolve(|_| Box::pin(async { Err(GraphQLError::new("child failed")) })),
                )
                .field(Field::new("sibling", TypeRef::string())),
        )
        .query("Query")
        .build()
        .expect("valid schema");

    let response = run(&schema, "{ parent { required sibling } }");
    assert_eq!(response.errors.len(), 1, "{:?}", response.errors);
    assert_eq!(path_of(&response.errors[0]), vec!["parent", "required"]);
    assert_eq!(result_field(&response, "parent"), &Value::Null);
}

#[test]
fn coverage_list_item_errors_use_index_paths_and_null_items() {
    let schema = Schema::builder()
        .object(ObjectType::new("Query").field(
            Field::new("users", TypeRef::named("User").list()).resolve(|_| {
                Box::pin(async {
                    Ok(Value::List(vec![
                        object([("id", Value::from("1")), ("name", Value::from("a"))]),
                        object([("id", Value::from("2"))]),
                        object([("id", Value::from("3")), ("name", Value::from("c"))]),
                    ]))
                })
            }),
        ))
        .object(
            ObjectType::new("User")
                .field(Field::new("id", TypeRef::id().non_null()))
                .field(Field::new("name", TypeRef::string().non_null())),
        )
        .query("Query")
        .build()
        .expect("valid schema");

    let response = run(&schema, "{ users: users { id name } }");
    assert_eq!(response.errors.len(), 1, "{:?}", response.errors);
    assert_eq!(path_of(&response.errors[0]), vec!["users", "1", "name"]);

    let users = result_field(&response, "users")
        .as_list()
        .expect("users is a list");
    assert_eq!(users.len(), 3);
    assert_eq!(
        users[0],
        object([("id", Value::from("1")), ("name", Value::from("a"))])
    );
    assert_eq!(users[1], Value::Null);
    assert_eq!(
        users[2],
        object([("id", Value::from("3")), ("name", Value::from("c"))])
    );
}

#[test]
fn coverage_non_null_list_items_null_the_whole_list() {
    let schema = Schema::builder()
        .object(ObjectType::new("Query").field(
            Field::new("users", TypeRef::named("User").non_null().list()).resolve(|_| {
                Box::pin(async {
                    Ok(Value::List(vec![
                        object([("id", Value::from("1"))]),
                        object([("id", Value::from("2"))]),
                    ]))
                })
            }),
        ))
        .object(
            ObjectType::new("User")
                .field(Field::new("id", TypeRef::id().non_null()))
                .field(Field::new("name", TypeRef::string().non_null())),
        )
        .query("Query")
        .build()
        .expect("valid schema");

    let response = run(&schema, "{ users { id name } }");
    assert_eq!(response.errors.len(), 1, "{:?}", response.errors);
    assert_eq!(path_of(&response.errors[0]), vec!["users", "0", "name"]);
    // The list field itself is nullable, so the error stops here.
    assert_eq!(result_field(&response, "users"), &Value::Null);
}

#[test]
fn coverage_non_null_list_error_bubbles_to_data_null() {
    let schema = Schema::builder()
        .object(ObjectType::new("Query").field(
            Field::new("users", TypeRef::named("User").non_null().list().non_null()).resolve(
                |_| Box::pin(async { Ok(Value::List(vec![object([("id", Value::from("1"))])])) }),
            ),
        ))
        .object(
            ObjectType::new("User")
                .field(Field::new("id", TypeRef::id().non_null()))
                .field(Field::new("name", TypeRef::string().non_null())),
        )
        .query("Query")
        .build()
        .expect("valid schema");

    let response = run(&schema, "{ users { id name } }");
    assert_eq!(response.errors.len(), 1, "{:?}", response.errors);
    assert_eq!(path_of(&response.errors[0]), vec!["users", "0", "name"]);
    assert_eq!(response.data, Some(Value::Null));
}

#[test]
fn coverage_every_list_item_error_is_reported() {
    let schema = Schema::builder()
        .object(ObjectType::new("Query").field(
            Field::new("users", TypeRef::named("User").list()).resolve(|_| {
                Box::pin(async {
                    Ok(Value::List(vec![
                        object([("id", Value::from("1"))]),
                        object([("id", Value::from("2"))]),
                        object([("id", Value::from("3"))]),
                    ]))
                })
            }),
        ))
        .object(
            ObjectType::new("User")
                .field(Field::new("id", TypeRef::id().non_null()))
                .field(Field::new("name", TypeRef::string().non_null())),
        )
        .query("Query")
        .build()
        .expect("valid schema");

    let response = run(&schema, "{ users { id name } }");
    assert_eq!(response.errors.len(), 3, "{:?}", response.errors);
    assert_eq!(path_of(&response.errors[0]), vec!["users", "0", "name"]);
    assert_eq!(path_of(&response.errors[1]), vec!["users", "1", "name"]);
    assert_eq!(path_of(&response.errors[2]), vec!["users", "2", "name"]);
    assert_eq!(
        result_field(&response, "users"),
        &Value::List(vec![Value::Null, Value::Null, Value::Null])
    );
}

#[test]
fn coverage_nested_non_null_error_bubbles_through_object_chain() {
    let schema = Schema::builder()
        .object(ObjectType::new("Query").field(Field::new("parent", TypeRef::named("Parent"))))
        .object(
            ObjectType::new("Parent")
                .field(Field::new("child", TypeRef::named("Child").non_null()))
                .field(Field::new("other", TypeRef::string())),
        )
        .object(ObjectType::new("Child").field(Field::new("value", TypeRef::string().non_null())))
        .query("Query")
        .build()
        .expect("valid schema");

    let response = block_on(gql::execute(
        &schema,
        Request::new("{ parent { child { value } other } }"),
        &ExecutionOptions::default().root_value(object([(
            "parent",
            object([("child", object([])), ("other", Value::from("x"))]),
        )])),
    ));

    assert_eq!(response.errors.len(), 1, "{:?}", response.errors);
    assert_eq!(
        path_of(&response.errors[0]),
        vec!["parent", "child", "value"]
    );
    assert_eq!(result_field(&response, "parent"), &Value::Null);
}

#[test]
fn coverage_multiple_errors_keep_partial_data() {
    let schema = Schema::builder()
        .object(
            ObjectType::new("Query")
                .field(
                    Field::new("a", TypeRef::string())
                        .resolve(|_| Box::pin(async { Err(GraphQLError::new("a failed")) })),
                )
                .field(
                    Field::new("b", TypeRef::string())
                        .resolve(|_| Box::pin(async { Ok(Value::from("b")) })),
                )
                .field(
                    Field::new("c", TypeRef::int())
                        .resolve(|_| Box::pin(async { Err(GraphQLError::new("c failed")) })),
                ),
        )
        .query("Query")
        .build()
        .expect("valid schema");

    let response = run(&schema, "{ a b c }");
    assert_eq!(response.errors.len(), 2, "{:?}", response.errors);
    assert_eq!(
        response.data,
        Some(object([
            ("a", Value::Null),
            ("b", Value::from("b")),
            ("c", Value::Null),
        ]))
    );
}

#[test]
fn coverage_nested_list_null_propagation_reports_index_paths() {
    let schema = Schema::builder()
        .object(
            ObjectType::new("Query")
                .field(
                    Field::new(
                        "matrix",
                        TypeRef::string().non_null().list().non_null().list(),
                    )
                    .resolve(|_| {
                        Box::pin(async {
                            Ok(Value::List(vec![Value::List(vec![
                                Value::from("a"),
                                Value::Null,
                            ])]))
                        })
                    }),
                )
                .field(
                    Field::new("loose", TypeRef::string().list().list()).resolve(|_| {
                        Box::pin(async {
                            Ok(Value::List(vec![Value::List(vec![
                                Value::from("a"),
                                Value::Null,
                            ])]))
                        })
                    }),
                ),
        )
        .query("Query")
        .build()
        .expect("valid schema");

    let response = run(&schema, "{ matrix loose }");
    assert_eq!(response.errors.len(), 1, "{:?}", response.errors);
    assert_eq!(path_of(&response.errors[0]), vec!["matrix", "0", "1"]);
    assert_eq!(result_field(&response, "matrix"), &Value::Null);
    // Nullable strings accept null elements without an error.
    assert_eq!(
        result_field(&response, "loose"),
        &Value::List(vec![Value::List(vec![Value::from("a"), Value::Null])])
    );
}

#[test]
fn coverage_all_root_fields_skipped_yields_an_empty_object() {
    let schema = Schema::builder()
        .object(
            ObjectType::new("Query").field(
                Field::new("a", TypeRef::string())
                    .resolve(|_| Box::pin(async { Ok(Value::from("a")) })),
            ),
        )
        .query("Query")
        .build()
        .expect("valid schema");

    let response = run_vars(
        &schema,
        "query Q($skip: Boolean!) { a @skip(if: $skip) }",
        Object::from_fields([("skip", Value::Boolean(true))]),
    );
    expect_no_errors(&response);
    assert_eq!(response.data, Some(Value::object(Object::new())));
}

#[test]
fn coverage_aliases_allow_the_same_field_with_different_arguments() {
    let schema = Schema::builder()
        .object(
            ObjectType::new("Query").field(
                Field::new("f", TypeRef::int())
                    .arg(InputValue::new("a", TypeRef::int()))
                    .resolve(|context| {
                        let value = context.args.get("a").and_then(Value::as_i32).unwrap_or(-1);
                        Box::pin(async move { Ok(Value::Int(value)) })
                    }),
            ),
        )
        .query("Query")
        .build()
        .expect("valid schema");

    let response = run(&schema, "{ one: f(a: 1) two: f(a: 2) }");
    expect_no_errors(&response);
    assert_eq!(result_field(&response, "one"), &Value::Int(1));
    assert_eq!(result_field(&response, "two"), &Value::Int(2));
}

#[test]
fn coverage_result_coercion_failures_are_field_errors() {
    let schema = Schema::builder()
        .enumeration(EnumType::new("Color").value("RED"))
        .object(
            ObjectType::new("Query")
                .field(
                    Field::new("not_a_list", TypeRef::string().list())
                        .resolve(|_| Box::pin(async { Ok(Value::from("not a list")) })),
                )
                .field(
                    Field::new("not_an_object", TypeRef::named("Thing"))
                        .resolve(|_| Box::pin(async { Ok(Value::Int(1)) })),
                )
                .field(
                    Field::new("bad_enum", TypeRef::named("Color"))
                        .resolve(|_| Box::pin(async { Ok(Value::enumeration("NOPE")) })),
                )
                .field(
                    Field::new("bad_scalar", TypeRef::int())
                        .resolve(|_| Box::pin(async { Ok(Value::from("nope")) })),
                ),
        )
        .object(ObjectType::new("Thing").field(Field::new("x", TypeRef::string())))
        .query("Query")
        .build()
        .expect("valid schema");

    let response = run(
        &schema,
        "{ not_a_list not_an_object { x } bad_enum bad_scalar }",
    );
    assert_eq!(response.errors.len(), 4, "{:?}", response.errors);
    assert_eq!(
        response.data,
        Some(object([
            ("not_a_list", Value::Null),
            ("not_an_object", Value::Null),
            ("bad_enum", Value::Null),
            ("bad_scalar", Value::Null),
        ]))
    );
}

#[test]
fn coverage_typename_is_resolved_per_union_list_item() {
    // Resolve each list item's concrete type by its id prefix with `is_type_of` checks.
    let schema = Schema::builder()
        .union(UnionType::new("Search").member("User").member("Post"))
        .object(
            ObjectType::new("User")
                .field(Field::new("id", TypeRef::id().non_null()))
                .is_type_of(|value| {
                    value
                        .as_object()
                        .and_then(|object| object.get("id"))
                        .and_then(Value::as_str)
                        .is_some_and(|id| id.starts_with('u'))
                }),
        )
        .object(
            ObjectType::new("Post")
                .field(Field::new("id", TypeRef::id().non_null()))
                .is_type_of(|value| {
                    value
                        .as_object()
                        .and_then(|object| object.get("id"))
                        .and_then(Value::as_str)
                        .is_some_and(|id| id.starts_with('p'))
                }),
        )
        .object(
            ObjectType::new("Query").field(
                Field::new(
                    "search",
                    TypeRef::named("Search").non_null().list().non_null(),
                )
                .resolve(|_| {
                    Box::pin(async {
                        Ok(Value::List(vec![
                            object([("id", Value::from("u1"))]),
                            object([("id", Value::from("p1"))]),
                        ]))
                    })
                }),
            ),
        )
        .query("Query")
        .build()
        .expect("valid schema");

    let response = run(&schema, "{ search { __typename ... on User { id } } }");
    expect_no_errors(&response);
    let items = result_field(&response, "search")
        .as_list()
        .expect("search list");
    assert_eq!(get(&items[0], "__typename"), &Value::from("User"));
    assert_eq!(get(&items[1], "__typename"), &Value::from("Post"));
}

// ---------------------------------------------------------------------------------------------
// 3. Execution: operations, mutations and root values
// ---------------------------------------------------------------------------------------------

#[test]
fn coverage_operation_selection_follows_the_specification() {
    let schema = sdl(r#"
        schema { query: Query mutation: Mutation }
        type Query { q: String }
        type Mutation { m: String }
        "#);

    let ambiguous = run(&schema, "query A { q } mutation B { m }");
    assert_eq!(ambiguous.data, None);
    assert_eq!(ambiguous.errors.len(), 1);
    assert!(ambiguous.errors[0].message().contains("operation name"));

    let selected = run_named(&schema, "query A { q } mutation B { m }", "B");
    assert_eq!(selected.data, Some(object([("m", Value::Null)])));

    let unknown = run_named(&schema, "query A { q } mutation B { m }", "C");
    assert_eq!(unknown.data, None);
    assert_eq!(unknown.errors.len(), 1);
    assert!(unknown.errors[0].message().contains("Unknown operation"));
}

#[test]
fn coverage_mutations_execute_in_document_order() {
    let order = Arc::new(std::sync::Mutex::new(Vec::<String>::new()));

    let builder = Schema::builder();
    let mut mutation = ObjectType::new("Mutation");
    for name in ["first", "second", "third"] {
        let log = Arc::clone(&order);
        mutation = mutation.field(Field::new(name, TypeRef::string()).resolve(move |_| {
            log.lock().expect("order lock").push(name.to_owned());
            Box::pin(async move { Ok(Value::from(name)) })
        }));
    }

    let schema = builder
        .object(ObjectType::new("Query").field(Field::new("q", TypeRef::string())))
        .object(mutation)
        .query("Query")
        .mutation("Mutation")
        .build()
        .expect("valid schema");

    let response = run(&schema, "mutation { third first second }");
    expect_no_errors(&response);
    assert_eq!(
        *order.lock().expect("order lock"),
        vec!["third".to_owned(), "first".to_owned(), "second".to_owned()]
    );
}

#[test]
fn coverage_root_value_is_passed_to_root_fields() {
    let schema = Schema::builder()
        .object(ObjectType::new("Query").field(Field::new("hello", TypeRef::string())))
        .query("Query")
        .build()
        .expect("valid schema");

    let from_object = block_on(gql::execute(
        &schema,
        Request::new("{ hello }"),
        &ExecutionOptions::default().root_value(object([("hello", Value::from("world"))])),
    ));
    expect_no_errors(&from_object);
    assert_eq!(result_field(&from_object, "hello"), &Value::from("world"));

    let from_null = block_on(gql::execute(
        &schema,
        Request::new("{ hello }"),
        &ExecutionOptions::default(),
    ));
    expect_no_errors(&from_null);
    assert_eq!(result_field(&from_null, "hello"), &Value::Null);
}

// ---------------------------------------------------------------------------------------------
// 4. Execution: __typename, interfaces and unions
// ---------------------------------------------------------------------------------------------

fn abstract_schema(resolved: &'static str) -> Schema {
    let schema = Schema::builder()
        .object(ObjectType::new("Query").field(Field::new("node", TypeRef::named("Node"))))
        .object(
            ObjectType::new("User")
                .implements("Node")
                .field(Field::new("id", TypeRef::id().non_null()))
                .field(Field::new("name", TypeRef::string())),
        )
        .object(
            ObjectType::new("Admin")
                .implements("Node")
                .field(Field::new("id", TypeRef::id().non_null())),
        )
        .interface(
            InterfaceType::new("Node")
                .field(Field::new("id", TypeRef::id().non_null()))
                .resolve_type(move |_, _| Ok(resolved.into())),
        )
        .query("Query")
        .build()
        .expect("valid schema");
    schema
}

#[test]
fn coverage_typename_works_on_objects_interfaces_and_unions() {
    let user = object([("id", Value::from("1")), ("name", Value::from("Ada"))]);
    let schema = Schema::builder()
        .object(
            ObjectType::new("Query")
                .field(
                    Field::new("user", TypeRef::named("User")).resolve(move |_| {
                        let user = user.clone();
                        Box::pin(async move { Ok(user) })
                    }),
                )
                .field(
                    Field::new("node", TypeRef::named("Node"))
                        .resolve(|_| Box::pin(async { Ok(Value::from("placeholder")) })),
                )
                .field(
                    Field::new("search", TypeRef::named("Search"))
                        .resolve(|_| Box::pin(async { Ok(Value::from("placeholder")) })),
                )
                .field(Field::new("user2", TypeRef::named("User")).resolve(|_| {
                    Box::pin(async {
                        Ok(object([
                            ("id", Value::from("1")),
                            ("name", Value::from("Ada")),
                        ]))
                    })
                })),
        )
        .object(
            ObjectType::new("User")
                .implements("Node")
                .field(Field::new("id", TypeRef::id().non_null()))
                .field(Field::new("name", TypeRef::string())),
        )
        .interface(
            InterfaceType::new("Node")
                .field(Field::new("id", TypeRef::id().non_null()))
                .resolve_type(|_, _| Ok("User".into())),
        )
        .union(
            UnionType::new("Search")
                .member("User")
                .resolve_type(|_, _| Ok("User".into())),
        )
        .query("Query")
        .build()
        .expect("valid schema");

    let response = block_on(gql::execute(
        &schema,
        Request::new("query { t: __typename user { __typename } u: user2 { kind: __typename } }"),
        &ExecutionOptions::default().root_value(object([
            ("user", object([("id", Value::from("1"))])),
            ("node", object([("id", Value::from("1"))])),
            ("search", object([("id", Value::from("1"))])),
        ])),
    ));

    expect_no_errors(&response);
    assert_eq!(result_field(&response, "t"), &Value::from("Query"));
    assert_eq!(
        get(result_field(&response, "user"), "__typename"),
        &Value::from("User")
    );
    assert_eq!(
        get(result_field(&response, "u"), "kind"),
        &Value::from("User")
    );
}

#[test]
fn coverage_abstract_type_resolution_rejects_non_possible_types() {
    let schema = abstract_schema("NotAnImplementation");
    let response = block_on(gql::execute(
        &schema,
        Request::new("{ node { id } }"),
        &ExecutionOptions::default()
            .root_value(object([("node", object([("id", Value::from("1"))]))])),
    ));

    assert_eq!(response.errors.len(), 1, "{:?}", response.errors);
    assert!(
        response.errors[0].message().contains("not a possible type"),
        "{}",
        response.errors[0]
    );
    assert_eq!(result_field(&response, "node"), &Value::Null);
}

#[test]
fn coverage_abstract_type_requires_a_composite_runtime_value() {
    let schema = Schema::builder()
        .object(
            ObjectType::new("Query").field(
                Field::new("node", TypeRef::named("Node"))
                    .resolve(|_| Box::pin(async { Ok(Value::Int(5)) })),
            ),
        )
        .object(
            ObjectType::new("User")
                .implements("Node")
                .field(Field::new("id", TypeRef::id().non_null())),
        )
        .interface(
            InterfaceType::new("Node")
                .field(Field::new("id", TypeRef::id().non_null()))
                .resolve_type(|_, _| Ok("User".into())),
        )
        .query("Query")
        .build()
        .expect("valid schema");

    let response = run(&schema, "{ node { id } }");
    assert_eq!(response.errors.len(), 1, "{:?}", response.errors);
    assert_eq!(result_field(&response, "node"), &Value::Null);
}

// ---------------------------------------------------------------------------------------------
// 5. Input coercion
// ---------------------------------------------------------------------------------------------

#[test]
fn spec_single_value_coerces_to_a_list_for_list_typed_variables() {
    // October 2021, 3.11 "List": input coercion turns a non-list, non-null value into a
    // single-item list, recursively.
    let schema = Schema::builder()
        .object(ObjectType::new("Query").field(echo_field(
            "sum",
            [InputValue::new("values", TypeRef::int().list())],
        )))
        .query("Query")
        .build()
        .expect("valid schema");

    let response = run_vars(
        &schema,
        "query Q($values: [Int]) { sum(values: $values) }",
        Object::from_fields([("values", Value::Int(4))]),
    );

    expect_no_errors(&response);
    assert_eq!(
        result_field(&response, "sum"),
        &Value::from("{values:[i32:4]}")
    );
}

#[test]
fn spec_single_value_coerces_to_a_list_inside_input_object_variables() {
    // Same list coercion rule applies to nested input object fields supplied through variables.
    let schema = Schema::builder()
        .input_object(
            InputObjectType::new("Filter")
                .field(InputValue::new("tags", TypeRef::string().non_null().list())),
        )
        .object(ObjectType::new("Query").field(echo_field(
            "f",
            [InputValue::new("filter", TypeRef::named("Filter"))],
        )))
        .query("Query")
        .build()
        .expect("valid schema");

    let response = run_vars(
        &schema,
        "query Q($filter: Filter) { f(filter: $filter) }",
        Object::from_fields([("filter", object([("tags", Value::from("solo"))]))]),
    );

    expect_no_errors(&response);
    assert_eq!(
        result_field(&response, "f"),
        &Value::from("{filter:{tags:[str:solo]}}")
    );
}

#[test]
fn coverage_variable_default_list_accepts_a_single_value() {
    let schema = Schema::builder()
        .object(ObjectType::new("Query").field(echo_field(
            "sum",
            [InputValue::new("values", TypeRef::int().list())],
        )))
        .query("Query")
        .build()
        .expect("valid schema");

    let response = run(
        &schema,
        "query Q($values: [Int] = 1) { sum(values: $values) }",
    );
    expect_no_errors(&response);
    assert_eq!(
        result_field(&response, "sum"),
        &Value::from("{values:[i32:1]}")
    );
}

#[test]
fn coverage_variable_defaults_and_location_defaults() {
    let schema =
        Schema::builder()
            .object(
                ObjectType::new("Query")
                    .field(echo_field(
                        "n",
                        [
                            InputValue::new("a", TypeRef::int().non_null()),
                            InputValue::new("b", TypeRef::string()).default_value("def"),
                            InputValue::new("c", TypeRef::int()).default_value(Value::Int(9)),
                        ],
                    ))
                    .field(echo_field(
                        "m",
                        [InputValue::new("a", TypeRef::int().non_null())
                            .default_value(Value::Int(7))],
                    )),
            )
            .query("Query")
            .build()
            .expect("valid schema");

    // A non-null default on a nullable variable satisfies a non-null argument.
    let from_variable_default = run(&schema, "query Q($x: Int = 3) { n(a: $x) }");
    expect_no_errors(&from_variable_default);
    assert_eq!(
        result_field(&from_variable_default, "n"),
        &Value::from("{a:i32:3,b:str:def,c:i32:9}")
    );

    // A default at the argument location satisfies a missing nullable variable.
    let from_argument_default = run(&schema, "query Q($x: Int) { m(a: $x) }");
    expect_no_errors(&from_argument_default);
    assert_eq!(
        result_field(&from_argument_default, "m"),
        &Value::from("{a:i32:7}")
    );

    let provided = run_vars(
        &schema,
        "query Q($x: Int) { m(a: $x) }",
        Object::from_fields([("x", Value::Int(5))]),
    );
    expect_no_errors(&provided);
    assert_eq!(result_field(&provided, "m"), &Value::from("{a:i32:5}"));
}

#[test]
fn coverage_null_is_distinct_from_an_omitted_argument() {
    let schema = Schema::builder()
        .object(ObjectType::new("Query").field(echo_field(
            "g",
            [
                InputValue::new("a", TypeRef::int()).default_value(Value::Int(5)),
                InputValue::new("b", TypeRef::int()),
            ],
        )))
        .query("Query")
        .build()
        .expect("valid schema");

    let omitted = run(&schema, "{ g }");
    expect_no_errors(&omitted);
    assert_eq!(result_field(&omitted, "g"), &Value::from("{a:i32:5}"));

    let explicit_null = run(&schema, "{ g(a: null) }");
    expect_no_errors(&explicit_null);
    assert_eq!(result_field(&explicit_null, "g"), &Value::from("{a:null}"));

    let other_null = run(&schema, "{ g(b: null) }");
    expect_no_errors(&other_null);
    assert_eq!(
        result_field(&other_null, "g"),
        &Value::from("{a:i32:5,b:null}")
    );

    // An explicitly provided null variable overrides the argument default.
    let variable_null = run_vars(
        &schema,
        "query Q($x: Int) { g(a: $x) }",
        Object::from_fields([("x", Value::Null)]),
    );
    expect_no_errors(&variable_null);
    assert_eq!(result_field(&variable_null, "g"), &Value::from("{a:null}"));

    // An omitted variable leaves the argument default in place.
    let variable_omitted = run(&schema, "query Q($x: Int) { g(a: $x) }");
    expect_no_errors(&variable_omitted);
    assert_eq!(
        result_field(&variable_omitted, "g"),
        &Value::from("{a:i32:5}")
    );
}

#[test]
fn coverage_scalar_input_coercion() {
    let schema = Schema::builder()
        .object(
            ObjectType::new("Query")
                .field(echo_field("ints", [InputValue::new("i", TypeRef::int())]))
                .field(echo_field(
                    "floats",
                    [InputValue::new("f", TypeRef::float())],
                ))
                .field(echo_field("ids", [InputValue::new("id", TypeRef::id())])),
        )
        .query("Query")
        .build()
        .expect("valid schema");

    let int_max = run(&schema, "{ ints(i: 2147483647) }");
    expect_no_errors(&int_max);
    assert_eq!(
        result_field(&int_max, "ints"),
        &Value::from("{i:i32:2147483647}")
    );

    let int_min = run(&schema, "{ ints(i: -2147483648) }");
    expect_no_errors(&int_min);
    assert_eq!(
        result_field(&int_min, "ints"),
        &Value::from("{i:i32:-2147483648}")
    );

    assert_invalid(&schema, "{ ints(i: 2147483648) }", "Int");
    assert_invalid(&schema, "{ ints(i: 1.5) }", "Int");
    assert_invalid(&schema, "{ ints(i: \"1\") }", "Int");

    let float_from_int = run(&schema, "{ floats(f: 2) }");
    expect_no_errors(&float_from_int);
    assert_eq!(
        result_field(&float_from_int, "floats"),
        &Value::from("{f:f64:2.0}")
    );

    let id_from_int = run(&schema, "{ ids(id: 42) }");
    expect_no_errors(&id_from_int);
    assert_eq!(
        result_field(&id_from_int, "ids"),
        &Value::from("{id:str:42}")
    );

    let id_from_string = run(&schema, "{ ids(id: \"abc\") }");
    expect_no_errors(&id_from_string);
    assert_eq!(
        result_field(&id_from_string, "ids"),
        &Value::from("{id:str:abc}")
    );

    assert_invalid(&schema, "{ ids(id: 4.0) }", "ID");
    assert_invalid(&schema, "{ ids(id: true) }", "ID");

    // Variables use the same coercion rules.
    let variable_float = run_vars(
        &schema,
        "query Q($f: Float) { floats(f: $f) }",
        Object::from_fields([("f", Value::Int(2))]),
    );
    expect_no_errors(&variable_float);
    assert_eq!(
        result_field(&variable_float, "floats"),
        &Value::from("{f:f64:2.0}")
    );

    let variable_id = run_vars(
        &schema,
        "query Q($id: ID) { ids(id: $id) }",
        Object::from_fields([("id", Value::Int(4))]),
    );
    expect_no_errors(&variable_id);
    assert_eq!(
        result_field(&variable_id, "ids"),
        &Value::from("{id:str:4}")
    );

    let bad_int_variable = run_vars(
        &schema,
        "query Q($i: Int) { ints(i: $i) }",
        Object::from_fields([("i", Value::Float(1.0))]),
    );
    assert_eq!(bad_int_variable.data, None);
    assert_eq!(bad_int_variable.errors.len(), 1);

    let bad_id_variable = run_vars(
        &schema,
        "query Q($id: ID) { ids(id: $id) }",
        Object::from_fields([("id", Value::Float(4.0))]),
    );
    assert_eq!(bad_id_variable.data, None);
    assert_eq!(bad_id_variable.errors.len(), 1);
}

#[test]
fn coverage_enum_input_coercion() {
    let schema = Schema::builder()
        .enumeration(EnumType::new("Color").value("RED").value("GREEN"))
        .object(ObjectType::new("Query").field(echo_field(
            "c",
            [InputValue::new("color", TypeRef::named("Color"))],
        )))
        .query("Query")
        .build()
        .expect("valid schema");

    let literal = run(&schema, "{ c(color: RED) }");
    expect_no_errors(&literal);
    assert_eq!(
        result_field(&literal, "c"),
        &Value::from("{color:enum:RED}")
    );

    assert_invalid(&schema, "{ c(color: \"RED\") }", "Color");
    assert_invalid(&schema, "{ c(color: BLUE) }", "BLUE");

    let variable = run_vars(
        &schema,
        "query Q($color: Color) { c(color: $color) }",
        Object::from_fields([("color", Value::from("GREEN"))]),
    );
    expect_no_errors(&variable);
    assert_eq!(
        result_field(&variable, "c"),
        &Value::from("{color:enum:GREEN}")
    );

    let unknown_variable = run_vars(
        &schema,
        "query Q($color: Color) { c(color: $color) }",
        Object::from_fields([("color", Value::from("BLUE"))]),
    );
    assert_eq!(unknown_variable.data, None);
    assert_eq!(unknown_variable.errors.len(), 1);
}

#[test]
fn coverage_input_object_coercion() {
    let schema = Schema::builder()
        .input_object(
            InputObjectType::new("Filter")
                .field(InputValue::new("limit", TypeRef::int().non_null()))
                .field(InputValue::new("tags", TypeRef::string().non_null().list())),
        )
        .object(ObjectType::new("Query").field(echo_field(
            "f",
            [InputValue::new("filter", TypeRef::named("Filter"))],
        )))
        .query("Query")
        .build()
        .expect("valid schema");

    // A single value is coerced into a one-element list.
    let literal = run(&schema, "{ f(filter: {limit: 1, tags: \"a\"}) }");
    expect_no_errors(&literal);
    assert_eq!(
        result_field(&literal, "f"),
        &Value::from("{filter:{limit:i32:1,tags:[str:a]}}")
    );

    assert_invalid(
        &schema,
        "{ f(filter: {limit: 1, nope: true}) }",
        "not defined by type",
    );
    assert_invalid(
        &schema,
        "{ f(filter: {limit: 1, limit: 2}) }",
        "only one input field",
    );
    assert_invalid(&schema, "{ f(filter: {tags: [\"a\"]}) }", "required");
    assert_invalid(
        &schema,
        "{ f(filter: {limit: 1, tags: [null]}) }",
        "String!",
    );

    let variable = run_vars(
        &schema,
        "query Q($filter: Filter) { f(filter: $filter) }",
        Object::from_fields([(
            "filter",
            object([
                ("limit", Value::Int(1)),
                (
                    "tags",
                    Value::List(vec![Value::from("a"), Value::from("b")]),
                ),
            ]),
        )]),
    );
    expect_no_errors(&variable);
    assert_eq!(
        result_field(&variable, "f"),
        &Value::from("{filter:{limit:i32:1,tags:[str:a,str:b]}}")
    );

    let missing = run_vars(
        &schema,
        "query Q($filter: Filter) { f(filter: $filter) }",
        Object::from_fields([("filter", object([("limit", Value::Int(1))]))]),
    );
    expect_no_errors(&missing);
    // `tags` is optional, so it is simply absent from the coerced value.
    assert_eq!(
        result_field(&missing, "f"),
        &Value::from("{filter:{limit:i32:1}}")
    );

    let missing_required = run_vars(
        &schema,
        "query Q($filter: Filter) { f(filter: $filter) }",
        Object::from_fields([("filter", object([("tags", Value::List(Vec::new()))]))]),
    );
    assert_eq!(missing_required.data, None);
    assert_eq!(missing_required.errors.len(), 1);
}

#[test]
fn coverage_custom_scalar_coercion() {
    let even = ScalarType::new("Even")
        .parse_value(|value| match value {
            Value::Int(value) if value % 2 == 0 => Ok(Value::Int(*value)),
            _ => Err("Even must be an even integer.".to_owned()),
        })
        .parse_literal(|value| match value {
            ast::Value::Int(value) => match i32::try_from(*value) {
                Ok(value) if value % 2 == 0 => Ok(Value::Int(value)),
                _ => Err("Even must be an even integer.".to_owned()),
            },
            _ => Err("Even must be an even integer.".to_owned()),
        })
        .serialize(|value| match value {
            Value::Int(value) => Ok(Value::Int(*value)),
            _ => Err("Even must serialize to an integer.".to_owned()),
        });

    let schema = Schema::builder()
        .scalar(even)
        .object(ObjectType::new("Query").field(echo_field(
            "e",
            [InputValue::new("x", TypeRef::named("Even"))],
        )))
        .query("Query")
        .build()
        .expect("valid schema");

    let literal = run(&schema, "{ e(x: 2) }");
    expect_no_errors(&literal);
    assert_eq!(result_field(&literal, "e"), &Value::from("{x:i32:2}"));

    assert_invalid(&schema, "{ e(x: 3) }", "even integer");

    let variable = run_vars(
        &schema,
        "query Q($x: Even) { e(x: $x) }",
        Object::from_fields([("x", Value::Int(4))]),
    );
    expect_no_errors(&variable);
    assert_eq!(result_field(&variable, "e"), &Value::from("{x:i32:4}"));

    let bad_variable = run_vars(
        &schema,
        "query Q($x: Even) { e(x: $x) }",
        Object::from_fields([("x", Value::Int(3))]),
    );
    assert_eq!(bad_variable.data, None);
    assert_eq!(bad_variable.errors.len(), 1);
}

#[test]
fn coverage_argument_and_input_field_defaults() {
    let schema = Schema::builder()
        .input_object(
            InputObjectType::new("Bag")
                .field(InputValue::new("a", TypeRef::int()).default_value(Value::Int(1)))
                .field(InputValue::new("b", TypeRef::int().non_null())),
        )
        .object(
            ObjectType::new("Query").field(echo_field(
                "f",
                [InputValue::new("bag", TypeRef::named("Bag"))
                    .default_value(object([("a", Value::Int(1)), ("b", Value::Int(2))]))],
            )),
        )
        .query("Query")
        .build()
        .expect("valid schema");

    let from_argument_default = run(&schema, "{ f }");
    expect_no_errors(&from_argument_default);
    assert_eq!(
        result_field(&from_argument_default, "f"),
        &Value::from("{bag:{a:i32:1,b:i32:2}}")
    );

    let from_input_field_default = run(&schema, "{ f(bag: {b: 3}) }");
    expect_no_errors(&from_input_field_default);
    assert_eq!(
        result_field(&from_input_field_default, "f"),
        &Value::from("{bag:{a:i32:1,b:i32:3}}")
    );

    assert_invalid(&schema, "{ f(bag: {a: 4}) }", "required");
    assert_invalid(&schema, "{ f(bag: {b: null}) }", "Int!");
}

#[test]
fn spec_argument_default_values_are_coerced_to_the_argument_type() {
    // A default value is part of the schema and must undergo input coercion before it reaches a
    // resolver. ID input coercion turns the integer 1 into the string "1".
    let schema = Schema::builder()
        .object(ObjectType::new("Query").field(echo_field(
            "f",
            [InputValue::new("id", TypeRef::id()).default_value(Value::Int(1))],
        )))
        .query("Query")
        .build()
        .expect("valid schema");

    let response = run(&schema, "{ f }");
    expect_no_errors(&response);
    assert_eq!(result_field(&response, "f"), &Value::from("{id:str:1}"));
}

#[test]
fn spec_argument_default_object_applies_nested_input_field_defaults() {
    // Coercing the object literal `{ b: 2 }` as `Bag` produces `{ a: 1, b: 2 }` because `a` has
    // a default. October 2021 3.10 "Input Coercion".
    let schema = Schema::builder()
        .input_object(
            InputObjectType::new("Bag2")
                .field(InputValue::new("a", TypeRef::int()).default_value(Value::Int(1)))
                .field(InputValue::new("b", TypeRef::int().non_null())),
        )
        .object(
            ObjectType::new("Query").field(echo_field(
                "f",
                [InputValue::new("bag", TypeRef::named("Bag2"))
                    .default_value(object([("b", Value::Int(2))]))],
            )),
        )
        .query("Query")
        .build()
        .expect("valid schema");

    let response = run(&schema, "{ f }");
    expect_no_errors(&response);
    assert_eq!(
        result_field(&response, "f"),
        &Value::from("{bag:{a:i32:1,b:i32:2}}")
    );
}

#[test]
fn coverage_one_of_input_objects() {
    let schema = Schema::builder()
        .input_object(
            InputObjectType::new("Choice")
                .one_of()
                .field(InputValue::new("a", TypeRef::int()))
                .field(InputValue::new("b", TypeRef::string())),
        )
        .object(ObjectType::new("Query").field(echo_field(
            "f",
            [InputValue::new("choice", TypeRef::named("Choice"))],
        )))
        .query("Query")
        .build()
        .expect("valid schema");

    let valid = run(&schema, "{ f(choice: {a: 1}) }");
    expect_no_errors(&valid);
    assert_eq!(
        result_field(&valid, "f"),
        &Value::from("{choice:{a:i32:1}}")
    );

    let none = run(&schema, "{ f(choice: {}) }");
    assert_eq!(none.data, Some(Value::Null));
    assert_eq!(none.errors.len(), 1);

    let both = run(&schema, "{ f(choice: {a: 1, b: \"x\"}) }");
    assert_eq!(both.data, Some(Value::Null));
    assert_eq!(both.errors.len(), 1);

    let null_value = run(&schema, "{ f(choice: {a: null}) }");
    assert_eq!(null_value.data, Some(Value::Null));
    assert_eq!(null_value.errors.len(), 1);

    let variable = run_vars(
        &schema,
        "query Q($choice: Choice) { f(choice: $choice) }",
        Object::from_fields([("choice", object([("b", Value::from("x"))]))]),
    );
    expect_no_errors(&variable);
    assert_eq!(
        result_field(&variable, "f"),
        &Value::from("{choice:{b:str:x}}")
    );

    let variable_both = run_vars(
        &schema,
        "query Q($choice: Choice) { f(choice: $choice) }",
        Object::from_fields([(
            "choice",
            object([("a", Value::Int(1)), ("b", Value::from("x"))]),
        )]),
    );
    assert_eq!(variable_both.data, None);

    let variable_null = run_vars(
        &schema,
        "query Q($choice: Choice) { f(choice: $choice) }",
        Object::from_fields([("choice", object([("a", Value::Null)]))]),
    );
    assert_eq!(variable_null.data, None);
}

#[test]
fn coverage_overlapping_fields_conflict_across_fragments() {
    let schema = sdl(r#"
        type User { id: ID!, name: String }
        type Query { user: User }
        "#);

    assert_invalid(
        &schema,
        r#"
        { user { ...A ...B } }
        fragment A on User { x: name }
        fragment B on User { x: id }
        "#,
        "conflict",
    );
    assert_valid(
        &schema,
        r#"
        { user { ...A ...B } }
        fragment A on User { id }
        fragment B on User { name }
        "#,
    );
}

#[test]
fn coverage_variables_in_allowed_position_inside_fragments() {
    let schema = sdl(r#"
        type Query { f(n: Int!): String }
        "#);

    assert_invalid(
        &schema,
        "query Q($x: Int) { ...F } fragment F on Query { f(n: $x) }",
        "expecting type \"Int!\"",
    );
    assert_valid(
        &schema,
        "query Q($x: Int!) { ...F } fragment F on Query { f(n: $x) }",
    );
}

#[test]
fn coverage_overlapping_fields_ignore_mutually_exclusive_fragments_at_every_depth() {
    let schema = sdl(r#"
        interface Pet { friend: Pet, name: String }
        type Dog implements Pet { friend: Pet, name: String, bark: Int }
        type Cat implements Pet { friend: Pet, name: String, meow: Int }
        type Query { pet: Pet }
        "#);

    assert_valid(
        &schema,
        "{ pet { ... on Dog { bark } ... on Cat { meow } } }",
    );
    // The same field selected under two exclusive fragments is also fine.
    assert_valid(
        &schema,
        "{ pet { ... on Dog { name } ... on Cat { name } } }",
    );
}

#[test]
fn coverage_unused_fragments_referenced_only_by_unused_fragments() {
    let schema = sdl(r#"type Query { f: String }"#);
    assert_invalid(
        &schema,
        "query Q { f } fragment A on Query { ...B } fragment B on Query { f }",
        "never used",
    );
}

#[test]
fn coverage_overlapping_fields_compare_nested_fragment_selections() {
    let schema = sdl(r#"
        type Child { v: Int, s: String }
        type Parent { child: Child }
        type Query { parent: Parent }
        "#);

    assert_invalid(
        &schema,
        r#"
        { parent { ...A ...B } }
        fragment A on Parent { child { v: v } }
        fragment B on Parent { child { v: s } }
        "#,
        "conflict",
    );
}

#[test]
fn coverage_nested_fragments_expand_without_duplicating_fields() {
    let calls = Arc::new(AtomicUsize::new(0));

    let counter = Arc::clone(&calls);
    let schema =
        Schema::builder()
            .object(ObjectType::new("Query").field(
                Field::new("user", TypeRef::named("User")).resolve(move |_| {
                    counter.fetch_add(1, Ordering::SeqCst);
                    Box::pin(async {
                        Ok(object([
                            ("id", Value::from("1")),
                            ("name", Value::from("Ada")),
                        ]))
                    })
                }),
            ))
            .object(
                ObjectType::new("User")
                    .field(Field::new("id", TypeRef::id().non_null()))
                    .field(Field::new("name", TypeRef::string())),
            )
            .query("Query")
            .build()
            .expect("valid schema");

    let response = run(
        &schema,
        r#"
        { ...Outer }
        fragment Outer on Query { user { ...Inner } }
        fragment Inner on User { id name }
        "#,
    );
    expect_no_errors(&response);
    assert_eq!(
        result_field(&response, "user"),
        &object([("id", Value::from("1")), ("name", Value::from("Ada"))])
    );
    assert_eq!(calls.load(Ordering::SeqCst), 1);
}

// ---------------------------------------------------------------------------------------------
// 6. Validation rules
// ---------------------------------------------------------------------------------------------

fn overlap_schema() -> Schema {
    sdl(r#"
        enum Color { RED GREEN }
        input Point { x: Int, y: Int }
        type Query {
          f(a: Int, b: Int, c: Color, d: [Int], e: Point, s: String): String
          child: Child
        }
        type Child { v: Int, s: String }
        "#)
}

#[test]
fn coverage_overlapping_fields_compare_arguments_order_insensitively() {
    let schema = overlap_schema();

    assert_valid(&schema, "{ f(a: 1, b: 2) f(b: 2, a: 1) }");
    assert_valid(&schema, "{ f(d: [1, 2]) f(d: [1, 2]) }");
    assert_valid(&schema, "{ f(e: {x: 1, y: 2}) f(e: {y: 2, x: 1}) }");
    assert_valid(&schema, "{ f(c: RED) f(c: RED) }");
    assert_valid(&schema, "query Q($x: Int) { f(a: $x) f(a: $x) }");

    assert_invalid(&schema, "{ f(a: 1) f(a: 2) }", "differing arguments");
    assert_invalid(
        &schema,
        "{ f(d: [1, 2]) f(d: [2, 1]) }",
        "differing arguments",
    );
    assert_invalid(&schema, "{ f(c: RED) f(c: GREEN) }", "differing arguments");
    assert_invalid(&schema, "{ f(a: 1) f(a: 1, b: 2) }", "differing arguments");
    assert_invalid(
        &schema,
        "query Q($x: Int) { f(a: $x) f(a: 1) }",
        "differing arguments",
    );
}

#[test]
fn coverage_overlapping_fields_recurse_into_sub_selections() {
    let schema = overlap_schema();

    assert_valid(&schema, "{ child { v: v } child { v: v } }");
    assert_invalid(&schema, "{ child { v: v } child { v: s } }", "conflict");
    assert_invalid(
        &schema,
        "{ x: f(a: 1) { y } x: f(a: 2) { y } }",
        "differing arguments",
    );
}

#[test]
fn coverage_overlapping_fields_ignore_mutually_exclusive_object_types() {
    let schema = sdl(r#"
        type A { v: Int }
        type B { v: String }
        union Search = A | B
        type Query { search: Search }
        "#);

    assert_valid(&schema, "{ search { ... on A { v } ... on B { v } } }");
}

#[test]
fn coverage_overlapping_fields_conflict_when_interfaces_overlap() {
    let schema = sdl(r#"
        interface Readable { value: String! }
        interface Countable { value: String }
        type Measurement implements Readable & Countable { value: String! }
        type Query { m: Measurement }
        "#);

    // Both fragments can apply to the same object, and String! and String have different shapes.
    assert_invalid(
        &schema,
        "{ m { ... on Readable { value } ... on Countable { value } } }",
        "conflicting types",
    );
}

#[test]
fn coverage_variables_in_allowed_position() {
    let schema = sdl(r#"
        input Nested { a: Int! = 1 }
        type Query {
          f(n: Int!): String
          g(xs: [Int!] = [1]): String
          h(n: Int! = 7): String
          k(i: Nested): String
        }
        "#);

    assert_invalid(
        &schema,
        "query Q($x: Int) { f(n: $x) }",
        "used in position expecting type \"Int!\"",
    );
    assert_valid(&schema, "query Q($x: Int = 1) { f(n: $x) }");
    assert_valid(&schema, "query Q($x: Int) { h(n: $x) }");
    assert_valid(&schema, "query Q($x: Int!) { f(n: $x) }");

    assert_invalid(
        &schema,
        "query Q($x: [Int]) { g(xs: $x) }",
        "used in position expecting type \"[Int!]\"",
    );
    assert_valid(&schema, "query Q($x: [Int!]) { g(xs: $x) }");
    assert_valid(&schema, "query Q($x: Int!) { g(xs: [$x]) }");

    // A default on the enclosing *object field* does make it acceptable.
    assert_valid(&schema, "query Q($x: Int) { k(i: {a: $x}) }");
}

#[test]
fn spec_argument_default_does_not_cover_a_list_item_variable() {
    // October 2021 5.8.5 IsVariableUsageAllowed: HasLocationDefaultValue only considers the
    // Argument or ObjectField where the variable is located. A variable inside a ListValue entry
    // has no location default, so an argument default must not make `Int` acceptable at `Int!`.
    let schema = sdl(r#"type Query { g(xs: [Int!] = [1]): String }"#);
    assert_invalid(
        &schema,
        "query Q($x: Int) { g(xs: [$x]) }",
        "used in position expecting type \"Int!\"",
    );
}

#[test]
fn spec_non_null_list_variable_is_allowed_in_a_nullable_list_position() {
    // October 2021 5.8.5 AreTypesCompatible: a non-null variable is unwrapped and compared with
    // the (nullable) location type, so `[Int!]!` may be used where `[Int!]` is expected.
    let schema = sdl(r#"type Query { g(xs: [Int!]): String }"#);
    assert_valid(&schema, "query Q($x: [Int!]!) { g(xs: $x) }");

    let schema = sdl(r#"type Query { users(ids: [ID!]): String }"#);
    assert_valid(&schema, "query Q($ids: [ID!]!) { users(ids: $ids) }");
}

#[test]
fn coverage_unused_variables_and_fragments() {
    let schema = overlap_schema();

    assert_invalid(
        &schema,
        "query Q($unused: Int) { child { v } }",
        "never used",
    );
    assert_valid(
        &schema,
        "query Q($used: Int) { ...F } fragment F on Query { f(a: $used) }",
    );
    assert_invalid(
        &schema,
        "{ child { v } } fragment Unused on Query { f }",
        "never used",
    );
    assert_valid(
        &schema,
        "{ ...Outer } fragment Outer on Query { ...Inner } fragment Inner on Query { child { v } }",
    );
    assert_invalid(&schema, "query Q { f(a: $missing) }", "not defined");
}

#[test]
fn spec_subscription_duplicate_root_fields_group_into_one_entry() {
    // October 2021 5.2.3.1: the grouped field set (which collapses response keys) must have
    // exactly one entry.
    let schema = sdl(r#"
        type Query { q: String }
        type Subscription { a: String, b: String }
        "#);

    assert_valid(&schema, "subscription { a a }");
    assert_valid(&schema, "subscription { x: a x: a }");
    assert_valid(
        &schema,
        "subscription { a ...F } fragment F on Subscription { a }",
    );
}

#[test]
fn spec_subscription_introspection_field_counts_as_a_second_root_entry() {
    let schema = sdl(r#"
        type Query { q: String }
        type Subscription { a: String, b: String }
        "#);

    assert_invalid(
        &schema,
        "subscription { a __typename }",
        "must select only one top-level field",
    );
    assert_invalid(&schema, "subscription { a b }", "must select only one");
}

#[test]
fn coverage_directive_locations_repeatability_and_arguments() {
    let schema = Schema::builder()
        .directive(
            DirectiveDefinition::new("tag")
                .arg(InputValue::new("name", TypeRef::string().non_null()))
                .location(DirectiveLocation::Field)
                .repeatable(),
        )
        .directive(DirectiveDefinition::new("once").location(DirectiveLocation::Field))
        .object(ObjectType::new("Query").field(Field::new("f", TypeRef::string())))
        .query("Query")
        .build()
        .expect("valid schema");

    assert_valid(&schema, "{ f @tag(name: \"a\") @tag(name: \"b\") }");
    assert_invalid(&schema, "{ f @once @once }", "only be used once");
    assert_invalid(&schema, "{ f @once(a: 1) }", "Unknown argument");
    assert_invalid(&schema, "{ f @unknown }", "Unknown directive");
    assert_invalid(&schema, "{ f @skip }", "required");
    assert_invalid(
        &schema,
        "fragment F on Query @include(if: true) { f }",
        "may not be used on FRAGMENT_DEFINITION",
    );
    assert_invalid(&schema, "{ f @include(if: 1) }", "Boolean");
    assert_invalid(
        &schema,
        "query Q @skip(if: true) { f }",
        "may not be used on QUERY",
    );
}

#[test]
fn coverage_values_of_correct_type_edge_cases() {
    let schema = sdl(r#"
        type Query {
          f(i: Int, n: Int!, s: String, b: Boolean, id: ID, l: [Int!]): String
        }
        "#);

    assert_invalid(&schema, "{ f(n: null) }", "Int!");
    assert_invalid(&schema, "{ f(l: [1, null]) }", "Int!");
    assert_invalid(&schema, "{ f(i: 2147483648) }", "Int");
    assert_invalid(&schema, "{ f(i: -2147483649) }", "Int");
    assert_invalid(&schema, "{ f(b: 1) }", "Boolean");
    assert_invalid(&schema, "{ f(id: true) }", "ID");
    assert_invalid(&schema, "{ f(s: RED) }", "String");
    assert_invalid(&schema, "{ f(i: 1.5) }", "Int");
    assert_invalid(&schema, "{ f(n: 1, unknown: 2) }", "Unknown argument");

    // A single value coerces to a one-item list.
    assert_valid(&schema, "{ f(n: 1, l: 1) }");
    assert_valid(&schema, "{ f(n: 1, l: [1, 2]) }");
}

// ---------------------------------------------------------------------------------------------
// 7. Introspection
// ---------------------------------------------------------------------------------------------

#[test]
fn coverage_introspection_wrappers_and_typename() {
    let schema = sdl(r#"
        type Query { users: [User!]!, user: User, maybe: [User!] }
        type User { id: ID!, name: String }
        "#);

    let response = run(
        &schema,
        r#"
        {
          __schema { __typename queryType { __typename name } }
          __type(name: "Query") {
            __typename
            kind
            name
            fields {
              name
              type {
                kind name
                ofType {
                  kind name
                  ofType {
                    kind name
                    ofType {
                      kind name
                      ofType { kind name }
                    }
                  }
                }
              }
            }
          }
        }
        "#,
    );
    expect_no_errors(&response);

    let schema_value = result_field(&response, "__schema");
    assert_eq!(get(schema_value, "__typename"), &Value::from("__Schema"));
    assert_eq!(
        get(get(schema_value, "queryType"), "__typename"),
        &Value::from("__Type")
    );
    assert_eq!(
        get(get(schema_value, "queryType"), "name"),
        &Value::from("Query")
    );

    let query_type = result_field(&response, "__type");
    assert_eq!(get(query_type, "__typename"), &Value::from("__Type"));
    assert_eq!(get(query_type, "kind"), &Value::enumeration("OBJECT"));
    assert_eq!(get(query_type, "name"), &Value::from("Query"));

    let fields = get(query_type, "fields").as_list().expect("fields list");
    let users = fields
        .iter()
        .find(|field| get(field, "name") == &Value::from("users"))
        .expect("users field");
    let chain = wrapper_chain(get(users, "type"));
    assert_eq!(
        chain,
        vec![
            ("NON_NULL".to_owned(), None),
            ("LIST".to_owned(), None),
            ("NON_NULL".to_owned(), None),
            ("OBJECT".to_owned(), Some("User".to_owned())),
        ]
    );

    let unknown = run(&schema, "{ __type(name: \"Nope\") { name } }");
    expect_no_errors(&unknown);
    assert_eq!(result_field(&unknown, "__type"), &Value::Null);
}

fn wrapper_chain(mut ty: &Value) -> Vec<(String, Option<String>)> {
    let mut chain = Vec::new();
    loop {
        let kind = get(ty, "kind");
        let kind = kind.as_str().unwrap_or_default().to_owned();
        let name = get(ty, "name").as_str().map(str::to_owned);
        chain.push((kind, name));
        ty = get(ty, "ofType");
        if ty.is_null() {
            break;
        }
    }
    chain
}

#[test]
fn coverage_introspection_include_deprecated_defaults_to_false() {
    let schema = sdl(r#"
        type Query {
          fresh: String
          old: String @deprecated(reason: "gone")
        }
        enum Color { RED, OLD @deprecated }
        "#);

    let default_fields = run(&schema, "{ __type(name: \"Query\") { fields { name } } }");
    expect_no_errors(&default_fields);
    assert_eq!(
        names(get(result_field(&default_fields, "__type"), "fields")),
        vec!["fresh"]
    );

    let included_fields = run(
        &schema,
        "{ __type(name: \"Query\") { fields(includeDeprecated: true) { name } } }",
    );
    expect_no_errors(&included_fields);
    assert_eq!(
        names(get(result_field(&included_fields, "__type"), "fields")),
        vec!["fresh", "old"]
    );

    let default_values = run(
        &schema,
        "{ __type(name: \"Color\") { enumValues { name } } }",
    );
    expect_no_errors(&default_values);
    assert_eq!(
        names(get(result_field(&default_values, "__type"), "enumValues")),
        vec!["RED"]
    );

    let included_values = run(
        &schema,
        "{ __type(name: \"Color\") { enumValues(includeDeprecated: true) { name } } }",
    );
    expect_no_errors(&included_values);
    assert_eq!(
        names(get(result_field(&included_values, "__type"), "enumValues")),
        vec!["RED", "OLD"]
    );
}

fn names(value: &Value) -> Vec<String> {
    value
        .as_list()
        .map(|items| {
            items
                .iter()
                .filter_map(|item| get(item, "name").as_str().map(str::to_owned))
                .collect()
        })
        .unwrap_or_default()
}

#[test]
fn coverage_introspection_possible_types_and_interfaces_are_transitive() {
    let schema = sdl(r#"
        interface Node { id: ID! }
        interface Entity implements Node { id: ID! }
        type User implements Entity & Node { id: ID!, name: String }
        type Post implements Node { id: ID! }
        type Query { node: Node, entity: Entity }
        "#);

    let response = run(
        &schema,
        r#"
        {
          node: __type(name: "Node") { possibleTypes { name } }
          entity: __type(name: "Entity") { possibleTypes { name } }
          user: __type(name: "User") { interfaces { name } }
        }
        "#,
    );
    expect_no_errors(&response);

    assert_eq!(
        names(get(result_field(&response, "node"), "possibleTypes")),
        vec!["User", "Post"]
    );
    assert_eq!(
        names(get(result_field(&response, "entity"), "possibleTypes")),
        vec!["User"]
    );
    assert_eq!(
        names(get(result_field(&response, "user"), "interfaces")),
        vec!["Entity", "Node"]
    );
}

#[test]
fn coverage_introspection_default_value_printing() {
    let schema = sdl(r#"
        enum Color { RED }
        input Point { x: Float, y: Int }
        type Query {
          f(
            s: String = "he said \"hi\"\nnext",
            c: Color = RED,
            l: [Int!] = [1, 2],
            p: Point = {x: 1.5, y: 2}
          ): String
        }
        "#);

    let response = run(
        &schema,
        r#"
        {
          __type(name: "Query") {
            fields {
              name
              args { name defaultValue }
            }
          }
        }
        "#,
    );
    expect_no_errors(&response);

    let fields = get(result_field(&response, "__type"), "fields")
        .as_list()
        .expect("fields list");
    let field = fields
        .iter()
        .find(|field| get(field, "name") == &Value::from("f"))
        .expect("f field");
    let args = get(field, "args").as_list().expect("args list");
    let default = |name: &str| -> String {
        args.iter()
            .find(|arg| get(arg, "name") == &Value::from(name))
            .map(|arg| {
                get(arg, "defaultValue")
                    .as_str()
                    .unwrap_or_default()
                    .to_owned()
            })
            .unwrap_or_else(|| panic!("missing argument {name}"))
    };

    assert_eq!(default("s"), r#""he said \"hi\"\nnext""#);
    assert_eq!(default("c"), "RED");
    assert_eq!(default("l"), "[1, 2]");
    assert_eq!(default("p"), "{x: 1.5, y: 2}");
}

#[test]
fn coverage_introspection_descriptor_typenames() {
    let schema = sdl(r#"
        enum Color { RED, OLD @deprecated }
        input Filter { a: Int = 1 }
        interface Node { id: ID! }
        type User implements Node { id: ID!, old: String @deprecated }
        type Query { user(filter: Filter): User, color: Color }
        "#);

    let response = run(
        &schema,
        r#"
        {
          meta: __schema { __typename }
          typ: __type(name: "Query") { __typename fields { __typename args { __typename } } }
          dir: __schema { directives { __typename } }
          en: __type(name: "Color") { __typename enumValues { __typename } }
          pos: __type(name: "Node") { __typename possibleTypes { __typename } }
          inp: __type(name: "Filter") { __typename inputFields { __typename } }
        }
        "#,
    );
    expect_no_errors(&response);

    assert_eq!(
        get(result_field(&response, "meta"), "__typename"),
        &Value::from("__Schema")
    );

    let typ = result_field(&response, "typ");
    assert_eq!(get(typ, "__typename"), &Value::from("__Type"));
    let fields = get(typ, "fields").as_list().expect("fields");
    assert!(!fields.is_empty());
    for field in fields {
        assert_eq!(get(field, "__typename"), &Value::from("__Field"));
        for argument in get(field, "args").as_list().expect("args") {
            assert_eq!(get(argument, "__typename"), &Value::from("__InputValue"));
        }
    }

    let directives = get(result_field(&response, "dir"), "directives")
        .as_list()
        .expect("directives");
    assert!(!directives.is_empty());
    for directive in directives {
        assert_eq!(get(directive, "__typename"), &Value::from("__Directive"));
    }

    let enum_values = get(result_field(&response, "en"), "enumValues")
        .as_list()
        .expect("enumValues");
    assert!(!enum_values.is_empty());
    for value in enum_values {
        assert_eq!(get(value, "__typename"), &Value::from("__EnumValue"));
    }

    let possible = get(result_field(&response, "pos"), "possibleTypes")
        .as_list()
        .expect("possibleTypes");
    assert!(!possible.is_empty());
    for value in possible {
        assert_eq!(get(value, "__typename"), &Value::from("__Type"));
    }

    let input_fields = get(result_field(&response, "inp"), "inputFields")
        .as_list()
        .expect("inputFields");
    assert!(!input_fields.is_empty());
    for value in input_fields {
        assert_eq!(get(value, "__typename"), &Value::from("__InputValue"));
    }
}

#[test]
fn coverage_type_meta_field_accepts_string_variables() {
    let schema = sdl(r#"type Query { f: String }"#);
    let response = run_vars(
        &schema,
        "query Q($name: String!) { __type(name: $name) { name kind } }",
        Object::from_fields([("name", Value::from("Query"))]),
    );
    expect_no_errors(&response);
    assert_eq!(
        get(result_field(&response, "__type"), "name"),
        &Value::from("Query")
    );
}

#[test]
fn coverage_introspection_union_metadata() {
    let schema = sdl(r#"
        type User { id: ID! }
        type Post { id: ID! }
        union Search = User | Post
        type Query { search: Search }
        "#);

    let response = run(
        &schema,
        r#"
        {
          __type(name: "Search") {
            kind
            possibleTypes { name }
            fields { name }
            interfaces { name }
          }
        }
        "#,
    );
    expect_no_errors(&response);
    let search = result_field(&response, "__type");
    assert_eq!(get(search, "kind"), &Value::enumeration("UNION"));
    assert_eq!(names(get(search, "possibleTypes")), vec!["User", "Post"]);
    assert_eq!(get(search, "fields"), &Value::Null);
    assert_eq!(get(search, "interfaces"), &Value::Null);
}

// ---------------------------------------------------------------------------------------------
// 8. Parser (gaps not covered by the existing parser suite)
// ---------------------------------------------------------------------------------------------

#[test]
fn coverage_parser_invalid_surrogate_pairs_and_block_strings() {
    // Lone surrogates are not Unicode scalar values.
    assert!(gql::parser::parse_query(r#"{ f(a: "\uD83D") }"#).is_err());
    assert!(gql::parser::parse_query(r#"{ f(a: "\uDE00") }"#).is_err());
    assert!(gql::parser::parse_query(r#"{ f(a: "\uD83D\u0041") }"#).is_err());

    let document = gql::parser::parse_query(r#"{ f(a: "\uD83D\uDE00", b: """x \""" y""") }"#)
        .expect("valid document");
    let operation = document.operations().next().expect("operation");
    let arguments = &operation.selection_set.items;
    let gql::ast::Selection::Field(field) = &arguments[0] else {
        panic!("expected a field");
    };
    let a = field.argument("a").expect("a argument");
    assert_eq!(
        a.value,
        ast::Value::String(std::borrow::Cow::Owned("😀".to_owned()))
    );
    let b = field.argument("b").expect("b argument");
    assert_eq!(
        b.value,
        ast::Value::String(std::borrow::Cow::Owned("x \"\"\" y".to_owned()))
    );
}

#[test]
fn coverage_parser_rejects_invalid_numbers_and_accepts_crlf_and_commas() {
    for source in [
        "{ f(a: 01) }",
        "{ f(a: 1.) }",
        "{ f(a: .5) }",
        "{ f(a: 1e) }",
        "{ f(a: 1.2.3) }",
        "{ f(a: +1) }",
        "{ f(a: 1x) }",
    ] {
        assert!(
            gql::parser::parse_query(source).is_err(),
            "expected a syntax error for {source:?}"
        );
    }

    let document = gql::parser::parse_query("{\r\n  # comment\r\n  f, (a: 1,, b: 2), }")
        .expect("valid document");
    let operation = document.operations().next().expect("operation");
    let gql::ast::Selection::Field(field) = &operation.selection_set.items[0] else {
        panic!("expected a field");
    };
    assert_eq!(field.arguments.len(), 2);
}

// ---------------------------------------------------------------------------------------------
// 9. Schema definition and validation
// ---------------------------------------------------------------------------------------------

#[test]
fn spec_sdl_infers_default_root_operation_type_names() {
    // October 2021 3.3: the schema definition may be omitted when the root operation types are
    // named Query, Mutation and Subscription.
    let schema = sdl(r#"
        type Query { q: String }
        type Mutation { m: String }
        type Subscription { s: String }
        "#);

    assert_eq!(
        schema.mutation_name().map(|name| name.to_string()),
        Some("Mutation".to_owned())
    );
    assert_eq!(
        schema.subscription_name().map(|name| name.to_string()),
        Some("Subscription".to_owned())
    );
}

#[test]
fn spec_interface_field_may_be_non_null_where_the_interface_field_is_nullable() {
    // October 2021 3.6.2: an implementing field's type must be equal to or a covariant subtype
    // of the interface field's type. `ID!` is a subtype of `ID`.
    let result = Schema::from_sdl(
        r#"
        interface Node { id: ID }
        type User implements Node { id: ID! }
        type Query { user: User }
        "#,
    );
    assert!(
        result.is_ok(),
        "a non-null implementation of a nullable interface field is valid: {:?}",
        result.err().map(|error| error.to_string())
    );
}

#[test]
fn spec_interface_field_must_not_be_nullable_where_the_interface_field_is_non_null() {
    // October 2021 3.6.2: `ID` is not a covariant subtype of `ID!`, so this schema is invalid.
    let result = Schema::from_sdl(
        r#"
        interface Node { id: ID! }
        type User implements Node { id: ID }
        type Query { user: User }
        "#,
    );
    assert!(
        result.is_err(),
        "a nullable implementation of a non-null interface field must be rejected"
    );
}

#[test]
fn spec_input_cycle_through_a_non_null_list_is_valid() {
    // October 2021 3.10: Example 81 makes `self: [Example!]!` explicitly valid because the list
    // can be empty.
    let result = Schema::from_sdl(
        r#"
        input Example { self: [Example!]!, value: String }
        type Query { example(input: Example): String }
        "#,
    );
    assert!(
        result.is_ok(),
        "a cycle broken by a list is valid: {:?}",
        result.err().map(|error| error.to_string())
    );
}

#[test]
fn spec_sdl_field_arguments_must_have_unique_names() {
    let result = Schema::from_sdl(
        r#"
        type Query { f(a: Int, a: Int): String }
        "#,
    );
    assert!(
        result.is_err(),
        "duplicate field argument names must be rejected"
    );
}

#[test]
fn spec_interface_implementing_interface_must_match_argument_types() {
    let result = Schema::from_sdl(
        r#"
        interface Base { f(a: Int): String }
        interface Child implements Base { f(a: String): String }
        type Query { child: Child }
        "#,
    );
    assert!(
        result.is_err(),
        "an interface argument must accept the same type (invariant) as the implemented field"
    );
}

#[test]
fn spec_object_may_not_add_required_arguments_to_an_interface_field() {
    let result = Schema::from_sdl(
        r#"
        interface Node { name(prefix: String): String }
        type User implements Node { name(prefix: String, suffix: String!): String }
        type Query { user: User }
        "#,
    );
    assert!(
        result.is_err(),
        "additional arguments on an implementing field must be optional"
    );
}

#[test]
fn spec_object_must_declare_transitively_implemented_interfaces() {
    let result = Schema::from_sdl(
        r#"
        interface Node { id: ID! }
        interface Entity implements Node { id: ID! }
        type User implements Entity { id: ID!, name: String }
        type Query { user: User }
        "#,
    );
    assert!(
        result.is_err(),
        "an object implementing Entity must also declare that it implements Node"
    );
}

#[test]
fn spec_sdl_unknown_directives_are_rejected() {
    let result = Schema::from_sdl(
        r#"
        type Query @nope { f: String }
        "#,
    );
    assert!(
        result.is_err(),
        "an unknown directive in SDL must be rejected rather than silently dropped"
    );
}

#[test]
fn spec_object_types_must_define_at_least_one_field() {
    let schema = Schema::builder()
        .object(ObjectType::new("Query"))
        .query("Query")
        .build();
    assert!(
        schema.is_err(),
        "an object type must define one or more fields"
    );
}

#[test]
fn spec_interface_types_must_define_at_least_one_field() {
    let schema = Schema::builder()
        .object(ObjectType::new("Query").field(Field::new("f", TypeRef::string())))
        .interface(InterfaceType::new("Empty"))
        .query("Query")
        .build();
    assert!(
        schema.is_err(),
        "an interface type must define one or more fields"
    );
}

#[test]
fn coverage_interface_field_covariance_allows_interface_and_union_subtypes() {
    let result = Schema::from_sdl(
        r#"
        interface Node { id: ID! }
        type User implements Node { id: ID! }
        union Pet = User
        interface Container { item: Node, pet: Pet }
        type UserContainer implements Container { item: User!, pet: User! }
        type Query { container: Container }
        "#,
    );
    assert!(
        result.is_ok(),
        "User is a subtype of both Node and Pet: {:?}",
        result.err().map(|error| error.to_string())
    );
}

#[test]
fn coverage_interface_field_covariance_for_lists_and_optional_arguments() {
    let result = Schema::from_sdl(
        r#"
        interface Node { id: ID! }
        type User implements Node { id: ID! }
        interface Container {
          nodes: [Node]
          f(a: Int = 1): String
        }
        type UserContainer implements Container {
          nodes: [User!]!
          f(a: Int = 2, extra: Int): String
        }
        type Query { container: Container }
        "#,
    );
    assert!(
        result.is_ok(),
        "list covariance and additional optional arguments are valid: {:?}",
        result.err().map(|error| error.to_string())
    );
}

#[test]
fn spec_interface_implementations_must_be_unique_and_not_self() {
    // October 2021 3.6: an object declares "one or more unique interfaces".
    assert!(
        Schema::from_sdl(
            r#"
            interface Node { id: ID! }
            type User implements Node & Node { id: ID! }
            type Query { user: User }
            "#
        )
        .is_err(),
        "an object may not declare the same interface twice"
    );

    // October 2021 3.7: an interface may not implement itself.
    assert!(
        Schema::from_sdl(
            r#"
            interface Node implements Node { id: ID! }
            type Query { node: Node }
            "#
        )
        .is_err(),
        "an interface may not implement itself"
    );
}

#[test]
fn coverage_one_of_schema_constraints() {
    assert!(
        Schema::from_sdl(
            r#"input Choice @oneOf { a: Int! } type Query { f(choice: Choice): String }"#
        )
        .is_err(),
        "a @oneOf field must be nullable"
    );
    assert!(
        Schema::from_sdl(
            r#"input Choice @oneOf { a: Int = 1 } type Query { f(choice: Choice): String }"#
        )
        .is_err(),
        "a @oneOf field must not have a default value"
    );
    assert!(
        Schema::from_sdl(
            r#"input Choice @oneOf { a: Int, b: String } type Query { f(choice: Choice): String }"#
        )
        .is_ok(),
        "a valid @oneOf input object"
    );
}

#[test]
fn spec_sdl_directive_locations_and_arguments_are_validated() {
    // `@skip` is only valid on fields, fragment spreads and inline fragments.
    assert!(
        Schema::from_sdl(r#"type Query @skip(if: true) { f: String }"#).is_err(),
        "@skip may not be used on an object type"
    );
    // `@oneOf` is only valid on input objects.
    assert!(
        Schema::from_sdl(r#"type Query @oneOf { f: String }"#).is_err(),
        "@oneOf may not be used on an object type"
    );
    // Arguments on applied directives must be defined.
    assert!(
        Schema::from_sdl(
            r#"
            directive @tag(name: String!) on FIELD_DEFINITION
            type Query { f: String @tag(nope: 1) }
            "#
        )
        .is_err(),
        "an applied directive argument must be defined"
    );
    assert!(
        Schema::from_sdl(
            r#"
            directive @tag(name: String!) on FIELD_DEFINITION
            type Query { f: String @tag }
            "#
        )
        .is_err(),
        "a required directive argument must be provided"
    );
}

#[test]
fn spec_sdl_non_repeatable_directives_may_not_repeat() {
    assert!(
        Schema::from_sdl(
            r#"
            directive @tag on OBJECT
            type Query @tag @tag { f: String }
            "#
        )
        .is_err(),
        "a non-repeatable directive may not be applied twice at one location"
    );

    assert!(
        Schema::from_sdl(
            r#"
            directive @tag repeatable on OBJECT
            type Query @tag @tag { f: String }
            "#
        )
        .is_ok(),
        "a repeatable directive may be applied twice"
    );
}

#[test]
fn spec_sdl_directive_argument_names_are_unique() {
    assert!(
        Schema::from_sdl(
            r#"
            directive @d(a: Int, a: Int) on FIELD_DEFINITION
            type Query { f: String }
            "#
        )
        .is_err(),
        "a directive definition must not declare an argument twice"
    );
}

#[test]
fn spec_sdl_type_extension_may_precede_the_type_definition() {
    // Type system documents are not order-dependent: extensions describe additions to a type
    // defined anywhere in the document.
    let result = Schema::from_sdl(
        r#"
        extend type Query { b: String }
        type Query { a: String }
        "#,
    );
    assert!(
        result.is_ok(),
        "a type extension may appear before the definition: {:?}",
        result.err().map(|error| error.to_string())
    );
}

#[test]
fn coverage_schema_rejects_duplicate_arguments_and_reserved_names() {
    let duplicate_args = Schema::builder()
        .object(
            ObjectType::new("Query").field(
                Field::new("f", TypeRef::string())
                    .arg(InputValue::new("a", TypeRef::int()))
                    .arg(InputValue::new("a", TypeRef::int())),
            ),
        )
        .query("Query")
        .build();
    assert!(
        duplicate_args.is_err(),
        "duplicate arguments must be rejected"
    );

    let reserved = Schema::builder()
        .object(ObjectType::new("Query").field(Field::new("__bad", TypeRef::string())))
        .query("Query")
        .build();
    assert!(reserved.is_err(), "reserved field names must be rejected");
}

// ---------------------------------------------------------------------------------------------
// 10. Schema diff compatibility
// ---------------------------------------------------------------------------------------------

#[test]
fn spec_schema_diff_treats_required_to_optional_input_field_as_compatible() {
    let old = sdl(r#"type Query { f(input: Filter): Int } input Filter { a: Int! }"#);
    let new = sdl(r#"type Query { f(input: Filter): Int } input Filter { a: Int }"#);
    let diff = gql::diff(&old, &new);
    assert!(
        diff.is_compatible(),
        "making an input field optional is not a breaking change: {diff}"
    );
}

#[test]
fn spec_schema_diff_treats_required_to_optional_argument_as_compatible() {
    let old = sdl(r#"type Query { f(a: Int!): Int }"#);
    let new = sdl(r#"type Query { f(a: Int): Int }"#);
    let diff = gql::diff(&old, &new);
    assert!(
        diff.is_compatible(),
        "making an argument optional is not a breaking change: {diff}"
    );
}

#[test]
fn spec_schema_diff_reports_removed_directive_arguments() {
    let old = sdl(r#"
        directive @tag(name: String!) on FIELD_DEFINITION
        type Query { f: String }
        "#);
    let new = sdl(r#"
        directive @tag on FIELD_DEFINITION
        type Query { f: String }
        "#);
    let diff = gql::diff(&old, &new);
    assert!(
        !diff.is_compatible(),
        "removing a directive argument breaks existing documents"
    );
}

// ---------------------------------------------------------------------------------------------
// 11. Subscriptions
// ---------------------------------------------------------------------------------------------

fn subscription_schema() -> Schema {
    Schema::builder()
        .object(ObjectType::new("Query").field(Field::new("ping", TypeRef::string())))
        .object(ObjectType::new("Event").field(Field::new("value", TypeRef::string().non_null())))
        .object(
            ObjectType::new("Subscription")
                .field(Field::new("event", TypeRef::named("Event")).subscribe(|_| {
                    Box::pin(async {
                        Ok(futures::stream::iter(vec![
                            Ok::<Value, GraphQLError>(object([("value", Value::from("ok"))])),
                            Ok(Value::object(Object::new())),
                        ])
                        .boxed())
                    })
                }))
                .field(
                    Field::new(
                        "events",
                        TypeRef::named("Event").non_null().list().non_null(),
                    )
                    .subscribe(|_| {
                        Box::pin(async {
                            Ok(
                                futures::stream::iter(vec![Ok::<Value, GraphQLError>(
                                    Value::List(vec![
                                        object([("value", Value::from("a"))]),
                                        Value::object(Object::new()),
                                        object([("value", Value::from("c"))]),
                                    ]),
                                )])
                                .boxed(),
                            )
                        })
                    }),
                )
                .field(
                    Field::new("strict", TypeRef::named("Event").non_null()).subscribe(|_| {
                        Box::pin(async {
                            Ok(
                                futures::stream::iter(vec![Ok::<Value, GraphQLError>(Value::Null)])
                                    .boxed(),
                            )
                        })
                    }),
                ),
        )
        .query("Query")
        .subscription("Subscription")
        .build()
        .expect("valid schema")
}

#[test]
fn coverage_subscription_events_apply_null_propagation_and_error_paths() {
    let schema = subscription_schema();

    let subscription = block_on(gql::subscribe(
        &schema,
        Request::new("subscription { event { value } }"),
        &ExecutionOptions::default(),
    ))
    .expect("subscription starts");
    let responses = block_on(subscription.stream.collect::<Vec<_>>());
    assert_eq!(responses.len(), 2);

    expect_no_errors(&responses[0]);
    assert_eq!(
        responses[0].data,
        Some(object([("event", object([("value", Value::from("ok"))]))]))
    );

    assert_eq!(responses[1].errors.len(), 1);
    assert_eq!(path_of(&responses[1].errors[0]), vec!["event", "value"]);
    assert_eq!(responses[1].data, Some(object([("event", Value::Null)])));

    // A failing item in a non-null item list fails the whole non-null root field.
    let list_subscription = block_on(gql::subscribe(
        &schema,
        Request::new("subscription { events { value } }"),
        &ExecutionOptions::default(),
    ))
    .expect("subscription starts");
    let list_responses = block_on(list_subscription.stream.collect::<Vec<_>>());
    assert_eq!(list_responses.len(), 1);
    assert_eq!(list_responses[0].errors.len(), 1);
    assert_eq!(
        path_of(&list_responses[0].errors[0]),
        vec!["events", "1", "value"]
    );
    assert_eq!(list_responses[0].data, Some(Value::Null));

    // A null event for a non-null root field nulls data.
    let strict_subscription = block_on(gql::subscribe(
        &schema,
        Request::new("subscription { strict { value } }"),
        &ExecutionOptions::default(),
    ))
    .expect("subscription starts");
    let strict_responses = block_on(strict_subscription.stream.collect::<Vec<_>>());
    assert_eq!(strict_responses.len(), 1);
    assert_eq!(strict_responses[0].errors.len(), 1);
    assert_eq!(path_of(&strict_responses[0].errors[0]), vec!["strict"]);
    assert_eq!(strict_responses[0].data, Some(Value::Null));
}

#[test]
fn coverage_subscription_responses_use_the_response_key() {
    let schema = subscription_schema();
    let subscription = block_on(gql::subscribe(
        &schema,
        Request::new("subscription { latest: event { value } }"),
        &ExecutionOptions::default(),
    ))
    .expect("subscription starts");
    let responses = block_on(subscription.stream.collect::<Vec<_>>());
    assert_eq!(responses.len(), 2);
    assert_eq!(
        responses[0].data,
        Some(object([("latest", object([("value", Value::from("ok"))]))]))
    );
}

#[test]
fn coverage_subscription_setup_errors_are_returned_before_streaming() {
    let schema = Schema::builder()
        .object(ObjectType::new("Query").field(Field::new("ping", TypeRef::string())))
        .object(
            ObjectType::new("Subscription").field(
                Field::new("broken", TypeRef::named("Event"))
                    .subscribe(|_| Box::pin(async { Err(GraphQLError::new("cannot subscribe")) })),
            ),
        )
        .object(ObjectType::new("Event").field(Field::new("value", TypeRef::string())))
        .query("Query")
        .subscription("Subscription")
        .build()
        .expect("valid schema");

    let result = block_on(gql::subscribe(
        &schema,
        Request::new("subscription { broken { value } }"),
        &ExecutionOptions::default(),
    ));
    let response = result.err().expect("setup must fail");
    assert_eq!(response.errors.len(), 1);
    assert_eq!(response.errors[0].message(), "cannot subscribe");
}

#[test]
fn coverage_subscription_events_resolve_abstract_types() {
    let schema = Schema::builder()
        .object(ObjectType::new("Query").field(Field::new("ping", TypeRef::string())))
        .object(
            ObjectType::new("User")
                .implements("Node")
                .field(Field::new("id", TypeRef::id().non_null())),
        )
        .interface(
            InterfaceType::new("Node")
                .field(Field::new("id", TypeRef::id().non_null()))
                .resolve_type(|_, _| Ok("User".into())),
        )
        .object(ObjectType::new("Subscription").field(
            Field::new("node", TypeRef::named("Node")).subscribe(|_| {
                Box::pin(async {
                    Ok(
                        futures::stream::iter(vec![Ok::<Value, GraphQLError>(object([(
                            "id",
                            Value::from("1"),
                        )]))])
                        .boxed(),
                    )
                })
            }),
        ))
        .query("Query")
        .subscription("Subscription")
        .build()
        .expect("valid schema");

    let subscription = block_on(gql::subscribe(
        &schema,
        Request::new("subscription { node { __typename id } }"),
        &ExecutionOptions::default(),
    ))
    .expect("subscription starts");
    let responses = block_on(subscription.stream.collect::<Vec<_>>());
    assert_eq!(responses.len(), 1);
    expect_no_errors(&responses[0]);
    let node = result_field(&responses[0], "node");
    assert_eq!(get(node, "__typename"), &Value::from("User"));
    assert_eq!(get(node, "id"), &Value::from("1"));
}

// ---------------------------------------------------------------------------------------------
// 12. Result coercion
// ---------------------------------------------------------------------------------------------

#[test]
fn spec_float_result_must_reject_non_finite_values() {
    // October 2021 3.5.2: NaN and Infinity cannot be coerced to Float and must raise a field
    // error.
    let schema = Schema::builder()
        .object(
            ObjectType::new("Query")
                .field(
                    Field::new("nan", TypeRef::float())
                        .resolve(|_| Box::pin(async { Ok(Value::Float(f64::NAN)) })),
                )
                .field(
                    Field::new("infinity", TypeRef::float())
                        .resolve(|_| Box::pin(async { Ok(Value::Float(f64::INFINITY)) })),
                ),
        )
        .query("Query")
        .build()
        .expect("valid schema");

    let response = run(&schema, "{ nan infinity }");
    assert_eq!(
        response.errors.len(),
        2,
        "non-finite Float results must be field errors: {:?}",
        response.data
    );
    assert_eq!(result_field(&response, "nan"), &Value::Null);
    assert_eq!(result_field(&response, "infinity"), &Value::Null);
}

#[test]
fn spec_float_input_must_reject_non_finite_values() {
    let schema = Schema::builder()
        .object(
            ObjectType::new("Query")
                .field(echo_field("f", [InputValue::new("x", TypeRef::float())])),
        )
        .query("Query")
        .build()
        .expect("valid schema");

    let response = run_vars(
        &schema,
        "query Q($x: Float) { f(x: $x) }",
        Object::from_fields([("x", Value::Float(f64::NAN))]),
    );
    assert!(
        !response.errors.is_empty(),
        "a non-finite Float variable must be rejected"
    );
    assert_eq!(response.data, None);
}

#[test]
fn spec_id_result_is_serialized_as_a_string() {
    let schema = Schema::builder()
        .object(ObjectType::new("Query").field(
            Field::new("id", TypeRef::id()).resolve(|_| Box::pin(async { Ok(Value::Int(7)) })),
        ))
        .query("Query")
        .build()
        .expect("valid schema");

    let response = run(&schema, "{ id }");
    expect_no_errors(&response);
    assert_eq!(result_field(&response, "id"), &Value::from("7"));
}
