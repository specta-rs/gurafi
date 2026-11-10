use gql::{
    DirectiveDefinition, EnumType, Field, InputObjectType, InputValue, InterfaceType, ObjectType,
    ScalarType, Schema, TypeRef, UnionType, ast::DirectiveLocation,
};

fn rich_schema() -> Schema {
    let mut builder = Schema::builder();

    builder.add_type(
        ScalarType::new("Date")
            .description("An ISO-8601 date.")
            .specified_by_url("https://example.com/date")
            .into(),
    );

    builder.add_type(
        EnumType::new("Color")
            .description("A colour.")
            .value_with("RED", Some("The colour red.".into()), None)
            .value_with("GREEN", None, None)
            .value_with("BLUE", None, Some("Use RED.".into()))
            .into(),
    );

    builder.add_type(
        InputObjectType::new("Filter")
            .description("Filtering options.")
            .field(
                InputValue::new("limit", TypeRef::int())
                    .description("Maximum results.")
                    .default_value(10),
            )
            .field(InputValue::new("term", TypeRef::string()))
            .into(),
    );

    builder.add_type(
        InputObjectType::new("Lookup")
            .field(InputValue::new("id", TypeRef::id()))
            .one_of()
            .into(),
    );

    builder.add_type(
        InterfaceType::new("Node")
            .description("Anything with an id.")
            .field(Field::new("id", TypeRef::id().non_null()))
            .into(),
    );

    builder.add_type(
        ObjectType::new("User")
            .description("A user.")
            .implements("Node")
            .field(Field::new("id", TypeRef::id().non_null()))
            .field(Field::new("name", TypeRef::string()).description("The display name."))
            .field(
                Field::new(
                    "friends",
                    TypeRef::named("User").non_null().list().non_null(),
                )
                .arg(InputValue::new("first", TypeRef::int()).default_value(10))
                .arg(
                    InputValue::new("filter", TypeRef::named("Filter"))
                        .description("Narrow the friends."),
                ),
            )
            .field(Field::new("color", TypeRef::named("Color")))
            .field(Field::new("legacy", TypeRef::string()).deprecation_reason("Use name."))
            .into(),
    );

    builder.add_type(
        ObjectType::new("Post")
            .implements("Node")
            .field(Field::new("id", TypeRef::id().non_null()))
            .field(Field::new("title", TypeRef::string()))
            .into(),
    );

    builder.add_type(
        UnionType::new("SearchResult")
            .description("Anything searchable.")
            .member("User")
            .member("Post")
            .into(),
    );

    builder.add_type(
        ObjectType::new("Query")
            .field(
                Field::new("node", TypeRef::named("Node"))
                    .arg(InputValue::new("id", TypeRef::id().non_null())),
            )
            .field(
                Field::new("users", TypeRef::named("User").non_null().list().non_null()).arg(
                    InputValue::new("filter", TypeRef::named("Filter"))
                        .description("Filtering options."),
                ),
            )
            .field(
                Field::new(
                    "search",
                    TypeRef::named("SearchResult").non_null().list().non_null(),
                )
                .arg(InputValue::new("term", TypeRef::string().non_null())),
            )
            .into(),
    );

    builder.add_type(
        ObjectType::new("Mutation")
            .field(
                Field::new("createUser", TypeRef::named("User").non_null())
                    .arg(InputValue::new("name", TypeRef::string().non_null())),
            )
            .into(),
    );

    builder.add_directive(
        DirectiveDefinition::new("auth")
            .description("Requires a role.")
            .arg(
                InputValue::new("role", TypeRef::string().non_null())
                    .description("The required role."),
            )
            .arg(InputValue::new("legacy", TypeRef::boolean()))
            .location(DirectiveLocation::FieldDefinition)
            .location(DirectiveLocation::Object)
            .repeatable(),
    );

    builder
        .query("Query")
        .mutation("Mutation")
        .build()
        .expect("valid schema")
}

