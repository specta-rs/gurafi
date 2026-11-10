//! The transport-independent GraphQL server.
//!
//! [`Server`] owns a [`gql::Schema`] and the defaults applied to every operation: introspection,
//! depth and complexity limits, timeouts, batch settings and the optional error formatter. It has
//! no opinion about HTTP; mount it with [`crate::http`] or [`crate::axum`].
//!
//! Cloning a `Server` is cheap (everything lives behind one `Arc`), so wrapping it in another
//! `Arc` is unnecessary.

use core::fmt;
use std::{any::Any, future::Future, sync::Arc, time::Duration};

use futures::StreamExt as _;
use gql::{
    Authorizer, ExecutionOptions, GraphQLError, Response, Schema, SubscriptionResponse, Value,
    ast::OperationKind,
    prepared::{PreparedQuery, PreparedQueryRegistry},
};

use crate::{
    error::{ErrorContext, ErrorFormatter, ErrorKind, ResponseMeta, format_response_errors},
    prepared::{PreparedQueries, persisted_query_not_found, persisted_query_not_found_error},
    request::GraphQLRequest,
};

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
    max_breadth: usize,
    list_complexity: u32,
    max_errors: usize,
    max_concurrency: usize,
    max_body_bytes: usize,
    timeout: Option<Duration>,
    max_batch_size: usize,
    batch_concurrency: usize,
    authorizer: Option<Arc<dyn Authorizer>>,
    prepared: Option<PreparedQueryRegistry>,
    require_prepared: bool,
    error_formatter: Option<Arc<dyn ErrorFormatter>>,
}

/// A GraphQL server.
///
/// Configuration is immutable and copy-on-write, so cloning a server is cheap. Install an error
/// formatter with [`Server::format_error`] to redact messages before they reach a client.
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
            .field("max_breadth", &self.inner.max_breadth)
            .field("max_errors", &self.inner.max_errors)
            .field("max_concurrency", &self.inner.max_concurrency)
            .field("max_body_bytes", &self.inner.max_body_bytes)
            .field("timeout", &self.inner.timeout)
            .field("max_batch_size", &self.inner.max_batch_size)
            .field("batch_concurrency", &self.inner.batch_concurrency)
            .field("authorizer", &self.inner.authorizer.is_some())
            .field(
                "prepared_queries",
                &self.inner.prepared.as_ref().map_or(0, |r| r.len()),
            )
            .field("require_prepared", &self.inner.require_prepared)
            .field("error_formatter", &self.inner.error_formatter.is_some())
            .finish()
    }
}

