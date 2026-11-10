//! Execution of documents that were prepared ahead of time.
//!
//! The public [`crate::prepared`] module owns the self-referential cell and the prepared-query
//! API. This module only contains the pieces the executor needs: the dependent document type and
//! the two entry points that replace parsing/validation with a borrow of an already parsed
//! document.

use std::{sync::Arc, time::Instant};

use crate::{
    Value,
    ast::{self, OperationKind},
    complexity::{self, StaticComplexity},
    error::{GraphQLError, Response},
    schema::Schema,
    types::ObjectType,
};

use super::{
    ExecutionContext, ExecutionOptions, coerce, executor,
    plan::{self, PreparedPlan},
    subscription::{EventHandler, subscribe_document},
};

/// A parsed document plus the index of the operation selected when it was prepared.
///
/// This is the dependent half of the self-referential cell in [`crate::prepared`]. It borrows the
/// source text owned by the cell's owner and is therefore never exposed outside the crate.
pub(crate) struct PreparedDocument<'a> {
    /// The parsed document, borrowing the owner's source text.
    pub(crate) document: ast::Document<'a>,
    /// Position of the selected operation among `document.definitions`.
    pub(crate) operation_index: usize,
    /// The errors normal validation would report for introspection fields when introspection is
    /// disabled, in discovery order. Empty when the document does not use `__schema` or `__type`.
    pub(crate) introspection_errors: Vec<GraphQLError>,
    /// A conservative upper bound on the selected operation's cost, depth and breadth, computed
    /// once because the document is fixed. `None` when no bound can be proven, in which case every
    /// request runs the exact analysis.
    pub(crate) static_complexity: Option<StaticComplexity>,
    /// The selected operation's variable definitions, compiled once. `None` when they could not be
    /// compiled, in which case the general per-request coercion path is used.
    pub(crate) variables: Option<coerce::CompiledVariables>,
    /// The precompiled field collection plan for the selected operation.
    pub(crate) plan: PreparedPlan<'a>,
}

impl<'a> PreparedDocument<'a> {
    /// The operation selected during preparation.
    ///
    /// The index always points at an operation: it is produced by scanning for one during
    /// preparation. Returning an option keeps this type free of panics without exposing the
    /// internal invariant to the public API.
    pub(crate) fn operation(&self) -> Option<&ast::OperationDefinition<'a>> {
        match self.document.definitions.get(self.operation_index) {
            Some(ast::Definition::Operation(operation)) => Some(operation),
            _ => None,
        }
    }
}

/// Execute a prepared document through the normal execution tail.
pub(crate) async fn execute<'a>(
    schema: &'a Schema,
    prepared: &'a PreparedDocument<'a>,
    operation_name: Option<Arc<str>>,
    root_type: &'a ObjectType,
    request_variables: Option<&crate::Object>,
    options: &ExecutionOptions,
) -> Response {
    if !options.introspection && !prepared.introspection_errors.is_empty() {
        return introspection_rejection(&prepared.introspection_errors, options.max_errors);
    }
    let Some(operation) = prepared.operation() else {
        return Response::error(GraphQLError::new(
            "Internal error: the prepared query has no selected operation.",
        ));
    };
    executor::execute_prepared_operation(
        schema,
        &prepared.document,
        operation,
        operation_name,
        &prepared.plan,
        root_type,
        prepared.static_complexity.as_ref(),
        prepared.variables.as_ref(),
        request_variables,
        options,
    )
    .await
}

/// Execute one subscription event against the prepared document.
pub(crate) async fn execute_event<'a>(
    context: &'a ExecutionContext,
    prepared: &'a PreparedDocument<'a>,
    event: Value,
    deadline: Option<Instant>,
) -> Response {
    let Some(operation) = prepared.operation() else {
        return Response::error(GraphQLError::new(
            "Internal error: the prepared query has no selected operation.",
        ));
    };
    let Some(root_type) = context.schema.subscription_type() else {
        return Response::error(GraphQLError::new(
            "Schema is not configured for subscriptions.",
        ));
    };
    let fragments = executor::FragmentMap::new(&prepared.document);
    executor::run_operation(
        context,
        fragments,
        Some(&prepared.plan),
        root_type,
        &operation.selection_set,
        event,
        deadline,
        true,
        true,
    )
    .await
}

