//! Query complexity and depth analysis.
//!
//! The analyzer walks an operation the way the executor would — expanding fragments, respecting
//! `@skip` and `@include`, and merging selections that share a response key — without running a
//! single resolver. It is meant to be called before execution so an oversized query is rejected
//! instead of tying up a worker.

use rustc_hash::FxHashMap;

use crate::{
    Object, Value,
    ast::{self, OperationKind},
    error::GraphQLError,
    schema::Schema,
    types::{Field, Type, TypeKind, TypeRef},
};

/// Inputs for a complexity analysis.
#[derive(Clone, Copy, Debug)]
pub struct ComplexityOptions {
    /// Multiplier applied to list fields.
    pub list_factor: u32,
    /// Cost of a field with no explicit estimator.
    pub field_cost: u32,
    /// Maximum estimated complexity. Zero disables the limit.
    pub max_complexity: u32,
    /// Maximum selection depth. Zero disables the limit.
    pub max_depth: usize,
}

impl Default for ComplexityOptions {
    fn default() -> Self {
        Self {
            list_factor: 10,
            field_cost: 1,
            max_complexity: 0,
            max_depth: 0,
        }
    }
}

/// The result of analysing an operation.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct Complexity {
    /// Estimated total cost.
    pub cost: u32,
    /// Selection-set depth.
    pub depth: usize,
}

/// Estimate the cost of the selected operation.
///
/// # Errors
///
/// Returns an error when the operation is unknown, ambiguous, or exceeds a configured limit.
pub fn analyze(
    schema: &Schema,
    document: &ast::Document<'_>,
    operation_name: Option<&str>,
    variables: &Object,
    options: &ComplexityOptions,
) -> Result<Complexity, GraphQLError> {
    let operation = select_operation(document, operation_name)?;
    let root = root_type_name(schema, operation.kind)?;

    let mut analyzer = Analyzer {
        schema,
        document,
        variables,
        options,
        fragment_cache: FxHashMap::default(),
        work: 0,
    };
    let mut active_fragments = Vec::new();
    let (cost, depth) = analyzer.analyze_selection(
        operation.selection_set.items.iter(),
        &root,
        1,
        &mut active_fragments,
    )?;

    if options.max_complexity != 0 && cost > options.max_complexity {
        return Err(GraphQLError::new(format!(
            "Query complexity {cost} exceeds the maximum allowed complexity of {}.",
            options.max_complexity
        )));
    }
    if options.max_depth != 0 && depth > options.max_depth {
        return Err(GraphQLError::new(format!(
            "Query depth {depth} exceeds the maximum allowed depth of {}.",
            options.max_depth
        )));
    }

    Ok(Complexity { cost, depth })
}

/// Select the operation the caller asked for, using the same rules as execution.
fn select_operation<'document, 'source>(
    document: &'document ast::Document<'source>,
    operation_name: Option<&str>,
) -> Result<&'document ast::OperationDefinition<'source>, GraphQLError> {
    let operations: Vec<&ast::OperationDefinition<'source>> = document.operations().collect();

    if let Some(name) = operation_name {
        return operations
            .into_iter()
            .find(|operation| operation.name == Some(name))
            .ok_or_else(|| GraphQLError::new(format!("Unknown operation named \"{name}\".")));
    }

    match operations.len() {
        0 => Err(GraphQLError::new("Must provide an operation.")),
        1 => Ok(operations[0]),
        _ => Err(GraphQLError::new(
            "Must provide an operation name if the document contains multiple operations.",
        )),
    }
}

fn root_type_name(schema: &Schema, kind: OperationKind) -> Result<String, GraphQLError> {
    match kind {
        OperationKind::Query => Ok(schema.query_name().to_string()),
        OperationKind::Mutation => schema
            .mutation_name()
            .map(ToString::to_string)
            .ok_or_else(|| GraphQLError::new("Schema is not configured for mutations.")),
        OperationKind::Subscription => schema
            .subscription_name()
            .map(ToString::to_string)
            .ok_or_else(|| GraphQLError::new("Schema is not configured for subscriptions.")),
    }
}

/// Maximum fragment nesting accepted by the analyzer.
const MAX_FRAGMENT_DEPTH: usize = 128;
/// Maximum number of selections the analyzer may visit before rejecting the operation.
const MAX_ANALYSIS_WORK: u64 = 500_000;

/// A field selection after fragment expansion and duplicate-response-key merging.
#[derive(Clone)]
struct CollectedField<'source, 'document> {
    response_key: &'source str,
    /// The type the field is selected on, which may be narrower than the enclosing type after a
    /// fragment with a type condition.
    scope: String,
    node: &'document ast::Field<'source>,
    /// Sub-selections of every field merged under this response key.
    selections: Vec<&'document ast::Selection<'source>>,
}

struct Analyzer<'a, 'document, 'source> {
    schema: &'a Schema,
    document: &'document ast::Document<'source>,
    variables: &'a Object,
    options: &'a ComplexityOptions,
    fragment_cache: FxHashMap<(&'source str, String), Vec<CollectedField<'source, 'document>>>,
    work: u64,
}

