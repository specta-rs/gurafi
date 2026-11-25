use std::{fmt::Write as _, time::Instant};

use gql::{GraphQLError, Schema, ValidationOptions, parser::parse_query, validate};

fn schema() -> Schema {
    Schema::from_sdl(
        "type Query { x: String left: String right: Int box: Box } type Box { x: String }",
    )
    .expect("valid schema")
}

fn options() -> ValidationOptions {
    ValidationOptions {
        introspection: true,
        max_depth: 32,
        max_errors: 100,
    }
}

fn validate_source(source: &str) -> Vec<GraphQLError> {
    let document = parse_query(source).expect("document parses");
    validate(&schema(), &document, &options())
}

fn unique_alias_query(count: usize) -> String {
    let mut source = String::with_capacity(count * 12 + 3);
    source.push('{');
    for index in 0..count {
        write!(&mut source, " a{index}: x").expect("write to query");
    }
    source.push_str(" }");
    source
}

fn fragment_query(count: usize) -> String {
    let mut source = String::from("{");
    for index in 0..count {
        write!(&mut source, " ...F{index}").expect("write to query");
    }
    source.push_str(" }");
    for index in 0..count {
        write!(&mut source, " fragment F{index} on Query {{ a{index}: x }}")
            .expect("write to query");
    }
    source
}

#[test]
fn response_key_precheck_uses_bounded_membership_without_changing_conflicts() {
    let source = unique_alias_query(32_000);
    let document = parse_query(&source).expect("document parses");
    let schema = schema();
    let started = Instant::now();
    let errors = validate(&schema, &document, &options());
    let elapsed = started.elapsed();
    assert!(
        errors.is_empty(),
        "unique aliases must be valid: {errors:?}"
    );
    assert!(
        elapsed.as_secs_f64() < 5.0,
        "response-key precheck exceeded its work budget: {elapsed:?}"
    );

    assert!(validate_source(&fragment_query(2_048)).is_empty());
    assert!(validate_source("{ a: box { x } b: box { x } }").is_empty());

    let errors = validate_source("{ x: left ...F ...F } fragment F on Query { x: right }");
    assert_eq!(errors.len(), 1, "{errors:?}");
    assert!(
        errors[0]
            .message()
            .contains("they have differing field names"),
        "{errors:?}"
    );
    assert_eq!(errors[0].locations().len(), 2);
}
