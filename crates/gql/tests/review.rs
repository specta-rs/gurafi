//! Adversarial review probes for PR #9. Scratch file: each test states the behaviour the
//! specification requires and prints what this implementation does.

use futures::executor::block_on;
use gql::{
    ExecutionOptions, Field, InputObjectType, InputValue, Object, ObjectType, Request, Response,
    Schema, TypeRef, ValidationOptions, Value,
};

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

fn echo_input_field(name: &'static str, ty: TypeRef) -> Field {
    Field::new(name, ty).resolve(move |context| {
        Box::pin(async move { Ok(context.args.get("arg").cloned().unwrap_or(Value::Null)) })
    })
}

fn echo_arg_field(name: &'static str, ty: TypeRef) -> Field {
    Field::new(name, ty).resolve(move |context| {
        Box::pin(async move {
            Ok(context
                .args
                .get("arg")
                .and_then(Value::as_object)
                .and_then(|object| object.get("xs"))
                .cloned()
                .unwrap_or(Value::Null))
        })
    })
}

/// PROBE 1: an input-object field default is applied *uncoerced* when the request supplies an
/// object literal, but coerced when the same object arrives as a variable.
///
/// `input In { xs: [Int] = 1 }` is a valid schema (a single value coerces to a one-element list,
/// October 2021 §3.10), so `{ echo(arg: {}) }` must produce `[1]` exactly like
/// `query ($i: In) { echo(arg: $i) }` with `{"i": {}}` does.
#[test]
fn probe_input_field_default_coercion() {
    let schema = Schema::builder()
        .input_object(
            InputObjectType::new("In")
                .field(InputValue::new("xs", TypeRef::int().list()).default_value(Value::Int(1))),
        )
        .object(ObjectType::new("Query").field(echo_arg_field("echo", TypeRef::int().list())))
        .query("Query")
        .build()
        .expect("valid schema");

    let literal = run(&schema, "{ echo(arg: {}) }");
    let mut variables = Object::new();
    variables.insert("i", Value::object(Object::new()));
    let variable = run_vars(&schema, "query ($i: In) { echo(arg: $i) }", variables);

    println!(
        "LITERAL  data={:?} errors={:?}",
        literal.data, literal.errors
    );
    println!(
        "VARIABLE data={:?} errors={:?}",
        variable.data, variable.errors
    );
    assert_eq!(
        literal.data, variable.data,
        "literal and variable paths must coerce defaults identically"
    );
}

/// PROBE 2: interface field covariance where the subtype is another interface.
///
/// `interface Special implements Node` and both declare `related`, so `Special.related:
/// Special` is a valid implementation of `Node.related: Node` (Special is a possible type of
/// Node). `is_output_subtype` only knows Object -> Interface / Object -> Union.
#[test]
fn probe_interface_to_interface_covariance() {
    let result = Schema::from_sdl(
        r#"
        interface Node { related: Node }
        interface Special implements Node { related: Special }
        type Query { node: Node }
        "#,
    );
    println!(
        "SDL result: {:?}",
        result.as_ref().err().map(ToString::to_string)
    );
}

/// PROBE 2b: the same covariance expressed as a public `is_output_subtype` call.
#[test]
fn probe_is_output_subtype_interface_interface() {
    let schema = Schema::from_sdl(
        r#"
        interface Node { id: ID! }
        interface Special implements Node { id: ID! }
        type Query { node: Node }
        "#,
    )
    .expect("valid schema");
    println!(
        "Special <: Node = {}",
        gql::is_output_subtype(&schema, &TypeRef::named("Special"), &TypeRef::named("Node"))
    );
}

/// PROBE 3: `@deprecated` is only valid on arguments and input fields that are optional.
/// graphql-js rejects `Int! @deprecated`; this implementation's SDL checker does not.
#[test]
fn probe_deprecated_on_required_input() {
    let result = Schema::from_sdl(
        r#"
        type Query {
          f(a: Int! @deprecated(reason: "required")): String
        }
        input In { x: Int! @deprecated }
        "#,
    );
    println!(
        "SDL result: {:?}",
        result.as_ref().err().map(ToString::to_string)
    );
}

