//! GraphQL introspection.
//!
//! Introspection is implemented with the same type system as user schemas: `__Schema`, `__Type`
//! and friends are ordinary object definitions whose resolvers read the schema and a compact
//! runtime descriptor. Keeping them in the schema means introspection queries are validated
//! exactly like any other query.
//!
//! Runtime descriptors are [`Value::Object`]s with a [`TYPENAME_KEY`] entry plus a handful of
//! private `__gql_` keys. They are never serialized directly: every introspection field has a
//! resolver which turns a descriptor into an ordinary JSON-shaped value. The `__gql_` prefix
//! makes collisions with real field names impossible.

use std::sync::Arc;

use crate::{
    Object, Value, ast,
    error::GraphQLError,
    execution::ExecutionContext,
    schema::Schema,
    types::{EnumType, EnumValue, Field, InputValue, ObjectType, Type, TypeRef},
};

/// A sentinel used by introspection resolvers to mark their runtime descriptors.
pub(crate) const TYPENAME_KEY: &str = "__gql_typename";

/// The name carried by a `__Type` descriptor for a named type.
const NAME_KEY: &str = "__gql_name";
/// The owner of a field, enum value or input value descriptor.
const PARENT_KEY: &str = "__gql_parent";
/// The field an argument descriptor belongs to.
const FIELD_KEY: &str = "__gql_field";
/// Which kind of owner an input value descriptor belongs to.
const OWNER_KEY: &str = "__gql_owner";
/// `LIST` or `NON_NULL` on a wrapper `__Type` descriptor.
const KIND_KEY: &str = "__gql_kind";
/// The wrapped type of a wrapper `__Type` descriptor.
const OF_KEY: &str = "__gql_of";

const SCHEMA_TYPE: &str = "__Schema";
const TYPE_TYPE: &str = "__Type";
const TYPE_KIND_TYPE: &str = "__TypeKind";
const FIELD_TYPE: &str = "__Field";
const INPUT_VALUE_TYPE: &str = "__InputValue";
const ENUM_VALUE_TYPE: &str = "__EnumValue";
const DIRECTIVE_TYPE: &str = "__Directive";
const DIRECTIVE_LOCATION_TYPE: &str = "__DirectiveLocation";

const OWNER_FIELD: &str = "field";
const OWNER_INPUT_OBJECT: &str = "inputObject";
const OWNER_DIRECTIVE: &str = "directive";

const DESCRIPTION_SCHEMA: &str = "A GraphQL Schema defines the capabilities of a GraphQL server. It exposes all available types and directives on the server, as well as the entry points for query, mutation, and subscription operations.";
const DESCRIPTION_TYPE: &str = "The fundamental unit of any GraphQL Schema is the type. There are many kinds of types in GraphQL as represented by the `__TypeKind` enum.\n\nDepending on the kind of a type, certain fields describe information about that type. Scalar types provide no information beyond a name, description and optional `specifiedByURL`, while Enum types provide their values. Object and Interface types provide the fields they describe. Abstract types, Union and Interface, provide the Object types possible at runtime. List and NonNull types compose other types.";
const DESCRIPTION_FIELD: &str = "Object and Interface types are described by a list of Fields, each of which has a name, potentially a list of arguments, and a return type.";
const DESCRIPTION_INPUT_VALUE: &str = "Arguments provided to Fields or Directives and the input fields of an InputObject are represented as Input Values which describe their type and optionally a default value.";
const DESCRIPTION_ENUM_VALUE: &str = "One possible value for a given Enum. Enum values are unique values, not a placeholder for a string or numeric value. However an Enum value is returned in a JSON response as a string.";
const DESCRIPTION_DIRECTIVE: &str = "A Directive provides a way to describe alternate runtime execution and type validation behavior in a GraphQL document.\n\nIn some cases, you need to provide options to alter GraphQL's execution behavior in ways field arguments will not suffice, such as conditionally including or skipping a field. Directives provide this by describing additional information to the executor.";
const DESCRIPTION_TYPE_KIND: &str = "An enum describing what kind of type a given `__Type` is.";
const DESCRIPTION_DIRECTIVE_LOCATION: &str = "A Directive can be adjacent to many parts of the GraphQL language, a __DirectiveLocation describes one such possible adjacencies.";

const DESCRIPTION_SCHEMA_TYPES: &str = "A list of all types supported by this server.";
const DESCRIPTION_SCHEMA_QUERY_TYPE: &str = "The type that query operations will be rooted at.";
const DESCRIPTION_SCHEMA_MUTATION_TYPE: &str =
    "If this server supports mutation, the type that mutation operations will be rooted at.";
