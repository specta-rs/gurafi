//! Applied directives, SDL printing and directive-argument validation.

use gql::{
    AppliedDirective, DirectiveDefinition, EnumType, Field, InputObjectType, InputValue,
    InterfaceType, ObjectType, ScalarType, Schema, TypeRef, UnionType, Value,
    ast::DirectiveLocation,
};

fn parse_sdl(source: &str) -> Schema {
    Schema::from_sdl(source).unwrap_or_else(|error| panic!("SDL should be valid: {error}"))
}

fn error(source: &str) -> String {
    match Schema::from_sdl(source) {
        Ok(_) => panic!("SDL should be rejected"),
        Err(error) => error.to_string(),
    }
}

const ALL_LOCATIONS: &str = r#"
directive @tag(
  name: String!
  meta: TagMeta
  flags: [Boolean!]
) repeatable on SCALAR | OBJECT | INTERFACE | UNION | ENUM | INPUT_OBJECT | FIELD_DEFINITION | ARGUMENT_DEFINITION | INPUT_FIELD_DEFINITION | ENUM_VALUE

input TagMeta {
  note: String
  tags: [String!]
}

scalar Date @tag(name: "scalar")

interface Node @tag(name: "interface") {
  id: ID!
}

type User implements Node @tag(name: "object", flags: [true, false]) {
  id: ID! @tag(name: "field")
  name(locale: String @tag(name: "argument")): String
}

union Search @tag(name: "union") = User

enum Color @tag(name: "enum") {
  RED @tag(name: "value")
  GREEN
}

input Filter @tag(name: "input object") {
  term: String @tag(name: "input field")
}

type Query {
  user: User
  search: Search
  color: Color
  filter(filter: Filter): String
  date: Date
}
"#;

