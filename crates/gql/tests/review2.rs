//! Adversarial review probes for PR #9 (see report). Scratch file.

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

/// PROBE 9: complexity/depth underestimate when one response key merges selections from two
/// different (overlapping) parent scopes.
///
/// `item` is selected on `Node` (`item: Node`) and on `User` (`item: User`). Merging is legal
/// because `User` is a possible type of `Node`, and at runtime a `User` value collects *both*
/// selections, so the real nesting is `node.item.a.b.c.d` (depth 6). The analyzer keeps the
/// first node's scope (`Node`) for the merged entry, so the `a`/`b`/`c`/`d` selections are
/// looked up on `Node`, where they do not exist, and depth/cost collapse to 2.
#[test]
fn probe_complexity_scope_undercount() {
    let schema = Schema::from_sdl(
        r#"
        interface Node { item: Node }
        type User implements Node { item: User, a: User }
        type Holder implements Node { item: Holder }
        type Query { node: Node }
        "#,
    )
    .expect("valid schema");

    let query = r#"
      query {
        node {
          ... on Node { item }
          ... on User { item { a { item { a { item { a } } } } } }
        }
      }
    "#;
    let document = gql::parser::parse_query(query).expect("parses");
    let validation = gql::validate(
        &schema,
        &document,
        &ValidationOptions {
            introspection: true,
            max_depth: 0,
            max_errors: 0,
        },
    );
    println!(
        "validation errors: {:?}",
        validation.iter().map(|e| e.message()).collect::<Vec<_>>()
    );
    let complexity = gql::complexity::analyze(
        &schema,
        &document,
        None,
        &Object::new(),
        &gql::complexity::ComplexityOptions::default(),
    );
    println!("complexity: {complexity:?}");

    // What actually executes: count resolver invocations through a side channel.
    let mut schema_with_resolvers = Schema::builder();
    let _ = &mut schema_with_resolvers;
}

/// PROBE 10: validation `max_depth` under-counts when a fragment is first reached on a shallow
/// path. This shows the validation API alone accepting a depth-6 document at max_depth 4.
#[test]
fn probe_validation_depth_undercount() {
    let schema = Schema::from_sdl(
        r#"
        type Query { a: Query, b: Query, c: Query, d: Query, x: Query, y: Query }
        "#,
    )
    .expect("valid schema");
    // `z` is a field of Query with no sub-selection, so the only remaining rule that can fire is
    // the depth limit.
    let query = "{ a { ...F } b { c { d { ...F } } } } fragment F on Query { x { y { a { b } } } }";
    let document = gql::parser::parse_query(query).expect("parses");
    for limit in [4usize, 5, 6] {
        let errors = gql::validate(
            &schema,
            &document,
            &ValidationOptions {
                introspection: true,
                max_depth: limit,
                max_errors: 0,
            },
        );
        println!(
            "validate(max_depth={limit}) errors: {:?}",
            errors.iter().map(|e| e.message()).collect::<Vec<_>>()
        );
    }
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
    println!("complexity(max_depth=4): {complexity:?}");
}

/// PROBE 11: input-object literal defaults are not coerced (repeat of the earlier probe, kept
/// next to its documentation).
#[test]
fn probe_input_default_literal() {
    let schema = Schema::builder()
        .input_object(
            InputObjectType::new("In")
                .field(InputValue::new("xs", TypeRef::int().list()).default_value(Value::Int(1))),
        )
        .object(
            ObjectType::new("Query").field(
                Field::new("echo", TypeRef::int().list())
                    .arg(InputValue::new("arg", TypeRef::named("In")))
                    .resolve(|context| {
                        Box::pin(async move {
                            Ok(context
                                .args
                                .get("arg")
                                .and_then(Value::as_object)
                                .and_then(|object| object.get("xs"))
                                .cloned()
                                .unwrap_or(Value::Null))
                        })
                    }),
            ),
        )
        .query("Query")
        .build()
        .expect("valid schema");
    let response = run(&schema, "{ echo(arg: {}) }");
    println!("data={:?} errors={:?}", response.data, response.errors);
}
