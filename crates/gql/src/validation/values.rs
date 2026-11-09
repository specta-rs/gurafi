//! Literal value validation.
//!
//! Values are checked against their expected input type recursively. Variable references are
//! deliberately skipped here: whether a variable may appear in a position is decided by
//! `VariablesInAllowedPosition`.

use std::collections::HashSet;

use crate::{
    ast::Value,
    error::{GraphQLError, Location},
    schema::Schema,
    types::{Type, TypeRef},
};

/// Validate one literal value against an input type.
pub(crate) fn validate_value(
    schema: &Schema,
    value: &Value<'_>,
    ty: &TypeRef,
    const_context: bool,
    location: Location,
    errors: &mut Vec<GraphQLError>,
) {
    if let Value::Variable(name) = value {
        if const_context {
            errors.push(
                GraphQLError::new(format!(
                    "Variable \"${name}\" cannot be used in a default value."
                ))
                .at(location),
            );
        }
        return;
    }

    match ty {
        TypeRef::NonNull(inner) => {
            if matches!(value, Value::Null) {
                errors.push(
                    GraphQLError::new(format!("Expected value of type \"{ty}\", found null."))
                        .at(location),
                );
            } else {
                validate_value(schema, value, inner, const_context, location, errors);
            }
        }
        TypeRef::List(item) => match value {
            Value::Null => {}
            Value::List(items) => {
                for item_value in items {
                    validate_value(schema, item_value, item, const_context, location, errors);
                }
            }
            // A single value is coerced into a list of one item, exactly like the
            // specification's input coercion.
            other => validate_value(schema, other, item, const_context, location, errors),
        },
        TypeRef::Named(name) => {
            if !matches!(value, Value::Null) {
                validate_named(schema, value, name, const_context, location, errors);
            }
        }
    }
}

fn validate_named(
    schema: &Schema,
    value: &Value<'_>,
    name: &str,
    const_context: bool,
    location: Location,
    errors: &mut Vec<GraphQLError>,
) {
    let Some(ty) = schema.get_type(name) else {
        // KnownTypeNames reports unknown types; do not pile on.
        return;
    };

    match ty {
        Type::Scalar(scalar) => {
            if let Some(parse_literal) = scalar.parse_literal_fn() {
                if let Err(message) = parse_literal(value) {
                    errors.push(GraphQLError::new(message).at(location));
                }
            } else if matches!(value, Value::List(_) | Value::Object(_)) {
                // Custom scalars without a parser accept every scalar literal kind, but lists
                // and objects are never valid scalar literals.
                errors.push(expected_value_error(name, value, location));
            }
        }
        Type::Enum(enumeration) => match value {
            Value::Enum(variant) => {
                if !enumeration.has_value(variant) {
                    errors.push(
                        GraphQLError::new(format!(
                            "Value \"{variant}\" does not exist in \"{name}\" enum."
                        ))
                        .at(location),
                    );
                }
            }
            other => errors.push(expected_value_error(name, other, location)),
        },
        Type::InputObject(input) => {
            let Value::Object(fields) = value else {
                errors.push(expected_value_error(name, value, location));
                return;
            };

            for field in fields {
                match input.fields.get(field.name) {
                    Some(definition) => validate_value(
                        schema,
                        &field.value,
                        &definition.ty,
                        const_context,
                        field.location,
                        errors,
                    ),
                    None => errors.push(
                        GraphQLError::new(format!(
                            "Field \"{}\" is not defined by type \"{name}\".",
                            field.name
                        ))
                        .at(field.location),
                    ),
                }
            }

            for definition in input.fields.values() {
                if matches!(definition.ty, TypeRef::NonNull(_))
                    && definition.default_value.is_none()
                    && !fields.iter().any(|field| field.name == &*definition.name)
                {
                    errors.push(
                        GraphQLError::new(format!(
                            "Field \"{name}.{}\" of required type \"{}\" was not provided.",
                            definition.name, definition.ty
                        ))
                        .at(location),
                    );
                }
            }
        }
        // Output types cannot be used as input types; the schema builder reports that.
        Type::Object(_) | Type::Interface(_) | Type::Union(_) => {}
    }
}

fn expected_value_error(name: &str, value: &Value<'_>, location: Location) -> GraphQLError {
    GraphQLError::new(format!(
        "Expected value of type \"{name}\", found {}.",
        describe_value(value)
    ))
    .at(location)
}

fn describe_value(value: &Value<'_>) -> String {
    match value {
        Value::Variable(name) => format!("${name}"),
        Value::Int(value) => value.to_string(),
        Value::Float(value) => value.to_string(),
        Value::String(value) => format!("\"{value}\""),
        Value::Boolean(value) => value.to_string(),
        Value::Null => "null".to_owned(),
        Value::Enum(value) => (*value).to_owned(),
        Value::List(_) => "a list".to_owned(),
        Value::Object(_) => "an object".to_owned(),
    }
}

/// Check that no input object literal defines the same field twice.
pub(crate) fn check_input_field_names(value: &Value<'_>, errors: &mut Vec<GraphQLError>) {
    match value {
        Value::Object(fields) => {
            let mut seen = HashSet::new();
            for field in fields {
                if !seen.insert(field.name) {
                    errors.push(
                        GraphQLError::new(format!(
                            "There can be only one input field named \"{}\".",
                            field.name
                        ))
                        .at(field.location),
                    );
                }
                check_input_field_names(&field.value, errors);
            }
        }
        Value::List(items) => {
            for item in items {
                check_input_field_names(item, errors);
            }
        }
        _ => {}
    }
}
