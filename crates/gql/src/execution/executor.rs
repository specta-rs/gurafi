//! Execution engine.
//!
//! Implements the October 2021 specification's `ExecuteRequest`, `ExecuteSelectionSet` and
//! `CompleteValue` algorithms. Field completion returns an error that has *not* been recorded yet:
//! the nearest nullable position decides whether to record it (and produce `null`) or let it
//! bubble through a non-null position. That is exactly the specification's null propagation rule,
//! and it keeps each error reported once, with the path where it originated.

use std::{
    borrow::Cow,
    fmt,
    sync::{Arc, Mutex},
    time::Instant,
};

use futures::{
    Future, StreamExt,
    future::{BoxFuture, Either, FutureExt, ready},
    stream,
};
use rustc_hash::FxHashMap;
use smallvec::SmallVec;

use crate::{
    Object, Value,
    ast::{self, OperationKind},
    complexity::{self, ComplexityOptions, StaticComplexity},
    error::{GraphQLError, Location, Path, PathSegment, Response},
    introspection,
    parser::parse_query,
    schema::Schema,
    types::{ObjectType, ResolveInfo, ResolverContext, ScalarType, Type, TypeRef},
    validation::{self, ValidationOptions},
};

use super::{
    AuthorizationContext, ExecutionContext, ExecutionOptions, Request, coerce,
    collect::{self, CollectedFields, Selections},
    plan::{self, GroupView, PreparedPlan},
};

/// How many nested selection-set levels the synchronous default-completion path may enter.
///
/// Default-resolved objects need no `await`, so most of a response can be built without boxing a
/// future per object. Recursing without bound would make the stack a function of the document's
/// depth, so past this many levels completion falls back to the boxed asynchronous path — the same
/// path (and the same stack bound) a document at the parser's depth limit already uses.
const SYNC_DEPTH_LIMIT: usize = 12;

/// A map of fragment definitions keyed by name, borrowed from the parsed document.
///
/// Documents usually define only a handful of fragments, and execution looks them up once per
/// selection set, so an inline list is cheaper than a hash map. Documents that define many
/// fragments fall back to hashing to keep lookup cost independent of their number.
pub(crate) enum FragmentMap<'a> {
    /// Few enough fragments that a linear scan wins.
    Small(SmallVec<[(&'a str, &'a ast::FragmentDefinition<'a>); 8]>),
    /// Many fragments, indexed by name.
    Large(FxHashMap<&'a str, &'a ast::FragmentDefinition<'a>>),
}

impl<'a> FragmentMap<'a> {
    /// Build a fragment lookup for a parsed document.
    pub(crate) fn new(document: &'a ast::Document<'a>) -> Self {
        let mut small = SmallVec::new();
        let mut fragments = document.fragments();
        for fragment in fragments.by_ref() {
            if small.len() == small.inline_size() {
                let mut large: FxHashMap<_, _> = small.iter().copied().collect();
                large.insert(fragment.name, fragment);
                for fragment in fragments {
                    large.insert(fragment.name, fragment);
                }
                return Self::Large(large);
            }
            small.push((fragment.name, fragment));
        }
        Self::Small(small)
    }

    /// Look up a fragment definition by name.
    pub(crate) fn get(&self, name: &str) -> Option<&'a ast::FragmentDefinition<'a>> {
        match self {
            Self::Small(fragments) => fragments
                .iter()
                .find(|(known, _)| crate::types::names_eq(known, name))
                .map(|(_, fragment)| *fragment),
            Self::Large(fragments) => fragments.get(name).copied(),
        }
    }
}

/// How a field is named in an error message.
///
/// Formatting `"Parent.field"` eagerly allocates a string for every field of every request, even
/// though only a failing field ever reads it. Keeping the two borrowed names around until an
/// error is built removes that allocation (and the `Arc` it used to live in).
#[derive(Clone, Copy)]
struct FieldLabel<'a> {
    parent: &'a str,
    name: &'a str,
}

impl fmt::Display for FieldLabel<'_> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{}.{}", self.parent, self.name)
    }
}

/// Execute a parsed and validated operation.
pub(crate) async fn execute(
    schema: &Schema,
    request: Request<'_>,
    options: &ExecutionOptions,
) -> Response {
    let document = match parse_query(request.query) {
        Ok(document) => document,
        Err(error) => return Response::error(error),
    };
    let validation_errors = validation::validate(
        schema,
        &document,
        &ValidationOptions {
            introspection: options.introspection,
            max_depth: options.max_depth,
            max_errors: options.max_errors,
        },
    );
    if !validation_errors.is_empty() {
        return Response {
            data: None,
            errors: validation_errors,
            extensions: None,
        };
    }

    let operation = match select_operation(&document, request.operation_name) {
        Ok(operation) => operation,
        Err(error) => return Response::error(error),
    };

    execute_operation(schema, &document, operation, request.variables, options).await
}

/// Execute an operation that has already been parsed, validated and selected.
///
/// This is the shared tail of both request execution paths. A prepared query stores the parsed
/// document and the selected operation, so it enters here directly and never repeats parsing,
/// validation or operation selection. Everything that depends on the request (variables,
/// complexity limits, introspection policy and execution options) is still applied per call, so
/// prepared execution is subject to exactly the same limits and authorization as normal execution.
pub(crate) async fn execute_operation<'doc>(
    schema: &'doc Schema,
    document: &'doc ast::Document<'doc>,
    operation: &'doc ast::OperationDefinition<'doc>,
    request_variables: Option<&Object>,
    options: &ExecutionOptions,
) -> Response {
    let variables = match coerce::coerce_variable_values(schema, operation, request_variables) {
        Ok(variables) => variables,
        Err(errors) => {
            return Response {
                data: None,
                errors,
                extensions: None,
            };
        }
    };

    if let Err(error) = complexity::analyze_operation(
        schema,
        document,
        operation,
        &variables.values,
        &ComplexityOptions {
            list_factor: options.list_complexity,
            field_cost: 1,
            max_complexity: options.max_complexity,
            max_depth: options.max_depth,
            max_breadth: options.max_breadth,
        },
    ) {
        return Response::error(error);
    }

    let root_name = match operation.kind {
        OperationKind::Query => schema.query_name().clone(),
        OperationKind::Mutation => match schema.mutation_name() {
            Some(name) => name.clone(),
            None => {
                return Response::error(GraphQLError::new(
                    "Schema is not configured for mutations.",
                ));
            }
        },
        OperationKind::Subscription => {
            return Response::error(GraphQLError::new(
                "Subscriptions must be executed with `subscribe`, not `execute`.",
            ));
        }
    };
    let Some(root_type) = schema.object_type(&root_name) else {
        return Response::error(GraphQLError::new(format!(
            "The root type \"{root_name}\" is not an object type."
        )));
    };

    finish_operation(
        schema,
        document,
        operation,
        operation.name.map(Arc::from),
        None,
        root_type,
        variables.values,
        options,
    )
    .await
}

/// Execute an operation prepared ahead of time, reusing its precomputed metadata.
///
/// This is the prepared counterpart of [`execute_operation`]. The document, selected operation,
/// operation name, root type and a conservative complexity bound are all fixed at preparation, so
/// only the request's variables and options are consulted here. Variable coercion, authorization
/// and all limits are still applied exactly as for normal execution.
#[allow(clippy::too_many_arguments)]
pub(crate) async fn execute_prepared_operation<'doc>(
    schema: &'doc Schema,
    document: &'doc ast::Document<'doc>,
    operation: &'doc ast::OperationDefinition<'doc>,
    operation_name: Option<Arc<str>>,
    plan: &'doc PreparedPlan<'doc>,
    root_type: &'doc ObjectType,
    static_complexity: Option<&StaticComplexity>,
    compiled_variables: Option<&coerce::CompiledVariables>,
    request_variables: Option<&Object>,
    options: &ExecutionOptions,
) -> Response {
    let variables = match match compiled_variables {
        Some(compiled) => compiled.coerce(schema, request_variables),
        None => coerce::coerce_variable_values(schema, operation, request_variables),
    } {
        Ok(variables) => variables,
        Err(errors) => {
            return Response {
                data: None,
                errors,
                extensions: None,
            };
        }
    };

    // A prepared document's expansion is fixed, so a bound computed at preparation proves the
    // limits for every request whose options fall inside it. The exact analysis still runs when the
    // bound is unknown or exceeds a limit, preserving the usual error messages.
    if prepared_needs_exact_analysis(static_complexity, options) {
        if let Err(error) = complexity::analyze_operation(
            schema,
            document,
            operation,
            &variables.values,
            &ComplexityOptions {
                list_factor: options.list_complexity,
                field_cost: 1,
                max_complexity: options.max_complexity,
                max_depth: options.max_depth,
                max_breadth: options.max_breadth,
            },
        ) {
            return Response::error(error);
        }
    }

    // Executing a subscription is always an error, exactly as on the normal path.
    if operation.kind == OperationKind::Subscription {
        return Response::error(GraphQLError::new(
            "Subscriptions must be executed with `subscribe`, not `execute`.",
        ));
    }

    finish_operation(
        schema,
        document,
        operation,
        operation_name,
        Some(plan),
        root_type,
        variables.values,
        options,
    )
    .await
}

/// Whether a prepared operation still needs the exact per-request complexity analysis.
pub(crate) fn prepared_needs_exact_analysis(
    static_complexity: Option<&StaticComplexity>,
    options: &ExecutionOptions,
) -> bool {
    if options.max_complexity == 0 && options.max_depth == 0 && options.max_breadth == 0 {
        // No limit is configured, so there is nothing to reject.
        return false;
    }
    match static_complexity {
        Some(bound) => !bound.within_limits(&ComplexityOptions {
            list_factor: options.list_complexity,
            field_cost: 1,
            max_complexity: options.max_complexity,
            max_depth: options.max_depth,
            max_breadth: options.max_breadth,
        }),
        None => true,
    }
}

/// Build the request context and run an operation that is already known to be allowed.
#[allow(clippy::too_many_arguments)]
async fn finish_operation<'doc>(
    schema: &'doc Schema,
    document: &'doc ast::Document<'doc>,
    operation: &'doc ast::OperationDefinition<'doc>,
    operation_name: Option<Arc<str>>,
    plan: Option<&'doc PreparedPlan<'doc>>,
    root_type: &'doc ObjectType,
    variables: Object,
    options: &ExecutionOptions,
) -> Response {
    let context = ExecutionContext {
        schema: schema.clone(),
        user_data: options.context.clone(),
        operation_name,
        variables,
        introspection: options.introspection,
        authorizer: options.authorizer.clone(),
        max_errors: options.max_errors,
        max_concurrency: options.max_concurrency,
        extensions: Mutex::new(None),
    };
    let deadline = options.timeout.map(|timeout| Instant::now() + timeout);

    let fragments = FragmentMap::new(document);
    run_operation(
        &context,
        fragments,
        plan,
        root_type,
        &operation.selection_set,
        options.root_value.clone(),
        deadline,
        matches!(operation.kind, OperationKind::Query),
        false,
    )
    .await
}

/// Select the operation a request names, following the specification exactly.
pub(crate) fn select_operation<'a>(
    document: &'a ast::Document<'a>,
    operation_name: Option<&str>,
) -> Result<&'a ast::OperationDefinition<'a>, GraphQLError> {
    let mut operations = document.operations();
    let Some(first) = operations.next() else {
        return Err(GraphQLError::new("Must provide an operation."));
    };
    match operation_name {
        Some(name) => {
            if first.name == Some(name) {
                return Ok(first);
            }
            operations
                .find(|operation| operation.name == Some(name))
                .ok_or_else(|| GraphQLError::new(format!("Unknown operation named \"{name}\".")))
        }
        None => {
            if operations.next().is_some() {
                Err(GraphQLError::new(
                    "Must provide an operation name if the document contains multiple operations.",
                ))
            } else {
                Ok(first)
            }
        }
    }
}

/// Execute an operation's root selection set and shape the response.
pub(crate) async fn run_operation<'doc>(
    context: &'doc ExecutionContext,
    fragments: FragmentMap<'doc>,
    plan: Option<&'doc PreparedPlan<'doc>>,
    root_type: &'doc ObjectType,
    selection_set: &'doc ast::SelectionSet<'doc>,
    parent: Value,
    deadline: Option<Instant>,
    parallel: bool,
    skip_root_authorization: bool,
) -> Response {
    let executor = Executor {
        context,
        fragments,
        plans: plan,
        deadline,
        parallel,
        skip_root_authorization,
    };
    let mut path = Path::new();
    let mut errors = Vec::new();
    let selection_sets = [selection_set];
    let selections = Selections::new(&selection_sets, plan.and_then(PreparedPlan::root));
    let result = match executor.try_execute_fields_sync(
        root_type,
        &parent,
        &selections,
        &mut path,
        &mut errors,
        0,
    ) {
        Some(result) => result,
        None => {
            let result = if executor.plans.is_some() {
                executor
                    .execute_fields_inner_planned(
                        root_type,
                        &parent,
                        &selections,
                        &mut path,
                        &mut errors,
                        0,
                    )
                    .await
            } else {
                executor
                    .execute_fields_inner_dynamic(
                        root_type,
                        &parent,
                        selections.sets,
                        &mut path,
                        &mut errors,
                        0,
                    )
                    .await
            };
            result.map(Arc::new)
        }
    };

    let data = match result {
        Ok(object) => Some(Value::Object(object)),
        Err(error) => {
            record_error(&mut errors, error, context.max_errors);
            // A failure that bubbles all the way to the root nulls the whole data entry.
            Some(Value::Null)
        }
    };

    Response {
        data,
        errors,
        extensions: take_extensions(context),
    }
}

/// Complete one event of a subscription.
///
/// October 2021 6.2.3.2: each source event is the operation's **root value**. The subscription
/// selection set is executed against that event exactly like a query root, so the root field's
/// normal `resolve` resolver runs (or the default property resolver reads the event's field name).
///
/// The document is re-parsed because the returned response stream is `'static` and therefore
/// cannot borrow the request string; owning a parsed document would require the AST to own its
/// source instead of borrowing it. Validation, variable coercion, complexity analysis and the
/// single-root-field check all happened during setup and are not repeated, and the root field was
/// authorized during setup, so event execution only resolves values with that event's own
/// deadline. Re-parsing per event is a known cost for high-frequency subscriptions.
pub(crate) async fn execute_subscription_event(
    context: &ExecutionContext,
    query: &str,
    operation_name: Option<&str>,
    event: Value,
    deadline: Option<Instant>,
) -> Response {
    let document = match parse_query(query) {
        Ok(document) => document,
        Err(error) => return Response::error(error),
    };
    let operation = match select_operation(&document, operation_name) {
        Ok(operation) => operation,
        Err(error) => return Response::error(error),
    };
    let Some(root_type) = context.schema.subscription_type() else {
        return Response::error(GraphQLError::new(
            "Schema is not configured for subscriptions.",
        ));
    };
    let fragments = FragmentMap::new(&document);
    run_operation(
        context,
        fragments,
        None,
        root_type,
        &operation.selection_set,
        event,
        deadline,
        true,
        true,
    )
    .await
}