const DESCRIPTION_SCHEMA_SUBSCRIPTION_TYPE: &str =
    "If this server support subscription, the type that subscription operations will be rooted at.";
const DESCRIPTION_SCHEMA_DIRECTIVES: &str = "A list of all directives supported by this server.";
const DESCRIPTION_INPUT_VALUE_DEFAULT_VALUE: &str =
    "A GraphQL-formatted string representing the default value for this input value.";

/// The introspection type definitions injected into every schema.
pub(crate) fn types() -> Vec<Type> {
    vec![
        schema_definition(),
        type_definition(),
        type_kind_definition(),
        field_definition(),
        input_value_definition(),
        enum_value_definition(),
        directive_definition(),
        directive_location_definition(),
    ]
}

/// Resolve one of the meta-fields which are not part of the schema's type map:
/// `__schema` and `__type` on the query root, and `__typename` anywhere.
///
/// The executor calls this before looking up a normal field definition. Returning `None` means
/// the field is not a meta-field and normal resolution should continue.
pub(crate) fn resolve_meta_field(
    field_name: &str,
    field: &crate::ast::Field<'_>,
    context: &crate::execution::ExecutionContext,
) -> Option<Result<Value, crate::error::GraphQLError>> {
    match field_name {
        "__schema" => Some(Ok(descriptor([(TYPENAME_KEY, Value::from(SCHEMA_TYPE))]))),
        "__type" => Some(resolve_type_meta_field(field, context)),
        _ => None,
    }
}

fn resolve_type_meta_field(
    field: &ast::Field<'_>,
    context: &ExecutionContext,
) -> Result<Value, GraphQLError> {
    let Some(argument) = field.argument("name") else {
        return Err(GraphQLError::new(
            "Field \"__type\" requires a \"name\" argument of type String!.",
        )
        .at(field.location));
    };

    // The `name` argument is `String!`. Coercing it directly keeps introspection independent of
    // the general input coercion machinery and is exactly what the specification prescribes.
    let name = match &argument.value {
        ast::Value::String(name) => name.as_ref().to_owned(),
        ast::Value::Variable(variable) => match context.variables.get(variable) {
            Some(Value::String(value)) => value.to_string(),
            Some(other) => {
                return Err(GraphQLError::new(format!(
                    "Argument \"name\" of \"__type\" must be a String, but variable \"${variable}\" holds {}.",
                    other.kind()
                ))
                .at(argument.location));
            }
            None => {
                return Err(GraphQLError::new(format!(
                    "Argument \"name\" of \"__type\" requires variable \"${variable}\", which was not provided."
                ))
                .at(argument.location));
            }
        },
        _ => {
            return Err(GraphQLError::new(
                "Argument \"name\" of \"__type\" must be a String literal or a variable.",
            )
            .at(argument.location));
        }
    };

    if context.schema.get_type(&name).is_none() {
        return Ok(Value::Null);
    }
    Ok(named_type_descriptor(&name))
}

// ---------------------------------------------------------------------------------------------
// Type definitions
// ---------------------------------------------------------------------------------------------

fn schema_definition() -> Type {
    object_type(
        SCHEMA_TYPE,
        Some(DESCRIPTION_SCHEMA),
        vec![
            field("description", TypeRef::string(), |context, _, _| {
                Ok(optional_string(context.schema.description()))
            }),
            field(
                "types",
                TypeRef::named(TYPE_TYPE).non_null().list().non_null(),
                |context, _, _| {
                    Ok(Value::List(
                        context
                            .schema
                            .types()
                            .map(|(name, _)| named_type_descriptor(name))
                            .collect(),
                    ))
                },
            )
            .description(DESCRIPTION_SCHEMA_TYPES),
            field(
                "queryType",
                TypeRef::named(TYPE_TYPE).non_null(),
                |context, _, _| Ok(named_type_descriptor(context.schema.query_name())),
            )
            .description(DESCRIPTION_SCHEMA_QUERY_TYPE),
            field(
                "mutationType",
                TypeRef::named(TYPE_TYPE),
                |context, _, _| {
                    Ok(context
                        .schema
                        .mutation_name()
                        .map_or(Value::Null, |name| named_type_descriptor(name)))
                },
            )
            .description(DESCRIPTION_SCHEMA_MUTATION_TYPE),
            field(
                "subscriptionType",
                TypeRef::named(TYPE_TYPE),
                |context, _, _| {
                    Ok(context
                        .schema
                        .subscription_name()
                        .map_or(Value::Null, |name| named_type_descriptor(name)))
                },
            )
            .description(DESCRIPTION_SCHEMA_SUBSCRIPTION_TYPE),
            field(
                "directives",
                TypeRef::named(DIRECTIVE_TYPE).non_null().list().non_null(),
                |context, _, args| {
                    let include_deprecated = include_deprecated(args);
                    Ok(Value::List(
                        context
                            .schema
                            .directives()
                            .filter(|(_, directive)| {
                                include_deprecated || directive.deprecation_reason.is_none()
                            })
                            .map(|(_, directive)| directive_descriptor(&directive.name))
                            .collect(),
                    ))
                },
            )
            .description(DESCRIPTION_SCHEMA_DIRECTIVES)
            .arg(include_deprecated_argument()),
        ],
    )
}

