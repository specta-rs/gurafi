//! Document validation.
//!
//! Validation implements every rule for executable documents in the GraphQL October 2021
//! specification. It runs in a handful of passes so that each pass can stay small and so that a
//! field is checked once even when many operations spread the same fragment.
//!
//! The entry point is [`validate`]. Validation is read-only: it never executes a resolver and
//! never mutates the schema or document.

mod overlap;
mod rules;
mod values;
mod variables;
mod visitor;

use std::collections::{HashMap, HashSet};

use crate::{
    ast::{
        Definition, DirectiveLocation, Document, Field, FragmentDefinition, OperationDefinition,
        OperationKind, Selection, SelectionSet, TypeSystemDefinition, VariableDefinition,
    },
    error::{GraphQLError, Location},
    schema::Schema,
    types::Type,
};
use rules::{FieldRules, check_directives};
use visitor::{type_ref_from_ast, walk};

/// Options for document validation.
#[derive(Clone, Debug, Default)]
pub struct ValidationOptions {
    /// Whether introspection fields are permitted.
    pub introspection: bool,
    /// An optional maximum selection-set depth. Zero disables the limit.
    ///
    /// The parser independently caps nesting at [`crate::parser::MAX_DEPTH`].
    pub max_depth: usize,
    /// Maximum number of errors to collect. Zero disables the limit.
    pub max_errors: usize,
}

/// Validate an executable document against a schema.
///
/// Returns every violation found. Validation never executes resolvers.
#[must_use]
pub fn validate<'a>(
    schema: &'a Schema,
    document: &'a Document<'a>,
    options: &'a ValidationOptions,
) -> Vec<GraphQLError> {
    // A very long fragment chain is shallow in the document but deep once expanded. Stop before
    // any of the recursive validation passes can exhaust the stack; the document is invalid
    // regardless of its other contents.
    let fragments: HashMap<&str, &FragmentDefinition<'_>> = document
        .fragments()
        .map(|fragment| (fragment.name, fragment))
        .collect();
    if let Some(error) = check_fragment_chain(&fragments) {
        return vec![error];
    }

    let mut validator = Validator::new(schema, document, options);
    validator.run();
    if options.max_errors != 0 {
        validator.errors.truncate(options.max_errors);
    }
    validator.errors
}

/// Maximum number of fragments that may be expanded before the document is rejected.
const MAX_FRAGMENT_CHAIN: usize = 128;

fn check_fragment_chain<'a>(
    fragments: &HashMap<&'a str, &'a FragmentDefinition<'a>>,
) -> Option<GraphQLError> {
    fn visit<'a>(
        fragments: &HashMap<&'a str, &'a FragmentDefinition<'a>>,
        name: &'a str,
        depth: usize,
        path: &mut Vec<&'a str>,
        seen: &mut HashMap<&'a str, usize>,
    ) -> Option<GraphQLError> {
        if depth > MAX_FRAGMENT_CHAIN {
            return Some(GraphQLError::new(format!(
                "Fragment nesting exceeds the maximum of {MAX_FRAGMENT_CHAIN}."
            )));
        }
        if path.contains(&name) {
            return None;
        }
        if seen.get(name).is_some_and(|previous| *previous >= depth) {
            return None;
        }
        seen.insert(name, depth);
        let fragment = fragments.get(name)?;
        path.push(name);
        let mut spreads = Vec::new();
        collect_spreads(&fragment.selection_set, &mut spreads);
        for spread in spreads {
            if let Some(error) = visit(fragments, spread, depth + 1, path, seen) {
                return Some(error);
            }
        }
        path.pop();
        None
    }

    for fragment in fragments.values() {
        let mut seen = HashMap::new();
        if let Some(error) = visit(fragments, fragment.name, 1, &mut Vec::new(), &mut seen) {
            return Some(error);
        }
    }
    None
}