struct Executor<'doc> {
    context: &'doc ExecutionContext,
    fragments: FragmentMap<'doc>,
    /// The precompiled collection plan of a prepared operation, when there is one.
    plans: Option<&'doc PreparedPlan<'doc>>,
    deadline: Option<Instant>,
    /// Whether sibling fields at this level may resolve concurrently.
    parallel: bool,
    /// Whether the operation's single root field was already authorized during setup.
    ///
    /// Subscription setup authorizes the root field before subscribing. Every source event then
    /// re-executes the same root field, so running the authorizer again would repeat the same
    /// decision once per event. Nested fields are still authorized for every event.
    skip_root_authorization: bool,
}

impl<'doc> Executor<'doc> {
    fn expired(&self) -> bool {
        self.deadline
            .is_some_and(|deadline| Instant::now() >= deadline)
    }

    async fn execute_fields_inner_planned<'s>(
        &'s self,
        object_type: &'doc ObjectType,
        parent: &'s Value,
        selections: &'s Selections<'_, 'doc>,
        path: &'s mut Path,
        errors: &'s mut Vec<GraphQLError>,
        depth: usize,
    ) -> Result<Object, GraphQLError>
    where
        'doc: 's,
    {
        let collected = collect::collect_fields_multi(
            &self.context.schema,
            &self.fragments,
            &self.context.variables,
            object_type,
            selections,
            self.plans,
        )?;
        let mut result = Object::with_capacity(collected.len());
        if self.parallel
            && collected.len() > 1
            && self.try_execute_all_leaves_planned(
                object_type,
                parent,
                &collected,
                path,
                errors,
                &mut result,
            )?
        {
            return Ok(result);
        }
        if self.parallel && collected.len() > 1 {
            // The batch lives on the heap so the boxed future below stays small enough to
            // recurse safely.
            let mut futures = Vec::with_capacity(collected.len());
            for index in 0..collected.len() {
                let Some(group) = collected.view(index, &self.context.variables, object_type)
                else {
                    continue;
                };
                if self.expired() {
                    return Err(GraphQLError::new(
                        "The operation timed out before it could finish.",
                    )
                    .with_path(path.to_vec()));
                }
                // Leaf fields that need no resolver complete synchronously; `ready` keeps
                // them in the same ordered batch as the asynchronous fields without boxing
                // a future or cloning the path for each one.
                let mut field_errors = Vec::new();
                let inline = self.try_execute_leaf_planned(
                    object_type,
                    parent,
                    &group,
                    path,
                    &mut field_errors,
                );
                let future = match inline {
                    Some((response_key, outcome)) => {
                        Either::Left(ready((response_key, outcome, field_errors)))
                    }
                    None => {
                        let mut field_path = path.clone();
                        Either::Right(async move {
                            let (response_key, outcome) = self
                                .execute_field_planned(
                                    object_type,
                                    parent,
                                    &group,
                                    &mut field_path,
                                    &mut field_errors,
                                    depth,
                                )
                                .await;
                            (response_key, outcome, field_errors)
                        })
                    }
                };
                futures.push(future);
            }
            // `join_all` reaches the same concurrency as `buffered` whenever every sibling
            // fits under the limit, and does not allocate the ordered-stream bookkeeping.
            let max_concurrency = self.context.max_concurrency;
            let outcomes: Vec<_> = if max_concurrency == 0 || futures.len() <= max_concurrency {
                futures::future::join_all(futures).await
            } else {
                buffered(futures, max_concurrency).await
            };
            let mut bubble = None;
            for (response_key, outcome, field_errors) in outcomes {
                append_errors(errors, field_errors, self.context.max_errors);
                match outcome {
                    Ok(value) => result.push_unchecked(response_key, value),
                    // A non-null field failed; keep collecting the other fields' errors, but
                    // the object itself becomes null.
                    Err(error) => {
                        if bubble.is_none() {
                            bubble = Some(error);
                        }
                    }
                }
            }
            if let Some(error) = bubble {
                return Err(error);
            }
            return Ok(result);
        }
        for index in 0..collected.len() {
            let Some(group) = collected.view(index, &self.context.variables, object_type) else {
                continue;
            };
            // Mutations must keep document order, and a single field has no concurrency to
            // gain.
            if self.expired() {
                return Err(timeout_error(path));
            }
            let (response_key, outcome) =
                match self.try_execute_leaf_planned(object_type, parent, &group, path, errors) {
                    Some(done) => done,
                    None => {
                        self.execute_field_planned(object_type, parent, &group, path, errors, depth)
                            .await
                    }
                };
            match outcome {
                Ok(value) => {
                    result.push_unchecked(response_key, value);
                }
                Err(error) => return Err(error),
            }
        }
        Ok(result)
    }

    async fn execute_fields_inner_dynamic<'s>(
        &'s self,
        object_type: &'doc ObjectType,
        parent: &'s Value,
        selection_sets: &'s [&'doc ast::SelectionSet<'doc>],
        path: &'s mut Path,
        errors: &'s mut Vec<GraphQLError>,
        depth: usize,
    ) -> Result<Object, GraphQLError>
    where
        'doc: 's,
    {
        let mut collected = collect::collect_dynamic(
            &self.context.schema,
            &self.fragments,
            &self.context.variables,
            object_type,
            selection_sets,
        )?;
        let mut result = Object::with_capacity(collected.len());
        if self.parallel
            && collected.len() > 1
            && self.try_execute_all_leaves_dynamic(
                object_type,
                parent,
                &mut collected,
                path,
                errors,
                &mut result,
            )?
        {
            return Ok(result);
        }
        if self.parallel && collected.len() > 1 {
            // The batch lives on the heap so the boxed future below stays small enough to
            // recurse safely.
            let mut futures = Vec::with_capacity(collected.len());
            for (response_key, nodes, field) in collected {
                if self.expired() {
                    return Err(GraphQLError::new(
                        "The operation timed out before it could finish.",
                    )
                    .with_path(path.to_vec()));
                }
                // Leaf fields that need no resolver complete synchronously; `ready` keeps
                // them in the same ordered batch as the asynchronous fields without boxing
                // a future or cloning the path for each one.
                let mut field_errors = Vec::new();
                let inline = self.try_execute_leaf_dynamic(
                    object_type,
                    parent,
                    response_key,
                    &nodes,
                    field,
                    path,
                    &mut field_errors,
                );
                let future = match inline {
                    Some((response_key, outcome)) => {
                        Either::Left(ready((response_key, outcome, field_errors)))
                    }
                    None => {
                        let mut field_path = path.clone();
                        Either::Right(async move {
                            let (response_key, outcome) = self
                                .dynamic_field_future(
                                    object_type,
                                    parent,
                                    response_key,
                                    &nodes,
                                    field,
                                    &mut field_path,
                                    &mut field_errors,
                                    depth,
                                )
                                .await;
                            (response_key, outcome, field_errors)
                        })
                    }
                };
                futures.push(future);
            }
            // `join_all` reaches the same concurrency as `buffered` whenever every sibling
            // fits under the limit, and does not allocate the ordered-stream bookkeeping.
            let max_concurrency = self.context.max_concurrency;
            let outcomes: Vec<_> = if max_concurrency == 0 || futures.len() <= max_concurrency {
                futures::future::join_all(futures).await
            } else {
                buffered(futures, max_concurrency).await
            };
            let mut bubble = None;
            for (response_key, outcome, field_errors) in outcomes {
                append_errors(errors, field_errors, self.context.max_errors);
                match outcome {
                    Ok(value) => result.push_unchecked(response_key, value),
                    // A non-null field failed; keep collecting the other fields' errors, but
                    // the object itself becomes null.
                    Err(error) => {
                        if bubble.is_none() {
                            bubble = Some(error);
                        }
                    }
                }
            }
            if let Some(error) = bubble {
                return Err(error);
            }
            return Ok(result);
        }
        for (response_key, nodes, field) in collected {
            // Mutations must keep document order, and a single field has no concurrency to
            // gain.
            if self.expired() {
                return Err(timeout_error(path));
            }
            let (response_key, outcome) = match self.try_execute_leaf_dynamic(
                object_type,
                parent,
                response_key,
                &nodes,
                field,
                path,
                errors,
            ) {
                Some(done) => done,
                None => {
                    self.dynamic_field_future(
                        object_type,
                        parent,
                        response_key,
                        &nodes,
                        field,
                        path,
                        errors,
                        depth,
                    )
                    .await
                }
            };
            match outcome {
                Ok(value) => {
                    result.push_unchecked(response_key, value);
                }
                Err(error) => return Err(error),
            }
        }
        Ok(result)
    }

    /// Complete a selection set made only of object properties, without a plan.
    fn try_execute_all_leaves_dynamic(
        &self,
        object_type: &ObjectType,
        parent: &Value,
        collected: &mut collect::DynamicFields<'doc>,
        path: &mut Path,
        errors: &mut Vec<GraphQLError>,
        result: &mut Object,
    ) -> Result<bool, GraphQLError> {
        if !collected.iter().all(|(_, nodes, field)| {
            self.can_execute_leaf_inline_dynamic(object_type, nodes, *field)
        }) {
            return Ok(false);
        }
        for (response_key, nodes, field) in collected.drain(..) {
            if self.expired() {
                return Err(timeout_error(path));
            }
            let (response_key, outcome) = self
                .try_execute_leaf_dynamic(
                    object_type,
                    parent,
                    response_key,
                    &nodes,
                    field,
                    path,
                    errors,
                )
                .expect("the inline predicate and the fast path agree");
            result.push_unchecked(response_key, outcome?);
        }
        Ok(true)
    }

    /// Whether a field can be completed by [`Self::try_execute_leaf_planned`].
    fn can_execute_leaf_inline_dynamic(
        &self,
        object_type: &ObjectType,
        nodes: &[&'doc ast::Field<'doc>],
        field: Option<&crate::types::Field>,
    ) -> bool {
        nodes
            .first()
            .is_some_and(|first| self.leaf_field(object_type, first, field).is_some())
    }

    /// The schema field for a selection that the synchronous leaf path can complete.
    ///
    /// The field must exist, have no resolver and no arguments (so argument coercion cannot
    /// fail or be observed), and return a scalar or enum.
    fn try_execute_leaf_dynamic<'s>(
        &'s self,
        object_type: &ObjectType,
        parent: &Value,
        response_key: &'doc str,
        nodes: &'s [&'doc ast::Field<'doc>],
        hint: Option<&'doc crate::types::Field>,
        path: &mut Path,
        errors: &mut Vec<GraphQLError>,
    ) -> Option<(Arc<str>, Result<Value, GraphQLError>)>
    where
        'doc: 's,
    {
        let first = nodes.first().copied()?;
        let field = self.leaf_field(object_type, first, hint)?;

        // An unaliased field's response key is the schema field's interned name, so the
        // response object and the error path can share that `Arc`.
        let key: Arc<str> = match first.alias {
            Some(_) => Arc::from(response_key),
            None => field.name.clone(),
        };
        let location = first.location;
        // The path takes ownership of the key and hands it back on the way out: cloning it for
        // the path would add a refcount round trip to every completed field.
        path.push(PathSegment::Field(key));
        if self.expired() {
            let error = GraphQLError::new("The operation timed out before it could finish.")
                .with_path(path.to_vec());
            path.pop();
            return Some((Arc::from(response_key), Err(error)));
        }

        let label = FieldLabel {
            parent: object_type.name.as_ref(),
            name: field.name.as_ref(),
        };
        let outcome = match default_property(label, field.name.as_ref(), parent) {
            Ok(value) => {
                let value = property_value(value);
                match self.complete_leaf(&value, &field.ty, label, location, path, false) {
                    Some(outcome) => outcome,
                    None => {
                        path.pop();
                        return None;
                    }
                }
            }
            Err(error) => Err(error),
        };
        let outcome = finish_field(
            outcome,
            Some(&field.ty),
            path,
            location,
            errors,
            self.context.max_errors,
        );
        Some((pop_field_key(path), outcome))
    }

    #[allow(clippy::too_many_arguments)]
    /// The boxed field future for the dynamic path, choosing the argument-free implementation
    /// when the field has no arguments.
    #[allow(clippy::too_many_arguments)]
    fn dynamic_field_future<'s>(
        &'s self,
        object_type: &'doc ObjectType,
        parent: &'s Value,
        response_key: &'doc str,
        nodes: &'s [&'doc ast::Field<'doc>],
        hint: Option<&'doc crate::types::Field>,
        path: &'s mut Path,
        errors: &'s mut Vec<GraphQLError>,
        depth: usize,
    ) -> BoxFuture<'s, (Arc<str>, Result<Value, GraphQLError>)>
    where
        'doc: 's,
    {
        let no_args = hint.map_or(true, |field| field.args.is_empty())
            && nodes.first().is_some_and(|node| node.arguments.is_empty());
        if no_args {
            self.execute_field_no_args_async(
                object_type,
                parent,
                response_key,
                nodes,
                hint,
                path,
                errors,
                depth,
            )
        } else {
            self.execute_field_dynamic_async(
                object_type,
                parent,
                response_key,
                nodes,
                hint,
                path,
                errors,
                depth,
            )
        }
    }

    fn execute_field_dynamic_async<'s>(
        &'s self,
        object_type: &'doc ObjectType,
        parent: &'s Value,
        response_key: &'doc str,
        nodes: &'s [&'doc ast::Field<'doc>],
        hint: Option<&'doc crate::types::Field>,
        path: &'s mut Path,
        errors: &'s mut Vec<GraphQLError>,
        depth: usize,
    ) -> BoxFuture<'s, (Arc<str>, Result<Value, GraphQLError>)>
    where
        'doc: 's,
    {
        async move {
            let Some(first) = nodes.first().copied() else {
                return (
                    Arc::from(response_key),
                    Err(GraphQLError::new(format!(
                        "No field node was provided for response key \"{response_key}\"."
                    ))),
                );
            };

            // An unaliased field's response key is exactly the schema field's interned name, so the
            // response object and the error path can share that `Arc` instead of allocating one.
            let field = match hint {
                Some(field) => Some(field),
                None => object_type.get_field(first.name),
            };
            let key: Arc<str> = match (field, first.alias) {
                (Some(field), None) => field.name.clone(),
                _ => Arc::from(response_key),
            };
            path.push(PathSegment::Field(key));

            let outcome = self
                .execute_field_inner_dynamic_async(
                    object_type,
                    field,
                    parent,
                    first,
                    nodes,
                    path,
                    errors,
                    depth,
                )
                .await;
            (pop_field_key(path), outcome)
        }
        .boxed()
    }

    #[allow(clippy::too_many_arguments)]
    async fn execute_field_inner_dynamic_async(
        &self,
        object_type: &ObjectType,
        field: Option<&crate::types::Field>,
        parent: &Value,
        first: &ast::Field<'_>,
        nodes: &[&'doc ast::Field<'doc>],
        path: &mut Path,
        errors: &mut Vec<GraphQLError>,
        depth: usize,
    ) -> Result<Value, GraphQLError> {
        let location = first.location;
        // A single leaf node collects no selection sets, which is the majority of fields and
        // does not need to build (or even walk) the list.
        let selection_sets: SmallVec<[&'doc ast::SelectionSet<'doc>; 2]> = match nodes {
            [only] if only.selection_set.is_none() => SmallVec::new(),
            _ => nodes
                .iter()
                .filter_map(|node| node.selection_set.as_ref())
                .collect(),
        };
        let selections = Selections::new(&selection_sets, None);

        if self.expired() {
            return Err(
                GraphQLError::new("The operation timed out before it could finish.")
                    .with_path(path.to_vec())
                    .at(location),
            );
        }

        // Meta fields are resolved before normal field lookup because they never appear in
        // the schema's type map.
        if let Some(ty) = meta_return_type(first.name) {
            let label = FieldLabel {
                parent: object_type.name.as_ref(),
                name: first.name,
            };
            let empty_args = Object::new();
            let info = ResolveInfo {
                field_name: first.name,
                parent_type: object_type.name.as_ref(),
                return_type: &ty,
                path: path.as_slice(),
                schema: &self.context.schema,
            };
            let denial = match self.context.authorizer.as_ref() {
                Some(authorizer) => {
                    let authorization = AuthorizationContext {
                        context: self.context,
                        field_name: first.name,
                        parent_type: object_type.name.as_ref(),
                        return_type: &ty,
                        path: path.as_slice(),
                        args: &empty_args,
                        info: &info,
                    };
                    authorizer.authorize_async(&authorization).await.err()
                }
                None => None,
            };
            let outcome = match denial {
                Some(error) => Err(error),
                None => match self.resolve_meta(object_type, parent, first) {
                    Ok(value) => {
                        match self.complete_leaf(&value, &ty, label, location, path, false) {
                            Some(completed) => completed,
                            None => match self.try_complete_field_value_sync(
                                &value,
                                &ty,
                                label,
                                &selections,
                                location,
                                path,
                                errors,
                                depth,
                            ) {
                                Some(completed) => completed,
                                None => {
                                    self.complete_field_value(
                                        &value,
                                        &ty,
                                        label,
                                        &selections,
                                        location,
                                        path,
                                        errors,
                                        depth,
                                    )
                                    .await
                                }
                            },
                        }
                    }
                    Err(error) => Err(error),
                },
            };
            return finish_field(
                outcome,
                Some(&ty),
                path,
                location,
                errors,
                self.context.max_errors,
            );
        }

        let Some(field) = field else {
            let error = unknown_field_error(first.name, object_type.name.as_ref(), location);
            return finish_field(
                Err(error),
                None,
                path,
                location,
                errors,
                self.context.max_errors,
            );
        };

        let label = FieldLabel {
            parent: object_type.name.as_ref(),
            name: field.name.as_ref(),
        };
        // Coercing an empty argument list cannot fail, and most fields take no arguments, so the
        // common case skips the work instead of building an object nothing reads.
        let mut args = Object::with_capacity(field.args.len());
        if !(first.arguments.is_empty() && field.args.is_empty()) {
            if let Err(error) = coerce::coerce_argument_values_into(
                &self.context.schema,
                field,
                first,
                &self.context.variables,
                &mut args,
            ) {
                return finish_field(
                    Err(error),
                    Some(&field.ty),
                    path,
                    location,
                    errors,
                    self.context.max_errors,
                );
            }
        }
        let args = &args;

        // The authorizer and the resolver both see the same `ResolveInfo`; the borrow of
        // `path` ends before completion needs it mutably again.
        //
        // A resolver produces an owned value, but a field without one just reads the parent
        // object's property. Borrowing that property instead of cloning it is what keeps
        // composite fields free of a full copy of the parent's list or object.
        let resolved = {
            let info = ResolveInfo {
                field_name: field.name.as_ref(),
                parent_type: object_type.name.as_ref(),
                return_type: &field.ty,
                path: path.as_slice(),
                schema: &self.context.schema,
            };
            let denial = match self.context.authorizer.as_ref() {
                Some(authorizer) if !(self.skip_root_authorization && path.len() == 1) => {
                    let authorization = AuthorizationContext {
                        context: self.context,
                        field_name: field.name.as_ref(),
                        parent_type: object_type.name.as_ref(),
                        return_type: &field.ty,
                        path: path.as_slice(),
                        args,
                        info: &info,
                    };
                    authorizer.authorize_async(&authorization).await.err()
                }
                _ => None,
            };
            if let Some(error) = denial {
                Err(error)
            } else {
                let resolver_context = ResolverContext {
                    parent,
                    args,
                    context: self.context,
                    info: &info,
                };
                match &field.resolve {
                    Some(resolve) => resolve(resolver_context).await.map(Cow::Owned),
                    None => {
                        default_property(label, field.name.as_ref(), parent).map(property_value)
                    }
                }
            }
        };

        let outcome = match resolved {
            Ok(value) => {
                match self.complete_leaf(&value, &field.ty, label, location, path, false) {
                    Some(completed) => completed,
                    None => match self.try_complete_field_value_sync(
                        &value,
                        &field.ty,
                        label,
                        &selections,
                        location,
                        path,
                        errors,
                        depth,
                    ) {
                        Some(completed) => completed,
                        None => {
                            self.complete_field_value(
                                &value,
                                &field.ty,
                                label,
                                &selections,
                                location,
                                path,
                                errors,
                                depth,
                            )
                            .await
                        }
                    },
                }
            }
            Err(error) => Err(error),
        };
        finish_field(
            outcome,
            Some(&field.ty),
            path,
            location,
            errors,
            self.context.max_errors,
        )
    }

    fn execute_field_no_args_async<'s>(
        &'s self,
        object_type: &'doc ObjectType,
        parent: &'s Value,
        response_key: &'doc str,
        nodes: &'s [&'doc ast::Field<'doc>],
        hint: Option<&'doc crate::types::Field>,
        path: &'s mut Path,
        errors: &'s mut Vec<GraphQLError>,
        depth: usize,
    ) -> BoxFuture<'s, (Arc<str>, Result<Value, GraphQLError>)>
    where
        'doc: 's,
    {
        async move {
            let Some(first) = nodes.first().copied() else {
                return (
                    Arc::from(response_key),
                    Err(GraphQLError::new(format!(
                        "No field node was provided for response key \"{response_key}\"."
                    ))),
                );
            };

            // An unaliased field's response key is exactly the schema field's interned name, so the
            // response object and the error path can share that `Arc` instead of allocating one.
            let field = match hint {
                Some(field) => Some(field),
                None => object_type.get_field(first.name),
            };
            let key: Arc<str> = match (field, first.alias) {
                (Some(field), None) => field.name.clone(),
                _ => Arc::from(response_key),
            };
            path.push(PathSegment::Field(key));

            let outcome = self
                .execute_field_inner_no_args_async(
                    object_type,
                    field,
                    parent,
                    first,
                    nodes,
                    path,
                    errors,
                    depth,
                )
                .await;
            (pop_field_key(path), outcome)
        }
        .boxed()
    }

    #[allow(clippy::too_many_arguments)]
    async fn execute_field_inner_no_args_async(
        &self,
        object_type: &ObjectType,
        field: Option<&crate::types::Field>,
        parent: &Value,
        first: &ast::Field<'_>,
        nodes: &[&'doc ast::Field<'doc>],
        path: &mut Path,
        errors: &mut Vec<GraphQLError>,
        depth: usize,
    ) -> Result<Value, GraphQLError> {
        let location = first.location;
        // A single leaf node collects no selection sets, which is the majority of fields and
        // does not need to build (or even walk) the list.
        let selection_sets: SmallVec<[&'doc ast::SelectionSet<'doc>; 2]> = match nodes {
            [only] if only.selection_set.is_none() => SmallVec::new(),
            _ => nodes
                .iter()
                .filter_map(|node| node.selection_set.as_ref())
                .collect(),
        };
        let selections = Selections::new(&selection_sets, None);

        if self.expired() {
            return Err(
                GraphQLError::new("The operation timed out before it could finish.")
                    .with_path(path.to_vec())
                    .at(location),
            );
        }

        // Meta fields are resolved before normal field lookup because they never appear in
        // the schema's type map.
        if let Some(ty) = meta_return_type(first.name) {
            let label = FieldLabel {
                parent: object_type.name.as_ref(),
                name: first.name,
            };
            let empty_args = Object::new();
            let info = ResolveInfo {
                field_name: first.name,
                parent_type: object_type.name.as_ref(),
                return_type: &ty,
                path: path.as_slice(),
                schema: &self.context.schema,
            };
            let denial = match self.context.authorizer.as_ref() {
                Some(authorizer) => {
                    let authorization = AuthorizationContext {
                        context: self.context,
                        field_name: first.name,
                        parent_type: object_type.name.as_ref(),
                        return_type: &ty,
                        path: path.as_slice(),
                        args: &empty_args,
                        info: &info,
                    };
                    authorizer.authorize_async(&authorization).await.err()
                }
                None => None,
            };
            let outcome = match denial {
                Some(error) => Err(error),
                None => match self.resolve_meta(object_type, parent, first) {
                    Ok(value) => {
                        match self.complete_leaf(&value, &ty, label, location, path, false) {
                            Some(completed) => completed,
                            None => match self.try_complete_field_value_sync(
                                &value,
                                &ty,
                                label,
                                &selections,
                                location,
                                path,
                                errors,
                                depth,
                            ) {
                                Some(completed) => completed,
                                None => {
                                    self.complete_field_value(
                                        &value,
                                        &ty,
                                        label,
                                        &selections,
                                        location,
                                        path,
                                        errors,
                                        depth,
                                    )
                                    .await
                                }
                            },
                        }
                    }
                    Err(error) => Err(error),
                },
            };
            return finish_field(
                outcome,
                Some(&ty),
                path,
                location,
                errors,
                self.context.max_errors,
            );
        }

        let Some(field) = field else {
            let error = unknown_field_error(first.name, object_type.name.as_ref(), location);
            return finish_field(
                Err(error),
                None,
                path,
                location,
                errors,
                self.context.max_errors,
            );
        };

        let label = FieldLabel {
            parent: object_type.name.as_ref(),
            name: field.name.as_ref(),
        };
        // Fields without arguments skip the argument object entirely. Keeping it out of this
        // frame is what preserves stack headroom at the parser's depth limit.
        let args = empty_object();

        // The authorizer and the resolver both see the same `ResolveInfo`; the borrow of
        // `path` ends before completion needs it mutably again.
        //
        // A resolver produces an owned value, but a field without one just reads the parent
        // object's property. Borrowing that property instead of cloning it is what keeps
        // composite fields free of a full copy of the parent's list or object.
        let resolved = {
            let info = ResolveInfo {
                field_name: field.name.as_ref(),
                parent_type: object_type.name.as_ref(),
                return_type: &field.ty,
                path: path.as_slice(),
                schema: &self.context.schema,
            };
            let denial = match self.context.authorizer.as_ref() {
                Some(authorizer) if !(self.skip_root_authorization && path.len() == 1) => {
                    let authorization = AuthorizationContext {
                        context: self.context,
                        field_name: field.name.as_ref(),
                        parent_type: object_type.name.as_ref(),
                        return_type: &field.ty,
                        path: path.as_slice(),
                        args,
                        info: &info,
                    };
                    authorizer.authorize_async(&authorization).await.err()
                }
                _ => None,
            };
            if let Some(error) = denial {
                Err(error)
            } else {
                let resolver_context = ResolverContext {
                    parent,
                    args,
                    context: self.context,
                    info: &info,
                };
                match &field.resolve {
                    Some(resolve) => resolve(resolver_context).await.map(Cow::Owned),
                    None => {
                        default_property(label, field.name.as_ref(), parent).map(property_value)
                    }
                }
            }
        };

        let outcome = match resolved {
            Ok(value) => {
                match self.complete_leaf(&value, &field.ty, label, location, path, false) {
                    Some(completed) => completed,
                    None => match self.try_complete_field_value_sync(
                        &value,
                        &field.ty,
                        label,
                        &selections,
                        location,
                        path,
                        errors,
                        depth,
                    ) {
                        Some(completed) => completed,
                        None => {
                            self.complete_field_value(
                                &value,
                                &field.ty,
                                label,
                                &selections,
                                location,
                                path,
                                errors,
                                depth,
                            )
                            .await
                        }
                    },
                }
            }
            Err(error) => Err(error),
        };
        finish_field(
            outcome,
            Some(&field.ty),
            path,
            location,
            errors,
            self.context.max_errors,
        )
    }

    /// Boxed form of [`Self::execute_fields_inner_planned`] for nested completion.
    ///
    /// The box is what bounds the recursive poll stack and breaks the future's type recursion:
    /// `complete_object` is reachable from a field future, which is reachable from this function,
    /// and naming a concrete `BoxFuture` here keeps that cycle finite.
    fn execute_fields<'s>(
        &'s self,
        object_type: &'doc ObjectType,
        parent: &'s Value,
        selections: &'s Selections<'_, 'doc>,
        path: &'s mut Path,
        errors: &'s mut Vec<GraphQLError>,
        depth: usize,
    ) -> BoxFuture<'s, Result<Object, GraphQLError>>
    where
        'doc: 's,
    {
        // Prepared queries use the planned completion path. Normal execution keeps the original
        // dynamic one, whose frames preserve the stack bound at the parser's depth limit.
        if self.plans.is_some() {
            self.execute_fields_inner_planned(object_type, parent, selections, path, errors, depth)
                .boxed()
        } else {
            self.execute_fields_inner_dynamic(
                object_type,
                parent,
                selections.sets,
                path,
                errors,
                depth,
            )
            .boxed()
        }
    }

    /// Complete a selection set made only of object properties.
    ///
    /// This runs before the asynchronous batch is built so such sets never allocate or poll a
    /// future, and it is a separate function so its locals do not enlarge the recursive
    /// [`Self::execute_fields`] frame.
    fn try_execute_all_leaves_planned(
        &self,
        object_type: &'doc ObjectType,
        parent: &Value,
        collected: &CollectedFields<'doc>,
        path: &mut Path,
        errors: &mut Vec<GraphQLError>,
        result: &mut Object,
    ) -> Result<bool, GraphQLError> {
        for index in 0..collected.len() {
            let Some(group) = collected.view(index, &self.context.variables, object_type) else {
                continue;
            };
            if !self.can_execute_leaf_inline_planned(object_type, &group) {
                return Ok(false);
            }
        }
        let check_deadline = self.deadline.is_some();
        for index in 0..collected.len() {
            let Some(group) = collected.view(index, &self.context.variables, object_type) else {
                continue;
            };
            if check_deadline && self.expired() {
                return Err(timeout_error(path));
            }
            let (response_key, outcome) = self
                .try_execute_leaf_planned(object_type, parent, &group, path, errors)
                .expect("the inline predicate and the fast path agree");
            result.push_unchecked(response_key, outcome?);
        }
        Ok(true)
    }

    /// Whether a field can be completed by [`Self::try_execute_leaf_planned`].
    fn can_execute_leaf_inline_planned(
        &self,
        object_type: &ObjectType,
        group: &GroupView<'_, 'doc>,
    ) -> bool {
        group
            .nodes
            .first()
            .is_some_and(|first| self.leaf_field(object_type, first, group.field).is_some())
    }

    /// The schema field for a selection that the synchronous leaf path can complete.
    ///
    /// The field must exist, have no resolver and no arguments (so argument coercion cannot
    /// fail or be observed), and return a scalar or enum.
    fn leaf_field<'f>(
        &self,
        object_type: &'f ObjectType,
        first: &ast::Field<'_>,
        hint: Option<&'f crate::types::Field>,
    ) -> Option<&'f crate::types::Field> {
        if !first.arguments.is_empty() || meta_return_type(first.name).is_some() {
            return None;
        }
        let field = match hint {
            Some(field) => field,
            None => object_type.get_field(first.name)?,
        };
        if field.resolve.is_some() || !field.args.is_empty() || self.context.authorizer.is_some() {
            return None;
        }
        match self.context.schema.get_type(field.ty.named_type())? {
            Type::Scalar(_) | Type::Enum(_) => Some(field),
            _ => None,
        }
    }

    /// Complete a leaf field that needs no asynchronous work in place.
    ///
    /// A field with no resolver, no arguments and a scalar or enum type resolves through the
    /// default property resolver and completes synchronously. Doing it here avoids boxing a
    /// future and cloning the path for every such field, which is the common case for object
    /// properties. Returning `None` hands the field to the general path unchanged.
    #[allow(clippy::too_many_arguments)]
    fn try_execute_leaf_planned<'s>(
        &'s self,
        object_type: &ObjectType,
        parent: &Value,
        group: &GroupView<'_, 'doc>,
        path: &mut Path,
        errors: &mut Vec<GraphQLError>,
    ) -> Option<(Arc<str>, Result<Value, GraphQLError>)>
    where
        'doc: 's,
    {
        let first = group.nodes.first()?;
        let field = self.leaf_field(object_type, first, group.field)?;

        // An unaliased field's response key is the schema field's interned name, so the
        // response object and the error path can share that `Arc`.
        let key = group.key_arc(first, Some(field));
        let location = first.location;
        // The path takes ownership of the key and hands it back on the way out: cloning it for
        // the path would add a refcount round trip to every completed field.
        path.push(PathSegment::Field(key));
        if self.expired() {
            let error = GraphQLError::new("The operation timed out before it could finish.")
                .with_path(path.to_vec());
            path.pop();
            return Some((Arc::from(group.response_key), Err(error)));
        }

        let label = FieldLabel {
            parent: object_type.name.as_ref(),
            name: field.name.as_ref(),
        };
        let outcome = match default_property(label, field.name.as_ref(), parent) {
            Ok(value) => {
                let value = property_value(value);
                match self.complete_leaf(&value, &field.ty, label, location, path, false) {
                    Some(outcome) => outcome,
                    None => {
                        path.pop();
                        return None;
                    }
                }
            }
            Err(error) => Err(error),
        };
        let outcome = finish_field(
            outcome,
            Some(&field.ty),
            path,
            location,
            errors,
            self.context.max_errors,
        );
        Some((pop_field_key(path), outcome))
    }

    #[allow(clippy::too_many_arguments)]
    fn execute_field_planned<'s>(
        &'s self,
        object_type: &'doc ObjectType,
        parent: &'s Value,
        group: &'s GroupView<'_, 'doc>,
        path: &'s mut Path,
        errors: &'s mut Vec<GraphQLError>,
        depth: usize,
    ) -> BoxFuture<'s, (Arc<str>, Result<Value, GraphQLError>)>
    where
        'doc: 's,
    {
        async move {
            let Some(first) = group.nodes.first() else {
                return (
                    Arc::from(group.response_key),
                    Err(GraphQLError::new(format!(
                        "No field node was provided for response key \"{}\".",
                        group.response_key
                    ))),
                );
            };

            // An unaliased field's response key is exactly the schema field's interned name, so the
            // response object and the error path can share that `Arc` instead of allocating one.
            let key = group.key_arc(first, group.field);
            path.push(PathSegment::Field(key));

            let outcome = self
                .execute_field_inner_planned(object_type, group, parent, path, errors, depth)
                .await;
            (pop_field_key(path), outcome)
        }
        .boxed()
    }

    #[allow(clippy::too_many_arguments)]
    async fn execute_field_inner_planned(
        &self,
        object_type: &ObjectType,
        group: &GroupView<'_, 'doc>,
        parent: &Value,
        path: &mut Path,
        errors: &mut Vec<GraphQLError>,
        depth: usize,
    ) -> Result<Value, GraphQLError> {
        let Some(first) = group.nodes.first() else {
            return Err(GraphQLError::new(format!(
                "No field node was provided for response key \"{}\".",
                group.response_key
            )));
        };
        let location = first.location;
        // The completion path takes the merged selection sets. They are built here in document
        // order; the buffer stays inline for the usual one or two nodes.
        let selection_sets: SmallVec<[&'doc ast::SelectionSet<'doc>; 2]> = match group.nodes.first()
        {
            Some(only) if only.selection_set.is_none() => SmallVec::new(),
            _ => group.nodes.selection_sets(),
        };
        let selections = Selections::new(
            &selection_sets,
            group.child.filter(|_| self.plans.is_some()),
        );

        if self.expired() {
            return Err(
                GraphQLError::new("The operation timed out before it could finish.")
                    .with_path(path.to_vec())
                    .at(location),
            );
        }

        // Meta fields are resolved before normal field lookup because they never appear in
        // the schema's type map.
        if let Some(ty) = meta_return_type(first.name) {
            let label = FieldLabel {
                parent: object_type.name.as_ref(),
                name: first.name,
            };
            let empty_args = Object::new();
            let info = ResolveInfo {
                field_name: first.name,
                parent_type: object_type.name.as_ref(),
                return_type: &ty,
                path: path.as_slice(),
                schema: &self.context.schema,
            };
            let denial = match self.context.authorizer.as_ref() {
                Some(authorizer) => {
                    let authorization = AuthorizationContext {
                        context: self.context,
                        field_name: first.name,
                        parent_type: object_type.name.as_ref(),
                        return_type: &ty,
                        path: path.as_slice(),
                        args: &empty_args,
                        info: &info,
                    };
                    authorizer.authorize_async(&authorization).await.err()
                }
                None => None,
            };
            let outcome = match denial {
                Some(error) => Err(error),
                None => match self.resolve_meta(object_type, parent, first) {
                    Ok(value) => {
                        match self.complete_leaf(&value, &ty, label, location, path, false) {
                            Some(completed) => completed,
                            None => match self.try_complete_field_value_sync(
                                &value,
                                &ty,
                                label,
                                &selections,
                                location,
                                path,
                                errors,
                                depth,
                            ) {
                                Some(completed) => completed,
                                None => {
                                    self.complete_field_value(
                                        &value,
                                        &ty,
                                        label,
                                        &selections,
                                        location,
                                        path,
                                        errors,
                                        depth,
                                    )
                                    .await
                                }
                            },
                        }
                    }
                    Err(error) => Err(error),
                },
            };
            return finish_field(
                outcome,
                Some(&ty),
                path,
                location,
                errors,
                self.context.max_errors,
            );
        }

        let Some(field) = group.field else {
            let error = unknown_field_error(first.name, object_type.name.as_ref(), location);
            return finish_field(
                Err(error),
                None,
                path,
                location,
                errors,
                self.context.max_errors,
            );
        };

        let label = FieldLabel {
            parent: object_type.name.as_ref(),
            name: field.name.as_ref(),
        };
        // Coercing an empty argument list cannot fail, and most fields take no arguments, so the
        // common case skips the work instead of building an object nothing reads.
        let mut args = Object::with_capacity(field.args.len());
        if let Err(error) = self.field_arguments(group, first, field, &mut args) {
            return finish_field(
                Err(error),
                Some(&field.ty),
                path,
                location,
                errors,
                self.context.max_errors,
            );
        }
        let args = &args;

        // The authorizer and the resolver both see the same `ResolveInfo`; the borrow of
        // `path` ends before completion needs it mutably again.
        //
        // A resolver produces an owned value, but a field without one just reads the parent
        // object's property. Borrowing that property instead of cloning it is what keeps
        // composite fields free of a full copy of the parent's list or object.
        let resolved = {
            let info = ResolveInfo {
                field_name: field.name.as_ref(),
                parent_type: object_type.name.as_ref(),
                return_type: &field.ty,
                path: path.as_slice(),
                schema: &self.context.schema,
            };
            let denial = match self.context.authorizer.as_ref() {
                Some(authorizer) if !(self.skip_root_authorization && path.len() == 1) => {
                    let authorization = AuthorizationContext {
                        context: self.context,
                        field_name: field.name.as_ref(),
                        parent_type: object_type.name.as_ref(),
                        return_type: &field.ty,
                        path: path.as_slice(),
                        args,
                        info: &info,
                    };
                    authorizer.authorize_async(&authorization).await.err()
                }
                _ => None,
            };
            if let Some(error) = denial {
                Err(error)
            } else {
                let resolver_context = ResolverContext {
                    parent,
                    args,
                    context: self.context,
                    info: &info,
                };
                match &field.resolve {
                    Some(resolve) => resolve(resolver_context).await.map(Cow::Owned),
                    None => {
                        default_property(label, field.name.as_ref(), parent).map(property_value)
                    }
                }
            }
        };

        let outcome = match resolved {
            Ok(value) => {
                match self.complete_leaf(&value, &field.ty, label, location, path, false) {
                    Some(completed) => completed,
                    None => match self.try_complete_field_value_sync(
                        &value,
                        &field.ty,
                        label,
                        &selections,
                        location,
                        path,
                        errors,
                        depth,
                    ) {
                        Some(completed) => completed,
                        None => {
                            self.complete_field_value(
                                &value,
                                &field.ty,
                                label,
                                &selections,
                                location,
                                path,
                                errors,
                                depth,
                            )
                            .await
                        }
                    },
                }
            }
            Err(error) => Err(error),
        };
        finish_field(
            outcome,
            Some(&field.ty),
            path,
            location,
            errors,
            self.context.max_errors,
        )
    }

    /// Coerce a field's arguments, using the plan's template when there is one.
    fn field_arguments(
        &self,
        group: &GroupView<'_, 'doc>,
        first: &ast::Field<'_>,
        field: &crate::types::Field,
        args: &mut Object,
    ) -> Result<(), GraphQLError> {
        if let Some(template) = group.args {
            return template.fill(&self.context.variables, args);
        }
        if first.arguments.is_empty() && field.args.is_empty() {
            return Ok(());
        }
        coerce::coerce_argument_values_into(
            &self.context.schema,
            field,
            first,
            &self.context.variables,
            args,
        )
    }

    /// Complete a leaf field without building a future.
    ///
    /// Scalars and enums are by far the most common field type and never need to `await`
    /// anything, so resolving them inline avoids one boxed future (plus its allocation) per
    /// leaf field. Returning `None` means the type needs the general path.
    ///
    /// `sync_only` is set by the synchronous default-completion path, which may still abandon its
    /// work and restart asynchronously. A custom scalar's `serialize` is caller-supplied code, so
    /// that path refuses to run it (returning `None`) rather than risk running it twice.
    fn complete_leaf(
        &self,
        value: &Value,
        ty: &TypeRef,
        label: FieldLabel<'_>,
        location: Location,
        path: &mut Path,
        sync_only: bool,
    ) -> Option<Result<Value, GraphQLError>> {
        let mut current = ty;
        let mut non_null = false;
        while let TypeRef::NonNull(inner) = current {
            non_null = true;
            current = inner;
        }
        let TypeRef::Named(name) = current else {
            return None;
        };
        let definition = self.context.schema.get_type(name)?;
        self.complete_leaf_definition(
            value, definition, non_null, label, location, path, sync_only,
        )
    }

    /// [`Self::complete_leaf`] with the field's named type already resolved.
    #[allow(clippy::too_many_arguments)]
    fn complete_leaf_definition(
        &self,
        value: &Value,
        definition: &Type,
        non_null: bool,
        label: FieldLabel<'_>,
        location: Location,
        path: &mut Path,
        sync_only: bool,
    ) -> Option<Result<Value, GraphQLError>> {
        if value.is_null() {
            return Some(if non_null {
                Err(located(
                    format!("Cannot return null for non-nullable field {label}."),
                    path,
                    location,
                ))
            } else {
                Ok(Value::Null)
            });
        }
        let completed = match definition {
            Type::Scalar(scalar) => {
                if let Some(serialized) = builtin_scalar_passthrough(scalar, value) {
                    return Some(Ok(serialized));
                }
                let serialized = if let Some(serialize) = scalar.serialize_fn() {
                    // A scalar the engine did not define may observe the call, and this path can
                    // still restart asynchronously, so only the built-in functions are safe here.
                    if sync_only && !scalar.builtin {
                        return None;
                    }
                    match serialize(value) {
                        Ok(value) => value,
                        Err(message) => {
                            return Some(Err(located(
                                format!("Field {label} could not be serialized: {message}"),
                                path,
                                location,
                            )));
                        }
                    }
                } else {
                    value.clone()
                };
                if !is_legal_scalar_value(&serialized) {
                    return Some(Err(located(
                        format!(
                            "Field {label} serialized to a {} which is not a valid scalar value.",
                            serialized.kind()
                        ),
                        path,
                        location,
                    )));
                }
                serialized
            }
            Type::Enum(enumeration) => match value {
                Value::Enum(value) | Value::String(value) if enumeration.has_value(value) => {
                    Value::Enum(value.clone())
                }
                Value::Enum(value) | Value::String(value) => {
                    return Some(Err(located(
                        format!(
                            "Value \"{value}\" does not exist in enum \"{}\" for field {label}.",
                            enumeration.name
                        ),
                        path,
                        location,
                    )));
                }
                other => {
                    return Some(Err(located(
                        format!(
                            "Field {label} expects enum \"{}\" but received a {}.",
                            enumeration.name,
                            other.kind()
                        ),
                        path,
                        location,
                    )));
                }
            },
            _ => return None,
        };
        if non_null && completed.is_null() {
            return Some(Err(located(
                format!("Cannot return null for non-nullable field {label}."),
                path,
                location,
            )));
        }
        Some(Ok(completed))
    }

    /// The concrete object type a value of `ty` completes as, when it is a plain object.
    ///
    /// Lists, interfaces and unions need the general path; a bare named object is by far the
    /// most common composite field and completing it here avoids boxing a future.
    fn plain_object(&self, ty: &TypeRef) -> Option<&'doc ObjectType> {
        let mut current = ty;
        while let TypeRef::NonNull(inner) = current {
            current = inner;
        }
        let TypeRef::Named(name) = current else {
            return None;
        };
        match self.context.schema.get_type(name) {
            Some(Type::Object(object)) => Some(object),
            _ => None,
        }
    }

    /// Complete a field whose type may be a plain object, without the boxed general path.
    #[allow(clippy::too_many_arguments)]
    async fn complete_field_value(
        &self,
        value: &Value,
        ty: &TypeRef,
        label: FieldLabel<'_>,
        selections: &Selections<'_, 'doc>,
        location: Location,
        path: &mut Path,
        errors: &mut Vec<GraphQLError>,
        depth: usize,
    ) -> Result<Value, GraphQLError> {
        if !value.is_null() {
            if let Some(object) = self.plain_object(ty) {
                return self
                    .complete_object(
                        object, value, label, selections, location, path, errors, depth,
                    )
                    .await;
            }
            if let Some((object, items_non_null)) = self.object_list(ty) {
                return self
                    .complete_object_list(
                        object,
                        items_non_null,
                        value,
                        label,
                        selections,
                        location,
                        path,
                        errors,
                        depth,
                    )
                    .await;
            }
        }
        self.complete_value(value, ty, label, selections, location, path, errors, depth)
            .await
    }

    /// The object type a list's items complete as, when every item is a plain object.
    ///
    /// Returns the item type's non-null-ness too, because it decides whether a failed item nulls
    /// the whole list.
    fn object_list(&self, ty: &TypeRef) -> Option<(&'doc ObjectType, bool)> {
        let mut current = ty;
        while let TypeRef::NonNull(inner) = current {
            current = inner;
        }
        let TypeRef::List(inner) = current else {
            return None;
        };
        let (inner, non_null) = match &**inner {
            TypeRef::NonNull(inner) => (&**inner, true),
            inner => (inner, false),
        };
        let TypeRef::Named(name) = inner else {
            return None;
        };
        match self.context.schema.get_type(name) {
            Some(Type::Object(object)) => Some((object, non_null)),
            _ => None,
        }
    }

    /// Complete a list of plain objects without a completion future.
    ///
    /// A list of records is the common shape in a response, and every item completes through
    /// [`Self::complete_object`]. Handling it here means the field allocates no completion future
    /// at all — only the response list itself.
    #[allow(clippy::too_many_arguments)]
    async fn complete_object_list(
        &self,
        object: &'doc ObjectType,
        items_non_null: bool,
        value: &Value,
        label: FieldLabel<'_>,
        selections: &Selections<'_, 'doc>,
        location: Location,
        path: &mut Path,
        errors: &mut Vec<GraphQLError>,
        depth: usize,
    ) -> Result<Value, GraphQLError> {
        let Value::List(items) = value else {
            return Err(list_mismatch_error(label, value, path, location));
        };
        let mut completed = Vec::with_capacity(items.len());
        for (index, item) in items.iter().enumerate() {
            path.push(PathSegment::index(index));
            let outcome = if item.is_null() {
                if items_non_null {
                    Err(located(
                        format!("Cannot return null for non-nullable field {label}."),
                        path,
                        location,
                    ))
                } else {
                    Ok(Value::Null)
                }
            } else {
                self.complete_object(
                    object, item, label, selections, location, path, errors, depth,
                )
                .await
            };
            path.pop();
            match outcome {
                Ok(value) => completed.push(value),
                Err(error) => {
                    if items_non_null {
                        return Err(error);
                    }
                    record_error(errors, error, self.context.max_errors);
                    completed.push(Value::Null);
                }
            }
        }
        Ok(Value::List(completed))
    }

    /// The `CompleteValue` algorithm for one field.
    ///
    /// The value is borrowed for the whole completion: a resolver hands over an owned value that
    /// the caller keeps alive, and a field without one points straight at the parent's property.
    ///
    /// Completion is boxed because that is what bounds the recursive poll stack: an inlined
    /// completion chain makes each nesting level's frame large enough that a document at the
    /// parser's depth limit would exhaust a Tokio worker's 2 MiB stack. List *items* still
    /// complete inline (see [`Self::complete_item`]), which is where the allocation per item
    /// used to go.
    #[allow(clippy::too_many_arguments)]
    fn complete_value<'s>(
        &'s self,
        value: &'s Value,
        ty: &'s TypeRef,
        label: FieldLabel<'s>,
        selections: &'s Selections<'_, 'doc>,
        location: Location,
        path: &'s mut Path,
        errors: &'s mut Vec<GraphQLError>,
        depth: usize,
    ) -> BoxFuture<'s, Result<Value, GraphQLError>>
    where
        'doc: 's,
    {
        self.complete_value_inner(value, ty, label, selections, location, path, errors, depth)
            .boxed()
    }

    #[allow(clippy::too_many_arguments)]
    async fn complete_value_inner(
        &self,
        value: &Value,
        ty: &TypeRef,
        label: FieldLabel<'_>,
        selections: &Selections<'_, 'doc>,
        location: Location,
        path: &mut Path,
        errors: &mut Vec<GraphQLError>,
        depth: usize,
    ) -> Result<Value, GraphQLError> {
        // Strip non-null wrappers up front instead of recursing through one future per wrapper.
        let mut current = ty;
        let mut non_null = false;
        while let TypeRef::NonNull(inner) = current {
            non_null = true;
            current = inner;
        }
        if value.is_null() {
            return if non_null {
                Err(non_null_error(label, path, location))
            } else {
                Ok(Value::Null)
            };
        }
        let completed = match current {
            TypeRef::List(inner) => {
                self.complete_list(
                    value, inner, label, selections, location, path, errors, depth,
                )
                .await?
            }
            TypeRef::Named(name) => {
                self.complete_named(
                    value, name, label, selections, location, path, errors, depth,
                )
                .await?
            }
            TypeRef::NonNull(_) => {
                return Err(located(
                    "Internal execution error: non-null type was not unwrapped.",
                    path,
                    location,
                ));
            }
        };
        if non_null && completed.is_null() {
            return Err(non_null_error(label, path, location));
        }
        Ok(completed)
    }

    /// Complete one item of a list.
    ///
    /// A named item completes through [`Self::complete_named`] without a completion future of its
    /// own, so a list of objects allocates only the response list and its objects.
    #[allow(clippy::too_many_arguments)]
    async fn complete_item(
        &self,
        item: &Value,
        inner: &TypeRef,
        label: FieldLabel<'_>,
        selections: &Selections<'_, 'doc>,
        location: Location,
        path: &mut Path,
        errors: &mut Vec<GraphQLError>,
        depth: usize,
    ) -> Result<Value, GraphQLError> {
        let mut current = inner;
        let mut non_null = false;
        while let TypeRef::NonNull(wrapped) = current {
            non_null = true;
            current = wrapped;
        }
        if item.is_null() {
            return if non_null {
                Err(non_null_error(label, path, location))
            } else {
                Ok(Value::Null)
            };
        }
        let completed = match current {
            // A list inside a list goes back through the boxed algorithm, which is what keeps
            // the completion future's size finite.
            TypeRef::List(_) => {
                return self
                    .complete_value(
                        item, inner, label, selections, location, path, errors, depth,
                    )
                    .await;
            }
            TypeRef::Named(name) => {
                self.complete_named(item, name, label, selections, location, path, errors, depth)
                    .await?
            }
            TypeRef::NonNull(_) => {
                return Err(located(
                    "Internal execution error: non-null type was not unwrapped.",
                    path,
                    location,
                ));
            }
        };
        if non_null && completed.is_null() {
            return Err(located(
                format!("Cannot return null for non-nullable field {label}."),
                path,
                location,
            ));
        }
        Ok(completed)
    }

    #[allow(clippy::too_many_arguments)]
    async fn complete_list(
        &self,
        value: &Value,
        inner: &TypeRef,
        label: FieldLabel<'_>,
        selections: &Selections<'_, 'doc>,
        location: Location,
        path: &mut Path,
        errors: &mut Vec<GraphQLError>,
        depth: usize,
    ) -> Result<Value, GraphQLError> {
        let Value::List(items) = value else {
            return Err(located(
                format!(
                    "Expected a list for field {label}, but the resolved value is a {}.",
                    value.kind()
                ),
                path,
                location,
            ));
        };

        let object = self.plain_object(inner);
        let mut completed = Vec::with_capacity(items.len());
        for (index, item) in items.iter().enumerate() {
            path.push(PathSegment::index(index));
            let outcome = match &object {
                Some(object) if !item.is_null() => {
                    self.complete_object(
                        object, item, label, selections, location, path, errors, depth,
                    )
                    .await
                }
                _ => {
                    self.complete_item(
                        item, inner, label, selections, location, path, errors, depth,
                    )
                    .await
                }
            };
            path.pop();
            match outcome {
                Ok(value) => completed.push(value),
                Err(error) => {
                    if inner.is_non_null() {
                        return Err(error);
                    }
                    record_error(errors, error, self.context.max_errors);
                    completed.push(Value::Null);
                }
            }
        }
        Ok(Value::List(completed))
    }

    #[allow(clippy::too_many_arguments)]
    async fn complete_named(
        &self,
        value: &Value,
        name: &crate::types::Name,
        label: FieldLabel<'_>,
        selections: &Selections<'_, 'doc>,
        location: Location,
        path: &mut Path,
        errors: &mut Vec<GraphQLError>,
        depth: usize,
    ) -> Result<Value, GraphQLError> {
        let Some(definition) = self.context.schema.get_type(name) else {
            return Err(undefined_output_type_error(name, label, path, location));
        };
        match definition {
            Type::Scalar(scalar) => {
                if let Some(serialized) = builtin_scalar_passthrough(scalar, value) {
                    return Ok(serialized);
                }
                let serialized = if let Some(serialize) = scalar.serialize_fn() {
                    serialize(value).map_err(|message| {
                        scalar_serialize_error(label, &message, path, location)
                    })?
                } else {
                    value.clone()
                };
                if is_legal_scalar_value(&serialized) {
                    Ok(serialized)
                } else {
                    Err(invalid_scalar_value_error(
                        label,
                        serialized.kind(),
                        path,
                        location,
                    ))
                }
            }
            Type::Enum(enumeration) => match &value {
                Value::Enum(value) | Value::String(value) if enumeration.has_value(value) => {
                    Ok(Value::Enum(value.clone()))
                }
                Value::Enum(value) | Value::String(value) => Err(unknown_enum_value_error(
                    value,
                    &enumeration.name,
                    label,
                    path,
                    location,
                )),
                other => Err(enum_value_type_error(
                    label,
                    &enumeration.name,
                    other.kind(),
                    path,
                    location,
                )),
            },
            Type::Object(object) => {
                self.complete_object(
                    object, value, label, selections, location, path, errors, depth,
                )
                .await
            }
            Type::Interface(_) | Type::Union(_) => {
                if !matches!(value, Value::Object(_) | Value::External(_)) {
                    return Err(abstract_value_type_error(
                        name,
                        label,
                        value.kind(),
                        path,
                        location,
                    ));
                }
                let object = self
                    .resolve_abstract_type(name.as_ref(), value)
                    .map_err(|error| locate(error, path, location))?;
                self.complete_object(
                    object, value, label, selections, location, path, errors, depth,
                )
                .await
            }
            Type::InputObject(_) => Err(input_object_return_error(name, label, path, location)),
        }
    }

    #[allow(clippy::too_many_arguments)]
    async fn complete_object(
        &self,
        object: &'doc ObjectType,
        value: &Value,
        label: FieldLabel<'_>,
        selections: &Selections<'_, 'doc>,
        location: Location,
        path: &mut Path,
        errors: &mut Vec<GraphQLError>,
        depth: usize,
    ) -> Result<Value, GraphQLError> {
        if !matches!(value, Value::Object(_) | Value::External(_)) {
            return Err(object_mismatch_error(label, object, value, path, location));
        }
        if selections.is_empty() {
            return Err(missing_selection_set_error(label, object, path, location));
        }
        // The field that produced this value already tried the synchronous completion path (see
        // [`Self::try_complete_field_value_sync`]); reaching here means the subtree needs the
        // asynchronous path somewhere.
        let completed = self
            .execute_fields(object, value, selections, path, errors, depth)
            .await?;
        Ok(Value::Object(Arc::new(completed)))
    }

    /// Try to complete a selection set without awaiting anything.
    ///
    /// Returns `None` when the depth limit makes the fast path unsafe or when a field needs the
    /// asynchronous path. Either way the attempt left no trace: errors went into a dropped scratch
    /// list, every path segment was popped, and no caller code ran (resolvers, authorizers, meta
    /// fields, abstract types and non-built-in scalar serializers all bail out first).
    #[inline(never)]
    fn try_execute_fields_sync(
        &self,
        object_type: &'doc ObjectType,
        parent: &Value,
        selections: &Selections<'_, 'doc>,
        path: &mut Path,
        errors: &mut Vec<GraphQLError>,
        depth: usize,
    ) -> Option<Result<Arc<Object>, GraphQLError>> {
        if depth >= SYNC_DEPTH_LIMIT || self.type_has_resolvers(selections.source, object_type) {
            return None;
        }
        let collected = match collect::collect_fields_multi(
            &self.context.schema,
            &self.fragments,
            &self.context.variables,
            object_type,
            selections,
            self.plans,
        ) {
            Ok(collected) => collected,
            Err(error) => return Some(Err(error)),
        };

        let mut records = Vec::new();
        let outcome = match &collected {
            CollectedFields::Planned(plan) => {
                self.planned_fields_sync(plan, object_type, parent, path, &mut records, depth)
            }
            CollectedFields::Dynamic(fields) => {
                self.execute_fields_sync(fields, object_type, parent, path, &mut records, depth)
            }
        };
        match outcome {
            Ok(Ok(object)) => {
                append_errors(errors, records, self.context.max_errors);
                Some(Ok(object))
            }
            Ok(Err(())) => None,
            Err(error) => {
                append_errors(errors, records, self.context.max_errors);
                Some(Err(error))
            }
        }
    }

    /// [`Self::complete_field_value_sync`] with its own error buffer.
    ///
    /// The fast path may abandon a partially completed subtree, so its errors are collected
    /// separately and only become visible once the completion actually finished. It is never
    /// inlined, so its locals do not enlarge the recursive frames that bound execution's stack.
    #[allow(clippy::too_many_arguments)]
    #[inline(never)]
    fn try_complete_field_value_sync(
        &self,
        value: &Value,
        ty: &TypeRef,
        label: FieldLabel<'_>,
        selections: &Selections<'_, 'doc>,
        location: Location,
        path: &mut Path,
        errors: &mut Vec<GraphQLError>,
        depth: usize,
    ) -> Option<Result<Value, GraphQLError>> {
        let mut records = Vec::new();
        let outcome = self.complete_field_value_sync(
            value,
            ty,
            label,
            selections,
            location,
            path,
            &mut records,
            depth,
        )?;
        append_errors(errors, records, self.context.max_errors);
        Some(outcome)
    }

    /// Complete fields that are already collected, without awaiting.
    ///
    /// Every construct that could touch caller code is refused rather than approximated:
    /// resolvers, authorizers, meta fields, abstract types and custom scalar serializers all
    /// hand the work back so the async path handles them with the exact same semantics.
    /// Fill a response object from a precompiled field plan.
    ///
    /// The plan already resolved every group's schema field, response key, argument template and
    /// built-in scalar shape, so the common scalar property needs no lookup and no checks beyond
    /// reading the parent value.
    #[allow(clippy::too_many_arguments)]
    #[inline]
    fn planned_fields_sync(
        &self,
        plan: &'doc crate::execution::plan::FieldPlan<'doc>,
        object_type: &'doc ObjectType,
        parent: &Value,
        path: &mut Path,
        errors: &mut Vec<GraphQLError>,
        depth: usize,
    ) -> Result<Result<Arc<Object>, ()>, GraphQLError> {
        // The response object is the one allocation this path needs, filled in place.
        let mut result = Arc::new(Object::with_capacity(plan.len()));
        let object = Arc::get_mut(&mut result).expect("a fresh Arc is uniquely owned");
        // An authorizer is caller code and must observe every field, so the scalar shortcut is
        // only safe when none is configured.
        let parent_object = match parent {
            Value::Object(parent) if self.context.authorizer.is_none() => Some(parent),
            _ => None,
        };
        let check_deadline = self.deadline.is_some();
        for group in plan.groups() {
            let Some(variant) = group.variant(&self.context.variables) else {
                continue;
            };
            // A variable condition can exclude every node of the group.
            if variant.nodes.is_empty() {
                continue;
            }
            if check_deadline && self.expired() {
                return Err(timeout_error(path));
            }
            if let (Some(property), Some(parent_object)) = (&group.property, parent_object) {
                if let Some(value) = parent_object
                    .get(property.name.as_ref())
                    .and_then(|value| scalar_shape(property.kind, value))
                {
                    object.push_unchecked(property.key.clone(), value);
                    continue;
                }
            }
            let Some((response_key, outcome)) =
                self.planned_field_sync(object_type, parent, group, variant, path, errors, depth)
            else {
                return Ok(Err(()));
            };
            object.push_unchecked(response_key, outcome?);
        }
        Ok(Ok(result))
    }

    /// Complete fields that were already collected, without awaiting.
    ///
    /// This is the pre-plan executor loop verbatim: it runs for queries that are not prepared,
    /// where the plan's extra layers would only cost time.
    #[allow(clippy::too_many_arguments)]
    fn execute_fields_sync(
        &self,
        fields: &collect::DynamicFields<'doc>,
        object_type: &'doc ObjectType,
        parent: &Value,
        path: &mut Path,
        errors: &mut Vec<GraphQLError>,
        depth: usize,
    ) -> Result<Result<Arc<Object>, ()>, GraphQLError> {
        // The response object is the one allocation it needs, so it is created up front and
        // filled in place; that saves copying the whole object into the `Arc` afterwards.
        let mut result = Arc::new(Object::with_capacity(fields.len()));
        let object = Arc::get_mut(&mut result).expect("a fresh Arc is uniquely owned");
        let check_deadline = self.deadline.is_some();
        for (response_key, nodes, field) in fields {
            let response_key: &'doc str = response_key;
            if check_deadline && self.expired() {
                return Err(timeout_error(path));
            }
            // A property of a built-in scalar type is the single most common field in a response,
            // and it cannot fail: no resolver, no arguments, no sub-selection and a value that
            // already has the scalar's shape. Handling it here skips the path bookkeeping and the
            // general completion path entirely; every other case falls through unchanged.
            if let Some(field) = *field {
                if nodes.first().is_some_and(|first| first.alias.is_none()) {
                    if let Some((key, value)) = self.scalar_property(field, parent) {
                        object.push_unchecked(key, value);
                        continue;
                    }
                }
            }
            let Some((response_key, outcome)) = self.execute_field_dynamic_sync(
                object_type,
                parent,
                response_key,
                nodes,
                *field,
                path,
                errors,
                depth,
            ) else {
                return Ok(Err(()));
            };
            object.push_unchecked(response_key, outcome?);
        }
        Ok(Ok(result))
    }

    /// Complete one dynamically collected field without awaiting.
    ///
    /// This mirrors the pre-plan implementation; the planned path has its own
    /// [`Self::planned_field_sync`], which can read the plan's resolved field and arguments.
    #[allow(clippy::too_many_arguments)]
    fn execute_field_dynamic_sync(
        &self,
        object_type: &'doc ObjectType,
        parent: &Value,
        response_key: &'doc str,
        nodes: &[&'doc ast::Field<'doc>],
        hint: Option<&'doc crate::types::Field>,
        path: &mut Path,
        errors: &mut Vec<GraphQLError>,
        depth: usize,
    ) -> Option<(Arc<str>, Result<Value, GraphQLError>)> {
        let Some(first) = nodes.first().copied() else {
            return Some((
                Arc::from(response_key),
                Err(missing_field_node_error(response_key)),
            ));
        };
        if meta_return_type(first.name).is_some() {
            return None;
        }
        let key: Arc<str> = match (hint, first.alias) {
            (Some(field), None) => field.name.clone(),
            _ => Arc::from(response_key),
        };
        path.push(PathSegment::Field(key));
        let outcome = self.execute_field_inner_dynamic_sync(
            object_type,
            hint,
            parent,
            first,
            nodes,
            path,
            errors,
            depth,
        );
        Some((pop_field_key(path), outcome?))
    }

    /// [`Self::execute_field_dynamic_sync`] with the path segment already pushed.
    #[allow(clippy::too_many_arguments)]
    fn execute_field_inner_dynamic_sync(
        &self,
        object_type: &ObjectType,
        field: Option<&crate::types::Field>,
        parent: &Value,
        first: &'doc ast::Field<'doc>,
        nodes: &[&'doc ast::Field<'doc>],
        path: &mut Path,
        errors: &mut Vec<GraphQLError>,
        depth: usize,
    ) -> Option<Result<Value, GraphQLError>> {
        let location = first.location;
        let selection_sets: SmallVec<[&'doc ast::SelectionSet<'doc>; 2]> = match nodes {
            [only] if only.selection_set.is_none() => SmallVec::new(),
            _ => nodes
                .iter()
                .copied()
                .filter_map(|node| node.selection_set.as_ref())
                .collect(),
        };
        let selections = Selections::new(&selection_sets, None);
        if self.expired() {
            return Some(Err(timeout_error_at(path, location)));
        }

        let Some(field) = field else {
            let error = unknown_field_error(first.name, object_type.name.as_ref(), location);
            return Some(finish_field(
                Err(error),
                None,
                path,
                location,
                errors,
                self.context.max_errors,
            ));
        };

        if field.resolve.is_some() || self.context.authorizer.is_some() {
            return None;
        }

        let label = FieldLabel {
            parent: object_type.name.as_ref(),
            name: field.name.as_ref(),
        };
        if !(first.arguments.is_empty() && field.args.is_empty()) {
            let mut args = Object::with_capacity(field.args.len());
            if let Err(error) = coerce::coerce_argument_values_into(
                &self.context.schema,
                field,
                first,
                &self.context.variables,
                &mut args,
            ) {
                return Some(finish_field(
                    Err(error),
                    Some(&field.ty),
                    path,
                    location,
                    errors,
                    self.context.max_errors,
                ));
            }
        }

        let definition = field
            .type_index
            .and_then(|index| self.context.schema.type_at(index));
        let outcome = match default_property(label, field.name.as_ref(), parent).map(property_value)
        {
            Ok(value) => self.complete_field_value_typed_sync(
                &value,
                &field.ty,
                definition,
                label,
                &selections,
                location,
                path,
                errors,
                depth,
            )?,
            Err(error) => Err(error),
        };
        Some(finish_field(
            outcome,
            Some(&field.ty),
            path,
            location,
            errors,
            self.context.max_errors,
        ))
    }

    /// Complete an unaliased built-in scalar property, when the field is one.
    ///
    /// Returns `None` for every construct that could observe more than the value itself (a
    /// resolver, arguments, an authorizer, a custom scalar, a missing property or a value of the
    /// wrong shape), so the caller's general path keeps its exact behaviour.
    fn scalar_property(
        &self,
        field: &crate::types::Field,
        parent: &Value,
    ) -> Option<(Arc<str>, Value)> {
        if field.resolve.is_some() || !field.args.is_empty() || self.context.authorizer.is_some() {
            return None;
        }
        let definition = self.context.schema.type_at(field.type_index?)?;
        let Type::Scalar(scalar) = definition else {
            return None;
        };
        let Value::Object(parent_object) = parent else {
            return None;
        };
        let value = parent_object.get(field.name.as_ref())?;
        let serialized = builtin_scalar_passthrough(scalar, value)?;
        Some((field.name.clone(), serialized))
    }

    /// Complete one planned field without awaiting.
    fn planned_field_sync(
        &self,
        object_type: &'doc ObjectType,
        parent: &Value,
        group: &'doc plan::FieldGroup<'doc>,
        variant: &'doc plan::Variant<'doc>,
        path: &mut Path,
        errors: &mut Vec<GraphQLError>,
        depth: usize,
    ) -> Option<(Arc<str>, Result<Value, GraphQLError>)> {
        let Some(first) = variant.nodes.first() else {
            return Some((
                Arc::from(group.response_key()),
                Err(GraphQLError::new(format!(
                    "No field node was provided for response key \"{}\".",
                    group.response_key()
                ))),
            ));
        };
        // Meta fields have their own resolution rules, including introspection.
        if meta_return_type(first.name).is_some() {
            return None;
        }
        let field = group.field(object_type);
        let key = group.key(field);
        path.push(PathSegment::Field(key));
        let outcome =
            self.planned_field_inner_sync(object_type, group, variant, parent, path, errors, depth);
        Some((pop_field_key(path), outcome?))
    }

    #[allow(clippy::too_many_arguments)]
    fn planned_field_inner_sync(
        &self,
        object_type: &ObjectType,
        group: &plan::FieldGroup<'doc>,
        variant: &'doc plan::Variant<'doc>,
        parent: &Value,
        path: &mut Path,
        errors: &mut Vec<GraphQLError>,
        depth: usize,
    ) -> Option<Result<Value, GraphQLError>> {
        let first = variant.nodes.first()?;
        let location = first.location;
        let selection_sets = variant.selection_sets();
        let selections = Selections::new(&selection_sets, variant.child);

        if self.expired() {
            return Some(Err(timeout_error_at(path, location)));
        }

        let Some(field) = group.field(object_type) else {
            let error = unknown_field_error(first.name, object_type.name.as_ref(), location);
            return Some(finish_field(
                Err(error),
                None,
                path,
                location,
                errors,
                self.context.max_errors,
            ));
        };

        // A resolver or an authorizer is caller code: hand the whole subtree to the async path
        // rather than run any of it here.
        if field.resolve.is_some() || self.context.authorizer.is_some() {
            return None;
        }

        let label = FieldLabel {
            parent: object_type.name.as_ref(),
            name: field.name.as_ref(),
        };
        // The default resolver ignores arguments, but they are coerced so that a field with
        // arguments reports exactly the errors the async path would.
        let mut args = Object::with_capacity(field.args.len());
        if let Err(error) = self.planned_field_arguments(group, first, field, &mut args) {
            return Some(finish_field(
                Err(error),
                Some(&field.ty),
                path,
                location,
                errors,
                self.context.max_errors,
            ));
        }

        // The schema resolved this field's output type when it was built, so completing the field
        // does not have to look the type up by name.
        let definition = field
            .type_index
            .and_then(|index| self.context.schema.type_at(index));
        let outcome = match default_property(label, field.name.as_ref(), parent).map(property_value)
        {
            Ok(value) => self.complete_field_value_typed_sync(
                &value,
                &field.ty,
                definition,
                label,
                &selections,
                location,
                path,
                errors,
                depth,
            )?,
            Err(error) => Err(error),
        };
        Some(finish_field(
            outcome,
            Some(&field.ty),
            path,
            location,
            errors,
            self.context.max_errors,
        ))
    }

    /// Coerce a planned field's arguments from its compiled template.
    fn planned_field_arguments(
        &self,
        group: &plan::FieldGroup<'doc>,
        first: &ast::Field<'_>,
        field: &crate::types::Field,
        args: &mut Object,
    ) -> Result<(), GraphQLError> {
        if let Some(template) = group.args() {
            return template.fill(&self.context.variables, args);
        }
        if first.arguments.is_empty() && field.args.is_empty() {
            return Ok(());
        }
        coerce::coerce_argument_values_into(
            &self.context.schema,
            field,
            first,
            &self.context.variables,
            args,
        )
    }

    /// [`Self::complete_field_value_sync`] with the field's named output type already resolved.
    ///
    /// This is the shape nearly every completed field takes — a scalar, an object, or a list of
    /// objects — and resolving it from the schema once is what keeps `Schema::get_type` off the
    /// per-field path. Anything else (abstract types, lists of lists, or a field the schema did not
    /// resolve) falls back to the general function, which looks the type up itself.
    #[allow(clippy::too_many_arguments)]
    fn complete_field_value_typed_sync(
        &self,
        value: &Value,
        ty: &TypeRef,
        definition: Option<&'doc Type>,
        label: FieldLabel<'_>,
        selections: &Selections<'_, 'doc>,
        location: Location,
        path: &mut Path,
        errors: &mut Vec<GraphQLError>,
        depth: usize,
    ) -> Option<Result<Value, GraphQLError>> {
        let Some(definition) = definition else {
            return self.complete_field_value_sync(
                value, ty, label, selections, location, path, errors, depth,
            );
        };
        let mut current = ty;
        let mut non_null = false;
        while let TypeRef::NonNull(inner) = current {
            non_null = true;
            current = inner;
        }
        if value.is_null() {
            return self
                .complete_value_sync(value, ty, label, selections, location, path, errors, depth);
        }
        match (definition, current) {
            // The built-in scalars are the overwhelming majority of leaves, and their value needs
            // no work beyond the clone, so the common answer is returned without entering the
            // general leaf path (which re-checks for null before deciding).
            (Type::Scalar(scalar), TypeRef::Named(_)) if scalar.builtin => {
                match builtin_scalar_passthrough(scalar, value) {
                    Some(serialized) => Some(Ok(serialized)),
                    None => self.complete_leaf_definition(
                        value, definition, non_null, label, location, path, true,
                    ),
                }
            }
            (Type::Scalar(_) | Type::Enum(_), TypeRef::Named(_)) => self
                .complete_leaf_definition(value, definition, non_null, label, location, path, true),
            (Type::Object(object), TypeRef::Named(_)) => self.complete_object_sync(
                object,
                value,
                label,
                selections,
                location,
                path,
                errors,
                depth + 1,
            ),
            (Type::Object(object), TypeRef::List(inner)) => {
                let (element, items_non_null) = match &**inner {
                    TypeRef::NonNull(inner) => (&**inner, true),
                    inner => (inner, false),
                };
                if !matches!(element, TypeRef::Named(_)) {
                    return self.complete_value_sync(
                        value, ty, label, selections, location, path, errors, depth,
                    );
                }
                self.complete_object_list_sync(
                    object,
                    items_non_null,
                    value,
                    label,
                    selections,
                    location,
                    path,
                    errors,
                    depth + 1,
                )
            }
            _ => self
                .complete_value_sync(value, ty, label, selections, location, path, errors, depth),
        }
    }

    /// [`Self::complete_field_value`] without awaiting.
    #[allow(clippy::too_many_arguments)]
    fn complete_field_value_sync(
        &self,
        value: &Value,
        ty: &TypeRef,
        label: FieldLabel<'_>,
        selections: &Selections<'_, 'doc>,
        location: Location,
        path: &mut Path,
        errors: &mut Vec<GraphQLError>,
        depth: usize,
    ) -> Option<Result<Value, GraphQLError>> {
        if !value.is_null() {
            if let Some(object) = self.plain_object(ty) {
                return self.complete_object_sync(
                    object,
                    value,
                    label,
                    selections,
                    location,
                    path,
                    errors,
                    depth + 1,
                );
            }
            if let Some((object, items_non_null)) = self.object_list(ty) {
                return self.complete_object_list_sync(
                    object,
                    items_non_null,
                    value,
                    label,
                    selections,
                    location,
                    path,
                    errors,
                    depth + 1,
                );
            }
        }
        self.complete_value_sync(value, ty, label, selections, location, path, errors, depth)
    }

    /// [`Self::complete_value`] without awaiting.
    ///
    /// A list nested directly in a list goes back through the boxed algorithm, exactly as the
    /// asynchronous path does, so this never recurses further than the selection set does.
    #[allow(clippy::too_many_arguments)]
    fn complete_value_sync(
        &self,
        value: &Value,
        ty: &TypeRef,
        label: FieldLabel<'_>,
        selections: &Selections<'_, 'doc>,
        location: Location,
        path: &mut Path,
        errors: &mut Vec<GraphQLError>,
        depth: usize,
    ) -> Option<Result<Value, GraphQLError>> {
        let mut current = ty;
        let mut non_null = false;
        while let TypeRef::NonNull(inner) = current {
            non_null = true;
            current = inner;
        }
        if value.is_null() {
            return Some(if non_null {
                Err(located(
                    format!("Cannot return null for non-nullable field {label}."),
                    path,
                    location,
                ))
            } else {
                Ok(Value::Null)
            });
        }
        let completed = match current {
            TypeRef::List(inner) => match self.complete_list_sync(
                value, inner, label, selections, location, path, errors, depth,
            ) {
                Some(Ok(value)) => value,
                Some(Err(error)) => return Some(Err(error)),
                None => return None,
            },
            TypeRef::Named(name) => match self.complete_named_sync(
                value, name, label, selections, location, path, errors, depth,
            ) {
                Some(Ok(value)) => value,
                Some(Err(error)) => return Some(Err(error)),
                None => return None,
            },
            TypeRef::NonNull(_) => {
                return Some(Err(located(
                    "Internal execution error: non-null type was not unwrapped.",
                    path,
                    location,
                )));
            }
        };
        if non_null && completed.is_null() {
            return Some(Err(located(
                format!("Cannot return null for non-nullable field {label}."),
                path,
                location,
            )));
        }
        Some(Ok(completed))
    }

    /// [`Self::complete_object_list`] without awaiting.
    #[allow(clippy::too_many_arguments)]
    fn complete_object_list_sync(
        &self,
        object: &'doc ObjectType,
        items_non_null: bool,
        value: &Value,
        label: FieldLabel<'_>,
        selections: &Selections<'_, 'doc>,
        location: Location,
        path: &mut Path,
        errors: &mut Vec<GraphQLError>,
        depth: usize,
    ) -> Option<Result<Value, GraphQLError>> {
        let Value::List(items) = value else {
            return Some(Err(list_mismatch_error(label, value, path, location)));
        };
        // Every item completes against the same runtime type and selection sets, so the fields are
        // collected once for the whole list. A missing selection set keeps the per-item path,
        // which reports exactly the error (or null item) it reported before; a collection failure
        // (only reachable for a document that was never validated) is reported once for the field
        // rather than once per item.
        let collected = if selections.is_empty() {
            None
        } else {
            match self.collect_object_sync(object, label, selections, location, path, depth)? {
                Ok(collected) => Some(collected),
                Err(error) => return Some(Err(error)),
            }
        };
        let mut completed = Vec::with_capacity(items.len());
        for (index, item) in items.iter().enumerate() {
            path.push(PathSegment::index(index));
            let outcome = if item.is_null() {
                if items_non_null {
                    Err(located(
                        format!("Cannot return null for non-nullable field {label}."),
                        path,
                        location,
                    ))
                } else {
                    Ok(Value::Null)
                }
            } else {
                let done = match &collected {
                    Some(collected) => self.complete_collected_object_sync(
                        object, item, label, collected, location, path, errors, depth,
                    ),
                    None => self.complete_object_sync(
                        object, item, label, selections, location, path, errors, depth,
                    ),
                };
                match done {
                    Some(outcome) => outcome,
                    None => {
                        path.pop();
                        return None;
                    }
                }
            };
            path.pop();
            match outcome {
                Ok(value) => completed.push(value),
                Err(error) => {
                    if items_non_null {
                        return Some(Err(error));
                    }
                    record_error(errors, error, self.context.max_errors);
                    completed.push(Value::Null);
                }
            }
        }
        Some(Ok(Value::List(completed)))
    }

    /// Collect a plain object's selection sets for the synchronous path.
    ///
    /// Returns `None` when the asynchronous path is required — a type that defines any resolver is
    /// completed asynchronously whatever this particular selection asks for, which keeps the
    /// attempt free for resolver-heavy schemas (introspection, most of all).
    fn collect_object_sync(
        &self,
        object: &'doc ObjectType,
        label: FieldLabel<'_>,
        selections: &Selections<'_, 'doc>,
        location: Location,
        path: &mut Path,
        depth: usize,
    ) -> Option<Result<collect::CollectedFields<'doc>, GraphQLError>> {
        if depth >= SYNC_DEPTH_LIMIT || self.type_has_resolvers(selections.source, object) {
            return None;
        }
        if selections.is_empty() {
            return Some(Err(located(
                format!(
                    "Field {label} of type \"{}\" must have a selection set.",
                    object.name
                ),
                path,
                location,
            )));
        }
        if self.plans.is_none() {
            return Some(
                collect::collect_dynamic(
                    &self.context.schema,
                    &self.fragments,
                    &self.context.variables,
                    object,
                    selections.sets,
                )
                .map(CollectedFields::Dynamic),
            );
        }
        Some(collect::collect_fields_multi(
            &self.context.schema,
            &self.fragments,
            &self.context.variables,
            object,
            selections,
            self.plans,
        ))
    }

    /// Whether a type defines any resolver, which forces the asynchronous completion path.
    ///
    /// A planned source already recorded the answer at preparation, so the common case avoids
    /// scanning the object type's field map for every completed object.
    fn type_has_resolvers(&self, source: Option<u32>, object_type: &ObjectType) -> bool {
        if let (Some(plan), Some(source)) = (self.plans, source) {
            if let Some(field_plan) = plan.field_plan(source, object_type) {
                return field_plan.has_resolvers;
            }
        }
        object_type
            .fields
            .values()
            .any(|field| field.resolve.is_some())
    }

    /// [`Self::complete_object`] without awaiting.
    ///
    /// `depth` is the level of the object's own selection set.
    #[allow(clippy::too_many_arguments)]
    fn complete_object_sync(
        &self,
        object: &'doc ObjectType,
        value: &Value,
        label: FieldLabel<'_>,
        selections: &Selections<'_, 'doc>,
        location: Location,
        path: &mut Path,
        errors: &mut Vec<GraphQLError>,
        depth: usize,
    ) -> Option<Result<Value, GraphQLError>> {
        let collected =
            match self.collect_object_sync(object, label, selections, location, path, depth)? {
                Ok(collected) => collected,
                Err(error) => return Some(Err(error)),
            };
        self.complete_collected_object_sync(
            object, value, label, &collected, location, path, errors, depth,
        )
    }

    /// Complete one object with fields that were already collected.
    ///
    /// A list completes every item against the same runtime type and selection sets, so collecting
    /// the fields once (including fragment expansion and `@skip`/`@include` evaluation) is enough
    /// for all of its items.
    #[allow(clippy::too_many_arguments)]
    fn complete_collected_object_sync(
        &self,
        object: &'doc ObjectType,
        value: &Value,
        label: FieldLabel<'_>,
        collected: &collect::CollectedFields<'doc>,
        location: Location,
        path: &mut Path,
        errors: &mut Vec<GraphQLError>,
        depth: usize,
    ) -> Option<Result<Value, GraphQLError>> {
        if !matches!(value, Value::Object(_) | Value::External(_)) {
            return Some(Err(object_mismatch_error(
                label, object, value, path, location,
            )));
        }
        let outcome = match collected {
            CollectedFields::Planned(plan) => {
                self.planned_fields_sync(plan, object, value, path, errors, depth)
            }
            CollectedFields::Dynamic(fields) => {
                self.execute_fields_sync(fields, object, value, path, errors, depth)
            }
        };
        match outcome {
            Ok(Ok(completed)) => Some(Ok(Value::Object(completed))),
            Ok(Err(_)) => None,
            Err(error) => Some(Err(error)),
        }
    }

    /// [`Self::complete_list`] without awaiting.
    #[allow(clippy::too_many_arguments)]
    fn complete_list_sync(
        &self,
        value: &Value,
        inner: &TypeRef,
        label: FieldLabel<'_>,
        selections: &Selections<'_, 'doc>,
        location: Location,
        path: &mut Path,
        errors: &mut Vec<GraphQLError>,
        depth: usize,
    ) -> Option<Result<Value, GraphQLError>> {
        let Value::List(items) = value else {
            return Some(Err(located(
                format!(
                    "Expected a list for field {label}, but the resolved value is a {}.",
                    value.kind()
                ),
                path,
                location,
            )));
        };
        let object = self.plain_object(inner);
        let mut completed = Vec::with_capacity(items.len());
        for (index, item) in items.iter().enumerate() {
            path.push(PathSegment::index(index));
            let outcome = match &object {
                Some(object) if !item.is_null() => match self.complete_object_sync(
                    object,
                    item,
                    label,
                    selections,
                    location,
                    path,
                    errors,
                    depth + 1,
                ) {
                    Some(outcome) => outcome,
                    None => {
                        path.pop();
                        return None;
                    }
                },
                _ => match self.complete_item_sync(
                    item, inner, label, selections, location, path, errors, depth,
                ) {
                    Some(outcome) => outcome,
                    None => {
                        path.pop();
                        return None;
                    }
                },
            };
            path.pop();
            match outcome {
                Ok(value) => completed.push(value),
                Err(error) => {
                    if inner.is_non_null() {
                        return Some(Err(error));
                    }
                    record_error(errors, error, self.context.max_errors);
                    completed.push(Value::Null);
                }
            }
        }
        Some(Ok(Value::List(completed)))
    }

    /// [`Self::complete_item`] without awaiting.
    #[allow(clippy::too_many_arguments)]
    fn complete_item_sync(
        &self,
        item: &Value,
        inner: &TypeRef,
        label: FieldLabel<'_>,
        selections: &Selections<'_, 'doc>,
        location: Location,
        path: &mut Path,
        errors: &mut Vec<GraphQLError>,
        depth: usize,
    ) -> Option<Result<Value, GraphQLError>> {
        let mut current = inner;
        let mut non_null = false;
        while let TypeRef::NonNull(wrapped) = current {
            non_null = true;
            current = wrapped;
        }
        if item.is_null() {
            return Some(if non_null {
                Err(located(
                    format!("Cannot return null for non-nullable field {label}."),
                    path,
                    location,
                ))
            } else {
                Ok(Value::Null)
            });
        }
        let completed = match current {
            // A list inside a list goes back through the boxed algorithm, which is what keeps
            // the completion future's size finite.
            TypeRef::List(_) => return None,
            TypeRef::Named(name) => match self
                .complete_named_sync(item, name, label, selections, location, path, errors, depth)
            {
                Some(Ok(value)) => value,
                Some(Err(error)) => return Some(Err(error)),
                None => return None,
            },
            TypeRef::NonNull(_) => {
                return Some(Err(located(
                    "Internal execution error: non-null type was not unwrapped.",
                    path,
                    location,
                )));
            }
        };
        if non_null && completed.is_null() {
            return Some(Err(located(
                format!("Cannot return null for non-nullable field {label}."),
                path,
                location,
            )));
        }
        Some(Ok(completed))
    }

    /// [`Self::complete_named`] without awaiting.
    #[allow(clippy::too_many_arguments)]
    fn complete_named_sync(
        &self,
        value: &Value,
        name: &crate::types::Name,
        label: FieldLabel<'_>,
        selections: &Selections<'_, 'doc>,
        location: Location,
        path: &mut Path,
        errors: &mut Vec<GraphQLError>,
        depth: usize,
    ) -> Option<Result<Value, GraphQLError>> {
        let Some(definition) = self.context.schema.get_type(name) else {
            return Some(Err(located(
                format!("Type \"{name}\" used by field {label} is not defined by the schema."),
                path,
                location,
            )));
        };
        match definition {
            Type::Scalar(scalar) => {
                if let Some(serialized) = builtin_scalar_passthrough(scalar, value) {
                    return Some(Ok(serialized));
                }
                if scalar.serialize_fn().is_some() && !scalar.builtin {
                    return None;
                }
                let serialized = value.clone();
                if is_legal_scalar_value(&serialized) {
                    Some(Ok(serialized))
                } else {
                    Some(Err(located(
                        format!(
                            "Field {label} serialized to a {} which is not a valid scalar value.",
                            serialized.kind()
                        ),
                        path,
                        location,
                    )))
                }
            }
            Type::Enum(enumeration) => match value {
                Value::Enum(value) | Value::String(value) if enumeration.has_value(value) => {
                    Some(Ok(Value::Enum(value.clone())))
                }
                Value::Enum(value) | Value::String(value) => Some(Err(located(
                    format!(
                        "Value \"{value}\" does not exist in enum \"{}\" for field {label}.",
                        enumeration.name
                    ),
                    path,
                    location,
                ))),
                other => Some(Err(located(
                    format!(
                        "Field {label} expects enum \"{}\" but received a {}.",
                        enumeration.name,
                        other.kind()
                    ),
                    path,
                    location,
                ))),
            },
            Type::Object(object) => self.complete_object_sync(
                object,
                value,
                label,
                selections,
                location,
                path,
                errors,
                depth + 1,
            ),
            Type::Interface(_) | Type::Union(_) => None,
            Type::InputObject(_) => Some(Err(located(
                format!("Field {label} has input object type \"{name}\" as its return type."),
                path,
                location,
            ))),
        }
    }

    fn resolve_abstract_type(
        &self,
        abstract_name: &str,
        value: &Value,
    ) -> Result<&'doc ObjectType, GraphQLError> {
        let Some(definition) = self.context.schema.get_type(abstract_name) else {
            return Err(GraphQLError::new(format!(
                "Abstract type \"{abstract_name}\" is not defined by the schema."
            )));
        };

        let resolve_type = match definition {
            Type::Interface(interface) => interface.resolve_type.clone(),
            Type::Union(union) => union.resolve_type.clone(),
            _ => {
                return Err(GraphQLError::new(format!(
                    "Type \"{abstract_name}\" is not an abstract type."
                )));
            }
        };

        let resolved = if let Some(resolve) = resolve_type {
            Some(resolve(value, self.context)?)
        } else if let Value::Object(object) = value {
            // An explicit `__typename` is the documented way for a raw object to choose its
            // concrete type, so it wins. `TYPENAME_KEY` is an internal introspection detail and
            // is only trusted when the value really is an introspection descriptor; otherwise a
            // user-supplied raw map could switch its own runtime type.
            object
                .get("__typename")
                .and_then(Value::as_str)
                .or_else(|| introspection_descriptor_typename(object))
                .map(Arc::from)
        } else {
            None
        };

        let concrete = match resolved {
            Some(name) => name,
            None => {
                let mut matches = Vec::new();
                for possible in self.context.schema.possible_types(abstract_name) {
                    let Some(object) = self.context.schema.object_type(&possible) else {
                        continue;
                    };
                    if let Some(check) = &object.is_type_of {
                        if check(value) {
                            matches.push(object.name.clone());
                        }
                    }
                }
                match matches.len() {
                    0 => {
                        return Err(GraphQLError::new(format!(
                            "Abstract type \"{abstract_name}\" could not determine the runtime type of the value; add a resolve_type function or an is_type_of check."
                        )));
                    }
                    1 => matches
                        .into_iter()
                        .next()
                        .ok_or_else(|| GraphQLError::new("No runtime type matched."))?,
                    _ => {
                        return Err(GraphQLError::new(format!(
                            "Abstract type \"{abstract_name}\" matched multiple runtime types; resolve_type must return exactly one."
                        )));
                    }
                }
            }
        };

        if !self
            .context
            .schema
            .is_possible_type(abstract_name, &concrete)
        {
            return Err(GraphQLError::new(format!(
                "Runtime type \"{concrete}\" is not a possible type of \"{abstract_name}\"."
            )));
        }
        self.context
            .schema
            .object_type(&concrete)
            .map(AsRef::as_ref)
            .ok_or_else(|| {
                GraphQLError::new(format!(
                    "Runtime type \"{concrete}\" is not an object type defined by the schema."
                ))
            })
    }

    fn resolve_meta(
        &self,
        object_type: &ObjectType,
        parent: &Value,
        node: &ast::Field<'_>,
    ) -> Result<Value, GraphQLError> {
        if node.name == "__schema" || node.name == "__type" {
            if self.context.schema.query_name().as_ref() != object_type.name.as_ref() {
                return Err(GraphQLError::new(format!(
                    "\"{}\" may only be queried on the query root type.",
                    node.name
                ))
                .at(node.location));
            }
            if !self.context.introspection {
                return Err(GraphQLError::new(format!(
                    "\"{}\" is not available because introspection is disabled.",
                    node.name
                ))
                .at(node.location));
            }
            return introspection::resolve_meta_field(node.name, node, self.context)
                .unwrap_or_else(|| Err(unknown_meta_field(node, object_type)));
        }

        if let Some(result) = introspection::resolve_meta_field(node.name, node, self.context) {
            return result;
        }

        if node.name == "__typename" {
            // The executor already completed this value as `object_type` (abstract resolution
            // and fragment matching both happened against it), so report that type. Only an
            // introspection runtime descriptor may override it with its internal type key.
            let typename = match parent {
                Value::Object(object) => introspection_descriptor_typename(object)
                    .map(Arc::from)
                    .unwrap_or_else(|| object_type.name.clone()),
                _ => object_type.name.clone(),
            };
            return Ok(Value::String(typename));
        }

        Err(unknown_meta_field(node, object_type))
    }
}