fn type_definition() -> Type {
    object_type(
        TYPE_TYPE,
        Some(DESCRIPTION_TYPE),
        vec![
            field(
                "kind",
                TypeRef::named(TYPE_KIND_TYPE).non_null(),
                |context, parent, _| resolve_type_kind(&context.schema, parent),
            ),
            field("name", TypeRef::string(), |_, parent, _| {
                Ok(descriptor_name(parent))
            }),
            field("description", TypeRef::string(), |context, parent, _| {
                Ok(named_type(&context.schema, parent)
                    .and_then(type_description)
                    .map_or(Value::Null, |description| Value::from(description)))
            }),
            field("specifiedByURL", TypeRef::string(), |context, parent, _| {
                Ok(match named_type(&context.schema, parent) {
                    Some(Type::Scalar(scalar)) => {
                        optional_string(scalar.specified_by_url.as_deref())
                    }
                    _ => Value::Null,
                })
            }),
            field(
                "fields",
                TypeRef::named(FIELD_TYPE).non_null().list(),
                |context, parent, args| resolve_fields(context, parent, args),
            )
            .arg(include_deprecated_argument()),
            field(
                "interfaces",
                TypeRef::named(TYPE_TYPE).non_null().list(),
                |context, parent, _| resolve_interfaces(context, parent),
            ),
            field(
                "possibleTypes",
                TypeRef::named(TYPE_TYPE).non_null().list(),
                |context, parent, _| resolve_possible_types(context, parent),
            ),
            field(
                "enumValues",
                TypeRef::named(ENUM_VALUE_TYPE).non_null().list(),
                |context, parent, args| resolve_enum_values(context, parent, args),
            )
            .arg(include_deprecated_argument()),
            field(
                "inputFields",
                TypeRef::named(INPUT_VALUE_TYPE).non_null().list(),
                |context, parent, args| resolve_input_fields(context, parent, args),
            )
            .arg(include_deprecated_argument()),
            field("ofType", TypeRef::named(TYPE_TYPE), |_, parent, _| {
                Ok(descriptor_of_type(parent))
            }),
            field("isOneOf", TypeRef::boolean(), |context, parent, _| {
                Ok(match named_type(&context.schema, parent) {
                    Some(Type::InputObject(input)) => Value::Boolean(input.one_of),
                    _ => Value::Null,
                })
            }),
        ],
    )
}

fn type_kind_definition() -> Type {
    let mut enumeration = EnumType::new(TYPE_KIND_TYPE).description(DESCRIPTION_TYPE_KIND);
    for (value, description) in [
        ("SCALAR", "Indicates this type is a scalar."),
        (
            "OBJECT",
            "Indicates this type is an object. `fields` and `interfaces` are valid fields.",
        ),
        (
            "INTERFACE",
            "Indicates this type is an interface. `fields`, `interfaces`, and `possibleTypes` are valid fields.",
        ),
        (
            "UNION",
            "Indicates this type is a union. `possibleTypes` is a valid field.",
        ),
        (
            "ENUM",
            "Indicates this type is an enum. `enumValues` is a valid field.",
        ),
        (
            "INPUT_OBJECT",
            "Indicates this type is an input object. `inputFields` is a valid field.",
        ),
        (
            "LIST",
            "Indicates this type is a list. `ofType` is a valid field.",
        ),
        (
            "NON_NULL",
            "Indicates this type is a non-null. `ofType` is a valid field.",
        ),
    ] {
        enumeration = enumeration.value_with(value, Some(description.into()), None);
    }
    enumeration.into()
}

