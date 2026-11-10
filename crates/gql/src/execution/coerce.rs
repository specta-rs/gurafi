//! Input coercion.
//!
//! Two entry points mirror the specification: variable values arrive as runtime [`Value`]s and
//! are coerced by [`coerce_variable_values`], while field arguments are literals in the document
//! and are coerced by [`coerce_argument_values`]. Both funnel into the same type-directed rules so
//! custom scalars, enums and nested input objects behave identically everywhere.
//!
//! Document literals can carry integers up to `i128` (see [`crate::ast::Value::Int`]), which lets
//! custom scalars receive values beyond the 32-bit `Int` range. JSON variable conversion happens
//! in the transport layer (for example `suku`), outside this crate; those transports must stop
//! narrowing JSON integers to `i32` before a custom scalar can receive a larger variable value.

use std::sync::Arc;

use crate::{
    Object, Value, ast,
    error::GraphQLError,
    schema::Schema,
    types::{EnumType, Field, InputObjectType, InputValue, ScalarType, Type, TypeRef},
};

use super::selection::type_ref;

/// Variable values after coercion.
#[derive(Clone, Debug, Default)]
pub struct CoercedVariables {
    /// The coerced values.
    pub values: Object,
}

/// Coerce variable definitions against the supplied input values.
///
/// # Errors
///
/// Returns every coercion failure, one per invalid variable.
pub fn coerce_variable_values(
    schema: &Schema,
    operation: &ast::OperationDefinition<'_>,
    inputs: Option<&Object>,
) -> Result<CoercedVariables, Vec<GraphQLError>> {
    let mut values = Object::with_capacity(operation.variables.len());
    let mut errors = Vec::new();

    for definition in &operation.variables {
        let ty = type_ref(&definition.ty);
        if schema.get_type(ty.named_type()).is_none() {
            errors.push(
                GraphQLError::new(format!(
                    "Variable \"${}\" has unknown type \"{}\".",
                    definition.name, ty
                ))
                .at(definition.location),
            );
            continue;
        }

        let supplied = inputs.and_then(|inputs| inputs.get(definition.name));
        match supplied {
            None => {
                if let Some(default) = &definition.default_value {
                    match coerce_literal(schema, default, &ty, None) {
                        Ok(value) => {
                            values.insert(definition.name, value);
                        }
                        Err(error) => {
                            let message = error.message().to_owned();
                            errors.push(
                                error
                                    .with_message(format!(
                                        "Variable \"${}\" has an invalid default value: {message}",
                                        definition.name
                                    ))
                                    .at(definition.location),
                            );
                        }
                    }
                } else if ty.is_non_null() {
                    errors.push(
                        GraphQLError::new(format!(
                            "Variable \"${}\" of required type \"{}\" was not provided.",
                            definition.name, ty
                        ))
                        .at(definition.location),
                    );
                }
                // A nullable variable that was not provided stays undefined, not null.
            }
            Some(input) => {
                if input.is_null() {
                    if ty.is_non_null() {
                        errors.push(
                            GraphQLError::new(format!(
                                "Variable \"${}\" of non-null type \"{}\" must not be null.",
                                definition.name, ty
                            ))
                            .at(definition.location),
                        );
                    } else {
                        values.insert(definition.name, Value::Null);
                    }
                } else {
                    let mut path = Vec::new();
                    match coerce_input_value(schema, input, &ty, &mut path) {
                        Ok(value) => {
                            values.insert(definition.name, value);
                        }
                        Err(error) => errors.push(
                            GraphQLError::new(format!(
                                "Variable \"${}\" got invalid value {}; {}",
                                definition.name,
                                render_value(input),
                                error.message()
                            ))
                            .at(definition.location),
                        ),
                    }
                }
            }
        }
    }

    if errors.is_empty() {
        Ok(CoercedVariables { values })
    } else {
        Err(errors)
    }
}

