//! Review probe for the error-masking helper added with the error-formatting wave.

use gql::{Field, GraphQLError, ObjectType, Schema, TypeRef, Value};
use suku::{Context, GraphQLRequest, ResponseExt, Server, mask_internal};

fn schema() -> Schema {
    let secret = Field::new("secret", TypeRef::string().non_null()).resolve(|_| {
        Box::pin(async {
            Err::<Value, GraphQLError>(
                GraphQLError::new("connection refused at db.internal:5432")
                    .extend("stacktrace", "panicked at src/db.rs:42"),
            )
        })
    });
    let coded = Field::new("coded", TypeRef::string().non_null()).resolve(|_| {
        Box::pin(async {
            Err::<Value, GraphQLError>(GraphQLError::new("not found").extend("code", "NOT_FOUND"))
        })
    });
    let validation = Field::new("validation", TypeRef::string().non_null()).resolve(|_| {
        Box::pin(async {
            Err::<Value, GraphQLError>(GraphQLError::new(
                "Invalid arguments for \"Query.validation\": name must not be empty",
            ))
        })
    });
    let auth = Field::new("auth", TypeRef::string().non_null()).resolve(|_| {
        Box::pin(async { Err::<Value, GraphQLError>(GraphQLError::new("Sign in to read names.")) })
    });
    let query = ObjectType::new("Query")
        .field(secret)
        .field(coded)
        .field(validation)
        .field(auth);
    let mut builder = Schema::builder();
    builder.add_type(query.into());
    builder.query("Query").build().expect("valid schema")
}

#[tokio::test]
async fn mask_internal_removes_arbitrary_extension_payloads() {
    let server = Server::new(schema()).format_error(mask_internal);
    let response = server
        .execute(&GraphQLRequest::new("{ secret }"), Context::empty())
        .await;

    let json = response.to_json_string();
    assert!(
        json.contains("Internal server error"),
        "the message is masked: {json}"
    );
    assert!(
        !json.contains("db.internal") && !json.contains("stacktrace"),
        "extensions must not survive the mask: {json}"
    );
}

#[tokio::test]
async fn mask_internal_keeps_coded_errors_verbatim() {
    let server = Server::new(schema()).format_error(mask_internal);
    let response = server
        .execute(&GraphQLRequest::new("{ coded }"), Context::empty())
        .await;
    let json = response.to_json_string();
    assert!(json.contains("not found"), "{json}");
}

#[tokio::test]
async fn mask_internal_hides_client_facing_resolver_errors() {
    // `gurafi` reports argument validation and `.auth(...)` denials as resolver errors with a
    // response path. `mask_internal` classifies every uncoded error with a path as internal, so
    // these deliberate, client-facing messages disappear behind "Internal server error".
    let server = Server::new(schema()).format_error(mask_internal);
    let response = server
        .execute(
            &GraphQLRequest::new("{ validation auth }"),
            Context::empty(),
        )
        .await;
    let json = response.to_json_string();
    println!("masked client-facing errors: {json}");
    assert!(
        !json.contains("name must not be empty"),
        "validation detail should be masked as internal: {json}"
    );
    assert!(
        !json.contains("Sign in to read names"),
        "auth message should be masked as internal: {json}"
    );
}
