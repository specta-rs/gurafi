//! Batch SDL extension: one print/parse pass for several documents.

use gql::{Field, ObjectType, Schema, TypeRef, Value};

fn base() -> Schema {
    let mut builder = Schema::builder();
    builder.add_type(
        ObjectType::new("Query")
            .field(
                Field::new("hello", TypeRef::string())
                    .resolve(|_| Box::pin(async { Ok(Value::from("resolved")) })),
            )
            .into(),
    );
    builder.query("Query").build().expect("valid schema")
}

const DOCUMENTS: [&str; 3] = [
    "type Extra { value: String }",
    "extend type Query { extra: Extra }",
    "type More { count: Int }",
];

#[test]
fn batch_extension_matches_sequential_extension() {
    let base = base();
    let mut sequential = base.clone();
    for document in DOCUMENTS {
        sequential = sequential
            .extend_sdl(document)
            .expect("sequential extension");
    }
    let batched = base.extend_sdl_many(DOCUMENTS).expect("batched extension");

    assert_eq!(batched.sdl(), sequential.sdl());
    assert_eq!(
        batched.query_type().get_field("extra").is_some(),
        sequential.query_type().get_field("extra").is_some()
    );
    assert!(batched.get_type("More").is_some());
}

#[test]
fn batch_extension_preserves_resolvers() {
    let extended = base()
        .extend_sdl_many(DOCUMENTS)
        .expect("batched extension");
    let hello = extended.query_type().get_field("hello").expect("hello");
    assert!(
        hello.resolve.is_some(),
        "batch extension dropped a resolver that extend_sdl preserves"
    );
}

#[test]
fn batch_extension_with_no_documents_keeps_the_schema() {
    let base = base();
    let extended = base.extend_sdl_many([]).expect("empty batch");
    assert_eq!(extended.sdl(), base.sdl());
}

#[test]
fn batch_extension_reports_errors_from_any_document() {
    let error = base()
        .extend_sdl_many([
            "type Extra { value: String }",
            "extend type Missing { x: String }",
        ])
        .expect_err("unknown extension target");
    assert!(error.to_string().contains("Missing"), "{error}");
}
