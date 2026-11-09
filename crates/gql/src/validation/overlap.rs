//! `OverlappingFieldsCanBeMerged`.
//!
//! Two fields may share a response key only when they are the same field selected with the same
//! arguments and compatible types. Fields coming from fragments are only compared when their
//! parent types can occur together, and comparisons are memoised so fragments shared by many
//! operations are not rechecked exponentially often.

use std::collections::{HashMap, HashSet};

use smallvec::SmallVec;

use crate::{
    ast::{Argument, Document, Field, FragmentDefinition, Selection, SelectionSet, Value},
    error::{GraphQLError, Location},
    schema::Schema,
    types::{Type, TypeRef},
    validation::{ValidationOptions, rules::types_overlap, visitor::field_context},
};

#[derive(Clone, Copy)]
struct Entry<'a> {
    parent: &'a Type,
    field: &'a Field<'a>,
    return_type: &'a TypeRef,
}

/// Entries found in one selection set. Selection sets are usually tiny, so the list lives
/// inline and only pathological documents spill to the heap.
type Entries<'a> = SmallVec<[Entry<'a>; 8]>;

/// One response key's entries, in document order.
type Group<'a> = (&'a str, SmallVec<[Entry<'a>; 4]>);

/// Above this many response keys a hash index beats a linear scan.
const LINEAR_INDEX_LIMIT: usize = 16;

/// Group entries by response key, preserving first-seen order.
fn group_entries<'a>(entries: Entries<'a>) -> SmallVec<[Group<'a>; 8]> {
    let mut groups: SmallVec<[Group<'a>; 8]> = SmallVec::new();
    let mut index: Option<HashMap<&'a str, usize>> = None;
    for entry in entries {
        let key = entry.field.response_key();
        if let Some(index) = &mut index {
            if let Some(position) = index.get(key).copied() {
                groups[position].1.push(entry);
            } else {
                index.insert(key, groups.len());
                groups.push((key, smallvec::smallvec![entry]));
            }
            continue;
        }
        if let Some(position) = groups.iter().position(|(known, _)| *known == key) {
            groups[position].1.push(entry);
            continue;
        }
        groups.push((key, smallvec::smallvec![entry]));
        if groups.len() > LINEAR_INDEX_LIMIT {
            index = Some(
                groups
                    .iter()
                    .enumerate()
                    .map(|(position, (key, _))| (*key, position))
                    .collect(),
            );
        }
    }
    groups
}

type Visited<'a> = crate::validation::NameSet<'a>;

struct State<'a, 'b> {
    schema: &'a Schema,
    options: &'b ValidationOptions,
    fragments: &'b HashMap<&'a str, &'a FragmentDefinition<'a>>,
    errors: &'b mut Vec<GraphQLError>,
    /// Field pairs already compared, keyed by location and parent type.
    compared: HashSet<(Location, Location, &'a str, &'a str)>,
    /// Conflicts already reported, to avoid duplicates when the same selection set is reached
    /// through more than one fragment.
    reported: HashSet<(String, Location, Location)>,
}

impl<'a> State<'a, '_> {
    fn collect(
        &self,
        parent: &'a Type,
        selection_set: &'a SelectionSet<'a>,
        visited: &mut Visited<'a>,
        out: &mut Entries<'a>,
    ) {
        for selection in &selection_set.items {
            match selection {
                Selection::Field(field) => {
                    let context = field_context(self.schema, self.options, Some(parent), field);
                    if let Some(return_type) = context.return_type {
                        out.push(Entry {
                            parent,
                            field,
                            return_type,
                        });
                    }
                }
                Selection::InlineFragment(fragment) => {
                    let condition = fragment
                        .type_condition
                        .and_then(|name| self.schema.get_type(name))
                        .filter(|ty| ty.is_composite())
                        .unwrap_or(parent);
                    self.collect(condition, &fragment.selection_set, visited, out);
                }
                Selection::FragmentSpread(spread) => {
                    let Some(fragment) = self.fragments.get(spread.name) else {
                        continue;
                    };
                    // A selection set only needs one expansion of a given fragment, even when
                    // it is spread more than once.
                    if !visited.insert(spread.name) {
                        continue;
                    }
                    let condition = self
                        .schema
                        .get_type(fragment.type_condition)
                        .filter(|ty| ty.is_composite())
                        .unwrap_or(parent);
                    self.collect(condition, &fragment.selection_set, visited, out);
                }
            }
        }
    }

    fn named_composite(&self, ty: &TypeRef) -> Option<&'a Type> {
        self.schema
            .get_type(ty.named_type())
            .filter(|ty| ty.is_composite())
    }

    fn types_conflict(&self, left: &TypeRef, right: &TypeRef) -> bool {
        match (left, right) {
            (TypeRef::NonNull(left), TypeRef::NonNull(right)) => self.types_conflict(left, right),
            (TypeRef::NonNull(_), _) | (_, TypeRef::NonNull(_)) => true,
            (TypeRef::List(left), TypeRef::List(right)) => self.types_conflict(left, right),
            (TypeRef::List(_), _) | (_, TypeRef::List(_)) => true,
            (TypeRef::Named(left), TypeRef::Named(right)) => {
                if left == right {
                    return false;
                }
                let left_is_leaf = self.schema.get_type(left).is_some_and(|ty| ty.is_leaf());
                let right_is_leaf = self.schema.get_type(right).is_some_and(|ty| ty.is_leaf());
                left_is_leaf || right_is_leaf
            }
        }
    }

    fn compare(&mut self, response_key: &str, left: &Entry<'a>, right: &Entry<'a>) {
        if !types_overlap(self.schema, left.parent.name(), right.parent.name()) {
            return;
        }

        let (first, second) = if left.field.location <= right.field.location {
            (left, right)
        } else {
            (right, left)
        };
        if !self.compared.insert((
            first.field.location,
            second.field.location,
            first.parent.name().as_ref(),
            second.parent.name().as_ref(),
        )) {
            return;
        }

        let reason = if left.field.name != right.field.name {
            Some(format!(
                "they have differing field names (\"{}\" and \"{}\")",
                left.field.name, right.field.name
            ))
        } else if !same_arguments(&left.field.arguments, &right.field.arguments) {
            Some("they have differing arguments".to_owned())
        } else if self.types_conflict(left.return_type, right.return_type) {
            Some(format!(
                "they return conflicting types \"{}\" and \"{}\"",
                left.return_type, right.return_type
            ))
        } else {
            None
        };

        if let Some(reason) = reason {
            self.report(response_key, first, second, &reason);
            return;
        }

        let (Some(left_parent), Some(right_parent)) = (
            self.named_composite(left.return_type),
            self.named_composite(right.return_type),
        ) else {
            return;
        };

        let mut left_entries = Entries::new();
        if let Some(selection_set) = &left.field.selection_set {
            self.collect(
                left_parent,
                selection_set,
                &mut Visited::new(),
                &mut left_entries,
            );
        }
        let mut right_entries = Entries::new();
        if let Some(selection_set) = &right.field.selection_set {
            self.collect(
                right_parent,
                selection_set,
                &mut Visited::new(),
                &mut right_entries,
            );
        }

        let left_groups = group_entries(left_entries);
        let right_groups = group_entries(right_entries);

        for (key, left_group) in &left_groups {
            let Some((_, right_group)) = right_groups.iter().find(|(known, _)| known == key) else {
                continue;
            };
            for left_entry in left_group {
                for right_entry in right_group {
                    self.compare(key, left_entry, right_entry);
                }
            }
        }
    }

    fn report(&mut self, response_key: &str, first: &Entry<'a>, second: &Entry<'a>, reason: &str) {
        if !self.reported.insert((
            response_key.to_owned(),
            first.field.location,
            second.field.location,
        )) {
            return;
        }
        self.errors.push(
            GraphQLError::new(format!(
                "Fields \"{response_key}\" conflict because {reason}. Use different aliases on the fields to fetch both if this was intentional."
            ))
            .at(first.field.location)
            .at(second.field.location),
        );
    }

    fn check_selection_set(&mut self, parent: &'a Type, selection_set: &'a SelectionSet<'a>) {
        let mut entries = Entries::new();
        self.collect(parent, selection_set, &mut Visited::new(), &mut entries);

        let groups = group_entries(entries);
        for (key, group) in &groups {
            for left in 0..group.len() {
                for right in (left + 1)..group.len() {
                    self.compare(key, &group[left], &group[right]);
                }
            }
        }

        // Nested selection sets are compared in their own right. Fragment definitions are
        // checked separately, so spreads are not descended into here.
        for selection in &selection_set.items {
            match selection {
                Selection::Field(field) => {
                    if let Some(child_set) = &field.selection_set {
                        let context = field_context(self.schema, self.options, Some(parent), field);
                        if let Some(child) = context
                            .return_type
                            .as_ref()
                            .and_then(|ty| self.schema.get_type(ty.named_type()))
                            .filter(|ty| ty.is_composite())
                        {
                            self.check_selection_set(child, child_set);
                        }
                    }
                }
                Selection::InlineFragment(fragment) => {
                    let condition = fragment
                        .type_condition
                        .and_then(|name| self.schema.get_type(name))
                        .filter(|ty| ty.is_composite())
                        .unwrap_or(parent);
                    self.check_selection_set(condition, &fragment.selection_set);
                }
                Selection::FragmentSpread(_) => {}
            }
        }
    }
}