/// The runtime type name of an introspection descriptor.
///
/// Introspection values are [`Value::Object`]s whose keys are all private `__gql_` metadata.
/// Anything containing a non-internal key is user data, even when a caller has inserted
/// `__gql_typename` into it. Treating those values as descriptors would let a raw object map
/// choose its own runtime type, bypassing `resolve_type`, object authorization and
/// `is_type_of` selection.
fn introspection_descriptor_typename(object: &Object) -> Option<&str> {
    let name = object.get(introspection::TYPENAME_KEY)?.as_str()?;
    object
        .keys()
        .all(|key| key.starts_with("__gql_"))
        .then_some(name)
}

/// Run sibling field futures with bounded concurrency.
///
/// This lives in its own boxed future because the ordered-stream bookkeeping is otherwise part of
/// every recursive [`Executor::execute_fields`] frame, and execution must fit a small stack.
fn buffered<'s, F>(futures: Vec<F>, max_concurrency: usize) -> BoxFuture<'s, Vec<F::Output>>
where
    F: Future + Send + 's,
    F::Output: Send + 's,
{
    async move {
        stream::iter(futures)
            .buffered(max_concurrency.max(1))
            .collect()
            .await
    }
    .boxed()
}

fn unknown_meta_field(node: &ast::Field<'_>, object_type: &ObjectType) -> GraphQLError {
    GraphQLError::new(format!(
        "Cannot query field \"{}\" on type \"{}\".",
        node.name, object_type.name
    ))
    .at(node.location)
}

