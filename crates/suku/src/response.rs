//! JSON helpers for [`gql::Response`].
//!
//! `suku` does not define a second response type: the engine's [`gql::Response`] is the canonical
//! response, and this module only adds the small conveniences a transport needs. The extension
//! trait lets call sites write `response.to_json()` without shadowing the type with a wrapper.

use gql::{GraphQLError, Response};
use serde_json::{Value, json};

/// The canonical GraphQL response type.
///
/// This is an alias rather than a wrapper: `suku` never defines a second response model, so
/// `gql::Response` values produced by execution can be returned directly.
pub type GraphQLResponse = Response;

/// JSON helpers for GraphQL responses.
pub trait ResponseExt {
    /// Serialize the response to a JSON value.
    ///
    /// If serialization fails (only possible when a schema leaked an opaque model value into the
    /// response), a spec-shaped error response is returned instead of panicking.
    fn to_json(&self) -> Value;

    /// Serialize the response to a JSON string.
    fn to_json_string(&self) -> String;

    /// Serialize the response to UTF-8 JSON bytes.
    fn to_json_bytes(&self) -> Vec<u8>;
}

impl ResponseExt for Response {
    fn to_json(&self) -> Value {
        match serde_json::to_value(self) {
            Ok(value) => value,
            Err(error) => serialization_error(error),
        }
    }

    fn to_json_string(&self) -> String {
        match serde_json::to_string(self) {
            Ok(value) => value,
            Err(error) => serialization_error(error).to_string(),
        }
    }

    fn to_json_bytes(&self) -> Vec<u8> {
        self.to_json_string().into_bytes()
    }
}

/// Serialize a response to a JSON value.
#[must_use]
pub fn to_json(response: &Response) -> Value {
    response.to_json()
}

/// Serialize a response to JSON bytes.
#[must_use]
pub fn to_json_bytes(response: &Response) -> Vec<u8> {
    response.to_json_bytes()
}

/// A specification-shaped error response.
#[must_use]
pub fn error_response(message: impl Into<String>) -> Response {
    Response::error(GraphQLError::new(message.into()))
}

/// A specification-shaped error response as JSON bytes.
#[must_use]
pub fn error_json_bytes(message: impl Into<String>) -> Vec<u8> {
    error_response(message).to_json_bytes()
}

fn serialization_error(error: serde_json::Error) -> Value {
    json!({
        "errors": [{
            "message": format!("Failed to serialize the GraphQL response: {error}")
        }]
    })
}