fn field_definition() -> Type {
    object_type(
        FIELD_TYPE,
        Some(DESCRIPTION_FIELD),
        vec![
            field("name", TypeRef::string().non_null(), |_, parent, _| {
                Ok(descriptor_name(parent))
            }),
            field("description", TypeRef::string(), |context, parent, _| {
                Ok(lookup_field(&context.schema, parent)
                    .and_then(|field| field.description.as_deref())
                    .map_or(Value::Null, |description| Value::from(description)))
            }),
            field(
                "args",
                TypeRef::named(INPUT_VALUE_TYPE)
                    .non_null()
                    .list()
                    .non_null(),
                |context, parent, args| resolve_field_arguments(context, parent, args),
            )
            .arg(include_deprecated_argument()),
            field(
                "type",
                TypeRef::named(TYPE_TYPE).non_null(),
                |context, parent, _| {
                    let field = lookup_field(&context.schema, parent).ok_or_else(|| {
                        GraphQLError::new("Invalid __Field descriptor: the field does not exist.")
                    })?;
                    Ok(type_ref_descriptor(&field.ty))
                },
            ),
            field(
                "isDeprecated",
                TypeRef::boolean().non_null(),
                |context, parent, _| {
                    Ok(Value::Boolean(
                        lookup_field(&context.schema, parent)
                            .is_some_and(|field| field.deprecation_reason.is_some()),
                    ))
                },
            ),
            field(
                "deprecationReason",
                TypeRef::string(),
                |context, parent, _| {
                    Ok(lookup_field(&context.schema, parent)
                        .and_then(|field| field.deprecation_reason.as_deref())
                        .map_or(Value::Null, |reason| Value::from(reason)))
                },
            ),
        ],
    )
}

fn input_value_definition() -> Type {
    object_type(
        INPUT_VALUE_TYPE,
        Some(DESCRIPTION_INPUT_VALUE),
        vec![
            field("name", TypeRef::string().non_null(), |_, parent, _| {
                Ok(descriptor_name(parent))
            }),
            field("description", TypeRef::string(), |context, parent, _| {
                Ok(lookup_input_value(&context.schema, parent)
                    .and_then(|value| value.description.as_deref())
                    .map_or(Value::Null, |description| Value::from(description)))
            }),
            field(
                "type",
                TypeRef::named(TYPE_TYPE).non_null(),
                |context, parent, _| {
                    let value = lookup_input_value(&context.schema, parent).ok_or_else(|| {
                        GraphQLError::new(
                            "Invalid __InputValue descriptor: the input value does not exist.",
                        )
                    })?;
                    Ok(type_ref_descriptor(&value.ty))
                },
            ),
            field("defaultValue", TypeRef::string(), |context, parent, _| {
                Ok(lookup_input_value(&context.schema, parent)
                    .and_then(|value| value.default_value.as_ref())
                    .map_or(Value::Null, |value| {
                        Value::from(crate::sdl::print_value(value))
                    }))
            })
            .description(DESCRIPTION_INPUT_VALUE_DEFAULT_VALUE),
            field(
                "isDeprecated",
                TypeRef::boolean().non_null(),
                |context, parent, _| {
                    Ok(Value::Boolean(
                        lookup_input_value(&context.schema, parent)
                            .is_some_and(|value| value.deprecation_reason.is_some()),
                    ))
                },
            ),
            field(
                "deprecationReason",
                TypeRef::string(),
                |context, parent, _| {
                    Ok(lookup_input_value(&context.schema, parent)
                        .and_then(|value| value.deprecation_reason.as_deref())
                        .map_or(Value::Null, |reason| Value::from(reason)))
                },
            ),
        ],
    )
}

fn enum_value_definition() -> Type {
    object_type(
        ENUM_VALUE_TYPE,
        Some(DESCRIPTION_ENUM_VALUE),
        vec![
            field("name", TypeRef::string().non_null(), |_, parent, _| {
                Ok(descriptor_name(parent))
            }),
            field("description", TypeRef::string(), |context, parent, _| {
                Ok(lookup_enum_value(&context.schema, parent)
                    .and_then(|value| value.description.as_deref())
                    .map_or(Value::Null, |description| Value::from(description)))
            }),
            field(
                "isDeprecated",
                TypeRef::boolean().non_null(),
                |context, parent, _| {
                    Ok(Value::Boolean(
                        lookup_enum_value(&context.schema, parent)
                            .is_some_and(|value| value.deprecation_reason.is_some()),
                    ))
                },
            ),
            field(
                "deprecationReason",
                TypeRef::string(),
                |context, parent, _| {
                    Ok(lookup_enum_value(&context.schema, parent)
                        .and_then(|value| value.deprecation_reason.as_deref())
                        .map_or(Value::Null, |reason| Value::from(reason)))
                },
            ),
        ],
    )
}