fn meta_return_type(name: &str) -> Option<TypeRef> {
    // Every meta field starts with `__`, so one two-byte comparison rejects the vast majority of
    // field names without walking the match arms.
    if !name.starts_with("__") {
        return None;
    }
    match name {
        "__typename" => Some(TypeRef::string().non_null()),
        "__schema" => Some(TypeRef::named("__Schema").non_null()),
        "__type" => Some(TypeRef::named("__Type")),
        _ => None,
    }
}

/// Read a field's value from its parent object without cloning it.
///
/// `None` means "this field is null": either the parent is explicit null, or the object has no
/// such property. Both cases complete as `Value::Null`, exactly as the default resolver's
/// `unwrap_or(Value::Null)` did, without allocating a copy of a composite value first.
fn default_property<'v>(
    label: FieldLabel<'_>,
    field_name: &str,
    parent: &'v Value,
) -> Result<Option<&'v Value>, GraphQLError> {
    match parent {
        Value::Object(object) => Ok(object.get(field_name)),
        Value::Null => Ok(None),
        Value::External(_) => Err(GraphQLError::new(format!(
            "No resolver is defined for field \"{label}\"; the parent is an opaque model value, so the default property resolver cannot read it."
        ))),
        other => Err(GraphQLError::new(format!(
            "No resolver is defined for field \"{label}\"; the default resolver requires an object parent, but the parent is a {}.",
            other.kind()
        ))),
    }
}

