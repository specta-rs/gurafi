//! The transport-independent GraphQL server.
//!
//! [`Server`] owns a [`gql::Schema`] and the defaults applied to every operation: introspection,
//! depth and complexity limits, timeouts and batch settings. It has no opinion about HTTP; mount
//! it with [`crate::http`] or [`crate::axum`].
//!
//! Cloning a `Server` is cheap (everything lives behind one `Arc`), so wrapping it in another
//! `Arc` is unnecessary.

use core::fmt;
use std::{any::Any, sync::Arc, time::Duration};

use gql::{Authorizer, ExecutionOptions, Schema, SubscriptionResponse, Value};

use crate::request::GraphQLRequest;

/// Default maximum request body size: 1 MiB.
pub const DEFAULT_MAX_BODY_BYTES: usize = 1024 * 1024;

/// Default maximum number of operations in a batch.
pub const DEFAULT_MAX_BATCH_SIZE: usize = 16;

/// Default number of batch operations executed concurrently.
pub const DEFAULT_BATCH_CONCURRENCY: usize = 16;

/// Per-request user data handed to resolvers.
///
/// This is deliberately a thin wrapper around `Arc<dyn Any + Send + Sync>` so servers can build
/// one value in their authentication layer and cheaply clone it into every batch operation.
#[derive(Clone)]
pub struct Context(Arc<dyn Any + Send + Sync>);

impl Default for Context {
    fn default() -> Self {
        Self::empty()
    }
}

impl Context {
    /// Wrap a typed value.
    #[must_use]
    pub fn new<T: Any + Send + Sync>(value: T) -> Self {
        Self(Arc::new(value))
    }

    /// A context containing `()`.
    #[must_use]
    pub fn empty() -> Self {
        Self(Arc::new(()))
    }

    /// Wrap an existing type-erased context.
    #[must_use]
    pub fn from_arc(context: Arc<dyn Any + Send + Sync>) -> Self {
        Self(context)
    }

    /// Borrow the type-erased value.
    #[must_use]
    pub fn as_arc(&self) -> &Arc<dyn Any + Send + Sync> {
        &self.0
    }

    /// Take the type-erased value.
    #[must_use]
    pub fn into_arc(self) -> Arc<dyn Any + Send + Sync> {
        self.0
    }

    /// Downcast to a concrete type.
    #[must_use]
    pub fn downcast_ref<T: Any>(&self) -> Option<&T> {
        self.0.downcast_ref()
    }

    /// Whether this is the empty context.
    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.0.is::<()>()
    }
}

impl fmt::Debug for Context {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("Context")
            .field("empty", &self.is_empty())
            .finish()
    }
}

impl From<Arc<dyn Any + Send + Sync>> for Context {
    fn from(value: Arc<dyn Any + Send + Sync>) -> Self {
        Self::from_arc(value)
    }
}

impl<T: Any + Send + Sync> From<Arc<T>> for Context {
    fn from(value: Arc<T>) -> Self {
        Self(value)
    }
}

impl From<Context> for Arc<dyn Any + Send + Sync> {
    fn from(value: Context) -> Self {
        value.into_arc()
    }
}

#[derive(Clone)]
struct ServerInner {
    schema: Schema,
    introspection: bool,
    max_depth: usize,
    max_complexity: u32,
    list_complexity: u32,
    max_errors: usize,
    max_concurrency: usize,
    max_body_bytes: usize,
    timeout: Option<Duration>,
    max_batch_size: usize,
    batch_concurrency: usize,
    authorizer: Option<Arc<dyn Authorizer>>,
}

/// A GraphQL server.
#[derive(Clone)]
pub struct Server {
    inner: Arc<ServerInner>,
}

impl fmt::Debug for Server {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("Server")
            .field("schema", &self.inner.schema)
            .field("introspection", &self.inner.introspection)
            .field("max_depth", &self.inner.max_depth)
            .field("max_complexity", &self.inner.max_complexity)
            .field("max_errors", &self.inner.max_errors)
            .field("max_concurrency", &self.inner.max_concurrency)
            .field("max_body_bytes", &self.inner.max_body_bytes)
            .field("timeout", &self.inner.timeout)
            .field("max_batch_size", &self.inner.max_batch_size)
            .field("batch_concurrency", &self.inner.batch_concurrency)
            .field("authorizer", &self.inner.authorizer.is_some())
            .finish()
    }
}

