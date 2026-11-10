//! Variable rules.
//!
//! Variable usages are collected per operation, expanding fragment spreads, because a variable
//! used inside a fragment belongs to every operation that reaches that fragment.

use rustc_hash::FxHashMap as HashMap;

use smallvec::SmallVec;

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
        visitor::{directive_argument_info, field_argument_info, field_context},
    },
};

/// One place a variable is used, with the type expected there.
struct VariableUsage<'a> {
    name: &'a str,
    ty: Option<&'a TypeRef>,
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
    expected: Option<&'a TypeRef>,
    has_default: bool,
    location: Location,
    out: &mut Usages<'a>,
) {
    match value {
        Value::Variable(name) => out.push(VariableUsage {
            name,
            ty: expected,
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
                match input.and_then(|input| crate::types::map_get(&input.fields, field.name)) {
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

/// Variable usages found in one operation. Documents reference only a handful of variables, so
/// the list lives inline and only unusual documents spill to the heap.
type Usages<'a> = SmallVec<[VariableUsage<'a>; 16]>;

/// Variable definitions in document order.
///
/// Operations declare few variables, so a linear scan beats a map for the common case. A
/// document with many definitions keeps the same order but gains a hash index so lookups stay
/// linear.
struct Definitions<'a> {
    list: SmallVec<[&'a VariableDefinition<'a>; 8]>,
    index: Option<HashMap<&'a str, usize>>,
}

impl<'a> Definitions<'a> {
    fn new() -> Self {
        Self {
            list: SmallVec::new(),
            index: None,
        }
    }

    fn insert(&mut self, variable: &'a VariableDefinition<'a>) {
        if let Some(index) = &mut self.index {
            if index.contains_key(variable.name) {
                return;
            }
            index.insert(variable.name, self.list.len());
            self.list.push(variable);
            return;
        }
        if self.list.iter().any(|known| known.name == variable.name) {
            return;
        }
        self.list.push(variable);
        if self.list.len() > 8 {
            self.index = Some(
                self.list
                    .iter()
                    .enumerate()
                    .map(|(position, variable)| (variable.name, position))
                    .collect(),
            );
        }
    }

    fn get(&self, name: &str) -> Option<&'a VariableDefinition<'a>> {
        match &self.index {
            Some(index) => index.get(name).map(|position| self.list[*position]),
            None => self
                .list
                .iter()
                .find(|variable| variable.name == name)
                .copied(),
        }
    }

    fn iter(&self) -> impl Iterator<Item = &'a VariableDefinition<'a>> {
        self.list.iter().copied()
    }
}

type Names<'a> = crate::validation::NameSet<'a>;

fn collect_directive_usages<'a>(
    schema: &'a Schema,
    directives: &'a [crate::ast::Directive<'a>],
    out: &mut Usages<'a>,
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
                    Some(info.ty),
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
    visited: &mut Names<'a>,
    out: &mut Usages<'a>,
) {
    for selection in &selection_set.items {
        match selection {
            Selection::Field(field) => {
                // One context per field: its arguments and its child selection set both need it.
                let context = field_context(schema, options, parent, field);
                collect_field_usages(schema, &context, field, out);
                if let Some(child_set) = &field.selection_set {
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
    visited: &mut Names<'a>,
    out: &mut Usages<'a>,
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
    context: &crate::validation::visitor::FieldContext<'a>,
    field: &'a Field<'a>,
    out: &mut Usages<'a>,
) {
    for argument in &field.arguments {
        match field_argument_info(context, argument.name) {
            Some(info) => collect_value_usages(
                schema,
                &argument.value,
                Some(info.ty),
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
    let Some(location_type) = usage.ty else {
        return true;
    };

    if location_type.is_non_null() && !matches!(variable.ty, crate::ast::Type::NonNull(_)) {
        let has_non_null_default = variable
            .default_value
            .as_ref()
            .is_some_and(|value| !matches!(value, Value::Null));
        if !has_non_null_default && !usage.has_location_default {
            return false;
        }
        return types_compatible(&variable.ty, nullable(location_type));
    }

    types_compatible(&variable.ty, location_type)
}

/// Whether an AST variable type may be used where a schema type is expected.
///
/// Comparing against the AST directly avoids allocating a [`TypeRef`] just to ask this question.
fn types_compatible(variable: &crate::ast::Type<'_>, location: &TypeRef) -> bool {
    match (variable, location) {
        (crate::ast::Type::NonNull(variable), TypeRef::NonNull(location)) => {
            types_compatible(variable, location)
        }
        // A non-null variable may be used in a nullable position: `[Int!]!` is compatible with
        // `[Int!]`, and `Int!` is compatible with `Int`.
        (crate::ast::Type::NonNull(variable), location) => types_compatible(variable, location),
        (_, TypeRef::NonNull(_)) => false,
        (crate::ast::Type::List(variable), TypeRef::List(location)) => {
            types_compatible(variable, location)
        }
        (crate::ast::Type::List(_), _) | (_, TypeRef::List(_)) => false,
        (crate::ast::Type::Named(variable), TypeRef::Named(location)) => *variable == &**location,
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
        let mut definitions = Definitions::new();
        for variable in &operation.variables {
            // First definition wins; UniqueVariableNames reports the duplicates.
            definitions.insert(variable);
        }

        let mut usages = Usages::new();
        collect_directive_usages(schema, &operation.directives, &mut usages);
        collect_selection_usages(
            schema,
            options,
            fragments,
            root_type(schema, operation.kind),
            &operation.selection_set,
            &mut Names::new(),
            &mut usages,
        );

        let mut undefined = Names::new();
        for usage in &usages {
            if definitions.get(usage.name).is_none() && undefined.insert(usage.name) {
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

        for variable in definitions.iter() {
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

        let mut reported: SmallVec<[(&'a str, Location); 4]> = SmallVec::new();
        for usage in &usages {
            let Some(variable) = definitions.get(usage.name) else {
                continue;
            };
            if allowed_position(variable, usage) {
                continue;
            }
            let duplicate = reported
                .iter()
                .any(|(name, location)| *name == usage.name && *location == usage.location);
            if duplicate {
                continue;
            }
            reported.push((usage.name, usage.location));
            errors.push(
                GraphQLError::new(format!(
                    "Variable \"${}\" of type \"{}\" used in position expecting type \"{}\".",
                    usage.name,
                    variable.ty,
                    usage.ty.map_or_else(String::new, ToString::to_string)
                ))
                .at(usage.location),
            );
        }
    }
}
