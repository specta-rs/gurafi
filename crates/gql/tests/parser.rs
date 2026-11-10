//! Parser conformance tests.
//!
//! These exercise the full October 2021 grammar through the public parser API: executable
//! documents, the type system definition language, lexical edge cases, UTF-8 locations and syntax
//! errors.

use std::borrow::Cow;

use gql::{
    GraphQLError, Location,
    ast::{
        Definition, DirectiveLocation, Document, EnumValueDefinition, FieldDefinition,
        InputValueDefinition, OperationKind, Selection, Type, TypeDefinitionKind,
        TypeExtensionKind, TypeSystemDefinition, Value,
    },
    parser::{parse_query, parse_schema},
};

// ---------------------------------------------------------------------------------------------
// Helpers
// ---------------------------------------------------------------------------------------------

fn parse_ok(source: &str) -> Document<'_> {
    match parse_query(source) {
        Ok(document) => document,
        Err(error) => panic!("expected `{source}` to parse, got: {error}"),
    }
}

fn schema(source: &str) -> Document<'_> {
    match parse_schema(source) {
        Ok(document) => document,
        Err(error) => panic!("expected schema `{source}` to parse, got: {error}"),
    }
}

fn query_error(source: &str) -> GraphQLError {
    match parse_query(source) {
        Ok(document) => panic!("expected `{source}` to fail, parsed: {document:?}"),
        Err(error) => error,
    }
}

fn schema_error(source: &str) -> GraphQLError {
    match parse_schema(source) {
        Ok(document) => panic!("expected schema `{source}` to fail, parsed: {document:?}"),
        Err(error) => error,
    }
}

fn assert_error(error: &GraphQLError, message: &str, line: u32, column: u32) {
    assert_eq!(error.message(), message);
    assert_eq!(error.locations(), &[Location::new(line, column)]);
}

fn operation<'doc, 'a>(document: &'doc Document<'a>) -> &'doc gql::ast::OperationDefinition<'a> {
    let Definition::Operation(operation) = &document.definitions[0] else {
        panic!("expected an operation definition");
    };
    operation
}

fn first_field<'doc, 'a>(document: &'doc Document<'a>) -> &'doc gql::ast::Field<'a> {
    let Selection::Field(field) = &operation(document).selection_set.items[0] else {
        panic!("expected a field selection");
    };
    field
}

fn first_argument_value<'doc, 'a>(document: &'doc Document<'a>) -> &'doc Value<'a> {
    &first_field(document).arguments[0].value
}

fn field<'doc, 'a>(
    operation: &'doc gql::ast::OperationDefinition<'a>,
    name: &str,
) -> &'doc gql::ast::Field<'a> {
    operation
        .selection_set
        .items
        .iter()
        .find_map(|selection| match selection {
            Selection::Field(field) if field.name == name => Some(field),
            _ => None,
        })
        .unwrap_or_else(|| panic!("missing field `{name}`"))
}

// ---------------------------------------------------------------------------------------------
// Executable documents
// ---------------------------------------------------------------------------------------------

#[test]
fn shorthand_operation() {
    let document = parse_ok("{ hero { name } }");
    assert_eq!(document.definitions.len(), 1);

    let operation = operation(&document);
    assert_eq!(operation.kind, OperationKind::Query);
    assert_eq!(operation.name, None);
    assert!(operation.variables.is_empty());
    assert!(operation.directives.is_empty());
    assert_eq!(operation.location, Location::new(1, 1));
    assert_eq!(operation.selection_set.location, Location::new(1, 1));

    let hero = first_field(&document);
    assert_eq!(hero.name, "hero");
    assert_eq!(hero.alias, None);
    assert_eq!(hero.location, Location::new(1, 3));

    let Selection::Field(name) = &hero.selection_set.as_ref().expect("selection set").items[0]
    else {
        panic!("expected nested field");
    };
    assert_eq!(name.name, "name");
    assert_eq!(name.location, Location::new(1, 10));
}

