//! Input coercion, exercised through the public coercion helpers.

use gql::{
    EnumType, Field, InputObjectType, InputValue, Object, ObjectType, Schema, TypeRef, Value, ast,
    execution::{coerce_argument_values, coerce_variable_values},
};

fn schema() -> Schema {
    Schema::builder()
        .enumeration(EnumType::new("Color").value("RED").value("GREEN"))
        .input_object(
            InputObjectType::new("Range")
                .field(InputValue::new("min", TypeRef::int().non_null()))
                .field(InputValue::new("max", TypeRef::int().non_null())),
        )
        .input_object(
            InputObjectType::new("Filter")
                .field(InputValue::new("limit", TypeRef::int().non_null()))
                .field(
                    InputValue::new("color", TypeRef::named("Color"))
                        .default_value(Value::enumeration("RED")),
                )
                .field(InputValue::new("tags", TypeRef::string().non_null().list()))
                .field(InputValue::new("range", TypeRef::named("Range"))),
        )
        .input_object(
            InputObjectType::new("Choice")
                .one_of()
                .field(InputValue::new("a", TypeRef::int()))
                .field(InputValue::new("b", TypeRef::string())),
        )
        .object(
            ObjectType::new("Query")
                .field(
                    Field::new("f", TypeRef::string())
                        .arg(InputValue::new("a", TypeRef::int().non_null()))
                        .arg(InputValue::new("b", TypeRef::string()).default_value("bee"))
                        .arg(
                            InputValue::new("values", TypeRef::int().non_null().list())
                                .default_value(Value::List(Vec::new())),
                        ),
                )
                .field(
                    Field::new("g", TypeRef::string())
                        .arg(InputValue::new("filter", TypeRef::named("Filter")))
                        .arg(InputValue::new("choice", TypeRef::named("Choice")))
                        .arg(InputValue::new("id", TypeRef::id()))
                        .arg(InputValue::new("color", TypeRef::named("Color")))
                        .arg(InputValue::new("ratio", TypeRef::float()))
                        .arg(InputValue::new("values", TypeRef::int().list())),
                ),
        )
        .query("Query")
        .build()
        .expect("valid schema")
}

fn parse(source: &str) -> ast::Document<'_> {
    gql::parser::parse_query(source).expect("valid document")
}

fn operation_of<'a>(document: &'a ast::Document<'a>) -> &'a ast::OperationDefinition<'a> {
    document.operations().next().expect("one operation")
}

fn selected_field<'a>(operation: &'a ast::OperationDefinition<'a>) -> &'a ast::Field<'a> {
    match &operation.selection_set.items[0] {
        ast::Selection::Field(field) => field,
        _ => panic!("expected a field selection"),
    }
}

fn field_definition<'a>(schema: &'a Schema, name: &str) -> &'a Field {
    schema
        .object_type("Query")
        .and_then(|object| object.get_field(name))
        .expect("field definition")
}

#[test]
fn variable_defaults_and_missing_variables() {
    let schema = schema();
    let document = parse("query Q($a: Int = 3, $b: Int, $c: Int!) { g }");
    let operation = operation_of(&document);

    let errors = coerce_variable_values(&schema, operation, None).expect_err("c is required");
    assert_eq!(errors.len(), 1);
    assert!(errors[0].message().contains("$c"), "{:?}", errors[0]);

    let inputs = Object::from_fields([("c", Value::Int(7))]);
    let coerced = coerce_variable_values(&schema, operation, Some(&inputs)).expect("coerced");
    assert_eq!(coerced.values.get("a"), Some(&Value::Int(3)));
    assert_eq!(coerced.values.get("b"), None);
    assert_eq!(coerced.values.get("c"), Some(&Value::Int(7)));
}

