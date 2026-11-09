//! Executable document validation tests.
//!
//! The schema is deliberately rich: objects, an interface with two implementations, a union, an
//! enum, input objects, every built-in scalar, a custom scalar and custom directives. Each test
//! targets one specification rule.

use gql::{
    DirectiveDefinition, EnumType, Field, GraphQLError, InputObjectType, InputValue, InterfaceType,
    Location, ObjectType, ScalarType, Schema, TypeRef, UnionType, ValidationOptions,
    ast::{DirectiveLocation, Field as AstField, Selection, SelectionSet},
    parser::parse_query,
    validate,
};

fn schema() -> Schema {
    let character = InterfaceType::new("Character")
        .field(Field::new("id", TypeRef::id().non_null()))
        .field(Field::new("name", TypeRef::string().non_null()))
        .field(Field::new(
            "friends",
            TypeRef::named("Character").non_null().list().non_null(),
        ));

    let human = ObjectType::new("Human")
        .implements("Character")
        .field(Field::new("id", TypeRef::id().non_null()))
        .field(Field::new("name", TypeRef::string().non_null()))
        .field(Field::new(
            "friends",
            TypeRef::named("Character").non_null().list().non_null(),
        ))
        .field(Field::new("homePlanet", TypeRef::string()))
        .field(Field::new("tag", TypeRef::string()));

    let droid = ObjectType::new("Droid")
        .implements("Character")
        .field(Field::new("id", TypeRef::id().non_null()))
        .field(Field::new("name", TypeRef::string().non_null()))
        .field(Field::new(
            "friends",
            TypeRef::named("Character").non_null().list().non_null(),
        ))
        .field(Field::new("primaryFunction", TypeRef::string()))
        .field(Field::new("tag", TypeRef::int()));

    let user = ObjectType::new("User")
        .field(Field::new("id", TypeRef::id().non_null()))
        .field(Field::new("name", TypeRef::string().non_null()))
        .field(Field::new("age", TypeRef::int()))
        .field(Field::new(
            "friends",
            TypeRef::named("User").non_null().list().non_null(),
        ))
        .field(Field::new("status", TypeRef::named("Episode")));

    // Two interfaces that share a concrete measurement. Their `value` fields have deliberately
    // different nullability so selecting both aliases at the same response key conflicts.
    let readable =
        InterfaceType::new("Readable").field(Field::new("value", TypeRef::string().non_null()));
    let countable = InterfaceType::new("Countable").field(Field::new("value", TypeRef::string()));
    let measurement = ObjectType::new("Measurement")
        .implements("Readable")
        .implements("Countable")
        .field(Field::new("value", TypeRef::string().non_null()));

    let query = ObjectType::new("Query")
        .field(Field::new("hello", TypeRef::string()))
        .field(
            Field::new("user", TypeRef::named("User"))
                .arg(InputValue::new("id", TypeRef::id().non_null())),
        )
        .field(
            Field::new("users", TypeRef::named("User").non_null().list().non_null())
                .arg(InputValue::new("filter", TypeRef::named("FilterInput")))
                .arg(InputValue::new("first", TypeRef::int()).default_value(10)),
        )
        .field(
            Field::new(
                "paginated",
                TypeRef::named("User").non_null().list().non_null(),
            )
            .arg(InputValue::new("count", TypeRef::int().non_null()).default_value(20)),
        )
        .field(
            Field::new("character", TypeRef::named("Character"))
                .arg(InputValue::new("id", TypeRef::id().non_null())),
        )
        .field(
            Field::new(
                "search",
                TypeRef::named("SearchResult").non_null().list().non_null(),
            )
            .arg(InputValue::new("term", TypeRef::string().non_null()))
            .arg(InputValue::new("episode", TypeRef::named("Episode"))),
        )
        .field(Field::new(
            "numbers",
            TypeRef::int().non_null().list().non_null(),
        ))
        .field(Field::new("sum", TypeRef::int()).arg(InputValue::new(
            "values",
            TypeRef::int().non_null().list().non_null(),
        )))
        .field(Field::new("echo", TypeRef::string()).arg(InputValue::new(
            "input",
            TypeRef::named("ReviewInput").non_null(),
        )))
        .field(Field::new("episode", TypeRef::named("Episode")))
        .field(Field::new("customTime", TypeRef::named("DateTime")))
        .field(Field::new("at", TypeRef::string()).arg(InputValue::new(
            "time",
            TypeRef::named("DateTime").non_null(),
        )))
        .field(Field::new("readable", TypeRef::named("Readable")))
        .field(Field::new("measurement", TypeRef::named("Measurement")));

    let mutation =
        ObjectType::new("Mutation").field(Field::new("createUser", TypeRef::named("User")).arg(
            InputValue::new("input", TypeRef::named("ReviewInput").non_null()),
        ));

    let subscription = ObjectType::new("Subscription")
        .field(Field::new("userCreated", TypeRef::named("User")))
        .field(Field::new("counter", TypeRef::int()));

    let filter = InputObjectType::new("FilterInput")
        .field(InputValue::new("limit", TypeRef::int().non_null()))
        .field(InputValue::new("offset", TypeRef::int()).default_value(0))
        .field(InputValue::new("search", TypeRef::string()));

    let review = InputObjectType::new("ReviewInput")
        .field(InputValue::new("stars", TypeRef::int().non_null()))
        .field(InputValue::new("commentary", TypeRef::string()))
        .field(InputValue::new("favoriteColor", TypeRef::named("Episode")))
        .field(InputValue::new("tags", TypeRef::string().non_null().list()))
        .field(InputValue::new("meta", TypeRef::named("DateTime")));

    let episode = EnumType::new("Episode")
        .value("NEWHOPE")
        .value("EMPIRE")
        .value("JEDI");

    let date_time = ScalarType::new("DateTime").parse_literal(|value| match value {
        gql::ast::Value::String(_) => Ok(gql::Value::Null),
        _ => Err("DateTime cannot represent a non-string literal".into()),
    });

    let trace = DirectiveDefinition::new("trace")
        .arg(InputValue::new("label", TypeRef::string()))
        .location(DirectiveLocation::Query)
        .location(DirectiveLocation::Mutation)
        .location(DirectiveLocation::Subscription)
        .location(DirectiveLocation::Field)
        .location(DirectiveLocation::FragmentDefinition)
        .location(DirectiveLocation::FragmentSpread)
        .location(DirectiveLocation::InlineFragment)
        .location(DirectiveLocation::VariableDefinition);

    let tag = DirectiveDefinition::new("tag")
        .arg(InputValue::new("name", TypeRef::string().non_null()))
        .location(DirectiveLocation::Field);

    Schema::builder()
        .scalar(date_time)
        .enumeration(episode)
        .interface(character)
        .interface(readable)
        .interface(countable)
        .object(human)
        .object(droid)
        .object(user)
        .object(measurement)
        .input_object(filter)
        .input_object(review)
        .union(
            UnionType::new("SearchResult")
                .member("Human")
                .member("Droid"),
        )
        .object(query)
        .object(mutation)
        .object(subscription)
        .query("Query")
        .mutation("Mutation")
        .subscription("Subscription")
        .directive(trace)
        .directive(tag)
        .directive(DirectiveDefinition::new("fieldOnly").location(DirectiveLocation::Field))
        .directive(
            DirectiveDefinition::new("repeated")
                .location(DirectiveLocation::Field)
                .repeatable(),
        )
        .build()
        .expect("test schema is valid")
}

