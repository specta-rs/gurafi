use gql::{SchemaChange, diff};

fn sdl(source: &str) -> gql::Schema {
    gql::Schema::from_sdl(source).unwrap_or_else(|error| panic!("{error}"))
}

#[test]
fn additive_changes_are_compatible() {
    let old = sdl(r#"
        type Query { user(id: ID!): User }
        type User { id: ID! }
        enum Role { USER }
    "#);
    let new = sdl(r#"
        directive @tag(name: String!) on FIELD_DEFINITION
        type Query { user(id: ID!): User, users: [User!]! }
        type User { id: ID!, name: String, role: Role }
        enum Role { USER ADMIN }
        input Filter { role: Role }
    "#);
    let diff = diff(&old, &new);
    assert_eq!(diff.changes, Vec::new(), "{diff}");
    assert!(diff.is_compatible());
}

#[test]
fn reports_breaking_changes() {
    let old = sdl(r#"
        directive @tag(name: String!) on FIELD_DEFINITION
        type Query { user(id: ID!): User }
        interface Node { id: ID! }
        type User implements Node { id: ID!, name: String!, role: Role }
        enum Role { USER ADMIN }
        union Search = User
        input Filter { role: Role, active: Boolean! }
    "#);
    let new = sdl(r#"
        type Query { user(id: ID): User }
        type User { id: ID!, name: String, role: Role }
        enum Role { USER }
        union Search = User
        input Filter { role: Role }
    "#);
    let diff = diff(&old, &new);
    let changes = &diff.changes;
    assert!(changes.iter().any(|c| matches!(c, SchemaChange::FieldTypeChanged { field_name, .. } if field_name == "name")), "{diff}");
    assert!(
        changes
            .iter()
            .any(|c| matches!(c, SchemaChange::EnumValueRemoved { value, .. } if value == "ADMIN")),
        "{diff}"
    );
    assert!(changes.iter().any(|c| matches!(c, SchemaChange::InterfaceRemoved { interface_name, .. } if interface_name == "Node")), "{diff}");
    assert!(changes.iter().any(|c| matches!(c, SchemaChange::InputFieldRemoved { field_name, .. } if field_name == "active")), "{diff}");
    assert!(
        changes
            .iter()
            .any(|c| matches!(c, SchemaChange::DirectiveRemoved { name } if name == "tag")),
        "{diff}"
    );
    assert!(
        changes
            .iter()
            .any(|c| matches!(c, SchemaChange::TypeRemoved { name } if name == "Node")),
        "{diff}"
    );
}

#[test]
fn new_required_input_is_breaking() {
    let old = sdl(r#"type Query { f(input: Filter): Int } input Filter { a: Int }"#);
    let new = sdl(r#"type Query { f(input: Filter): Int } input Filter { a: Int, b: Int! }"#);
    let diff = diff(&old, &new);
    assert!(diff.changes.iter().any(|c| matches!(c, SchemaChange::InputFieldBecameRequired { field_name, .. } if field_name == "b")), "{diff}");
}

#[test]
fn new_required_argument_is_breaking() {
    let old = sdl(r#"type Query { f: Int }"#);
    let new = sdl(r#"type Query { f(limit: Int!): Int }"#);
    let diff = diff(&old, &new);
    assert!(diff.changes.iter().any(|c| matches!(c, SchemaChange::RequiredArgumentAdded { argument_name, .. } if argument_name == "limit")), "{diff}");
}

#[test]
fn sdl_schema_has_introspection_types_and_roots() {
    let schema = sdl(r#"
        schema { query: Root }
        type Root { hello: String }
    "#);
    assert_eq!(&**schema.query_name(), "Root");
    assert!(schema.get_type("__Schema").is_some());
    assert!(schema.get_type("__Type").is_some());
    assert!(schema.get_type("String").is_some());
    assert!(schema.get_directive("skip").is_some());
}
