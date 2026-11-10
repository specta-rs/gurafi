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
    complexity::{self, ComplexityOptions},
    error::{GraphQLError, Location, Path, PathSegment, Response},
    introspection,
    parser::parse_query,
    schema::Schema,
    types::{ObjectType, ResolveInfo, ResolverContext, Type, TypeRef},
    validation::{self, ValidationOptions},
};

use super::{AuthorizationContext, ExecutionContext, ExecutionOptions, Request, coerce, collect};

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
                .find(|(known, _)| *known == name)
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

    let variables = match coerce::coerce_variable_values(schema, operation, request.variables) {
        Ok(variables) => variables,
        Err(errors) => {
            return Response {
                data: None,
                errors,
                extensions: None,
            };
        }
    };

    if let Err(error) = complexity::analyze(
        schema,
        &document,
        operation.name,
        &variables.values,
        &ComplexityOptions {
            list_factor: options.list_complexity,
            field_cost: 1,
            max_complexity: options.max_complexity,
            max_depth: options.max_depth,
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

    let context = ExecutionContext {
        schema: schema.clone(),
        user_data: options.context.clone(),
        operation_name: operation.name.map(Arc::from),
        variables: variables.values,
        introspection: options.introspection,
        authorizer: options.authorizer.clone(),
        max_errors: options.max_errors,
        max_concurrency: options.max_concurrency,
        extensions: Mutex::new(None),
    };
    let deadline = options.timeout.map(|timeout| Instant::now() + timeout);

    let fragments = FragmentMap::new(&document);
    run_operation(
        &context,
        fragments,
        root_type.clone(),
        &operation.selection_set,
        options.root_value.clone(),
        deadline,
        matches!(operation.kind, OperationKind::Query),
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
    root_type: Arc<ObjectType>,
    selection_set: &'doc ast::SelectionSet<'doc>,
    parent: Value,
    deadline: Option<Instant>,
    parallel: bool,
) -> Response {
    let executor = Executor {
        context,
        fragments,
        deadline,
        parallel,
    };
    let mut path = Path::new();
    let mut errors = Vec::new();
    let selection_sets = std::slice::from_ref(&selection_set);
    let result = match executor.try_execute_fields_sync(
        &root_type,
        &parent,
        selection_sets,
        &mut path,
        &mut errors,
        0,
    ) {
        Some(result) => result,
        None => executor
            .execute_fields(
                root_type,
                &parent,
                selection_sets,
                &mut path,
                &mut errors,
                0,
            )
            .await
            .map(Arc::new),
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
/// Each source event is the *field value*, not a new root value: the subscription field's
/// `subscribe` resolver produced it, so executing the root field again (and therefore calling its
/// `resolve` resolver) would be wrong. Only the field's declared type and selection sets are
/// completed, exactly as `CompleteValue` would for a field resolved to this event.
///
/// The document is re-parsed because the returned response stream is `'static` and therefore
/// cannot borrow the request string. Validation and variable coercion are *not* repeated; the
/// coerced field type is passed in from setup.
pub(crate) async fn execute_subscription_event(
    context: &ExecutionContext,
    query: &str,
    operation_name: Option<&str>,
    response_key: &str,
    field_ty: TypeRef,
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

    let collected = match collect::collect_fields(
        &context.schema,
        &fragments,
        &context.variables,
        root_type.name.as_ref(),
        &operation.selection_set,
    ) {
        Ok(collected) => collected,
        Err(error) => return Response::error(error),
    };
    let nodes = collected
        .iter()
        .find(|(key, _)| *key == response_key)
        .map(|(_, nodes)| nodes.as_slice());
    let Some(nodes) = nodes else {
        return Response::error(GraphQLError::new(format!(
            "Subscription root field \"{response_key}\" was not selected."
        )));
    };
    let Some(first) = nodes.first().copied() else {
        return Response::error(GraphQLError::new(
            "A subscription root field has no field node.",
        ));
    };
    let selection_sets: Vec<&ast::SelectionSet<'_>> = nodes
        .iter()
        .filter_map(|node| (*node).selection_set.as_ref())
        .collect();

    let label = FieldLabel {
        parent: root_type.name.as_ref(),
        name: first.name,
    };
    let mut path = Path::new();
    path.push(PathSegment::field(response_key));
    let mut errors = Vec::new();
    let executor = Executor {
        context,
        fragments,
        deadline,
        parallel: true,
    };
    let outcome = executor
        .complete_value(
            &event,
            &field_ty,
            label,
            &selection_sets,
            first.location,
            &mut path,
            &mut errors,
            0,
        )
        .await;

    match outcome {
        Ok(value) => {
            let mut data = Object::with_capacity(1);
            data.insert(response_key, value);
            Response {
                data: Some(Value::Object(Arc::new(data))),
                errors,
                extensions: take_extensions(context),
            }
        }
        Err(error) => {
            record_error(&mut errors, error, context.max_errors);
            let data = if field_ty.is_non_null() {
                Some(Value::Null)
            } else {
                let mut data = Object::new();
                data.insert(response_key, Value::Null);
                Some(Value::Object(Arc::new(data)))
            };
            Response {
                data,
                errors,
                extensions: take_extensions(context),
            }
        }
    }
}

struct Executor<'doc> {
    context: &'doc ExecutionContext,
    fragments: FragmentMap<'doc>,
    deadline: Option<Instant>,
    /// Whether sibling fields at this level may resolve concurrently.
    parallel: bool,
}

impl<'doc> Executor<'doc> {
    fn expired(&self) -> bool {
        self.deadline
            .is_some_and(|deadline| Instant::now() >= deadline)
    }

    fn execute_fields<'s>(
        &'s self,
        object_type: Arc<ObjectType>,
        parent: &'s Value,
        selection_sets: &'s [&'doc ast::SelectionSet<'doc>],
        path: &'s mut Path,
        errors: &'s mut Vec<GraphQLError>,
        depth: usize,
    ) -> BoxFuture<'s, Result<Object, GraphQLError>>
    where
        'doc: 's,
    {
        async move {
            let mut collected = collect::collect_fields_multi(
                &self.context.schema,
                &self.fragments,
                &self.context.variables,
                object_type.name.as_ref(),
                selection_sets,
            )?;
            let mut result = Object::with_capacity(collected.len());
            if self.parallel
                && collected.len() > 1
                && self.try_execute_all_leaves(
                    &object_type,
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
                for (response_key, nodes) in collected {
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
                    let inline = self.try_execute_leaf(
                        &object_type,
                        parent,
                        response_key,
                        &nodes,
                        path,
                        &mut field_errors,
                    );
                    let future = match inline {
                        Some((response_key, outcome)) => {
                            Either::Left(ready((response_key, outcome, field_errors)))
                        }
                        None => {
                            let mut field_path = path.clone();
                            let object_type = object_type.clone();
                            Either::Right(async move {
                                let (response_key, outcome) = self
                                    .execute_field(
                                        object_type,
                                        parent,
                                        response_key,
                                        &nodes,
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
            for (response_key, nodes) in collected {
                // Mutations must keep document order, and a single field has no concurrency to
                // gain.
                if self.expired() {
                    return Err(GraphQLError::new(
                        "The operation timed out before it could finish.",
                    )
                    .with_path(path.to_vec()));
                }
                let (response_key, outcome) = match self.try_execute_leaf(
                    &object_type,
                    parent,
                    response_key,
                    &nodes,
                    path,
                    errors,
                ) {
                    Some(done) => done,
                    None => {
                        self.execute_field(
                            object_type.clone(),
                            parent,
                            response_key,
                            &nodes,
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
        .boxed()
    }

    /// Complete a selection set made only of object properties.
    ///
    /// This runs before the asynchronous batch is built so such sets never allocate or poll a
    /// future, and it is a separate function so its locals do not enlarge the recursive
    /// [`Self::execute_fields`] frame.
    fn try_execute_all_leaves(
        &self,
        object_type: &ObjectType,
        parent: &Value,
        collected: &mut collect::CollectedFields<'doc>,
        path: &mut Path,
        errors: &mut Vec<GraphQLError>,
        result: &mut Object,
    ) -> Result<bool, GraphQLError> {
        if !collected
            .iter()
            .all(|(_, nodes)| self.can_execute_leaf_inline(object_type, nodes))
        {
            return Ok(false);
        }
        for (response_key, nodes) in collected.drain(..) {
            if self.expired() {
                return Err(
                    GraphQLError::new("The operation timed out before it could finish.")
                        .with_path(path.to_vec()),
                );
            }
            let (response_key, outcome) = self
                .try_execute_leaf(object_type, parent, response_key, &nodes, path, errors)
                .expect("the inline predicate and the fast path agree");
            result.push_unchecked(response_key, outcome?);
        }
        Ok(true)
    }

    /// Whether a field can be completed by [`Self::try_execute_leaf`].
    fn can_execute_leaf_inline(
        &self,
        object_type: &ObjectType,
        nodes: &[&'doc ast::Field<'doc>],
    ) -> bool {
        nodes
            .first()
            .is_some_and(|first| self.leaf_field(object_type, first).is_some())
    }

    /// The schema field for a selection that the synchronous leaf path can complete.
    ///
    /// The field must exist, have no resolver and no arguments (so argument coercion cannot
    /// fail or be observed), and return a scalar or enum.
    fn leaf_field<'f>(
        &self,
        object_type: &'f ObjectType,
        first: &ast::Field<'_>,
    ) -> Option<&'f crate::types::Field> {
        if !first.arguments.is_empty() || meta_return_type(first.name).is_some() {
            return None;
        }
        let field = object_type.get_field(first.name)?;
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
    fn try_execute_leaf<'s>(
        &'s self,
        object_type: &ObjectType,
        parent: &Value,
        response_key: &'doc str,
        nodes: &'s [&'doc ast::Field<'doc>],
        path: &mut Path,
        errors: &mut Vec<GraphQLError>,
    ) -> Option<(Arc<str>, Result<Value, GraphQLError>)>
    where
        'doc: 's,
    {
        let first = nodes.first().copied()?;
        let field = self.leaf_field(object_type, first)?;

        // An unaliased field's response key is the schema field's interned name, so the
        // response object and the error path can share that `Arc`.
        let response_key: Arc<str> = match first.alias {
            Some(_) => Arc::from(response_key),
            None => field.name.clone(),
        };
        let location = first.location;
        path.push(PathSegment::Field(response_key.clone()));
        if self.expired() {
            let error = GraphQLError::new("The operation timed out before it could finish.")
                .with_path(path.to_vec());
            path.pop();
            return Some((response_key, Err(error)));
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
        path.pop();
        Some((response_key, outcome))
    }

    #[allow(clippy::too_many_arguments)]
    fn execute_field<'s>(
        &'s self,
        object_type: Arc<ObjectType>,
        parent: &'s Value,
        response_key: &'doc str,
        nodes: &'s [&'doc ast::Field<'doc>],
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
            let field = object_type.get_field(first.name);
            let response_key: Arc<str> = match (field, first.alias) {
                (Some(field), None) => field.name.clone(),
                _ => Arc::from(response_key),
            };
            path.push(PathSegment::Field(response_key.clone()));

            let outcome = self
                .execute_field_inner(
                    &object_type,
                    field,
                    parent,
                    first,
                    nodes,
                    path,
                    errors,
                    depth,
                )
                .await;
            path.pop();
            (response_key, outcome)
        }
        .boxed()
    }

    #[allow(clippy::too_many_arguments)]
    async fn execute_field_inner(
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
        let selection_sets: SmallVec<[&'doc ast::SelectionSet<'doc>; 2]> = nodes
            .iter()
            .filter_map(|node| node.selection_set.as_ref())
            .collect();

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
            let denial = self.context.authorizer.as_ref().and_then(|authorizer| {
                let authorization = AuthorizationContext {
                    context: self.context,
                    field_name: first.name,
                    parent_type: object_type.name.as_ref(),
                    return_type: &ty,
                    path: path.as_slice(),
                    args: &empty_args,
                    info: &info,
                };
                authorizer.authorize(&authorization).err()
            });
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
                                &selection_sets,
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
                                        &selection_sets,
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
            let error = GraphQLError::new(format!(
                "Cannot query field \"{}\" on type \"{}\".",
                first.name, object_type.name
            ))
            .at(location);
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
            let denial = self.context.authorizer.as_ref().and_then(|authorizer| {
                let authorization = AuthorizationContext {
                    context: self.context,
                    field_name: field.name.as_ref(),
                    parent_type: object_type.name.as_ref(),
                    return_type: &field.ty,
                    path: path.as_slice(),
                    args,
                    info: &info,
                };
                authorizer.authorize(&authorization).err()
            });
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
                        &selection_sets,
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
                                &selection_sets,
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
    fn plain_object(&self, ty: &TypeRef) -> Option<Arc<ObjectType>> {
        let mut current = ty;
        while let TypeRef::NonNull(inner) = current {
            current = inner;
        }
        let TypeRef::Named(name) = current else {
            return None;
        };
        match self.context.schema.get_type(name) {
            Some(Type::Object(object)) => Some(object.clone()),
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
        selection_sets: &[&'doc ast::SelectionSet<'doc>],
        location: Location,
        path: &mut Path,
        errors: &mut Vec<GraphQLError>,
        depth: usize,
    ) -> Result<Value, GraphQLError> {
        if !value.is_null() {
            if let Some(object) = self.plain_object(ty) {
                return self
                    .complete_object(
                        object,
                        value,
                        label,
                        selection_sets,
                        location,
                        path,
                        errors,
                        depth,
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
                        selection_sets,
                        location,
                        path,
                        errors,
                        depth,
                    )
                    .await;
            }
        }
        self.complete_value(
            value,
            ty,
            label,
            selection_sets,
            location,
            path,
            errors,
            depth,
        )
        .await
    }

    /// The object type a list's items complete as, when every item is a plain object.
    ///
    /// Returns the item type's non-null-ness too, because it decides whether a failed item nulls
    /// the whole list.
    fn object_list(&self, ty: &TypeRef) -> Option<(Arc<ObjectType>, bool)> {
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
            Some(Type::Object(object)) => Some((object.clone(), non_null)),
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
        object: Arc<ObjectType>,
        items_non_null: bool,
        value: &Value,
        label: FieldLabel<'_>,
        selection_sets: &[&'doc ast::SelectionSet<'doc>],
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
                    object.clone(),
                    item,
                    label,
                    selection_sets,
                    location,
                    path,
                    errors,
                    depth,
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
        selection_sets: &'s [&'doc ast::SelectionSet<'doc>],
        location: Location,
        path: &'s mut Path,
        errors: &'s mut Vec<GraphQLError>,
        depth: usize,
    ) -> BoxFuture<'s, Result<Value, GraphQLError>>
    where
        'doc: 's,
    {
        self.complete_value_inner(
            value,
            ty,
            label,
            selection_sets,
            location,
            path,
            errors,
            depth,
        )
        .boxed()
    }

    #[allow(clippy::too_many_arguments)]
    async fn complete_value_inner(
        &self,
        value: &Value,
        ty: &TypeRef,
        label: FieldLabel<'_>,
        selection_sets: &[&'doc ast::SelectionSet<'doc>],
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
                Err(located(
                    format!("Cannot return null for non-nullable field {label}."),
                    path,
                    location,
                ))
            } else {
                Ok(Value::Null)
            };
        }
        let completed = match current {
            TypeRef::List(inner) => {
                self.complete_list(
                    value,
                    inner,
                    label,
                    selection_sets,
                    location,
                    path,
                    errors,
                    depth,
                )
                .await?
            }
            TypeRef::Named(name) => {
                self.complete_named(
                    value,
                    name,
                    label,
                    selection_sets,
                    location,
                    path,
                    errors,
                    depth,
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
            return Err(located(
                format!("Cannot return null for non-nullable field {label}."),
                path,
                location,
            ));
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
        selection_sets: &[&'doc ast::SelectionSet<'doc>],
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
                Err(located(
                    format!("Cannot return null for non-nullable field {label}."),
                    path,
                    location,
                ))
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
                        item,
                        inner,
                        label,
                        selection_sets,
                        location,
                        path,
                        errors,
                        depth,
                    )
                    .await;
            }
            TypeRef::Named(name) => {
                self.complete_named(
                    item,
                    name,
                    label,
                    selection_sets,
                    location,
                    path,
                    errors,
                    depth,
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
        selection_sets: &[&'doc ast::SelectionSet<'doc>],
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
                        object.clone(),
                        item,
                        label,
                        selection_sets,
                        location,
                        path,
                        errors,
                        depth,
                    )
                    .await
                }
                _ => {
                    self.complete_item(
                        item,
                        inner,
                        label,
                        selection_sets,
                        location,
                        path,
                        errors,
                        depth,
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
        selection_sets: &[&'doc ast::SelectionSet<'doc>],
        location: Location,
        path: &mut Path,
        errors: &mut Vec<GraphQLError>,
        depth: usize,
    ) -> Result<Value, GraphQLError> {
        let Some(definition) = self.context.schema.get_type(name) else {
            return Err(located(
                format!("Type \"{name}\" used by field {label} is not defined by the schema."),
                path,
                location,
            ));
        };
        match definition {
            Type::Scalar(scalar) => {
                let serialized = if let Some(serialize) = scalar.serialize_fn() {
                    serialize(value).map_err(|message| {
                        located(
                            format!("Field {label} could not be serialized: {message}"),
                            path,
                            location,
                        )
                    })?
                } else {
                    value.clone()
                };
                if is_legal_scalar_value(&serialized) {
                    Ok(serialized)
                } else {
                    Err(located(
                        format!(
                            "Field {label} serialized to a {} which is not a valid scalar value.",
                            serialized.kind()
                        ),
                        path,
                        location,
                    ))
                }
            }
            Type::Enum(enumeration) => match &value {
                Value::Enum(value) | Value::String(value) if enumeration.has_value(value) => {
                    Ok(Value::Enum(value.clone()))
                }
                Value::Enum(value) | Value::String(value) => Err(located(
                    format!(
                        "Value \"{value}\" does not exist in enum \"{}\" for field {label}.",
                        enumeration.name
                    ),
                    path,
                    location,
                )),
                other => Err(located(
                    format!(
                        "Field {label} expects enum \"{}\" but received a {}.",
                        enumeration.name,
                        other.kind()
                    ),
                    path,
                    location,
                )),
            },
            Type::Object(object) => {
                let object = object.clone();
                self.complete_object(
                    object,
                    value,
                    label,
                    selection_sets,
                    location,
                    path,
                    errors,
                    depth,
                )
                .await
            }
            Type::Interface(_) | Type::Union(_) => {
                if !matches!(value, Value::Object(_) | Value::External(_)) {
                    return Err(located(
                        format!(
                            "Field {label} expects a {} but received a {}.",
                            name,
                            value.kind()
                        ),
                        path,
                        location,
                    ));
                }
                let object = self
                    .resolve_abstract_type(name.as_ref(), value)
                    .map_err(|error| locate(error, path, location))?;
                self.complete_object(
                    object,
                    value,
                    label,
                    selection_sets,
                    location,
                    path,
                    errors,
                    depth,
                )
                .await
            }
            Type::InputObject(_) => Err(located(
                format!("Field {label} has input object type \"{name}\" as its return type."),
                path,
                location,
            )),
        }
    }

    #[allow(clippy::too_many_arguments)]
    async fn complete_object(
        &self,
        object: Arc<ObjectType>,
        value: &Value,
        label: FieldLabel<'_>,
        selection_sets: &[&'doc ast::SelectionSet<'doc>],
        location: Location,
        path: &mut Path,
        errors: &mut Vec<GraphQLError>,
        depth: usize,
    ) -> Result<Value, GraphQLError> {
        if !matches!(value, Value::Object(_) | Value::External(_)) {
            return Err(located(
                format!(
                    "Field {label} expects an object of type \"{}\" but received a {}.",
                    object.name,
                    value.kind()
                ),
                path,
                location,
            ));
        }
        if selection_sets.is_empty() {
            return Err(located(
                format!(
                    "Field {label} of type \"{}\" must have a selection set.",
                    object.name
                ),
                path,
                location,
            ));
        }
        // The field that produced this value already tried the synchronous completion path (see
        // [`Self::try_complete_field_value_sync`]); reaching here means the subtree needs the
        // asynchronous path somewhere.
        let completed = self
            .execute_fields(object, value, selection_sets, path, errors, depth)
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
        object_type: &Arc<ObjectType>,
        parent: &Value,
        selection_sets: &[&'doc ast::SelectionSet<'doc>],
        path: &mut Path,
        errors: &mut Vec<GraphQLError>,
        depth: usize,
    ) -> Option<Result<Arc<Object>, GraphQLError>> {
        if depth >= SYNC_DEPTH_LIMIT
            || object_type
                .fields
                .values()
                .any(|field| field.resolve.is_some())
        {
            return None;
        }
        let collected = match collect::collect_fields_multi(
            &self.context.schema,
            &self.fragments,
            &self.context.variables,
            object_type.name.as_ref(),
            selection_sets,
        ) {
            Ok(collected) => collected,
            Err(error) => return Some(Err(error)),
        };

        let mut records = Vec::new();
        match self.execute_fields_sync(object_type, parent, &collected, path, &mut records, depth) {
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
        selection_sets: &[&'doc ast::SelectionSet<'doc>],
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
            selection_sets,
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
    fn execute_fields_sync(
        &self,
        object_type: &Arc<ObjectType>,
        parent: &Value,
        collected: &collect::CollectedFields<'doc>,
        path: &mut Path,
        errors: &mut Vec<GraphQLError>,
        depth: usize,
    ) -> Result<Result<Arc<Object>, ()>, GraphQLError> {
        // The response object is the one allocation it needs, so it is created up front and
        // filled in place; that saves copying the whole object into the `Arc` afterwards.
        let mut result = Arc::new(Object::with_capacity(collected.len()));
        let object = Arc::get_mut(&mut result).expect("a fresh Arc is uniquely owned");
        let mut index = 0;
        while index < collected.len() {
            let (response_key, nodes) = &collected[index];
            if self.expired() {
                return Err(
                    GraphQLError::new("The operation timed out before it could finish.")
                        .with_path(path.to_vec()),
                );
            }
            let Some((response_key, outcome)) = self.execute_field_sync(
                object_type,
                parent,
                response_key,
                nodes,
                path,
                errors,
                depth,
            ) else {
                return Ok(Err(()));
            };
            object.push_unchecked(response_key, outcome?);
            index += 1;
        }
        Ok(Ok(result))
    }

    #[allow(clippy::too_many_arguments)]
    fn execute_field_sync(
        &self,
        object_type: &Arc<ObjectType>,
        parent: &Value,
        response_key: &'doc str,
        nodes: &[&'doc ast::Field<'doc>],
        path: &mut Path,
        errors: &mut Vec<GraphQLError>,
        depth: usize,
    ) -> Option<(Arc<str>, Result<Value, GraphQLError>)> {
        let Some(first) = nodes.first().copied() else {
            return Some((
                Arc::from(response_key),
                Err(GraphQLError::new(format!(
                    "No field node was provided for response key \"{response_key}\"."
                ))),
            ));
        };
        // Meta fields have their own resolution rules, including introspection.
        if meta_return_type(first.name).is_some() {
            return None;
        }
        let field = object_type.get_field(first.name);
        let response_key: Arc<str> = match (field, first.alias) {
            (Some(field), None) => field.name.clone(),
            _ => Arc::from(response_key),
        };
        path.push(PathSegment::Field(response_key.clone()));
        let outcome = self.execute_field_inner_sync(
            object_type,
            field,
            parent,
            first,
            nodes,
            path,
            errors,
            depth,
        );
        path.pop();
        Some((response_key, outcome?))
    }

    #[allow(clippy::too_many_arguments)]
    fn execute_field_inner_sync(
        &self,
        object_type: &ObjectType,
        field: Option<&crate::types::Field>,
        parent: &Value,
        first: &ast::Field<'_>,
        nodes: &[&'doc ast::Field<'doc>],
        path: &mut Path,
        errors: &mut Vec<GraphQLError>,
        depth: usize,
    ) -> Option<Result<Value, GraphQLError>> {
        let location = first.location;
        let selection_sets: SmallVec<[&'doc ast::SelectionSet<'doc>; 2]> = nodes
            .iter()
            .filter_map(|node| node.selection_set.as_ref())
            .collect();

        if self.expired() {
            return Some(Err(GraphQLError::new(
                "The operation timed out before it could finish.",
            )
            .with_path(path.to_vec())
            .at(location)));
        }

        let Some(field) = field else {
            let error = GraphQLError::new(format!(
                "Cannot query field \"{}\" on type \"{}\".",
                first.name, object_type.name
            ))
            .at(location);
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
        // arguments reports exactly the errors the async path would. An empty argument list cannot
        // fail, so the common case skips building an object nothing reads.
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

        // The schema resolved this field's output type when it was built, so completing the field
        // does not have to look the type up by name.
        let definition = field
            .type_index
            .and_then(|index| self.context.schema.type_at(index));
        let outcome = match default_property(label, field.name.as_ref(), parent).map(property_value)
        {
            Ok(value) => {
                // `?` on the typed path only propagates "needs the asynchronous path", which
                // abandons the attempt. The completion result itself is returned to `finish_field`
                // below so null propagation and error recording stay identical to the async path.
                self.complete_field_value_typed_sync(
                    &value,
                    &field.ty,
                    definition,
                    label,
                    &selection_sets,
                    location,
                    path,
                    errors,
                    depth,
                )?
            }
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
        definition: Option<&Type>,
        label: FieldLabel<'_>,
        selection_sets: &[&'doc ast::SelectionSet<'doc>],
        location: Location,
        path: &mut Path,
        errors: &mut Vec<GraphQLError>,
        depth: usize,
    ) -> Option<Result<Value, GraphQLError>> {
        let Some(definition) = definition else {
            return self.complete_field_value_sync(
                value,
                ty,
                label,
                selection_sets,
                location,
                path,
                errors,
                depth,
            );
        };
        let mut current = ty;
        let mut non_null = false;
        while let TypeRef::NonNull(inner) = current {
            non_null = true;
            current = inner;
        }
        if value.is_null() {
            return self.complete_value_sync(
                value,
                ty,
                label,
                selection_sets,
                location,
                path,
                errors,
                depth,
            );
        }
        match (definition, current) {
            (Type::Scalar(_) | Type::Enum(_), TypeRef::Named(_)) => self
                .complete_leaf_definition(value, definition, non_null, label, location, path, true),
            (Type::Object(object), TypeRef::Named(_)) => self.complete_object_sync(
                object,
                value,
                label,
                selection_sets,
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
                        value,
                        ty,
                        label,
                        selection_sets,
                        location,
                        path,
                        errors,
                        depth,
                    );
                }
                self.complete_object_list_sync(
                    object,
                    items_non_null,
                    value,
                    label,
                    selection_sets,
                    location,
                    path,
                    errors,
                    depth + 1,
                )
            }
            _ => self.complete_value_sync(
                value,
                ty,
                label,
                selection_sets,
                location,
                path,
                errors,
                depth,
            ),
        }
    }

    /// [`Self::complete_field_value`] without awaiting.
    #[allow(clippy::too_many_arguments)]
    fn complete_field_value_sync(
        &self,
        value: &Value,
        ty: &TypeRef,
        label: FieldLabel<'_>,
        selection_sets: &[&'doc ast::SelectionSet<'doc>],
        location: Location,
        path: &mut Path,
        errors: &mut Vec<GraphQLError>,
        depth: usize,
    ) -> Option<Result<Value, GraphQLError>> {
        if !value.is_null() {
            if let Some(object) = self.plain_object(ty) {
                return self.complete_object_sync(
                    &object,
                    value,
                    label,
                    selection_sets,
                    location,
                    path,
                    errors,
                    depth + 1,
                );
            }
            if let Some((object, items_non_null)) = self.object_list(ty) {
                return self.complete_object_list_sync(
                    &object,
                    items_non_null,
                    value,
                    label,
                    selection_sets,
                    location,
                    path,
                    errors,
                    depth + 1,
                );
            }
        }
        self.complete_value_sync(
            value,
            ty,
            label,
            selection_sets,
            location,
            path,
            errors,
            depth,
        )
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
        selection_sets: &[&'doc ast::SelectionSet<'doc>],
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
                value,
                inner,
                label,
                selection_sets,
                location,
                path,
                errors,
                depth,
            ) {
                Some(Ok(value)) => value,
                Some(Err(error)) => return Some(Err(error)),
                None => return None,
            },
            TypeRef::Named(name) => match self.complete_named_sync(
                value,
                name,
                label,
                selection_sets,
                location,
                path,
                errors,
                depth,
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
        object: &Arc<ObjectType>,
        items_non_null: bool,
        value: &Value,
        label: FieldLabel<'_>,
        selection_sets: &[&'doc ast::SelectionSet<'doc>],
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
        // Every item completes against the same runtime type and selection sets, so the fields are
        // collected once for the whole list. A missing selection set keeps the per-item path,
        // which reports exactly the error (or null item) it reported before; a collection failure
        // (only reachable for a document that was never validated) is reported once for the field
        // rather than once per item.
        let collected = if selection_sets.is_empty() {
            None
        } else {
            match self.collect_object_sync(object, label, selection_sets, location, path, depth)? {
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
                        object,
                        item,
                        label,
                        selection_sets,
                        location,
                        path,
                        errors,
                        depth,
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
        object: &Arc<ObjectType>,
        label: FieldLabel<'_>,
        selection_sets: &[&'doc ast::SelectionSet<'doc>],
        location: Location,
        path: &mut Path,
        depth: usize,
    ) -> Option<Result<collect::CollectedFields<'doc>, GraphQLError>> {
        if depth >= SYNC_DEPTH_LIMIT || object.fields.values().any(|field| field.resolve.is_some())
        {
            return None;
        }
        if selection_sets.is_empty() {
            return Some(Err(located(
                format!(
                    "Field {label} of type \"{}\" must have a selection set.",
                    object.name
                ),
                path,
                location,
            )));
        }
        Some(collect::collect_fields_multi(
            &self.context.schema,
            &self.fragments,
            &self.context.variables,
            object.name.as_ref(),
            selection_sets,
        ))
    }

    /// [`Self::complete_object`] without awaiting.
    ///
    /// `depth` is the level of the object's own selection set.
    #[allow(clippy::too_many_arguments)]
    fn complete_object_sync(
        &self,
        object: &Arc<ObjectType>,
        value: &Value,
        label: FieldLabel<'_>,
        selection_sets: &[&'doc ast::SelectionSet<'doc>],
        location: Location,
        path: &mut Path,
        errors: &mut Vec<GraphQLError>,
        depth: usize,
    ) -> Option<Result<Value, GraphQLError>> {
        let collected =
            match self.collect_object_sync(object, label, selection_sets, location, path, depth)? {
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
        object: &Arc<ObjectType>,
        value: &Value,
        label: FieldLabel<'_>,
        collected: &collect::CollectedFields<'doc>,
        location: Location,
        path: &mut Path,
        errors: &mut Vec<GraphQLError>,
        depth: usize,
    ) -> Option<Result<Value, GraphQLError>> {
        if !matches!(value, Value::Object(_) | Value::External(_)) {
            return Some(Err(located(
                format!(
                    "Field {label} expects an object of type \"{}\" but received a {}.",
                    object.name,
                    value.kind()
                ),
                path,
                location,
            )));
        }
        match self.execute_fields_sync(object, value, collected, path, errors, depth) {
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
        selection_sets: &[&'doc ast::SelectionSet<'doc>],
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
                    selection_sets,
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
                    item,
                    inner,
                    label,
                    selection_sets,
                    location,
                    path,
                    errors,
                    depth,
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
        selection_sets: &[&'doc ast::SelectionSet<'doc>],
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
            TypeRef::Named(name) => match self.complete_named_sync(
                item,
                name,
                label,
                selection_sets,
                location,
                path,
                errors,
                depth,
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

    /// [`Self::complete_named`] without awaiting.
    #[allow(clippy::too_many_arguments)]
    fn complete_named_sync(
        &self,
        value: &Value,
        name: &crate::types::Name,
        label: FieldLabel<'_>,
        selection_sets: &[&'doc ast::SelectionSet<'doc>],
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
                selection_sets,
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
    ) -> Result<Arc<ObjectType>, GraphQLError> {
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
            object
                .get(introspection::TYPENAME_KEY)
                .or_else(|| object.get("__typename"))
                .and_then(Value::as_str)
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
            .cloned()
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
            let typename = match parent {
                Value::Object(object) => object
                    .get(introspection::TYPENAME_KEY)
                    .and_then(Value::as_str)
                    .map(Arc::from)
                    .unwrap_or_else(|| object_type.name.clone()),
                _ => object_type.name.clone(),
            };
            return Ok(Value::String(typename));
        }

        Err(unknown_meta_field(node, object_type))
    }
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
        Value::Null | Value::Boolean(_) | Value::Int(_) | Value::String(_) | Value::Enum(_) => true,
        // JSON has no representation for NaN or infinity, and the GraphQL `Float` scalar is
        // defined as a finite IEEE-754 double.
        Value::Float(value) => value.is_finite(),
        Value::List(_) | Value::Object(_) | Value::External(_) => false,
    }
}