/// The root type an operation selects from, when the schema defines it.
pub(crate) fn root_type(schema: &Schema, kind: OperationKind) -> Option<&Type> {
    match kind {
        OperationKind::Query => schema.get_type(schema.query_name()),
        OperationKind::Mutation => schema
            .mutation_name()
            .and_then(|name| schema.get_type(name)),
        OperationKind::Subscription => schema
            .subscription_name()
            .and_then(|name| schema.get_type(name)),
    }
}

fn directive_location(kind: OperationKind) -> DirectiveLocation {
    match kind {
        OperationKind::Query => DirectiveLocation::Query,
        OperationKind::Mutation => DirectiveLocation::Mutation,
        OperationKind::Subscription => DirectiveLocation::Subscription,
    }
}

fn is_introspection_field(name: &str) -> bool {
    name.starts_with("__")
}

fn collect_spreads<'a>(selection_set: &'a SelectionSet<'a>, out: &mut Vec<&'a str>) {
    for selection in &selection_set.items {
        match selection {
            Selection::Field(field) => {
                if let Some(child) = &field.selection_set {
                    collect_spreads(child, out);
                }
            }
            Selection::InlineFragment(fragment) => {
                collect_spreads(&fragment.selection_set, out);
            }
            Selection::FragmentSpread(spread) => out.push(spread.name),
        }
    }
}

fn collect_top_level_fields<'a>(
    fragments: &HashMap<&'a str, &'a FragmentDefinition<'a>>,
    selection_set: &'a SelectionSet<'a>,
    visited: &mut HashSet<&'a str>,
    out: &mut Vec<&'a Field<'a>>,
) {
    for selection in &selection_set.items {
        match selection {
            Selection::Field(field) => out.push(field),
            Selection::InlineFragment(fragment) => {
                collect_top_level_fields(fragments, &fragment.selection_set, visited, out);
            }
            Selection::FragmentSpread(spread) => {
                let Some(fragment) = fragments.get(spread.name) else {
                    continue;
                };
                if !visited.insert(spread.name) {
                    continue;
                }
                collect_top_level_fields(fragments, &fragment.selection_set, visited, out);
            }
        }
    }
}

fn collect_spread_locations<'a>(
    selection_set: &'a SelectionSet<'a>,
    out: &mut Vec<(&'a str, Location)>,
) {
    for selection in &selection_set.items {
        match selection {
            Selection::Field(field) => {
                if let Some(child) = &field.selection_set {
                    collect_spread_locations(child, out);
                }
            }
            Selection::InlineFragment(fragment) => {
                collect_spread_locations(&fragment.selection_set, out);
            }
            Selection::FragmentSpread(spread) => out.push((spread.name, spread.location)),
        }
    }
}

fn selection_depth<'a>(
    fragments: &HashMap<&'a str, &'a FragmentDefinition<'a>>,
    selection_set: &'a SelectionSet<'a>,
    visited: &mut HashSet<&'a str>,
    depth: usize,
) -> usize {
    let mut max = depth;
    for selection in &selection_set.items {
        match selection {
            Selection::Field(field) => {
                if let Some(child) = &field.selection_set {
                    max = max.max(selection_depth(fragments, child, visited, depth + 1));
                }
            }
            Selection::InlineFragment(fragment) => {
                max = max.max(selection_depth(
                    fragments,
                    &fragment.selection_set,
                    visited,
                    depth,
                ));
            }
            Selection::FragmentSpread(spread) => {
                if !visited.insert(spread.name) {
                    continue;
                }
                if let Some(fragment) = fragments.get(spread.name) {
                    max = max.max(selection_depth(
                        fragments,
                        &fragment.selection_set,
                        visited,
                        depth,
                    ));
                }
            }
        }
    }
    max
}