/// Coerce a field's arguments, applying defaults.
///
/// # Errors
///
/// Returns an error for missing required arguments, unknown arguments and invalid values.
pub fn coerce_argument_values(
    schema: &Schema,
    field: &Field,
    node: &ast::Field<'_>,
    variables: &Object,
) -> Result<Object, GraphQLError> {
    let mut coerced = Object::with_capacity(field.args.len());
    coerce_argument_values_into(schema, field, node, variables, &mut coerced)?;
    Ok(coerced)
}

/// Coerce a field's arguments into an existing, empty object.
///
/// Execution coerces arguments for every field it completes, so building the result in the
/// caller's slot avoids copying an [`Object`] out of the callee for each one.
pub(crate) fn coerce_argument_values_into(
    schema: &Schema,
    field: &Field,
    node: &ast::Field<'_>,
    variables: &Object,
    coerced: &mut Object,
) -> Result<(), GraphQLError> {
    debug_assert!(coerced.is_empty(), "argument object must start empty");

    // A document that lists exactly the field's arguments, in schema order, needs no
    // unknown-argument scan and no per-definition search: the two lists already correspond.
    if node.arguments.len() == field.args.len()
        && node
            .arguments
            .iter()
            .zip(field.args.iter())
            .all(|(argument, (name, _))| crate::types::names_eq(argument.name, name))
    {
        for (argument, (name, definition)) in node.arguments.iter().zip(field.args.iter()) {
            coerce_provided_argument(
                schema, field, name, definition, argument, variables, coerced,
            )?;
        }
        return Ok(());
    }

    for argument in &node.arguments {
        if crate::types::map_get(&field.args, argument.name).is_none() {
            return Err(GraphQLError::new(format!(
                "Unknown argument \"{}\" on field \"{}\".",
                argument.name, field.name
            ))
            .at(argument.location));
        }
    }

    for (name, definition) in &field.args {
        let Some(argument) = node.argument(name) else {
            if let Some(default) = &definition.default_value {
                let mut default_path = Vec::new();
                let value = coerce_input_value(schema, default, &definition.ty, &mut default_path)
                    .map_err(|error| {
                        let message = error.message().to_owned();
                        error.with_message(format!(
                            "Field \"{}\" argument \"{}\" has an invalid default value: {message}",
                            field.name, name
                        ))
                    })?;
                coerced.insert(name.clone(), value);
            } else if definition.ty.is_non_null() {
                return Err(GraphQLError::new(format!(
                    "Field \"{}\" argument \"{}\" of type \"{}\" is required, but it was not provided.",
                    field.name, name, definition.ty
                ))
                .at(node.location));
            }
            continue;
        };

        coerce_provided_argument(
            schema, field, name, definition, argument, variables, coerced,
        )?;
    }
    Ok(())
}

/// Coerce one provided argument into the field's argument object.
#[allow(clippy::too_many_arguments)]
fn coerce_provided_argument(
    schema: &Schema,
    field: &Field,
    name: &crate::types::Name,
    definition: &InputValue,
    argument: &ast::Argument<'_>,
    variables: &Object,
    coerced: &mut Object,
) -> Result<(), GraphQLError> {
    // A top-level variable reference follows the specification's *argument* rules rather
    // than literal rules: an undefined nullable variable means the argument is omitted.
    if let ast::Value::Variable(variable_name) = &argument.value {
        match variables.get(variable_name) {
            Some(value) => {
                if value.is_null() && definition.ty.is_non_null() {
                    return Err(GraphQLError::new(format!(
                        "Field \"{}\" argument \"{}\" of non-null type \"{}\" must not be null.",
                        field.name, name, definition.ty
                    ))
                    .at(argument.location));
                }
                coerced.insert(name.clone(), value.clone());
            }
            None => {
                if let Some(default) = &definition.default_value {
                    let mut default_path = Vec::new();
                    let value =
                        coerce_input_value(schema, default, &definition.ty, &mut default_path)
                            .map_err(|error| {
                                let message = error.message().to_owned();
                                error.with_message(format!(
                            "Field \"{}\" argument \"{}\" has an invalid default value: {message}",
                            field.name, name
                        ))
                            })?;
                    coerced.insert(name.clone(), value);
                } else if definition.ty.is_non_null() {
                    return Err(GraphQLError::new(format!(
                            "Field \"{}\" argument \"{}\" of required type \"{}\" was not provided because variable \"${}\" was not provided.",
                            field.name, name, definition.ty, variable_name
                        ))
                        .at(argument.location));
                }
            }
        }
    } else {
        let mut path = Vec::new();
        match value_from_ast(
            schema,
            &argument.value,
            &definition.ty,
            Some(variables),
            &mut path,
        ) {
            Ok(Some(value)) => {
                coerced.insert(name.clone(), value);
            }
            Ok(None) => {
                return Err(GraphQLError::new(format!(
                        "Field \"{}\" argument \"{}\" is invalid: a variable used in its value was not provided.",
                        field.name, name
                    ))
                    .at(argument.location));
            }
            Err(error) => {
                let message = error.message().to_owned();
                return Err(error
                    .with_message(format!(
                        "Field \"{}\" argument \"{}\": {message}",
                        field.name, name
                    ))
                    .at(argument.location));
            }
        }
    }
    Ok(())
}

