//! Introspection tests.
//!
//! Most tests drive the introspection definitions directly: the executor is being implemented in
//! a parallel change, so looking up `__Schema`/`__Type` in the schema and calling their resolvers
//! exercises exactly the code introspection ships without waiting for the integration. The
//! descriptors built here use the documented internal `__gql_` protocol.

use std::sync::Arc;

use gql::{
    DirectiveDefinition, EnumType, ExecutionContext, Field, InputObjectType, InputValue,
    InterfaceType, Object, ObjectType, ResolveInfo, ResolverContext, ScalarType, Schema, Type,
    TypeRef, UnionType, Value, ast::DirectiveLocation,
};

const TYPENAME: &str = "__gql_typename";
const NAME: &str = "__gql_name";
const PARENT: &str = "__gql_parent";
const FIELD: &str = "__gql_field";
const OWNER: &str = "__gql_owner";
const KIND: &str = "__gql_kind";
const OF: &str = "__gql_of";

fn schema() -> Schema {
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
            .field(InputValue::new("term", TypeRef::string()).default_value("all"))
            .field(
                InputValue::new("tags", TypeRef::string().non_null().list())
                    .default_value(Value::List(vec![Value::from("a"), Value::from("b")])),
            )
            .field(InputValue::new("legacy", TypeRef::boolean()).deprecation_reason("Use term."))
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
            .field(Field::new("color", TypeRef::named("Color")))
            .field(
                Field::new(
                    "friends",
                    TypeRef::named("User").non_null().list().non_null(),
                )
                .arg(InputValue::new("first", TypeRef::int()).default_value(10)),
            )
            .field(Field::new("oldField", TypeRef::string()).deprecation_reason("Use name."))
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
                Field::new("users", TypeRef::named("User").non_null().list().non_null())
                    .arg(
                        InputValue::new("filter", TypeRef::named("Filter"))
                            .description("Filter the users."),
                    )
                    .arg(InputValue::new("limit", TypeRef::int()).default_value(20))
                    .arg(
                        InputValue::new("old", TypeRef::boolean())
                            .deprecation_reason("Use filter."),
                    ),
            )
            .field(Field::new(
                "search",
                TypeRef::named("SearchResult").non_null().list().non_null(),
            ))
            .into(),
    );

    builder.add_directive(
        DirectiveDefinition::new("auth")
            .description("Requires a role.")
            .arg(
                InputValue::new("role", TypeRef::string().non_null())
                    .description("The required role."),
            )
            .arg(InputValue::new("legacy", TypeRef::boolean()).deprecation_reason("Use role."))
            .location(DirectiveLocation::FieldDefinition)
            .location(DirectiveLocation::Object)
            .repeatable(),
    );

    builder.add_directive(
        DirectiveDefinition::new("legacyDirective")
            .description("An old directive.")
            .arg(InputValue::new("value", TypeRef::string()))
            .location(DirectiveLocation::FieldDefinition)
            .deprecation_reason("Use auth."),
    );

    builder.query("Query").build().expect("valid schema")
}

fn descriptor(typename: &str, pairs: &[(&str, Value)]) -> Value {
    let mut object = Object::new();
    object.insert(TYPENAME, Value::from(typename));
    for (key, value) in pairs {
        object.insert(*key, value.clone());
    }
    Value::Object(Arc::new(object))
}

fn type_desc(name: &str) -> Value {
    descriptor("__Type", &[(NAME, Value::from(name))])
}

fn list_of(inner: Value) -> Value {
    descriptor("__Type", &[(KIND, Value::from("LIST")), (OF, inner)])
}

fn non_null_of(inner: Value) -> Value {
    descriptor("__Type", &[(KIND, Value::from("NON_NULL")), (OF, inner)])
}

fn field_desc(parent: &str, name: &str) -> Value {
    descriptor(
        "__Field",
        &[(PARENT, Value::from(parent)), (NAME, Value::from(name))],
    )
}