/// Turn a borrowed property into a value the caller can complete.
///
/// A missing property becomes an owned `null`, which allocates nothing; a present one stays
/// borrowed so completion reads the parent's value in place.
fn property_value<'v>(value: Option<&'v Value>) -> Cow<'v, Value> {
    value.map_or(Cow::Owned(Value::Null), Cow::Borrowed)
}

/// The value a planned scalar property passes through unchanged.
///
/// This is [`builtin_scalar_passthrough`] with the scalar already identified at preparation, so a
/// scalar leaf costs one shape check and a clone.
fn scalar_shape(kind: plan::ScalarKind, value: &Value) -> Option<Value> {
    let matches = match kind {
        plan::ScalarKind::String => matches!(value, Value::String(_)),
        plan::ScalarKind::Int => matches!(value, Value::Int(_)),
        plan::ScalarKind::Boolean => matches!(value, Value::Boolean(_)),
        plan::ScalarKind::Float => matches!(value, Value::Float(number) if number.is_finite()),
    };
    matches.then(|| value.clone())
}

/// A value a built-in scalar serialises to itself.
///
/// The built-in scalars accept exactly the JSON shape their name implies, so a value that already
/// has that shape needs no call into the registered serializer — which is an indirect call through
/// an `Arc<dyn Fn>` for nearly every scalar leaf in a response. Anything else (an `Int` where an
/// `ID` belongs, a non-finite `Float`, or a scalar the schema replaced) returns `None` and takes
/// the general path, so behaviour and errors are unchanged.
fn builtin_scalar_passthrough(scalar: &ScalarType, value: &Value) -> Option<Value> {
    if !scalar.builtin {
        return None;
    }
    // The scalar is identified by its name's length and first byte before the bytes are compared:
    // string equality against a literal compiles to a `memcmp` call, and this runs for every
    // scalar leaf in a response.
    let name = scalar.name.as_ref();
    let matches = match (name.len(), name.as_bytes().first()) {
        (6, Some(b'S')) if crate::types::names_eq(name, "String") => {
            matches!(value, Value::String(_))
        }
        (2, Some(b'I')) if crate::types::names_eq(name, "ID") => {
            matches!(value, Value::String(_))
        }
        (3, Some(b'I')) if crate::types::names_eq(name, "Int") => matches!(value, Value::Int(_)),
        (7, Some(b'B')) if crate::types::names_eq(name, "Boolean") => {
            matches!(value, Value::Boolean(_))
        }
        (5, Some(b'F')) if crate::types::names_eq(name, "Float") => {
            matches!(value, Value::Float(number) if number.is_finite())
        }
        _ => false,
    };
    matches.then(|| value.clone())
}

