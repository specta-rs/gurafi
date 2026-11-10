//! Precompiled field collection for prepared queries.
//!
//! A prepared query's document never changes, so everything `CollectFields` derives from the
//! document alone can be computed once, at preparation, instead of once per object in every
//! response. This module builds that plan and lends it to the executor:
//!
//! * Each selection *source* (the merged selection sets reached from a field group) is identified
//!   once; its child sources are enumerated while planning, so execution never has to expand
//!   fragments or re-scan selection sets again.
//! * Field groups keep the specification's first-seen response-key order, their merged field
//!   nodes, and the schema field they resolve to (as a positional index, so execution does not
//!   hash the object type's field map).
//! * A plan whose response is exactly its parent object (only unconditional, unaliased built-in
//!   scalar properties) records that fact, so a matching parent value can be reused instead of
//!   copied field by field.
//! * `@skip`/`@include` conditions that depend on variables are kept as per-node requirements. A
//!   group precomputes one variant per possible combination of its conditional nodes, so runtime
//!   work is limited to evaluating those requirements and indexing the variant.
//! * Field arguments are compiled into a template: fully coerced literal values are stored, and
//!   only variable references are filled in per request. Documents with a nested variable inside
//!   an argument value keep the general coercion path.
//!
//! Anything the plan cannot model exactly (conditional fragment spreads, an unknown fragment, a
//! plan budget being exhausted) leaves that selection unplanned; execution then falls back to the
//! specification's dynamic `CollectFields`, so arbitrary valid documents still execute correctly.
//! The plan is a pure optimization: a miss is always safe.
//!
//! The plan owns the field nodes it refers to (as `Arc<ast::Field>`, cloned at preparation). The
//! parsed document lives in the same self-referential cell as this plan, so the plan cannot borrow
//! it; the clones borrow only the source text, which the cell's owner keeps alive for the plan's
//! whole lifetime.

use std::{collections::VecDeque, sync::Arc};

use rustc_hash::FxHashMap;
use smallvec::SmallVec;

use crate::{
    Location, Object, Value, ast,
    error::GraphQLError,
    schema::Schema,
    types::{Field, InputValue, Name, ObjectType, Type, TypeRef, names_eq},
};

use super::{coerce, collect, selection::type_condition_matches};

/// Maximum number of selection sources a plan may intern.
///
/// A source is a distinct merged selection list. Real documents need a handful; the bound keeps a
/// pathological document from turning planning into unbounded work at preparation time.
const MAX_SOURCES: usize = 512;

/// Maximum number of `(source, object type)` field plans a prepared query may build.
const MAX_PLANS: usize = 256;

/// Maximum number of variable-dependent field nodes that may share one response key.
///
/// Every conditional node doubles that group's variant table, so a group with more than this many
/// keeps the dynamic collection path instead of enumerating combinations.
const MAX_CONDITIONAL_NODES: usize = 4;

/// A precompiled collection plan for one prepared operation.
pub(crate) struct PreparedPlan<'a> {
    sources: Vec<Source<'a>>,
    root: Option<u32>,
    /// Selection-set levels reachable through the plan's child sources.
    ///
    /// The planner is iterative, but execution follows those sources recursively. This lets the
    /// executor send a very deep plan down the dynamic path, whose stack bound is the one the
    /// parser's depth limit was chosen against.
    depth: usize,
}

/// One merged selection list and the field plans derived from it.
struct Source<'a> {
    /// The field nodes whose sub-selections this source merges, in document order.
    nodes: Vec<Arc<ast::Field<'a>>>,
    /// Explicit selection sets, used only by the operation's root source.
    root_sets: Vec<Arc<ast::SelectionSet<'a>>>,
    /// Sorted node addresses, used to recognize an already interned source.
    identity: Vec<usize>,
    /// Field plans per concrete object type, usually exactly one.
    types: Vec<FieldPlan<'a>>,
    /// Whether this source uses a construct the plan does not model.
    unplannable: bool,
}

/// A selection source collected for one concrete object type.
pub(crate) struct FieldPlan<'a> {
    /// The object type this plan applies to.
    type_name: Name,
    /// Response keys in first-seen order.
    groups: Vec<FieldGroup<'a>>,
    /// Whether any field of the type defines a resolver.
    ///
    /// The synchronous completion path refuses a type that defines one, whatever the selection
    /// asks for. Planning it once keeps that check off the per-object path.
    pub(crate) has_resolvers: bool,
    /// Whether the response object is exactly a projection of the parent object.
    ///
    /// True when every group is an unconditional, unaliased, resolver-free built-in scalar
    /// property. Such a plan can reuse the parent object without copying when the parent's keys
    /// and value shapes line up, which is common for leaf objects nested in a list.
    pub(crate) identity_projection: bool,
}

/// A field that completes by copying an unaliased built-in scalar property.
///
/// The executor's synchronous path handles these inline. Their shape is fixed by the schema and
/// the document, so it is decided at preparation instead of re-checked for every object.
pub(crate) struct Property {
    /// The property name in the parent object.
    pub(crate) name: Name,
    /// The response key (the field's interned name).
    pub(crate) key: Name,
    /// Which built-in scalar accepts which value shape.
    pub(crate) kind: ScalarKind,
}

/// The built-in scalar shapes a [`Property`] may accept.
#[derive(Clone, Copy)]
pub(crate) enum ScalarKind {
    /// `String` and `ID`, which both pass string values through.
    String,
    /// `Int`.
    Int,
    /// `Boolean`.
    Boolean,
    /// `Float`.
    Float,
}

impl ScalarKind {
    /// Whether `value` already has the shape this scalar passes through unchanged.
    #[inline(always)]
    pub(crate) fn accepts(self, value: &Value) -> bool {
        match self {
            Self::String => matches!(value, Value::String(_)),
            Self::Int => matches!(value, Value::Int(_)),
            Self::Boolean => matches!(value, Value::Boolean(_)),
            Self::Float => matches!(value, Value::Float(number) if number.is_finite()),
        }
    }
}