fn input_desc(owner: &str, parent: &str, field: Option<&str>, name: &str) -> Value {
    let mut pairs = vec![
        (OWNER, Value::from(owner)),
        (PARENT, Value::from(parent)),
        (NAME, Value::from(name)),
    ];
    if let Some(field) = field {
        pairs.push((FIELD, Value::from(field)));
    }
    descriptor("__InputValue", &pairs)
}

fn enum_value_desc(parent: &str, name: &str) -> Value {
    descriptor(
        "__EnumValue",
        &[(PARENT, Value::from(parent)), (NAME, Value::from(name))],
    )
}

fn directive_desc(name: &str) -> Value {
    descriptor("__Directive", &[(NAME, Value::from(name))])
}

fn schema_desc() -> Value {
    descriptor("__Schema", &[])
}

fn context(schema: &Schema) -> ExecutionContext {
    ExecutionContext {
        schema: schema.clone(),
        user_data: Arc::new(()),
        operation_name: None,
        variables: Object::new(),
        introspection: true,
        authorizer: None,
        max_errors: 100,
        max_concurrency: 16,
        extensions: std::sync::Mutex::new(None),
    }
}

fn call(
    schema: &Schema,
    context: &ExecutionContext,
    parent_type: &str,
    field_name: &str,
    parent: &Value,
    args: Object,
) -> Value {
    let ty = schema.get_type(parent_type).expect("parent type exists");
    let fields = match ty {
        Type::Object(object) => &object.fields,
        Type::Interface(interface) => &interface.fields,
        _ => panic!("{parent_type} is not a composite type"),
    };
    let definition = fields.get(field_name).expect("field definition exists");
    let info = ResolveInfo {
        field_name: definition.name.as_ref(),
        parent_type,
        return_type: &definition.ty,
        path: &[],
        schema,
    };
    let resolver = definition.resolve.as_ref().expect("field has a resolver");
    let resolver_context = ResolverContext {
        parent,
        args: &args,
        context,
        info: &info,
    };
    futures::executor::block_on(resolver(resolver_context)).expect("resolver succeeds")
}

fn field(
    schema: &Schema,
    context: &ExecutionContext,
    parent_type: &str,
    name: &str,
    parent: &Value,
) -> Value {
    call(schema, context, parent_type, name, parent, Object::new())
}

fn with_args(name: &str, value: Value) -> Object {
    Object::from_fields([(name, value)])
}

fn object_get<'a>(value: &'a Value, key: &str) -> &'a Value {
    value
        .as_object()
        .expect("object")
        .get(key)
        .expect("key exists")
}

/// Read the field names out of a list of introspection descriptors.
fn names(value: &Value) -> Vec<String> {
    value
        .as_list()
        .expect("list")
        .iter()
        .map(|item| {
            object_get(item, NAME)
                .as_str()
                .expect("name is a string")
                .to_owned()
        })
        .collect()
}

/// Read the field names out of a list of executed response objects.
fn response_names(value: &Value) -> Vec<String> {
    value
        .as_list()
        .expect("list")
        .iter()
        .map(|item| {
            object_get(item, "name")
                .as_str()
                .expect("name is a string")
                .to_owned()
        })
        .collect()
}

#[test]
fn type_kinds_and_wrappers() {
    let schema = schema();
    let context = context(&schema);

    let kind = |parent: &Value| field(&schema, &context, "__Type", "kind", parent);
    let name = |parent: &Value| field(&schema, &context, "__Type", "name", parent);

    assert_eq!(kind(&type_desc("User")), Value::enumeration("OBJECT"));
    assert_eq!(kind(&type_desc("Node")), Value::enumeration("INTERFACE"));
    assert_eq!(
        kind(&type_desc("SearchResult")),
        Value::enumeration("UNION")
    );
    assert_eq!(kind(&type_desc("Color")), Value::enumeration("ENUM"));
    assert_eq!(
        kind(&type_desc("Filter")),
        Value::enumeration("INPUT_OBJECT")
    );
    assert_eq!(kind(&type_desc("Date")), Value::enumeration("SCALAR"));

    assert_eq!(name(&type_desc("User")), Value::from("User"));
    assert_eq!(name(&list_of(type_desc("User"))), Value::Null);

    assert_eq!(
        kind(&list_of(type_desc("User"))),
        Value::enumeration("LIST")
    );
    assert_eq!(
        kind(&non_null_of(type_desc("User"))),
        Value::enumeration("NON_NULL")
    );

    let wrapped = non_null_of(list_of(non_null_of(type_desc("User"))));
    let of_type = field(&schema, &context, "__Type", "ofType", &wrapped);
    assert_eq!(kind(&of_type), Value::enumeration("LIST"));
    let of_type = field(&schema, &context, "__Type", "ofType", &of_type);
    assert_eq!(kind(&of_type), Value::enumeration("NON_NULL"));
    let of_type = field(&schema, &context, "__Type", "ofType", &of_type);
    assert_eq!(kind(&of_type), Value::enumeration("OBJECT"));
    assert_eq!(name(&of_type), Value::from("User"));
    assert_eq!(
        field(&schema, &context, "__Type", "ofType", &of_type),
        Value::Null
    );
}