#[test]
fn executable_kitchen_sink() {
    let source = r#"
# Kitchen sink executable document
query Hero($episode: Episode = JEDI, $withFriends: Boolean! = false, $ids: [ID!]! = ["1", "2"], $height: Float = 1.75e2, $note: String @deprecated(reason: "still parsed")) @operation(flag: true) {
  hero(episode: $episode, ids: $ids, obj: {b: 2, a: [1, 2.5, -3e10, null, ENUM], s: "text"}) {
    name
    alias: name
    friends @include(if: $withFriends) {
      name
    }
    ...FriendsNames @spread
    ... on Droid @inline {
      primaryFunction
    }
    ... @noTypeCondition {
      id
    }
  }
}

fragment FriendsNames on Character @fragment {
  friends { name }
}

{ shorthand }
"#;

    let document = parse_ok(source);
    assert_eq!(document.definitions.len(), 3);

    let operation = operation(&document);
    assert_eq!(operation.kind, OperationKind::Query);
    assert_eq!(operation.name, Some("Hero"));
    assert_eq!(operation.location, Location::new(3, 1));
    assert_eq!(operation.variables.len(), 5);

    let episode = &operation.variables[0];
    assert_eq!(episode.name, "episode");
    assert_eq!(episode.ty, Type::Named("Episode"));
    assert_eq!(episode.default_value, Some(Value::Enum("JEDI")));
    assert!(episode.directives.is_empty());

    let with_friends = &operation.variables[1];
    assert_eq!(
        with_friends.ty,
        Type::NonNull(Box::new(Type::Named("Boolean")))
    );
    assert_eq!(with_friends.default_value, Some(Value::Boolean(false)));

    let ids = &operation.variables[2];
    assert_eq!(
        ids.ty,
        Type::NonNull(Box::new(Type::List(Box::new(Type::NonNull(Box::new(
            Type::Named("ID")
        ))))))
    );
    assert_eq!(
        ids.default_value,
        Some(Value::List(vec![
            Value::String(Cow::Borrowed("1")),
            Value::String(Cow::Borrowed("2")),
        ]))
    );

    let height = &operation.variables[3];
    assert_eq!(height.ty, Type::Named("Float"));
    assert_eq!(height.default_value, Some(Value::Float(175.0)));

    let note = &operation.variables[4];
    assert_eq!(note.default_value, None);
    assert_eq!(note.directives.len(), 1);
    assert_eq!(note.directives[0].name, "deprecated");
    assert_eq!(
        note.directives[0].argument("reason").map(|arg| &arg.value),
        Some(&Value::String(Cow::Borrowed("still parsed")))
    );

    assert_eq!(operation.directives.len(), 1);
    assert_eq!(operation.directives[0].name, "operation");
    assert_eq!(
        operation.directives[0]
            .argument("flag")
            .map(|arg| &arg.value),
        Some(&Value::Boolean(true))
    );

    assert_eq!(operation.selection_set.items.len(), 1);
    let hero = field(operation, "hero");
    assert_eq!(hero.arguments.len(), 3);
    assert_eq!(
        hero.argument("episode").map(|arg| &arg.value),
        Some(&Value::Variable("episode"))
    );
    assert_eq!(
        hero.argument("ids").map(|arg| &arg.value),
        Some(&Value::Variable("ids"))
    );

    let Some(Value::Object(object)) = hero.argument("obj").map(|arg| &arg.value) else {
        panic!("expected an object value");
    };
    let names: Vec<&str> = object.iter().map(|field| field.name).collect();
    assert_eq!(names, ["b", "a", "s"]);
    assert_eq!(object[0].value, Value::Int(2));
    assert_eq!(
        object[1].value,
        Value::List(vec![
            Value::Int(1),
            Value::Float(2.5),
            Value::Float(-30_000_000_000.0),
            Value::Null,
            Value::Enum("ENUM"),
        ])
    );
    assert_eq!(object[2].value, Value::String(Cow::Borrowed("text")));

    let selections = &hero.selection_set.as_ref().expect("selection set").items;
    assert_eq!(selections.len(), 6);

    let Selection::Field(alias) = &selections[1] else {
        panic!("expected alias field");
    };
    assert_eq!(alias.alias, Some("alias"));
    assert_eq!(alias.name, "name");

    let Selection::Field(friends) = &selections[2] else {
        panic!("expected friends field");
    };
    assert_eq!(friends.name, "friends");
    assert_eq!(friends.directives.len(), 1);
    assert_eq!(friends.directives[0].name, "include");
    assert_eq!(
        friends.directives[0].argument("if").map(|arg| &arg.value),
        Some(&Value::Variable("withFriends"))
    );

    let Selection::FragmentSpread(spread) = &selections[3] else {
        panic!("expected fragment spread");
    };
    assert_eq!(spread.name, "FriendsNames");
    assert_eq!(spread.directives[0].name, "spread");

    let Selection::InlineFragment(inline) = &selections[4] else {
        panic!("expected inline fragment");
    };
    assert_eq!(inline.type_condition, Some("Droid"));
    assert_eq!(inline.directives[0].name, "inline");

    let Selection::InlineFragment(untyped) = &selections[5] else {
        panic!("expected untyped inline fragment");
    };
    assert_eq!(untyped.type_condition, None);
    assert_eq!(untyped.directives[0].name, "noTypeCondition");

    let fragment = document.fragments().next().expect("fragment");
    assert_eq!(fragment.name, "FriendsNames");
    assert_eq!(fragment.type_condition, "Character");
    assert_eq!(fragment.directives[0].name, "fragment");
    assert_eq!(
        document.fragment("FriendsNames").map(|f| f.name),
        Some("FriendsNames")
    );

    // The shorthand operation parses after other definitions too.
    let Definition::Operation(shorthand) = &document.definitions[2] else {
        panic!("expected shorthand operation");
    };
    assert_eq!(shorthand.kind, OperationKind::Query);
    assert_eq!(shorthand.name, None);
    assert_eq!(shorthand.location, Location::new(24, 1));
}

#[test]
fn mutation_and_subscription() {
    let document =
        parse_ok("mutation M($x: Int) @d { set(x: $x) } subscription S { events { id } }");
    assert_eq!(document.operations().count(), 2);
    let operations: Vec<_> = document.operations().collect();
    assert_eq!(operations[0].kind, OperationKind::Mutation);
    assert_eq!(operations[0].name, Some("M"));
    assert_eq!(operations[1].kind, OperationKind::Subscription);
    assert_eq!(operations[1].name, Some("S"));
}

