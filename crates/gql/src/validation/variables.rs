//! Variable rules.
//!
//! Variable usages are collected per operation, expanding fragment spreads, because a variable
//! used inside a fragment belongs to every operation that reaches that fragment.

use std::collections::{HashMap, HashSet};

use crate::{
    ast::{
        Document, Field, FragmentDefinition, InlineFragment, Selection, SelectionSet, Value,
        VariableDefinition,
    },
    error::{GraphQLError, Location},
    schema::Schema,
    types::{Type, TypeRef},
    validation::{
        ValidationOptions, root_type,
        visitor::{directive_argument_info, field_argument_info, field_context, type_ref_from_ast},
    },
};

/// One place a variable is used, with the type expected there.
struct VariableUsage<'a> {
    name: &'a str,
    ty: Option<TypeRef>,
    location: Location,
    has_location_default: bool,
}

fn nullable(ty: &TypeRef) -> &TypeRef {
    match ty {
        TypeRef::NonNull(inner) => inner,
        other => other,
    }
}

fn list_item(ty: &TypeRef) -> Option<&TypeRef> {
    match nullable(ty) {
        TypeRef::List(item) => Some(item),
        _ => None,
    }
}

/// Collect variable references inside a literal value.
fn collect_value_usages<'a>(
    schema: &'a Schema,
    value: &'a Value<'a>,
    expected: Option<&TypeRef>,
    has_default: bool,
    location: Location,
    out: &mut Vec<VariableUsage<'a>>,
) {
    match value {
        Value::Variable(name) => out.push(VariableUsage {
            name,
            ty: expected.cloned(),
            location,
            has_location_default: has_default,
        }),
        Value::List(items) => {
            let item_type = expected.and_then(list_item);
            for item in items {
                // An argument or input-field default covers the value in that position, not the
                // items inside a list literal.
                collect_value_usages(schema, item, item_type, false, location, out);
            }
        }
        Value::Object(fields) => {
            let input = expected.and_then(|ty| match nullable(ty) {
                TypeRef::Named(name) => schema.input_object_type(name),
                _ => None,
            });
            for field in fields {
                match input.and_then(|input| input.fields.get(field.name)) {
                    Some(definition) => collect_value_usages(
                        schema,
                        &field.value,
                        Some(&definition.ty),
                        definition.default_value.is_some(),
                        field.location,
                        out,
                    ),
                    None => {
                        collect_value_usages(schema, &field.value, None, false, field.location, out)
                    }
                }
            }
        }
        Value::Int(_)
        | Value::Float(_)
        | Value::String(_)
        | Value::Boolean(_)
        | Value::Null
        | Value::Enum(_) => {}
    }
}

fn collect_directive_usages<'a>(
    schema: &'a Schema,
    directives: &'a [crate::ast::Directive<'a>],
    out: &mut Vec<VariableUsage<'a>>,
) {
    for directive in directives {
        let Some(definition) = schema.get_directive(directive.name) else {
            for argument in &directive.arguments {
                collect_value_usages(schema, &argument.value, None, false, argument.location, out);
            }
            continue;
        };
        for argument in &directive.arguments {
            match directive_argument_info(definition, argument.name) {
                Some(info) => collect_value_usages(
                    schema,
                    &argument.value,
                    Some(&info.ty),
                    info.has_default,
                    argument.location,
                    out,
                ),
                None => collect_value_usages(
                    schema,
                    &argument.value,
                    None,
                    false,
                    argument.location,
                    out,
                ),
            }
        }
    }
}

fn collect_selection_usages<'a>(
    schema: &'a Schema,
    options: &ValidationOptions,
    fragments: &HashMap<&'a str, &'a FragmentDefinition<'a>>,
    parent: Option<&'a Type>,
    selection_set: &'a SelectionSet<'a>,
    visited: &mut HashSet<&'a str>,
    out: &mut Vec<VariableUsage<'a>>,
) {
    for selection in &selection_set.items {
        match selection {
            Selection::Field(field) => {
                collect_field_usages(schema, options, parent, field, out);
                if let Some(child_set) = &field.selection_set {
                    let context = field_context(schema, options, parent, field);
                    let child = context
                        .return_type
                        .as_ref()
                        .and_then(|ty| schema.get_type(ty.named_type()))
                        .filter(|ty| ty.is_composite());
                    collect_selection_usages(
                        schema, options, fragments, child, child_set, visited, out,
                    );
                }
            }
            Selection::InlineFragment(fragment) => {
                collect_inline_usages(schema, options, fragments, parent, fragment, visited, out);
            }
            Selection::FragmentSpread(spread) => {
                collect_directive_usages(schema, &spread.directives, out);
                let Some(fragment) = fragments.get(spread.name) else {
                    continue;
                };
                if !visited.insert(spread.name) {
                    continue;
                }
                collect_directive_usages(schema, &fragment.directives, out);
                let condition = schema
                    .get_type(fragment.type_condition)
                    .filter(|ty| ty.is_composite());
                collect_selection_usages(
                    schema,
                    options,
                    fragments,
                    condition,
                    &fragment.selection_set,
                    visited,
                    out,
                );
            }
        }
    }
}