#[test]
fn rejects_invalid_scalar_variable_values() {
    let schema = schema();

    let document = parse("query Q($i: Int) { g }");
    let operation = operation_of(&document);
    let inputs = Object::from_fields([("i", Value::Float(1.5))]);
    let errors = coerce_variable_values(&schema, operation, Some(&inputs)).expect_err("bad int");
    assert!(errors[0].message().contains("$i"), "{:?}", errors[0]);

    let document = parse("query Q($s: String) { g }");
    let operation = operation_of(&document);
    let inputs = Object::from_fields([("s", Value::Int(1))]);
    let errors = coerce_variable_values(&schema, operation, Some(&inputs)).expect_err("bad string");
    assert!(errors[0].message().contains("String"), "{:?}", errors[0]);

    let document = parse("query Q($t: Int!) { g }");
    let operation = operation_of(&document);
    let inputs = Object::from_fields([("t", Value::Null)]);
    let errors = coerce_variable_values(&schema, operation, Some(&inputs)).expect_err("null");
    assert!(errors[0].message().contains("must not be null"));

    let document = parse("query Q($x: Missing) { g }");
    let operation = operation_of(&document);
    let errors = coerce_variable_values(&schema, operation, None).expect_err("unknown type");
    assert!(
        errors[0].message().contains("unknown type"),
        "{:?}",
        errors[0]
    );
}

#[test]
fn coerces_ids_and_accepts_integers_for_float() {
    let schema = schema();
    let document = parse("query Q($id: ID, $ratio: Float) { g }");
    let operation = operation_of(&document);
    let inputs = Object::from_fields([("id", Value::Int(5)), ("ratio", Value::Int(2))]);
    let coerced = coerce_variable_values(&schema, operation, Some(&inputs)).expect("coerced");
    assert_eq!(coerced.values.get("id"), Some(&Value::from("5")));
    assert_eq!(coerced.values.get("ratio"), Some(&Value::Float(2.0)));
}

#[test]
fn coerces_input_object_variables_with_defaults() {
    let schema = schema();
    let document = parse("query Q($f: Filter!) { g }");
    let operation = operation_of(&document);

    let inputs = Object::from_fields([(
        "f",
        Value::object(Object::from_fields([
            ("limit", Value::Int(10)),
            (
                "tags",
                Value::List(vec![Value::from("a"), Value::from("b")]),
            ),
        ])),
    )]);
    let coerced = coerce_variable_values(&schema, operation, Some(&inputs)).expect("coerced");
    let filter = coerced
        .values
        .get("f")
        .and_then(Value::as_object)
        .expect("filter object");
    assert_eq!(filter.get("limit"), Some(&Value::Int(10)));
    assert_eq!(filter.get("color"), Some(&Value::Enum("RED".into())));
    assert_eq!(filter.get("range"), None);

    let inputs = Object::from_fields([(
        "f",
        Value::object(Object::from_fields([
            ("limit", Value::Int(1)),
            ("tags", Value::List(Vec::new())),
            ("nope", Value::Int(1)),
        ])),
    )]);
    let errors = coerce_variable_values(&schema, operation, Some(&inputs)).expect_err("unknown");
    assert!(errors[0].message().contains("nope"), "{:?}", errors[0]);

    let inputs = Object::from_fields([(
        "f",
        Value::object(Object::from_fields([("tags", Value::List(Vec::new()))])),
    )]);
    let errors = coerce_variable_values(&schema, operation, Some(&inputs)).expect_err("missing");
    assert!(errors[0].message().contains("limit"), "{:?}", errors[0]);

    let inputs = Object::from_fields([(
        "f",
        Value::object(Object::from_fields([
            ("limit", Value::Int(1)),
            ("tags", Value::List(Vec::new())),
            (
                "range",
                Value::object(Object::from_fields([("min", Value::Int(1))])),
            ),
        ])),
    )]);
    let errors = coerce_variable_values(&schema, operation, Some(&inputs)).expect_err("range");
    assert!(errors[0].message().contains("max"), "{:?}", errors[0]);
    assert!(errors[0].message().contains("range"), "{:?}", errors[0]);
}