#[test]
fn string_escapes_and_block_strings() {
    let source = r#"{
  field(
    escaped: "\t\n\r\b\f\"\\\/"
    block: """
        Hello,
          World!

        Yours,
          GraphQL.
    """
    quoted: """a \""" b"""
    spaced: """  spaces  """
    simple: """simple"""
    empty: """"""
    emptyBlock: """"""
  )
}"#;

    let document = parse_ok(source);
    let field = first_field(&document);
    let value = |name: &str| -> &Value<'_> {
        field
            .argument(name)
            .map(|argument| &argument.value)
            .unwrap_or_else(|| panic!("missing argument `{name}`"))
    };

    assert_eq!(
        value("escaped"),
        &Value::String(Cow::Owned("\t\n\r\u{8}\u{c}\"\\/".to_owned()))
    );
    assert_eq!(
        value("block"),
        &Value::String(Cow::Owned(
            "Hello,\n  World!\n\nYours,\n  GraphQL.".to_owned()
        ))
    );
    assert_eq!(
        value("quoted"),
        &Value::String(Cow::Owned("a \"\"\" b".to_owned()))
    );
    assert_eq!(
        value("spaced"),
        &Value::String(Cow::Owned("  spaces  ".to_owned()))
    );
    assert_eq!(
        value("simple"),
        &Value::String(Cow::Owned("simple".to_owned()))
    );
    assert_eq!(value("empty"), &Value::String(Cow::Owned(String::new())));
    assert_eq!(
        value("emptyBlock"),
        &Value::String(Cow::Owned(String::new()))
    );
}