/// Coerce a literal to a runtime value for an input type.
pub(crate) fn coerce_literal(
    schema: &Schema,
    value: &ast::Value<'_>,
    ty: &TypeRef,
    variables: Option<&Object>,
) -> Result<Value, GraphQLError> {
    let mut path = Vec::new();
    match value_from_ast(schema, value, ty, variables, &mut path)? {
        Some(value) => Ok(value),
        None => Err(GraphQLError::new(format!(
            "A value of type \"{ty}\" was required, but the variable used for it was not provided."
        ))),
    }
}

fn at_path(error: GraphQLError, path: &[String]) -> GraphQLError {
    if path.is_empty() {
        error
    } else {
        let message = error.message().to_owned();
        error.with_message(format!("{message} (at \"{}\")", path.join(".")))
    }
}

fn variable_is_missing(value: &ast::Value<'_>, variables: Option<&Object>) -> bool {
    match value {
        ast::Value::Variable(name) => {
            !variables.is_some_and(|variables| variables.contains_key(name))
        }
        _ => false,
    }
}

/// Verify that a schema's default value is valid for its declared input type.
pub(crate) fn validate_default_value(
    schema: &Schema,
    value: &Value,
    ty: &TypeRef,
) -> Result<(), GraphQLError> {
    let mut path = Vec::new();
    coerce_input_value(schema, value, ty, &mut path).map(|_| ())
}

/// Coerce a runtime (already parsed) input value.
pub(crate) fn coerce_input_value(
    schema: &Schema,
    value: &Value,
    ty: &TypeRef,
    path: &mut Vec<String>,
) -> Result<Value, GraphQLError> {
    match ty {
        TypeRef::NonNull(inner) => {
            if value.is_null() {
                return Err(at_path(
                    GraphQLError::new(format!("Expected a non-null value of type \"{ty}\".")),
                    path,
                ));
            }
            coerce_input_value(schema, value, inner, path)
        }
        _ => {
            if value.is_null() {
                return Ok(Value::Null);
            }
            match ty {
                TypeRef::List(inner) => coerce_list_value(schema, value, inner, path),
                TypeRef::Named(name) => coerce_named_value(schema, value, name, path),
                TypeRef::NonNull(_) => Err(at_path(
                    GraphQLError::new(
                        "Internal input coercion error: non-null type was not unwrapped.",
                    ),
                    path,
                )),
            }
        }
    }
}

fn coerce_list_value(
    schema: &Schema,
    value: &Value,
    inner: &TypeRef,
    path: &mut Vec<String>,
) -> Result<Value, GraphQLError> {
    // A non-list value coerces to a one-element list (October 2021 3.10 "Input Coercion").
    if let Value::List(items) = value {
        let mut coerced = Vec::with_capacity(items.len());
        for (index, item) in items.iter().enumerate() {
            path.push(index.to_string());
            let result = coerce_input_value(schema, item, inner, path);
            path.pop();
            coerced.push(result?);
        }
        Ok(Value::List(coerced))
    } else {
        let result = coerce_input_value(schema, value, inner, path)?;
        Ok(Value::List(vec![result]))
    }
}