impl Server {
    /// Create a server for a schema.
    ///
    /// Defaults: introspection enabled, no depth or complexity limit, at most 100 reported errors,
    /// a 1 MiB body limit, batches of 16 operations with 16-way concurrency, and no timeout.
    #[must_use]
    pub fn new(schema: Schema) -> Self {
        Self {
            inner: Arc::new(ServerInner {
                schema,
                introspection: true,
                max_depth: 0,
                max_complexity: 0,
                list_complexity: 10,
                max_errors: 100,
                max_concurrency: 16,
                max_body_bytes: DEFAULT_MAX_BODY_BYTES,
                timeout: None,
                max_batch_size: DEFAULT_MAX_BATCH_SIZE,
                batch_concurrency: DEFAULT_BATCH_CONCURRENCY,
                authorizer: None,
            }),
        }
    }

    fn inner_mut(&mut self) -> &mut ServerInner {
        Arc::make_mut(&mut self.inner)
    }

    /// Enable or disable `__schema` and `__type` introspection fields.
    #[must_use]
    pub fn introspection(mut self, enabled: bool) -> Self {
        self.inner_mut().introspection = enabled;
        self
    }

    /// Reject operations deeper than this many selection-set levels. `0` disables the limit.
    #[must_use]
    pub fn max_depth(mut self, depth: usize) -> Self {
        self.inner_mut().max_depth = depth;
        self
    }

    /// Reject operations whose estimated cost exceeds this value. `0` disables the limit.
    #[must_use]
    pub fn max_complexity(mut self, complexity: u32) -> Self {
        self.inner_mut().max_complexity = complexity;
        self
    }

    /// Limit how many errors are reported before execution stops adding more. Zero disables the
    /// limit.
    #[must_use]
    pub fn max_errors(mut self, max_errors: usize) -> Self {
        self.inner_mut().max_errors = max_errors;
        self
    }

    /// Limit how many sibling fields resolve concurrently. `0` means unlimited.
    #[must_use]
    pub fn max_concurrency(mut self, concurrency: usize) -> Self {
        self.inner_mut().max_concurrency = concurrency;
        self
    }

    /// Set the default maximum HTTP request body size in bytes. `0` disables the limit.
    ///
    /// This is a transport default; [`crate::http::HttpConfig`] can override it for one mount.
    #[must_use]
    pub fn max_body_bytes(mut self, bytes: usize) -> Self {
        self.inner_mut().max_body_bytes = bytes;
        self
    }

    /// Set a wall-clock timeout for each operation. `None` disables it.
    #[must_use]
    pub fn timeout(mut self, timeout: Duration) -> Self {
        self.inner_mut().timeout = Some(timeout);
        self
    }

    /// Remove the wall-clock timeout.
    #[must_use]
    pub fn without_timeout(mut self) -> Self {
        self.inner_mut().timeout = None;
        self
    }

    /// Limit the number of operations accepted in one batch. `0` disables the limit.
    #[must_use]
    pub fn max_batch_size(mut self, size: usize) -> Self {
        self.inner_mut().max_batch_size = size;
        self
    }

    /// Limit how many operations in a batch run concurrently. Values below one are treated as
    /// one.
    #[must_use]
    pub fn batch_concurrency(mut self, concurrency: usize) -> Self {
        self.inner_mut().batch_concurrency = concurrency.max(1);
        self
    }

    /// Apply complexity analysis settings from `gql`.
    ///
    /// Zero values in `config` leave the corresponding server limit unchanged. `list_factor`
    /// becomes the executor's list multiplier; `field_cost` is not represented by
    /// [`gql::ExecutionOptions`] and is currently ignored.
    #[must_use]
    pub fn complexity_config(mut self, config: gql::complexity::ComplexityOptions) -> Self {
        let inner = self.inner_mut();
        inner.list_complexity = config.list_factor;
        if config.max_depth != 0 {
            inner.max_depth = config.max_depth;
        }
        if config.max_complexity != 0 {
            inner.max_complexity = config.max_complexity;
        }
        self
    }

    /// Install a field-level authorizer.
    #[must_use]
    pub fn authorizer(mut self, authorizer: impl Authorizer + 'static) -> Self {
        self.inner_mut().authorizer = Some(Arc::new(authorizer));
        self
    }

    /// The schema this server executes.
    #[must_use]
    pub fn schema(&self) -> &Schema {
        &self.inner.schema
    }

    /// The configured maximum body size.
    #[must_use]
    pub fn max_body_bytes_limit(&self) -> usize {
        self.inner.max_body_bytes
    }

    /// The configured maximum batch size.
    #[must_use]
    pub fn max_batch_size_limit(&self) -> usize {
        self.inner.max_batch_size
    }