#[test]
fn prints_rich_schema() {
    let schema = rich_schema();
    let expected = r#""""Requires a role."""
directive @auth(
  """The required role."""
  role: String!
  legacy: Boolean
) repeatable on FIELD_DEFINITION | OBJECT

"""An ISO-8601 date."""
scalar Date @specifiedBy(url: "https://example.com/date")

"""A colour."""
enum Color {
  """The colour red."""
  RED
  GREEN
  BLUE @deprecated(reason: "Use RED.")
}

"""Filtering options."""
input Filter {
  """Maximum results."""
  limit: Int = 10
  term: String
}

input Lookup @oneOf {
  id: ID
}

"""Anything with an id."""
interface Node {
  id: ID!
}

"""A user."""
type User implements Node {
  id: ID!
  """The display name."""
  name: String
  friends(
    first: Int = 10
    """Narrow the friends."""
    filter: Filter
  ): [User!]!
  color: Color
  legacy: String @deprecated(reason: "Use name.")
}

type Post implements Node {
  id: ID!
  title: String
}

"""Anything searchable."""
union SearchResult = User | Post

type Query {
  node(id: ID!): Node
  users(
    """Filtering options."""
    filter: Filter
  ): [User!]!
  search(term: String!): [SearchResult!]!
}

type Mutation {
  createUser(name: String!): User!
}
"#;

    assert_eq!(schema.sdl(), expected);
}

#[test]
fn prints_explicit_schema_definition_and_description() {
    let mut builder = Schema::builder().description("My schema.");
    builder.add_type(
        ObjectType::new("Root")
            .field(Field::new("hello", TypeRef::string()))
            .into(),
    );
    let schema = builder.query("Root").build().expect("valid schema");

    assert_eq!(
        schema.sdl(),
        "\"\"\"My schema.\"\"\"\nschema {\n  query: Root\n}\n\ntype Root {\n  hello: String\n}\n"
    );
}

#[test]
fn omits_schema_definition_when_defaults_apply() {
    let mut builder = Schema::builder();
    builder.add_type(
        ObjectType::new("Query")
            .field(Field::new("hello", TypeRef::string()))
            .into(),
    );
    let schema = builder.build().expect("valid schema");

    assert_eq!(schema.sdl(), "type Query {\n  hello: String\n}\n");
}

#[test]
fn escapes_strings_and_multiline_descriptions() {
    let mut builder = Schema::builder();
    builder.add_type(
        ObjectType::new("Query")
            .description("First line\nSecond \"line\"")
            .field(
                Field::new("echo", TypeRef::string())
                    .arg(
                        InputValue::new("text", TypeRef::string())
                            .default_value("line\n\"quote\" and \\ backslash"),
                    )
                    .description("Says \"\"\"hello\"\"\"."),
            )
            .field(Field::new("quoted", TypeRef::string()).description("Ends with a quote\""))
            .field(Field::new("slashed", TypeRef::string()).description("Ends with a backslash\\"))
            .into(),
    );
    let schema = builder.build().expect("valid schema");

    let printed = schema.sdl();
    assert_eq!(
        printed,
        concat!(
            "\"\"\"\n",
            "First line\n",
            "Second \"line\"\n",
            "\"\"\"\n",
            "type Query {\n",
            "  \"\"\"Says \\\"\"\"hello\\\"\"\".\"\"\"\n",
            "  echo(text: String = \"line\\n\\\"quote\\\" and \\\\ backslash\"): String\n",
            "  \"\"\"\n",
            "  Ends with a quote\"\n",
            "  \"\"\"\n",
            "  quoted: String\n",
            "  \"\"\"\n",
            "  Ends with a backslash\\\n",
            "  \"\"\"\n",
            "  slashed: String\n",
            "}\n",
        )
    );

    // The output must be valid SDL and must round-trip the descriptions exactly.
    let document = gql::parser::parse_schema(&printed).expect("printed schema re-parses");
    let query = document
        .definitions
        .iter()
        .find_map(|definition| match definition {
            gql::ast::Definition::TypeSystem(gql::ast::TypeSystemDefinition::Type(ty))
                if ty.name == "Query" =>
            {
                Some(ty)
            }
            _ => None,
        })
        .expect("Query type");
    let gql::ast::TypeDefinitionKind::Object(object) = &query.kind else {
        panic!("Query is an object");
    };
    assert_eq!(
        query.description.as_deref(),
        Some("First line\nSecond \"line\"")
    );
    let description = |name: &str| {
        object
            .fields
            .iter()
            .find(|field| field.name == name)
            .and_then(|field| field.description.as_deref())
    };
    assert_eq!(description("echo"), Some("Says \"\"\"hello\"\"\"."));
    assert_eq!(description("quoted"), Some("Ends with a quote\""));
    assert_eq!(description("slashed"), Some("Ends with a backslash\\"));
}

#[test]
fn reparses_rich_schema() {
    let printed = rich_schema().sdl();
    let document = gql::parser::parse_schema(&printed).expect("printed schema re-parses");
    assert_eq!(
        document
            .definitions
            .iter()
            .filter(|definition| matches!(definition, gql::ast::Definition::TypeSystem(_)))
            .count(),
        document.definitions.len()
    );
}