fn detect_cycle<'a>(
    fragments: &HashMap<&'a str, &'a FragmentDefinition<'a>>,
    name: &'a str,
    path: &mut Vec<&'a str>,
    visited: &mut HashSet<&'a str>,
    errors: &mut Vec<GraphQLError>,
) {
    if visited.contains(name) || path.contains(&name) {
        return;
    }
    let Some(fragment) = fragments.get(name) else {
        return;
    };

    path.push(name);
    let mut spreads = Vec::new();
    collect_spread_locations(&fragment.selection_set, &mut spreads);
    for (spread, location) in spreads {
        if let Some(index) = path.iter().position(|item| *item == spread) {
            let via = path[index + 1..]
                .iter()
                .map(|name| format!("\"{name}\""))
                .collect::<Vec<_>>()
                .join(", ");
            let message = if via.is_empty() {
                format!("Cannot spread fragment \"{spread}\" within itself.")
            } else {
                format!("Cannot spread fragment \"{spread}\" within itself via {via}.")
            };
            errors.push(GraphQLError::new(message).at(location));
        } else if !visited.contains(spread) {
            detect_cycle(fragments, spread, path, visited, errors);
        }
    }
    path.pop();
    visited.insert(name);
}

fn check_inline_type_conditions(
    schema: &Schema,
    selection_set: &SelectionSet<'_>,
    errors: &mut Vec<GraphQLError>,
) {
    for selection in &selection_set.items {
        match selection {
            Selection::Field(field) => {
                if let Some(child) = &field.selection_set {
                    check_inline_type_conditions(schema, child, errors);
                }
            }
            Selection::InlineFragment(fragment) => {
                if let Some(condition) = fragment.type_condition {
                    match schema.get_type(condition) {
                        None => errors.push(
                            GraphQLError::new(format!("Unknown type \"{condition}\"."))
                                .at(fragment.location),
                        ),
                        Some(ty) if !ty.is_composite() => errors.push(
                            GraphQLError::new(format!(
                                "Fragment cannot condition on non composite type \"{condition}\"."
                            ))
                            .at(fragment.location),
                        ),
                        Some(_) => {}
                    }
                }
                check_inline_type_conditions(schema, &fragment.selection_set, errors);
            }
            Selection::FragmentSpread(_) => {}
        }
    }
}

struct Validator<'a> {
    schema: &'a Schema,
    document: &'a Document<'a>,
    options: &'a ValidationOptions,
    errors: Vec<GraphQLError>,
    fragments: HashMap<&'a str, &'a FragmentDefinition<'a>>,
}

impl<'a> Validator<'a> {
    fn new(schema: &'a Schema, document: &'a Document<'a>, options: &'a ValidationOptions) -> Self {
        let mut fragments = HashMap::new();
        for fragment in document.fragments() {
            // First definition wins; UniqueFragmentNames reports the duplicates.
            fragments.entry(fragment.name).or_insert(fragment);
        }
        Self {
            schema,
            document,
            options,
            errors: Vec::new(),
            fragments,
        }
    }

    fn push(&mut self, message: impl Into<String>, location: Location) {
        if self.options.max_errors != 0 && self.errors.len() >= self.options.max_errors {
            return;
        }
        self.errors
            .push(GraphQLError::new(message.into()).at(location));
    }

    fn run(&mut self) {
        self.check_executable_definitions();
        self.check_operation_names();
        self.check_fragment_names();
        self.check_type_references();
        self.check_selection_sets();
        self.check_fragment_reachability();
        self.check_fragment_cycles();
        self.check_depth();
        variables::check_variables(
            self.schema,
            self.options,
            &self.fragments,
            self.document,
            &mut self.errors,
        );
        overlap::check(
            self.schema,
            self.options,
            &self.fragments,
            self.document,
            &mut self.errors,
        );
    }