/// Whether two argument lists are equivalent, ignoring order.
fn same_arguments(left: &[Argument<'_>], right: &[Argument<'_>]) -> bool {
    if left.len() != right.len() {
        return false;
    }
    left.iter().all(|argument| {
        right
            .iter()
            .find(|other| other.name == argument.name)
            .is_some_and(|other| same_value(&argument.value, &other.value))
    })
}

fn same_value(left: &Value<'_>, right: &Value<'_>) -> bool {
    match (left, right) {
        (Value::Variable(left), Value::Variable(right)) => left == right,
        (Value::Int(left), Value::Int(right)) => left == right,
        (Value::Float(left), Value::Float(right)) => left == right,
        (Value::String(left), Value::String(right)) => left == right,
        (Value::Boolean(left), Value::Boolean(right)) => left == right,
        (Value::Null, Value::Null) => true,
        (Value::Enum(left), Value::Enum(right)) => left == right,
        (Value::List(left), Value::List(right)) => {
            left.len() == right.len()
                && left
                    .iter()
                    .zip(right)
                    .all(|(left, right)| same_value(left, right))
        }
        (Value::Object(left), Value::Object(right)) => {
            left.len() == right.len()
                && left.iter().all(|field| {
                    right
                        .iter()
                        .find(|other| other.name == field.name)
                        .is_some_and(|other| same_value(&field.value, &other.value))
                })
        }
        _ => false,
    }
}

/// Validate `OverlappingFieldsCanBeMerged` for every operation and fragment definition.
pub(crate) fn check<'a>(
    schema: &'a Schema,
    options: &ValidationOptions,
    fragments: &HashMap<&'a str, &'a FragmentDefinition<'a>>,
    document: &'a Document<'a>,
    errors: &mut Vec<GraphQLError>,
) {
    let mut state = State {
        schema,
        options,
        fragments,
        errors,
        compared: HashSet::new(),
        reported: HashSet::new(),
    };

    for operation in document.operations() {
        let Some(root) = crate::validation::root_type(schema, operation.kind) else {
            continue;
        };
        state.check_selection_set(root, &operation.selection_set);
    }

    for fragment in document.fragments() {
        if let Some(condition) = schema
            .get_type(fragment.type_condition)
            .filter(|ty| ty.is_composite())
        {
            state.check_selection_set(condition, &fragment.selection_set);
        }
    }
}
