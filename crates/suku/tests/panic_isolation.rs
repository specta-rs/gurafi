//! Error-formatter coverage for resolver panic isolation.

use gql::{Field, ObjectType, Schema, TypeRef, Value};
use suku::{Context, ErrorKind, GraphQLRequest, ResponseExt as _, Server, mask_internal};

fn schema() -> Schema {
    Schema::builder()
        .object(
            ObjectType::new("Query").field(
                Field::new("boom", TypeRef::string())
                    .resolve(|_| Box::pin(async { panic!("database password leaked") })),
            ),
        )
        .query("Query")
        .build()
        .expect("valid schema")
}

fn code(error: &gql::GraphQLError) -> Option<&str> {
    error.extensions()?.get("code").and_then(Value::as_str)
}

#[tokio::test]
async fn formatter_sees_the_coded_error_and_mask_internal_keeps_it() {
    let server = Server::new(schema()).format_error(|error, context| {
        // The formatter runs after execution: by then the panic is an ordinary field error.
        assert_eq!(context.kind(), ErrorKind::Execution);
        assert_eq!(error.message(), "Internal server error");
        assert_eq!(code(error), Some("INTERNAL_SERVER_ERROR"));
        assert_eq!(
            error.path().map(<[_]>::to_vec),
            Some(vec![gql::PathSegment::field("boom")])
        );
        mask_internal(error, context)
    });

    let response = server
        .execute(&GraphQLRequest::new("{ boom }"), Context::empty())
        .await;

    assert_eq!(response.errors.len(), 1);
    assert_eq!(response.errors[0].message(), "Internal server error");
    assert_eq!(code(&response.errors[0]), Some("INTERNAL_SERVER_ERROR"));
    assert_eq!(
        response.errors[0].path().map(<[_]>::to_vec),
        Some(vec![gql::PathSegment::field("boom")])
    );

    let json = response.to_json_string();
    assert!(json.contains("INTERNAL_SERVER_ERROR"), "{json}");
    assert!(
        !json.contains("database password leaked"),
        "the panic payload must not reach the client: {json}"
    );
}