fn coerce_named_value(
    schema: &Schema,
    value: &Value,
    name: &crate::types::Name,
    path: &mut Vec<String>,
) -> Result<Value, GraphQLError> {
    let Some(definition) = schema.get_type(name) else {
        return Err(at_path(
            GraphQLError::new(format!("Unknown input type \"{name}\".")),
            path,
        ));
    };
    match definition {
        Type::Scalar(scalar) => coerce_scalar_value(scalar, value, path),
        Type::Enum(enumeration) => coerce_enum_value(enumeration, value, path),
        Type::InputObject(input) => coerce_input_object_value(schema, input, value, path),
        _ => Err(at_path(
            GraphQLError::new(format!("Type \"{name}\" is not an input type.")),
            path,
        )),
    }
}

fn coerce_scalar_value(
    scalar: &ScalarType,
    value: &Value,
    path: &mut Vec<String>,
) -> Result<Value, GraphQLError> {
    if let Some(parse) = scalar.parse_value_fn() {
        return parse(value).map_err(|message| at_path(GraphQLError::new(message), path));
    }
    match scalar.name.as_ref() {
        "String" => match value {
            Value::String(_) => Ok(value.clone()),
            _ => Err(at_path(
                GraphQLError::new(format!(
                    "String cannot represent a value of type {}.",
                    value.kind()
                )),
                path,
            )),
        },
        "Int" => match value {
            Value::Int(_) => Ok(value.clone()),
            _ => Err(at_path(
                GraphQLError::new(format!(
                    "Int cannot represent a value of type {}.",
                    value.kind()
                )),
                path,
            )),
        },
        "Float" => match value {
            Value::Float(_) => Ok(value.clone()),
            Value::Int(integer) => Ok(Value::Float(f64::from(*integer))),
            _ => Err(at_path(
                GraphQLError::new(format!(
                    "Float cannot represent a value of type {}.",
                    value.kind()
                )),
                path,
            )),
        },
        "Boolean" => match value {
            Value::Boolean(_) => Ok(value.clone()),
            _ => Err(at_path(
                GraphQLError::new(format!(
                    "Boolean cannot represent a value of type {}.",
                    value.kind()
                )),
                path,
            )),
        },
        "ID" => match value {
            Value::String(value) | Value::Enum(value) => Ok(Value::String(value.clone())),
            Value::Int(value) => Ok(Value::String(Arc::from(value.to_string()))),
            _ => Err(at_path(
                GraphQLError::new(format!(
                    "ID cannot represent a value of type {}.",
                    value.kind()
                )),
                path,
            )),
        },
        // A custom scalar with no registered parser is transparent.
        _ => Ok(value.clone()),
    }
}

fn coerce_enum_value(
    enumeration: &EnumType,
    value: &Value,
    path: &mut Vec<String>,
) -> Result<Value, GraphQLError> {
    match value {
        Value::Enum(name) | Value::String(name) => {
            if enumeration.has_value(name) {
                Ok(Value::Enum(name.clone()))
            } else {
                Err(at_path(
                    GraphQLError::new(format!(
                        "Value \"{name}\" does not exist in enum \"{}\".",
                        enumeration.name
                    )),
                    path,
                ))
            }
        }
        other => Err(at_path(
            GraphQLError::new(format!(
                "Enum \"{}\" cannot represent a value of type {}.",
                enumeration.name,
                other.kind()
            )),
            path,
        )),
    }
}

