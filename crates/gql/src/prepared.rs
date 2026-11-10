//! Prepared (persisted) query support.
//!
//! A [`PreparedQuery`] parses, validates and selects an operation once, then keeps the source and
//! its syntax tree alive for the lifetime of the value. Executing it skips parsing, validation and
//! operation selection entirely, while still applying per-request variables, complexity limits,
//! introspection policy, authorization, timeouts and cancellation.
//!
//! ```no_run
//! # use gql::{ExecutionOptions, Schema};
//! # fn run(schema: &Schema) {
//! # let source = "{ hello }";
//! let prepared = gql::prepared::PreparedQuery::new(
//!     schema,
//!     "hello",
//!     source,
//!     None,
//! )
//! .expect("valid query");
//!
//! # futures::executor::block_on(async {
//! let response = prepared.execute(None, &ExecutionOptions::default()).await;
//! # let _ = response;
//! # });
//! # }
//! ```
//!
//! The source and AST are owned through a self-referential cell. No `unsafe` code is written in
//! this crate: `self_cell` provides the safe wrapper.
//!
//! # Prepared versus normal execution
//!
//! [`crate::execute`] always parses and validates the document. That is the right default for
//! arbitrary client-supplied documents. Once a document is trusted (for example, a persisted
//! operation loaded from a build-time manifest), preparing it moves that work to startup and
//! produces a value that can be cloned, shared and executed concurrently.

use std::{fmt, sync::Arc, time::Duration};

use futures::FutureExt as _;
use rustc_hash::FxHashMap;
use self_cell::self_cell;

use crate::{
    Object,
    ast::OperationKind,
    error::{GraphQLError, Response},
    execution::{self, ExecutionContext, ExecutionOptions, PreparedDocument, SubscriptionResponse},
    schema::Schema,
    types::ObjectType,
};

self_cell!(
    struct PreparedQueryCell {
        owner: Arc<str>,

        #[covariant]
        dependent: PreparedDocument,
    }
);

/// An error produced while preparing a query.
#[derive(Clone, Debug, PartialEq)]
pub enum PreparedQueryError {
    /// The source could not be parsed.
    Parse(GraphQLError),
    /// The document parsed but failed validation.
    Validation(Vec<GraphQLError>),
    /// The document is valid but the selected operation is unusable against this schema.
    Operation(GraphQLError),
}

impl PreparedQueryError {
    /// The GraphQL errors behind this preparation failure.
    #[must_use]
    pub fn errors(&self) -> Vec<GraphQLError> {
        match self {
            Self::Parse(error) | Self::Operation(error) => vec![error.clone()],
            Self::Validation(errors) => errors.clone(),
        }
    }
}

impl fmt::Display for PreparedQueryError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Parse(error) => write!(f, "failed to parse query: {error}"),
            Self::Operation(error) => write!(f, "invalid operation: {error}"),
            Self::Validation(errors) => {
                f.write_str("query failed validation")?;
                for error in errors {
                    write!(f, ": {error}")?;
                }
                Ok(())
            }
        }
    }
}

impl std::error::Error for PreparedQueryError {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        match self {
            Self::Parse(error) | Self::Operation(error) => Some(error),
            Self::Validation(_) => None,
        }
    }
}

impl From<GraphQLError> for PreparedQueryError {
    fn from(value: GraphQLError) -> Self {
        Self::Operation(value)
    }
}

struct PreparedQueryInner {
    id: Arc<str>,
    schema: Schema,
    operation_name: Option<Arc<str>>,
    kind: OperationKind,
    root_type: Arc<ObjectType>,
    cell: PreparedQueryCell,
}

/// A query that has already been parsed, validated and selected.
///
/// Cloning is cheap: every prepared query is one shared allocation, so `PreparedQuery` is
/// `Send + Sync` and can be executed concurrently from any number of tasks.
#[derive(Clone)]
pub struct PreparedQuery {
    inner: Arc<PreparedQueryInner>,
}