/// An error produced while preparing a document.
pub(crate) enum BuildPreparedError {
    /// The source could not be parsed.
    Parse(GraphQLError),
    /// The document parsed but failed validation.
    Validation(Vec<GraphQLError>),
    /// The document is valid but has no usable selected operation.
    Operation(GraphQLError),
}

/// Prepare the document for execution against a schema.
///
/// Parsing, validation and operation selection happen here, once. The operation index and the
/// introspection compatibility errors are stored alongside the document so per-request execution
/// only has to borrow them.
pub(crate) fn build<'a>(
    schema: &Schema,
    source: &'a str,
    operation_name: Option<&str>,
) -> Result<PreparedDocument<'a>, BuildPreparedError> {
    use crate::validation::{self, ValidationOptions};

    let document = match crate::parser::parse_query(source) {
        Ok(document) => document,
        Err(error) => return Err(BuildPreparedError::Parse(error)),
    };

    // Preparation is schema-time work, so it validates with introspection enabled. Whether a
    // particular execution may use introspection is an execution option; the exact error normal
    // validation would report is recorded here so a prepared query can reproduce it without
    // re-validating.
    let validation_errors = validation::validate(
        schema,
        &document,
        &ValidationOptions {
            introspection: true,
            max_depth: 0,
            max_errors: 100,
        },
    );
    if !validation_errors.is_empty() {
        return Err(BuildPreparedError::Validation(validation_errors));
    }

    let operation_index = match select_operation_index(&document, operation_name) {
        Ok(index) => index,
        Err(error) => return Err(BuildPreparedError::Operation(error)),
    };

    // The document's expansion cannot change between requests, so derive the conservative
    // complexity bound once and store it next to the document.
    let static_complexity = match document.definitions.get(operation_index) {
        Some(ast::Definition::Operation(operation)) => {
            complexity::static_bound(schema, &document, operation)
        }
        _ => None,
    };
    let variables = match document.definitions.get(operation_index) {
        Some(ast::Definition::Operation(operation)) => {
            coerce::CompiledVariables::compile(schema, operation).ok()
        }
        _ => None,
    };

    // The document is fixed, so the field collection it implies (fragment expansion, directive
    // conditions, merged selections and resolved schema fields) is computed once here. Anything
    // the plan cannot model exactly stays unplanned and execution falls back to dynamic
    // collection for it.
    let plan = match document.definitions.get(operation_index) {
        Some(ast::Definition::Operation(operation)) => match root_type(schema, operation.kind) {
            Ok(root) => plan::build(schema, &document, operation, &root),
            Err(_) => plan::build_empty(),
        },
        _ => plan::build_empty(),
    };

    let introspection_errors = if uses_introspection(&document) {
        // Normal validation reports every introspection field it finds (bounded by the request's
        // error limit). Storing all of them here lets a prepared request reproduce that exact
        // response instead of collapsing to a single error.
        validation::validate(
            schema,
            &document,
            &ValidationOptions {
                introspection: false,
                max_depth: 0,
                max_errors: MAX_STORED_INTROSPECTION_ERRORS,
            },
        )
    } else {
        Vec::new()
    };

    Ok(PreparedDocument {
        document,
        operation_index,
        introspection_errors,
        static_complexity,
        variables,
        plan,
    })
}

/// Upper bound on the introspection-disabled validation errors kept at preparation.
///
/// Validation itself stops at this count, and replay truncates to the request's own
/// [`ExecutionOptions::max_errors`], so a prepared response still matches normal execution.
const MAX_STORED_INTROSPECTION_ERRORS: usize = 100;

/// Build the response normal validation would produce when introspection is disabled.
fn introspection_rejection(errors: &[GraphQLError], max_errors: usize) -> Response {
    let limit = if max_errors == 0 {
        errors.len()
    } else {
        max_errors.min(errors.len())
    };
    Response {
        data: None,
        errors: errors[..limit].to_vec(),
        extensions: None,
    }
}