#[test]
fn type_descriptions_and_specified_by() {
    let schema = schema();
    let context = context(&schema);

    assert_eq!(
        field(
            &schema,
            &context,
            "__Type",
            "description",
            &type_desc("User")
        ),
        Value::from("A user.")
    );
    assert_eq!(
        field(
            &schema,
            &context,
            "__Type",
            "description",
            &type_desc("Query")
        ),
        Value::Null
    );
    assert_eq!(
        field(
            &schema,
            &context,
            "__Type",
            "specifiedByURL",
            &type_desc("Date")
        ),
        Value::from("https://example.com/date")
    );
    assert_eq!(
        field(
            &schema,
            &context,
            "__Type",
            "specifiedByURL",
            &type_desc("User")
        ),
        Value::Null
    );
}

#[test]
fn type_fields_honour_deprecation() {
    let schema = schema();
    let context = context(&schema);
    let user = type_desc("User");

    let visible = field(&schema, &context, "__Type", "fields", &user);
    assert_eq!(names(&visible), vec!["id", "name", "color", "friends"]);

    let all = call(
        &schema,
        &context,
        "__Type",
        "fields",
        &user,
        with_args("includeDeprecated", Value::from(true)),
    );
    assert_eq!(
        names(&all),
        vec!["id", "name", "color", "friends", "oldField"]
    );

    // Only object and interface types expose fields.
    assert_eq!(
        field(&schema, &context, "__Type", "fields", &type_desc("Date")),
        Value::Null
    );
    assert_eq!(
        field(&schema, &context, "__Type", "fields", &list_of(user)),
        Value::Null
    );

    let deprecated = field_desc("User", "oldField");
    assert_eq!(
        field(&schema, &context, "__Field", "isDeprecated", &deprecated),
        Value::Boolean(true)
    );
    assert_eq!(
        field(
            &schema,
            &context,
            "__Field",
            "deprecationReason",
            &deprecated
        ),
        Value::from("Use name.")
    );
    assert_eq!(
        field(
            &schema,
            &context,
            "__Field",
            "deprecationReason",
            &field_desc("User", "name")
        ),
        Value::Null
    );
    assert_eq!(
        field(
            &schema,
            &context,
            "__Field",
            "description",
            &field_desc("User", "name")
        ),
        Value::from("The display name.")
    );
}