fn directive_definition() -> Type {
    object_type(
        DIRECTIVE_TYPE,
        Some(DESCRIPTION_DIRECTIVE),
        vec![
            field("name", TypeRef::string().non_null(), |_, parent, _| {
                Ok(descriptor_name(parent))
            }),
            field("description", TypeRef::string(), |context, parent, _| {
                Ok(lookup_directive(&context.schema, parent)
                    .and_then(|directive| directive.description.as_deref())
                    .map_or(Value::Null, |description| Value::from(description)))
            }),
            field(
                "isRepeatable",
                TypeRef::boolean().non_null(),
                |context, parent, _| {
                    Ok(Value::Boolean(
                        lookup_directive(&context.schema, parent)
                            .is_some_and(|directive| directive.repeatable),
                    ))
                },
            ),
            field(
                "locations",
                TypeRef::named(DIRECTIVE_LOCATION_TYPE)
                    .non_null()
                    .list()
                    .non_null(),
                |context, parent, _| {
                    let directive = lookup_directive(&context.schema, parent).ok_or_else(|| {
                        GraphQLError::new(
                            "Invalid __Directive descriptor: the directive does not exist.",
                        )
                    })?;
                    Ok(Value::List(
                        directive
                            .locations
                            .iter()
                            .map(|location| Value::enumeration(location.as_str()))
                            .collect(),
                    ))
                },
            ),
            field(
                "args",
                TypeRef::named(INPUT_VALUE_TYPE)
                    .non_null()
                    .list()
                    .non_null(),
                |context, parent, args| resolve_directive_arguments(context, parent, args),
            )
            .arg(include_deprecated_argument()),
            field(
                "isDeprecated",
                TypeRef::boolean().non_null(),
                |context, parent, _| {
                    Ok(Value::Boolean(
                        lookup_directive(&context.schema, parent)
                            .is_some_and(|directive| directive.deprecation_reason.is_some()),
                    ))
                },
            ),
            field(
                "deprecationReason",
                TypeRef::string(),
                |context, parent, _| {
                    Ok(lookup_directive(&context.schema, parent)
                        .and_then(|directive| directive.deprecation_reason.as_deref())
                        .map_or(Value::Null, |reason| Value::from(reason)))
                },
            ),
        ],
    )
}

fn directive_location_definition() -> Type {
    let mut enumeration =
        EnumType::new(DIRECTIVE_LOCATION_TYPE).description(DESCRIPTION_DIRECTIVE_LOCATION);
    for (location, description) in [
        (
            ast::DirectiveLocation::Query,
            "Location adjacent to a query operation.",
        ),
        (
            ast::DirectiveLocation::Mutation,
            "Location adjacent to a mutation operation.",
        ),
        (
            ast::DirectiveLocation::Subscription,
            "Location adjacent to a subscription operation.",
        ),
        (
            ast::DirectiveLocation::Field,
            "Location adjacent to a field.",
        ),
        (
            ast::DirectiveLocation::FragmentDefinition,
            "Location adjacent to a fragment definition.",
        ),
        (
            ast::DirectiveLocation::FragmentSpread,
            "Location adjacent to a fragment spread.",
        ),
        (
            ast::DirectiveLocation::InlineFragment,
            "Location adjacent to an inline fragment.",
        ),
        (
            ast::DirectiveLocation::VariableDefinition,
            "Location adjacent to a variable definition.",
        ),
        (
            ast::DirectiveLocation::Schema,
            "Location adjacent to a schema definition.",
        ),
        (
            ast::DirectiveLocation::Scalar,
            "Location adjacent to a scalar definition.",
        ),
        (
            ast::DirectiveLocation::Object,
            "Location adjacent to an object type definition.",
        ),
        (
            ast::DirectiveLocation::FieldDefinition,
            "Location adjacent to a field definition.",
        ),
        (
            ast::DirectiveLocation::ArgumentDefinition,
            "Location adjacent to an argument definition.",
        ),
        (
            ast::DirectiveLocation::Interface,
            "Location adjacent to an interface definition.",
        ),
        (
            ast::DirectiveLocation::Union,
            "Location adjacent to a union definition.",
        ),
        (
            ast::DirectiveLocation::Enum,
            "Location adjacent to an enum definition.",
        ),
        (
            ast::DirectiveLocation::EnumValue,
            "Location adjacent to an enum value definition.",
        ),
        (
            ast::DirectiveLocation::InputObject,
            "Location adjacent to an input object type definition.",
        ),
        (
            ast::DirectiveLocation::InputFieldDefinition,
            "Location adjacent to an input object field definition.",
        ),
        (
            ast::DirectiveLocation::DirectiveDefinition,
            "Location adjacent to a directive definition.",
        ),
    ] {
        enumeration = enumeration.value_with(location.as_str(), Some(description.into()), None);
    }
    enumeration.into()
}