/// One response key's merged field nodes.
pub(crate) struct FieldGroup<'a> {
    /// The key this group contributes to the response object.
    response_key: &'a str,
    /// The response key as an owned `Arc`, when the group is aliased.
    ///
    /// An unaliased group reuses the schema field's interned name instead, so aliased keys are the
    /// only ones a request would otherwise have to allocate.
    alias_key: Option<Arc<str>>,
    /// Position of the schema field in the object type, or `None` for meta/unknown fields.
    field_index: Option<u32>,
    /// Every node of the group, in document order, with its inclusion requirements.
    nodes: Vec<PlanNode<'a>>,
    /// One entry per possible combination of conditional nodes (exactly one when unconditional).
    variants: Vec<Variant<'a>>,
    /// Compiled argument template. `None` means "coerce per request", exactly as before.
    args: Option<ArgsTemplate<'a>>,
    /// Set when the group is an unaliased, resolver-free built-in scalar property.
    pub(crate) property: Option<Property>,
}

/// A field node and the variable requirements that decide whether it is collected.
struct PlanNode<'a> {
    node: Arc<ast::Field<'a>>,
    /// Address of the node in the parsed document, used to intern sources.
    origin: usize,
    requirements: Vec<Requirement<'a>>,
}

/// A node considered while interning a source.
struct PlanNodeRef<'a> {
    node: Arc<ast::Field<'a>>,
    origin: usize,
    included: bool,
}

/// One `@skip`/`@include` condition on a variable.
#[derive(Clone, Copy)]
struct Requirement<'a> {
    variable: &'a str,
    /// The variable value the selection requires, matching `should_include`'s treatment of
    /// missing and non-boolean values as `false`.
    expected: bool,
}

/// The collected nodes and child source for one conditional combination.
pub(crate) struct Variant<'a> {
    pub(crate) nodes: Vec<Arc<ast::Field<'a>>>,
    pub(crate) child: Option<u32>,
}

impl<'a> Variant<'a> {
    /// The nodes' sub-selections, merged in document order.
    #[inline]
    pub(crate) fn selection_sets(&'a self) -> SmallVec<[&'a ast::SelectionSet<'a>; 2]> {
        self.nodes
            .iter()
            .filter_map(|node| node.selection_set.as_ref())
            .collect()
    }
}

/// A precompiled field argument list.
pub(crate) struct ArgsTemplate<'a> {
    field_name: Name,
    slots: Vec<ArgSlot<'a>>,
}

enum ArgSlot<'a> {
    /// A literal (or schema default) already coerced against the argument's input type.
    Literal { name: Name, value: Value },
    /// A top-level variable reference, resolved per request.
    Variable {
        name: Name,
        variable: &'a str,
        ty: TypeRef,
        location: Location,
        default: Option<Value>,
    },
}

/// The field nodes behind one group: borrowed from the document, or owned by the plan.
#[derive(Clone, Copy)]
pub(crate) enum GroupNodes<'s, 'a> {
    /// Nodes from the document (dynamic collection).
    Borrowed(&'s [&'a ast::Field<'a>]),
    /// Nodes cloned into the plan at preparation, borrowed from the plan.
    Planned(&'a [Arc<ast::Field<'a>>]),
}

impl<'s, 'a> GroupNodes<'s, 'a> {
    /// The first node, when the group is non-empty.
    #[inline(always)]
    pub(crate) fn first(&self) -> Option<&'a ast::Field<'a>> {
        match self {
            Self::Borrowed(nodes) => nodes.first().copied(),
            Self::Planned(nodes) => nodes.first().map(|node| &**node),
        }
    }

    /// The nodes' sub-selections, merged in document order.
    #[inline]
    pub(crate) fn selection_sets(&self) -> SmallVec<[&'a ast::SelectionSet<'a>; 2]> {
        match self {
            Self::Borrowed(nodes) => nodes
                .iter()
                .copied()
                .filter_map(|node| node.selection_set.as_ref())
                .collect(),
            Self::Planned(nodes) => nodes
                .iter()
                .filter_map(|node| node.selection_set.as_ref())
                .collect(),
        }
    }
}

/// A runtime view of one collected field group.
///
/// Both the precompiled and the dynamic collection paths produce this, so the executor has one
/// shape to consume.
pub(crate) struct GroupView<'s, 'a> {
    /// The key this group contributes to the response object.
    pub(crate) response_key: &'a str,
    /// The precomputed aliased key, when the group is planned.
    pub(crate) alias_key: Option<&'s Arc<str>>,
    /// Included field nodes, in document order.
    pub(crate) nodes: GroupNodes<'s, 'a>,
    /// The schema field the group resolves to, when there is one.
    pub(crate) field: Option<&'a Field>,
    /// Compiled arguments, when the plan could build them.
    pub(crate) args: Option<&'s ArgsTemplate<'a>>,
    /// The child selection source, when the plan knows it.
    pub(crate) child: Option<u32>,
    /// Whether this view came from a precompiled plan rather than dynamic collection.
    pub(crate) planned: bool,
}

impl<'s, 'a> GroupView<'s, 'a> {
    /// The response key as an owned `Arc`.
    ///
    /// Aliased keys were interned once at preparation, so this only allocates when the group came
    /// from the dynamic path.
    #[inline(always)]
    pub(crate) fn key_arc(&self, first: &ast::Field<'_>, field: Option<&Field>) -> Arc<str> {
        if let Some(key) = self.alias_key {
            return Arc::clone(key);
        }
        match (self.planned, field, first.alias) {
            // A planned group's key was decided at preparation; an unaliased one reuses the
            // schema field's interned name.
            (true, Some(field), _) => field.name.clone(),
            (true, None, _) => Arc::from(self.response_key),
            (false, Some(field), None) => field.name.clone(),
            (false, _, _) => Arc::from(self.response_key),
        }
    }
}

