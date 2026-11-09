//! Execution engine.
//!
//! Implements the October 2021 specification's `ExecuteRequest`, `ExecuteSelectionSet` and
//! `CompleteValue` algorithms. Field completion returns an error that has *not* been recorded yet:
//! the nearest nullable position decides whether to record it (and produce `null`) or let it
//! bubble through a non-null position. That is exactly the specification's null propagation rule,
//! and it keeps each error reported once, with the path where it originated.

use std::{
    collections::HashMap,
    sync::{Arc, Mutex},
    time::Instant,
};

use futures::{
    StreamExt,
    future::{BoxFuture, FutureExt},
    stream,
};

use crate::{
    Object, Value,
    ast::{self, OperationKind},
    complexity::{self, ComplexityOptions},
    error::{GraphQLError, Location, PathSegment, Response},
    introspection,
    parser::parse_query,
    schema::Schema,
    types::{ObjectType, ResolveInfo, ResolverContext, Type, TypeRef},
    validation::{self, ValidationOptions},
};

use super::{AuthorizationContext, ExecutionContext, ExecutionOptions, Request, coerce, collect};

/// A map of fragment definitions keyed by name, borrowed from the parsed document.
pub(crate) type FragmentMap<'a> = HashMap<&'a str, &'a ast::FragmentDefinition<'a>>;