/// Take back the response key a field pushed onto the path.
///
/// The key is moved into the path rather than cloned into it, so it has to be moved back out to
/// name the response field. The path always holds a field segment here.
fn pop_field_key(path: &mut Path) -> Arc<str> {
    let segment = path.pop();
    debug_assert!(matches!(segment, Some(PathSegment::Field(_))));
    match segment {
        Some(PathSegment::Field(key)) => key,
        _ => Arc::from(""),
    }
}

fn finish_field(
    outcome: Result<Value, GraphQLError>,
    ty: Option<&TypeRef>,
    path: &[PathSegment],
    location: Location,
    errors: &mut Vec<GraphQLError>,
    max_errors: usize,
) -> Result<Value, GraphQLError> {
    match outcome {
        Ok(value) => Ok(value),
        Err(error) => {
            let error = locate(error, path, location);
            if ty.is_some_and(TypeRef::is_non_null) {
                // Keep bubbling; whoever catches this records it once.
                Err(error)
            } else {
                record_error(errors, error, max_errors);
                Ok(Value::Null)
            }
        }
    }
}

fn locate(mut error: GraphQLError, path: &[PathSegment], location: Location) -> GraphQLError {
    if error.path().is_none() {
        error.set_path(path.to_vec());
    }
    if error.locations().is_empty() {
        error = error.at(location);
    }
    error
}

