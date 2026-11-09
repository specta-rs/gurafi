//! GraphQL request parsing and JSON conversion.
//!
//! A [`GraphQLRequest`] is the body of a GraphQL-over-HTTP request: a query document, an optional
//! operation name, variables and extensions. The type deliberately mirrors the JSON payload one
//! field for field so that `serde` reports useful errors for malformed clients.
//!
//! `gql` does not depend on `serde_json`; the conversion from JSON values to [`gql::Value`] lives
//! here, at the transport boundary.

use core::fmt;

use gql::{GraphQLError, Object, Value};
use serde::{Deserialize, Serialize};
use serde_json::{Map, Value as JsonValue};

/// A GraphQL-over-HTTP request body.
///
/// ```json
/// {
///   "query": "query Greeting($name: String!) { hello(name: $name) }",
///   "operationName": "Greeting",
///   "variables": { "name": "Ada" },
///   "extensions": { "trace": true }
/// }
/// ```
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct GraphQLRequest {
    /// The GraphQL document to execute.
    pub query: String,
    /// Selects one operation when the document contains several.
    #[serde(
        rename = "operationName",
        default,
        skip_serializing_if = "Option::is_none"
    )]
    pub operation_name: Option<String>,
    /// JSON variable values. Converted to [`gql::Object`] just before execution.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub variables: Option<Map<String, JsonValue>>,
    /// Free-form request extensions, reserved for things like persisted queries.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub extensions: Option<Map<String, JsonValue>>,
}

impl GraphQLRequest {
    /// Create a request for a query with no variables.
    #[must_use]
    pub fn new(query: impl Into<String>) -> Self {
        Self {
            query: query.into(),
            operation_name: None,
            variables: None,
            extensions: None,
        }
    }

    /// Parse one request from JSON bytes.
    ///
    /// # Errors
    ///
    /// Returns a request error for malformed JSON or a missing `query` field.
    pub fn from_json_slice(body: &[u8]) -> Result<Self, RequestError> {
        serde_json::from_slice(body)
            .map_err(|error| RequestError::new(format!("Invalid GraphQL request: {error}")))
    }

    /// Parse one request from a JSON string.
    ///
    /// # Errors
    ///
    /// Returns a request error for malformed JSON or a missing `query` field.
    pub fn from_json_str(body: &str) -> Result<Self, RequestError> {
        serde_json::from_str(body)
            .map_err(|error| RequestError::new(format!("Invalid GraphQL request: {error}")))
    }

    /// Select an operation by name.
    #[must_use]
    pub fn operation_name(mut self, name: impl Into<String>) -> Self {
        self.operation_name = Some(name.into());
        self
    }

    /// Attach JSON variable values.
    #[must_use]
    pub fn variables(mut self, variables: Map<String, JsonValue>) -> Self {
        self.variables = Some(variables);
        self
    }

    /// Convert the JSON variables into the GraphQL value model.
    ///
    /// # Errors
    ///
    /// Returns a request error when a variable cannot be represented by the GraphQL input value
    /// model. The only lossy case in JSON is an integer that does not fit in GraphQL's 32-bit
    /// `Int`.
    pub fn variables_object(&self) -> Result<Option<Object>, RequestError> {
        self.variables
            .as_ref()
            .map(|variables| variables_to_object(variables, ""))
            .transpose()
    }

    /// Build the borrowed request type used by [`gql::execute`].
    ///
    /// The returned request borrows the query and operation name. Variable values are stored in
    /// the supplied object, which must outlive the request; callers normally keep both values in
    /// the same function scope.
    ///
    /// # Errors
    ///
    /// Returns a request error when variables cannot be converted.
    pub fn as_gql<'a>(&'a self, variables: Option<&'a Object>) -> gql::Request<'a> {
        gql::Request {
            query: &self.query,
            operation_name: self.operation_name.as_deref(),
            variables,
        }
    }
}

/// A request body, which is either a single operation or a batch.
#[derive(Clone, Debug, PartialEq)]
pub enum RequestPayload {
    /// One GraphQL operation.
    Single(GraphQLRequest),
    /// A JSON array of operations, executed concurrently.
    Batch(Vec<GraphQLRequest>),
}