fn validate_with(query: &str, introspection: bool) -> Vec<GraphQLError> {
    let schema = schema();
    let document = parse_query(query).expect("test query parses");
    validate(
        &schema,
        &document,
        &ValidationOptions {
            introspection,
            ..Default::default()
        },
    )
}

fn errors(query: &str) -> Vec<GraphQLError> {
    validate_with(query, true)
}

fn messages(errors: &[GraphQLError]) -> Vec<String> {
    errors
        .iter()
        .map(|error| error.message().to_owned())
        .collect()
}

#[track_caller]
fn assert_error(query: &str, needle: &str) {
    let errors = errors(query);
    assert!(
        errors.iter().any(|error| error.message().contains(needle)),
        "expected an error containing {needle:?}, got {:?}",
        messages(&errors)
    );
}

#[track_caller]
fn assert_valid(query: &str) {
    let errors = errors(query);
    assert!(
        errors.is_empty(),
        "expected no errors, got {:?}",
        messages(&errors)
    );
}

#[test]
fn kitchen_sink_is_valid() {
    assert_valid(
        r#"
        query GetUser($id: ID!, $withFriends: Boolean! = true, $filter: FilterInput) {
          hello @trace(label: "hi")
          user(id: $id) {
            id
            name
            age
            status
            friends @include(if: $withFriends) {
              id
              name
            }
          }
          users(filter: $filter, first: 5) {
            id
            name
          }
          character(id: "1") {
            __typename
            id
            name
            ...CharacterFields
            ... on Human {
              homePlanet
            }
            ... on Droid {
              primaryFunction
            }
          }
          search(term: "r2", episode: JEDI) {
            __typename
            ... on Human {
              name
            }
            ... on Droid {
              name
            }
          }
          numbers
          sum(values: [1, 2, 3])
          episode
          customTime
          echo(input: {
            stars: 5
            commentary: "great"
            favoriteColor: EMPIRE
            tags: ["a", "b"]
            meta: "2020-01-01"
          })
        }

        mutation CreateReview($input: ReviewInput!) {
          createUser(input: $input) {
            id
            name
          }
        }

        subscription OnUserCreated {
          userCreated {
            id
            name
          }
        }

        fragment CharacterFields on Character {
          id
          name
          friends {
            id
            name
          }
        }
        "#,
    );
}

