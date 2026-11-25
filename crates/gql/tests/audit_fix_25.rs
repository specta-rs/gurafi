use std::panic::{AssertUnwindSafe, catch_unwind};

use futures::executor::block_on;
use gql::{
    ExecutionOptions, Field, Object, ObjectType, PathSegment, PreparedQuery, Request, Response,
    Schema, TypeRef, Value,
};

struct PanickingDrop;

impl Drop for PanickingDrop {
    fn drop(&mut self) {
        panic!("panic payload Drop escaped isolation");
    }
}

fn query_schema() -> Schema {
    Schema::builder()
        .object(
            ObjectType::new("Query")
                .field(
                    Field::new("boom", TypeRef::string())
                        .resolve(|_| Box::pin(async { std::panic::panic_any(PanickingDrop) })),
                )
                .field(Field::new("boomDropTwice", TypeRef::string()).resolve(|_| {
                    Box::pin(async { std::panic::panic_any((PanickingDrop, PanickingDrop)) })
                })),
        )
        .query("Query")
        .build()
        .expect("valid schema")
}

fn subscription_schema() -> Schema {
    Schema::builder()
        .object(ObjectType::new("Query").field(Field::new("ping", TypeRef::string())))
        .object(
            ObjectType::new("Subscription").field(
                Field::new("tick", TypeRef::string())
                    .subscribe(|_| Box::pin(async { std::panic::panic_any(PanickingDrop) })),
            ),
        )
        .query("Query")
        .subscription("Subscription")
        .build()
        .expect("valid schema")
}

fn assert_internal(response: &Response, expected_path: Option<&[PathSegment]>) {
    assert_eq!(response.errors.len(), 1, "{response:?}");
    let error = &response.errors[0];
    assert_eq!(error.message(), "Internal server error");
    assert_eq!(
        error
            .extensions()
            .and_then(|extensions| extensions.get("code"))
            .and_then(Value::as_str),
        Some("INTERNAL_SERVER_ERROR")
    );
    assert_eq!(error.path(), expected_path);
}

fn assert_query_response(response: Response) {
    assert_internal(&response, Some(&[PathSegment::field("boom")]));
    assert_eq!(
        response.data,
        Some(Value::Object(
            Object::from_fields([("boom", Value::Null)]).into()
        ))
    );
}

fn assert_setup_response(response: Response) {
    assert_internal(&response, Some(&[PathSegment::field("tick")]));
    assert_eq!(response.data, None);
}

#[test]
fn caught_panic_payload_drop_does_not_unwind_execute() {
    let schema = query_schema();
    let prepared =
        PreparedQuery::new(&schema, "panic-drop", "{ boom }", None).expect("prepared query");

    let dynamic = catch_unwind(AssertUnwindSafe(|| {
        block_on(gql::execute(
            &schema,
            Request::new("{ boom }"),
            &ExecutionOptions::default(),
        ))
    }))
    .unwrap_or_else(|_| panic!("dynamic execute unwound while disposing the panic payload"));
    assert_query_response(dynamic);

    let planned = catch_unwind(AssertUnwindSafe(|| {
        block_on(prepared.execute(None, &ExecutionOptions::default()))
    }))
    .unwrap_or_else(|_| panic!("prepared execute unwound while disposing the panic payload"));
    assert_query_response(planned);
}

#[test]
fn payload_with_multiple_panicking_destructors_cannot_abort_the_process() {
    let schema = query_schema();
    let response = block_on(gql::execute(
        &schema,
        Request::new("{ boomDropTwice }"),
        &ExecutionOptions::default(),
    ));
    assert_internal(&response, Some(&[PathSegment::field("boomDropTwice")]));
}

#[test]
fn caught_subscription_payload_drop_returns_setup_error() {
    let schema = subscription_schema();
    let prepared = PreparedQuery::new(
        &schema,
        "panic-drop-subscription",
        "subscription { tick }",
        None,
    )
    .expect("prepared subscription");

    let normal = catch_unwind(AssertUnwindSafe(|| {
        block_on(gql::subscribe(
            &schema,
            Request::new("subscription { tick }"),
            &ExecutionOptions::default(),
        ))
    }))
    .unwrap_or_else(|_| panic!("subscription setup unwound while disposing the panic payload"));
    let response = match normal {
        Err(response) => response,
        Ok(_) => panic!("subscription setup unexpectedly succeeded"),
    };
    assert_setup_response(response);

    let planned = catch_unwind(AssertUnwindSafe(|| {
        block_on(prepared.subscribe(None, &ExecutionOptions::default()))
    }))
    .unwrap_or_else(|_| panic!("prepared setup unwound while disposing the panic payload"));
    let response = match planned {
        Err(response) => response,
        Ok(_) => panic!("prepared subscription setup unexpectedly succeeded"),
    };
    assert_setup_response(response);
}