impl<'a> PreparedPlan<'a> {
    /// The source id of the operation's root selection set.
    pub(crate) fn root(&self) -> Option<u32> {
        self.root
    }

    /// The number of selection-set levels the planned path would recurse through.
    ///
    /// Zero for an empty plan. Callers use it to keep execution's stack bound: the dynamic path
    /// is the one the parser depth limit protects, while the planned path has larger frames.
    pub(crate) fn depth(&self) -> usize {
        self.depth
    }

    /// Iterate a source's selection sets, borrowing them from the plan.
    pub(crate) fn source_sets(
        &self,
        source: u32,
    ) -> impl Iterator<Item = &ast::SelectionSet<'a>> + '_ {
        let source = self.sources.get(source as usize);
        source.into_iter().flat_map(|source| {
            source.root_sets.iter().map(|set| &**set).chain(
                source
                    .nodes
                    .iter()
                    .filter_map(|node| node.selection_set.as_ref()),
            )
        })
    }

    /// The field plan for `object_type`, when this source was planned for it.
    #[inline]
    pub(crate) fn field_plan(
        &self,
        source: u32,
        object_type: &ObjectType,
    ) -> Option<&FieldPlan<'a>> {
        self.sources
            .get(source as usize)?
            .types
            .iter()
            .find(|plan| names_eq(&plan.type_name, object_type.name.as_ref()))
    }
}

impl<'a> FieldPlan<'a> {
    /// The number of response keys in this plan.
    pub(crate) fn len(&self) -> usize {
        self.groups.len()
    }

    /// The response-key groups, in first-seen order.
    #[inline(always)]
    pub(crate) fn groups(&'a self) -> &'a [FieldGroup<'a>] {
        &self.groups
    }

    /// Whether `parent` is already exactly this plan's response object.
    ///
    /// Only plans whose response is the identity projection qualify. The check compares response
    /// keys and value shapes, so a hit means the response can reuse the parent's object without
    /// copying it or changing any value.
    #[inline]
    pub(crate) fn matches_parent(&self, parent: &Object) -> bool {
        if !self.identity_projection || self.groups.len() != parent.len() {
            return false;
        }
        let mut fields = parent.iter();
        for group in &self.groups {
            let Some((key, value)) = fields.next() else {
                return false;
            };
            let Some(property) = &group.property else {
                return false;
            };
            if !names_eq(group.response_key, key) || !property.kind.accepts(value) {
                return false;
            }
        }
        true
    }

    /// View one group of this plan.
    pub(crate) fn group_view(
        &'a self,
        index: usize,
        variables: &Object,
        object_type: &'a ObjectType,
    ) -> Option<GroupView<'a, 'a>> {
        let group = self.groups.get(index)?;
        let variant = group.variant(variables)?;
        if variant.nodes.is_empty() {
            return None;
        }
        Some(GroupView {
            response_key: group.response_key,
            alias_key: group.alias_key.as_ref(),
            nodes: GroupNodes::Planned(&variant.nodes),
            field: group
                .field_index
                .and_then(|index| object_type.fields.get_index(index as usize))
                .map(|(_, field)| field),
            args: group.args.as_ref(),
            child: variant.child,
            planned: true,
        })
    }
}

impl<'a> FieldGroup<'a> {
    /// The response key this group contributes.
    pub(crate) fn response_key(&self) -> &'a str {
        self.response_key
    }

    /// The schema field this group resolves to.
    pub(crate) fn field(&self, object_type: &'a ObjectType) -> Option<&'a Field> {
        self.field_index
            .and_then(|index| object_type.fields.get_index(index as usize))
            .map(|(_, field)| field)
    }

    /// The compiled argument template, when the plan built one.
    pub(crate) fn args(&self) -> Option<&ArgsTemplate<'a>> {
        self.args.as_ref()
    }

    /// The response key as an owned `Arc`, reusing the interned alias or schema field name.
    pub(crate) fn key(&self, field: Option<&Field>) -> Arc<str> {
        if let Some(key) = &self.alias_key {
            return Arc::clone(key);
        }
        match field {
            Some(field) => field.name.clone(),
            None => Arc::from(self.response_key),
        }
    }

    /// The variant selected by `variables`.
    #[inline(always)]
    pub(crate) fn variant(&'a self, variables: &Object) -> Option<&'a Variant<'a>> {
        if self.variants.len() == 1 {
            return self.variants.first();
        }
        let mut mask = 0usize;
        let mut bit = 0;
        for node in &self.nodes {
            if node.requirements.is_empty() {
                continue;
            }
            if node.requirements.iter().all(|requirement| {
                variables
                    .get(requirement.variable)
                    .and_then(Value::as_bool)
                    .unwrap_or(false)
                    == requirement.expected
            }) {
                mask |= 1 << bit;
            }
            bit += 1;
        }
        self.variants.get(mask)
    }
}

impl<'a> ArgsTemplate<'a> {
    /// Fill the argument object for one request.
    ///
    /// Reproduces `coerce_argument_values_into` for the cases the template models: defaulted and
    /// literal arguments, and top-level variable references.
    pub(crate) fn fill(
        &self,
        variables: &Object,
        coerced: &mut Object,
    ) -> Result<(), GraphQLError> {
        for slot in &self.slots {
            match slot {
                ArgSlot::Literal { name, value } => {
                    coerced.push_unchecked(name.clone(), value.clone());
                }
                ArgSlot::Variable {
                    name,
                    variable,
                    ty,
                    location,
                    default,
                } => match variables.get(variable) {
                    Some(value) => {
                        if value.is_null() && ty.is_non_null() {
                            return Err(GraphQLError::new(format!(
                                "Field \"{}\" argument \"{}\" of non-null type \"{}\" must not be null.",
                                self.field_name, name, ty
                            ))
                            .at(*location));
                        }
                        coerced.push_unchecked(name.clone(), value.clone());
                    }
                    None => {
                        if let Some(value) = default {
                            coerced.push_unchecked(name.clone(), value.clone());
                        } else if ty.is_non_null() {
                            return Err(GraphQLError::new(format!(
                                "Field \"{}\" argument \"{}\" of required type \"{}\" was not provided because variable \"${}\" was not provided.",
                                self.field_name, name, ty, variable
                            ))
                            .at(*location));
                        }
                    }
                },
            }
        }
        Ok(())
    }
}