impl Server {
    /// Create a server for a schema.
    ///
    /// Defaults are deliberately safe for an internet-facing endpoint: introspection enabled, a
    /// depth limit of 32, a complexity limit of 100,000, a 30 second timeout, at most 100 reported
    /// errors, 16-way field and batch concurrency, and a 1 MiB body limit.
    #[must_use]
    pub fn new(schema: Schema) -> Self {
        Self {
            inner: Arc::new(ServerInner {
                schema,
                introspection: true,
                max_depth: 32,
                max_complexity: 100_000,
                max_breadth: 1_000,
                list_complexity: 10,
                max_errors: 100,
                max_concurrency: 16,
                max_body_bytes: DEFAULT_MAX_BODY_BYTES,
                timeout: Some(Duration::from_secs(30)),
                max_batch_size: DEFAULT_MAX_BATCH_SIZE,
                batch_concurrency: DEFAULT_BATCH_CONCURRENCY,
                authorizer: None,
                prepared: None,
                require_prepared: false,
                error_formatter: None,
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

    /// Limit the number of distinct fields in any one selection set. `0` disables the limit.
    #[must_use]
    pub fn max_breadth(mut self, breadth: usize) -> Self {
        self.inner_mut().max_breadth = breadth;
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
        if config.max_breadth != 0 {
            inner.max_breadth = config.max_breadth;
        }
        self
    }

    /// Install a field-level authorizer.
    #[must_use]
    pub fn authorizer(mut self, authorizer: impl Authorizer + 'static) -> Self {
        self.inner_mut().authorizer = Some(Arc::new(authorizer));
        self
    }

    /// Install a set of persisted operations loaded from a manifest.
    ///
    /// When a request carries an identifier that matches an entry, the registered query text is
    /// executed. Supplied query text is ignored for a matching identifier, so a client can never
    /// substitute its own document for a trusted persisted one.
    ///
    /// The entries are tied to the schema instance they were prepared against. Installing a set
    /// prepared for a different schema does not panic; requests that resolve to such an entry are
    /// rejected with a GraphQL error.
    #[must_use]
    pub fn prepared_queries(mut self, queries: PreparedQueries) -> Self {
        self.inner_mut().prepared = Some(queries.into_registry());
        self
    }

    /// Install a prepared-query registry directly.
    ///
    /// As with [`Server::prepared_queries`], entries prepared against a different schema instance
    /// are rejected at execution time.
    #[must_use]
    pub fn prepared_query_registry(mut self, registry: PreparedQueryRegistry) -> Self {
        self.inner_mut().prepared = Some(registry);
        self
    }

    /// Require every operation to resolve to a registered prepared query.
    ///
    /// In allowlist mode requests without an identifier, and requests whose identifier is
    /// unknown, fail with `PersistedQueryNotFound`. Query text alone is never executed.
    #[must_use]
    pub fn require_prepared(mut self, required: bool) -> Self {
        self.inner_mut().require_prepared = required;
        self
    }

    /// The installed prepared-query registry, if any.
    #[must_use]
    pub fn prepared_query_registry_ref(&self) -> Option<&PreparedQueryRegistry> {
        self.inner.prepared.as_ref()
    }

    /// Look up a prepared query by identifier.
    ///
    /// This does not verify that the query was prepared against this server's schema instance.
    /// Execution does; use [`Server::prepared_query_for_current_schema`] when the caller needs the
    /// same guarantee.
    #[must_use]
    pub fn prepared_query(&self, id: &str) -> Option<&PreparedQuery> {
        self.inner.prepared.as_ref()?.get(id)
    }

    /// Look up a prepared query and verify it targets this server's schema instance.
    ///
    /// A registry can be built against one schema and then installed on a server for another.
    /// Executing such a query would run resolvers and type lookups from the wrong schema, so all
    /// request paths reject it with a GraphQL error instead.
    ///
    /// # Errors
    ///
    /// Returns a [`gql::GraphQLError`] when the identifier exists but its query was prepared against
    /// a different schema instance. Returns `Ok(None)` for an unknown identifier.
    pub fn prepared_query_for_current_schema(
        &self,
        id: &str,
    ) -> Result<Option<&PreparedQuery>, gql::GraphQLError> {
        let Some(prepared) = self.prepared_query(id) else {
            return Ok(None);
        };
        if !prepared.schema().ptr_eq(&self.inner.schema) {
            return Err(GraphQLError::new(format!(
                "Prepared query \"{id}\" was prepared against a different schema instance; prepare it with this server's schema."
            )));
        }
        Ok(Some(prepared))
    }

    /// Whether prepared queries are required (allowlist mode).
    #[must_use]
    pub fn requires_prepared_queries(&self) -> bool {
        self.inner.require_prepared
    }

    /// Install an error formatter that runs over every error in every response.
    ///
    /// The formatter is called once per error, just before the response is returned or streamed.
    /// It applies to single operations, batches, persisted operations, subscription setup errors
    /// and subscription events, and to request errors produced by [`crate::http`]. It receives an
    /// [`ErrorContext`] describing the phase, operation name, persisted-query identifier and
    /// response path.
    ///
    /// The formatter must be deterministic and thread-safe. It is called synchronously; a panic
    /// propagates to the caller (or fails the subscription stream) and is not caught.
    ///
    /// [`mask_internal`](crate::mask_internal) is a conservative formatter that hides messages
    /// which are neither request diagnostics nor tagged with an `extensions.code`.
    ///
    /// ```no_run
    /// use suku::{Server, mask_internal};
    ///
    /// # fn build(schema: gql::Schema) {
    /// let server = Server::new(schema).format_error(mask_internal);
    /// # let _ = server;
    /// # }
    /// ```
    #[must_use]
    pub fn format_error<F>(mut self, formatter: F) -> Self
    where
        F: for<'a> Fn(&GraphQLError, &ErrorContext<'a>) -> GraphQLError + Send + Sync + 'static,
    {
        self.inner_mut().error_formatter = Some(Arc::new(formatter) as Arc<dyn ErrorFormatter>);
        self
    }

    /// Install a pre-built error formatter.
    ///
    /// This is the trait-object form of [`Server::format_error`], useful when the formatter is a
    /// named type or is shared with other endpoints.
    #[must_use]
    pub fn format_error_with(mut self, formatter: Arc<dyn ErrorFormatter>) -> Self {
        self.inner_mut().error_formatter = Some(formatter);
        self
    }

    /// Whether an error formatter is installed.
    #[must_use]
    pub fn has_error_formatter(&self) -> bool {
        self.inner.error_formatter.is_some()
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
    /// response, exactly as the GraphQL specification requires. If an error formatter is
    /// installed, every error in the response has already passed through it.
    #[cfg_attr(
        feature = "tracing",
        tracing::instrument(
            skip_all,
            level = "debug",
            name = "suku.execute",
            fields(
                operation = request.operation_name.as_deref().unwrap_or("<anonymous>"),
                prepared = request.prepared_id().is_some(),
                document_bytes = request.query.len(),
                variable_count = request.variables.as_ref().map_or(0, serde_json::Map::len),
                errors = tracing::field::Empty,
            )
        )
    )]
    pub async fn execute(&self, request: &GraphQLRequest, context: Context) -> gql::Response {
        let variables = match request.variables_object() {
            Ok(variables) => variables,
            Err(error) => return self.request_response(request, error.into_response()),
        };
        let options = self.execution_options(context);

        if let Some(id) = request.prepared_id() {
            match self.prepared_query_for_current_schema(id) {
                Ok(Some(prepared)) => {
                    if let Some(error) =
                        prepared_operation_name_error(prepared, request.operation_name.as_deref())
                    {
                        return self.request_response(request, Response::error(error));
                    }
                    let meta = ResponseMeta::from_request(request)
                        .with_operation_name(prepared.operation_name());
                    let future = prepared.execute(variables.as_ref(), &options);
                    return self.await_execute(meta, options.timeout, future).await;
                }
                Ok(None) => {}
                Err(error) => {
                    return self.request_response(request, Response::error(error));
                }
            }
            if self.inner.require_prepared || request.query.trim().is_empty() {
                return self.request_response(request, persisted_query_not_found(id));
            }
        } else if self.inner.require_prepared {
            return self.request_response(request, persisted_query_not_found(""));
        } else if request.query.trim().is_empty() {
            return self.request_response(request, missing_query_response());
        }

        let gql_request = request.as_gql(variables.as_ref());
        let future = gql::execute(&self.inner.schema, gql_request, &options);
        self.await_execute(ResponseMeta::from_request(request), options.timeout, future)
            .await
    }

    /// Execute an array of requests concurrently, preserving their order.
    ///
    /// Concurrency is bounded by [`Server::batch_concurrency`]. A request with invalid variables
    /// produces its own error response; it does not fail the batch. Each response is formatted
    /// independently, so a formatter never reorders a batch.
    #[cfg_attr(
        feature = "tracing",
        tracing::instrument(
            skip_all,
            level = "debug",
            name = "suku.execute_batch",
            fields(batch_size = requests.len(), concurrency = self.inner.batch_concurrency)
        )
    )]
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
    /// Setup errors and every response in the returned event stream pass through the installed
    /// error formatter before they are observed.
    ///
    /// # Errors
    ///
    /// Returns a fully formed error response when the request is not a valid subscription or the
    /// source stream could not be created.
    #[cfg_attr(
        feature = "tracing",
        tracing::instrument(
            skip_all,
            level = "debug",
            name = "suku.subscription",
            fields(
                operation = request.operation_name.as_deref().unwrap_or("<anonymous>"),
                prepared = request.prepared_id().is_some(),
            )
        )
    )]
    pub async fn subscribe(
        &self,
        request: &GraphQLRequest,
        context: Context,
    ) -> Result<SubscriptionResponse, gql::Response> {
        let variables = match request.variables_object() {
            Ok(variables) => variables,
            Err(error) => return Err(self.request_response(request, error.into_response())),
        };
        let options = self.execution_options(context);

        if let Some(id) = request.prepared_id() {
            match self.prepared_query_for_current_schema(id) {
                Ok(Some(prepared)) => {
                    if let Some(error) =
                        prepared_operation_name_error(prepared, request.operation_name.as_deref())
                    {
                        return Err(self.request_response(request, Response::error(error)));
                    }
                    let meta = ResponseMeta::from_request(request)
                        .with_operation_name(prepared.operation_name());
                    let future = prepared.subscribe(variables.as_ref(), &options);
                    return self.await_subscribe(meta, options.timeout, future).await;
                }
                Ok(None) => {}
                Err(error) => {
                    return Err(self.request_response(request, Response::error(error)));
                }
            }
            if self.inner.require_prepared || request.query.trim().is_empty() {
                return Err(self.request_response(request, persisted_query_not_found(id)));
            }
        } else if self.inner.require_prepared {
            return Err(self.request_response(request, persisted_query_not_found("")));
        } else if request.query.trim().is_empty() {
            return Err(self.request_response(request, missing_query_response()));
        }

        let gql_request = request.as_gql(variables.as_ref());
        let future = gql::subscribe(&self.inner.schema, gql_request, &options);
        self.await_subscribe(ResponseMeta::from_request(request), options.timeout, future)
            .await
    }

    /// Determine the operation kind a request will execute.
    ///
    /// A prepared identifier is resolved without parsing any client-supplied query text. This is
    /// what lets the HTTP layer make GET-safety and event-stream decisions for persisted requests.
    pub(crate) fn operation_kind(
        &self,
        request: &GraphQLRequest,
    ) -> Result<Option<OperationKind>, gql::GraphQLError> {
        if let Some(id) = request.prepared_id() {
            match self.prepared_query_for_current_schema(id) {
                Ok(Some(prepared)) => {
                    if let Some(error) =
                        prepared_operation_name_error(prepared, request.operation_name.as_deref())
                    {
                        return Err(error);
                    }
                    return Ok(Some(prepared.kind()));
                }
                Ok(None) => {}
                Err(error) => return Err(error),
            }
            if self.inner.require_prepared || request.query.trim().is_empty() {
                return Err(persisted_query_not_found_error(id));
            }
        } else if self.inner.require_prepared {
            return Err(persisted_query_not_found_error(""));
        } else if request.query.trim().is_empty() {
            return Err(missing_query_error());
        }

        operation_kind_from_text(request)
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
            max_breadth: inner.max_breadth,
            list_complexity: inner.list_complexity,
            max_errors: inner.max_errors,
            max_concurrency: inner.max_concurrency,
            timeout: inner.timeout,
        }
    }

    /// Apply the installed formatter to every error in a response.
    pub(crate) fn format_response(
        &self,
        response: &mut gql::Response,
        meta: ResponseMeta<'_>,
        default_kind: ErrorKind,
    ) {
        let Some(formatter) = self.inner.error_formatter.as_deref() else {
            return;
        };
        format_response_errors(
            formatter,
            response,
            meta.operation_name,
            meta.request_id,
            default_kind,
        );
    }

    /// Format a response built for a request error.
    fn request_response(&self, request: &GraphQLRequest, response: gql::Response) -> gql::Response {
        let mut response = response;
        self.format_response(
            &mut response,
            ResponseMeta::from_request(request),
            ErrorKind::Request,
        );
        response
    }

    /// Await execution and format its result.
    ///
    /// Responses from `gql::execute` mix request errors (parse, validation, coercion) with
    /// execution errors. [`format_response`] classifies each error by its response path, so a
    /// no-path error defaults to a request error. A wall-clock timeout is constructed here and
    /// explicitly classified as an execution error.
    async fn await_execute<F>(
        &self,
        meta: ResponseMeta<'_>,
        timeout: Option<Duration>,
        future: F,
    ) -> gql::Response
    where
        F: Future<Output = gql::Response>,
    {
        let (mut response, kind) = match timeout {
            Some(timeout) => match timed(timeout, future).await {
                Some(response) => (response, ErrorKind::Request),
                None => (operation_timeout_response(timeout), ErrorKind::Execution),
            },
            None => (future.await, ErrorKind::Request),
        };
        self.format_response(&mut response, meta, kind);
        response
    }

    /// Await subscription setup, format failures and wrap the event stream.
    #[allow(clippy::result_large_err)]
    async fn await_subscribe<F>(
        &self,
        meta: ResponseMeta<'_>,
        timeout: Option<Duration>,
        future: F,
    ) -> Result<SubscriptionResponse, gql::Response>
    where
        F: Future<Output = Result<SubscriptionResponse, gql::Response>>,
    {
        let outcome = match timeout {
            Some(timeout) => timed_subscribe(timeout, future).await,
            None => future
                .await
                .map_err(|response| (response, ErrorKind::Request)),
        };
        match outcome {
            Ok(subscription) => Ok(self.format_subscription(subscription, meta)),
            Err((mut response, kind)) => {
                self.format_response(&mut response, meta, kind);
                Err(response)
            }
        }
    }

    /// Format every response in a subscription's event stream.
    ///
    /// Mapping a stream preserves its order, so events keep the order the source produced them in.
    fn format_subscription(
        &self,
        subscription: SubscriptionResponse,
        meta: ResponseMeta<'_>,
    ) -> SubscriptionResponse {
        let Some(formatter) = self.inner.error_formatter.clone() else {
            return subscription;
        };
        let meta = meta.into_owned();
        let stream = subscription
            .stream
            .map(move |mut response| {
                format_response_errors(
                    formatter.as_ref(),
                    &mut response,
                    meta.operation_name.as_deref(),
                    meta.request_id.as_deref(),
                    ErrorKind::Execution,
                );
                response
            })
            .boxed();
        SubscriptionResponse { stream }
    }
}