// ---------------------------------------------------------------------------------------------
// Definition helpers
// ---------------------------------------------------------------------------------------------

fn object_type(name: &str, description: Option<&str>, fields: Vec<Field>) -> Type {
    let mut object = ObjectType::new(name);
    if let Some(description) = description {
        object = object.description(description);
    }
    for field in fields {
        object = object.field(field);
    }
    object.into()
}

fn field<F>(name: &str, ty: TypeRef, resolve: F) -> Field
where
    F: for<'a> Fn(&'a ExecutionContext, &'a Value, &'a Object) -> Result<Value, GraphQLError>
        + Send
        + Sync
        + 'static,
{
    Field::new(name, ty).resolve(move |context| {
        let result = resolve(context.context, context.parent, context.args);
        Box::pin(async move { result })
    })
}

fn include_deprecated_argument() -> InputValue {
    InputValue::new("includeDeprecated", TypeRef::boolean())
        .description("Include deprecated entries in the result.")
        .default_value(false)
}

// ---------------------------------------------------------------------------------------------
// Descriptors
// ---------------------------------------------------------------------------------------------

fn descriptor(pairs: impl IntoIterator<Item = (&'static str, Value)>) -> Value {
    let mut object = Object::new();
    for (key, value) in pairs {
        object.insert(key, value);
    }
    Value::Object(Arc::new(object))
}

fn named_type_descriptor(name: &str) -> Value {
    descriptor([
        (TYPENAME_KEY, Value::from(TYPE_TYPE)),
        (NAME_KEY, Value::from(name)),
    ])
}

fn type_ref_descriptor(ty: &TypeRef) -> Value {
    match ty {
        TypeRef::Named(name) => named_type_descriptor(name),
        TypeRef::List(inner) => descriptor([
            (TYPENAME_KEY, Value::from(TYPE_TYPE)),
            (KIND_KEY, Value::from("LIST")),
            (OF_KEY, type_ref_descriptor(inner)),
        ]),
        TypeRef::NonNull(inner) => descriptor([
            (TYPENAME_KEY, Value::from(TYPE_TYPE)),
            (KIND_KEY, Value::from("NON_NULL")),
            (OF_KEY, type_ref_descriptor(inner)),
        ]),
    }
}

fn field_descriptor(owner: &str, name: &str) -> Value {
    descriptor([
        (TYPENAME_KEY, Value::from(FIELD_TYPE)),
        (PARENT_KEY, Value::from(owner)),
        (NAME_KEY, Value::from(name)),
    ])
}

fn field_argument_descriptor(owner: &str, field_name: &str, name: &str) -> Value {
    descriptor([
        (TYPENAME_KEY, Value::from(INPUT_VALUE_TYPE)),
        (OWNER_KEY, Value::from(OWNER_FIELD)),
        (PARENT_KEY, Value::from(owner)),
        (FIELD_KEY, Value::from(field_name)),
        (NAME_KEY, Value::from(name)),
    ])
}

fn input_field_descriptor(owner: &str, name: &str) -> Value {
    descriptor([
        (TYPENAME_KEY, Value::from(INPUT_VALUE_TYPE)),
        (OWNER_KEY, Value::from(OWNER_INPUT_OBJECT)),
        (PARENT_KEY, Value::from(owner)),
        (NAME_KEY, Value::from(name)),
    ])
}

fn directive_argument_descriptor(owner: &str, name: &str) -> Value {
    descriptor([
        (TYPENAME_KEY, Value::from(INPUT_VALUE_TYPE)),
        (OWNER_KEY, Value::from(OWNER_DIRECTIVE)),
        (PARENT_KEY, Value::from(owner)),
        (NAME_KEY, Value::from(name)),
    ])
}

fn enum_value_descriptor(owner: &str, name: &str) -> Value {
    descriptor([
        (TYPENAME_KEY, Value::from(ENUM_VALUE_TYPE)),
        (PARENT_KEY, Value::from(owner)),
        (NAME_KEY, Value::from(name)),
    ])
}

fn directive_descriptor(name: &str) -> Value {
    descriptor([
        (TYPENAME_KEY, Value::from(DIRECTIVE_TYPE)),
        (NAME_KEY, Value::from(name)),
    ])
}

// ---------------------------------------------------------------------------------------------
// Descriptor accessors
// ---------------------------------------------------------------------------------------------

fn descriptor_name(parent: &Value) -> Value {
    parent
        .as_object()
        .and_then(|object| object.get(NAME_KEY))
        .cloned()
        .unwrap_or(Value::Null)
}

fn descriptor_of_type(parent: &Value) -> Value {
    parent
        .as_object()
        .and_then(|object| object.get(OF_KEY))
        .cloned()
        .unwrap_or(Value::Null)
}

fn object_str<'a>(parent: &'a Value, key: &str) -> Option<&'a str> {
    parent.as_object()?.get(key)?.as_str()
}