fn coerce_input_object_value(
    schema: &Schema,
    input: &InputObjectType,
    value: &Value,
    path: &mut Vec<String>,
) -> Result<Value, GraphQLError> {
    let Value::Object(object) = value else {
        return Err(at_path(
            GraphQLError::new(format!(
                "Expected an object value of type \"{}\", but got a {}.",
                input.name,
                value.kind()
            )),
            path,
        ));
    };

    for (key, _) in object.iter() {
        if !input.fields.contains_key(&**key) {
            return Err(at_path(
                GraphQLError::new(format!(
                    "Field \"{key}\" is not defined by input object \"{}\".",
                    input.name
                )),
                path,
            ));
        }
    }

    let mut coerced = Object::with_capacity(input.fields.len());
    let mut provided = 0usize;
    for (field_name, field) in &input.fields {
        match object.get(field_name) {
            None => apply_input_default(schema, field, &mut coerced, path)?,
            Some(field_value) => {
                provided += 1;
                path.push(field_name.to_string());
                let result = coerce_input_value(schema, field_value, &field.ty, path);
                path.pop();
                coerced.insert(field_name.clone(), result?);
            }
        }
    }

    if input.one_of {
        check_one_of(&input.name, provided, &coerced, path)?;
    }

    Ok(Value::Object(Arc::new(coerced)))
}

fn apply_input_default(
    schema: &Schema,
    field: &InputValue,
    coerced: &mut Object,
    path: &mut Vec<String>,
) -> Result<(), GraphQLError> {
    if let Some(default) = &field.default_value {
        path.push(field.name.to_string());
        let value = coerce_input_value(schema, default, &field.ty, path);
        path.pop();
        coerced.insert(field.name.clone(), value?);
        Ok(())
    } else if field.ty.is_non_null() {
        Err(at_path(
            GraphQLError::new(format!(
                "Field \"{}\" of required type \"{}\" was not provided.",
                field.name, field.ty
            )),
            path,
        ))
    } else {
        Ok(())
    }
}

fn check_one_of(
    name: &str,
    provided: usize,
    coerced: &Object,
    path: &[String],
) -> Result<(), GraphQLError> {
    if provided != 1 {
        return Err(at_path(
            GraphQLError::new(format!(
                "OneOf input object \"{name}\" must specify exactly one field, but {provided} were provided."
            )),
            path,
        ));
    }
    if coerced.values().next().is_some_and(Value::is_null) {
        return Err(at_path(
            GraphQLError::new(format!(
                "OneOf input object \"{name}\" must not have a null value."
            )),
            path,
        ));
    }
    Ok(())
}

/// Coerce a document literal, returning `None` when a nested variable reference is undefined.
fn value_from_ast(
    schema: &Schema,
    value: &ast::Value<'_>,
    ty: &TypeRef,
    variables: Option<&Object>,
    path: &mut Vec<String>,
) -> Result<Option<Value>, GraphQLError> {
    match ty {
        TypeRef::NonNull(inner) => {
            if matches!(value, ast::Value::Null) {
                return Err(at_path(
                    GraphQLError::new(format!(
                        "Expected a value of non-null type \"{ty}\", but got null."
                    )),
                    path,
                ));
            }
            match value_from_ast(schema, value, inner, variables, path)? {
                Some(Value::Null) => Err(at_path(
                    GraphQLError::new(format!(
                        "Expected a value of non-null type \"{ty}\", but got null."
                    )),
                    path,
                )),
                other => Ok(other),
            }
        }
        _ => {
            if let ast::Value::Variable(name) = value {
                return Ok(variables.and_then(|variables| variables.get(name)).cloned());
            }
            if matches!(value, ast::Value::Null) {
                return Ok(Some(Value::Null));
            }
            match ty {
                TypeRef::List(inner) => coerce_list_literal(schema, value, inner, variables, path),
                TypeRef::Named(name) => coerce_named_literal(schema, value, name, variables, path),
                TypeRef::NonNull(_) => Err(at_path(
                    GraphQLError::new(
                        "Internal input coercion error: non-null type was not unwrapped.",
                    ),
                    path,
                )),
            }
        }
    }
}