/// Select the operation and return its index in `document.definitions`.
fn select_operation_index(
    document: &ast::Document<'_>,
    operation_name: Option<&str>,
) -> Result<usize, GraphQLError> {
    let mut operations = document.definitions.iter().enumerate().filter_map(
        |(index, definition)| match definition {
            ast::Definition::Operation(operation) => Some((index, operation)),
            _ => None,
        },
    );

    let Some((first_index, first)) = operations.next() else {
        return Err(GraphQLError::new("Must provide an operation."));
    };

    match operation_name {
        Some(name) => {
            if first.name == Some(name) {
                return Ok(first_index);
            }
            operations
                .find(|(_, operation)| operation.name == Some(name))
                .map(|(index, _)| index)
                .ok_or_else(|| GraphQLError::new(format!("Unknown operation named \"{name}\".")))
        }
        None => {
            if operations.next().is_some() {
                Err(GraphQLError::new(
                    "Must provide an operation name if the document contains multiple operations.",
                ))
            } else {
                Ok(first_index)
            }
        }
    }
}

/// Whether a document uses `__schema` or `__type` anywhere.
///
/// `__typename` is allowed regardless of the introspection setting, so it is deliberately not
/// counted.
fn uses_introspection(document: &ast::Document<'_>) -> bool {
    fn selection_set_uses(selection_set: &ast::SelectionSet<'_>) -> bool {
        selection_set.items.iter().any(|selection| match selection {
            ast::Selection::Field(field) => {
                matches!(field.name, "__schema" | "__type")
                    || field.selection_set.as_ref().is_some_and(selection_set_uses)
            }
            ast::Selection::InlineFragment(fragment) => selection_set_uses(&fragment.selection_set),
            // Fragment spreads are checked where the fragment is defined.
            ast::Selection::FragmentSpread(_) => false,
        })
    }

    document
        .definitions
        .iter()
        .any(|definition| match definition {
            ast::Definition::Operation(operation) => selection_set_uses(&operation.selection_set),
            ast::Definition::Fragment(fragment) => selection_set_uses(&fragment.selection_set),
            ast::Definition::TypeSystem(_) => false,
        })
}

/// The root type an operation of `kind` executes against.
pub(crate) fn root_type(
    schema: &Schema,
    kind: OperationKind,
) -> Result<Arc<ObjectType>, GraphQLError> {
    match kind {
        OperationKind::Query => Ok(Arc::clone(schema.query_type())),
        OperationKind::Mutation => schema
            .mutation_name()
            .and_then(|name| schema.object_type(name.as_ref()))
            .cloned()
            .ok_or_else(|| GraphQLError::new("Schema is not configured for mutations.")),
        OperationKind::Subscription => schema
            .subscription_type()
            .cloned()
            .ok_or_else(|| GraphQLError::new("Schema is not configured for subscriptions.")),
    }
}

/// Precomputed metadata a prepared document can lend to the shared subscription setup.
pub(crate) struct PreparedExecution<'a> {
    /// The selected operation's name, interned once at preparation.
    pub(crate) operation_name: Option<Arc<str>>,
    /// The conservative limit bound computed at preparation.
    pub(crate) static_complexity: Option<&'a StaticComplexity>,
    /// The selected operation's compiled variable definitions.
    pub(crate) variables: Option<&'a coerce::CompiledVariables>,
    /// The precompiled field collection plan.
    pub(crate) plan: &'a PreparedPlan<'a>,
}

/// Start a subscription from a prepared document.
#[allow(clippy::result_large_err)]
pub(crate) async fn subscribe<'a>(
    schema: &'a Schema,
    prepared: &'a PreparedDocument<'a>,
    operation_name: Option<Arc<str>>,
    request_variables: Option<&crate::Object>,
    options: &ExecutionOptions,
    make_event_handler: impl FnOnce(Arc<ExecutionContext>, Option<std::time::Duration>) -> EventHandler,
) -> Result<crate::SubscriptionResponse, Response> {
    if !options.introspection && !prepared.introspection_errors.is_empty() {
        return Err(introspection_rejection(
            &prepared.introspection_errors,
            options.max_errors,
        ));
    }
    let Some(operation) = prepared.operation() else {
        return Err(Response::error(GraphQLError::new(
            "Internal error: the prepared query has no selected operation.",
        )));
    };
    let execution = PreparedExecution {
        operation_name,
        static_complexity: prepared.static_complexity.as_ref(),
        variables: prepared.variables.as_ref(),
        plan: &prepared.plan,
    };
    subscribe_document(
        schema,
        &prepared.document,
        operation,
        request_variables,
        options,
        Some(execution),
        make_event_handler,
    )
    .await
}
