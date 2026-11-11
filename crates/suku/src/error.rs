//! Error formatting and masking for responses.
//!
//! A [`GraphQLError`] that reaches a client is not always safe to show: a resolver may put a
//! database error, a filesystem path or an internal type name in its message. [`Server`] does not
//! guess which messages are safe; it exposes [`Server::format_error`], which runs over every error
//! in every response just before the response is returned or streamed.
//!
//! The formatter is called once per error, synchronously, in response order. It receives the
//! original error and an [`ErrorContext`] describing where the error came from. Returning a new
//! [`GraphQLError`] replaces the error; the formatter can rewrite the message while keeping the
//! `extensions.code`, source locations and response path intact.
//!
//! ```no_run
//! use suku::{Server, mask_internal};
//!
//! # fn build(schema: gql::Schema) {
//! let server = Server::new(schema).format_error(mask_internal);
//! # let _ = server;
//! # }
//! ```
//!
//! [`Server`]: crate::Server
//! [`Server::format_error`]: crate::Server::format_error

use std::sync::Arc;

use gql::{GraphQLError, PathSegment};

/// The message used by [`mask_internal`] for errors that are not safe to expose.
pub const INTERNAL_ERROR_MESSAGE: &str = "Internal server error";

/// Whether an error happened before execution or during it.
///
/// A request error is produced while interpreting the request: malformed variables, a parse
/// error, a validation error, variable coercion, an unknown persisted identifier and similar
/// failures. An execution error is produced by a valid operation: a resolver, an authorization
/// plugin, a subscription event or a timeout.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum ErrorKind {
    /// The error was produced before any field resolver ran.
    Request,
    /// The error was produced while executing a valid operation.
    Execution,
}

impl ErrorKind {
    /// Whether this is a request error.
    #[must_use]
    pub const fn is_request(self) -> bool {
        matches!(self, Self::Request)
    }

    /// Whether this is an execution error.
    #[must_use]
    pub const fn is_execution(self) -> bool {
        matches!(self, Self::Execution)
    }
}

/// Context handed to an error formatter.
///
/// It deliberately contains only request-scoped values that a formatter may already see: the
/// phase, the operation name, the persisted-query identifier and the response path. It never
/// exposes the schema, the resolver value, the user context or any other server internals.
#[derive(Clone, Copy, Debug)]
pub struct ErrorContext<'a> {
    kind: ErrorKind,
    operation_name: Option<&'a str>,
    request_id: Option<&'a str>,
    path: Option<&'a [PathSegment]>,
}

impl<'a> ErrorContext<'a> {
    pub(crate) fn new(
        kind: ErrorKind,
        operation_name: Option<&'a str>,
        request_id: Option<&'a str>,
        path: Option<&'a [PathSegment]>,
    ) -> Self {
        Self {
            kind,
            operation_name,
            request_id,
            path,
        }
    }

    /// Whether the error happened before execution or during it.
    #[must_use]
    pub const fn kind(&self) -> ErrorKind {
        self.kind
    }

    /// Whether the error was produced before any field resolver ran.
    #[must_use]
    pub fn is_request(&self) -> bool {
        self.kind.is_request()
    }

    /// Whether the error was produced while executing a valid operation.
    #[must_use]
    pub fn is_execution(&self) -> bool {
        self.kind.is_execution()
    }

    /// The operation named by the request, or the prepared operation's name when it has one.
    #[must_use]
    pub const fn operation_name(&self) -> Option<&'a str> {
        self.operation_name
    }

    /// The persisted-query or prepared-query identifier carried by the request.
    ///
    /// This is `None` for a normal request that sends its query text.
    #[must_use]
    pub const fn request_id(&self) -> Option<&'a str> {
        self.request_id
    }

    /// The prepared-query identifier carried by the request.
    ///
    /// This is an alias for [`ErrorContext::request_id`]: suku does not mint request ids, so the
    /// only identifier available at this point is the persisted/prepared one.
    #[must_use]
    pub const fn prepared_id(&self) -> Option<&'a str> {
        self.request_id
    }

    /// The response path attached to the error, when it has one.
    ///
    /// Request errors normally have no path. Execution errors normally do; the path identifies
    /// the field or list index in the response that failed.
    #[must_use]
    pub const fn path(&self) -> Option<&'a [PathSegment]> {
        self.path
    }
}