#[test]
fn enforces_one_of_input_objects() {
    let schema = schema();
    let document = parse("query Q($c: Choice) { g }");
    let operation = operation_of(&document);

    let inputs = Object::from_fields([(
        "c",
        Value::object(Object::from_fields([("a", Value::Int(1))])),
    )]);
    let coerced = coerce_variable_values(&schema, operation, Some(&inputs)).expect("one field");
    let choice = coerced
        .values
        .get("c")
        .and_then(Value::as_object)
        .expect("choice");
    assert_eq!(choice.get("a"), Some(&Value::Int(1)));

    let inputs = Object::from_fields([("c", Value::object(Object::new()))]);
    let errors = coerce_variable_values(&schema, operation, Some(&inputs)).expect_err("empty");
    assert!(errors[0].message().contains("exactly one"));

    let inputs = Object::from_fields([(
        "c",
        Value::object(Object::from_fields([
            ("a", Value::Int(1)),
            ("b", Value::from("x")),
        ])),
    )]);
    let errors = coerce_variable_values(&schema, operation, Some(&inputs)).expect_err("two");
    assert!(errors[0].message().contains("exactly one"));

    let inputs = Object::from_fields([(
        "c",
        Value::object(Object::from_fields([("a", Value::Null)])),
    )]);
    let errors = coerce_variable_values(&schema, operation, Some(&inputs)).expect_err("null");
    assert!(errors[0].message().contains("null"), "{:?}", errors[0]);
}

#[test]
fn argument_literals_apply_defaults() {
    let schema = schema();
    let document = parse("{ f(a: 1) }");
    let operation = operation_of(&document);
    let node = selected_field(operation);
    let definition = field_definition(&schema, "f");

    let args = coerce_argument_values(&schema, definition, node, &Object::new()).expect("coerced");
    assert_eq!(args.get("a"), Some(&Value::Int(1)));
    assert_eq!(args.get("b"), Some(&Value::from("bee")));
    assert_eq!(args.get("values"), Some(&Value::List(Vec::new())));
}

#[test]
fn argument_literal_lists_accept_single_values() {
    let schema = schema();
    let definition = field_definition(&schema, "f");

    let document = parse("{ f(a: 1, values: 5) }");
    let node = selected_field(operation_of(&document));
    let args = coerce_argument_values(&schema, definition, node, &Object::new()).expect("coerced");
    assert_eq!(args.get("values"), Some(&Value::List(vec![Value::Int(5)])));

    let document = parse("{ f(a: 1, values: [5, 6]) }");
    let node = selected_field(operation_of(&document));
    let args = coerce_argument_values(&schema, definition, node, &Object::new()).expect("coerced");
    assert_eq!(
        args.get("values"),
        Some(&Value::List(vec![Value::Int(5), Value::Int(6)]))
    );

    let document = parse("{ f(a: 1, values: [5, null]) }");
    let node = selected_field(operation_of(&document));
    let error =
        coerce_argument_values(&schema, definition, node, &Object::new()).expect_err("null item");
    assert!(error.message().contains("non-null"), "{error:?}");
}

#[test]
fn argument_variables_use_coerced_values() {
    let schema = schema();
    let definition = field_definition(&schema, "f");

    let document = parse("query Q($n: Int!) { f(a: $n) }");
    let node = selected_field(operation_of(&document));
    let variables = Object::from_fields([("n", Value::Int(9))]);
    let args = coerce_argument_values(&schema, definition, node, &variables).expect("coerced");
    assert_eq!(args.get("a"), Some(&Value::Int(9)));
    assert_eq!(args.get("b"), Some(&Value::from("bee")));

    // An absent variable with a default uses the default.
    let document = parse("query Q($b: String) { f(a: 1, b: $b) }");
    let node = selected_field(operation_of(&document));
    let args = coerce_argument_values(&schema, definition, node, &Object::new()).expect("coerced");
    assert_eq!(args.get("b"), Some(&Value::from("bee")));

    // An explicitly supplied null overrides a default for a nullable argument.
    let document = parse("query Q($b: String) { f(a: 1, b: $b) }");
    let node = selected_field(operation_of(&document));
    let variables = Object::from_fields([("b", Value::Null)]);
    let args = coerce_argument_values(&schema, definition, node, &variables).expect("coerced");
    assert_eq!(args.get("b"), Some(&Value::Null));

    // A missing required variable is an error.
    let document = parse("query Q($n: Int!) { f(a: $n) }");
    let node = selected_field(operation_of(&document));
    let error = coerce_argument_values(&schema, definition, node, &Object::new())
        .expect_err("missing variable");
    assert!(error.message().contains("$n"), "{error:?}");
}