/// The request error for a body that has neither query text nor a persisted identifier.
fn missing_query_error() -> gql::GraphQLError {
    gql::GraphQLError::new("Missing required `query` field.")
}

/// Reject a persisted request whose `operationName` does not match the prepared operation.
///
/// The prepared document selected its operation at preparation time, so a request cannot change
/// which operation runs. Silently ignoring a disagreeing name would hide a client bug, so it is
/// reported instead.
fn prepared_operation_name_error(
    prepared: &PreparedQuery,
    requested: Option<&str>,
) -> Option<gql::GraphQLError> {
    let requested = requested?;
    if prepared.operation_name() == Some(requested) {
        return None;
    }
    Some(gql::GraphQLError::new(format!(
        "Prepared query \"{}\" does not contain an operation named \"{requested}\".",
        prepared.id()
    )))
}

fn missing_query_response() -> gql::Response {
    gql::Response::error(missing_query_error())
}

/// Parse the operation kind from query text.
fn operation_kind_from_text(
    request: &GraphQLRequest,
) -> Result<Option<OperationKind>, gql::GraphQLError> {
    let document = gql::parser::parse_query(&request.query)?;
    match request.operation_name.as_deref() {
        Some(name) => document
            .operations()
            .find(|operation| operation.name == Some(name))
            .map(|operation| Some(operation.kind))
            .ok_or_else(|| gql::GraphQLError::new(format!("Unknown operation named \"{name}\"."))),
        None => {
            let mut operations = document.operations();
            let Some(operation) = operations.next() else {
                return Err(gql::GraphQLError::new(
                    "The document does not contain any executable operation.",
                ));
            };
            if operations.next().is_some() {
                return Err(gql::GraphQLError::new(
                    "Must provide an operation name when the document contains multiple operations.",
                ));
            }
            Ok(Some(operation.kind))
        }
    }
}