/// A function that rewrites errors before they are returned to a client.
///
/// A blanket implementation exists for closures of the shape
/// `Fn(&GraphQLError, &ErrorContext<'_>) -> GraphQLError + Send + Sync + 'static`, so most servers
/// use [`Server::format_error`] and never name this trait. Implement it directly when the
/// formatter needs a named type, and install that type with
/// [`Server::format_error_with`].
///
/// Implementations must be thread-safe (`Send + Sync`) and deterministic: the same error and
/// context must always produce the same result. The trait does not provide interior mutability,
/// so a formatter that records state has to do so explicitly with a thread-safe container.
///
/// A panic inside a formatter is not caught. Unlike a panic in a resolver, which `gql` converts
/// into a coded field error, it unwinds through the executing future; for a subscription event it
/// ends the stream. A formatter should therefore avoid `unwrap` and other panic paths.
///
/// [`Server::format_error`]: crate::Server::format_error
/// [`Server::format_error_with`]: crate::Server::format_error_with
pub trait ErrorFormatter: Send + Sync + 'static {
    /// Rewrite one error.
    fn format(&self, error: &GraphQLError, context: &ErrorContext<'_>) -> GraphQLError;
}

impl<F> ErrorFormatter for F
where
    F: for<'a> Fn(&GraphQLError, &ErrorContext<'a>) -> GraphQLError + Send + Sync + 'static,
{
    fn format(&self, error: &GraphQLError, context: &ErrorContext<'_>) -> GraphQLError {
        self(error, context)
    }
}

/// Mask messages that are not intentionally client-facing.
///
/// The rules are explicit:
///
/// * An error carrying an `extensions.code` is left untouched. A code marks an error the
///   application deliberately classified, so its message is considered safe and the code is kept
///   for clients to branch on.
/// * A request error ([`ErrorContext::is_request`]) is left untouched. Parse, validation and
///   variable-coercion messages describe the caller's document, not server internals.
/// * Any other error (an execution error without a code) is replaced by a fresh error with the
///   message [`INTERNAL_ERROR_MESSAGE`]. Source locations and the response path are preserved.
///   Extensions are **removed**: resolvers commonly put stack traces, backend hostnames or other
///   diagnostics there, and preserving them would defeat the mask.
///
/// This is a conservative default, not a complete security policy: a resolver that attaches an
/// `extensions.code` to an internal error opts that message (and its extensions) back in. Prefer
/// attaching codes only to errors that are intentionally safe to expose. `gurafi` attaches
/// `UNAUTHORIZED` and `BAD_USER_INPUT` to its own client-facing errors, so those survive.
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
pub fn mask_internal(error: &GraphQLError, context: &ErrorContext<'_>) -> GraphQLError {
    if context.is_request() || has_code(error) {
        return error.clone();
    }
    let mut masked = GraphQLError::new(INTERNAL_ERROR_MESSAGE);
    for location in error.locations() {
        masked = masked.at(*location);
    }
    if let Some(path) = error.path() {
        masked = masked.with_path(path.to_vec());
    }
    masked
}

fn has_code(error: &GraphQLError) -> bool {
    error
        .extensions()
        .and_then(|extensions| extensions.get("code"))
        .is_some()
}

/// Apply a formatter to every error in a response.
pub(crate) fn format_response_errors(
    formatter: &dyn ErrorFormatter,
    response: &mut gql::Response,
    operation_name: Option<&str>,
    request_id: Option<&str>,
    default_kind: ErrorKind,
) {
    for error in &mut response.errors {
        // A response path is the strongest signal that a field was resolved. Responses from
        // `gql::execute` mix request errors (parse, validation, coercion) with execution errors
        // in the same `errors` array, so classify each error instead of the whole response.
        let kind = if error.path().is_some() {
            ErrorKind::Execution
        } else {
            default_kind
        };
        let context = ErrorContext::new(kind, operation_name, request_id, error.path());
        *error = formatter.format(error, &context);
    }
}

/// Internal request metadata shared by the server and transport.
#[derive(Clone, Copy, Debug, Default)]
pub(crate) struct ResponseMeta<'a> {
    pub(crate) operation_name: Option<&'a str>,
    pub(crate) request_id: Option<&'a str>,
}

impl<'a> ResponseMeta<'a> {
    pub(crate) fn from_request(request: &'a crate::GraphQLRequest) -> Self {
        Self {
            operation_name: request.operation_name.as_deref(),
            request_id: request.prepared_id(),
        }
    }

    pub(crate) const fn none() -> Self {
        Self {
            operation_name: None,
            request_id: None,
        }
    }

    pub(crate) fn with_operation_name(mut self, operation_name: Option<&'a str>) -> Self {
        self.operation_name = operation_name.or(self.operation_name);
        self
    }

    pub(crate) fn into_owned(self) -> OwnedResponseMeta {
        OwnedResponseMeta {
            operation_name: self.operation_name.map(Arc::from),
            request_id: self.request_id.map(Arc::from),
        }
    }
}

/// Owned request metadata for a `'static` subscription stream.
#[derive(Clone, Debug, Default)]
pub(crate) struct OwnedResponseMeta {
    pub(crate) operation_name: Option<Arc<str>>,
    pub(crate) request_id: Option<Arc<str>>,
}