#[test]
fn field_arguments_and_default_values() {
    let schema = schema();
    let context = context(&schema);
    let users = field_desc("Query", "users");

    let visible = field(&schema, &context, "__Field", "args", &users);
    assert_eq!(names(&visible), vec!["filter", "limit"]);

    let all = call(
        &schema,
        &context,
        "__Field",
        "args",
        &users,
        with_args("includeDeprecated", Value::from(true)),
    );
    assert_eq!(names(&all), vec!["filter", "limit", "old"]);

    // `limit: Int = 20` is printed as a GraphQL literal.
    let limit = input_desc("field", "Query", Some("users"), "limit");
    assert_eq!(
        field(&schema, &context, "__InputValue", "defaultValue", &limit),
        Value::from("20")
    );
    let limit_type = field(&schema, &context, "__InputValue", "type", &limit);
    assert_eq!(
        field(&schema, &context, "__Type", "name", &limit_type),
        Value::from("Int")
    );

    let filter = input_desc("field", "Query", Some("users"), "filter");
    assert_eq!(
        field(&schema, &context, "__InputValue", "description", &filter),
        Value::from("Filter the users.")
    );
    assert_eq!(
        field(&schema, &context, "__InputValue", "defaultValue", &filter),
        Value::Null
    );
    let filter_type = field(&schema, &context, "__InputValue", "type", &filter);
    assert_eq!(
        field(&schema, &context, "__Type", "name", &filter_type),
        Value::from("Filter")
    );

    let old = input_desc("field", "Query", Some("users"), "old");
    assert_eq!(
        field(&schema, &context, "__InputValue", "isDeprecated", &old),
        Value::Boolean(true)
    );
    assert_eq!(
        field(&schema, &context, "__InputValue", "deprecationReason", &old),
        Value::from("Use filter.")
    );

    // The field's return type is exposed as a full type reference.
    let return_type = field(&schema, &context, "__Field", "type", &users);
    let non_null = field(&schema, &context, "__Type", "ofType", &return_type);
    let list = field(&schema, &context, "__Type", "ofType", &non_null);
    let item = field(&schema, &context, "__Type", "ofType", &list);
    assert_eq!(
        field(&schema, &context, "__Type", "name", &item),
        Value::from("User")
    );
}

#[test]
fn input_objects_enums_and_one_of() {
    let schema = schema();
    let context = context(&schema);

    let filter = type_desc("Filter");
    let visible = field(&schema, &context, "__Type", "inputFields", &filter);
    assert_eq!(names(&visible), vec!["limit", "term", "tags"]);

    let all = call(
        &schema,
        &context,
        "__Type",
        "inputFields",
        &filter,
        with_args("includeDeprecated", Value::from(true)),
    );
    assert_eq!(names(&all), vec!["limit", "term", "tags", "legacy"]);

    let term = input_desc("inputObject", "Filter", None, "term");
    assert_eq!(
        field(&schema, &context, "__InputValue", "defaultValue", &term),
        Value::from("\"all\"")
    );
    let tags = input_desc("inputObject", "Filter", None, "tags");
    assert_eq!(
        field(&schema, &context, "__InputValue", "defaultValue", &tags),
        Value::from("[\"a\", \"b\"]")
    );

    assert_eq!(
        field(&schema, &context, "__Type", "isOneOf", &filter),
        Value::Boolean(false)
    );
    assert_eq!(
        field(&schema, &context, "__Type", "isOneOf", &type_desc("Lookup")),
        Value::Boolean(true)
    );
    // `isOneOf` is only defined for input objects; every other kind returns null.
    for non_input in ["User", "Date", "Node", "SearchResult", "Color"] {
        assert_eq!(
            field(
                &schema,
                &context,
                "__Type",
                "isOneOf",
                &type_desc(non_input)
            ),
            Value::Null,
            "isOneOf should be null for {non_input}"
        );
    }
    assert_eq!(
        field(
            &schema,
            &context,
            "__Type",
            "isOneOf",
            &list_of(type_desc("Lookup"))
        ),
        Value::Null
    );

    let color = type_desc("Color");
    let visible = field(&schema, &context, "__Type", "enumValues", &color);
    assert_eq!(names(&visible), vec!["RED", "GREEN"]);
    let all = call(
        &schema,
        &context,
        "__Type",
        "enumValues",
        &color,
        with_args("includeDeprecated", Value::from(true)),
    );
    assert_eq!(names(&all), vec!["RED", "GREEN", "BLUE"]);

    let blue = enum_value_desc("Color", "BLUE");
    assert_eq!(
        field(&schema, &context, "__EnumValue", "isDeprecated", &blue),
        Value::Boolean(true)
    );
    assert_eq!(
        field(&schema, &context, "__EnumValue", "deprecationReason", &blue),
        Value::from("Use RED.")
    );
    assert_eq!(
        field(
            &schema,
            &context,
            "__EnumValue",
            "description",
            &enum_value_desc("Color", "RED")
        ),
        Value::from("The colour red.")
    );
}