#[test]
fn unicode_escapes() {
    let document = parse_ok(r#"{ f(arg: "\u0041\u00E9\uD83D\uDE00\u{1F600}\u{41}") }"#);
    assert_eq!(
        first_argument_value(&document),
        &Value::String(Cow::Owned("Aé😀😀A".to_owned()))
    );
}

#[test]
fn plain_strings_borrow_from_source() {
    let source = r#"{ f(arg: "plain", other: "\u0041") }"#;
    let document = parse_ok(source);
    let field = first_field(&document);

    match &field.arguments[0].value {
        Value::String(Cow::Borrowed(value)) => assert_eq!(*value, "plain"),
        other => panic!("expected a borrowed string, got {other:?}"),
    }
    match &field.arguments[1].value {
        Value::String(Cow::Owned(value)) => assert_eq!(value, "A"),
        other => panic!("expected an owned string, got {other:?}"),
    }

    // Names always borrow from the source.
    assert_eq!(field.name, "f");
    assert_eq!(field.arguments[0].name, "arg");
}

#[test]
fn ignored_tokens_and_empty_documents() {
    // A BOM, commas, comments and whitespace are all ignored.
    let source = "\u{feff},# leading comment\r\n{, f, (a: 1,, b: 2),} # trailing comment\r";
    let document = parse_ok(source);
    let field = first_field(&document);
    assert_eq!(field.arguments.len(), 2);
    assert_eq!(field.location, Location::new(2, 4));

    // `Document : Definition+`, so whitespace and comments alone are not a document.
    assert_eq!(query_error("").message(), "Syntax Error: Unexpected <EOF>.");
    assert_eq!(
        query_error("   \n\t,# only a comment").message(),
        "Syntax Error: Unexpected <EOF>."
    );
}

#[test]
fn block_string_dedent_examples() {
    let cases: &[(&str, &str)] = &[
        ("\"\"\"\"\"\"", ""),
        ("\"\"\"simple\"\"\"", "simple"),
        ("\"\"\"  spaces  \"\"\"", "  spaces  "),
        ("\"\"\"\n  a\n  b\n\"\"\"", "a\nb"),
        (
            "\"\"\"\n    Hello,\n      World!\n\n    Yours,\n      GraphQL.\n\"\"\"",
            "Hello,\n  World!\n\nYours,\n  GraphQL.",
        ),
        ("\"\"\"a \\\"\"\" b\"\"\"", "a \"\"\" b"),
        // The first line is never dedented, only subsequent lines.
        ("\"\"\" first\n    second\n\"\"\"", " first\nsecond"),
        // Blank lines shorter than the common indent become empty.
        ("\"\"\"\n      a\n\n        b\n\"\"\"", "a\n\n  b"),
    ];

    for (literal, expected) in cases {
        let source = format!("{{ f(arg: {literal}) }}");
        let document = parse_ok(&source);
        assert_eq!(
            first_argument_value(&document),
            &Value::String(Cow::Owned((*expected).to_owned())),
            "block string {literal}"
        );
    }
}

// ---------------------------------------------------------------------------------------------
// Type system definitions
// ---------------------------------------------------------------------------------------------

#[test]
fn schema_kitchen_sink() {
    let source = concat!(
        "\"\"\"\n",
        r#"The schema.
"""
schema @schemaDir(active: true) {
  query: Query
  mutation: Mutation
  subscription: Subscription
}

"A date and time"
scalar DateTime @specifiedBy(url: "https://example.com")

"""The query root."""
type Query implements & Node & Named @key(fields: "id") {
  "Fetch a node."
  node(
    "The identifier."
    id: ID!
    filter: Filter = {limit: 10, tags: ["a"]} @argDir
  ): Node @deprecated(reason: "Use nodeById")
  plain: String
}

interface Node implements Entity @interfaceDir {
  id: ID!
}

interface Entity {
  id: ID!
}

union SearchResult @unionDir = | User | Post

enum Episode @enumDir {
  "A New Hope"
  NEWHOPE @deprecated(reason: "old")
  EMPIRE
  JEDI
}

input Filter @inputDir {
  "Maximum results."
  limit: Int = 10 @constraint(min: 1)
  tags: [String!] = ["a", "b"]
}

directive @auth(role: Role = ADMIN) repeatable on FIELD_DEFINITION | OBJECT | SCHEMA

extend schema @extended { mutation: Mutation }
extend scalar DateTime @format(pattern: "iso")
extend type Query implements Extra @extra { extra: String }
extend interface Node { extra: String }
extend union SearchResult = | Comment
extend enum Episode { SOLO }
extend input Filter { offset: Int = 0 }
"#,
    );

    let document = schema(source);
    assert_eq!(document.definitions.len(), 16);

    let Definition::TypeSystem(TypeSystemDefinition::Schema(schema_definition)) =
        &document.definitions[0]
    else {
        panic!("expected schema definition");
    };
    assert_eq!(
        schema_definition.description.as_deref(),
        Some("The schema.")
    );
    assert_eq!(schema_definition.location, Location::new(1, 1));
    assert_eq!(schema_definition.directives[0].name, "schemaDir");
    assert_eq!(
        schema_definition.directives[0]
            .argument("active")
            .map(|argument| &argument.value),
        Some(&Value::Boolean(true))
    );
    assert_eq!(schema_definition.operations.len(), 3);
    assert_eq!(
        schema_definition.operations[0].operation,
        OperationKind::Query
    );
    assert_eq!(schema_definition.operations[0].ty, "Query");
    assert_eq!(
        schema_definition.operations[1].operation,
        OperationKind::Mutation
    );
    assert_eq!(
        schema_definition.operations[2].operation,
        OperationKind::Subscription
    );

    let Definition::TypeSystem(TypeSystemDefinition::Type(scalar)) = &document.definitions[1]
    else {
        panic!("expected scalar");
    };
    assert_eq!(scalar.name, "DateTime");
    assert_eq!(scalar.description.as_deref(), Some("A date and time"));
    assert_eq!(scalar.kind, TypeDefinitionKind::Scalar);
    assert_eq!(scalar.directives[0].name, "specifiedBy");

    let Definition::TypeSystem(TypeSystemDefinition::Type(object)) = &document.definitions[2]
    else {
        panic!("expected object type");
    };
    assert_eq!(object.name, "Query");
    assert_eq!(object.description.as_deref(), Some("The query root."));
    assert_eq!(object.directives[0].name, "key");
    let TypeDefinitionKind::Object(object) = &object.kind else {
        panic!("expected object kind");
    };
    assert_eq!(object.interfaces, ["Node", "Named"]);
    assert_eq!(object.fields.len(), 2);

    let node: &FieldDefinition<'_> = &object.fields[0];
    assert_eq!(node.name, "node");
    assert_eq!(node.description.as_deref(), Some("Fetch a node."));
    assert_eq!(node.arguments.len(), 2);
    assert_eq!(node.arguments[0].name, "id");
    assert_eq!(
        node.arguments[0].description.as_deref(),
        Some("The identifier.")
    );
    assert_eq!(
        node.arguments[0].ty,
        Type::NonNull(Box::new(Type::Named("ID")))
    );
    let filter: &InputValueDefinition<'_> = &node.arguments[1];
    assert_eq!(filter.name, "filter");
    assert_eq!(filter.ty, Type::Named("Filter"));
    assert_eq!(filter.directives[0].name, "argDir");
    let Some(Value::Object(default)) = &filter.default_value else {
        panic!("expected object default");
    };
    assert_eq!(default[0].name, "limit");
    assert_eq!(default[0].value, Value::Int(10));
    assert_eq!(
        default[1].value,
        Value::List(vec![Value::String(Cow::Borrowed("a"))])
    );
    assert_eq!(node.ty, Type::Named("Node"));
    assert_eq!(node.directives[0].name, "deprecated");

    let plain = &object.fields[1];
    assert_eq!(plain.name, "plain");
    assert!(plain.arguments.is_empty());
    assert_eq!(plain.description, None);

    let Definition::TypeSystem(TypeSystemDefinition::Type(node_interface)) =
        &document.definitions[3]
    else {
        panic!("expected interface");
    };
    assert_eq!(node_interface.name, "Node");
    assert_eq!(node_interface.directives[0].name, "interfaceDir");
    let TypeDefinitionKind::Interface(interface) = &node_interface.kind else {
        panic!("expected interface kind");
    };
    assert_eq!(interface.interfaces, ["Entity"]);
    assert_eq!(interface.fields[0].name, "id");

    let Definition::TypeSystem(TypeSystemDefinition::Type(entity)) = &document.definitions[4]
    else {
        panic!("expected interface");
    };
    let TypeDefinitionKind::Interface(entity) = &entity.kind else {
        panic!("expected interface kind");
    };
    assert!(entity.interfaces.is_empty());

    let Definition::TypeSystem(TypeSystemDefinition::Type(union)) = &document.definitions[5] else {
        panic!("expected union");
    };
    assert_eq!(union.name, "SearchResult");
    let TypeDefinitionKind::Union(union) = &union.kind else {
        panic!("expected union kind");
    };
    assert_eq!(union.types, ["User", "Post"]);

    let Definition::TypeSystem(TypeSystemDefinition::Type(enum_definition)) =
        &document.definitions[6]
    else {
        panic!("expected enum");
    };
    assert_eq!(enum_definition.name, "Episode");
    let TypeDefinitionKind::Enum(enum_definition) = &enum_definition.kind else {
        panic!("expected enum kind");
    };
    assert_eq!(enum_definition.values.len(), 3);
    let new_hope: &EnumValueDefinition<'_> = &enum_definition.values[0];
    assert_eq!(new_hope.name, "NEWHOPE");
    assert_eq!(new_hope.description.as_deref(), Some("A New Hope"));
    assert_eq!(new_hope.directives[0].name, "deprecated");
    assert_eq!(enum_definition.values[1].description, None);

    let Definition::TypeSystem(TypeSystemDefinition::Type(input)) = &document.definitions[7] else {
        panic!("expected input object");
    };
    assert_eq!(input.name, "Filter");
    let TypeDefinitionKind::InputObject(input) = &input.kind else {
        panic!("expected input kind");
    };
    assert_eq!(input.fields.len(), 2);
    assert_eq!(
        input.fields[0].description.as_deref(),
        Some("Maximum results.")
    );
    assert_eq!(input.fields[0].default_value, Some(Value::Int(10)));
    assert_eq!(input.fields[0].directives[0].name, "constraint");
    assert_eq!(
        input.fields[1].ty,
        Type::List(Box::new(Type::NonNull(Box::new(Type::Named("String")))))
    );

    let Definition::TypeSystem(TypeSystemDefinition::Directive(directive)) =
        &document.definitions[8]
    else {
        panic!("expected directive definition");
    };
    assert_eq!(directive.name, "auth");
    assert!(directive.repeatable);
    assert_eq!(directive.arguments.len(), 1);
    assert_eq!(directive.arguments[0].name, "role");
    assert_eq!(
        directive.arguments[0].default_value,
        Some(Value::Enum("ADMIN"))
    );
    assert_eq!(
        directive.locations,
        [
            DirectiveLocation::FieldDefinition,
            DirectiveLocation::Object,
            DirectiveLocation::Schema,
        ]
    );

    let Definition::TypeSystem(TypeSystemDefinition::SchemaExtension(extension)) =
        &document.definitions[9]
    else {
        panic!("expected schema extension");
    };
    assert_eq!(extension.directives[0].name, "extended");
    assert_eq!(extension.operations[0].operation, OperationKind::Mutation);
    assert_eq!(extension.operations[0].ty, "Mutation");

    let Definition::TypeSystem(TypeSystemDefinition::TypeExtension(extension)) =
        &document.definitions[10]
    else {
        panic!("expected scalar extension");
    };
    assert_eq!(extension.name, "DateTime");
    assert_eq!(extension.kind, TypeExtensionKind::Scalar);
    assert_eq!(extension.directives[0].name, "format");

    let Definition::TypeSystem(TypeSystemDefinition::TypeExtension(extension)) =
        &document.definitions[11]
    else {
        panic!("expected object extension");
    };
    assert_eq!(extension.name, "Query");
    let TypeExtensionKind::Object(object) = &extension.kind else {
        panic!("expected object extension kind");
    };
    assert_eq!(object.interfaces, ["Extra"]);
    assert_eq!(object.fields[0].name, "extra");
    assert_eq!(extension.directives[0].name, "extra");

    let Definition::TypeSystem(TypeSystemDefinition::TypeExtension(extension)) =
        &document.definitions[12]
    else {
        panic!("expected interface extension");
    };
    let TypeExtensionKind::Interface(interface) = &extension.kind else {
        panic!("expected interface extension kind");
    };
    assert_eq!(interface.fields[0].name, "extra");

    let Definition::TypeSystem(TypeSystemDefinition::TypeExtension(extension)) =
        &document.definitions[13]
    else {
        panic!("expected union extension");
    };
    let TypeExtensionKind::Union(union) = &extension.kind else {
        panic!("expected union extension kind");
    };
    assert_eq!(union.types, ["Comment"]);

    let Definition::TypeSystem(TypeSystemDefinition::TypeExtension(extension)) =
        &document.definitions[14]
    else {
        panic!("expected enum extension");
    };
    let TypeExtensionKind::Enum(enum_extension) = &extension.kind else {
        panic!("expected enum extension kind");
    };
    assert_eq!(enum_extension.values[0].name, "SOLO");

    let Definition::TypeSystem(TypeSystemDefinition::TypeExtension(extension)) =
        &document.definitions[15]
    else {
        panic!("expected input extension");
    };
    let TypeExtensionKind::InputObject(input) = &extension.kind else {
        panic!("expected input extension kind");
    };
    assert_eq!(input.fields[0].name, "offset");
    assert_eq!(input.fields[0].default_value, Some(Value::Int(0)));
}

#[test]
fn parse_query_accepts_type_system_definitions() {
    let document = parse_ok("type Query { hello: String }");
    assert_eq!(document.definitions.len(), 1);
    let Definition::TypeSystem(TypeSystemDefinition::Type(definition)) = &document.definitions[0]
    else {
        panic!("expected a type definition");
    };
    assert_eq!(definition.name, "Query");
}

#[test]
fn schema_rejects_executable_definitions() {
    for source in [
        "query Q { field }",
        "mutation { field }",
        "subscription S { field }",
        "{ field }",
        "fragment F on T { field }",
        "\"description\" query Q { field }",
    ] {
        let error = schema_error(source);
        assert!(
            error
                .message()
                .contains("Executable definitions are not allowed in a schema document"),
            "source {source:?} produced: {}",
            error.message()
        );
        assert_eq!(error.locations().len(), 1);
    }

    // Executable definitions are still accepted by the executable entry point.
    assert_eq!(parse_ok("query Q { field }").definitions.len(), 1);
}

#[test]
fn directive_locations_and_repeatable() {
    let document = schema(
        "directive @a on QUERY\ndirective @b repeatable on | FIELD | FRAGMENT_SPREAD\ndirective @c(x: Int = 1, y: String) on ARGUMENT_DEFINITION",
    );
    assert_eq!(document.definitions.len(), 3);

    let location_sets: Vec<Vec<DirectiveLocation>> = document
        .definitions
        .iter()
        .filter_map(|definition| match definition {
            Definition::TypeSystem(TypeSystemDefinition::Directive(directive)) => {
                Some(directive.locations.clone())
            }
            _ => None,
        })
        .collect();
    assert_eq!(location_sets[0], [DirectiveLocation::Query]);
    assert_eq!(
        location_sets[1],
        [DirectiveLocation::Field, DirectiveLocation::FragmentSpread]
    );
    assert_eq!(location_sets[2], [DirectiveLocation::ArgumentDefinition]);
}

#[test]
fn nested_types_and_extensions() {
    let document = schema(
        "type Query { a: [[Int!]!]! b: [String] c: Int }\nscalar S\nextend scalar S @d\nextend schema @d",
    );
    assert_eq!(document.definitions.len(), 4);

    let Definition::TypeSystem(TypeSystemDefinition::Type(query)) = &document.definitions[0] else {
        panic!("expected type definition");
    };
    let TypeDefinitionKind::Object(object) = &query.kind else {
        panic!("expected object kind");
    };
    assert_eq!(
        object.fields[0].ty,
        Type::NonNull(Box::new(Type::List(Box::new(Type::NonNull(Box::new(
            Type::List(Box::new(Type::NonNull(Box::new(Type::Named("Int")))))
        ))))))
    );
    assert_eq!(
        object.fields[1].ty,
        Type::List(Box::new(Type::Named("String")))
    );
    assert_eq!(object.fields[2].ty, Type::Named("Int"));

    let Definition::TypeSystem(TypeSystemDefinition::TypeExtension(scalar)) =
        &document.definitions[2]
    else {
        panic!("expected scalar extension");
    };
    assert_eq!(scalar.directives[0].name, "d");

    let Definition::TypeSystem(TypeSystemDefinition::SchemaExtension(extension)) =
        &document.definitions[3]
    else {
        panic!("expected schema extension");
    };
    assert_eq!(extension.directives[0].name, "d");
    assert!(extension.operations.is_empty());
}

#[test]
fn empty_blocks_are_rejected() {
    let cases: &[(&str, &str)] = &[
        (
            "query Q() { f }",
            "Syntax Error: Expected \"$\", found \")\".",
        ),
        (
            "query Q @d() { f }",
            "Syntax Error: Expected Name, found \")\".",
        ),
        ("query Q { }", "Syntax Error: Expected Name, found \"}\"."),
        ("schema { }", "Syntax Error: Expected Name, found \"}\"."),
        ("type T { }", "Syntax Error: Expected Name, found \"}\"."),
        ("enum E { }", "Syntax Error: Expected Name, found \"}\"."),
        ("input I { }", "Syntax Error: Expected Name, found \"}\"."),
        (
            "type T { f(): Int }",
            "Syntax Error: Expected Name, found \")\".",
        ),
        (
            "directive @d() on FIELD",
            "Syntax Error: Expected Name, found \")\".",
        ),
        ("extend schema", "Syntax Error: Unexpected <EOF>."),
        ("extend scalar S", "Syntax Error: Unexpected <EOF>."),
        ("extend type T", "Syntax Error: Unexpected <EOF>."),
        ("extend interface I", "Syntax Error: Unexpected <EOF>."),
        ("extend union U", "Syntax Error: Unexpected <EOF>."),
        ("extend enum E", "Syntax Error: Unexpected <EOF>."),
        ("extend input I", "Syntax Error: Unexpected <EOF>."),
    ];

    for (source, message) in cases {
        assert_eq!(query_error(source).message(), *message, "source {source:?}");
    }
}

#[test]
fn description_on_directive_definition() {
    let document = schema("\"Authorises a field.\"\ndirective @auth on FIELD");
    let Definition::TypeSystem(TypeSystemDefinition::Directive(directive)) =
        &document.definitions[0]
    else {
        panic!("expected directive definition");
    };
    assert_eq!(
        directive.description.as_deref(),
        Some("Authorises a field.")
    );
    assert_eq!(directive.location, Location::new(1, 1));
}

#[test]
fn block_string_updates_line_and_column() {
    let document = parse_ok("{ f(arg: \"\"\"\nline\n\"\"\") x }");
    let Selection::Field(field) = &operation(&document).selection_set.items[1] else {
        panic!("expected second field");
    };
    assert_eq!(field.name, "x");
    assert_eq!(field.location, Location::new(3, 6));
}

#[test]
fn crlf_in_block_strings_is_normalised() {
    let document = parse_ok("{ f(arg: \"\"\"\r\n  a\r\n  b\r\n\"\"\") }");
    assert_eq!(
        first_argument_value(&document),
        &Value::String(Cow::Owned("a\nb".to_owned()))
    );
}

#[test]
fn documents_require_at_least_one_definition() {
    for source in ["", "   ", "# nothing here\n"] {
        assert_eq!(
            query_error(source).message(),
            "Syntax Error: Unexpected <EOF>."
        );
        assert_eq!(
            schema_error(source).message(),
            "Syntax Error: Unexpected <EOF>."
        );
    }
}

// ---------------------------------------------------------------------------------------------
// Locations
// ---------------------------------------------------------------------------------------------

#[test]
fn crlf_updates_line_and_column() {
    let document = parse_ok("{\r\n  f\r\n}");
    let Selection::Field(field) = &operation(&document).selection_set.items[0] else {
        panic!("expected field");
    };
    assert_eq!(field.name, "f");
    assert_eq!(field.location, Location::new(2, 3));

    let document = parse_ok("# comment\r\nquery Q { f }");
    assert_eq!(operation(&document).location, Location::new(2, 1));
}

#[test]
fn utf8_columns_count_characters() {
    // `日`, `本` and `語` are three scalar values, each advancing the column by one.
    let document = parse_ok("{ f(arg: \"日本語\") x }");
    let field = first_field(&document);
    assert_eq!(field.name, "f");
    let Selection::Field(x) = &operation(&document).selection_set.items[1] else {
        panic!("expected second field");
    };
    assert_eq!(x.name, "x");
    assert_eq!(x.location, Location::new(1, 17));

    // An emoji is one scalar value (one column) even though it is four bytes.
    let error = query_error("{ f(arg: \"😀\" }");
    assert_error(&error, "Syntax Error: Expected Name, found \"}\".", 1, 14);

    // A non-ASCII character outside a string is still reported as one column.
    let error = query_error("{ a😀 }");
    assert_error(&error, "Syntax Error: Unexpected character: U+1F600.", 1, 4);

    // Multi-byte characters inside comments do not disturb line tracking.
    let document = parse_ok("# 😀 emoji\nquery Q { f }");
    assert_eq!(operation(&document).location, Location::new(2, 1));
    let Selection::Field(field) = &operation(&document).selection_set.items[0] else {
        panic!("expected field");
    };
    assert_eq!(field.location, Location::new(2, 11));
}

// ---------------------------------------------------------------------------------------------
// Syntax errors
// ---------------------------------------------------------------------------------------------

#[test]
fn syntax_errors() {
    let cases: &[(&str, &str, u32, u32)] = &[
        (
            "{ field ",
            "Syntax Error: Expected Name, found <EOF>.",
            1,
            9,
        ),
        (
            "query Q { field(arg 1) }",
            "Syntax Error: Expected \":\", found Int \"1\".",
            1,
            21,
        ),
        (
            "{ f(arg: \"abc) }",
            "Syntax Error: Unterminated string.",
            1,
            17,
        ),
        (
            "{ f(arg: 01) }",
            "Syntax Error: Invalid number, unexpected digit after 0: \"1\".",
            1,
            11,
        ),
        (
            "{ f(arg: 123abc) }",
            "Syntax Error: Invalid number, expected digit but got: \"a\".",
            1,
            13,
        ),
        (
            "{ f(arg: 1.2.3) }",
            "Syntax Error: Invalid number, expected digit but got: \".\".",
            1,
            13,
        ),
        (
            "{ f(arg: 1.) }",
            "Syntax Error: Invalid number, expected digit but got: \")\".",
            1,
            12,
        ),
        ("{ ? }", "Syntax Error: Unexpected character: \"?\".", 1, 3),
        (
            "query { f @ }",
            "Syntax Error: Expected Name, found \"}\".",
            1,
            13,
        ),
        (
            "fragment on on T { x }",
            "Syntax Error: Unexpected Name \"on\".",
            1,
            10,
        ),
        (
            "enum E { true }",
            "Syntax Error: Name \"true\" is reserved and cannot be used for an enum value.",
            1,
            10,
        ),
        (
            "directive @d on NOPE",
            "Syntax Error: Unexpected Name \"NOPE\".",
            1,
            17,
        ),
        (
            "directive @d FIELD",
            "Syntax Error: Expected \"on\", found Name \"FIELD\".",
            1,
            14,
        ),
        (
            "query Q($v: Int = $x) { f }",
            "Syntax Error: Unexpected variable \"$x\" in constant value.",
            1,
            19,
        ),
        (
            "type Query implements { f: String }",
            "Syntax Error: Expected Name, found \"{\".",
            1,
            23,
        ),
        ("{ f } }", "Syntax Error: Unexpected \"}\".", 1, 7),
        (
            "query Q { field(arg: ) }",
            "Syntax Error: Unexpected \")\".",
            1,
            22,
        ),
        (
            "{ f(arg: 1true) }",
            "Syntax Error: Invalid number, expected digit but got: \"t\".",
            1,
            11,
        ),
        (
            "type Query { field: String! = 1 }",
            "Syntax Error: Expected Name, found \"=\".",
            1,
            29,
        ),
        (
            "schema { query }",
            "Syntax Error: Expected \":\", found \"}\".",
            1,
            16,
        ),
        (
            "extend wat Foo",
            "Syntax Error: Unexpected Name \"wat\".",
            1,
            8,
        ),
        ("extend {", "Syntax Error: Unexpected \"{\".", 1, 8),
        (
            "\"description\" query Q { field }",
            "Syntax Error: Unexpected description, descriptions are supported only on type definitions.",
            1,
            1,
        ),
        (
            "\"description\" extend type T @d",
            "Syntax Error: Unexpected description, descriptions are supported only on type definitions.",
            1,
            1,
        ),
        ("\"description\"", "Syntax Error: Unexpected <EOF>.", 1, 14),
        (
            "directive @d on FIELD |",
            "Syntax Error: Expected Name, found <EOF>.",
            1,
            24,
        ),
    ];

    for (source, message, line, column) in cases {
        let error = query_error(source);
        assert_error(&error, message, *line, *column);
    }
}

#[test]
fn invalid_escapes() {
    let error = query_error(r#"{ f(arg: "\x") }"#);
    assert_eq!(
        error.message(),
        "Syntax Error: Invalid character escape sequence: \"\\x\"."
    );

    let error = query_error(r#"{ f(arg: "\uZZZZ") }"#);
    assert_eq!(
        error.message(),
        "Syntax Error: Invalid Unicode escape sequence: \"\\uZZZZ\"."
    );

    let error = query_error(r#"{ f(arg: "\uD800") }"#);
    assert_eq!(
        error.message(),
        "Syntax Error: Invalid Unicode escape sequence: \"\\uD800\"."
    );

    let error = query_error(r#"{ f(arg: "\uDE00") }"#);
    assert_eq!(
        error.message(),
        "Syntax Error: Invalid Unicode escape sequence: \"\\uDE00\"."
    );

    let error = query_error("{ f(arg: \"bad\u{7}char\") }");
    assert_eq!(
        error.message(),
        "Syntax Error: Invalid character within String: U+0007."
    );

    let error = query_error(r#"{ f(arg: "\u{}") }"#);
    assert_eq!(
        error.message(),
        "Syntax Error: Invalid Unicode escape sequence: \"\\u{}\"."
    );

    let error = query_error(r#"{ f(arg: "\u{ZZ}") }"#);
    assert_eq!(
        error.message(),
        "Syntax Error: Invalid Unicode escape sequence: \"\\u{Z\"."
    );

    let error = query_error(r#"{ f(arg: "\u{110000}") }"#);
    assert_eq!(
        error.message(),
        "Syntax Error: Invalid Unicode escape sequence: \"\\u{110000}\"."
    );

    let error = query_error("{ f(arg: \"abc\\");
    assert_error(
        &error,
        "Syntax Error: Invalid character escape sequence: \"\\\".",
        1,
        14,
    );
}

#[test]
fn single_quotes_get_a_helpful_message() {
    let error = query_error("{ f(arg: 'x') }");
    assert_error(
        &error,
        "Syntax Error: Unexpected single quote character ('), did you mean to use a double quote (\")?",
        1,
        10,
    );
}

#[test]
fn schema_errors() {
    let error = schema_error("type Query { field: }");
    assert_error(&error, "Syntax Error: Expected Name, found \"}\".", 1, 21);

    let error = schema_error("directive @d on NOPE");
    assert_error(&error, "Syntax Error: Unexpected Name \"NOPE\".", 1, 17);

    let error = schema_error("enum E { true }");
    assert_eq!(
        error.message(),
        "Syntax Error: Name \"true\" is reserved and cannot be used for an enum value."
    );
}

#[test]
fn deeply_nested_documents_are_rejected_without_overflowing_the_stack() {
    let depth = 600;
    let mut query = String::new();
    for _ in 0..depth {
        query.push_str("{ a");
    }
    for _ in 0..depth {
        query.push_str(" }");
    }
    let error = gql::parser::parse_query(&query).expect_err("nesting limit");
    assert!(error.message().contains("nested more than"), "{error}");

    let mut value = String::from("{ f(v: ");
    for _ in 0..depth {
        value.push('[');
    }
    for _ in 0..depth {
        value.push(']');
    }
    value.push_str(") }");
    let error = gql::parser::parse_query(&value).expect_err("value nesting limit");
    assert!(error.message().contains("nested more than"), "{error}");
}