impl<'a, 'document, 'source> Analyzer<'a, 'document, 'source> {
    /// Analyze one selection set and return its cost and deepest nesting level.
    fn analyze_selection<I>(
        &mut self,
        items: I,
        scope: &str,
        depth: usize,
        active_fragments: &mut Vec<&'source str>,
    ) -> Result<(u32, usize), GraphQLError>
    where
        I: IntoIterator<Item = &'document ast::Selection<'source>>,
    {
        let mut collected = Vec::new();
        let mut index = FxHashMap::default();
        self.collect_fields(items, scope, active_fragments, &mut collected, &mut index)?;

        let mut cost = 0u32;
        let mut max_depth = depth;
        for field in &collected {
            let (field_cost, field_depth) = self.analyze_field(field, depth, active_fragments)?;
            cost = cost.saturating_add(field_cost);
            max_depth = max_depth.max(field_depth);
        }
        Ok((cost, max_depth))
    }

    /// Flatten a selection set into fields, expanding fragments and merging duplicate response
    /// keys. Directives are evaluated here so skipped selections never contribute cost.
    ///
    /// Fragment expansions are memoised by `(fragment, scope)`. Without that cache a document
    /// where each fragment spreads the previous one twice expands exponentially even though the
    /// merged result is tiny, which would let a client burn arbitrary CPU before a complexity
    /// limit is ever compared.
    fn collect_fields<I>(
        &mut self,
        items: I,
        scope: &str,
        active_fragments: &mut Vec<&'source str>,
        output: &mut Vec<CollectedField<'source, 'document>>,
        index: &mut FxHashMap<&'source str, usize>,
    ) -> Result<(), GraphQLError>
    where
        I: IntoIterator<Item = &'document ast::Selection<'source>>,
    {
        for selection in items {
            self.work = self.work.saturating_add(1);
            if self.work > MAX_ANALYSIS_WORK {
                return Err(GraphQLError::new(
                    "The query expands to too many fields to analyse safely.",
                ));
            }
            if !self.is_included(selection.directives()) {
                continue;
            }

            match selection {
                ast::Selection::Field(field) => {
                    merge_field(output, index, field, scope);
                }
                ast::Selection::FragmentSpread(spread) => {
                    if active_fragments.contains(&spread.name) {
                        // A fragment cycle is a validation error; guard against it so analysis can
                        // never hang or overflow the stack.
                        continue;
                    }
                    let Some(fragment) = self.document.fragment(spread.name) else {
                        continue;
                    };
                    if !self.is_included(&fragment.directives) {
                        continue;
                    }
                    let Some(effective_scope) =
                        fragment_scope(self.schema, fragment.type_condition, scope)
                    else {
                        continue;
                    };
                    if active_fragments.len() >= MAX_FRAGMENT_DEPTH {
                        return Err(GraphQLError::new(format!(
                            "Fragment nesting exceeds the maximum of {MAX_FRAGMENT_DEPTH}."
                        )));
                    }

                    let key = (spread.name, effective_scope.clone());
                    let cached = self.fragment_cache.get(&key).cloned();
                    if let Some(fields) = cached {
                        merge_all(output, index, &fields);
                        continue;
                    }

                    let mut fields = Vec::new();
                    let mut fragment_index = FxHashMap::default();
                    active_fragments.push(spread.name);
                    let result = self.collect_fields(
                        fragment.selection_set.items.iter(),
                        &effective_scope,
                        active_fragments,
                        &mut fields,
                        &mut fragment_index,
                    );
                    active_fragments.pop();
                    result?;
                    self.fragment_cache.insert(key, fields.clone());
                    merge_all(output, index, &fields);
                }
                ast::Selection::InlineFragment(fragment) => {
                    let effective_scope = match fragment.type_condition {
                        Some(condition) => match fragment_scope(self.schema, condition, scope) {
                            Some(scope) => scope,
                            None => continue,
                        },
                        None => scope.to_owned(),
                    };
                    self.collect_fields(
                        fragment.selection_set.items.iter(),
                        &effective_scope,
                        active_fragments,
                        output,
                        index,
                    )?;
                }
            }
        }
        Ok(())
    }

    fn analyze_field(
        &mut self,
        field: &CollectedField<'source, 'document>,
        depth: usize,
        active_fragments: &mut Vec<&'source str>,
    ) -> Result<(u32, usize), GraphQLError> {
        let base = self.field_cost(field);

        if field.selections.is_empty() {
            return Ok((base, depth));
        }

        let Some(return_type) = self.field_type(&field.scope, field.node.name) else {
            // An unknown field is a validation error. Be defensive: charge its base cost and do
            // not try to descend into a type we cannot resolve.
            return Ok((base, depth));
        };

        let named_type = return_type.named_type().to_string();
        let (selection_cost, selection_depth) = self.analyze_selection(
            field.selections.iter().copied(),
            &named_type,
            depth.saturating_add(1),
            active_fragments,
        )?;

        let multiplier = list_multiplier(&return_type, self.options.list_factor);
        let total = base.saturating_add(selection_cost.saturating_mul(multiplier));
        Ok((total, selection_depth))
    }

