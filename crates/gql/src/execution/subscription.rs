//! Subscription execution.
//!
//! A subscription is set up like a query: the document is parsed, validated, an operation is
//! selected and its variables are coerced. The single root field's `subscribe` resolver then
//! produces a source stream. Per October 2021 6.2.3.2 each source event is the operation's root
//! value: event execution re-runs the subscription selection set with the event as the parent, so
//! the root field's `resolve` resolver (or the default property resolver) runs for every event.

use std::{fmt, sync::Arc, time::Instant};

use futures::{StreamExt, stream::BoxStream};

use crate::{
    Object, Value,
    complexity::{self, ComplexityOptions},
    error::{GraphQLError, Location, PathSegment, Response},
    parser::parse_query,
    schema::Schema,
    types::{ResolveInfo, ResolverContext, TypeRef},
    validation::{self, ValidationOptions},
};

use super::{
    AuthorizationContext, ExecutionContext, ExecutionOptions, Request, coerce, collect, executor,
};

/// A subscription's event stream.
pub struct SubscriptionResponse {
    /// Responses, one per source event.
    pub stream: BoxStream<'static, Response>,
}

impl fmt::Debug for SubscriptionResponse {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("SubscriptionResponse")
            .field("stream", &"<stream>")
            .finish()
    }
}

/// Start a subscription.
pub(crate) async fn subscribe(
    schema: &Schema,
    request: Request<'_>,
    options: &ExecutionOptions,
) -> Result<SubscriptionResponse, Response> {
    let document = parse_query(request.query).map_err(Response::error)?;

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
        return Err(Response {
            data: None,
            errors: validation_errors,
            extensions: None,
        });
    }

    let operation =
        executor::select_operation(&document, request.operation_name).map_err(Response::error)?;
    if operation.kind != crate::ast::OperationKind::Subscription {
        return Err(Response::error(GraphQLError::new(
            "The selected operation is not a subscription; use execute for queries and mutations.",
        )));
    }

    let variables =
        coerce::coerce_variable_values(schema, operation, request.variables).map_err(|errors| {
            Response {
                data: None,
                errors,
                extensions: None,
            }
        })?;

    complexity::analyze(
        schema,
        &document,
        operation.name,
        &variables.values,
        &ComplexityOptions {
            list_factor: options.list_complexity,
            field_cost: 1,
            max_complexity: options.max_complexity,
            max_depth: options.max_depth,
            max_breadth: options.max_breadth,
        },
    )
    .map_err(Response::error)?;

    let Some(subscription_type) = schema.subscription_type() else {
        return Err(Response::error(GraphQLError::new(
            "Schema is not configured for subscriptions.",
        )));
    };
    let subscription_type = subscription_type.clone();

    let context = ExecutionContext {
        schema: schema.clone(),
        user_data: options.context.clone(),
        operation_name: operation.name.map(Arc::from),
        variables: variables.values,
        introspection: options.introspection,
        authorizer: options.authorizer.clone(),
        max_errors: options.max_errors,
        max_concurrency: options.max_concurrency,
        extensions: std::sync::Mutex::new(None),
    };

    let fragments = executor::FragmentMap::new(&document);
    let collected = collect::collect_fields(
        schema,
        &fragments,
        &context.variables,
        &subscription_type,
        &operation.selection_set,
    )
    .map_err(Response::error)?;

    if collected.is_empty() {
        return Err(Response::error(GraphQLError::new(
            "A subscription selected no root fields; it must select exactly one.",
        )));
    }
    if collected.len() != 1 {
        return Err(Response::error(GraphQLError::new(
            "A subscription must select exactly one root field.",
        )));
    }
    let Some((response_key, nodes, _)) = collected.first() else {
        return Err(Response::error(GraphQLError::new(
            "A subscription selected no root fields; it must select exactly one.",
        )));
    };
    let Some(node) = nodes.first().copied() else {
        return Err(Response::error(GraphQLError::new(
            "A subscription root field has no field node.",
        )));
    };
    if node.name.starts_with("__") {
        return Err(Response::error(GraphQLError::new(format!(
            "Subscription root field \"{}\" must not be an introspection field.",
            node.name
        ))));
    }

    let Some(field) = subscription_type.get_field(node.name) else {
        return Err(Response::error(
            GraphQLError::new(format!(
                "Cannot query field \"{}\" on type \"{}\".",
                node.name, subscription_type.name
            ))
            .at(node.location),
        ));
    };
    let Some(subscribe) = field.subscribe.clone() else {
        return Err(Response::error(
            GraphQLError::new(format!(
                "Subscription field \"{}.{}\" does not define a subscribe resolver.",
                subscription_type.name, field.name
            ))
            .at(node.location),
        ));
    };

    let args = coerce::coerce_argument_values(schema, field, node, &context.variables)
        .map_err(Response::error)?;

    let path = vec![PathSegment::field(*response_key)];
    let info = ResolveInfo {
        field_name: field.name.as_ref(),
        parent_type: subscription_type.name.as_ref(),
        return_type: &field.ty,
        path: &path,
        schema,
    };

    if let Some(authorizer) = &context.authorizer {
        let authorization = AuthorizationContext {
            context: &context,
            field_name: field.name.as_ref(),
            parent_type: subscription_type.name.as_ref(),
            return_type: &field.ty,
            path: &path,
            args: &args,
            info: &info,
        };
        if let Err(error) = authorizer.authorize_async(&authorization).await {
            return Err(Response::error(error));
        }
    }

    let source = subscribe(ResolverContext {
        parent: &options.root_value,
        args: &args,
        context: &context,
        info: &info,
    })
    .await
    .map_err(Response::error)?;

    let field_ty = field.ty.clone();
    let location = node.location;
    let response_key: Arc<str> = Arc::from(*response_key);
    let operation_name: Option<Arc<str>> = operation.name.map(Arc::from);
    let query: Arc<str> = Arc::from(request.query);
    // Each event gets its own deadline: a subscription is long-lived, so one absolute deadline
    // would kill the stream after the first timeout window.
    let timeout = options.timeout;
    let context = Arc::new(context);

    let stream = source
        .then(move |event| {
            let context = context.clone();
            let query = query.clone();
            let operation_name = operation_name.clone();
            let field_ty = field_ty.clone();
            let response_key = response_key.clone();
            async move {
                let deadline = timeout.map(|timeout| Instant::now() + timeout);
                match event {
                    Ok(value) => {
                        executor::execute_subscription_event(
                            &context,
                            &query,
                            operation_name.as_deref(),
                            value,
                            deadline,
                        )
                        .await
                    }
                    Err(error) => {
                        event_error_response(error, &response_key, &field_ty, location, &context)
                    }
                }
            }
        })
        .boxed();

    Ok(SubscriptionResponse { stream })
}

/// Shape a failed source event according to the root field's nullability.
fn event_error_response(
    error: GraphQLError,
    response_key: &str,
    field_ty: &TypeRef,
    location: Location,
    context: &ExecutionContext,
) -> Response {
    let error = locate(error, response_key, location);

    let data = if field_ty.is_non_null() {
        Some(Value::Null)
    } else {
        let mut object = Object::new();
        object.insert(response_key, Value::Null);
        Some(Value::Object(Arc::new(object)))
    };

    Response {
        data,
        errors: vec![error],
        extensions: super::executor::take_extensions(context),
    }
}

fn locate(mut error: GraphQLError, response_key: &str, location: Location) -> GraphQLError {
    if error.path().is_none() {
        error.set_path(vec![PathSegment::field(response_key)]);
    }
    if error.locations().is_empty() {
        error = error.at(location);
    }
    error
}
