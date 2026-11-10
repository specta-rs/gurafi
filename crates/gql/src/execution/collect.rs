//! Field collection.
//!
//! Implements the specification's `CollectFields` algorithm: evaluate `@skip`/`@include`, expand
//! fragment spreads and inline fragments, and group selections by response key so that fields
//! selected more than once are resolved once and their sub-selections merged.

use rustc_hash::{FxHashMap, FxHashSet};
use smallvec::SmallVec;

use crate::{
    Object, Value, ast,
    error::GraphQLError,
    schema::Schema,
    types::{ObjectType, names_eq},
};

use super::{executor::FragmentMap, selection::type_condition_matches};

/// One response key's field nodes, in document order, and the schema field they resolve to.
///
/// Resolving the definition while grouping means a list completes its items without looking the
/// same handful of fields up again for every element.
pub(crate) type FieldGroup<'a> = (
    &'a str,
    SmallVec<[&'a ast::Field<'a>; 1]>,
    Option<&'a crate::types::Field>,
);

/// Fields grouped by response key, preserving first-seen order.
///
/// Selection sets are usually tiny, so the outer list lives inline. Only pathological documents
/// spill to the heap.
pub(crate) type CollectedFields<'a> = SmallVec<[FieldGroup<'a>; 8]>;

/// Maximum number of selections collection may visit. A document can stay tiny while fragment
/// expansion makes it enormous; validation and complexity analysis bound this earlier, but
/// execution must not rely on them having run.
const MAX_COLLECT_WORK: u64 = 500_000;

/// Maximum fragment nesting during field collection.
const MAX_COLLECT_FRAGMENT_DEPTH: usize = 128;

/// Above this many distinct response keys a hash index beats a linear scan.
const LINEAR_INDEX_LIMIT: usize = 16;

/// The response-key index for one collection. Small selection sets are scanned linearly, which
/// avoids a hash map allocation for the common case; larger ones build the map on demand.
type FieldIndex<'a> = Option<FxHashMap<&'a str, usize>>;

/// Collect fields for one runtime type from a single selection set.
pub(crate) fn collect_fields<'a>(
    schema: &'a Schema,
    fragments: &FragmentMap<'a>,
    variables: &Object,
    object_type: &'a ObjectType,
    selection_set: &'a ast::SelectionSet<'a>,
) -> Result<CollectedFields<'a>, GraphQLError> {
    collect_fields_multi(
        schema,
        fragments,
        variables,
        object_type,
        std::slice::from_ref(&selection_set),
    )
}

/// Collect fields for one runtime type, merging several selection sets.
///
/// The specification's `CollectSubfields` merges every field node that shares a response key;
/// passing all of those nodes' selection sets here produces the same result without cloning the
/// syntax tree.
pub(crate) fn collect_fields_multi<'a>(
    schema: &'a Schema,
    fragments: &FragmentMap<'a>,
    variables: &Object,
    object_type: &'a ObjectType,
    selection_sets: &[&'a ast::SelectionSet<'a>],
) -> Result<CollectedFields<'a>, GraphQLError> {
    let mut fields = CollectedFields::new();
    let mut index: FieldIndex<'a> = None;
    let mut visited = VisitedFragments::new();
    let mut work = 0u64;
    for selection_set in selection_sets {
        collect_into(
            schema,
            fragments,
            variables,
            object_type,
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
    schema: &'a Schema,
    fragments: &FragmentMap<'a>,
    variables: &Object,
    object_type: &'a ObjectType,
    selection_set: &'a ast::SelectionSet<'a>,
    fields: &mut CollectedFields<'a>,
    index: &mut FieldIndex<'a>,
    visited: &mut VisitedFragments<'a>,
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
        if !should_include(selection.directives(), variables) {
            continue;
        }
        match selection {
            ast::Selection::Field(field) => {
                add_field(fields, index, object_type, field.response_key(), field);
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
                if !type_condition_matches(
                    schema,
                    fragment.type_condition,
                    object_type.name.as_ref(),
                )? {
                    continue;
                }
                collect_into(
                    schema,
                    fragments,
                    variables,
                    object_type,
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
                    if !type_condition_matches(schema, condition, object_type.name.as_ref())? {
                        continue;
                    }
                }
                collect_into(
                    schema,
                    fragments,
                    variables,
                    object_type,
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

/// Append a field node to its response key's group, creating the group if needed.
fn add_field<'a>(
    fields: &mut CollectedFields<'a>,
    index: &mut FieldIndex<'a>,
    object_type: &'a ObjectType,
    response_key: &'a str,
    field: &'a ast::Field<'a>,
) {
    if let Some(existing) = find_group(fields, index, response_key) {
        fields[existing].1.push(field);
        return;
    }
    let position = fields.len();
    fields.push((
        response_key,
        smallvec::smallvec![field],
        object_type.get_field(field.name),
    ));
    match index {
        Some(index) => {
            index.insert(response_key, position);
        }
        None if fields.len() > LINEAR_INDEX_LIMIT => {
            *index = Some(
                fields
                    .iter()
                    .enumerate()
                    .map(|(position, (key, _, _))| (*key, position))
                    .collect(),
            );
        }
        None => {}
    }
}

fn find_group<'a>(
    fields: &CollectedFields<'a>,
    index: &FieldIndex<'a>,
    response_key: &str,
) -> Option<usize> {
    match index {
        Some(index) => index.get(response_key).copied(),
        None => fields
            .iter()
            .position(|(key, _, _)| names_eq(key, response_key)),
    }
}

/// The set of fragment names expanded so far.
///
/// Documents expand only a handful of fragments, so the names live inline; a pathological
/// document with many spreads falls back to a hash set to keep `CollectFields` linear.
struct VisitedFragments<'a> {
    small: SmallVec<[&'a str; 8]>,
    large: Option<FxHashSet<&'a str>>,
}

impl<'a> VisitedFragments<'a> {
    fn new() -> Self {
        Self {
            small: SmallVec::new(),
            large: None,
        }
    }

    /// Insert a fragment name, returning whether it was not already present.
    fn insert(&mut self, name: &'a str) -> bool {
        if let Some(large) = &mut self.large {
            return large.insert(name);
        }
        if self.small.iter().any(|known| names_eq(known, name)) {
            return false;
        }
        self.small.push(name);
        if self.small.len() > LINEAR_INDEX_LIMIT {
            self.large = Some(self.small.iter().copied().collect());
        }
        true
    }
}

/// Evaluate `@skip` and `@include` for a selection.
///
/// A missing, null or non-boolean `if` value is treated as `false`, matching the complexity
/// analyser and the reference implementation's `getDirectiveValues` behaviour. Validation reports
/// malformed directive arguments before execution starts, so this only decides inclusion.
pub(crate) fn should_include(directives: &[ast::Directive<'_>], variables: &Object) -> bool {
    for directive in directives {
        match directive.name {
            "skip"
                if directive
                    .argument("if")
                    .is_some_and(|argument| boolean_value(&argument.value, variables)) =>
            {
                return false;
            }
            "include"
                if directive
                    .argument("if")
                    .is_some_and(|argument| !boolean_value(&argument.value, variables)) =>
            {
                return false;
            }
            // Unknown directives are a validation concern; execution ignores them.
            _ => {}
        }
    }
    true
}

fn boolean_value(value: &ast::Value<'_>, variables: &crate::Object) -> bool {
    match value {
        ast::Value::Boolean(value) => *value,
        ast::Value::Variable(name) => variables
            .get(name)
            .and_then(Value::as_bool)
            .unwrap_or(false),
        _ => false,
    }
}