fn collect_inline_usages<'a>(
    schema: &'a Schema,
    options: &ValidationOptions,
    fragments: &HashMap<&'a str, &'a FragmentDefinition<'a>>,
    parent: Option<&'a Type>,
    fragment: &'a InlineFragment<'a>,
    visited: &mut HashSet<&'a str>,
    out: &mut Vec<VariableUsage<'a>>,
) {
    collect_directive_usages(schema, &fragment.directives, out);
    let condition = fragment
        .type_condition
        .and_then(|name| schema.get_type(name))
        .filter(|ty| ty.is_composite())
        .or(parent);
    collect_selection_usages(
        schema,
        options,
        fragments,
        condition,
        &fragment.selection_set,
        visited,
        out,
    );
}

fn collect_field_usages<'a>(
    schema: &'a Schema,
    options: &ValidationOptions,
    parent: Option<&'a Type>,
    field: &'a Field<'a>,
    out: &mut Vec<VariableUsage<'a>>,
) {
    let context = field_context(schema, options, parent, field);
    for argument in &field.arguments {
        match field_argument_info(&context, argument.name) {
            Some(info) => collect_value_usages(
                schema,
                &argument.value,
                Some(&info.ty),
                info.has_default,
                argument.location,
                out,
            ),
            None => {
                collect_value_usages(schema, &argument.value, None, false, argument.location, out)
            }
        }
    }
    collect_directive_usages(schema, &field.directives, out);
}

/// Whether a variable declared as `variable` may be used where `location` is expected.
fn allowed_position(variable: &VariableDefinition<'_>, usage: &VariableUsage<'_>) -> bool {
    let Some(location_type) = usage.ty.as_ref() else {
        return true;
    };
    let variable_type = type_ref_from_ast(&variable.ty);

    if location_type.is_non_null() && !variable_type.is_non_null() {
        let has_non_null_default = variable
            .default_value
            .as_ref()
            .is_some_and(|value| !matches!(value, Value::Null));
        if !has_non_null_default && !usage.has_location_default {
            return false;
        }
        return types_compatible(&variable_type, nullable(location_type));
    }

    types_compatible(&variable_type, location_type)
}

fn types_compatible(variable: &TypeRef, location: &TypeRef) -> bool {
    match (variable, location) {
        (TypeRef::NonNull(variable), TypeRef::NonNull(location)) => {
            types_compatible(variable, location)
        }
        // A non-null variable may be used in a nullable position: `[Int!]!` is compatible with
        // `[Int!]`, and `Int!` is compatible with `Int`.
        (TypeRef::NonNull(variable), location) => types_compatible(variable, location),
        (_, TypeRef::NonNull(_)) => false,
        (TypeRef::List(variable), TypeRef::List(location)) => types_compatible(variable, location),
        (TypeRef::List(_), _) | (_, TypeRef::List(_)) => false,
        (TypeRef::Named(variable), TypeRef::Named(location)) => variable == location,
    }
}

/// Run every rule that depends on an operation's variable definitions.
pub(crate) fn check_variables<'a>(
    schema: &'a Schema,
    options: &'a ValidationOptions,
    fragments: &HashMap<&'a str, &'a FragmentDefinition<'a>>,
    document: &'a Document<'a>,
    errors: &mut Vec<GraphQLError>,
) {
    for operation in document.operations() {
        let mut definitions: HashMap<&'a str, &'a VariableDefinition<'a>> = HashMap::new();
        for variable in &operation.variables {
            definitions.entry(variable.name).or_insert(variable);
        }

        let mut usages = Vec::new();
        collect_directive_usages(schema, &operation.directives, &mut usages);
        collect_selection_usages(
            schema,
            options,
            fragments,
            root_type(schema, operation.kind),
            &operation.selection_set,
            &mut HashSet::new(),
            &mut usages,
        );

        let mut undefined = HashSet::new();
        for usage in &usages {
            if !definitions.contains_key(usage.name) && undefined.insert(usage.name) {
                let message = match operation.name {
                    Some(name) => format!(
                        "Variable \"${}\" is not defined by operation \"{name}\".",
                        usage.name
                    ),
                    None => format!("Variable \"${}\" is not defined.", usage.name),
                };
                errors.push(GraphQLError::new(message).at(usage.location));
            }
        }

        for variable in definitions.values() {
            if !usages.iter().any(|usage| usage.name == variable.name) {
                let message = match operation.name {
                    Some(name) => format!(
                        "Variable \"${}\" is never used in operation \"{name}\".",
                        variable.name
                    ),
                    None => format!("Variable \"${}\" is never used.", variable.name),
                };
                errors.push(GraphQLError::new(message).at(variable.location));
            }
        }

        let mut reported = HashSet::new();
        for usage in &usages {
            let Some(variable) = definitions.get(usage.name) else {
                continue;
            };
            if allowed_position(variable, usage) {
                continue;
            }
            if !reported.insert((usage.name, usage.location)) {
                continue;
            }
            errors.push(
                GraphQLError::new(format!(
                    "Variable \"${}\" of type \"{}\" used in position expecting type \"{}\".",
                    usage.name,
                    variable.ty,
                    usage
                        .ty
                        .as_ref()
                        .map_or_else(String::new, ToString::to_string)
                ))
                .at(usage.location),
            );
        }
    }
}