#[test]
fn interfaces_and_possible_types() {
    let schema = schema();
    let context = context(&schema);

    let user_interfaces = field(
        &schema,
        &context,
        "__Type",
        "interfaces",
        &type_desc("User"),
    );
    assert_eq!(names(&user_interfaces), vec!["Node"]);

    let interface_possibilities = field(
        &schema,
        &context,
        "__Type",
        "possibleTypes",
        &type_desc("Node"),
    );
    assert_eq!(names(&interface_possibilities), vec!["User", "Post"]);

    let union_possibilities = field(
        &schema,
        &context,
        "__Type",
        "possibleTypes",
        &type_desc("SearchResult"),
    );
    assert_eq!(names(&union_possibilities), vec!["User", "Post"]);

    assert_eq!(
        field(
            &schema,
            &context,
            "__Type",
            "possibleTypes",
            &type_desc("User")
        ),
        Value::Null
    );
    assert_eq!(
        field(
            &schema,
            &context,
            "__Type",
            "interfaces",
            &type_desc("Date")
        ),
        Value::Null
    );
}

#[test]
fn schema_shape() {
    let schema = schema();
    let context = context(&schema);
    let schema_value = schema_desc();

    assert_eq!(
        field(&schema, &context, "__Schema", "description", &schema_value),
        Value::Null
    );

    let types = field(&schema, &context, "__Schema", "types", &schema_value);
    let type_names = names(&types);
    for expected in [
        "Query",
        "User",
        "Post",
        "Node",
        "SearchResult",
        "Color",
        "Filter",
        "Lookup",
        "Date",
        "String",
        "__Schema",
        "__Type",
        "__TypeKind",
        "__Field",
        "__InputValue",
        "__EnumValue",
        "__Directive",
        "__DirectiveLocation",
    ] {
        assert!(
            type_names.contains(&expected.to_owned()),
            "missing {expected}"
        );
    }
    // Schema order: the referenced built-in scalars come first, in canonical order. `Float` is
    // not referenced by this schema, so — like graphql-js — it is absent from introspection.
    assert_eq!(
        type_names[..4]
            .iter()
            .map(String::as_str)
            .collect::<Vec<_>>(),
        vec!["String", "Int", "Boolean", "ID"]
    );
    assert!(!type_names.iter().any(|name| name == "Float"));

    let query_type = field(&schema, &context, "__Schema", "queryType", &schema_value);
    assert_eq!(
        field(&schema, &context, "__Type", "name", &query_type),
        Value::from("Query")
    );
    assert_eq!(
        field(&schema, &context, "__Schema", "mutationType", &schema_value),
        Value::Null
    );
    assert_eq!(
        field(
            &schema,
            &context,
            "__Schema",
            "subscriptionType",
            &schema_value
        ),
        Value::Null
    );

    let directives = field(&schema, &context, "__Schema", "directives", &schema_value);
    assert_eq!(
        names(&directives),
        vec![
            "skip",
            "include",
            "deprecated",
            "specifiedBy",
            "oneOf",
            "auth"
        ]
    );
}

#[test]
fn directive_metadata() {
    let schema = schema();
    let context = context(&schema);
    let auth = directive_desc("auth");

    assert_eq!(
        field(&schema, &context, "__Directive", "name", &auth),
        Value::from("auth")
    );
    assert_eq!(
        field(&schema, &context, "__Directive", "description", &auth),
        Value::from("Requires a role.")
    );

    let locations = field(&schema, &context, "__Directive", "locations", &auth);
    assert_eq!(
        locations,
        Value::List(vec![
            Value::enumeration("FIELD_DEFINITION"),
            Value::enumeration("OBJECT")
        ])
    );

    assert_eq!(
        field(&schema, &context, "__Directive", "isRepeatable", &auth),
        Value::Boolean(true)
    );

    let visible = field(&schema, &context, "__Directive", "args", &auth);
    assert_eq!(names(&visible), vec!["role"]);
    let all = call(
        &schema,
        &context,
        "__Directive",
        "args",
        &auth,
        with_args("includeDeprecated", Value::from(true)),
    );
    assert_eq!(names(&all), vec!["role", "legacy"]);

    let role = input_desc("directive", "auth", None, "role");
    assert_eq!(
        field(&schema, &context, "__InputValue", "description", &role),
        Value::from("The required role.")
    );
    let role_type = field(&schema, &context, "__InputValue", "type", &role);
    assert_eq!(
        field(&schema, &context, "__Type", "kind", &role_type),
        Value::enumeration("NON_NULL")
    );
}

