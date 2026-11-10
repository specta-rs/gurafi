//! Schema composition: merging, filtering and field transforms.
//!
//! Every test builds two real schemas (or extends one) and inspects the rebuilt result, so a
//! merge that silently drops a type, a directive or a resolver fails here rather than in a
//! downstream application.

use gql::{
    ExecutionOptions, Field, FieldTransform, ObjectType, Request, Schema, SchemaTransform, Type,
    TypeRef, Value, execute,
};

fn sdl(source: &str) -> Schema {
    Schema::from_sdl(source).expect("valid SDL schema")
}

fn query_schema(field: Field) -> Schema {
    let mut builder = Schema::builder();
    builder.add_type(ObjectType::new("Query").field(field).into());
    builder.query("Query").build().expect("valid schema")
}

fn response_string(response: &gql::Response, key: &str) -> String {
    response
        .data
        .as_ref()
        .and_then(Value::as_object)
        .and_then(|data| data.get(key))
        .and_then(Value::as_str)
        .unwrap_or_else(|| panic!("missing string response field {key}: {response:?}"))
        .to_owned()
}

#[test]
fn merge_combines_disjoint_types_and_roots() {
    let base = sdl("type Query { shared: String }");
    let extension = sdl("type Query { shared: String }
         type Extra { value: String }
         type Mutation { write: String }
         type Subscription { tick: String }
         schema { query: Query mutation: Mutation subscription: Subscription }");

    let merged = base.merge(&extension).expect("compatible schemas merge");
    assert!(merged.get_type("Extra").is_some());
    assert_eq!(
        merged.mutation_name().map(|name| name.as_ref()),
        Some("Mutation")
    );
    assert_eq!(
        merged.subscription_name().map(|name| name.as_ref()),
        Some("Subscription")
    );
    assert_eq!(merged.query_name().as_ref(), "Query");
}

#[test]
fn merge_of_identical_schemas_keeps_resolvers() {
    let schema = query_schema(
        Field::new("hello", TypeRef::string())
            .resolve(|_| Box::pin(async { Ok(Value::from("resolved")) })),
    );

    let merged = schema.merge(&schema).expect("a schema merges with itself");
    let response = futures::executor::block_on(execute(
        &merged,
        Request::new("{ hello }"),
        &ExecutionOptions::default(),
    ));

    assert!(response.errors.is_empty(), "{response:?}");
    assert_eq!(response_string(&response, "hello"), "resolved");
}

#[test]
fn merge_preserves_resolvers_from_either_side() {
    let without = sdl("type Query { hello: String }");
    let with = query_schema(
        Field::new("hello", TypeRef::string())
            .resolve(|_| Box::pin(async { Ok(Value::from("from-resolver")) })),
    );

    for merged in [
        without.merge(&with).expect("merge without -> with"),
        with.merge(&without).expect("merge with -> without"),
    ] {
        let response = futures::executor::block_on(execute(
            &merged,
            Request::new("{ hello }"),
            &ExecutionOptions::default(),
        ));
        assert!(response.errors.is_empty(), "{response:?}");
        assert_eq!(response_string(&response, "hello"), "from-resolver");
    }
}

#[test]
fn merge_rejects_conflicting_type_definitions() {
    let left = sdl("type Query { value: String }");
    let right = sdl("type Query { value: Int }");

    let error = left.merge(&right).expect_err("field type conflict");
    let message = error.to_string();
    assert!(message.contains("type \"Query\""), "{message}");
    assert!(message.contains("defined differently"), "{message}");
}

#[test]
fn merge_rejects_a_different_type_kind() {
    let left = sdl("type Query { value: Item } type Item { x: String }");
    let right = sdl("type Query { value: Item } scalar Item");

    let error = left.merge(&right).expect_err("kind conflict");
    let message = error.to_string();
    assert!(message.contains("type \"Item\""), "{message}");
    assert!(message.contains("different kind"), "{message}");
}

#[test]
fn merge_rejects_conflicting_directives() {
    let left = sdl("directive @tag(name: String) on FIELD_DEFINITION
         type Query { value: String }");
    let right = sdl("directive @tag(name: Int) on FIELD_DEFINITION
         type Query { value: String }");

    let error = left.merge(&right).expect_err("directive conflict");
    let message = error.to_string();
    assert!(message.contains("@tag"), "{message}");
    assert!(message.contains("defined differently"), "{message}");
}

#[test]
fn merge_accepts_identical_directives_interfaces_and_objects() {
    let left = sdl("directive @tag on OBJECT
         interface Node { id: ID! }
         type Query { node: Node }");
    let right = sdl("directive @tag on OBJECT
         interface Node { id: ID! }
         type User implements Node { id: ID! }
         type Query { node: Node }");

    let merged = left.merge(&right).expect("compatible composition");
    assert!(merged.get_directive("tag").is_some());
    let node = merged
        .interface_type("Node")
        .expect("interface survives composition");
    assert_eq!(node.fields.len(), 1);
    let user = merged.object_type("User").expect("new object survives");
    assert_eq!(user.interfaces.len(), 1);
}

#[test]
fn merge_combines_schema_description_and_applied_directives() {
    let left = Schema::from_sdl(
        "directive @tag on SCHEMA
         schema @tag { query: Query }
         type Query { value: String }",
    )
    .expect("left schema");
    let right = Schema::from_sdl("type Query { value: String }").expect("right schema");

    let merged = left.merge(&right).expect("schema directives merge");
    assert_eq!(merged.applied_directives().len(), 1);
    assert_eq!(merged.applied_directives()[0].name.as_ref(), "tag");
}

#[test]
fn without_types_removes_unreferenced_types() {
    let schema = sdl("type Query { value: Kept }
         type Kept { value: String }
         type Removed { value: String }");

    let trimmed = schema
        .without_types(&["Removed"])
        .expect("removal is valid");
    assert!(trimmed.get_type("Removed").is_none());
    assert!(trimmed.get_type("Kept").is_some());
    assert!(trimmed.query_type().get_field("value").is_some());
}

#[test]
fn removing_a_referenced_type_fails_validation() {
    let schema = sdl("type Query { value: Kept }
         type Kept { value: String }");

    let error = schema
        .without_types(&["Kept"])
        .expect_err("Kept is still referenced");
    assert!(error.to_string().contains("Kept"), "{error}");
}

#[test]
fn retain_types_builds_an_allow_list() {
    let schema = sdl("type Query { value: Kept }
         type Kept { value: String }
         type Removed { value: String }");

    let trimmed = schema
        .retain_types(|ty| matches!(ty.name().as_ref(), "Query" | "Kept"))
        .expect("allow list is valid");
    assert!(trimmed.get_type("Removed").is_none());
    assert!(trimmed.get_type("Kept").is_some());
}

#[test]
fn schema_transform_applies_a_type_filter() {
    let schema = sdl("type Query { value: Kept }
         type Kept { value: String }
         type Removed { value: String }");

    let trimmed = SchemaTransform::new()
        .remove("Removed")
        .apply(&schema)
        .expect("filter is valid");
    assert!(trimmed.get_type("Removed").is_none());

    let allow_listed = SchemaTransform::new()
        .keep("Query")
        .keep("Kept")
        .apply(&schema)
        .expect("allow list is valid");
    assert!(allow_listed.get_type("Removed").is_none());
    assert!(allow_listed.get_type("Kept").is_some());
}

#[test]
fn transform_removes_and_replaces_fields() {
    let schema = sdl("type Query { keep: String drop: String old: String }");

    let transformed = schema
        .transform(|_, field| match field.name.as_ref() {
            "drop" => FieldTransform::Remove,
            "old" => FieldTransform::Replace(Field::new("old", TypeRef::named("Int"))),
            _ => FieldTransform::Keep,
        })
        .expect("field transform is valid");

    let query = transformed.query_type();
    assert!(query.get_field("keep").is_some());
    assert!(query.get_field("drop").is_none());
    let old = query.get_field("old").expect("replaced field survives");
    assert_eq!(old.ty, TypeRef::named("Int"));
}

#[test]
fn transform_rejects_renaming_a_field() {
    let schema = sdl("type Query { value: String }");

    let error = schema
        .transform(|_, _| FieldTransform::Replace(Field::new("renamed", TypeRef::string())))
        .expect_err("renaming is rejected");
    assert!(error.to_string().contains("keep the field name"), "{error}");
}

#[test]
fn extend_sdl_preserves_existing_resolvers() {
    let schema = query_schema(
        Field::new("hello", TypeRef::string())
            .resolve(|_| Box::pin(async { Ok(Value::from("world")) })),
    );

    let extended = schema
        .extend_sdl("extend type Query { extra: String } type Added { value: String }")
        .expect("extension applies");

    assert!(extended.get_type("Added").is_some());
    let hello = extended
        .query_type()
        .get_field("hello")
        .expect("original field survives");
    assert!(hello.resolve.is_some());
    assert!(extended.query_type().get_field("extra").is_some());

    let response = futures::executor::block_on(execute(
        &extended,
        Request::new("{ hello extra }"),
        &ExecutionOptions::default(),
    ));
    assert!(response.errors.is_empty(), "{response:?}");
    assert_eq!(response_string(&response, "hello"), "world");
}

#[test]
fn builder_merge_composes_before_build() {
    let other = sdl("type Query { value: String } type Other { value: String }");

    let mut builder = Schema::builder();
    builder.add_type(
        ObjectType::new("Query")
            .field(Field::new("value", TypeRef::string()))
            .into(),
    );
    builder = builder.query("Query");
    builder.merge(&other);

    let schema = builder.build().expect("builder composition is valid");
    assert!(schema.get_type("Other").is_some());
    assert_eq!(schema.query_name().as_ref(), "Query");
}

#[test]
fn merge_rejects_conflicting_root_names() {
    let left = sdl("type Query { value: String }");
    let right = sdl("type Root { value: String } schema { query: Root }");

    let error = left.merge(&right).expect_err("different query roots");
    let message = error.to_string();
    assert!(message.contains("query root"), "{message}");
    assert!(message.contains("Query"), "{message}");
    assert!(message.contains("Root"), "{message}");
}

#[test]
fn merge_accepts_identical_interface_implementations() {
    let left = sdl("interface Node { id: ID! }
         type User implements Node { id: ID! }
         type Query { user: User }");
    let right = left.clone();

    let merged = left.merge(&right).expect("identical interfaces merge");
    let user = merged.object_type("User").expect("User survives");
    assert_eq!(user.interfaces.len(), 1);
    assert!(matches!(merged.get_type("Node"), Some(Type::Interface(_))));
}