impl fmt::Debug for PreparedQuery {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("PreparedQuery")
            .field("id", &self.inner.id)
            .field("operation_name", &self.inner.operation_name)
            .field("kind", &self.inner.kind)
            .field("root_type", &self.inner.root_type.name)
            .field("source_len", &self.inner.cell.borrow_owner().len())
            .finish()
    }
}

impl PreparedQuery {
    /// Prepare a query under an identifier.
    ///
    /// Parsing, validation, operation selection and the root-type check all happen here.
    ///
    /// # Errors
    ///
    /// Returns [`PreparedQueryError`] when the source is malformed, invalid against the schema, or
    /// names an operation that does not exist.
    pub fn new(
        schema: &Schema,
        id: impl Into<Arc<str>>,
        source: &str,
        operation_name: Option<&str>,
    ) -> Result<Self, PreparedQueryError> {
        Self::prepare(schema, id, source, operation_name)
    }

    /// Prepare a query under an identifier.
    ///
    /// This is an alias for [`PreparedQuery::new`].
    ///
    /// # Errors
    ///
    /// Returns [`PreparedQueryError`] when the source cannot be prepared.
    pub fn prepare(
        schema: &Schema,
        id: impl Into<Arc<str>>,
        source: &str,
        operation_name: Option<&str>,
    ) -> Result<Self, PreparedQueryError> {
        let owner: Arc<str> = Arc::from(source);
        let cell = PreparedQueryCell::try_new(owner, |owner| {
            execution::build_prepared(schema, owner.as_ref(), operation_name).map_err(|error| {
                match error {
                    execution::BuildPreparedError::Parse(error) => PreparedQueryError::Parse(error),
                    execution::BuildPreparedError::Validation(errors) => {
                        PreparedQueryError::Validation(errors)
                    }
                    execution::BuildPreparedError::Operation(error) => {
                        PreparedQueryError::Operation(error)
                    }
                }
            })
        })?;

        let (operation_name, kind) = cell.with_dependent(|_, prepared| {
            let Some(operation) = prepared.operation() else {
                return Err(PreparedQueryError::Operation(GraphQLError::new(
                    "Must provide an operation.",
                )));
            };
            Ok((operation.name.map(Arc::from), operation.kind))
        })?;
        let root_type =
            execution::prepared_root_type(schema, kind).map_err(PreparedQueryError::from)?;

        Ok(Self {
            inner: Arc::new(PreparedQueryInner {
                id: id.into(),
                schema: schema.clone(),
                operation_name,
                kind,
                root_type,
                cell,
            }),
        })
    }

    /// The identifier this query was prepared under.
    #[must_use]
    pub fn id(&self) -> &str {
        &self.inner.id
    }

    /// The original GraphQL source.
    #[must_use]
    pub fn source(&self) -> &str {
        self.inner.cell.borrow_owner()
    }

    /// The selected operation's name, if it has one.
    #[must_use]
    pub fn operation_name(&self) -> Option<&str> {
        self.inner.operation_name.as_deref()
    }

    /// Whether the selected operation is a query, mutation or subscription.
    #[must_use]
    pub fn kind(&self) -> OperationKind {
        self.inner.kind
    }

    /// The schema root type this operation executes against.
    #[must_use]
    pub fn root_type(&self) -> &ObjectType {
        &self.inner.root_type
    }

    /// The schema this query was prepared against.
    #[must_use]
    pub fn schema(&self) -> &Schema {
        &self.inner.schema
    }

    /// Execute the prepared query.
    ///
    /// Variables, complexity limits, introspection policy, authorization, timeouts and the root
    /// value all come from `options`, exactly as they do for [`crate::execute`]. Only parsing,
    /// validation and operation selection are skipped.
    ///
    /// Executing a prepared subscription returns the same error normal execution does: use
    /// [`PreparedQuery::subscribe`] instead.
    pub async fn execute(
        &self,
        variables: Option<&Object>,
        options: &ExecutionOptions,
    ) -> Response {
        let prepared = self.inner.cell.borrow_dependent();
        execution::execute_prepared(&self.inner.schema, prepared, variables, options).await
    }