    fn check_executable_definitions(&mut self) {
        for definition in &self.document.definitions {
            let Definition::TypeSystem(type_system) = definition else {
                continue;
            };
            let (message, location) = match type_system {
                TypeSystemDefinition::Schema(definition) => (
                    "The schema definition is not executable.".to_owned(),
                    definition.location,
                ),
                TypeSystemDefinition::Type(definition) => (
                    format!("The \"{}\" definition is not executable.", definition.name),
                    definition.location,
                ),
                TypeSystemDefinition::Directive(definition) => (
                    format!("The \"@{}\" definition is not executable.", definition.name),
                    definition.location,
                ),
                TypeSystemDefinition::SchemaExtension(extension) => (
                    "The schema extension is not executable.".to_owned(),
                    extension.location,
                ),
                TypeSystemDefinition::TypeExtension(extension) => (
                    format!("The \"{}\" extension is not executable.", extension.name),
                    extension.location,
                ),
            };
            self.push(message, location);
        }
    }

    fn check_operation_names(&mut self) {
        let document = self.document;
        let operations: Vec<&'a OperationDefinition<'a>> = document.operations().collect();

        let mut seen = HashSet::new();
        for operation in &operations {
            if let Some(name) = operation.name {
                if !seen.insert(name) {
                    self.push(
                        format!("There can be only one operation named \"{name}\"."),
                        operation.location,
                    );
                }
            }
        }

        if operations.len() > 1 {
            for operation in operations
                .iter()
                .filter(|operation| operation.name.is_none())
            {
                self.push(
                    "This anonymous operation must be the only defined operation.",
                    operation.location,
                );
            }
        }

        for operation in &operations {
            let mut variable_names = HashSet::new();
            for variable in &operation.variables {
                if !variable_names.insert(variable.name) {
                    self.push(
                        format!(
                            "There can be only one variable named \"${}\".",
                            variable.name
                        ),
                        variable.location,
                    );
                }
            }

            check_directives(
                self.schema,
                &operation.directives,
                directive_location(operation.kind),
                &mut self.errors,
            );
            if operation.kind == OperationKind::Subscription {
                self.check_single_field_subscription(operation);
            }
        }
    }

    fn check_single_field_subscription(&mut self, operation: &'a OperationDefinition<'a>) {
        let mut fields = Vec::new();
        collect_top_level_fields(
            &self.fragments,
            &operation.selection_set,
            &mut HashSet::new(),
            &mut fields,
        );

        // The specification groups by response key before counting, so `subscription { a a }`
        // has one entry even though it contains two field nodes.
        let mut keys = HashSet::new();
        for field in &fields {
            keys.insert(field.response_key());
        }
        let count = keys.len();
        let introspection = fields
            .iter()
            .find(|field| is_introspection_field(field.name));

        let subject = match operation.name {
            Some(name) => format!("Subscription \"{name}\""),
            None => "Anonymous subscription".to_owned(),
        };

        if count != 1 {
            self.push(
                format!("{subject} must select only one top-level field."),
                operation.location,
            );
        } else if let Some(field) = introspection {
            self.push(
                format!("{subject} must not select an introspection top-level field."),
                field.location,
            );
        }
    }

    fn check_fragment_names(&mut self) {
        let document = self.document;
        let mut seen = HashSet::new();
        for fragment in document.fragments() {
            if !seen.insert(fragment.name) {
                self.push(
                    format!(
                        "There can be only one fragment named \"{}\".",
                        fragment.name
                    ),
                    fragment.location,
                );
            }
            check_directives(
                self.schema,
                &fragment.directives,
                DirectiveLocation::FragmentDefinition,
                &mut self.errors,
            );
        }
    }

    fn check_type_condition(
        &mut self,
        condition: &str,
        fragment: Option<&str>,
        location: Location,
    ) {
        match self.schema.get_type(condition) {
            None => self.push(format!("Unknown type \"{condition}\"."), location),
            Some(ty) if !ty.is_composite() => {
                let message = match fragment {
                    Some(name) => format!(
                        "Fragment \"{name}\" cannot condition on non composite type \"{condition}\"."
                    ),
                    None => {
                        format!("Fragment cannot condition on non composite type \"{condition}\".")
                    }
                };
                self.push(message, location);
            }
            Some(_) => {}
        }
    }

