//! Security / denial-of-service regression tests.
//!
//! Each `FINDING` test demonstrates a behaviour that is unsafe for a server exposed to
//! untrusted clients and currently passes (i.e. the problem is present). The remaining tests
//! pin protections that the engine does provide, so a regression shows up here.
//!
//! Two tests abort the process on purpose (stack overflow) and are `#[ignore]`d; run them on
//! their own:
//!
//! ```text
//! cargo test -p gql --test security -- --ignored --nocapture
//! ```

use std::sync::{Arc, Mutex};

use futures::executor::block_on;
use gql::{
    AuthorizationContext, Authorizer, ExecutionOptions, Field, GraphQLError, InterfaceType,
    ObjectType, Request, Response, Schema, TypeRef, ValidationOptions, Value, validate,
};

// -------------------------------------------------------------------------------------------
// Fixtures
// -------------------------------------------------------------------------------------------

/// Records every `(parent_type, field_name)` pair the authorizer is asked about.
#[derive(Clone)]
struct RecordingAuthorizer {
    seen: Arc<Mutex<Vec<String>>>,
    deny: Arc<dyn Fn(&AuthorizationContext<'_>) -> bool + Send + Sync>,
}

impl RecordingAuthorizer {
    fn denying_all() -> Self {
        Self {
            seen: Arc::default(),
            deny: Arc::new(|_| true),
        }
    }