    fn field_cost(&self, field: &CollectedField<'source, 'document>) -> u32 {
        let Some(definition) = self.field_definition(&field.scope, field.node.name) else {
            return self.options.field_cost;
        };
        let Some(estimator) = &definition.complexity else {
            return self.options.field_cost;
        };
        // Validation reports argument errors before this runs; if coercion still fails, fall back
        // to defaults rather than rejecting the whole analysis.
        let args = crate::execution::coerce_argument_values(
            self.schema,
            definition,
            field.node,
            self.variables,
        )
        .unwrap_or_default();
        estimator(&args)
    }

    fn field_definition(&self, scope: &str, name: &str) -> Option<&Field> {
        match self.schema.get_type(scope)? {
            Type::Object(object) => object.fields.get(name),
            Type::Interface(interface) => interface.fields.get(name),
            _ => None,
        }
    }

    fn field_type(&self, scope: &str, name: &str) -> Option<TypeRef> {
        match name {
            "__typename" => None,
            "__schema" => Some(TypeRef::named("__Schema").non_null()),
            "__type" => Some(TypeRef::named("__Type")),
            _ => self
                .field_definition(scope, name)
                .map(|field| field.ty.clone()),
        }
    }

    /// Evaluate `@skip` and `@include`.
    ///
    /// A variable that is missing or not a boolean is treated as `false`, which is what validation
    /// assumed when it let the document through.
    fn is_included(&self, directives: &[ast::Directive<'source>]) -> bool {
        for directive in directives {
            match directive.name {
                "skip" => {
                    if directive_if(directive, self.variables).unwrap_or(false) {
                        return false;
                    }
                }
                "include" => {
                    if !directive_if(directive, self.variables).unwrap_or(false) {
                        return false;
                    }
                }
                _ => {}
            }
        }
        true
    }
}

fn merge_field<'source, 'document>(
    output: &mut Vec<CollectedField<'source, 'document>>,
    index: &mut FxHashMap<&'source str, usize>,
    field: &'document ast::Field<'source>,
    scope: &str,
) {
    let response_key = field.response_key();
    if let Some(existing) = index.get(response_key).copied() {
        let collected = &mut output[existing];
        // Validation guarantees that fields merged under one response key have the same name.
        if let Some(selection_set) = &field.selection_set {
            collected.selections.extend(selection_set.items.iter());
        }
        return;
    }
    index.insert(response_key, output.len());
    output.push(CollectedField {
        response_key,
        scope: scope.to_owned(),
        node: field,
        selections: field
            .selection_set
            .as_ref()
            .map(|selection_set| selection_set.items.iter().collect())
            .unwrap_or_default(),
    });
}

fn merge_all<'source, 'document>(
    output: &mut Vec<CollectedField<'source, 'document>>,
    index: &mut FxHashMap<&'source str, usize>,
    fields: &[CollectedField<'source, 'document>],
) {
    for field in fields {
        if let Some(existing) = index.get(field.response_key).copied() {
            let collected = &mut output[existing];
            collected
                .selections
                .extend(field.selections.iter().copied());
        } else {
            index.insert(field.response_key, output.len());
            output.push(field.clone());
        }
    }
}

fn directive_if(directive: &ast::Directive<'_>, variables: &Object) -> Option<bool> {
    match &directive.argument("if")?.value {
        ast::Value::Boolean(value) => Some(*value),
        ast::Value::Variable(name) => variables.get(name).and_then(Value::as_bool),
        _ => None,
    }
}

/// The type a fragment's selections apply to, or `None` when it can never apply.
fn fragment_scope(schema: &Schema, condition: &str, parent: &str) -> Option<String> {
    if condition == parent {
        return Some(parent.to_owned());
    }

    let Some(condition_type) = schema.get_type(condition) else {
        // An unknown condition is a validation error; keep the enclosing scope.
        return Some(parent.to_owned());
    };
    let parent_type = schema.get_type(parent);

    match condition_type {
        Type::Object(_) => {
            if parent_type.is_some_and(|ty| ty.kind() == TypeKind::Object) {
                // Two distinct object types never overlap.
                None
            } else if schema.is_possible_type(parent, condition) {
                Some(condition.to_owned())
            } else {
                None
            }
        }
        Type::Interface(_) | Type::Union(_) => {
            if parent_type.is_some_and(|ty| ty.kind() == TypeKind::Object) {
                if schema.is_possible_type(condition, parent) {
                    Some(parent.to_owned())
                } else {
                    None
                }
            } else {
                Some(condition.to_owned())
            }
        }
        _ => None,
    }
}

/// The multiplier contributed by every list wrapper around a type.
fn list_multiplier(ty: &TypeRef, factor: u32) -> u32 {
    let mut multiplier = 1u32;
    let mut current = ty;
    loop {
        match current {
            TypeRef::NonNull(inner) => current = inner,
            TypeRef::List(inner) => {
                multiplier = multiplier.saturating_mul(factor);
                current = inner;
            }
            TypeRef::Named(_) => break,
        }
    }
    multiplier
}
