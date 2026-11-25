//! Regression coverage for derived `@oneOf` input validation.

use std::sync::{
    Arc,
    atomic::{AtomicUsize, Ordering},
};

use gurafi::{FromValue, Input, gql};

#[derive(Debug, Input)]
#[graphql(name = "AuditFix26OneOfInput", one_of)]
struct AuditFix26OneOfInput {
    by_id: Option<String>,
    by_name: Option<String>,
}

fn input(fields: impl IntoIterator<Item = (&'static str, gql::Value)>) -> gql::Value {
    gql::Value::Object(Arc::new(gql::Object::from_fields(fields)))
}

#[test]
fn one_of_from_value_requires_exactly_one_non_null_field() {
    let valid = input([("by_id", gql::Value::String(Arc::from("42")))]);
    let parsed = <AuditFix26OneOfInput as FromValue>::from_value(&valid)
        .expect("one non-null field must remain valid");
    assert_eq!(parsed.by_id.as_deref(), Some("42"));
    assert_eq!(parsed.by_name, None);

    let invalid = [
        ("zero fields", input([])),
        (
            "two non-null fields",
            input([
                ("by_id", gql::Value::String(Arc::from("42"))),
                ("by_name", gql::Value::String(Arc::from("Ada"))),
            ]),
        ),
        (
            "one explicit null plus one non-null field",
            input([
                ("by_id", gql::Value::Null),
                ("by_name", gql::Value::String(Arc::from("Ada"))),
            ]),
        ),
        (
            "one explicit null field",
            input([("by_id", gql::Value::Null)]),
        ),
    ];

    for (label, value) in invalid {
        let error = <AuditFix26OneOfInput as FromValue>::from_value(&value)
            .expect_err(&format!("{label} must be rejected"));
        assert!(
            error.message().contains("exactly one non-null field"),
            "unexpected error for {label}: {error:?}"
        );
    }
}

static AUTHOR_VALIDATION_CALLS: AtomicUsize = AtomicUsize::new(0);

fn count_field_validation(_: &Option<String>) -> Result<(), gql::GraphQLError> {
    AUTHOR_VALIDATION_CALLS.fetch_add(1, Ordering::SeqCst);
    Ok(())
}

fn count_type_validation(_: &AuditFix26ValidatedOneOfInput) -> Result<(), gql::GraphQLError> {
    AUTHOR_VALIDATION_CALLS.fetch_add(1, Ordering::SeqCst);
    Ok(())
}

#[derive(Debug, Input)]
#[graphql(
    name = "AuditFix26ValidatedOneOfInput",
    one_of,
    validate = "count_type_validation"
)]
struct AuditFix26ValidatedOneOfInput {
    #[graphql(validate = "count_field_validation")]
    by_id: Option<String>,
    #[graphql(validate = "count_field_validation")]
    by_name: Option<String>,
}

#[test]
fn one_of_cardinality_is_checked_before_author_validators() {
    AUTHOR_VALIDATION_CALLS.store(0, Ordering::SeqCst);

    let error = <AuditFix26ValidatedOneOfInput as FromValue>::from_value(&input([]))
        .expect_err("zero selected fields must be rejected");
    assert!(
        error.message().contains("exactly one non-null field"),
        "cardinality must fail before author validation: {error:?}"
    );
    assert_eq!(AUTHOR_VALIDATION_CALLS.load(Ordering::SeqCst), 0);

    let valid = input([("by_id", gql::Value::String(Arc::from("42")))]);
    assert!(
        <AuditFix26ValidatedOneOfInput as FromValue>::from_value(&valid).is_ok(),
        "author validators must still accept a valid selection"
    );
    assert_eq!(
        AUTHOR_VALIDATION_CALLS.load(Ordering::SeqCst),
        3,
        "both field validators and the type validator must run for valid input"
    );
}