    fn seen(&self) -> Vec<String> {
        self.seen.lock().expect("authorizer log").clone()
    }
}

impl Authorizer for RecordingAuthorizer {
    fn authorize(&self, context: &AuthorizationContext<'_>) -> Result<(), GraphQLError> {
        self.seen
            .lock()
            .expect("authorizer log")
            .push(format!("{}.{}", context.parent_type, context.field_name));
        if (self.deny)(context) {
            Err(GraphQLError::new("denied"))
        } else {
            Ok(())
        }
    }
}

/// `Query { hello: String! ping: String }`.
fn simple_schema() -> Schema {
    Schema::builder()
        .object(
            ObjectType::new("Query")
                .field(
                    Field::new("hello", TypeRef::string().non_null())
                        .resolve(|_| Box::pin(async { Ok(Value::from("world")) })),
                )
                .field(
                    Field::new("ping", TypeRef::string())
                        .resolve(|_| Box::pin(async { Ok(Value::from("pong")) })),
                ),
        )
        .query("Query")
        .build()
        .expect("valid schema")
}

/// A schema where a field can be walked to any depth: `Query.a: Query`.
fn recursive_schema() -> Schema {
    Schema::builder()
        .object(
            ObjectType::new("Query")
                .field(
                    Field::new("a", TypeRef::named("Query"))
                        .resolve(|_| Box::pin(async { Ok(Value::object(gql::Object::new())) })),
                )
                .field(
                    Field::new("ping", TypeRef::string())
                        .resolve(|_| Box::pin(async { Ok(Value::from("pong")) })),
                ),
        )
        .query("Query")
        .build()
        .expect("valid schema")
}

/// `Query { node: Node!, users: [User!]!, public, secret }`, `Node { id }`,
/// `User { id name secret }`.
fn abstract_schema() -> Schema {
    let node = InterfaceType::new("Node").field(Field::new("id", TypeRef::id().non_null()));

    let user = ObjectType::new("User")
        .implements("Node")
        .field(
            Field::new("id", TypeRef::id().non_null())
                .resolve(|_| Box::pin(async { Ok(Value::from("1")) })),
        )
        .field(
            Field::new("name", TypeRef::string())
                .resolve(|_| Box::pin(async { Ok(Value::from("Ada")) })),
        )
        .field(
            Field::new("secret", TypeRef::string())
                .resolve(|_| Box::pin(async { Ok(Value::from("hunter2")) })),
        )
        .is_type_of(|value| value.as_object().is_some());

    let query = ObjectType::new("Query")
        .field(
            Field::new("users", TypeRef::named("User").non_null().list().non_null()).resolve(
                |_| {
                    Box::pin(async {
                        Ok(Value::List(vec![
                            Value::object(gql::Object::new()),
                            Value::object(gql::Object::new()),
                            Value::object(gql::Object::new()),
                        ]))
                    })
                },
            ),
        )
        .field(
            Field::new("node", TypeRef::named("Node").non_null())
                .resolve(|_| Box::pin(async { Ok(Value::object(gql::Object::new())) })),
        )
        .field(
            Field::new("public", TypeRef::string())
                .resolve(|_| Box::pin(async { Ok(Value::from("ok")) })),
        )
        .field(
            Field::new("secret", TypeRef::string())
                .resolve(|_| Box::pin(async { Ok(Value::from("hunter2")) })),
        );

    let mut builder = Schema::builder();
    builder.add_type(node.into());
    builder.add_type(user.into());
    builder.add_type(query.into());
    builder.query("Query").build().expect("valid schema")
}

fn run(schema: &Schema, query: &str, options: &ExecutionOptions) -> Response {
    block_on(gql::execute(schema, Request::new(query), options))
}

fn analyzer_options(max_complexity: u32) -> gql::complexity::ComplexityOptions {
    gql::complexity::ComplexityOptions {
        list_factor: 10,
        field_cost: 1,
        max_complexity,
        max_depth: 0,
        max_breadth: 0,
    }
}

// -------------------------------------------------------------------------------------------
// 1. Authorization
// -------------------------------------------------------------------------------------------

/// FINDING: meta-fields are resolved before the authorizer is consulted. An authorizer that
/// denies everything still answers `__typename`, `__schema` and `__type` (the introspection
/// tree's own fields are authorized). The meta-field entry points are authorized like normal
/// fields, so a deny-everything authorizer also denies `__typename`, `__schema` and `__type`.
#[test]
fn authorizer_also_covers_meta_fields() {
    let schema = simple_schema();
    let authorizer = RecordingAuthorizer::denying_all();
    let options = ExecutionOptions::default().authorizer(authorizer.clone());

    // Baseline: a normal field is denied and recorded.
    let response = run(&schema, "{ hello }", &options);
    assert_eq!(
        response.errors.first().map(GraphQLError::message),
        Some("denied")
    );
    assert!(
        authorizer.seen().iter().any(|seen| seen == "Query.hello"),
        "authorizer should have been consulted: {:?}",
        authorizer.seen()
    );

    for query in [
        "{ __typename }",
        "{ __schema { description } }",
        r#"{ __type(name: "Query") { name } }"#,
    ] {
        let response = run(&schema, query, &options);
        assert!(
            response
                .errors
                .iter()
                .any(|error| error.message() == "denied"),
            "expected meta-field in {query} to be denied: {:?}",
            response.errors
        );
    }
    assert!(
        authorizer
            .seen()
            .iter()
            .any(|seen| seen == "Query.__typename"),
        "{:?}",
        authorizer.seen()
    );
    assert!(
        authorizer
            .seen()
            .iter()
            .any(|seen| seen == "Query.__schema"),
        "{:?}",
        authorizer.seen()
    );
    assert!(
        authorizer.seen().iter().any(|seen| seen == "Query.__type"),
        "{:?}",
        authorizer.seen()
    );
}

/// Control: the authorizer runs for every field resolution reached through aliases, fragments,
/// repeated selections, list items and abstract types.
#[test]
fn authorizer_runs_for_aliases_fragments_lists_and_abstract_types() {
    let schema = abstract_schema();
    let authorizer = RecordingAuthorizer {
        seen: Arc::default(),
        deny: Arc::new(|context| context.field_name == "secret"),
    };
    let options = ExecutionOptions::default().authorizer(authorizer.clone());

    // Alias + fragment + duplicate selection under different aliases: all three resolutions of
    // `secret` must be denied.
    let response = run(
        &schema,
        "query { a: secret s: secret ...F } fragment F on Query { b: secret }",
        &options,
    );
    let data = response
        .data
        .as_ref()
        .and_then(Value::as_object)
        .expect("data");
    for key in ["a", "s", "b"] {
        assert_eq!(data.get(key), Some(&Value::Null), "alias {key}");
    }
    assert_eq!(
        response
            .errors
            .iter()
            .filter(|error| error.message() == "denied")
            .count(),
        3,
        "{:?}",
        response.errors
    );

    // List items: one authorization decision per resolved item.
    let response = run(&schema, "{ users { secret } }", &options);
    assert_eq!(
        response
            .errors
            .iter()
            .filter(|error| error.message() == "denied")
            .count(),
        3,
        "each list item must be authorized: {:?}",
        response.errors
    );
    assert!(authorizer.seen().iter().any(|seen| seen == "User.secret"));

    // Abstract type reached through an inline fragment.
    let response = run(&schema, "{ node { ... on User { secret } } }", &options);
    assert!(
        response
            .errors
            .iter()
            .any(|error| error.message() == "denied"),
        "abstract-type field must be authorized: {:?}",
        response.errors
    );
}

// -------------------------------------------------------------------------------------------
// 2. Parsing / validation limits (regressions)
// -------------------------------------------------------------------------------------------

/// The parser caps nesting, so a deep document is rejected instead of exhausting the stack.
#[test]
fn deeply_nested_query_is_rejected_by_the_parser() {
    let schema = recursive_schema();
    let depth = 10_000;
    let mut query = String::new();
    for _ in 0..depth {
        query.push_str("{ a ");
    }
    query.push_str("{ ping }");
    for _ in 0..depth {
        query.push_str(" }");
    }

    let response = run(&schema, &query, &ExecutionOptions::default());
    assert_eq!(response.data, None);
    assert!(
        response
            .errors
            .iter()
            .any(|error| error.message().contains("nested more than")),
        "{:?}",
        response.errors
    );
}

/// List/object literals are depth-limited too, so a deeply nested input value cannot overflow
/// the stack in the parser.
#[test]
fn deeply_nested_input_literal_is_rejected_by_the_parser() {
    let mut query = String::from("{ ping(n: ");
    for _ in 0..10_000 {
        query.push('[');
    }
    for _ in 0..10_000 {
        query.push(']');
    }
    query.push_str(") }");

    let error = gql::parser::parse_query(&query).expect_err("parser rejects the document");
    assert!(
        error.message().contains("nested more than"),
        "{}",
        error.message()
    );
}

/// A long chain of fragment spreads is rejected before validation recurses through it.
#[test]
fn long_fragment_chain_is_rejected() {
    fn chain(n: usize) -> String {
        let mut query = String::new();
        for level in 1..n {
            query.push_str(&format!(
                "fragment F{level} on Query {{ ...F{} }}\n",
                level + 1
            ));
        }
        query.push_str(&format!("fragment F{n} on Query {{ ping }}\n{{ ...F1 }}"));
        query
    }

    let schema = recursive_schema();
    let response = run(&schema, &chain(2_000), &ExecutionOptions::default());
    assert_eq!(response.data, None);
    assert!(
        response
            .errors
            .iter()
            .any(|error| error.message().contains("Fragment nesting")),
        "{:?}",
        response.errors
    );
}

/// Validation errors obey `max_errors` (an unbounded error vector is itself a DoS vector).
#[test]
fn validation_errors_respect_max_errors() {
    let schema = simple_schema();
    let mut query = String::from("{");
    for index in 0..20_000 {
        query.push_str(&format!(" unknown{index}"));
    }
    query.push_str(" }");

    let response = run(&schema, &query, &ExecutionOptions::default().max_errors(1));
    assert_eq!(response.errors.len(), 1, "validation errors are capped");
}

/// `max_depth` is enforced by validation as well as by the analyzer.
#[test]
fn max_depth_is_enforced_by_validation() {
    let schema = recursive_schema();
    let document = gql::parser::parse_query("{ a { a { a { ping } } } }").expect("parses");
    let errors = validate(
        &schema,
        &document,
        &ValidationOptions {
            introspection: true,
            max_depth: 2,
            max_errors: 0,
        },
    );
    assert!(
        errors
            .iter()
            .any(|error| error.message().contains("depth exceeds")),
        "{errors:?}"
    );
}

/// A valid but deeply nested query is rejected by the validation depth limit before execution.
/// The document stays under `gql::parser::MAX_DEPTH` (96) so this exercises the configurable
/// `max_depth` path rather than the parser's hard backstop.
#[test]
fn deep_query_is_rejected_by_max_depth_before_execution() {
    let schema = recursive_schema();
    let depth = 50;
    assert!(
        depth < gql::parser::MAX_DEPTH,
        "keep this test under the parser limit"
    );
    let mut query = String::new();
    for _ in 0..depth {
        query.push_str("{ a ");
    }
    query.push_str("{ ping }");
    for _ in 0..depth {
        query.push_str(" }");
    }

    // The document parses; only `max_depth` rejects it.
    let document = gql::parser::parse_query(&query).expect("parses");
    assert!(
        validate(
            &schema,
            &document,
            &ValidationOptions {
                introspection: true,
                max_depth: 10,
                max_errors: 0,
            },
        )
        .iter()
        .any(|error| error.message().contains("depth exceeds"))
    );

    let response = run(&schema, &query, &ExecutionOptions::default().max_depth(10));
    assert_eq!(response.data, None);
    assert!(
        response
            .errors
            .iter()
            .any(|error| error.message().contains("depth exceeds")),
        "{:?}",
        response.errors
    );
}

/// A doubling-fragment document is analysed in bounded work because fragment expansion is
/// memoised by `(fragment, scope)`. 60 levels would be 2^60 leaf visits without the cache, so a
/// regression hangs here rather than merely running slowly.
#[test]
fn fragment_expansion_is_memoised() {
    fn doubling_query(n: usize) -> String {
        let mut query = String::from("fragment F0 on Query { ping hello }\n");
        for level in 1..=n {
            query.push_str(&format!(
                "fragment F{level} on Query {{ ...F{} ...F{} }}\n",
                level - 1,
                level - 1
            ));
        }
        query.push_str(&format!("{{ ...F{n} }}"));
        query
    }

    let schema = simple_schema();
    let source = doubling_query(60);
    let document = gql::parser::parse_query(&source).expect("parses");
    let complexity = gql::complexity::analyze(
        &schema,
        &document,
        None,
        &gql::Object::new(),
        &analyzer_options(10),
    )
    .expect("cost 2 is under the limit");

    // Both leaves merge into one response key each, so the cost is 2 however many levels the
    // fragments nest: the analyzer did not expand every path.
    assert_eq!(complexity.cost, 2, "{complexity:?}");
    assert_eq!(complexity.depth, 1, "{complexity:?}");
}

/// Sibling fields are merged through a hash index, so a flat document of aliases is analysed in
/// bounded work and reports the exact cost.
#[test]
fn analyzing_many_sibling_aliases_is_bounded() {
    let mut query = String::from("{");
    for index in 0..8_000 {
        query.push_str(&format!(" a{index}: ping"));
    }
    query.push_str(" }");

    let schema = simple_schema();
    let document = gql::parser::parse_query(&query).expect("parses");
    let complexity = gql::complexity::analyze(
        &schema,
        &document,
        None,
        &gql::Object::new(),
        &analyzer_options(0),
    )
    .expect("analysis succeeds");

    assert_eq!(complexity.cost, 8_000, "{complexity:?}");
    assert_eq!(complexity.depth, 1, "{complexity:?}");
}

// -------------------------------------------------------------------------------------------
// 3. Execution: stack and CPU exhaustion
// -------------------------------------------------------------------------------------------

/// A document that the parser accepts can always be executed on a 2 MiB stack.
///
/// The executor recurses once per selection-set level through boxed futures, so a deep enough
/// document would exhaust a Tokio worker's 2 MiB stack. `gql::parser::MAX_DEPTH` (96) is
/// deliberately below that point: nesting beyond it is a syntax error, so no document that
/// parses can abort the process during execution.
#[test]
fn deep_query_is_rejected_before_execution_on_a_small_stack() {
    let depth = gql::parser::MAX_DEPTH * 4;
    let schema = recursive_schema();
    let mut query = String::new();
    for _ in 0..depth {
        query.push_str("{ a ");
    }
    query.push_str("{ ping }");
    for _ in 0..depth {
        query.push_str(" }");
    }

    // Run on a 2 MiB stack, the default for a Tokio worker thread.
    std::thread::Builder::new()
        .stack_size(2 * 1024 * 1024)
        .spawn(move || {
            let response = run(&schema, &query, &ExecutionOptions::default());
            assert_eq!(response.data, None, "the document must not execute");
            assert_eq!(response.errors.len(), 1, "{:?}", response.errors);
            assert!(
                response.errors[0].message().contains("nested more than"),
                "{:?}",
                response.errors
            );
        })
        .expect("spawn")
        .join()
        .expect("join");
}

/// A document exactly at the parser's limit still executes, so the backstop is not overly
/// aggressive: parse/validate/analyse/execute all succeed at `MAX_DEPTH - 1` levels of nesting.
#[test]
fn query_at_the_parser_depth_limit_executes() {
    let depth = gql::parser::MAX_DEPTH - 2;
    let schema = recursive_schema();
    let mut query = String::new();
    for _ in 0..depth {
        query.push_str("{ a ");
    }
    query.push_str("{ ping }");
    for _ in 0..depth {
        query.push_str(" }");
    }

    std::thread::Builder::new()
        .stack_size(2 * 1024 * 1024)
        .spawn(move || {
            let response = run(&schema, &query, &ExecutionOptions::default());
            assert!(response.errors.is_empty(), "{:?}", response.errors);
            assert!(response.data.is_some());
        })
        .expect("spawn")
        .join()
        .expect("join");
}