/// Build a fragment lookup for a parsed document.
pub(crate) fn fragment_map<'a>(document: &'a ast::Document<'a>) -> FragmentMap<'a> {
    document
        .fragments()
        .map(|fragment| (fragment.name, fragment))
        .collect()
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

    let fragments = fragment_map(&document);
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
    let operations: Vec<&ast::OperationDefinition<'_>> = document.operations().collect();
    if operations.is_empty() {
        return Err(GraphQLError::new("Must provide an operation."));
    }
    match operation_name {
        Some(name) => operations
            .into_iter()
            .find(|operation| operation.name == Some(name))
            .ok_or_else(|| GraphQLError::new(format!("Unknown operation named \"{name}\"."))),
        None => {
            if operations.len() > 1 {
                Err(GraphQLError::new(
                    "Must provide an operation name if the document contains multiple operations.",
                ))
            } else {
                operations
                    .first()
                    .copied()
                    .ok_or_else(|| GraphQLError::new("Must provide an operation."))
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
    let mut path = Vec::new();
    let mut errors = Vec::new();
    let result = executor
        .execute_fields(
            root_type,
            &parent,
            std::slice::from_ref(&selection_set),
            &mut path,
            &mut errors,
        )
        .await;

    let data = match result {
        Ok(object) => Some(Value::Object(Arc::new(object))),
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
    let fragments = fragment_map(&document);

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

    let label: Arc<str> = Arc::from(format!("{}.{}", root_type.name, first.name));
    let mut path = vec![PathSegment::field(response_key)];
    let mut errors = Vec::new();
    let executor = Executor {
        context,
        fragments,
        deadline,
        parallel: true,
    };
    let outcome = executor
        .complete_value(
            event,
            field_ty.clone(),
            label,
            &selection_sets,
            first.location,
            &mut path,
            &mut errors,
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
        path: &'s mut Vec<PathSegment>,
        errors: &'s mut Vec<GraphQLError>,
    ) -> BoxFuture<'s, Result<Object, GraphQLError>>
    where
        'doc: 's,
    {
        async move {
            let collected = collect::collect_fields_multi(
                &self.context.schema,
                &self.fragments,
                &self.context.variables,
                object_type.name.as_ref(),
                selection_sets,
            )?;

            let mut result = Object::with_capacity(collected.len());
            if self.parallel && collected.len() > 1 {
                type Outcome<'a, 'doc> =
                    BoxFuture<'a, (&'doc str, Result<Value, GraphQLError>, Vec<GraphQLError>)>;

                let mut futures: Vec<Outcome<'s, 'doc>> = Vec::with_capacity(collected.len());
                for (response_key, nodes) in collected {
                    if self.expired() {
                        return Err(GraphQLError::new(
                            "The operation timed out before it could finish.",
                        )
                        .with_path(path.clone()));
                    }
                    let mut field_path = path.clone();
                    field_path.push(PathSegment::field(response_key));
                    let object_type = object_type.clone();
                    futures.push(
                        async move {
                            let mut field_errors = Vec::new();
                            let outcome = self
                                .execute_field(
                                    object_type.clone(),
                                    parent,
                                    response_key,
                                    &nodes,
                                    &mut field_path,
                                    &mut field_errors,
                                )
                                .await;
                            (response_key, outcome, field_errors)
                        }
                        .boxed(),
                    );
                }

                let outcomes: Vec<_> = if self.context.max_concurrency == 0 {
                    futures::future::join_all(futures).await
                } else {
                    stream::iter(futures)
                        .buffered(self.context.max_concurrency.max(1))
                        .collect()
                        .await
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
            } else {
                for (response_key, nodes) in collected {
                    // Mutations must keep document order, and a single field has no concurrency
                    // to gain.
                    if self.expired() {
                        return Err(GraphQLError::new(
                            "The operation timed out before it could finish.",
                        )
                        .with_path(path.clone()));
                    }
                    path.push(PathSegment::field(response_key));
                    let outcome = self
                        .execute_field(
                            object_type.clone(),
                            parent,
                            response_key,
                            &nodes,
                            path,
                            errors,
                        )
                        .await;
                    path.pop();
                    match outcome {
                        Ok(value) => {
                            result.push_unchecked(response_key, value);
                        }
                        Err(error) => return Err(error),
                    }
                }
            }
            Ok(result)
        }
        .boxed()
    }

    fn execute_field<'s>(
        &'s self,
        object_type: Arc<ObjectType>,
        parent: &'s Value,
        response_key: &'doc str,
        nodes: &'s [&'doc ast::Field<'doc>],
        path: &'s mut Vec<PathSegment>,
        errors: &'s mut Vec<GraphQLError>,
    ) -> BoxFuture<'s, Result<Value, GraphQLError>>
    where
        'doc: 's,
    {
        async move {
            let Some(first) = nodes.first().copied() else {
                return Err(GraphQLError::new(format!(
                    "No field node was provided for response key \"{response_key}\"."
                )));
            };
            let location = first.location;
            let selection_sets: Vec<&'doc ast::SelectionSet<'doc>> = nodes
                .iter()
                .filter_map(|node| (*node).selection_set.as_ref())
                .collect();

            if self.expired() {
                return Err(
                    GraphQLError::new("The operation timed out before it could finish.")
                        .with_path(path.clone())
                        .at(location),
                );
            }

            // Meta fields are resolved before normal field lookup because they never appear in
            // the schema's type map.
            if let Some(ty) = meta_return_type(first.name) {
                let label: Arc<str> = Arc::from(format!("{}.{}", object_type.name, first.name));
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
                    None => match self.resolve_meta(object_type.as_ref(), parent, first) {
                        Ok(value) => {
                            self.complete_value(
                                value,
                                ty.clone(),
                                label,
                                &selection_sets,
                                location,
                                path,
                                errors,
                            )
                            .await
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

            let Some(field) = object_type.get_field(first.name) else {
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

            let label: Arc<str> = Arc::from(format!("{}.{}", object_type.name, field.name));
            let args = match coerce::coerce_argument_values(
                &self.context.schema,
                field,
                first,
                &self.context.variables,
            ) {
                Ok(args) => args,
                Err(error) => {
                    return finish_field(
                        Err(error),
                        Some(&field.ty),
                        path,
                        location,
                        errors,
                        self.context.max_errors,
                    );
                }
            };

            // The authorizer and the resolver both see the same `ResolveInfo`; the borrow of
            // `path` ends before completion needs it mutably again.
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
                        args: &args,
                        info: &info,
                    };
                    authorizer.authorize(&authorization).err()
                });
                if let Some(error) = denial {
                    Err(error)
                } else {
                    let resolver_context = ResolverContext {
                        parent,
                        args: &args,
                        context: self.context,
                        info: &info,
                    };
                    match &field.resolve {
                        Some(resolve) => resolve(resolver_context).await,
                        None => default_resolve(label.as_ref(), field.name.as_ref(), parent),
                    }
                }
            };

            let outcome = match resolved {
                Ok(value) => {
                    self.complete_value(
                        value,
                        field.ty.clone(),
                        label,
                        &selection_sets,
                        location,
                        path,
                        errors,
                    )
                    .await
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
        .boxed()
    }

    fn complete_value<'s>(
        &'s self,
        value: Value,
        ty: TypeRef,
        label: Arc<str>,
        selection_sets: &'s [&'doc ast::SelectionSet<'doc>],
        location: Location,
        path: &'s mut Vec<PathSegment>,
        errors: &'s mut Vec<GraphQLError>,
    ) -> BoxFuture<'s, Result<Value, GraphQLError>>
    where
        'doc: 's,
    {
        async move {
            match ty {
                TypeRef::NonNull(inner) => {
                    if value.is_null() {
                        return Err(located(
                            format!("Cannot return null for non-nullable field {label}."),
                            path,
                            location,
                        ));
                    }
                    let completed = self
                        .complete_value(
                            value,
                            (*inner).clone(),
                            label.clone(),
                            selection_sets,
                            location,
                            path,
                            errors,
                        )
                        .await?;
                    if completed.is_null() {
                        return Err(located(
                            format!("Cannot return null for non-nullable field {label}."),
                            path,
                            location,
                        ));
                    }
                    Ok(completed)
                }
                other => {
                    if value.is_null() {
                        return Ok(Value::Null);
                    }
                    match other {
                        TypeRef::List(inner) => {
                            self.complete_list(
                                value,
                                *inner,
                                label,
                                selection_sets,
                                location,
                                path,
                                errors,
                            )
                            .await
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
                            )
                            .await
                        }
                        TypeRef::NonNull(_) => Err(located(
                            "Internal execution error: non-null type was not unwrapped.",
                            path,
                            location,
                        )),
                    }
                }
            }
        }
        .boxed()
    }

    async fn complete_list<'s>(
        &'s self,
        value: Value,
        inner: TypeRef,
        label: Arc<str>,
        selection_sets: &'s [&'doc ast::SelectionSet<'doc>],
        location: Location,
        path: &'s mut Vec<PathSegment>,
        errors: &'s mut Vec<GraphQLError>,
    ) -> Result<Value, GraphQLError>
    where
        'doc: 's,
    {
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
        for (index, item) in items.into_iter().enumerate() {
            path.push(PathSegment::index(index));
            let outcome = self
                .complete_value(
                    item,
                    inner.clone(),
                    label.clone(),
                    selection_sets,
                    location,
                    path,
                    errors,
                )
                .await;
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

    async fn complete_named<'s>(
        &'s self,
        value: Value,
        name: crate::types::Name,
        label: Arc<str>,
        selection_sets: &'s [&'doc ast::SelectionSet<'doc>],
        location: Location,
        path: &'s mut Vec<PathSegment>,
        errors: &'s mut Vec<GraphQLError>,
    ) -> Result<Value, GraphQLError>
    where
        'doc: 's,
    {
        let Some(definition) = self.context.schema.get_type(&name) else {
            return Err(located(
                format!("Type \"{name}\" used by field {label} is not defined by the schema."),
                path,
                location,
            ));
        };
        match definition {
            Type::Scalar(scalar) => {
                let scalar = scalar.clone();
                let serialized = if let Some(serialize) = scalar.serialize_fn() {
                    serialize(&value).map_err(|message| {
                        located(
                            format!("Field {label} could not be serialized: {message}"),
                            path,
                            location,
                        )
                    })?
                } else {
                    value
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
                self.complete_object(object, value, label, selection_sets, location, path, errors)
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
                    .resolve_abstract_type(name.as_ref(), &value)
                    .map_err(|error| locate(error, path, location))?;
                self.complete_object(object, value, label, selection_sets, location, path, errors)
                    .await
            }
            Type::InputObject(_) => Err(located(
                format!("Field {label} has input object type \"{name}\" as its return type."),
                path,
                location,
            )),
        }
    }

    async fn complete_object<'s>(
        &'s self,
        object: Arc<ObjectType>,
        value: Value,
        label: Arc<str>,
        selection_sets: &'s [&'doc ast::SelectionSet<'doc>],
        location: Location,
        path: &'s mut Vec<PathSegment>,
        errors: &'s mut Vec<GraphQLError>,
    ) -> Result<Value, GraphQLError>
    where
        'doc: 's,
    {
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
        let completed = self
            .execute_fields(object, &value, selection_sets, path, errors)
            .await?;
        Ok(Value::Object(Arc::new(completed)))
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

fn unknown_meta_field(node: &ast::Field<'_>, object_type: &ObjectType) -> GraphQLError {
    GraphQLError::new(format!(
        "Cannot query field \"{}\" on type \"{}\".",
        node.name, object_type.name
    ))
    .at(node.location)
}

fn meta_return_type(name: &str) -> Option<TypeRef> {
    match name {
        "__typename" => Some(TypeRef::string().non_null()),
        "__schema" => Some(TypeRef::named("__Schema").non_null()),
        "__type" => Some(TypeRef::named("__Type")),
        _ => None,
    }
}

fn default_resolve(label: &str, field_name: &str, parent: &Value) -> Result<Value, GraphQLError> {
    match parent {
        Value::Object(object) => Ok(object.get(field_name).cloned().unwrap_or(Value::Null)),
        Value::Null => Ok(Value::Null),
        Value::External(_) => Err(GraphQLError::new(format!(
            "No resolver is defined for field \"{label}\"; the parent is an opaque model value, so the default property resolver cannot read it."
        ))),
        other => Err(GraphQLError::new(format!(
            "No resolver is defined for field \"{label}\"; the default resolver requires an object parent, but the parent is a {}.",
            other.kind()
        ))),
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