/// A plan with no sources, used when the operation has no root type to plan against.
pub(crate) fn build_empty<'a>() -> PreparedPlan<'a> {
    PreparedPlan {
        sources: Vec::new(),
        root: None,
        depth: 0,
    }
}

/// Build a collection plan for `operation`.
///
/// Planning never fails: anything it cannot model exactly simply stays unplanned, and the executor
/// falls back to dynamic collection for that selection.
pub(crate) fn build<'a>(
    schema: &Schema,
    document: &ast::Document<'a>,
    operation: &ast::OperationDefinition<'a>,
    root_type: &ObjectType,
) -> PreparedPlan<'a> {
    let fragments: Vec<&ast::FragmentDefinition<'a>> = document.fragments().collect();
    let mut builder = Builder {
        schema,
        fragments,
        sources: vec![Source {
            nodes: Vec::new(),
            root_sets: vec![Arc::new(operation.selection_set.clone())],
            identity: vec![std::ptr::from_ref(&operation.selection_set) as usize],
            types: Vec::new(),
            unplannable: false,
        }],
        fields: FxHashMap::default(),
        queue: VecDeque::new(),
        plan_count: 0,
        exhausted: false,
    };
    builder.queue.push_back((0, root_type.name.clone()));

    while let Some((source, type_name)) = builder.queue.pop_front() {
        if builder.exhausted || builder.plan_count >= MAX_PLANS {
            break;
        }
        if builder.sources[source as usize].unplannable
            || builder.sources[source as usize]
                .types
                .iter()
                .any(|plan| names_eq(&plan.type_name, &type_name))
        {
            continue;
        }
        let Some(object_type) = schema.object_type(type_name.as_ref()).cloned() else {
            continue;
        };
        match builder.plan(source, &object_type) {
            Ok(plan) => {
                builder.sources[source as usize].types.push(plan);
                builder.plan_count += 1;
            }
            Err(PlanError::Budget) => {
                // Leave this and every later selection to dynamic collection.
                builder.exhausted = true;
            }
            Err(PlanError::Unsupported) => {
                // Only this source is unrepresentable; other sources keep planning.
                builder.sources[source as usize].unplannable = true;
            }
        }
    }

    PreparedPlan {
        depth: plan_depth(&builder.sources),
        sources: builder.sources,
        root: Some(0),
    }
}