#[test]
fn directive_deprecation_and_introspection_descriptions() {
    let schema = schema();
    let context = context(&schema);
    let schema_value = schema_desc();

    // Deprecated directives are hidden unless explicitly requested.
    let visible = field(&schema, &context, "__Schema", "directives", &schema_value);
    assert_eq!(
        names(&visible),
        vec![
            "skip",
            "include",
            "deprecated",
            "specifiedBy",
            "oneOf",
            "auth"
        ]
    );
    let all = call(
        &schema,
        &context,
        "__Schema",
        "directives",
        &schema_value,
        with_args("includeDeprecated", Value::from(true)),
    );
    assert_eq!(
        names(&all),
        vec![
            "skip",
            "include",
            "deprecated",
            "specifiedBy",
            "oneOf",
            "auth",
            "legacyDirective"
        ]
    );

    let legacy = directive_desc("legacyDirective");
    assert_eq!(
        field(&schema, &context, "__Directive", "isDeprecated", &legacy),
        Value::Boolean(true)
    );
    assert_eq!(
        field(
            &schema,
            &context,
            "__Directive",
            "deprecationReason",
            &legacy
        ),
        Value::from("Use auth.")
    );
    let auth = directive_desc("auth");
    assert_eq!(
        field(&schema, &context, "__Directive", "isDeprecated", &auth),
        Value::Boolean(false)
    );
    assert_eq!(
        field(&schema, &context, "__Directive", "deprecationReason", &auth),
        Value::Null
    );

    // The exact `__Directive` shape modern clients expect.
    let directive_fields = field(
        &schema,
        &context,
        "__Type",
        "fields",
        &type_desc("__Directive"),
    );
    assert_eq!(
        names(&directive_fields),
        vec![
            "name",
            "description",
            "isRepeatable",
            "locations",
            "args",
            "isDeprecated",
            "deprecationReason"
        ]
    );

    let directive_argument_names = field(
        &schema,
        &context,
        "__Field",
        "args",
        &field_desc("__Schema", "directives"),
    );
    assert_eq!(names(&directive_argument_names), vec!["includeDeprecated"]);

    // The twentieth directive location and the canonical value descriptions.
    let locations = call(
        &schema,
        &context,
        "__Type",
        "enumValues",
        &type_desc("__DirectiveLocation"),
        with_args("includeDeprecated", Value::from(true)),
    );
    let location_names = names(&locations);
    assert_eq!(location_names.len(), 20);
    assert_eq!(
        location_names.last().map(String::as_str),
        Some("DIRECTIVE_DEFINITION")
    );
    assert_eq!(
        field(
            &schema,
            &context,
            "__EnumValue",
            "description",
            &enum_value_desc("__DirectiveLocation", "DIRECTIVE_DEFINITION")
        ),
        Value::from("Location adjacent to a directive definition.")
    );
    assert_eq!(
        field(
            &schema,
            &context,
            "__EnumValue",
            "description",
            &enum_value_desc("__DirectiveLocation", "QUERY")
        ),
        Value::from("Location adjacent to a query operation.")
    );

    let kinds = call(
        &schema,
        &context,
        "__Type",
        "enumValues",
        &type_desc("__TypeKind"),
        with_args("includeDeprecated", Value::from(true)),
    );
    assert_eq!(
        names(&kinds),
        vec![
            "SCALAR",
            "OBJECT",
            "INTERFACE",
            "UNION",
            "ENUM",
            "INPUT_OBJECT",
            "LIST",
            "NON_NULL"
        ]
    );
    assert_eq!(
        field(
            &schema,
            &context,
            "__EnumValue",
            "description",
            &enum_value_desc("__TypeKind", "SCALAR")
        ),
        Value::from("Indicates this type is a scalar.")
    );
    assert_eq!(
        field(
            &schema,
            &context,
            "__EnumValue",
            "description",
            &enum_value_desc("__TypeKind", "INPUT_OBJECT")
        ),
        Value::from("Indicates this type is an input object. `inputFields` is a valid field.")
    );

    // Field and argument descriptions match graphql-js.
    assert_eq!(
        field(
            &schema,
            &context,
            "__Field",
            "description",
            &field_desc("__Schema", "types")
        ),
        Value::from("A list of all types supported by this server.")
    );
    assert_eq!(
        field(
            &schema,
            &context,
            "__Field",
            "description",
            &field_desc("__Schema", "queryType")
        ),
        Value::from("The type that query operations will be rooted at.")
    );
    assert_eq!(
        field(
            &schema,
            &context,
            "__Field",
            "description",
            &field_desc("__InputValue", "defaultValue")
        ),
        Value::from(
            "A GraphQL-formatted string representing the default value for this input value."
        )
    );
    let include_deprecated =
        input_desc("field", "__Schema", Some("directives"), "includeDeprecated");
    assert_eq!(
        field(
            &schema,
            &context,
            "__InputValue",
            "description",
            &include_deprecated
        ),
        Value::from("Include deprecated entries in the result.")
    );
}