    /// Start a prepared subscription.
    ///
    /// Setup reuses the parsed, validated document, and each source event reuses it too, so a
    /// prepared subscription does not re-parse per event the way normal subscriptions do.
    ///
    /// # Errors
    ///
    /// Returns a fully formed error response when the prepared operation is not a subscription or
    /// the source stream could not be created.
    #[allow(clippy::result_large_err)]
    pub async fn subscribe(
        &self,
        variables: Option<&Object>,
        options: &ExecutionOptions,
    ) -> Result<SubscriptionResponse, Response> {
        let inner = Arc::clone(&self.inner);
        let prepared = self.inner.cell.borrow_dependent();
        execution::subscribe_prepared(
            &self.inner.schema,
            prepared,
            variables,
            options,
            move |_context, timeout| prepared_event_handler(inner, timeout),
        )
        .await
    }
}

/// Build the per-event executor for a prepared subscription.
fn prepared_event_handler(
    inner: Arc<PreparedQueryInner>,
    timeout: Option<Duration>,
) -> execution::subscription::EventHandler {
    Arc::new(move |context: Arc<ExecutionContext>, event: crate::Value| {
        let inner = Arc::clone(&inner);
        async move {
            let deadline = timeout.map(|timeout| std::time::Instant::now() + timeout);
            let prepared = inner.cell.borrow_dependent();
            execution::execute_prepared_event(&context, prepared, event, deadline).await
        }
        .boxed()
    })
}

/// Execute a prepared query.
///
/// This is the free-function form of [`PreparedQuery::execute`].
pub async fn execute_prepared(
    query: &PreparedQuery,
    variables: Option<&Object>,
    options: &ExecutionOptions,
) -> Response {
    query.execute(variables, options).await
}

/// An error produced while building a [`PreparedQueryRegistry`].
#[derive(Clone, Debug, PartialEq)]
pub enum PreparedQueryRegistryError {
    /// An identifier was empty.
    EmptyId,
    /// Two entries used the same identifier.
    DuplicateId {
        /// The identifier that appeared more than once.
        id: Arc<str>,
    },
    /// One entry could not be prepared.
    Query {
        /// The identifier of the failing entry.
        id: Arc<str>,
        /// Why preparation failed.
        error: PreparedQueryError,
    },
}

impl fmt::Display for PreparedQueryRegistryError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::EmptyId => f.write_str("a prepared query must have a non-empty id"),
            Self::DuplicateId { id } => write!(f, "duplicate prepared query id \"{id}\""),
            Self::Query { id, error } => write!(f, "prepared query \"{id}\" is invalid: {error}"),
        }
    }
}

impl std::error::Error for PreparedQueryRegistryError {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        match self {
            Self::Query { error, .. } => Some(error),
            Self::EmptyId | Self::DuplicateId { .. } => None,
        }
    }
}

struct PreparedQueryRegistryInner {
    queries: FxHashMap<Arc<str>, PreparedQuery>,
    ids: Vec<Arc<str>>,
}

impl Clone for PreparedQueryRegistryInner {
    fn clone(&self) -> Self {
        Self {
            queries: self.queries.clone(),
            ids: self.ids.clone(),
        }
    }
}

/// A validated collection of prepared queries.
///
/// The registry is cheap to clone and share: all entries live behind one `Arc`. The insertion
/// order is preserved by [`PreparedQueryRegistry::ids`].
#[derive(Clone)]
pub struct PreparedQueryRegistry {
    inner: Arc<PreparedQueryRegistryInner>,
}

impl fmt::Debug for PreparedQueryRegistry {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("PreparedQueryRegistry")
            .field("len", &self.inner.queries.len())
            .finish()
    }
}

