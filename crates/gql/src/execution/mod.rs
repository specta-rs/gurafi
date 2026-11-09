//! Request execution.
//!
//! Execution is split into the stages required by the specification: parse, validate, select an
//! operation, coerce variables, enforce request limits, then resolve fields. Each stage produces
//! specification-shaped errors instead of panicking, and a request that fails validation never
//! reaches a resolver.

pub(crate) mod coerce;
mod collect;
mod executor;
mod selection;
mod subscription;

use std::{any::Any, fmt, sync::Arc, time::Duration};

use futures::stream::BoxStream;

use crate::{
    Object, Value,
    error::{GraphQLError, PathSegment, Response},
    schema::Schema,
    types::{ResolveInfo, TypeRef},
};

pub use coerce::{CoercedVariables, coerce_argument_values, coerce_variable_values};
pub use subscription::SubscriptionResponse;

/// Limits and hooks applied to one execution.
pub struct ExecutionOptions {
    /// Arbitrary per-request data made available to resolvers.
    ///
    /// A server usually puts its authenticated user, database handles and request identifiers in
    /// here. `gql` never interprets it.
    pub context: Arc<dyn Any + Send + Sync>,
    /// The root value passed to root field resolvers.
    pub root_value: Value,
    /// Whether `__schema` and `__type` introspection fields may be used.
    pub introspection: bool,
    /// An optional field authorization hook.
    pub authorizer: Option<Arc<dyn Authorizer>>,
    /// Reject operations deeper than this many selection-set levels.
    ///
    /// Zero disables the configurable limit. The parser always rejects documents nested more
    /// than [`crate::parser::MAX_DEPTH`] levels deep, because execution itself is recursive.
    pub max_depth: usize,
    /// Reject operations whose estimated cost exceeds this value.
    pub max_complexity: u32,
    /// Multiplier applied to list fields while estimating complexity.
    pub list_complexity: u32,
    /// Maximum number of errors reported before execution stops reporting further errors.
    /// Zero disables the limit.
    pub max_errors: usize,
    /// Maximum number of sibling fields resolved concurrently. Zero means unlimited.
    ///
    /// Mutations always execute serially, as the specification requires.
    pub max_concurrency: usize,
    /// An optional wall-clock deadline for the whole operation.
    pub timeout: Option<Duration>,
}

impl fmt::Debug for ExecutionOptions {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("ExecutionOptions")
            .field("root_value", &self.root_value)
            .field("introspection", &self.introspection)
            .field("authorizer", &self.authorizer.is_some())
            .field("max_depth", &self.max_depth)
            .field("max_complexity", &self.max_complexity)
            .field("list_complexity", &self.list_complexity)
            .field("max_errors", &self.max_errors)
            .field("max_concurrency", &self.max_concurrency)
            .field("timeout", &self.timeout)
            .finish_non_exhaustive()
    }
}

impl Default for ExecutionOptions {
    fn default() -> Self {
        Self {
            context: Arc::new(()),
            root_value: Value::Null,
            introspection: true,
            authorizer: None,
            max_depth: 0,
            max_complexity: 0,
            list_complexity: 10,
            max_errors: 100,
            max_concurrency: 16,
            timeout: None,
        }
    }
}

impl ExecutionOptions {
    /// Options with a typed context object.
    #[must_use]
    pub fn with_context<T: Any + Send + Sync>(context: T) -> Self {
        Self {
            context: Arc::new(context),
            ..Self::default()
        }
    }

    /// Replace the context object.
    #[must_use]
    pub fn context<T: Any + Send + Sync>(mut self, context: T) -> Self {
        self.context = Arc::new(context);
        self
    }

    /// Set the root value.
    #[must_use]
    pub fn root_value(mut self, value: impl Into<Value>) -> Self {
        self.root_value = value.into();
        self
    }

    /// Enable or disable introspection.
    #[must_use]
    pub const fn introspection(mut self, enabled: bool) -> Self {
        self.introspection = enabled;
        self
    }

    /// Install a field authorizer.
    #[must_use]
    pub fn authorizer(mut self, authorizer: impl Authorizer + 'static) -> Self {
        self.authorizer = Some(Arc::new(authorizer));
        self
    }

    /// Limit query depth.
    #[must_use]
    pub const fn max_depth(mut self, depth: usize) -> Self {
        self.max_depth = depth;
        self
    }

    /// Limit estimated query complexity.
    #[must_use]
    pub const fn max_complexity(mut self, complexity: u32) -> Self {
        self.max_complexity = complexity;
        self
    }

    /// Limit the number of reported errors.
    #[must_use]
    pub const fn max_errors(mut self, max_errors: usize) -> Self {
        self.max_errors = max_errors;
        self
    }

    /// Limit how many sibling fields resolve concurrently. Zero means unlimited.
    #[must_use]
    pub const fn max_concurrency(mut self, max_concurrency: usize) -> Self {
        self.max_concurrency = max_concurrency;
        self
    }
}

/// Per-request execution state, visible to resolvers and introspection.
pub struct ExecutionContext {
    /// The schema being executed.
    pub schema: Schema,
    /// Per-request user data from [`ExecutionOptions::context`].
    pub user_data: Arc<dyn Any + Send + Sync>,
    /// The selected operation's name.
    pub operation_name: Option<Arc<str>>,
    /// Coerced variable values, including defaults.
    pub variables: Object,
    /// Whether introspection is enabled for this request.
    pub introspection: bool,
    /// The field authorizer, when one was configured.
    pub authorizer: Option<Arc<dyn Authorizer>>,
    /// The maximum number of errors to report.
    pub max_errors: usize,
    /// The maximum number of sibling fields resolved concurrently. Zero means unlimited.
    pub max_concurrency: usize,
    /// Extensions that plugins may write into the response.
    pub extensions: std::sync::Mutex<Option<Object>>,
}