#[cold]
#[inline(never)]
fn object_mismatch_error(
    label: FieldLabel<'_>,
    object: &ObjectType,
    value: &Value,
    path: &[PathSegment],
    location: Location,
) -> GraphQLError {
    located(
        format!(
            "Field {label} expects an object of type \"{}\" but received a {}.",
            object.name,
            value.kind()
        ),
        path,
        location,
    )
}

#[cold]
#[inline(never)]
fn missing_selection_set_error(
    label: FieldLabel<'_>,
    object: &ObjectType,
    path: &[PathSegment],
    location: Location,
) -> GraphQLError {
    located(
        format!(
            "Field {label} of type \"{}\" must have a selection set.",
            object.name
        ),
        path,
        location,
    )
}

#[cold]
#[inline(never)]
fn timeout_error(path: &[PathSegment]) -> GraphQLError {
    GraphQLError::new("The operation timed out before it could finish.").with_path(path.to_vec())
}

#[cold]
#[inline(never)]
fn timeout_error_at(path: &[PathSegment], location: Location) -> GraphQLError {
    timeout_error(path).at(location)
}

#[cold]
#[inline(never)]
fn unknown_field_error(name: &str, parent: &str, location: Location) -> GraphQLError {
    GraphQLError::new(format!(
        "Cannot query field \"{name}\" on type \"{parent}\"."
    ))
    .at(location)
}