fn coerce_list_literal(
    schema: &Schema,
    value: &ast::Value<'_>,
    inner: &TypeRef,
    variables: Option<&Object>,
    path: &mut Vec<String>,
) -> Result<Option<Value>, GraphQLError> {
    let ast::Value::List(items) = value else {
        // A single value coerces to a one-element list.
        return match value_from_ast(schema, value, inner, variables, path)? {
            Some(value) => Ok(Some(Value::List(vec![value]))),
            None => Ok(None),
        };
    };

    let mut coerced = Vec::with_capacity(items.len());
    for (index, item) in items.iter().enumerate() {
        path.push(index.to_string());
        let result = value_from_ast(schema, item, inner, variables, path);
        path.pop();
        match result? {
            Some(value) => coerced.push(value),
            None => {
                if inner.is_non_null() {
                    return Ok(None);
                }
                coerced.push(Value::Null);
            }
        }
    }
    Ok(Some(Value::List(coerced)))
}

fn coerce_named_literal(
    schema: &Schema,
    value: &ast::Value<'_>,
    name: &crate::types::Name,
    variables: Option<&Object>,
    path: &mut Vec<String>,
) -> Result<Option<Value>, GraphQLError> {
    let Some(definition) = schema.get_type(name) else {
        return Err(at_path(
            GraphQLError::new(format!("Unknown input type \"{name}\".")),
            path,
        ));
    };
    match definition {
        Type::Scalar(scalar) => coerce_scalar_literal(scalar, value, variables, path),
        Type::Enum(enumeration) => match value {
            ast::Value::Enum(value) => {
                if enumeration.has_value(value) {
                    Ok(Some(Value::Enum((*value).into())))
                } else {
                    Err(at_path(
                        GraphQLError::new(format!(
                            "Value \"{value}\" does not exist in enum \"{}\".",
                            enumeration.name
                        )),
                        path,
                    ))
                }
            }
            other => Err(at_path(
                GraphQLError::new(format!(
                    "Enum \"{}\" cannot represent a {} literal.",
                    enumeration.name,
                    literal_kind(other)
                )),
                path,
            )),
        },
        Type::InputObject(input) => {
            coerce_input_object_literal(schema, input, value, variables, path)
        }
        _ => Err(at_path(
            GraphQLError::new(format!("Type \"{name}\" is not an input type.")),
            path,
        )),
    }
}

fn coerce_scalar_literal(
    scalar: &ScalarType,
    value: &ast::Value<'_>,
    variables: Option<&Object>,
    path: &mut Vec<String>,
) -> Result<Option<Value>, GraphQLError> {
    if let Some(parse) = scalar.parse_literal_fn() {
        return parse(value)
            .map(Some)
            .map_err(|message| at_path(GraphQLError::new(message), path));
    }

    // Built-in scalars always register a literal parser, so this fallback belongs to custom
    // scalars: accept any JSON-shaped literal and let the scalar interpret it.
    let _ = &scalar.name;
    generic_literal(value, variables, path)
}

fn generic_literal(
    value: &ast::Value<'_>,
    variables: Option<&Object>,
    path: &mut Vec<String>,
) -> Result<Option<Value>, GraphQLError> {
    match value {
        ast::Value::Variable(name) => {
            Ok(variables.and_then(|variables| variables.get(name)).cloned())
        }
        ast::Value::Null => Ok(Some(Value::Null)),
        ast::Value::Boolean(value) => Ok(Some(Value::Boolean(*value))),
        ast::Value::Int(value) => Ok(Some(Value::Int(i32::try_from(*value).map_err(|_| {
            at_path(
                GraphQLError::new(format!(
                    "Int cannot represent non 32-bit signed integer value {value}."
                )),
                path,
            )
        })?))),
        ast::Value::Float(value) => Ok(Some(Value::Float(*value))),
        ast::Value::String(value) => Ok(Some(Value::String(Arc::from(value.as_ref())))),
        ast::Value::Enum(value) => Ok(Some(Value::Enum((*value).into()))),
        ast::Value::List(items) => {
            let mut coerced = Vec::with_capacity(items.len());
            for item in items {
                coerced.push(generic_literal(item, variables, path)?.unwrap_or(Value::Null));
            }
            Ok(Some(Value::List(coerced)))
        }
        ast::Value::Object(fields) => {
            let mut object = Object::with_capacity(fields.len());
            for field in fields {
                let value = generic_literal(&field.value, variables, path)?.unwrap_or(Value::Null);
                object.insert(field.name, value);
            }
            Ok(Some(Value::Object(Arc::new(object))))
        }
    }
}