#[test]
fn executable_definitions() {
    assert_error(
        "query Q { hello } type Foo { bar: String }",
        "The \"Foo\" definition is not executable.",
    );
    assert_error(
        "query Q { hello } schema { query: Query }",
        "The schema definition is not executable.",
    );
}

#[test]
fn operation_name_uniqueness() {
    assert_error(
        "query A { hello } query A { hello }",
        "There can be only one operation named \"A\".",
    );
}

#[test]
fn lone_anonymous_operation() {
    assert_error(
        "{ hello } query A { hello }",
        "This anonymous operation must be the only defined operation.",
    );
    assert_valid("{ hello }");
}

#[test]
fn single_field_subscriptions() {
    assert_error(
        "subscription S { userCreated { id } counter }",
        "must select only one top-level field.",
    );
    assert_error(
        "subscription S { __typename }",
        "must not select an introspection top-level field.",
    );
    assert_error(
        "subscription S { ...F } fragment F on Subscription { userCreated { id } counter }",
        "must select only one top-level field.",
    );
    assert_valid("subscription S { userCreated { id } }");
    // The same fragment spread twice must not be counted twice.
    assert_valid("subscription S { ...F ...F } fragment F on Subscription { userCreated { id } }");
}

#[test]
fn known_type_names() {
    assert_error(
        "query Q($x: Missing) { hello }",
        "Unknown type \"Missing\".",
    );
    assert_error(
        "fragment F on Missing { hello }",
        "Unknown type \"Missing\".",
    );
    assert_error(
        "{ hello ... on Missing { hello } }",
        "Unknown type \"Missing\".",
    );
}

#[test]
fn fragments_on_composite_types() {
    assert_error(
        "fragment F on String { hello }",
        "Fragment \"F\" cannot condition on non composite type \"String\".",
    );
    assert_error(
        "{ hello ... on String { hello } }",
        "Fragment cannot condition on non composite type \"String\".",
    );
}

#[test]
fn variables_are_input_types() {
    assert_error(
        "query Q($user: User) { hello }",
        "Variable \"$user\" cannot be non-input type \"User\".",
    );
    assert_valid("query Q($filter: FilterInput) { users(filter: $filter) { id } }");
}

#[test]
fn scalar_leafs() {
    assert_error(
        "{ hello { x } }",
        "must not have a selection since type \"String\" has no subfields.",
    );
    assert_error("{ user(id: \"1\") }", "must have a selection of subfields.");
    assert_error(
        "{ episode { x } }",
        "Field \"episode\" must not have a selection",
    );
    assert_error(
        "{ __typename { x } }",
        "Field \"__typename\" must not have a selection",
    );
    assert_error(
        "{ __schema }",
        "Field \"__schema\" of type \"__Schema!\" must have a selection of subfields.",
    );
    assert_error(
        "{ __type(name: \"Query\") }",
        "must have a selection of subfields.",
    );
}

#[test]
fn fields_on_correct_type() {
    assert_error("{ nope }", "Cannot query field \"nope\" on type \"Query\".");
    assert_error(
        "{ search(term: \"x\") { name } }",
        "Cannot query field \"name\" on type \"SearchResult\".",
    );
    assert_error(
        "{ hello { nested } }",
        // The selection is invalid as a whole, but the inner field is still reported against the
        // scalar's (unknown) parent type; ScalarLeafs is the primary error.
        "must not have a selection",
    );
}