    fn check_variable_definition(&mut self, variable: &'a VariableDefinition<'a>) {
        let named = variable.ty.named_type();
        match self.schema.get_type(named) {
            None => self.push(format!("Unknown type \"{named}\"."), variable.location),
            Some(ty) if !ty.is_input_type() => self.push(
                format!(
                    "Variable \"${}\" cannot be non-input type \"{}\".",
                    variable.name, variable.ty
                ),
                variable.location,
            ),
            Some(_) => {
                if let Some(default) = &variable.default_value {
                    values::validate_value(
                        self.schema,
                        default,
                        &type_ref_from_ast(&variable.ty),
                        true,
                        variable.location,
                        &mut self.errors,
                    );
                    values::check_input_field_names(default, &mut self.errors);
                }
            }
        }

        check_directives(
            self.schema,
            &variable.directives,
            DirectiveLocation::VariableDefinition,
            &mut self.errors,
        );
    }

    fn check_type_references(&mut self) {
        let document = self.document;
        for definition in &document.definitions {
            match definition {
                Definition::Operation(operation) => {
                    for variable in &operation.variables {
                        self.check_variable_definition(variable);
                    }
                    check_inline_type_conditions(
                        self.schema,
                        &operation.selection_set,
                        &mut self.errors,
                    );
                }
                Definition::Fragment(fragment) => {
                    self.check_type_condition(
                        fragment.type_condition,
                        Some(fragment.name),
                        fragment.location,
                    );
                    check_inline_type_conditions(
                        self.schema,
                        &fragment.selection_set,
                        &mut self.errors,
                    );
                }
                Definition::TypeSystem(_) => {}
            }
        }
    }

    fn check_selection_sets(&mut self) {
        let document = self.document;
        for operation in document.operations() {
            let Some(root) = root_type(self.schema, operation.kind) else {
                continue;
            };
            let mut rules = FieldRules {
                schema: self.schema,
                fragments: &self.fragments,
                errors: &mut self.errors,
            };
            walk(
                self.schema,
                self.options,
                Some(root),
                &operation.selection_set,
                &mut rules,
            );
        }

        for fragment in document.fragments() {
            let condition = self
                .schema
                .get_type(fragment.type_condition)
                .filter(|ty| ty.is_composite());
            let mut rules = FieldRules {
                schema: self.schema,
                fragments: &self.fragments,
                errors: &mut self.errors,
            };
            walk(
                self.schema,
                self.options,
                condition,
                &fragment.selection_set,
                &mut rules,
            );
        }
    }

    fn check_depth(&mut self) {
        if self.options.max_depth == 0 {
            return;
        }
        for operation in self.document.operations() {
            let mut visited = HashSet::new();
            if selection_depth(&self.fragments, &operation.selection_set, &mut visited, 1)
                > self.options.max_depth
            {
                self.push(
                    format!(
                        "Query depth exceeds the maximum allowed depth of {}.",
                        self.options.max_depth
                    ),
                    operation.location,
                );
            }
        }
    }

    fn check_fragment_reachability(&mut self) {
        let document = self.document;
        let fragments = &self.fragments;
        let mut reachable: HashSet<&'a str> = HashSet::new();
        let mut stack: Vec<&'a str> = Vec::new();
        for operation in document.operations() {
            collect_spreads(&operation.selection_set, &mut stack);
        }
        while let Some(name) = stack.pop() {
            if !reachable.insert(name) {
                continue;
            }
            if let Some(fragment) = fragments.get(name) {
                collect_spreads(&fragment.selection_set, &mut stack);
            }
        }

        for fragment in document.fragments() {
            if !reachable.contains(fragment.name) {
                self.push(
                    format!("Fragment \"{}\" is never used.", fragment.name),
                    fragment.location,
                );
            }
        }
    }

    fn check_fragment_cycles(&mut self) {
        let document = self.document;
        let mut visited = HashSet::new();
        for fragment in document.fragments() {
            detect_cycle(
                &self.fragments,
                fragment.name,
                &mut Vec::new(),
                &mut visited,
                &mut self.errors,
            );
        }
    }
}