impl RequestPayload {
    /// Parse a JSON request body.
    ///
    /// A leading `[` selects batch parsing. Empty batches and batches larger than
    /// `max_batch_size` are rejected. Passing `0` for `max_batch_size` disables the size limit.
    ///
    /// # Errors
    ///
    /// Returns a request error for malformed JSON, an invalid payload shape, or a batch that
    /// violates the configured limits.
    pub fn from_json_slice(body: &[u8], max_batch_size: usize) -> Result<Self, RequestError> {
        let is_batch = body
            .iter()
            .copied()
            .find(|byte| !byte.is_ascii_whitespace())
            .is_some_and(|byte| byte == b'[');

        if is_batch {
            let batch: Vec<GraphQLRequest> = serde_json::from_slice(body)
                .map_err(|error| RequestError::new(format!("Invalid batch request: {error}")))?;
            if batch.is_empty() {
                return Err(RequestError::new(
                    "A batch request must contain at least one operation.",
                ));
            }
            if max_batch_size != 0 && batch.len() > max_batch_size {
                return Err(RequestError::new(format!(
                    "A batch request may contain at most {max_batch_size} operations."
                )));
            }
            Ok(Self::Batch(batch))
        } else {
            let request: GraphQLRequest = serde_json::from_slice(body)
                .map_err(|error| RequestError::new(format!("Invalid GraphQL request: {error}")))?;
            Ok(Self::Single(request))
        }
    }

    /// The requests in this payload.
    #[must_use]
    pub fn requests(&self) -> &[GraphQLRequest] {
        match self {
            Self::Single(request) => core::slice::from_ref(request),
            Self::Batch(requests) => requests,
        }
    }

    /// Whether this is a batch payload.
    #[must_use]
    pub const fn is_batch(&self) -> bool {
        matches!(self, Self::Batch(_))
    }
}

/// An error that occurred before or while interpreting a request.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct RequestError {
    message: String,
}

impl RequestError {
    /// Create a request error with a message that is safe to show to clients.
    #[must_use]
    pub fn new(message: impl Into<String>) -> Self {
        Self {
            message: message.into(),
        }
    }

    /// The human-readable message.
    #[must_use]
    pub fn message(&self) -> &str {
        &self.message
    }

    /// Convert the error into a specification-shaped GraphQL response.
    #[must_use]
    pub fn into_response(self) -> gql::Response {
        gql::Response::error(GraphQLError::new(self.message))
    }
}

impl fmt::Display for RequestError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.message)
    }
}

impl std::error::Error for RequestError {}

impl From<GraphQLError> for RequestError {
    fn from(value: GraphQLError) -> Self {
        Self::new(value.message().to_owned())
    }
}

fn variables_to_object(
    variables: &Map<String, JsonValue>,
    path: &str,
) -> Result<Object, RequestError> {
    let mut object = Object::with_capacity(variables.len());
    for (key, value) in variables {
        object.push_unchecked(
            key.as_str(),
            json_to_graphql(value, &format!("{path}.{key}"))?,
        );
    }
    Ok(object)
}

/// Convert a JSON value into the GraphQL value model.
///
/// GraphQL's built-in `Int` is 32-bit. A JSON integer outside that range cannot be represented
/// without guessing at a custom scalar, so it is reported as a request error rather than silently
/// truncated or turned into a lossy float.
///
/// # Errors
///
/// Returns an error for integers outside the `Int` range.
pub fn json_to_graphql(value: &JsonValue, path: &str) -> Result<Value, RequestError> {
    Ok(match value {
        JsonValue::Null => Value::Null,
        JsonValue::Bool(value) => Value::Boolean(*value),
        JsonValue::Number(number) => {
            if let Some(number) = number.as_i64() {
                Value::Int(i32::try_from(number).map_err(|_| int_range_error(path))?)
            } else if let Some(number) = number.as_u64() {
                Value::Int(i32::try_from(number).map_err(|_| int_range_error(path))?)
            } else if let Some(number) = number.as_f64() {
                Value::Float(number)
            } else {
                return Err(RequestError::new(format!(
                    "Unsupported number at `{path}`."
                )));
            }
        }
        JsonValue::String(value) => Value::String(value.as_str().into()),
        JsonValue::Array(values) => {
            let mut list = Vec::with_capacity(values.len());
            for (index, value) in values.iter().enumerate() {
                list.push(json_to_graphql(value, &format!("{path}[{index}]"))?);
            }
            Value::List(list)
        }
        JsonValue::Object(values) => {
            Value::Object(std::sync::Arc::new(variables_to_object(values, path)?))
        }
    })
}

fn int_range_error(_path: &str) -> RequestError {
    RequestError::new("Integer variable out of Int range; use a custom scalar.")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_operation_name_alias() {
        let request: GraphQLRequest =
            serde_json::from_str(r#"{"query":"{ hello }","operationName":"Greeting"}"#)
                .expect("valid request");
        assert_eq!(request.operation_name.as_deref(), Some("Greeting"));
    }

    #[test]
    fn rejects_out_of_range_integers() {
        let request: GraphQLRequest =
            serde_json::from_str(r#"{"query":"{ echo(n: 1) }","variables":{"n":2147483648}}"#)
                .expect("valid request");
        let error = request.variables_object().expect_err("out of range");
        assert!(error.message().contains("Int range"));
    }

    #[test]
    fn rejects_empty_batches() {
        let error = RequestPayload::from_json_slice(b"[]", 10).expect_err("empty batch");
        assert!(error.message().contains("at least one"));
    }
}