#[test]
fn fragment_names_are_unique() {
    assert_error(
        "{ ...F } fragment F on Query { hello } fragment F on Query { hello }",
        "There can be only one fragment named \"F\".",
    );
}

#[test]
fn known_fragment_names() {
    assert_error("{ ...Missing }", "Unknown fragment \"Missing\".");
}

#[test]
fn no_unused_fragments() {
    assert_error(
        "{ hello } fragment F on Query { hello }",
        "Fragment \"F\" is never used.",
    );
    assert_valid("query Q { ...A } fragment A on Query { ...B } fragment B on Query { hello }");
}

#[test]
fn no_fragment_cycles() {
    assert_error(
        "{ ...A } fragment A on Query { ...B } fragment B on Query { ...A }",
        "Cannot spread fragment \"A\" within itself via \"B\".",
    );
    assert_error(
        "{ ...A } fragment A on Query { ...A }",
        "Cannot spread fragment \"A\" within itself.",
    );
}

#[test]
fn no_undefined_variables() {
    assert_error(
        "query Q { ...F } fragment F on Query { hello @trace(label: $x) }",
        "Variable \"$x\" is not defined by operation \"Q\".",
    );
}

#[test]
fn no_unused_variables() {
    assert_error(
        "query Q($x: Int) { hello }",
        "Variable \"$x\" is never used in operation \"Q\".",
    );
}

#[test]
fn unique_variable_names() {
    assert_error(
        "query Q($x: Int, $x: Int) { hello }",
        "There can be only one variable named \"$x\".",
    );
}

#[test]
fn variables_used_through_fragments_are_defined() {
    assert_valid("query Q($id: ID!) { ...F } fragment F on Query { user(id: $id) { id } }");
}

#[test]
fn possible_fragment_spreads() {
    assert_error(
        "{ search(term: \"x\") { ... on Human { ... on Droid { id } } } }",
        "can never be of type \"Droid\".",
    );
    assert_error(
        "{ search(term: \"x\") { ... on Human { ...DroidFields } } } fragment DroidFields on Droid { id }",
        "Fragment \"DroidFields\" cannot be spread here as objects of type \"Human\" can never be of type \"Droid\".",
    );
    assert_valid(
        "{ character(id: \"1\") { ...DroidFields } } fragment DroidFields on Droid { id }",
    );
}

#[test]
fn known_directives() {
    assert_error("{ hello @nope }", "Unknown directive \"@nope\".");
    assert_error(
        "query Q @fieldOnly { hello }",
        "Directive \"@fieldOnly\" may not be used on QUERY.",
    );
    assert_error(
        "query Q($x: Int @fieldOnly) { hello }",
        "Directive \"@fieldOnly\" may not be used on VARIABLE_DEFINITION.",
    );
    assert_error(
        "fragment F on Query @fieldOnly { hello }",
        "Directive \"@fieldOnly\" may not be used on FRAGMENT_DEFINITION.",
    );
    assert_error(
        "{ hello @include(if: true) @include(if: true) }",
        "can only be used once at this location.",
    );
}

#[test]
fn repeatable_directives_may_repeat() {
    assert_valid("{ hello @repeated @repeated }");
}

#[test]
fn known_argument_names() {
    assert_error(
        "{ user(foo: 1) { id } }",
        "Unknown argument \"foo\" on field \"Query.user\".",
    );
    assert_error(
        "{ hello @trace(bad: \"x\") }",
        "Unknown argument \"bad\" on directive \"@trace\".",
    );
    assert_error(
        "{ __schema(bad: 1) { queryType { name } } }",
        "Unknown argument \"bad\" on field \"Query.__schema\".",
    );
    assert_error(
        "{ __typename(foo: 1) }",
        "Unknown argument \"foo\" on field \"Query.__typename\".",
    );
}

#[test]
fn unique_argument_names() {
    assert_error(
        "{ user(id: \"1\", id: \"2\") { id } }",
        "There can be only one argument named \"id\".",
    );
    assert_error(
        "{ hello @trace(label: \"a\", label: \"b\") }",
        "There can be only one argument named \"label\".",
    );
}