#[test]
fn rejects_unknown_arguments_and_missing_required_arguments() {
    let schema = schema();
    let definition = field_definition(&schema, "f");

    let document = parse("{ f }");
    let node = selected_field(operation_of(&document));
    let error = coerce_argument_values(&schema, definition, node, &Object::new())
        .expect_err("missing required");
    assert!(error.message().contains("required"), "{error:?}");

    let document = parse("{ f(a: 1, nope: 2) }");
    let node = selected_field(operation_of(&document));
    let error = coerce_argument_values(&schema, definition, node, &Object::new())
        .expect_err("unknown argument");
    assert!(error.message().contains("Unknown argument"), "{error:?}");
}

#[test]
fn coerces_id_int_and_float_literals() {
    let schema = schema();
    let definition = field_definition(&schema, "g");

    let document = parse("{ g(id: 7) }");
    let node = selected_field(operation_of(&document));
    let args = coerce_argument_values(&schema, definition, node, &Object::new()).expect("coerced");
    assert_eq!(args.get("id"), Some(&Value::from("7")));

    let document = parse(r#"{ g(id: "abc") }"#);
    let node = selected_field(operation_of(&document));
    let args = coerce_argument_values(&schema, definition, node, &Object::new()).expect("coerced");
    assert_eq!(args.get("id"), Some(&Value::from("abc")));

    let document = parse("{ g(id: 1.5) }");
    let node = selected_field(operation_of(&document));
    let error =
        coerce_argument_values(&schema, definition, node, &Object::new()).expect_err("float id");
    assert!(error.message().contains("ID"), "{error:?}");

    let document = parse("{ g(ratio: 2) }");
    let node = selected_field(operation_of(&document));
    let args = coerce_argument_values(&schema, definition, node, &Object::new()).expect("coerced");
    assert_eq!(args.get("ratio"), Some(&Value::Float(2.0)));

    let definition = field_definition(&schema, "f");
    let document = parse("{ f(a: 3000000000) }");
    let node = selected_field(operation_of(&document));
    let error = coerce_argument_values(&schema, definition, node, &Object::new())
        .expect_err("out of range");
    assert!(error.message().contains("32-bit"), "{error:?}");
}

#[test]
fn coerces_enum_literals() {
    let schema = schema();
    let definition = field_definition(&schema, "g");

    let document = parse("{ g(color: GREEN) }");
    let node = selected_field(operation_of(&document));
    let args = coerce_argument_values(&schema, definition, node, &Object::new()).expect("coerced");
    assert_eq!(args.get("color"), Some(&Value::Enum("GREEN".into())));

    let document = parse("{ g(color: PURPLE) }");
    let node = selected_field(operation_of(&document));
    let error = coerce_argument_values(&schema, definition, node, &Object::new())
        .expect_err("unknown enum value");
    assert!(error.message().contains("PURPLE"), "{error:?}");

    let document = parse(r#"{ g(color: "GREEN") }"#);
    let node = selected_field(operation_of(&document));
    let error = coerce_argument_values(&schema, definition, node, &Object::new())
        .expect_err("string literal for enum");
    assert!(error.message().contains("Enum"), "{error:?}");
}

#[test]
fn input_object_literals_apply_field_defaults() {
    let schema = schema();
    let definition = field_definition(&schema, "g");

    let document = parse("{ g(filter: { limit: 1, tags: [] }) }");
    let node = selected_field(operation_of(&document));
    let args = coerce_argument_values(&schema, definition, node, &Object::new()).expect("coerced");
    let filter = args
        .get("filter")
        .and_then(Value::as_object)
        .expect("filter object");
    assert_eq!(filter.get("limit"), Some(&Value::Int(1)));
    assert_eq!(filter.get("color"), Some(&Value::Enum("RED".into())));

    let document = parse("{ g(filter: { limit: 1, tags: [], extra: true }) }");
    let node = selected_field(operation_of(&document));
    let error = coerce_argument_values(&schema, definition, node, &Object::new())
        .expect_err("unknown field");
    assert!(error.message().contains("extra"), "{error:?}");

    let document = parse("{ g(choice: { a: 1, b: \"x\" }) }");
    let node = selected_field(operation_of(&document));
    let error =
        coerce_argument_values(&schema, definition, node, &Object::new()).expect_err("oneOf");
    assert!(error.message().contains("exactly one"), "{error:?}");
}