    /// Execute a query or mutation.
    ///
    /// Parse, validation, variable coercion and resolver errors all come back inside the returned
    /// response, exactly as the GraphQL specification requires.
    #[tracing::instrument(skip(self, request, context), level = "debug")]
    pub async fn execute(&self, request: &GraphQLRequest, context: Context) -> gql::Response {
        let variables = match request.variables_object() {
            Ok(variables) => variables,
            Err(error) => return error.into_response(),
        };
        let gql_request = request.as_gql(variables.as_ref());
        let options = self.execution_options(context);
        let future = gql::execute(&self.inner.schema, gql_request, &options);
        match options.timeout {
            Some(timeout) => timed(timeout, future).await,
            None => future.await,
        }
    }

    /// Execute an array of requests concurrently, preserving their order.
    ///
    /// Concurrency is bounded by [`Server::batch_concurrency`]. A request with invalid variables
    /// produces its own error response; it does not fail the batch.
    pub async fn execute_batch(
        &self,
        requests: &[GraphQLRequest],
        context: Context,
    ) -> Vec<gql::Response> {
        let limit = self.inner.batch_concurrency.max(1);
        let mut responses = Vec::with_capacity(requests.len());

        // Run in chunks so concurrency is bounded but request order is preserved.
        for chunk in requests.chunks(limit) {
            let futures = chunk
                .iter()
                .map(|request| self.execute(request, context.clone()));
            responses.extend(futures::future::join_all(futures).await);
        }

        responses
    }

    /// Start a subscription.
    ///
    /// # Errors
    ///
    /// Returns a fully formed error response when the request is not a valid subscription or the
    /// source stream could not be created.
    #[tracing::instrument(skip(self, request, context), level = "debug")]
    pub async fn subscribe(
        &self,
        request: &GraphQLRequest,
        context: Context,
    ) -> Result<SubscriptionResponse, gql::Response> {
        let variables = match request.variables_object() {
            Ok(variables) => variables,
            Err(error) => return Err(error.into_response()),
        };
        let gql_request = request.as_gql(variables.as_ref());
        let options = self.execution_options(context);
        let future = gql::subscribe(&self.inner.schema, gql_request, &options);
        match options.timeout {
            Some(timeout) => timed_subscribe(timeout, future).await,
            None => future.await,
        }
    }

    fn execution_options(&self, context: Context) -> ExecutionOptions {
        let inner = &*self.inner;
        ExecutionOptions {
            context: context.into_arc(),
            root_value: Value::Null,
            introspection: inner.introspection,
            authorizer: inner.authorizer.clone(),
            max_depth: inner.max_depth,
            max_complexity: inner.max_complexity,
            list_complexity: inner.list_complexity,
            max_errors: inner.max_errors,
            max_concurrency: inner.max_concurrency,
            timeout: inner.timeout,
        }
    }
}

/// Enforce a real wall-clock deadline around execution.
///
/// `gql` checks its own deadline between fields so it can report a partial result, but a resolver
/// that never yields would otherwise run forever. Transports that have a timer runtime wrap the
/// future and turn an expiry into a normal GraphQL error response.
#[cfg(feature = "axum")]
async fn timed_subscribe<F>(
    timeout: Duration,
    future: F,
) -> Result<gql::SubscriptionResponse, gql::Response>
where
    F: std::future::Future<Output = Result<gql::SubscriptionResponse, gql::Response>>,
{
    match tokio::time::timeout(timeout, future).await {
        Ok(response) => response,
        Err(_) => Err(gql::Response::error(gql::GraphQLError::new(format!(
            "The subscription timed out while starting after {timeout:?}."
        )))),
    }
}

#[cfg(not(feature = "axum"))]
async fn timed_subscribe<F>(
    _timeout: Duration,
    future: F,
) -> Result<gql::SubscriptionResponse, gql::Response>
where
    F: std::future::Future<Output = Result<gql::SubscriptionResponse, gql::Response>>,
{
    future.await
}

#[cfg(feature = "axum")]
async fn timed<F>(timeout: Duration, future: F) -> gql::Response
where
    F: std::future::Future<Output = gql::Response>,
{
    match tokio::time::timeout(timeout, future).await {
        Ok(response) => response,
        Err(_) => gql::Response::error(gql::GraphQLError::new(format!(
            "The operation timed out after {timeout:?}."
        ))),
    }
}

#[cfg(not(feature = "axum"))]
async fn timed<F>(_timeout: Duration, future: F) -> gql::Response
where
    F: std::future::Future<Output = gql::Response>,
{
    future.await
}