#[test]
fn provided_required_arguments() {
    assert_error(
        "{ user { id } }",
        "Field \"Query.user\" argument \"id\" of type \"ID!\" is required, but it was not provided.",
    );
    assert_error(
        "{ hello @skip }",
        "Directive \"@skip\" argument \"if\" of type \"Boolean!\" is required, but it was not provided.",
    );
    assert_error(
        "{ __type { name } }",
        "Field \"Query.__type\" argument \"name\" of type \"String!\" is required, but it was not provided.",
    );
    assert_error(
        "{ hello @tag }",
        "Directive \"@tag\" argument \"name\" of type \"String!\" is required, but it was not provided.",
    );
}

#[test]
fn values_of_correct_type() {
    assert_error(
        "{ user(id: null) { id } }",
        "Expected value of type \"ID!\", found null.",
    );
    assert_error(
        "{ users(first: \"x\") { id } }",
        "Int cannot represent a non-integer literal",
    );
    assert_error(
        "{ users(first: 2147483648) { id } }",
        "Int cannot represent non 32-bit signed integer value",
    );
    assert_error(
        "{ users(filter: { limit: \"x\" }) { id } }",
        "Int cannot represent a non-integer literal",
    );
    assert_error(
        "{ search(term: \"x\", episode: BAD) { __typename } }",
        "Value \"BAD\" does not exist in \"Episode\" enum.",
    );
    assert_error(
        "{ search(term: \"x\", episode: \"JEDI\") { __typename } }",
        "Expected value of type \"Episode\", found \"JEDI\".",
    );
    assert_error(
        "{ users(filter: { limit: 1, nope: 2 }) { id } }",
        "Field \"nope\" is not defined by type \"FilterInput\".",
    );
    assert_error(
        "{ users(filter: {}) { id } }",
        "Field \"FilterInput.limit\" of required type \"Int!\" was not provided.",
    );
    assert_error(
        "{ at(time: 5) }",
        "DateTime cannot represent a non-string literal",
    );
    assert_error(
        "{ hello @trace(label: 5) }",
        "String cannot represent a non-string literal",
    );
    assert_error(
        "query Q($episode: Episode = BAD) { search(term: \"x\", episode: $episode) { __typename } }",
        "Value \"BAD\" does not exist in \"Episode\" enum.",
    );
    assert_valid("{ at(time: \"2020-01-01\") }");
    assert_valid("{ user(id: 3) { id } }");
    assert_valid("{ users(filter: { limit: 1, offset: 3 }) { id } }");
}

#[test]
fn unique_input_field_names() {
    assert_error(
        "{ users(filter: { limit: 1, limit: 2 }) { id } }",
        "There can be only one input field named \"limit\".",
    );
}

#[test]
fn variables_in_allowed_position() {
    assert_error(
        "query Q($id: ID) { user(id: $id) { id } }",
        "Variable \"$id\" of type \"ID\" used in position expecting type \"ID!\".",
    );
    assert_error(
        "query Q($filter: String) { users(filter: $filter) { id } }",
        "used in position expecting type \"FilterInput\".",
    );
    assert_error(
        "query Q($values: [Int]) { sum(values: $values) }",
        "Variable \"$values\" of type \"[Int]\" used in position expecting type \"[Int!]!\".",
    );

    // A non-null default on the variable makes the nullable declaration acceptable.
    assert_valid("query Q($id: ID = \"1\") { user(id: $id) { id } }");
    // A default on the argument location does the same.
    assert_valid("query Q($count: Int) { paginated(count: $count) { id } }");
    // Non-null variables are accepted in nullable positions.
    assert_valid("query Q($filter: FilterInput!) { users(filter: $filter) { id } }");
    assert_valid("query Q($values: [Int!]!) { sum(values: $values) }");
    assert_valid("query Q($x: Int!) { sum(values: [$x]) }");

    assert_error(
        "query Q($flag: Boolean) { hello @include(if: $flag) }",
        "Variable \"$flag\" of type \"Boolean\" used in position expecting type \"Boolean!\".",
    );
    assert_valid("query Q($flag: Boolean!) { hello @include(if: $flag) }");
}

