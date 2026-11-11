use gql::{
    EnumType, Field, InputObjectType, InputValue, InterfaceType, ObjectType, ScalarType, Schema,
    SchemaBuilder, TypeRef, UnionType, Value,
};

fn string_field(name: &str) -> Field {
    Field::new(name, TypeRef::string().non_null())
}

fn query(fields: impl IntoIterator<Item = Field>) -> SchemaBuilder {
    let mut builder = Schema::builder();
    let mut object = ObjectType::new("Query");
    for field in fields {
        object = object.field(field);
    }
    builder.add_type(object.into());
    builder
}

#[test]
fn rejects_duplicate_type_names() {
    let mut builder = Schema::builder();
    builder.add_type(ObjectType::new("Query").into());
    builder.add_type(ObjectType::new("Query").into());
    let error = builder.query("Query").build().unwrap_err();
    assert!(error.to_string().contains("only one type named"), "{error}");
}

#[test]
fn rejects_duplicate_fields() {
    // Build an object with the duplicate fields directly, then add it.
    let mut object = ObjectType::new("Query");
    object = object.field(string_field("a"));
    object = object.field(string_field("a"));
    let mut builder = Schema::builder();
    builder.add_type(object.into());
    let error = builder.query("Query").build().unwrap_err();
    assert!(error.to_string().contains("more than once"), "{error}");
}

#[test]
fn rejects_unknown_field_type() {
    let builder = query([Field::new("a", TypeRef::named("Missing"))]);
    let error = builder.query("Query").build().unwrap_err();
    assert!(error.to_string().contains("unknown type"), "{error}");
}

#[test]
fn rejects_input_object_used_as_output() {
    let mut input = InputObjectType::new("Filter");
    input = input.field(InputValue::new("a", TypeRef::int()));
    let mut builder = query([Field::new("a", TypeRef::named("Filter"))]);
    builder.add_type(input.into());
    let error = builder.query("Query").build().unwrap_err();
    assert!(
        error.to_string().contains("must use an output type"),
        "{error}"
    );
}

#[test]
fn rejects_invalid_root_types() {
    let mut builder = Schema::builder();
    builder.add_type(ScalarType::new("Date").into());
    builder = builder.query("Date");
    let error = builder.build().unwrap_err();
    assert!(error.to_string().contains("must be an object"), "{error}");
}

#[test]
fn rejects_reserved_names() {
    let mut builder = Schema::builder();
    builder.add_type(ObjectType::new("__Bad").into());
    builder.add_type(ObjectType::new("Query").into());
    let error = builder.query("Query").build().unwrap_err();
    assert!(error.to_string().contains("must not begin with"), "{error}");
}

#[test]
fn rejects_input_cycles_without_nullable_escape() {
    let mut a = InputObjectType::new("A");
    a = a.field(InputValue::new("b", TypeRef::named("B").non_null()));
    let mut b = InputObjectType::new("B");
    b = b.field(InputValue::new("a", TypeRef::named("A").non_null()));
    let mut builder = query([Field::new("f", TypeRef::string())]);
    builder.add_type(a.into());
    builder.add_type(b.into());
    let error = builder.query("Query").build().unwrap_err();
    assert!(error.to_string().contains("cycle"), "{error}");
}

#[test]
fn accepts_input_cycle_through_nullable_or_list() {
    let mut a = InputObjectType::new("A");
    a = a.field(InputValue::new("b", TypeRef::named("B")));
    let mut b = InputObjectType::new("B");
    b = b.field(InputValue::new("as_", TypeRef::named("A").list()));
    let mut builder = query([Field::new("f", TypeRef::string())]);
    builder.add_type(a.into());
    builder.add_type(b.into());
    builder.query("Query").build().expect("valid schema");
}

#[test]
fn rejects_bad_interface_implementation() {
    let mut node = InterfaceType::new("Node");
    node = node.field(string_field("id"));
    let mut user = ObjectType::new("User");
    user = user
        .implements("Node")
        .field(Field::new("id", TypeRef::int()));
    let mut builder = query([Field::new("u", TypeRef::named("User"))]);
    builder.add_type(node.into());
    builder.add_type(user.into());
    let error = builder.query("Query").build().unwrap_err();
    assert!(error.to_string().contains("expects type"), "{error}");
}

#[test]
fn accepts_valid_interface_implementation() {
    let mut node = InterfaceType::new("Node");
    node = node.field(string_field("id"));
    let mut user = ObjectType::new("User");
    user = user.implements("Node").field(string_field("id"));
    let mut builder = query([Field::new("u", TypeRef::named("User"))]);
    builder.add_type(node.into());
    builder.add_type(user.into());
    builder.query("Query").build().expect("valid schema");
}

#[test]
fn rejects_empty_enum_and_union_duplicates() {
    let empty = EnumType::new("Role");
    let mut builder = query([Field::new("r", TypeRef::named("Role"))]);
    builder.add_type(empty.into());
    let error = builder.query("Query").build().unwrap_err();
    assert!(error.to_string().contains("at least one value"), "{error}");

    let mut union = UnionType::new("Search");
    union = union.member("User").member("User");
    let mut builder = query([Field::new("s", TypeRef::named("Search"))]);
    builder.add_type(ObjectType::new("User").into());
    builder.add_type(union.into());
    let error = builder.query("Query").build().unwrap_err();
    assert!(error.to_string().contains("only include type"), "{error}");
}

#[test]
fn rejects_invalid_argument_defaults() {
    let field = Field::new("f", TypeRef::string()).arg(
        InputValue::new("limit", TypeRef::int().non_null()).default_value(Value::from("nope")),
    );
    let builder = query([field]);
    let error = builder.query("Query").build().unwrap_err();
    assert!(error.to_string().contains("Default value"), "{error}");
}

#[test]
fn rejects_invalid_input_field_defaults() {
    let mut input = InputObjectType::new("Filter");
    input =
        input.field(InputValue::new("limit", TypeRef::int()).default_value(Value::from("nope")));
    let mut builder = query([Field::new("f", TypeRef::string())]);
    builder.add_type(input.into());
    let error = builder.query("Query").build().unwrap_err();
    assert!(error.to_string().contains("Default value"), "{error}");
}

#[test]
fn rejects_null_default_for_non_null_argument() {
    let field = Field::new("f", TypeRef::string())
        .arg(InputValue::new("limit", TypeRef::int().non_null()).default_value(Value::Null));
    let builder = query([field]);
    let error = builder.query("Query").build().unwrap_err();
    assert!(error.to_string().contains("non-null"), "{error}");
}