impl fmt::Debug for ExecutionContext {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("ExecutionContext")
            .field("operation_name", &self.operation_name)
            .field("variables", &self.variables)
            .field("introspection", &self.introspection)
            .finish_non_exhaustive()
    }
}

impl ExecutionContext {
    /// Downcast the user data to a concrete type.
    #[must_use]
    pub fn data<T: Any>(&self) -> Option<&T> {
        self.user_data.downcast_ref()
    }

    /// Downcast the user data, returning a GraphQL error when the type is wrong.
    ///
    /// # Errors
    ///
    /// Returns an error when the server supplied a different context type. This should be
    /// unreachable in well-typed setups, but reporting it is far better than panicking inside a
    /// request.
    pub fn require_data<T: Any>(&self) -> Result<&T, GraphQLError> {
        self.data().ok_or_else(|| {
            GraphQLError::new("Execution context has an unexpected type; check the server setup.")
        })
    }

    /// Store a response extension.
    pub fn set_extension(&self, key: impl Into<Arc<str>>, value: impl Into<Value>) {
        if let Ok(mut extensions) = self.extensions.lock() {
            extensions
                .get_or_insert_with(Object::new)
                .insert(key, value);
        }
    }
}

/// Information passed to an [`Authorizer`] for each field.
pub struct AuthorizationContext<'a> {
    /// The execution context.
    pub context: &'a ExecutionContext,
    /// The field's name in the schema.
    pub field_name: &'a str,
    /// The parent type's name.
    pub parent_type: &'a str,
    /// The field's return type.
    pub return_type: &'a TypeRef,
    /// The response path to the field.
    pub path: &'a [PathSegment],
    /// The coerced arguments.
    pub args: &'a Object,
    /// Static field information, shared with resolvers.
    pub info: &'a ResolveInfo<'a>,
}

impl fmt::Debug for AuthorizationContext<'_> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("AuthorizationContext")
            .field("field_name", &self.field_name)
            .field("parent_type", &self.parent_type)
            .field("path", &self.path)
            .finish_non_exhaustive()
    }
}

/// A field-level authorization hook.
///
/// Authentication (who the caller is) belongs to the HTTP layer. Authorization (whether this
/// caller may see this field) is enforced here, immediately before a resolver runs, so a denied
/// field behaves like a resolver error: it is nulled in the response and reported in `errors`.
pub trait Authorizer: Send + Sync {
    /// Decide whether a field may be resolved.
    ///
    /// # Errors
    ///
    /// Return an error to deny access. The error message is exposed to the client, so avoid
    /// leaking sensitive details.
    fn authorize(&self, context: &AuthorizationContext<'_>) -> Result<(), GraphQLError>;
}

/// A GraphQL request.
#[derive(Clone, Copy, Debug)]
pub struct Request<'a> {
    /// The GraphQL document.
    pub query: &'a str,
    /// The operation to execute when the document contains more than one.
    pub operation_name: Option<&'a str>,
    /// Variable values, as a JSON-like object.
    pub variables: Option<&'a Object>,
}

impl<'a> Request<'a> {
    /// A request for a document with no variables.
    #[must_use]
    pub const fn new(query: &'a str) -> Self {
        Self {
            query,
            operation_name: None,
            variables: None,
        }
    }

    /// Select an operation by name.
    #[must_use]
    pub const fn operation_name(mut self, name: &'a str) -> Self {
        self.operation_name = Some(name);
        self
    }

    /// Attach variable values.
    #[must_use]
    pub const fn variables(mut self, variables: &'a Object) -> Self {
        self.variables = Some(variables);
        self
    }
}

/// Execute a GraphQL request.
///
/// The returned response always has the shape a GraphQL client expects: `data` is omitted when
/// execution could not start, `errors` is omitted when there were none, and partial data is
/// preserved exactly as the specification requires.
#[cfg_attr(
    feature = "tracing",
    tracing::instrument(
        skip(schema, request, options),
        level = "debug",
        name = "graphql.execute"
    )
)]
pub async fn execute(
    schema: &Schema,
    request: Request<'_>,
    options: &ExecutionOptions,
) -> Response {
    executor::execute(schema, request, options).await
}

/// Execute a GraphQL subscription.
///
/// Parsing, validation and subscription-field selection happen immediately. The returned stream
/// then yields one response per source event. Errors during setup are returned as a response
/// instead of a stream.
///
/// # Errors
///
/// Returns a fully formed error response when the request is invalid or the subscription could
/// not be started.
#[cfg_attr(
    feature = "tracing",
    tracing::instrument(
        skip(schema, request, options),
        level = "debug",
        name = "graphql.subscribe"
    )
)]
pub async fn subscribe(
    schema: &Schema,
    request: Request<'_>,
    options: &ExecutionOptions,
) -> Result<SubscriptionResponse, Response> {
    subscription::subscribe(schema, request, options).await
}

/// A boxed stream of subscription responses, useful for transport layers.
pub type ResponseStream = BoxStream<'static, Response>;
