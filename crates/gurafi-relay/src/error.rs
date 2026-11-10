//! Registration errors.

use std::{error::Error, fmt};

use gurafi::gql::GraphQLError;

/// A schema-registration error produced by the Relay helpers.
///
/// These errors describe a mistake in the Rust builder (a missing loader, a duplicate type or an
/// invalid GraphQL name), not a request-time failure. They are separate from
/// [`GraphQLError`](gurafi::gql::GraphQLError), which is what resolvers return.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct RelayError {
    message: String,
}

impl RelayError {
    /// Create a registration error with a human-readable message.
    #[must_use]
    pub fn new(message: impl Into<String>) -> Self {
        Self {
            message: message.into(),
        }
    }

    /// The error message.
    #[must_use]
    pub fn message(&self) -> &str {
        &self.message
    }
}

impl fmt::Display for RelayError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.message)
    }
}

impl Error for RelayError {}

impl From<RelayError> for GraphQLError {
    fn from(error: RelayError) -> Self {
        GraphQLError::new(error.message)
    }
}