#[test]
fn overlapping_fields() {
    assert_error(
        "{ x: hello x: episode }",
        "Fields \"x\" conflict because they have differing field names (\"hello\" and \"episode\").",
    );
    assert_error(
        "{ user(id: \"1\") { id } user(id: \"2\") { id } }",
        "Fields \"user\" conflict because they have differing arguments.",
    );
    assert_error(
        "{ readable { ... on Readable { value } ... on Countable { value } } }",
        "Fields \"value\" conflict because they return conflicting types \"String!\" and \"String\".",
    );
    assert_error(
        "{ user(id: \"1\") { name } user(id: \"1\") { name: id } }",
        "Fields \"name\" conflict because they have differing field names (\"name\" and \"id\").",
    );

    assert_valid("{ user(id: \"1\") { name } user(id: \"1\") { name } }");
    assert_valid("{ user(id: \"1\") { id } user(id: \"1\") { name } }");
    // Identical fields under different aliases are distinct response keys.
    assert_valid("{ a: hello b: hello }");
    // Input object argument values compare regardless of field order.
    assert_valid(
        "{ echo(input: { stars: 1, commentary: \"x\" }) echo(input: { commentary: \"x\", stars: 1 }) }",
    );
    assert_valid("query Q($id: ID!) { user(id: $id) { id } user(id: $id) { name } }");
    // Mutually exclusive type conditions are allowed to select different fields.
    assert_valid("{ search(term: \"x\") { ... on Human { name } ... on Droid { name } } }");
    assert_valid("{ character(id: \"1\") { ...CF id } } fragment CF on Character { id }");
}

#[test]
fn meta_fields_respect_introspection_option() {
    assert_valid("{ __typename }");
    assert_valid("{ __schema { queryType { name } } }");
    assert_valid("{ __type(name: \"Query\") { name } }");
    assert_valid("{ user(id: \"1\") { __typename } }");
    assert_error(
        "{ user(id: \"1\") { __schema { queryType { name } } } }",
        "Cannot query field \"__schema\" on type \"User\".",
    );
    assert_error(
        "{ user(id: \"1\") { __type(name: \"User\") { name } } }",
        "Cannot query field \"__type\" on type \"User\".",
    );

    let typename = validate_with("{ __typename }", false);
    assert!(typename.is_empty(), "{:?}", messages(&typename));

    let schema_field = validate_with("{ __schema { queryType { name } } }", false);
    assert!(
        schema_field
            .iter()
            .any(|error| error.message().contains("Cannot query field \"__schema\"")),
        "{:?}",
        messages(&schema_field)
    );

    let type_field = validate_with("{ __type(name: \"Query\") { name } }", false);
    assert!(
        type_field
            .iter()
            .any(|error| error.message().contains("Cannot query field \"__type\"")),
        "{:?}",
        messages(&type_field)
    );
}

#[test]
fn variables_in_default_values_are_rejected() {
    use gql::ast::{Definition, Document, OperationDefinition, OperationKind, Type, Value};

    let document = Document {
        definitions: vec![Definition::Operation(OperationDefinition {
            kind: OperationKind::Query,
            name: Some("Q"),
            variables: vec![
                gql::ast::VariableDefinition {
                    name: "x",
                    ty: Type::Named("Int"),
                    default_value: Some(Value::Variable("y")),
                    directives: Vec::new(),
                    location: Location::new(1, 1),
                },
                gql::ast::VariableDefinition {
                    name: "y",
                    ty: Type::Named("Int"),
                    default_value: None,
                    directives: Vec::new(),
                    location: Location::new(1, 12),
                },
            ],
            directives: Vec::new(),
            selection_set: SelectionSet {
                items: vec![Selection::Field(AstField {
                    alias: None,
                    name: "hello",
                    arguments: Vec::new(),
                    directives: Vec::new(),
                    selection_set: None,
                    location: Location::new(1, 24),
                })],
                location: Location::new(1, 23),
            },
            location: Location::new(1, 1),
        })],
    };

    let schema = schema();
    let errors = validate(
        &schema,
        &document,
        &ValidationOptions {
            introspection: true,
            ..Default::default()
        },
    );
    assert!(
        errors.iter().any(|error| error
            .message()
            .contains("cannot be used in a default value")),
        "{:?}",
        messages(&errors)
    );
}

#[test]
fn errors_have_locations() {
    let field_errors = errors("{ nope }");
    assert!(
        field_errors
            .iter()
            .all(|error| !error.locations().is_empty()),
        "every validation error should carry a source location"
    );

    let overlap_errors = errors("{ x: hello x: episode }");
    let conflict = overlap_errors
        .iter()
        .find(|error| error.message().contains("Fields \"x\" conflict"))
        .expect("conflicting fields error");
    assert_eq!(
        conflict.locations().len(),
        2,
        "overlap errors should point at both fields"
    );
}
