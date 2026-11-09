//! Field collection.
//!
//! Implements the specification's `CollectFields` algorithm: evaluate `@skip`/`@include`, expand
//! fragment spreads and inline fragments, and group selections by response key so that fields
//! selected more than once are resolved once and their sub-selections merged.

use std::collections::HashMap;

use rustc_hash::{FxHashMap, FxHashSet};

use crate::{Object, Value, ast, error::GraphQLError, schema::Schema};

use super::selection::type_condition_matches;

/// Fields grouped by response key, preserving first-seen order.
pub(crate) type CollectedFields<'a> = Vec<(&'a str, Vec<&'a ast::Field<'a>>)>;

/// Maximum number of selections collection may visit. A document can stay tiny while fragment
/// expansion makes it enormous; validation and complexity analysis bound this earlier, but
/// execution must not rely on them having run.
const MAX_COLLECT_WORK: u64 = 500_000;

/// Maximum fragment nesting during field collection.
const MAX_COLLECT_FRAGMENT_DEPTH: usize = 128;

/// Collect fields for one runtime type from a single selection set.
pub(crate) fn collect_fields<'a>(
    schema: &Schema,
    fragments: &HashMap<&'a str, &'a ast::FragmentDefinition<'a>>,
    variables: &Object,
    runtime_type: &str,
    selection_set: &'a ast::SelectionSet<'a>,
) -> Result<CollectedFields<'a>, GraphQLError> {
    collect_fields_multi(
        schema,
        fragments,
        variables,
        runtime_type,
        std::slice::from_ref(&selection_set),
    )
}

/// Collect fields for one runtime type, merging several selection sets.
///
/// The specification's `CollectSubfields` merges every field node that shares a response key;
/// passing all of those nodes' selection sets here produces the same result without cloning the
/// syntax tree.
pub(crate) fn collect_fields_multi<'a>(
    schema: &Schema,
    fragments: &HashMap<&'a str, &'a ast::FragmentDefinition<'a>>,
    variables: &Object,
    runtime_type: &str,
    selection_sets: &[&'a ast::SelectionSet<'a>],
) -> Result<CollectedFields<'a>, GraphQLError> {
    let mut fields = Vec::new();
    let mut index = FxHashMap::default();
    let mut visited = FxHashSet::default();
    let mut work = 0u64;
    for selection_set in selection_sets {
        collect_into(
            schema,
            fragments,
            variables,
            runtime_type,
            selection_set,
            &mut fields,
            &mut index,
            &mut visited,
            0,
            &mut work,
        )?;
    }
    Ok(fields)
}

#[allow(clippy::too_many_arguments)]
fn collect_into<'a>(
    schema: &Schema,
    fragments: &HashMap<&'a str, &'a ast::FragmentDefinition<'a>>,
    variables: &Object,
    runtime_type: &str,
    selection_set: &'a ast::SelectionSet<'a>,
    fields: &mut CollectedFields<'a>,
    index: &mut FxHashMap<&'a str, usize>,
    visited: &mut FxHashSet<&'a str>,
    depth: usize,
    work: &mut u64,
) -> Result<(), GraphQLError> {
    for selection in &selection_set.items {
        *work = work.saturating_add(1);
        if *work > MAX_COLLECT_WORK {
            return Err(GraphQLError::new(
                "The query expands to too many fields to execute safely.",
            ));
        }
        if !should_include(selection.directives(), variables)? {
            continue;
        }
        match selection {
            ast::Selection::Field(field) => {
                let response_key = field.response_key();
                if let Some(existing) = index.get(response_key) {
                    if let Some(group) = fields.get_mut(*existing) {
                        group.1.push(field);
                    }
                } else {
                    index.insert(response_key, fields.len());
                    fields.push((response_key, vec![field]));
                }
            }
            ast::Selection::FragmentSpread(spread) => {
                let Some(fragment) = fragments.get(spread.name) else {
                    return Err(GraphQLError::new(format!(
                        "Unknown fragment \"{}\".",
                        spread.name
                    ))
                    .at(spread.location));
                };
                // The specification's `visitedFragments` set is shared for the whole collection:
                // spreading the same fragment twice must not expand it twice.
                if !visited.insert(spread.name) {
                    continue;
                }
                if depth >= MAX_COLLECT_FRAGMENT_DEPTH {
                    return Err(GraphQLError::new(format!(
                        "Fragment nesting exceeds the maximum of {MAX_COLLECT_FRAGMENT_DEPTH}."
                    ))
                    .at(spread.location));
                }
                if !type_condition_matches(schema, fragment.type_condition, runtime_type)? {
                    continue;
                }
                collect_into(
                    schema,
                    fragments,
                    variables,
                    runtime_type,
                    &fragment.selection_set,
                    fields,
                    index,
                    visited,
                    depth + 1,
                    work,
                )?;
            }
            ast::Selection::InlineFragment(fragment) => {
                if let Some(condition) = fragment.type_condition {
                    if !type_condition_matches(schema, condition, runtime_type)? {
                        continue;
                    }
                }
                collect_into(
                    schema,
                    fragments,
                    variables,
                    runtime_type,
                    &fragment.selection_set,
                    fields,
                    index,
                    visited,
                    depth,
                    work,
                )?;
            }
        }
    }
    Ok(())
}

/// Evaluate `@skip` and `@include` for a selection.
pub(crate) fn should_include(
    directives: &[ast::Directive<'_>],
    variables: &Object,
) -> Result<bool, GraphQLError> {
    for directive in directives {
        match directive.name {
            "skip" | "include" => {
                let Some(argument) = directive.argument("if") else {
                    return Err(GraphQLError::new(format!(
                        "Directive \"@{}\" requires an \"if\" argument.",
                        directive.name
                    ))
                    .at(directive.location));
                };
                let value = boolean_value(&argument.value, variables).map_err(|message| {
                    GraphQLError::new(format!("Directive \"@{}\": {message}", directive.name))
                        .at(argument.location)
                })?;
                if directive.name == "skip" && value {
                    return Ok(false);
                }
                if directive.name == "include" && !value {
                    return Ok(false);
                }
            }
            // Unknown directives are a validation concern; execution ignores them.
            _ => {}
        }
    }
    Ok(true)
}

fn boolean_value(value: &ast::Value<'_>, variables: &Object) -> Result<bool, String> {
    match value {
        ast::Value::Boolean(value) => Ok(*value),
        ast::Value::Variable(name) => match variables.get(name) {
            Some(Value::Boolean(value)) => Ok(*value),
            Some(value) => Err(format!(
                "variable \"${name}\" must be a Boolean, but it is a {}.",
                value.kind()
            )),
            None => Err(format!("variable \"${name}\" was not provided.")),
        },
        _ => Err("\"if\" must be a Boolean.".to_owned()),
    }
}