/// The longest chain of planned selection sources reachable from the root.
///
/// Sources are interned as a graph: a source can be reached from several parents, and a
/// pathological document can even point back at an ancestor. A source whose plan is missing
/// (unplannable, budget exhausted) ends the planned recursion there, but counting it as a level
/// only over-estimates, which is the safe direction for the caller's stack check.
fn plan_depth(sources: &[Source<'_>]) -> usize {
    fn visit(sources: &[Source<'_>], id: u32, state: &mut [u8], memo: &mut [usize]) -> usize {
        let index = id as usize;
        if index >= sources.len() {
            return 0;
        }
        match state[index] {
            2 => return memo[index],
            // A cycle already being visited contributes no additional levels of its own.
            1 => return 1,
            _ => {}
        }
        state[index] = 1;
        let mut depth = 1;
        for plan in &sources[index].types {
            for group in &plan.groups {
                for variant in &group.variants {
                    let Some(child) = variant.child else {
                        continue;
                    };
                    if child as usize == index {
                        continue;
                    }
                    depth = depth.max(1 + visit(sources, child, state, memo));
                }
            }
        }
        state[index] = 2;
        memo[index] = depth;
        depth
    }

    if sources.is_empty() {
        return 0;
    }
    visit(
        sources,
        0,
        &mut vec![0u8; sources.len()],
        &mut vec![0usize; sources.len()],
    )
}

enum PlanError {
    /// This source uses a construct the plan does not model.
    Unsupported,
    /// The plan size budget is exhausted.
    Budget,
}

struct Builder<'a, 's, 'f> {
    schema: &'s Schema,
    fragments: Vec<&'f ast::FragmentDefinition<'a>>,
    sources: Vec<Source<'a>>,
    /// Field nodes cloned into the plan, keyed by their address in the document.
    ///
    /// A node can appear in several sources (fragments, abstract types); cloning it once keeps
    /// preparation linear in the size of the operation.
    fields: FxHashMap<usize, Arc<ast::Field<'a>>>,
    queue: VecDeque<(u32, Name)>,
    plan_count: usize,
    exhausted: bool,
}

impl<'a> Builder<'a, '_, '_> {
    /// Plan one source for one object type.
    fn plan(&mut self, source: u32, object_type: &ObjectType) -> Result<FieldPlan<'a>, PlanError> {
        let (root_sets, source_nodes) = {
            let source = &self.sources[source as usize];
            (source.root_sets.clone(), source.nodes.clone())
        };
        let mut groups: Vec<FieldGroup<'a>> = Vec::new();
        let mut visited: SmallVec<[&'a str; 8]> = SmallVec::new();
        let mut work = 0u64;
        let mut tasks: SmallVec<[(u32, Name); 4]> = SmallVec::new();
        let schema = self.schema;

        for selection_set in root_sets.iter().map(|set| set.as_ref()).chain(
            source_nodes
                .iter()
                .filter_map(|node| node.selection_set.as_ref()),
        ) {
            visit_selection_set(
                schema,
                &self.fragments,
                &mut self.fields,
                object_type,
                selection_set,
                &[],
                &mut visited,
                &mut groups,
                0,
                &mut work,
            )?;
        }

        for group in &mut groups {
            let field = group
                .field_index
                .and_then(|index| object_type.fields.get_index(index as usize))
                .map(|(_, field)| field);

            // Compile the arguments once; anything not modelled keeps the general path.
            group.args = match field {
                Some(field) => match compile_args(self.schema, field, &group.nodes) {
                    Ok(args) => args,
                    Err(()) => None,
                },
                None => None,
            };
            group.property = field
                .filter(|_| group.alias_key.is_none())
                .and_then(|field| property(self.schema, field));

            let conditional = group
                .nodes
                .iter()
                .filter(|node| !node.requirements.is_empty())
                .count();
            if conditional > MAX_CONDITIONAL_NODES {
                return Err(PlanError::Unsupported);
            }

            let child_types = field.map(|field| child_type_names(self.schema, &field.ty));
            if conditional == 0 {
                let nodes: Vec<PlanNodeRef<'a>> = group
                    .nodes
                    .iter()
                    .map(|node| PlanNodeRef {
                        node: node.node.clone(),
                        origin: node.origin,
                        included: true,
                    })
                    .collect();
                let child = self.intern_nodes(&nodes);
                group.variants.push(Variant {
                    nodes: nodes.into_iter().map(|node| node.node).collect(),
                    child,
                });
                if let (Some(child), Some(types)) = (child, &child_types) {
                    for name in types {
                        tasks.push((child, name.clone()));
                    }
                }
            } else {
                let combinations = 1usize << conditional;
                for mask in 0..combinations {
                    let mut nodes: Vec<PlanNodeRef<'a>> = Vec::with_capacity(group.nodes.len());
                    let mut bit = 0;
                    for node in &group.nodes {
                        let included = if node.requirements.is_empty() {
                            true
                        } else {
                            let included = mask & (1 << bit) != 0;
                            bit += 1;
                            included
                        };
                        nodes.push(PlanNodeRef {
                            node: node.node.clone(),
                            origin: node.origin,
                            included,
                        });
                    }
                    let child = self.intern_nodes(&nodes);
                    group.variants.push(Variant {
                        nodes: nodes
                            .iter()
                            .filter(|node| node.included)
                            .map(|node| node.node.clone())
                            .collect(),
                        child,
                    });
                    if let (Some(child), Some(types)) = (child, &child_types) {
                        for name in types {
                            tasks.push((child, name.clone()));
                        }
                    }
                }
            }
        }

        let has_resolvers = object_type
            .fields
            .values()
            .any(|field| field.resolve.is_some());
        // A plan can stand in for its parent object only when the response is the identity
        // projection: every group is one unconditional, unaliased built-in scalar property.
        let identity_projection = groups.iter().all(|group| {
            group.property.is_some()
                && group.variants.len() == 1
                && group.variants[0].nodes.len() == 1
        });

        for (source, name) in tasks {
            self.queue.push_back((source, name));
        }

        Ok(FieldPlan {
            type_name: object_type.name.clone(),
            groups,
            has_resolvers,
            identity_projection,
        })
    }

    /// Intern the union of the nodes' sub-selections.
    ///
    /// A source whose nodes have no sub-selections is `None`, which leaves the completion error for
    /// a missing selection set to the executor.
    fn intern_nodes(&mut self, nodes: &[PlanNodeRef<'a>]) -> Option<u32> {
        let selected: Vec<&PlanNodeRef<'a>> = nodes
            .iter()
            .filter(|node| node.included && node.node.selection_set.is_some())
            .collect();
        if selected.is_empty() {
            return None;
        }
        let mut identity: Vec<usize> = selected.iter().map(|node| node.origin).collect();
        identity.sort_unstable();
        if let Some(index) = self
            .sources
            .iter()
            .position(|source| !source.unplannable && source.identity == identity)
        {
            return Some(index as u32);
        }
        if self.sources.len() >= MAX_SOURCES {
            return None;
        }
        self.sources.push(Source {
            nodes: selected.iter().map(|node| node.node.clone()).collect(),
            root_sets: Vec::new(),
            identity,
            types: Vec::new(),
            unplannable: false,
        });
        Some((self.sources.len() - 1) as u32)
    }
}

/// Walk a selection set the way `CollectFields` does, recording groups instead of materializing
/// them per request.
#[allow(clippy::too_many_arguments)]
fn visit_selection_set<'s, 'a>(
    schema: &Schema,
    fragments: &[&'s ast::FragmentDefinition<'a>],
    fields: &mut FxHashMap<usize, Arc<ast::Field<'a>>>,
    object_type: &ObjectType,
    selection_set: &'s ast::SelectionSet<'a>,
    inherited: &[Requirement<'a>],
    visited: &mut SmallVec<[&'a str; 8]>,
    groups: &mut Vec<FieldGroup<'a>>,
    depth: usize,
    work: &mut u64,
) -> Result<(), PlanError> {
    for selection in &selection_set.items {
        *work += 1;
        if *work > collect::MAX_COLLECT_WORK {
            return Err(PlanError::Budget);
        }
        let own = condition(selection.directives());
        let Condition::Include(own) = own else {
            continue;
        };
        match selection {
            ast::Selection::Field(field) => {
                let mut requirements: Vec<Requirement<'a>> = Vec::new();
                requirements.extend(inherited.iter().copied());
                requirements.extend(own.iter().copied());
                add_field(groups, fields, object_type, field, requirements);
            }
            ast::Selection::InlineFragment(fragment) => {
                if let Some(condition) = fragment.type_condition {
                    if !type_condition_matches(schema, condition, object_type.name.as_ref())
                        .map_err(|_| PlanError::Unsupported)?
                    {
                        continue;
                    }
                }
                let mut combined: SmallVec<[Requirement<'a>; 2]> =
                    inherited.iter().copied().collect();
                combined.extend(own.iter().copied());
                visit_selection_set(
                    schema,
                    fragments,
                    fields,
                    object_type,
                    &fragment.selection_set,
                    &combined,
                    visited,
                    groups,
                    depth,
                    work,
                )?;
            }
            ast::Selection::FragmentSpread(spread) => {
                // A spread's expansion marks its name as visited only when it is included, so a
                // conditional spread (or a spread inside a conditional fragment) can change what
                // a later spread expands. That interaction is not worth modelling; keep the
                // dynamic path for it.
                if !own.is_empty() || !inherited.is_empty() {
                    return Err(PlanError::Unsupported);
                }
                if visited.iter().any(|name| names_eq(name, spread.name)) {
                    continue;
                }
                visited.push(spread.name);
                let Some(fragment) = fragments
                    .iter()
                    .find(|fragment| names_eq(fragment.name, spread.name))
                    .copied()
                else {
                    return Err(PlanError::Unsupported);
                };
                if depth >= collect::MAX_COLLECT_FRAGMENT_DEPTH {
                    return Err(PlanError::Unsupported);
                }
                if !type_condition_matches(
                    schema,
                    fragment.type_condition,
                    object_type.name.as_ref(),
                )
                .map_err(|_| PlanError::Unsupported)?
                {
                    continue;
                }
                visit_selection_set(
                    schema,
                    fragments,
                    fields,
                    object_type,
                    &fragment.selection_set,
                    inherited,
                    visited,
                    groups,
                    depth + 1,
                    work,
                )?;
            }
        }
    }
    Ok(())
}

/// Append a field node to its response key's group, creating the group if needed.
///
/// The node is cloned into the plan's field interner; the walk itself only ever reads the document.
fn add_field<'a>(
    groups: &mut Vec<FieldGroup<'a>>,
    fields: &mut FxHashMap<usize, Arc<ast::Field<'a>>>,
    object_type: &ObjectType,
    field: &ast::Field<'a>,
    requirements: Vec<Requirement<'a>>,
) {
    let response_key = field.response_key();
    let index = match groups
        .iter()
        .position(|group| names_eq(group.response_key, response_key))
    {
        Some(index) => index,
        None => {
            let field_index = object_type
                .fields
                .get_index_of(field.name)
                .and_then(|index| u32::try_from(index).ok());
            groups.push(FieldGroup {
                response_key,
                alias_key: field.alias.map(|_| Arc::from(response_key)),
                field_index,
                nodes: Vec::new(),
                variants: Vec::new(),
                args: None,
                property: None,
            });
            groups.len() - 1
        }
    };
    groups[index].nodes.push(PlanNode {
        node: intern_field(fields, field),
        origin: std::ptr::from_ref(field) as usize,
        requirements,
    });
}

/// Clone a document field node into the plan, once per node.
fn intern_field<'a>(
    fields: &mut FxHashMap<usize, Arc<ast::Field<'a>>>,
    field: &ast::Field<'a>,
) -> Arc<ast::Field<'a>> {
    let address = std::ptr::from_ref(field) as usize;
    if let Some(node) = fields.get(&address) {
        return Arc::clone(node);
    }
    let node = Arc::new(field.clone());
    fields.insert(address, Arc::clone(&node));
    node
}

/// The `@skip`/`@include` decision for one selection.
enum Condition<'a> {
    /// The selection is included when every requirement holds.
    Include(SmallVec<[Requirement<'a>; 1]>),
    /// The selection can never be included.
    Exclude,
}

fn condition<'a>(directives: &[ast::Directive<'a>]) -> Condition<'a> {
    let mut requirements = SmallVec::new();
    for directive in directives {
        match directive.name {
            "skip" => {
                if let Some(argument) = directive.argument("if") {
                    match argument.value {
                        ast::Value::Boolean(true) => return Condition::Exclude,
                        ast::Value::Variable(variable) => requirements.push(Requirement {
                            variable,
                            expected: false,
                        }),
                        _ => {}
                    }
                }
            }
            "include" => {
                if let Some(argument) = directive.argument("if") {
                    match argument.value {
                        ast::Value::Boolean(false) => return Condition::Exclude,
                        ast::Value::Variable(variable) => requirements.push(Requirement {
                            variable,
                            expected: true,
                        }),
                        _ => {}
                    }
                }
            }
            _ => {}
        }
    }
    Condition::Include(requirements)
}