#[cold]
#[inline(never)]
fn missing_field_node_error(response_key: &str) -> GraphQLError {
    GraphQLError::new(format!(
        "No field node was provided for response key \"{response_key}\"."
    ))
}

#[cold]
#[inline(never)]
fn list_mismatch_error(
    label: FieldLabel<'_>,
    value: &Value,
    path: &[PathSegment],
    location: Location,
) -> GraphQLError {
    located(
        format!(
            "Expected a list for field {label}, but the resolved value is a {}.",
            value.kind()
        ),
        path,
        location,
    )
}

#[cold]
#[inline(never)]
fn non_null_error(label: FieldLabel<'_>, path: &[PathSegment], location: Location) -> GraphQLError {
    located(
        format!("Cannot return null for non-nullable field {label}."),
        path,
        location,
    )
}

#[cold]
#[inline(never)]
fn undefined_output_type_error(
    name: &crate::types::Name,
    label: FieldLabel<'_>,
    path: &[PathSegment],
    location: Location,
) -> GraphQLError {
    located(
        format!("Type \"{name}\" used by field {label} is not defined by the schema."),
        path,
        location,
    )
}

#[cold]
#[inline(never)]
fn scalar_serialize_error(
    label: FieldLabel<'_>,
    message: &str,
    path: &[PathSegment],
    location: Location,
) -> GraphQLError {
    located(
        format!("Field {label} could not be serialized: {message}"),
        path,
        location,
    )
}

#[cold]
#[inline(never)]
fn invalid_scalar_value_error(
    label: FieldLabel<'_>,
    kind: &str,
    path: &[PathSegment],
    location: Location,
) -> GraphQLError {
    located(
        format!("Field {label} serialized to a {kind} which is not a valid scalar value."),
        path,
        location,
    )
}

#[cold]
#[inline(never)]
fn unknown_enum_value_error(
    value: &str,
    enumeration: &crate::types::Name,
    label: FieldLabel<'_>,
    path: &[PathSegment],
    location: Location,
) -> GraphQLError {
    located(
        format!("Value \"{value}\" does not exist in enum \"{enumeration}\" for field {label}."),
        path,
        location,
    )
}

#[cold]
#[inline(never)]
fn enum_value_type_error(
    label: FieldLabel<'_>,
    enumeration: &crate::types::Name,
    kind: &str,
    path: &[PathSegment],
    location: Location,
) -> GraphQLError {
    located(
        format!("Field {label} expects enum \"{enumeration}\" but received a {kind}."),
        path,
        location,
    )
}

#[cold]
#[inline(never)]
fn abstract_value_type_error(
    name: &crate::types::Name,
    label: FieldLabel<'_>,
    kind: &str,
    path: &[PathSegment],
    location: Location,
) -> GraphQLError {
    located(
        format!("Field {label} expects a {name} but received a {kind}."),
        path,
        location,
    )
}

#[cold]
#[inline(never)]
fn input_object_return_error(
    name: &crate::types::Name,
    label: FieldLabel<'_>,
    path: &[PathSegment],
    location: Location,
) -> GraphQLError {
    located(
        format!("Field {label} has input object type \"{name}\" as its return type."),
        path,
        location,
    )
}

/// A shared empty argument object for fields that take no arguments.
fn empty_object() -> &'static Object {
    use std::sync::OnceLock;
    static EMPTY: OnceLock<Object> = OnceLock::new();
    EMPTY.get_or_init(Object::new)
}

fn located(
    message: impl Into<std::borrow::Cow<'static, str>>,
    path: &[PathSegment],
    location: Location,
) -> GraphQLError {
    GraphQLError::new(message)
        .with_path(path.to_vec())
        .at(location)
}

/// Append an error unless the configured limit has been reached.
///
/// A limit of zero therefore reports no errors, matching the option's documented meaning.
pub(crate) fn append_errors(
    errors: &mut Vec<GraphQLError>,
    extra: Vec<GraphQLError>,
    max_errors: usize,
) {
    for error in extra {
        if max_errors != 0 && errors.len() >= max_errors {
            return;
        }
        errors.push(error);
    }
}

fn record_error(errors: &mut Vec<GraphQLError>, error: GraphQLError, max_errors: usize) {
    // Zero means "no limit"; it must not silently discard every error.
    if max_errors == 0 || errors.len() < max_errors {
        errors.push(error);
    }
}

/// Take the response extensions accumulated by resolvers and plugins.
pub(crate) fn take_extensions(context: &ExecutionContext) -> Option<Object> {
    context
        .extensions
        .lock()
        .ok()
        .and_then(|mut extensions| extensions.take())
}

fn is_legal_scalar_value(value: &Value) -> bool {
    match value {
        Value::Null
        | Value::Boolean(_)
        | Value::Int(_)
        | Value::BigInt(_)
        | Value::String(_)
        | Value::Enum(_) => true,
        // JSON has no representation for NaN or infinity, and the GraphQL `Float` scalar is
        // defined as a finite IEEE-754 double.
        Value::Float(value) => value.is_finite(),
        Value::List(_) | Value::Object(_) | Value::External(_) => false,
    }
}