/// Enforce a real wall-clock deadline around execution.///
/// `gql` checks its own deadline between fields so it can report a partial result, but a resolver
/// that never yields would otherwise run forever. Transports that have a timer runtime wrap the
/// future and turn an expiry into a normal GraphQL error response.
#[cfg(feature = "axum")]
#[allow(clippy::result_large_err)]
async fn timed_subscribe<F>(
    timeout: Duration,
    future: F,
) -> Result<gql::SubscriptionResponse, (gql::Response, ErrorKind)>
where
    F: std::future::Future<Output = Result<gql::SubscriptionResponse, gql::Response>>,
{
    // `tokio::time` panics without a runtime; core `Server` execution must still work under a
    // plain futures executor. Without a reactor we fall back to gql's cooperative deadline.
    if tokio::runtime::Handle::try_current().is_err() {
        return future
            .await
            .map_err(|response| (response, ErrorKind::Request));
    }
    match tokio::time::timeout(timeout, future).await {
        Ok(response) => response.map_err(|response| (response, ErrorKind::Request)),
        Err(_) => Err((subscribe_timeout_response(timeout), ErrorKind::Execution)),
    }
}

#[cfg(not(feature = "axum"))]
#[allow(clippy::result_large_err)]
async fn timed_subscribe<F>(
    _timeout: Duration,
    future: F,
) -> Result<gql::SubscriptionResponse, (gql::Response, ErrorKind)>
where
    F: std::future::Future<Output = Result<gql::SubscriptionResponse, gql::Response>>,
{
    future
        .await
        .map_err(|response| (response, ErrorKind::Request))
}

#[cfg(feature = "axum")]
async fn timed<F>(timeout: Duration, future: F) -> Option<gql::Response>
where
    F: std::future::Future<Output = gql::Response>,
{
    if tokio::runtime::Handle::try_current().is_err() {
        return Some(future.await);
    }
    tokio::time::timeout(timeout, future).await.ok()
}

#[cfg(not(feature = "axum"))]
async fn timed<F>(_timeout: Duration, future: F) -> Option<gql::Response>
where
    F: std::future::Future<Output = gql::Response>,
{
    Some(future.await)
}

fn operation_timeout_response(timeout: Duration) -> gql::Response {
    gql::Response::error(gql::GraphQLError::new(format!(
        "The operation timed out after {timeout:?}."
    )))
}

#[cfg(feature = "axum")]
fn subscribe_timeout_response(timeout: Duration) -> gql::Response {
    gql::Response::error(gql::GraphQLError::new(format!(
        "The subscription timed out while starting after {timeout:?}."
    )))
}