/// Describe an unaliased, resolver-free built-in scalar property.
///
/// Anything that could observe the field (a resolver, arguments, a custom scalar, a composite or
/// list type) returns `None` and keeps the general completion path.
fn property(schema: &Schema, field: &Field) -> Option<Property> {
    if field.resolve.is_some() || !field.args.is_empty() {
        return None;
    }
    let mut ty = &field.ty;
    while let TypeRef::NonNull(inner) = ty {
        ty = inner;
    }
    let TypeRef::Named(name) = ty else {
        return None;
    };
    let Some(Type::Scalar(scalar)) = schema.get_type(name) else {
        return None;
    };
    if !scalar.builtin {
        return None;
    }
    let kind = match name.as_ref() {
        "String" | "ID" => ScalarKind::String,
        "Int" => ScalarKind::Int,
        "Boolean" => ScalarKind::Boolean,
        "Float" => ScalarKind::Float,
        _ => return None,
    };
    Some(Property {
        name: field.name.clone(),
        key: field.name.clone(),
        kind,
    })
}

/// Compile a group's arguments into a per-request template.
///
/// `Ok(None)` means there is nothing to coerce. `Err(())` means this group must keep the general
/// argument coercion path.
fn compile_args<'a>(
    schema: &Schema,
    field: &Field,
    nodes: &[PlanNode<'a>],
) -> Result<Option<ArgsTemplate<'a>>, ()> {
    if field.args.is_empty() {
        return Ok(None);
    }
    let node = nodes.first().ok_or(())?.node.as_ref();
    for argument in &node.arguments {
        if !field.args.contains_key(argument.name) {
            return Err(());
        }
        if !matches!(argument.value, ast::Value::Variable(_)) && contains_variable(&argument.value)
        {
            // A variable nested inside a literal (an input object or list) needs the general
            // path, which knows how to coerce the whole value.
            return Err(());
        }
    }

    let mut slots: Vec<ArgSlot<'a>> = Vec::with_capacity(field.args.len());
    for (name, definition) in &field.args {
        match node.argument(name) {
            Some(argument) => match &argument.value {
                ast::Value::Variable(variable) => {
                    let default = match &definition.default_value {
                        Some(default) => Some(coerce_runtime(schema, default, definition)?),
                        None => None,
                    };
                    slots.push(ArgSlot::Variable {
                        name: name.clone(),
                        variable,
                        ty: definition.ty.clone(),
                        location: argument.location,
                        default,
                    });
                }
                value => {
                    let value = coerce_literal(schema, value, definition)?;
                    slots.push(ArgSlot::Literal {
                        name: name.clone(),
                        value,
                    });
                }
            },
            None => {
                let Some(default) = &definition.default_value else {
                    // A missing required argument is an error the general path must produce.
                    if definition.ty.is_non_null() {
                        return Err(());
                    }
                    continue;
                };
                let value = coerce_runtime(schema, default, definition)?;
                slots.push(ArgSlot::Literal {
                    name: name.clone(),
                    value,
                });
            }
        }
    }
    Ok(Some(ArgsTemplate {
        field_name: field.name.clone(),
        slots,
    }))
}