/// PROBE 4: validation's `max_depth` shares one `visited` set across the whole operation, so a
/// fragment first seen on a shallow path hides its depth on a deeper later path.
#[test]
fn probe_depth_limit_under_count() {
    let schema = Schema::from_sdl(
        r#"
        type Query { a: Query, b: Query, c: Query, d: Query, x: Query, y: Query, z: Query }
        "#,
    )
    .expect("valid schema");
    let query = "{ a { ...F } b { c { d { ...F } } } } fragment F on Query { x { y { z } } }";
    let document = gql::parser::parse_query(query).expect("parses");
    let errors = gql::validate(
        &schema,
        &document,
        &ValidationOptions {
            introspection: true,
            max_depth: 4,
            max_errors: 0,
        },
    );
    println!(
        "validate(max_depth=4) errors: {:?}",
        errors.iter().map(|e| e.message()).collect::<Vec<_>>()
    );
    let complexity = gql::complexity::analyze(
        &schema,
        &document,
        None,
        &Object::new(),
        &gql::complexity::ComplexityOptions {
            max_depth: 4,
            ..Default::default()
        },
    );
    println!("complexity analyze: {complexity:?}");
}

/// PROBE 5: `@oneOf` applied through an input-object *extension* is not picked up.
#[test]
fn probe_one_of_extension() {
    let result = Schema::from_sdl(
        r#"
        input Choice { a: Int, b: String }
        extend input Choice @oneOf
        type Query { f(c: Choice): String }
        "#,
    );
    match result {
        Ok(schema) => {
            let input = schema.input_object_type("Choice").expect("input object");
            println!("one_of = {}", input.one_of);
        }
        Err(error) => println!("SDL rejected: {error}"),
    }
}

/// PROBE 6: spot-check introspection nullability against graphql-js's `introspectionQuery`.
#[test]
fn probe_introspection_nullability() {
    let schema = Schema::from_sdl(
        r#"
        scalar Date
        enum Role { A }
        input Filter { term: String }
        interface Node { id: ID! }
        type User implements Node { id: ID! }
        union Search = User
        type Query { node: Node, search: Search, users(f: Filter): [User!]!, date: Date! }
        "#,
    )
    .expect("valid schema");
    let query = r#"{
      __type(name: "Search") { kind name fields { name } interfaces { name } possibleTypes { name } inputFields { name } enumValues { name } isOneOf }
      __type2: __type(name: "Filter") { kind name fields { name } interfaces { name } possibleTypes { name } inputFields { name type { kind name } } isOneOf }
      __type3: __type(name: "Date") { kind name fields { name } interfaces { name } possibleTypes { name } }
      __schema { description types { name } }
    }"#;
    let response = run(&schema, query);
    println!("errors={:?}", response.errors);
    println!("data={:#?}", response.data);
}

/// PROBE 7: `__typename` when the parent is an opaque model value (gurafi's normal parent).
#[test]
fn probe_typename_on_external_parent() {
    let schema = Schema::builder()
        .object(
            ObjectType::new("Thing")
                .is_type_of(|value| value.as_external::<String>().is_some())
                .field(Field::new("name", TypeRef::string()).resolve(|context| {
                    Box::pin(async move {
                        Ok(context
                            .parent
                            .as_external::<String>()
                            .map_or(Value::Null, |name| Value::from(name.as_str())))
                    })
                })),
        )
        .interface(gql::InterfaceType::new("Node").field(Field::new("name", TypeRef::string())))
        .object(
            ObjectType::new("Query").field(
                Field::new("node", TypeRef::named("Node"))
                    .resolve(|_| Box::pin(async { Ok(Value::external(String::from("thing"))) })),
            ),
        )
        .query("Query")
        .build()
        .expect("valid schema");

    let response = run(&schema, "{ node { __typename name } }");
    println!("data={:?} errors={:?}", response.data, response.errors);
}

/// PROBE 8: a non-null field error inside a merged selection set must report one error per
/// occurrence at the right path even when siblings resolve concurrently.
#[test]
fn probe_concurrent_error_paths() {
    let schema = Schema::builder()
        .object(
            ObjectType::new("Thing")
                .field(
                    Field::new("ok", TypeRef::string())
                        .resolve(|_| Box::pin(async { Ok(Value::from("ok")) })),
                )
                .field(
                    Field::new("bad", TypeRef::string().non_null())
                        .resolve(|_| Box::pin(async { Ok(Value::Null) })),
                ),
        )
        .object(
            ObjectType::new("Query").field(
                Field::new("thing", TypeRef::named("Thing"))
                    .resolve(|_| Box::pin(async { Ok(Value::object(Object::new())) })),
            ),
        )
        .query("Query")
        .build()
        .expect("valid schema");

    for concurrency in [0usize, 1, 16] {
        let response = block_on(gql::execute(
            &schema,
            Request::new("{ thing { bad ok } }"),
            &ExecutionOptions::default().max_concurrency(concurrency),
        ));
        println!(
            "concurrency={concurrency} data={:?} errors={:?}",
            response.data,
            response
                .errors
                .iter()
                .map(|error| (error.message().to_owned(), error.path().map(<[_]>::to_vec)))
                .collect::<Vec<_>>()
        );
    }
}
