//! JSON helpers for [`gql::Response`].
//!
//! `suku` does not define a second response type: the engine's [`gql::Response`] is the canonical
//! response, and this module only adds the small conveniences a transport needs. The extension
//! trait lets call sites write `response.to_json()` without shadowing the type with a wrapper.

use gql::{GraphQLError, Response};
use serde_json::{Value, json};

use crate::error::INTERNAL_ERROR_MESSAGE;

/// A known-safe fallback for the degenerate case where a formatter itself returns an
/// unserializable error. Transport code must never expose a serializer diagnostic.
pub(crate) const INTERNAL_ERROR_JSON: &str = r#"{"errors":[{"message":"Internal server error"}]}"#;

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
    /// response), a generic spec-shaped error response is returned instead of panicking. Server
    /// transports use the fallible helper so they can also format the fallback and return HTTP
    /// 500.
    fn to_json(&self) -> Value;

    /// Serialize the response to a JSON string.
    fn to_json_string(&self) -> String;

    /// Serialize the response to UTF-8 JSON bytes.
    fn to_json_bytes(&self) -> Vec<u8>;
}

/// Fallible serialization for transports that must react to a late serialization failure.
pub(crate) trait FallibleResponseExt {
    /// Serialize to a JSON string without replacing the error.
    fn try_to_json_string(&self) -> serde_json::Result<String>;

    /// Serialize to UTF-8 JSON bytes without replacing the error.
    fn try_to_json_bytes(&self) -> serde_json::Result<Vec<u8>>;
}

impl FallibleResponseExt for Response {
    fn try_to_json_string(&self) -> serde_json::Result<String> {
        serde_json::to_string(self)
    }

    fn try_to_json_bytes(&self) -> serde_json::Result<Vec<u8>> {
        serde_json::to_vec(self)
    }
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

/// A fresh, generic internal error response.
pub(crate) fn internal_error_response() -> Response {
    error_response(INTERNAL_ERROR_MESSAGE)
}

/// A specification-shaped error response as JSON bytes.
#[must_use]
pub fn error_json_bytes(message: impl Into<String>) -> Vec<u8> {
    error_response(message).to_json_bytes()
}

pub(crate) fn serialization_error(_error: serde_json::Error) -> Value {
    json!({
        "errors": [{
            "message": INTERNAL_ERROR_MESSAGE
        }]
    })
}