#[test]
fn schema_descriptions_are_exposed() {
    let mut builder = Schema::builder().description("A described schema.");
    builder.add_type(
        ObjectType::new("Query")
            .field(Field::new("hello", TypeRef::string()))
            .into(),
    );
    let schema = builder.build().expect("valid schema");
    let context = context(&schema);

    assert_eq!(
        field(&schema, &context, "__Schema", "description", &schema_desc()),
        Value::from("A described schema.")
    );
}

#[test]
fn type_meta_field_resolves_names() {
    let schema = schema();
    let options = gql::ExecutionOptions::default();

    let response = futures::executor::block_on(gql::execute(
        &schema,
        gql::Request::new(
            "{ __type(name: \"User\") { kind name } missing: __type(name: \"Nope\") { name } }",
        ),
        &options,
    ));
    assert!(response.errors.is_empty(), "{:?}", response.errors);
    let data = response.data.expect("data");
    assert_eq!(
        object_get(object_get(&data, "__type"), "kind"),
        &Value::enumeration("OBJECT")
    );
    assert_eq!(
        object_get(object_get(&data, "__type"), "name"),
        &Value::from("User")
    );
    assert_eq!(object_get(&data, "missing"), &Value::Null);

    let variables = Object::from_fields([("name", Value::from("Color"))]);
    let response = futures::executor::block_on(gql::execute(
        &schema,
        gql::Request::new("query Q($name: String!) { __type(name: $name) { name kind } }")
            .variables(&variables),
        &options,
    ));
    assert!(response.errors.is_empty(), "{:?}", response.errors);
    let data = response.data.expect("data");
    assert_eq!(
        object_get(object_get(&data, "__type"), "kind"),
        &Value::enumeration("ENUM")
    );
}

#[test]
fn type_meta_field_requires_a_name() {
    let schema = schema();
    let options = gql::ExecutionOptions::default();

    let response = futures::executor::block_on(gql::execute(
        &schema,
        gql::Request::new("{ __type { name } }"),
        &options,
    ));
    assert!(response.has_errors());
    assert!(
        response
            .errors
            .iter()
            .any(|error| error.message().contains("name")),
        "{:?}",
        response.errors
    );
}

/// The standard GraphiQL introspection query.
const FULL_INTROSPECTION_QUERY: &str = r#"
query IntrospectionQuery {
  __schema {
    queryType { name }
    mutationType { name }
    subscriptionType { name }
    types { ...FullType }
    directives {
      name
      description
      isRepeatable
      locations
      args(includeDeprecated: true) { ...InputValue }
      isDeprecated
      deprecationReason
    }
  }
}