#[test]
fn applied_directives_round_trip_on_every_location() {
    let schema = parse_sdl(ALL_LOCATIONS);
    let printed = schema.sdl();

    // Every application is printed in the location it belongs to.
    for expected in [
        r#"scalar Date @tag(name: "scalar")"#,
        r#"interface Node @tag(name: "interface")"#,
        r#"type User implements Node @tag(name: "object", flags: [true, false])"#,
        r#"id: ID! @tag(name: "field")"#,
        r#"locale: String @tag(name: "argument")"#,
        r#"union Search @tag(name: "union") = User"#,
        r#"enum Color @tag(name: "enum")"#,
        r#"RED @tag(name: "value")"#,
        r#"input Filter @tag(name: "input object")"#,
        r#"term: String @tag(name: "input field")"#,
    ] {
        assert!(
            printed.contains(expected),
            "missing {expected:?} in:\n{printed}"
        );
    }

    // Arguments keep nested list/object values and their order.
    let with_meta = parse_sdl(
        r#"
        directive @tag(meta: TagMeta) on OBJECT
        input TagMeta { note: String, tags: [String!] }
        type Query @tag(meta: {note: "a \"quote\"", tags: ["x", "y"]}) { f: String }
        "#,
    );
    let printed_meta = with_meta.sdl();
    assert!(
        printed_meta.contains(r#"@tag(meta: {note: "a \"quote\"", tags: ["x", "y"]})"#),
        "{printed_meta}"
    );

    let reparsed = parse_sdl(&printed);
    assert_eq!(printed, reparsed.sdl(), "printing must be stable");
}

#[test]
fn directive_argument_strings_are_escaped_and_round_trip() {
    let schema = parse_sdl(
        r#"
        directive @tag(text: String) on FIELD_DEFINITION
        type Query { f: String @tag(text: "a\nb\tc\\d\"e") }
        "#,
    );
    let printed = schema.sdl();
    assert!(
        printed.contains(r#"@tag(text: "a\nb\tc\\d\"e")"#),
        "{printed}"
    );
    assert_eq!(printed, parse_sdl(&printed).sdl());
}

#[test]
fn applied_directives_are_stored_in_the_model() {
    let schema = parse_sdl(ALL_LOCATIONS);

    let date = schema.scalar_type("Date").expect("Date");
    assert_eq!(date.directives.len(), 1);
    assert_eq!(date.directives[0].name.as_ref(), "tag");
    assert_eq!(
        date.directives[0].get_argument("name"),
        Some(&Value::from("scalar"))
    );

    let user = schema.object_type("User").expect("User");
    assert_eq!(user.directives[0].name.as_ref(), "tag");
    assert_eq!(
        user.fields["id"].directives[0].get_argument("name"),
        Some(&Value::from("field"))
    );
    assert_eq!(
        user.fields["name"].args["locale"].directives[0].get_argument("name"),
        Some(&Value::from("argument"))
    );

    let color = schema.enum_type("Color").expect("Color");
    assert_eq!(color.directives[0].name.as_ref(), "tag");
    assert_eq!(
        color.values["RED"].directives[0].get_argument("name"),
        Some(&Value::from("value"))
    );

    let filter = schema.input_object_type("Filter").expect("Filter");
    assert_eq!(filter.directives[0].name.as_ref(), "tag");
    assert_eq!(
        filter.fields["term"].directives[0].get_argument("name"),
        Some(&Value::from("input field"))
    );

    let node = schema.interface_type("Node").expect("Node");
    assert_eq!(node.directives[0].name.as_ref(), "tag");

    let search = schema.union_type("Search").expect("Search");
    assert_eq!(search.directives[0].name.as_ref(), "tag");
}

#[test]
fn builder_applied_directives_are_printed() {
    let schema = Schema::builder()
        .directive(
            DirectiveDefinition::new("tag")
                .arg(InputValue::new("name", TypeRef::string().non_null()))
                .location(DirectiveLocation::Object)
                .location(DirectiveLocation::FieldDefinition),
        )
        .object(
            ObjectType::new("Query")
                .directive(AppliedDirective::new("tag").argument("name", "object"))
                .field(
                    Field::new("hello", TypeRef::string())
                        .directive(AppliedDirective::new("tag").argument("name", "field")),
                ),
        )
        .query("Query")
        .build()
        .expect("valid schema");

    let printed = schema.sdl();
    assert!(
        printed.contains(r#"type Query @tag(name: "object")"#),
        "{printed}"
    );
    assert!(
        printed.contains(r#"hello: String @tag(name: "field")"#),
        "{printed}"
    );
}

#[test]
fn extend_scalar_specified_by_sets_the_url() {
    let schema = parse_sdl(
        r#"
        directive @tag on SCALAR
        scalar Date
        extend scalar Date @specifiedBy(url: "https://example.com/date") @tag
        type Query { date: Date }
        "#,
    );
    assert_eq!(
        schema
            .scalar_type("Date")
            .unwrap()
            .specified_by_url
            .as_deref(),
        Some("https://example.com/date")
    );
    let printed = schema.sdl();
    assert!(printed.contains(r#"scalar Date @specifiedBy(url: "https://example.com/date") @tag"#));
}

#[test]
fn extend_input_one_of_sets_the_flag() {
    let schema = parse_sdl(
        r#"
        input Choice { a: Int, b: String }
        extend input Choice @oneOf
        type Query { f(choice: Choice): String }
        "#,
    );
    assert!(schema.input_object_type("Choice").unwrap().one_of);
    assert!(schema.sdl().contains("input Choice @oneOf"));
}

#[test]
fn specified_by_cannot_appear_on_the_definition_and_an_extension() {
    let message = error(
        r#"
        scalar Date @specifiedBy(url: "https://example.com/a")
        extend scalar Date @specifiedBy(url: "https://example.com/b")
        type Query { date: Date }
        "#,
    );
    assert!(
        message.contains("may only be used once"),
        "unexpected message: {message}"
    );
}

#[test]
fn one_of_cannot_appear_on_the_definition_and_an_extension() {
    let message = error(
        r#"
        input Choice @oneOf { a: Int }
        extend input Choice @oneOf
        type Query { f(choice: Choice): String }
        "#,
    );
    assert!(
        message.contains("may only be used once"),
        "unexpected message: {message}"
    );
}

#[test]
fn custom_non_repeatable_directive_cannot_cross_an_extension() {
    let message = error(
        r#"
        directive @tag on OBJECT
        type Query @tag { f: String }
        extend type Query @tag
        "#,
    );
    assert!(
        message.contains("may only be used once"),
        "unexpected message: {message}"
    );
}

#[test]
fn custom_repeatable_directive_may_cross_an_extension() {
    parse_sdl(
        r#"
        directive @tag repeatable on OBJECT
        type Query @tag { f: String }
        extend type Query @tag
        "#,
    );
}

#[test]
fn wrong_typed_builtin_directive_arguments_are_rejected() {
    // A wrong-typed reason must not silently become the empty string or the default.
    let message = error(
        r#"
        type Query { f: String @deprecated(reason: 1) }
        "#,
    );
    assert!(
        message.contains("reason") && message.contains("String"),
        "unexpected message: {message}"
    );

    let message = error(
        r#"
        scalar Date @specifiedBy(url: 1)
        type Query { date: Date }
        "#,
    );
    assert!(
        message.contains("url") && message.contains("String"),
        "unexpected message: {message}"
    );

    // `@oneOf` takes no arguments at all.
    let message = error(
        r#"
        input Choice @oneOf(extra: 1) { a: Int }
        type Query { f(choice: Choice): String }
        "#,
    );
    assert!(
        message.contains("Unknown argument"),
        "unexpected message: {message}"
    );
}

#[test]
fn wrong_typed_custom_directive_arguments_are_rejected() {
    // Unknown enum value.
    let message = error(
        r#"
        directive @mode(value: Mode!) on FIELD_DEFINITION
        enum Mode { FAST, SLOW }
        type Query { f: String @mode(value: TURBO) }
        "#,
    );
    assert!(
        message.contains("TURBO") || message.contains("Mode"),
        "unexpected message: {message}"
    );

    // An input object literal with an unknown field.
    let message = error(
        r#"
        directive @tag(meta: Meta!) on FIELD_DEFINITION
        input Meta { note: String }
        type Query { f: String @tag(meta: {nope: "x"}) }
        "#,
    );
    assert!(
        message.contains("nope") || message.contains("Meta"),
        "unexpected message: {message}"
    );

    // A list element of the wrong type.
    let message = error(
        r#"
        directive @tag(flags: [Boolean!]!) on FIELD_DEFINITION
        type Query { f: String @tag(flags: [true, "no"]) }
        "#,
    );
    assert!(
        message.contains("Boolean") || message.contains("flags"),
        "unexpected message: {message}"
    );

    // A null where the argument is non-null.
    let message = error(
        r#"
        directive @tag(name: String!) on FIELD_DEFINITION
        type Query { f: String @tag(name: null) }
        "#,
    );
    assert!(message.contains("name"), "unexpected message: {message}");

    // Missing required arguments and unknown arguments are rejected too.
    assert!(
        error(r#"directive @tag(name: String!) on FIELD_DEFINITION type Query { f: String @tag }"#)
            .contains("name")
    );
    assert!(error(r#"directive @tag(name: String!) on FIELD_DEFINITION type Query { f: String @tag(nope: "x") }"#).contains("nope"));
}

#[test]
fn directive_may_not_reference_itself_directly() {
    let message = error(
        r#"
        directive @tag(name: String @tag) on ARGUMENT_DEFINITION
        type Query { f: String }
        "#,
    );
    assert!(
        message.contains("cannot reference itself"),
        "unexpected message: {message}"
    );
}

#[test]
fn directive_may_not_reference_itself_indirectly() {
    let message = error(
        r#"
        directive @a(name: String @b) on ARGUMENT_DEFINITION
        directive @b(name: String @c) on ARGUMENT_DEFINITION
        directive @c(name: String @a) on ARGUMENT_DEFINITION
        type Query { f: String }
        "#,
    );
    assert!(
        message.contains("cannot reference itself"),
        "unexpected message: {message}"
    );
}

#[test]
fn directives_on_directive_definition_arguments_are_validated() {
    // Valid: ARGUMENT_DEFINITION is a permitted location and the value type is right.
    let schema = parse_sdl(
        r#"
        directive @tag(name: String) on ARGUMENT_DEFINITION
        directive @withTag(arg: String @tag(name: "arg")) on FIELD_DEFINITION
        type Query { f: String }
        "#,
    );
    let with_tag = schema.get_directive("withTag").expect("withTag");
    assert_eq!(
        with_tag.args["arg"].directives[0].get_argument("name"),
        Some(&Value::from("arg"))
    );
    assert!(schema.sdl().contains(r#"arg: String @tag(name: "arg")"#));

    // Wrong location.
    let message = error(
        r#"
        directive @tag on OBJECT
        directive @withTag(arg: String @tag) on FIELD_DEFINITION
        type Query { f: String }
        "#,
    );
    assert!(
        message.contains("may not be used on ARGUMENT_DEFINITION"),
        "unexpected message: {message}"
    );

    // Unknown directive.
    let message = error(
        r#"
        directive @withTag(arg: String @nope) on FIELD_DEFINITION
        type Query { f: String }
        "#,
    );
    assert!(
        message.contains("Unknown directive"),
        "unexpected message: {message}"
    );

    // Wrong argument value type.
    let message = error(
        r#"
        directive @tag(name: String!) on ARGUMENT_DEFINITION
        directive @withTag(arg: String @tag(name: 1)) on FIELD_DEFINITION
        type Query { f: String }
        "#,
    );
    assert!(
        message.contains("name") && message.contains("String"),
        "unexpected message: {message}"
    );
}

#[test]
fn descriptions_with_carriage_returns_round_trip() {
    let description = "first line\r\nsecond line\rthird line";
    let schema = Schema::builder()
        .object(
            ObjectType::new("Query")
                .description(description)
                .field(Field::new("hello", TypeRef::string()).description("field\rdescription")),
        )
        .query("Query")
        .build()
        .expect("valid schema");

    let printed = schema.sdl();
    assert!(
        !printed.contains('\r'),
        "a raw carriage return would be normalised away by the parser:\n{printed}"
    );

    let reparsed = parse_sdl(&printed);
    assert_eq!(
        reparsed.query_type().description.as_deref(),
        Some(description)
    );
    assert_eq!(
        reparsed.query_type().fields["hello"].description.as_deref(),
        Some("field\rdescription")
    );
}

#[test]
fn schema_applied_directives_are_validated() {
    let message = error(
        r#"
        directive @tag(version: String!) on SCHEMA
        schema @tag(version: 1) { query: Query }
        type Query { f: String }
        "#,
    );
    assert!(
        message.contains("version") && message.contains("String"),
        "unexpected message: {message}"
    );

    let message = error(
        r#"
        directive @tag on SCHEMA
        schema @tag { query: Query }
        extend schema @tag
        type Query { f: String }
        "#,
    );
    assert!(
        message.contains("may only be used once on the schema"),
        "unexpected message: {message}"
    );

    // Valid schema-level applications are accepted.
    parse_sdl(
        r#"
        directive @tag(version: String!) on SCHEMA
        schema @tag(version: "1") { query: Query }
        type Query { f: String }
        "#,
    );
}

#[test]
fn deprecated_directive_definitions_are_printed() {
    let schema = Schema::builder()
        .directive(
            DirectiveDefinition::new("old")
                .location(DirectiveLocation::FieldDefinition)
                .deprecation_reason("Use @new."),
        )
        .object(ObjectType::new("Query").field(Field::new("f", TypeRef::string())))
        .query("Query")
        .build()
        .expect("valid schema");

    assert!(
        schema
            .sdl()
            .contains(r#"directive @old @deprecated(reason: "Use @new.") on FIELD_DEFINITION"#),
        "{}",
        schema.sdl()
    );
}

#[test]
fn deprecated_reason_is_stored_without_a_dedicated_directive_slot() {
    let schema = parse_sdl(
        r#"
        enum Color {
          RED
          BLUE @deprecated
          GREEN @deprecated(reason: "Use RED.")
        }
        type Query { color: Color }
        "#,
    );
    let color = schema.enum_type("Color").expect("Color");
    assert_eq!(color.values["RED"].deprecation_reason, None);
    assert_eq!(
        color.values["BLUE"].deprecation_reason.as_deref(),
        Some("No longer supported")
    );
    assert_eq!(
        color.values["GREEN"].deprecation_reason.as_deref(),
        Some("Use RED.")
    );
    // `@deprecated` is authoritative through the field, so it is not printed twice.
    let printed = schema.sdl();
    assert_eq!(printed.matches("@deprecated").count(), 2, "{printed}");
}

#[test]
fn enum_value_builder_directive_is_printable() {
    let mut color = EnumType::new("Color")
        .directive(AppliedDirective::new("tag").argument("name", "enum"))
        .value("RED");
    color
        .values
        .get_mut("RED")
        .expect("RED")
        .directives
        .push(AppliedDirective::new("tag").argument("name", "value"));

    let schema = Schema::builder()
        .directive(
            DirectiveDefinition::new("tag")
                .arg(InputValue::new("name", TypeRef::string().non_null()))
                .location(DirectiveLocation::EnumValue)
                .location(DirectiveLocation::Enum),
        )
        .enumeration(color)
        .object(ObjectType::new("Query").field(Field::new("color", TypeRef::named("Color"))))
        .query("Query")
        .build()
        .expect("valid schema");

    let printed = schema.sdl();
    assert!(
        printed.contains(r#"enum Color @tag(name: "enum")"#),
        "{printed}"
    );
    assert!(printed.contains(r#"RED @tag(name: "value")"#), "{printed}");
}

#[test]
fn scalar_and_interface_builder_directives_are_printable() {
    let schema = Schema::builder()
        .directive(
            DirectiveDefinition::new("tag")
                .arg(InputValue::new("name", TypeRef::string().non_null()))
                .location(DirectiveLocation::Scalar)
                .location(DirectiveLocation::Interface)
                .location(DirectiveLocation::Union)
                .location(DirectiveLocation::InputObject),
        )
        .scalar(
            ScalarType::new("Date").directive(AppliedDirective::new("tag").argument("name", "s")),
        )
        .interface(
            InterfaceType::new("Node")
                .field(Field::new("id", TypeRef::id().non_null()))
                .directive(AppliedDirective::new("tag").argument("name", "i")),
        )
        .union(
            UnionType::new("Search")
                .member("User")
                .directive(AppliedDirective::new("tag").argument("name", "u")),
        )
        .input_object(
            InputObjectType::new("Filter")
                .field(InputValue::new("term", TypeRef::string()))
                .directive(AppliedDirective::new("tag").argument("name", "in")),
        )
        .object(
            ObjectType::new("User")
                .implements("Node")
                .field(Field::new("id", TypeRef::id().non_null())),
        )
        .object(
            ObjectType::new("Query")
                .field(Field::new("date", TypeRef::named("Date")))
                .field(Field::new("search", TypeRef::named("Search")))
                .field(
                    Field::new("f", TypeRef::string())
                        .arg(InputValue::new("filter", TypeRef::named("Filter"))),
                )
                .field(Field::new("node", TypeRef::named("Node"))),
        )
        .query("Query")
        .build()
        .expect("valid schema");

    let printed = schema.sdl();
    assert!(
        printed.contains(r#"scalar Date @tag(name: "s")"#),
        "{printed}"
    );
    assert!(
        printed.contains(r#"interface Node @tag(name: "i")"#),
        "{printed}"
    );
    assert!(
        printed.contains(r#"union Search @tag(name: "u") = User"#),
        "{printed}"
    );
    assert!(
        printed.contains(r#"input Filter @tag(name: "in")"#),
        "{printed}"
    );
}