fn coerce_input_object_literal(
    schema: &Schema,
    input: &InputObjectType,
    value: &ast::Value<'_>,
    variables: Option<&Object>,
    path: &mut Vec<String>,
) -> Result<Option<Value>, GraphQLError> {
    let ast::Value::Object(fields) = value else {
        return Err(at_path(
            GraphQLError::new(format!(
                "Expected an object literal of type \"{}\", but got a {} literal.",
                input.name,
                literal_kind(value)
            )),
            path,
        ));
    };

    for field in fields {
        if !input.fields.contains_key(field.name) {
            return Err(at_path(
                GraphQLError::new(format!(
                    "Field \"{}.{}\" is not defined by input object \"{}\".",
                    input.name, field.name, input.name
                )),
                path,
            ));
        }
    }

    let mut coerced = Object::with_capacity(input.fields.len());
    let mut provided = 0usize;
    for (field_name, field) in &input.fields {
        let node = fields
            .iter()
            .find(|candidate| candidate.name == &**field_name)
            .map(|candidate| &candidate.value);
        let missing_variable = node.is_some_and(|value| variable_is_missing(value, variables));

        if node.is_none() || missing_variable {
            if let Some(default) = &field.default_value {
                path.push(field_name.to_string());
                let value = coerce_input_value(schema, default, &field.ty, path);
                path.pop();
                coerced.insert(field_name.clone(), value?);
            } else if field.ty.is_non_null() {
                if missing_variable {
                    if let Some(ast::Value::Variable(name)) = node {
                        return Err(at_path(
                            GraphQLError::new(format!(
                                "Field \"{}\" of required type \"{}\" was not provided because variable \"${name}\" was not provided.",
                                field_name, field.ty
                            )),
                            path,
                        ));
                    }
                }
                return Err(at_path(
                    GraphQLError::new(format!(
                        "Field \"{}\" of required type \"{}\" was not provided.",
                        field_name, field.ty
                    )),
                    path,
                ));
            }
            continue;
        }

        let Some(node) = node else { continue };
        provided += 1;
        path.push(field_name.to_string());
        let result = value_from_ast(schema, node, &field.ty, variables, path);
        path.pop();
        match result? {
            Some(value) => {
                coerced.insert(field_name.clone(), value);
            }
            None => return Ok(None),
        }
    }

    if input.one_of {
        check_one_of(&input.name, provided, &coerced, path)?;
    }

    Ok(Some(Value::Object(Arc::new(coerced))))
}

fn literal_kind(value: &ast::Value<'_>) -> &'static str {
    match value {
        ast::Value::Variable(_) => "variable",
        ast::Value::Int(_) => "integer",
        ast::Value::Float(_) => "float",
        ast::Value::String(_) => "string",
        ast::Value::Boolean(_) => "boolean",
        ast::Value::Null => "null",
        ast::Value::Enum(_) => "enum",
        ast::Value::List(_) => "list",
        ast::Value::Object(_) => "object",
    }
}

fn render_value(value: &Value) -> String {
    match value {
        Value::Null => "null".to_owned(),
        Value::Boolean(value) => value.to_string(),
        Value::Int(value) => value.to_string(),
        Value::Float(value) => value.to_string(),
        Value::String(value) => format!("{value:?}"),
        Value::Enum(value) => value.to_string(),
        Value::List(_) => "[list]".to_owned(),
        Value::Object(_) => "{object}".to_owned(),
        Value::External(_) => "<model>".to_owned(),
    }
}