fn named_type<'a>(schema: &'a Schema, parent: &Value) -> Option<&'a Type> {
    schema.get_type(object_str(parent, NAME_KEY)?)
}

fn optional_string(value: Option<&str>) -> Value {
    value.map_or(Value::Null, |value| Value::from(value))
}

fn include_deprecated(args: &Object) -> bool {
    args.get("includeDeprecated")
        .and_then(Value::as_bool)
        .unwrap_or(false)
}

fn type_description(ty: &Type) -> Option<&str> {
    match ty {
        Type::Scalar(ty) => ty.description.as_deref(),
        Type::Object(ty) => ty.description.as_deref(),
        Type::Interface(ty) => ty.description.as_deref(),
        Type::Union(ty) => ty.description.as_deref(),
        Type::Enum(ty) => ty.description.as_deref(),
        Type::InputObject(ty) => ty.description.as_deref(),
    }
}

// ---------------------------------------------------------------------------------------------
// Schema lookups
// ---------------------------------------------------------------------------------------------

fn lookup_field<'a>(schema: &'a Schema, parent: &Value) -> Option<&'a Field> {
    let owner = object_str(parent, PARENT_KEY)?;
    let name = object_str(parent, NAME_KEY)?;
    match schema.get_type(owner)? {
        Type::Object(object) => object.fields.get(name),
        Type::Interface(interface) => interface.fields.get(name),
        _ => None,
    }
}

fn lookup_input_value<'a>(schema: &'a Schema, parent: &Value) -> Option<&'a InputValue> {
    let owner = object_str(parent, OWNER_KEY)?;
    let parent_name = object_str(parent, PARENT_KEY)?;
    let name = object_str(parent, NAME_KEY)?;
    match owner {
        OWNER_FIELD => {
            let field_name = object_str(parent, FIELD_KEY)?;
            let field = match schema.get_type(parent_name)? {
                Type::Object(object) => object.fields.get(field_name),
                Type::Interface(interface) => interface.fields.get(field_name),
                _ => None,
            }?;
            field.args.get(name)
        }
        OWNER_INPUT_OBJECT => schema.input_object_type(parent_name)?.fields.get(name),
        OWNER_DIRECTIVE => schema.get_directive(parent_name)?.args.get(name),
        _ => None,
    }
}

fn lookup_enum_value<'a>(schema: &'a Schema, parent: &Value) -> Option<&'a EnumValue> {
    let owner = object_str(parent, PARENT_KEY)?;
    let name = object_str(parent, NAME_KEY)?;
    schema.enum_type(owner)?.values.get(name)
}

fn lookup_directive<'a>(
    schema: &'a Schema,
    parent: &Value,
) -> Option<&'a crate::types::DirectiveDefinition> {
    schema.get_directive(object_str(parent, NAME_KEY)?)
}

// ---------------------------------------------------------------------------------------------
// Resolvers
// ---------------------------------------------------------------------------------------------

fn resolve_type_kind(schema: &Schema, parent: &Value) -> Result<Value, GraphQLError> {
    if let Some(kind) = object_str(parent, KIND_KEY) {
        return Ok(Value::enumeration(kind));
    }
    let name = object_str(parent, NAME_KEY)
        .ok_or_else(|| GraphQLError::new("Invalid __Type descriptor: it has no name."))?;
    let ty = schema
        .get_type(name)
        .ok_or_else(|| GraphQLError::new(format!("Unknown type \"{name}\".")))?;
    Ok(Value::enumeration(ty.kind().as_str()))
}

