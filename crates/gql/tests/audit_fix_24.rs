use std::{
    fmt::Write as _,
    time::{Duration, Instant},
};

use gql::{GraphQLError, Schema, ValidationOptions, parser::parse_query, validate};

fn schema() -> Schema {
    Schema::from_sdl("type Query { sum(values: [Int!]!): Int }").expect("valid schema")
}

fn options() -> ValidationOptions {
    ValidationOptions {
        introspection: true,
        max_errors: 100,
        ..Default::default()
    }
}

fn repeated_usage_query(definitions: usize, usages: usize) -> String {
    let mut source = String::from("query Q(");
    for index in 0..definitions {
        if index != 0 {
            source.push(',');
        }
        write!(&mut source, "$v{index}: Int!").expect("write to query");
    }
    source.push_str(") { sum(values: [");
    for index in 0..usages {
        if index != 0 {
            source.push(',');
        }
        write!(&mut source, "$v{}", definitions - 1).expect("write to query");
    }
    source.push_str("]) }");
    source
}

fn distinct_usage_query(count: usize) -> String {
    let mut source = String::from("query Q(");
    for index in 0..count {
        if index != 0 {
            source.push(',');
        }
        write!(&mut source, "$v{index}: Int!").expect("write to query");
    }
    source.push_str(") { sum(values: [");
    for index in 0..count {
        if index != 0 {
            source.push(',');
        }
        write!(&mut source, "$v{index}").expect("write to query");
    }
    source.push_str("]) }");
    source
}

fn validate_source(source: &str) -> Vec<GraphQLError> {
    let document = parse_query(source).expect("document parses");
    validate(&schema(), &document, &options())
}

fn measure_repeated_usages(count: usize) -> Duration {
    let source = repeated_usage_query(count, count);
    let document = parse_query(&source).expect("document parses");
    let schema = schema();
    let mut best = Duration::MAX;
    for _ in 0..3 {
        let started = Instant::now();
        for _ in 0..5 {
            let errors = validate(&schema, &document, &options());
            assert_eq!(errors.len(), 100, "max_errors must cap unused variables");
        }
        best = best.min(started.elapsed() / 5);
    }
    best
}

#[test]
fn unused_variable_check_is_linear_and_preserves_variable_errors() {
    let small = measure_repeated_usages(1_000);
    let large = measure_repeated_usages(4_000);
    assert!(
        large < small * 8,
        "unused-variable validation grew superlinearly: 1000={small:?}, 4000={large:?}"
    );

    let errors = validate_source(&repeated_usage_query(4_000, 4_000));
    assert_eq!(errors.len(), 100);
    for (index, error) in errors.iter().enumerate() {
        assert_eq!(
            error.message(),
            format!("Variable \"$v{index}\" is never used in operation \"Q\".")
        );
    }
    assert!(
        errors
            .iter()
            .all(|error| !error.message().contains("$v3999")),
        "the repeatedly used variable must not be reported as unused"
    );

    assert!(validate_source(&distinct_usage_query(1_024)).is_empty());
    assert!(
        validate_source("query Q($x: Int!) { ...F } fragment F on Query { sum(values: [$x]) }")
            .is_empty()
    );

    let errors = validate_source(
        "query Q($used: Int!, $unused: String!, $bad: Int) { sum(values: [$used, $missing, $bad]) }",
    );
    let messages = errors.iter().map(GraphQLError::message).collect::<Vec<_>>();
    assert_eq!(
        messages,
        vec![
            "Variable \"$missing\" is not defined by operation \"Q\".",
            "Variable \"$unused\" is never used in operation \"Q\".",
            "Variable \"$bad\" of type \"Int\" used in position expecting type \"Int!\".",
        ]
    );
}
