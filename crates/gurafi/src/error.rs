//! Schema build errors.

use core::fmt;

/// An error produced while building a schema.
///
/// All problems found while assembling the schema are collected and reported together, so one
/// `cargo run` reports every unregistered type and every invalid default rather than only the
/// first. The contained errors use the same [`gql::GraphQLError`] type the executor reports, so
/// a server can log or render them consistently.
#[derive(Clone, Debug, Default)]
pub struct SchemaError {
    errors: Vec<gql::GraphQLError>,
}

impl SchemaError {
    /// Create an error with a single message.
    #[must_use]
    pub fn new(message: impl Into<String>) -> Self {
        Self {
            errors: vec![gql::GraphQLError::new(message.into())],
        }
    }

    /// Create an error from a collection of GraphQL errors.
    #[must_use]
    pub fn from_errors(errors: Vec<gql::GraphQLError>) -> Self {
        Self { errors }
    }

    /// Whether no problems were found.
    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.errors.is_empty()
    }

    /// The number of problems.
    #[must_use]
    pub fn len(&self) -> usize {
        self.errors.len()
    }

    /// Iterate over the individual problems.
    pub fn errors(&self) -> &[gql::GraphQLError] {
        &self.errors
    }

    /// Consume the error into its individual problems.
    #[must_use]
    pub fn into_errors(self) -> Vec<gql::GraphQLError> {
        self.errors
    }
}

impl fmt::Display for SchemaError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        for (index, error) in self.errors.iter().enumerate() {
            if index != 0 {
                f.write_str("\n")?;
            }
            write!(f, "{error}")?;
        }
        Ok(())
    }
}

impl std::error::Error for SchemaError {}

impl From<gql::SchemaError> for SchemaError {
    fn from(value: gql::SchemaError) -> Self {
        Self {
            errors: value.into_errors(),
        }
    }
}

impl From<gql::GraphQLError> for SchemaError {
    fn from(value: gql::GraphQLError) -> Self {
        Self {
            errors: vec![value],
        }
    }
}
