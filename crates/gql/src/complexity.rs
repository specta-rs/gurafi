//! Query complexity and depth analysis.
//!
//! The analyzer walks an operation the way the executor would — expanding fragments, respecting
//! `@skip` and `@include`, and merging selections that share a response key — without running a
//! single resolver. It is meant to be called before execution so an oversized query is rejected
//! instead of tying up a worker.

use std::{borrow::Cow, sync::Arc};

use rustc_hash::{FxHashMap, FxHashSet};
use smallvec::SmallVec;

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
    /// Maximum number of distinct fields in any one selection set after fragment expansion.
    ///
    /// This bounds wide, heavily aliased queries that stay shallow. Zero disables the limit.
    pub max_breadth: usize,
}

impl Default for ComplexityOptions {
    fn default() -> Self {
        Self {
            list_factor: 10,
            field_cost: 1,
            max_complexity: 0,
            max_depth: 0,
            max_breadth: 0,
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
    /// The largest number of distinct response keys in any one selection set.
    pub breadth: usize,
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
    analyze_operation(schema, document, operation, variables, options)
}

/// Estimate the cost of an operation that has already been selected.
///
/// A prepared query selects its operation once, at preparation time, so it can use this entry
/// point and skip the document scan [`analyze`] performs on every request.
///
/// # Errors
///
/// Returns an error when the operation exceeds a configured limit.
pub(crate) fn analyze_operation(
    schema: &Schema,
    document: &ast::Document<'_>,
    operation: &ast::OperationDefinition<'_>,
    variables: &Object,
    options: &ComplexityOptions,
) -> Result<Complexity, GraphQLError> {
    let root = root_type_name(schema, operation.kind)?;

    let mut analyzer = Analyzer {
        schema,
        document,
        variables,
        options,
        fragment_cache: FragmentCache::new(),
        work: 0,
        breadth: 0,
    };
    let mut active_fragments = SmallVec::new();
    let (cost, depth) = analyzer.analyze_selection(
        operation.selection_set.items.iter(),
        &root,
        1,
        &mut active_fragments,
    )?;
    let breadth = analyzer.breadth;

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
    if options.max_breadth != 0 && breadth > options.max_breadth {
        return Err(GraphQLError::new(format!(
            "Query breadth {breadth} exceeds the maximum allowed breadth of {}.",
            options.max_breadth
        )));
    }

    Ok(Complexity {
        cost,
        depth,
        breadth,
    })
}

/// Select the operation the caller asked for, using the same rules as execution.
fn select_operation<'document, 'source>(
    document: &'document ast::Document<'source>,
    operation_name: Option<&str>,
) -> Result<&'document ast::OperationDefinition<'source>, GraphQLError> {
    if let Some(name) = operation_name {
        return document
            .operations()
            .find(|operation| operation.name == Some(name))
            .ok_or_else(|| GraphQLError::new(format!("Unknown operation named \"{name}\".")));
    }

    let mut operations = document.operations();
    match operations.next() {
        None => Err(GraphQLError::new("Must provide an operation.")),
        Some(first) if operations.next().is_none() => Ok(first),
        Some(_) => Err(GraphQLError::new(
            "Must provide an operation name if the document contains multiple operations.",
        )),
    }
}

fn root_type_name(schema: &Schema, kind: OperationKind) -> Result<Arc<str>, GraphQLError> {
    match kind {
        // The schema already interns its root names, so the common case shares that allocation.
        OperationKind::Query => Ok(Arc::clone(schema.query_name())),
        OperationKind::Mutation => schema
            .mutation_name()
            .map(Arc::clone)
            .ok_or_else(|| GraphQLError::new("Schema is not configured for mutations.")),
        OperationKind::Subscription => schema
            .subscription_name()
            .map(Arc::clone)
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
    scope: Arc<str>,
    node: &'document ast::Field<'source>,
    /// Sub-selections of every field merged under this response key.
    selections: SmallVec<[&'document ast::Selection<'source>; 4]>,
}

/// The fields of one selection set after flattening.
///
/// Selection sets overwhelmingly hold a handful of fields, and analysis allocates this for every
/// one of them, so the common shapes (up to eight fields) stay inline.
type CollectedFields<'source, 'document> = SmallVec<[CollectedField<'source, 'document>; 8]>;

/// A memoised fragment expansion: the fragment name, the scope it was expanded for, and the
/// resulting fields.
type CachedFragment<'source, 'document> =
    (&'source str, Arc<str>, CollectedFields<'source, 'document>);

/// Fragment expansions memoised for one analysis.
///
/// Documents define only a handful of fragments and analysis visits each one a handful of times,
/// so a linear scan over an inline list is both faster and allocation-free compared to hashing.
struct FragmentCache<'source, 'document> {
    entries: SmallVec<[CachedFragment<'source, 'document>; 8]>,
    index: Option<FxHashMap<(&'source str, Arc<str>), usize>>,
}

impl<'source, 'document> FragmentCache<'source, 'document> {
    fn new() -> Self {
        Self {
            entries: SmallVec::new(),
            index: None,
        }
    }

    fn get(&self, key: &(&'source str, Arc<str>)) -> Option<&[CollectedField<'source, 'document>]> {
        match &self.index {
            Some(index) => index
                .get(key)
                .map(|position| self.entries[*position].2.as_slice()),
            None => self
                .entries
                .iter()
                .find(|(name, scope, _)| *name == key.0 && scope == &key.1)
                .map(|(_, _, fields)| fields.as_slice()),
        }
    }

    fn insert(
        &mut self,
        key: (&'source str, Arc<str>),
        fields: CollectedFields<'source, 'document>,
    ) {
        let position = self.entries.len();
        self.entries.push((key.0, key.1, fields));
        match &mut self.index {
            Some(index) => {
                index.insert(
                    (
                        self.entries[position].0,
                        Arc::clone(&self.entries[position].1),
                    ),
                    position,
                );
            }
            None if self.entries.len() > LINEAR_INDEX_LIMIT => {
                self.index = Some(
                    self.entries
                        .iter()
                        .enumerate()
                        .map(|(position, (name, scope, _))| ((*name, Arc::clone(scope)), position))
                        .collect(),
                );
            }
            None => {}
        }
    }
}

/// Above this many entries a hash index beats a linear scan.
const LINEAR_INDEX_LIMIT: usize = 16;

struct Analyzer<'a, 'document, 'source> {
    schema: &'a Schema,
    document: &'document ast::Document<'source>,
    variables: &'a Object,
    options: &'a ComplexityOptions,
    fragment_cache: FragmentCache<'source, 'document>,
    work: u64,
    breadth: usize,
}

impl<'a, 'document, 'source> Analyzer<'a, 'document, 'source> {
    /// Analyze one selection set and return its cost and deepest nesting level.
    fn analyze_selection<I>(
        &mut self,
        items: I,
        scope: &Arc<str>,
        depth: usize,
        active_fragments: &mut SmallVec<[&'source str; 8]>,
    ) -> Result<(u32, usize), GraphQLError>
    where
        I: IntoIterator<Item = &'document ast::Selection<'source>>,
    {
        let mut collected: CollectedFields<'source, 'document> = SmallVec::new();
        let mut index = FieldIndex::new();
        self.collect_fields(items, scope, active_fragments, &mut collected, &mut index)?;
        self.breadth = self.breadth.max(distinct_response_keys(&collected));

        // Every field in the set is looked up on the same parent type, so it is resolved once for
        // the set instead of once per field. A field merged from a fragment with a narrower scope
        // carries its own scope and resolves that separately.
        let parent_type = self.schema.get_type(scope);
        let mut cost = 0u32;
        let mut max_depth = depth;
        for field in &collected {
            let (field_cost, field_depth) =
                self.analyze_field(field, parent_type, scope, depth, active_fragments)?;
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
        scope: &Arc<str>,
        active_fragments: &mut SmallVec<[&'source str; 8]>,
        output: &mut CollectedFields<'source, 'document>,
        index: &mut FieldIndex<'source>,
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

                    let key = (spread.name, Arc::clone(&effective_scope));
                    if let Some(fields) = self.fragment_cache.get(&key) {
                        merge_all(output, index, fields);
                        continue;
                    }

                    let mut fields = SmallVec::new();
                    let mut fragment_index = FieldIndex::new();
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
                    merge_all(output, index, &fields);
                    self.fragment_cache.insert(key, fields);
                }
                ast::Selection::InlineFragment(fragment) => {
                    let effective_scope = match fragment.type_condition {
                        Some(condition) => match fragment_scope(self.schema, condition, scope) {
                            Some(scope) => scope,
                            None => continue,
                        },
                        None => Arc::clone(scope),
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
        parent_type: Option<&'a Type>,
        parent_scope: &str,
        depth: usize,
        active_fragments: &mut SmallVec<[&'source str; 8]>,
    ) -> Result<(u32, usize), GraphQLError> {
        // The field's definition is needed for both its cost and its return type, so it is
        // resolved once instead of once per question.
        let definition = if field.scope.as_ref() == parent_scope {
            field_definition_on(parent_type, field.node.name)
        } else {
            self.field_definition(&field.scope, field.node.name)
        };
        let base = self.field_cost(field, definition);

        if field.selections.is_empty() {
            return Ok((base, depth));
        }

        let return_type = match definition {
            Some(definition) => Cow::Borrowed(&definition.ty),
            None => match meta_field_type(field.node.name) {
                Some(ty) => ty,
                // An unknown field is a validation error. Be defensive: charge its base cost and
                // do not try to descend into a type we cannot resolve.
                None => return Ok((base, depth)),
            },
        };

        let multiplier = list_multiplier(&return_type, self.options.list_factor);
        let named_type = return_type.named_type().clone();
        let (selection_cost, selection_depth) = self.analyze_selection(
            field.selections.iter().copied(),
            &named_type,
            depth.saturating_add(1),
            active_fragments,
        )?;

        let total = base.saturating_add(selection_cost.saturating_mul(multiplier));
        Ok((total, selection_depth))
    }

    fn field_cost(
        &self,
        field: &CollectedField<'source, 'document>,
        definition: Option<&Field>,
    ) -> u32 {
        let Some(definition) = definition else {
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
        field_definition_on(self.schema.get_type(scope), name)
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

/// The number of distinct response keys in one collected selection set.
///
/// `collected` keeps one entry per (response key, fragment scope) so cost accounting stays
/// conservative, but breadth counts response keys: a field selected again through a narrower
/// fragment does not make the set wider.
fn distinct_response_keys(fields: &[CollectedField<'_, '_>]) -> usize {
    let mut small: SmallVec<[&str; 16]> = SmallVec::new();
    let mut large: Option<FxHashSet<&str>> = None;
    for field in fields {
        if let Some(large) = &mut large {
            large.insert(field.response_key);
            continue;
        }
        if small.contains(&field.response_key) {
            continue;
        }
        small.push(field.response_key);
        if small.len() > 16 {
            large = Some(small.iter().copied().collect());
        }
    }
    large.map_or(small.len(), |large| large.len())
}

/// The response-key index for one collected selection set.
///
/// Selection sets are small, so the common case scans the collected list directly and never
/// allocates a hash map. Only a document with many distinct response keys pays for hashing.
struct FieldIndex<'source> {
    map: Option<FxHashMap<&'source str, SmallVec<[usize; 1]>>>,
}

impl<'source> FieldIndex<'source> {
    fn new() -> Self {
        Self { map: None }
    }
}

fn merge_field<'source, 'document>(
    output: &mut CollectedFields<'source, 'document>,
    index: &mut FieldIndex<'source>,
    field: &'document ast::Field<'source>,
    scope: &Arc<str>,
) {
    let response_key = field.response_key();
    if let Some(existing) = find_entry(output, index, response_key, scope) {
        // The specification merges field nodes with the same response key and the same parent
        // scope; their sub-selections are collected once.
        if let Some(selection_set) = &field.selection_set {
            output[existing]
                .selections
                .extend(selection_set.items.iter());
        }
        return;
    }
    // The same response key selected through a different, overlapping fragment scope is kept as a
    // separate accounting entry. The executor still resolves it once; charging both scopes is
    // deliberately conservative and prevents a narrow scope from hiding expensive fields that
    // only the other scope can see.
    push_entry(
        output,
        index,
        CollectedField {
            response_key,
            scope: Arc::clone(scope),
            node: field,
            selections: field
                .selection_set
                .as_ref()
                .map(|selection_set| selection_set.items.iter().collect())
                .unwrap_or_default(),
        },
    );
}

fn merge_all<'source, 'document>(
    output: &mut CollectedFields<'source, 'document>,
    index: &mut FieldIndex<'source>,
    fields: &[CollectedField<'source, 'document>],
) {
    for field in fields {
        // Re-run the scope-aware merge for the cached fragment's fields.
        match find_entry(output, index, field.response_key, &field.scope) {
            Some(existing) => output[existing]
                .selections
                .extend(field.selections.iter().copied()),
            None => push_entry(output, index, field.clone()),
        }
    }
}

fn find_entry<'source, 'document>(
    output: &CollectedFields<'source, 'document>,
    index: &FieldIndex<'source>,
    response_key: &str,
    scope: &str,
) -> Option<usize> {
    match &index.map {
        Some(map) => map.get(response_key).and_then(|positions| {
            positions
                .iter()
                .copied()
                .find(|position| output[*position].scope.as_ref() == scope)
        }),
        None => output.iter().position(|collected| {
            collected.response_key == response_key && collected.scope.as_ref() == scope
        }),
    }
}

fn push_entry<'source, 'document>(
    output: &mut CollectedFields<'source, 'document>,
    index: &mut FieldIndex<'source>,
    entry: CollectedField<'source, 'document>,
) {
    let position = output.len();
    let response_key = entry.response_key;
    output.push(entry);
    match &mut index.map {
        Some(map) => map.entry(response_key).or_default().push(position),
        None if output.len() > LINEAR_INDEX_LIMIT => {
            let mut map: FxHashMap<&'source str, SmallVec<[usize; 1]>> = FxHashMap::default();
            for (position, collected) in output.iter().enumerate() {
                map.entry(collected.response_key)
                    .or_default()
                    .push(position);
            }
            index.map = Some(map);
        }
        None => {}
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
fn fragment_scope(schema: &Schema, condition: &str, parent: &Arc<str>) -> Option<Arc<str>> {
    if condition == parent.as_ref() {
        return Some(Arc::clone(parent));
    }

    let Some(condition_type) = schema.get_type(condition) else {
        // An unknown condition is a validation error; keep the enclosing scope.
        return Some(Arc::clone(parent));
    };
    let parent_type = schema.get_type(parent);

    match condition_type {
        Type::Object(_) => {
            if parent_type.is_some_and(|ty| ty.kind() == TypeKind::Object) {
                // Two distinct object types never overlap.
                None
            } else if schema.is_possible_type(parent, condition) {
                Some(Arc::from(condition))
            } else {
                None
            }
        }
        Type::Interface(_) | Type::Union(_) => {
            if parent_type.is_some_and(|ty| ty.kind() == TypeKind::Object) {
                if schema.is_possible_type(condition, parent) {
                    Some(Arc::clone(parent))
                } else {
                    None
                }
            } else {
                Some(Arc::from(condition))
            }
        }
        _ => None,
    }
}

/// Look a field up on a type that is already resolved.
fn field_definition_on<'t>(parent: Option<&'t Type>, name: &str) -> Option<&'t Field> {
    match parent? {
        Type::Object(object) => object.get_field(name),
        Type::Interface(interface) => interface.get_field(name),
        _ => None,
    }
}

/// The return type of a specification meta-field, which has no schema definition.
fn meta_field_type(name: &str) -> Option<Cow<'_, TypeRef>> {
    match name {
        "__typename" => None,
        "__schema" => Some(Cow::Owned(TypeRef::named("__Schema").non_null())),
        "__type" => Some(Cow::Owned(TypeRef::named("__Type"))),
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