fragment FullType on __Type {
  kind
  name
  description
  specifiedByURL
  isOneOf
  fields(includeDeprecated: true) {
    name
    description
    args(includeDeprecated: true) { ...InputValue }
    type { ...TypeRef }
    isDeprecated
    deprecationReason
  }
  inputFields(includeDeprecated: true) { ...InputValue }
  interfaces { ...TypeRef }
  enumValues(includeDeprecated: true) {
    name
    description
    isDeprecated
    deprecationReason
  }
  possibleTypes { ...TypeRef }
}

fragment InputValue on __InputValue {
  name
  description
  type { ...TypeRef }
  defaultValue
}

fragment TypeRef on __Type {
  kind
  name
  ofType {
    kind
    name
    ofType {
      kind
      name
      ofType {
        kind
        name
        ofType {
          kind
          name
          ofType {
            kind
            name
          }
        }
      }
    }
  }
}
"#;

#[test]
fn full_introspection_query_executes() {
    let schema = schema();
    let options = gql::ExecutionOptions::default();

    let response = futures::executor::block_on(gql::execute(
        &schema,
        gql::Request::new(FULL_INTROSPECTION_QUERY),
        &options,
    ));
    assert!(response.errors.is_empty(), "{:?}", response.errors);

    let data = response.data.expect("data");
    let schema_data = object_get(&data, "__schema");
    assert_eq!(
        object_get(object_get(schema_data, "queryType"), "name"),
        &Value::from("Query")
    );
    assert_eq!(object_get(schema_data, "mutationType"), &Value::Null);

    let types = schema_data
        .as_object()
        .expect("object")
        .get("types")
        .and_then(Value::as_list)
        .expect("types list");

    let user = types
        .iter()
        .find(|ty| object_get(ty, "name") == &Value::from("User"))
        .expect("User is in the schema");
    assert_eq!(object_get(user, "kind"), &Value::enumeration("OBJECT"));
    assert_eq!(object_get(user, "isOneOf"), &Value::Null);
    let interfaces = response_names(object_get(user, "interfaces"));
    assert_eq!(interfaces, vec!["Node"]);

    let fields = object_get(user, "fields").as_list().expect("fields list");
    assert!(
        fields
            .iter()
            .any(|field| object_get(field, "name") == &Value::from("name"))
    );

    let date = types
        .iter()
        .find(|ty| object_get(ty, "name") == &Value::from("Date"))
        .expect("Date is in the schema");
    assert_eq!(
        object_get(date, "specifiedByURL"),
        &Value::from("https://example.com/date")
    );

    let color = types
        .iter()
        .find(|ty| object_get(ty, "name") == &Value::from("Color"))
        .expect("Color is in the schema");
    let values = response_names(object_get(color, "enumValues"));
    assert_eq!(values, vec!["RED", "GREEN", "BLUE"]);

    let directives = object_get(schema_data, "directives")
        .as_list()
        .expect("directives list");
    let auth = directives
        .iter()
        .find(|directive| object_get(directive, "name") == &Value::from("auth"))
        .expect("auth directive");
    assert_eq!(
        object_get(auth, "locations"),
        &Value::List(vec![
            Value::enumeration("FIELD_DEFINITION"),
            Value::enumeration("OBJECT")
        ])
    );
    assert_eq!(object_get(auth, "isRepeatable"), &Value::Boolean(true));
    assert_eq!(object_get(auth, "isDeprecated"), &Value::Boolean(false));
    assert_eq!(object_get(auth, "deprecationReason"), &Value::Null);

    // Deprecated directives are omitted from the default `directives` selection.
    assert!(
        !directives
            .iter()
            .any(|directive| object_get(directive, "name") == &Value::from("legacyDirective"))
    );

    // The new twentieth directive location is visible through the full query.
    let directive_location = types
        .iter()
        .find(|ty| object_get(ty, "name") == &Value::from("__DirectiveLocation"))
        .expect("__DirectiveLocation is in the schema");
    let locations = response_names(object_get(directive_location, "enumValues"));
    assert_eq!(locations.len(), 20);
    assert_eq!(
        locations.last().map(String::as_str),
        Some("DIRECTIVE_DEFINITION")
    );
}
