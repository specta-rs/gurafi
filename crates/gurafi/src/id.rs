//! The `ID` scalar.

use std::{fmt, str::FromStr, sync::Arc};

use gql::{GraphQLError, TypeRef, Value};

use crate::traits::{FromValue, InputType, IntoValue, OutputType};

/// The GraphQL `ID` scalar.
///
/// An `ID` is serialised as a string but may be supplied as a string or an integer, exactly like
/// the specification requires. Use it for primary keys and opaque references.
///
/// ```
/// use gurafi::ID;
///
/// let id: ID = "42".parse().unwrap();
/// assert_eq!(id.to_string(), "42");
/// ```
#[derive(Clone, Debug, Default, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct ID(pub String);

impl ID {
    /// Create an ID from anything string-like.
    pub fn new(value: impl Into<String>) -> Self {
        Self(value.into())
    }

    /// Borrow the ID as a string.
    #[must_use]
    pub fn as_str(&self) -> &str {
        &self.0
    }

    /// Consume the ID into its string.
    #[must_use]
    pub fn into_string(self) -> String {
        self.0
    }
}

impl fmt::Display for ID {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.0)
    }
}

impl FromStr for ID {
    type Err = GraphQLError;

    fn from_str(value: &str) -> Result<Self, Self::Err> {
        Ok(Self(value.to_owned()))
    }
}

impl From<String> for ID {
    fn from(value: String) -> Self {
        Self(value)
    }
}

impl From<&str> for ID {
    fn from(value: &str) -> Self {
        Self(value.to_owned())
    }
}

impl From<ID> for String {
    fn from(value: ID) -> Self {
        value.0
    }
}

impl AsRef<str> for ID {
    fn as_ref(&self) -> &str {
        &self.0
    }
}

impl std::ops::Deref for ID {
    type Target = str;

    fn deref(&self) -> &Self::Target {
        &self.0
    }
}

impl OutputType for ID {
    fn output_type() -> TypeRef {
        TypeRef::id().non_null()
    }
}

impl InputType for ID {
    fn input_type() -> TypeRef {
        TypeRef::id().non_null()
    }
}

impl IntoValue for ID {
    fn into_value(self) -> Result<Value, GraphQLError> {
        Ok(Value::String(Arc::from(self.0)))
    }
}

impl FromValue for ID {
    fn from_value(value: &Value) -> Result<Self, GraphQLError> {
        match value {
            Value::String(string) | Value::Enum(string) => Ok(Self(string.to_string())),
            Value::Int(number) => Ok(Self(number.to_string())),
            other => Err(GraphQLError::new(format!(
                "`ID` cannot represent a value of kind {}.",
                other.kind()
            ))),
        }
    }
}