fn resolve_fields(
    context: &ExecutionContext,
    parent: &Value,
    args: &Object,
) -> Result<Value, GraphQLError> {
    let include_deprecated = include_deprecated(args);
    let Some(ty) = named_type(&context.schema, parent) else {
        return Ok(Value::Null);
    };
    let (owner, fields) = match ty {
        Type::Object(object) => (&object.name, &object.fields),
        Type::Interface(interface) => (&interface.name, &interface.fields),
        _ => return Ok(Value::Null),
    };
    Ok(Value::List(
        fields
            .values()
            .filter(|field| !field.name.starts_with("__"))
            .filter(|field| include_deprecated || field.deprecation_reason.is_none())
            .map(|field| field_descriptor(owner, &field.name))
            .collect(),
    ))
}

fn resolve_interfaces(context: &ExecutionContext, parent: &Value) -> Result<Value, GraphQLError> {
    let Some(ty) = named_type(&context.schema, parent) else {
        return Ok(Value::Null);
    };
    let interfaces = match ty {
        Type::Object(object) => &object.interfaces,
        Type::Interface(interface) => &interface.interfaces,
        _ => return Ok(Value::Null),
    };
    Ok(Value::List(
        interfaces
            .iter()
            .map(|name| named_type_descriptor(name))
            .collect(),
    ))
}

fn resolve_possible_types(
    context: &ExecutionContext,
    parent: &Value,
) -> Result<Value, GraphQLError> {
    let Some(ty) = named_type(&context.schema, parent) else {
        return Ok(Value::Null);
    };
    if !matches!(ty, Type::Interface(_) | Type::Union(_)) {
        return Ok(Value::Null);
    }
    Ok(Value::List(
        context
            .schema
            .possible_types(ty.name())
            .iter()
            .map(|name| named_type_descriptor(name))
            .collect(),
    ))
}

fn resolve_enum_values(
    context: &ExecutionContext,
    parent: &Value,
    args: &Object,
) -> Result<Value, GraphQLError> {
    let include_deprecated = include_deprecated(args);
    let Some(ty) = named_type(&context.schema, parent) else {
        return Ok(Value::Null);
    };
    let Type::Enum(enumeration) = ty else {
        return Ok(Value::Null);
    };
    Ok(Value::List(
        enumeration
            .values
            .values()
            .filter(|value| include_deprecated || value.deprecation_reason.is_none())
            .map(|value| enum_value_descriptor(&enumeration.name, &value.name))
            .collect(),
    ))
}

fn resolve_input_fields(
    context: &ExecutionContext,
    parent: &Value,
    args: &Object,
) -> Result<Value, GraphQLError> {
    let include_deprecated = include_deprecated(args);
    let Some(ty) = named_type(&context.schema, parent) else {
        return Ok(Value::Null);
    };
    let Type::InputObject(input) = ty else {
        return Ok(Value::Null);
    };
    Ok(Value::List(
        input
            .fields
            .values()
            .filter(|field| include_deprecated || field.deprecation_reason.is_none())
            .map(|field| input_field_descriptor(&input.name, &field.name))
            .collect(),
    ))
}

fn resolve_field_arguments(
    context: &ExecutionContext,
    parent: &Value,
    args: &Object,
) -> Result<Value, GraphQLError> {
    let include_deprecated = include_deprecated(args);
    let field = lookup_field(&context.schema, parent).ok_or_else(|| {
        GraphQLError::new("Invalid __Field descriptor: the field does not exist.")
    })?;
    let owner = object_str(parent, PARENT_KEY).unwrap_or_default();
    let field_name = object_str(parent, NAME_KEY).unwrap_or_default();
    Ok(Value::List(
        field
            .args
            .values()
            .filter(|argument| include_deprecated || argument.deprecation_reason.is_none())
            .map(|argument| field_argument_descriptor(owner, field_name, &argument.name))
            .collect(),
    ))
}

fn resolve_directive_arguments(
    context: &ExecutionContext,
    parent: &Value,
    args: &Object,
) -> Result<Value, GraphQLError> {
    let include_deprecated = include_deprecated(args);
    let directive = lookup_directive(&context.schema, parent).ok_or_else(|| {
        GraphQLError::new("Invalid __Directive descriptor: the directive does not exist.")
    })?;
    Ok(Value::List(
        directive
            .args
            .values()
            .filter(|argument| include_deprecated || argument.deprecation_reason.is_none())
            .map(|argument| directive_argument_descriptor(&directive.name, &argument.name))
            .collect(),
    ))
}