fn coerce_literal(
    schema: &Schema,
    value: &ast::Value<'_>,
    definition: &InputValue,
) -> Result<Value, ()> {
    coerce::coerce_literal(schema, value, &definition.ty, None).map_err(|_| ())
}

/// Validate an already-runtime default value against its input type, as argument coercion does.
fn coerce_runtime(schema: &Schema, value: &Value, definition: &InputValue) -> Result<Value, ()> {
    let mut path = Vec::new();
    coerce::coerce_input_value(schema, value, &definition.ty, &mut path).map_err(|_| ())
}

fn contains_variable(value: &ast::Value<'_>) -> bool {
    match value {
        ast::Value::Variable(_) => true,
        ast::Value::List(items) => items.iter().any(contains_variable),
        ast::Value::Object(fields) => fields.iter().any(|field| contains_variable(&field.value)),
        _ => false,
    }
}

/// The concrete object types a field's output type can complete as.
fn child_type_names(schema: &Schema, ty: &TypeRef) -> Vec<Name> {
    let mut current = ty;
    loop {
        current = match current {
            TypeRef::NonNull(inner) => inner,
            TypeRef::List(inner) => inner,
            TypeRef::Named(_) => break,
        };
    }
    let TypeRef::Named(name) = current else {
        return Vec::new();
    };
    match schema.get_type(name) {
        Some(Type::Object(object)) => vec![object.name.clone()],
        Some(Type::Interface(_) | Type::Union(_)) => schema.possible_types(name),
        _ => Vec::new(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{
        Field, Object, ObjectType, Schema, TypeRef, ValidationOptions, parser::parse_query,
    };

    fn schema() -> Schema {
        let user = ObjectType::new("User")
            .field(Field::new("id", TypeRef::id().non_null()))
            .field(Field::new("name", TypeRef::string().non_null()))
            .field(Field::new("age", TypeRef::int()))
            .field(Field::new(
                "friends",
                TypeRef::named("User").non_null().list().non_null(),
            ));
        let query = ObjectType::new("Query")
            .field(
                Field::new("user", TypeRef::named("User"))
                    .arg(InputValue::new("id", TypeRef::id()))
                    .resolve(|context| {
                        let parent = context.parent.clone();
                        Box::pin(async move { Ok(parent) })
                    }),
            )
            .field(
                Field::new("health", TypeRef::string().non_null())
                    .resolve(|_| Box::pin(async { Ok(Value::from("ok")) })),
            );
        let mut builder = Schema::builder();
        builder.add_type(user.into());
        builder.add_type(query.into());
        builder.query("Query").build().expect("valid schema")
    }

    fn build_plan<'a>(schema: &Schema, document: &'a ast::Document<'a>) -> PreparedPlan<'a> {
        let errors = crate::validate(
            schema,
            document,
            &ValidationOptions {
                introspection: true,
                max_depth: 0,
                max_errors: 100,
            },
        );
        assert!(errors.is_empty(), "{errors:?}");
        let operation = document.operations().next().expect("operation");
        build(schema, document, operation, schema.query_type())
    }

    fn variables(fields: impl IntoIterator<Item = (&'static str, Value)>) -> Object {
        Object::from_fields(fields)
    }

    #[test]
    fn concrete_paths_are_planned() {
        let schema = schema();
        let document = parse_query(
            r#"
            query {
              user(id: "1") {
                id
                called: name
                ...UserFields
                friends { id name }
              }
              health
            }
            fragment UserFields on User { age }
            "#,
        )
        .expect("parses");
        let plan = build_plan(&schema, &document);

        let root = plan
            .field_plan(0, schema.query_type())
            .expect("root is planned");
        assert!(root.has_resolvers);
        let keys: Vec<_> = root.groups.iter().map(|group| group.response_key).collect();
        assert_eq!(keys, ["user", "health"]);

        let root_variant = root.groups[0].variant(&Object::new()).expect("variant");
        let child = root_variant.child.expect("user has a child source");
        let user = schema.object_type("User").expect("user type");
        let user_plan = plan.field_plan(child, user).expect("user is planned");
        assert!(!user_plan.has_resolvers);
        let keys: Vec<_> = user_plan
            .groups
            .iter()
            .map(|group| group.response_key)
            .collect();
        // Aliases are separate response keys; the fragment's `age` merges into its own group in
        // document order.
        assert_eq!(keys, ["id", "called", "age", "friends"]);
        // The alias is interned once, and the merged fragment selection is a single node.
        assert!(user_plan.groups[1].alias_key.is_some());
        assert_eq!(user_plan.groups[2].nodes.len(), 1);
        assert!(user_plan.groups[0].property.is_some());
        assert!(user_plan.groups[3].property.is_none());
    }

    #[test]
    fn exact_projection_reuses_the_parent_object() {
        let schema = schema();
        let document = parse_query(
            r#"
            query {
              user { id name age }
            }
            "#,
        )
        .expect("parses");
        let plan = build_plan(&schema, &document);
        let root = plan.field_plan(0, schema.query_type()).expect("root");
        let child = root.groups[0]
            .variant(&Object::new())
            .expect("variant")
            .child
            .expect("child source");
        let user = schema.object_type("User").expect("user type");
        let user_plan = plan.field_plan(child, user).expect("user plan");
        assert!(user_plan.identity_projection);

        let exact = Object::from_fields([
            ("id", Value::from("1")),
            ("name", Value::from("Alice")),
            ("age", Value::Int(30)),
        ]);
        assert!(user_plan.matches_parent(&exact));

        // A different order, an extra key, a missing key, or a value of the wrong shape keeps
        // the general completion path.
        let reordered = Object::from_fields([
            ("name", Value::from("Alice")),
            ("id", Value::from("1")),
            ("age", Value::Int(30)),
        ]);
        assert!(!user_plan.matches_parent(&reordered));
        let extra = Object::from_fields([
            ("id", Value::from("1")),
            ("name", Value::from("Alice")),
            ("age", Value::Int(30)),
            ("friends", Value::List(Vec::new())),
        ]);
        assert!(!user_plan.matches_parent(&extra));
        let null_age = Object::from_fields([
            ("id", Value::from("1")),
            ("name", Value::from("Alice")),
            ("age", Value::Null),
        ]);
        assert!(!user_plan.matches_parent(&null_age));
    }

    #[test]
    fn conditional_groups_precompute_variants() {
        let schema = schema();
        let document = parse_query(
            r#"
            query ($show: Boolean!, $hide: Boolean!) {
              user {
                id
                name @include(if: $show)
                age @skip(if: $hide)
              }
            }
            "#,
        )
        .expect("parses");
        let plan = build_plan(&schema, &document);
        let root = plan.field_plan(0, schema.query_type()).expect("root");
        let child = root.groups[0]
            .variant(&Object::new())
            .expect("variant")
            .child
            .expect("child source");
        let user = schema.object_type("User").expect("user type");
        let user_plan = plan.field_plan(child, user).expect("user plan");

        let name = &user_plan.groups[1];
        assert_eq!(name.variants.len(), 2);
        assert!(
            name.variant(&Object::new())
                .expect("mask 0")
                .nodes
                .is_empty()
        );
        assert_eq!(
            name.variant(&variables([("show", Value::Boolean(true))]))
                .expect("mask 1")
                .nodes
                .len(),
            1
        );

        let age = &user_plan.groups[2];
        assert_eq!(age.variants.len(), 2);
        assert_eq!(
            age.variant(&variables([("hide", Value::Boolean(false))]))
                .expect("included")
                .nodes
                .len(),
            1
        );
        assert!(
            age.variant(&variables([("hide", Value::Boolean(true))]))
                .expect("excluded")
                .nodes
                .is_empty()
        );
    }

    #[test]
    fn conditional_fragment_spread_stays_dynamic() {
        let schema = schema();
        let document = parse_query(
            r#"
            query ($on: Boolean!) {
              user { ...Fields @include(if: $on) ...Fields }
            }
            fragment Fields on User { id name }
            "#,
        )
        .expect("parses");
        let plan = build_plan(&schema, &document);
        let root = plan.field_plan(0, schema.query_type()).expect("root");
        let child = root.groups[0]
            .variant(&Object::new())
            .expect("variant")
            .child
            .expect("child source");
        let user = schema.object_type("User").expect("user type");
        assert!(plan.field_plan(child, user).is_none());
        // The source keeps its selection sets so execution can fall back to `CollectFields`.
        assert!(plan.source_sets(child).next().is_some());
    }

    #[test]
    fn argument_templates_are_compiled_only_when_static() {
        let schema = schema();
        let document = parse_query(
            r#"
            query ($id: ID) {
              a: user(id: "1") { id }
              b: user(id: $id) { id }
            }
            "#,
        )
        .expect("parses");
        let plan = build_plan(&schema, &document);
        let root = plan.field_plan(0, schema.query_type()).expect("root");
        let template = root.groups[0].args.as_ref().expect("literal template");
        let mut args = Object::new();
        template.fill(&Object::new(), &mut args).expect("fills");
        assert_eq!(args.get("id"), Some(&Value::from("1")));

        let template = root.groups[1].args.as_ref().expect("variable template");
        let mut args = Object::new();
        template
            .fill(&variables([("id", Value::from("7"))]), &mut args)
            .expect("fills");
        assert_eq!(args.get("id"), Some(&Value::from("7")));
        // An absent nullable variable omits the argument, exactly like the dynamic path.
        let mut args = Object::new();
        template.fill(&Object::new(), &mut args).expect("fills");
        assert!(args.get("id").is_none());
    }
}