impl Default for PreparedQueryRegistry {
    fn default() -> Self {
        Self::empty()
    }
}

impl PreparedQueryRegistry {
    /// Build a registry from `(id, source, operation_name)` entries.
    ///
    /// # Errors
    ///
    /// Returns an error for an empty or duplicate identifier, or for the first entry that cannot
    /// be prepared.
    pub fn new<I, Id, S>(schema: &Schema, queries: I) -> Result<Self, PreparedQueryRegistryError>
    where
        I: IntoIterator<Item = (Id, S, Option<S>)>,
        Id: AsRef<str>,
        S: AsRef<str>,
    {
        let mut inner = PreparedQueryRegistryInner {
            queries: FxHashMap::default(),
            ids: Vec::new(),
        };
        for (id, source, operation_name) in queries {
            let id: Arc<str> = Arc::from(id.as_ref());
            if id.is_empty() {
                return Err(PreparedQueryRegistryError::EmptyId);
            }
            if inner.queries.contains_key(&id) {
                return Err(PreparedQueryRegistryError::DuplicateId { id });
            }
            let query = PreparedQuery::new(
                schema,
                Arc::clone(&id),
                source.as_ref(),
                operation_name.as_ref().map(AsRef::as_ref),
            )
            .map_err(|error| PreparedQueryRegistryError::Query {
                id: Arc::clone(&id),
                error,
            })?;
            inner.ids.push(Arc::clone(&id));
            inner.queries.insert(id, query);
        }
        Ok(Self {
            inner: Arc::new(inner),
        })
    }

    /// An empty registry.
    #[must_use]
    pub fn empty() -> Self {
        Self {
            inner: Arc::new(PreparedQueryRegistryInner {
                queries: FxHashMap::default(),
                ids: Vec::new(),
            }),
        }
    }

    /// Insert an already prepared query.
    ///
    /// # Errors
    ///
    /// Returns an error when an entry with the same identifier already exists.
    pub fn insert(&mut self, query: PreparedQuery) -> Result<(), PreparedQueryRegistryError> {
        if query.id().is_empty() {
            return Err(PreparedQueryRegistryError::EmptyId);
        }
        let inner = Arc::make_mut(&mut self.inner);
        let id = Arc::from(query.id());
        if inner.queries.contains_key(&id) {
            return Err(PreparedQueryRegistryError::DuplicateId { id });
        }
        inner.ids.push(Arc::clone(&id));
        inner.queries.insert(id, query);
        Ok(())
    }

    /// Look up a prepared query by identifier.
    #[must_use]
    pub fn get(&self, id: &str) -> Option<&PreparedQuery> {
        self.inner.queries.get(id)
    }

    /// Whether the registry contains an identifier.
    #[must_use]
    pub fn contains(&self, id: &str) -> bool {
        self.inner.queries.contains_key(id)
    }

    /// The number of prepared queries.
    #[must_use]
    pub fn len(&self) -> usize {
        self.inner.queries.len()
    }

    /// Whether the registry is empty.
    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.inner.queries.is_empty()
    }

    /// Identifiers in insertion order.
    pub fn ids(&self) -> impl ExactSizeIterator<Item = &str> {
        self.inner.ids.iter().map(AsRef::as_ref)
    }

    /// Prepared queries in insertion order.
    pub fn iter(&self) -> impl Iterator<Item = (&str, &PreparedQuery)> {
        self.inner
            .ids
            .iter()
            .filter_map(|id| self.inner.queries.get(id).map(|query| (id.as_ref(), query)))
    }

    /// Execute a registry entry, or return `None` when the identifier is unknown.
    pub async fn execute(
        &self,
        id: &str,
        variables: Option<&Object>,
        options: &ExecutionOptions,
    ) -> Option<Response> {
        let query = self.get(id)?;
        Some(query.execute(variables, options).await)
    }
}
