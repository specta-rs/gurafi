//! GraphQL errors and response shaping.
//!
//! Errors are values, not exceptions. Every error the engine reports can be turned into the
//! `errors` array described by the GraphQL specification and carries an optional source
//! location and response path.

use core::fmt;
use std::{borrow::Cow, sync::Arc};

use smallvec::SmallVec;

use crate::Value;

/// A line and column in a GraphQL source document.
///
/// Lines and columns are 1-indexed, exactly like the specification requires.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct Location {
    /// The 1-indexed line number.
    pub line: u32,
    /// The 1-indexed column number.
    pub column: u32,
}

impl Location {
    /// Construct a location.
    #[must_use]
    pub const fn new(line: u32, column: u32) -> Self {
        Self { line, column }
    }
}

impl fmt::Display for Location {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{}:{}", self.line, self.column)
    }
}

/// One segment of a response path.
///
/// Response paths point at the exact field or list index that produced an error, which is why
/// they are kept apart from source locations.
#[derive(Clone, Debug, PartialEq, Eq, Hash)]
pub enum PathSegment {
    /// An object field, named by its *response key* (the alias when one is used).
    Field(Arc<str>),
    /// A list index.
    Index(usize),
}

impl PathSegment {
    /// Build a field path segment.
    #[must_use]
    pub fn field(name: impl Into<Arc<str>>) -> Self {
        Self::Field(name.into())
    }

    /// Build an index path segment.
    #[must_use]
    pub const fn index(index: usize) -> Self {
        Self::Index(index)
    }
}

impl fmt::Display for PathSegment {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Field(name) => f.write_str(name),
            Self::Index(index) => write!(f, "{index}"),
        }
    }
}

/// A GraphQL error.
///
/// This is deliberately a plain owned value: it can cross an `await`, be stored in a response,
/// or be created by an authorization plugin without knowing anything about the executor.
#[derive(Clone, Debug, PartialEq)]
pub struct GraphQLError {
    inner: Box<GraphQLErrorInner>,
}

#[derive(Clone, Debug, PartialEq)]
struct GraphQLErrorInner {
    message: Cow<'static, str>,
    locations: SmallVec<[Location; 1]>,
    path: Option<Vec<PathSegment>>,
    extensions: Option<crate::Object>,
}

impl GraphQLError {
    /// Create an error with a message.
    #[must_use]
    pub fn new(message: impl Into<Cow<'static, str>>) -> Self {
        Self {
            inner: Box::new(GraphQLErrorInner {
                message: message.into(),
                locations: SmallVec::new(),
                path: None,
                extensions: None,
            }),
        }
    }

    /// Add a source location.
    #[must_use]
    pub fn at(mut self, location: Location) -> Self {
        self.inner.locations.push(location);
        self
    }

    /// Add a response path.
    #[must_use]
    pub fn with_path(mut self, path: Vec<PathSegment>) -> Self {
        self.inner.path = Some(path);
        self
    }

    /// Set the response path.
    pub fn set_path(&mut self, path: Vec<PathSegment>) {
        self.inner.path = Some(path);
    }

    /// Mutable access to the response path, if one is set.
    pub fn path_mut(&mut self) -> Option<&mut Vec<PathSegment>> {
        self.inner.path.as_mut()
    }

    /// Add an extension entry.
    #[must_use]
    pub fn extend(mut self, key: impl Into<Arc<str>>, value: impl Into<Value>) -> Self {
        self.inner
            .extensions
            .get_or_insert_with(crate::Object::new)
            .insert(key, value.into());
        self
    }

    /// The human-readable message.
    #[must_use]
    pub fn message(&self) -> &str {
        &self.inner.message
    }

    /// Source locations associated with this error.
    #[must_use]
    pub fn locations(&self) -> &[Location] {
        &self.inner.locations
    }

    /// The response path, when the error came from execution.
    #[must_use]
    pub fn path(&self) -> Option<&[PathSegment]> {
        self.inner.path.as_deref()
    }

    /// Error extensions, when any were attached.
    #[must_use]
    pub fn extensions(&self) -> Option<&crate::Object> {
        self.inner.extensions.as_ref()
    }

    /// Rewrite the message, useful for adding context in resolvers.
    #[must_use]
    pub fn with_message(mut self, message: impl Into<Cow<'static, str>>) -> Self {
        self.inner.message = message.into();
        self
    }
}

impl fmt::Display for GraphQLError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.inner.message)?;
        for location in &self.inner.locations {
            write!(f, " (at {location})")?;
        }
        if let Some(path) = &self.inner.path {
            f.write_str(" at path ")?;
            for (index, segment) in path.iter().enumerate() {
                if index != 0 {
                    f.write_str(".")?;
                }
                write!(f, "{segment}")?;
            }
        }
        Ok(())
    }
}

impl std::error::Error for GraphQLError {}

impl From<&str> for GraphQLError {
    fn from(value: &str) -> Self {
        Self::new(value.to_owned())
    }
}

impl From<String> for GraphQLError {
    fn from(value: String) -> Self {
        Self::new(value)
    }
}

impl From<Cow<'static, str>> for GraphQLError {
    fn from(value: Cow<'static, str>) -> Self {
        Self::new(value)
    }
}

/// Convenient alias for resolver results.
pub type Result<T, E = GraphQLError> = core::result::Result<T, E>;

/// Serialization support for errors, when the `serde` feature is enabled.
#[cfg(feature = "serde")]
impl serde::Serialize for GraphQLError {
    fn serialize<S: serde::Serializer>(
        &self,
        serializer: S,
    ) -> core::result::Result<S::Ok, S::Error> {
        use serde::ser::SerializeMap as _;

        let mut map = serializer.serialize_map(None)?;
        map.serialize_entry("message", &self.inner.message)?;
        if !self.inner.locations.is_empty() {
            map.serialize_entry("locations", &self.inner.locations)?;
        }
        if let Some(path) = &self.inner.path {
            map.serialize_entry("path", &PathRef(path))?;
        }
        if let Some(extensions) = &self.inner.extensions {
            map.serialize_entry("extensions", extensions)?;
        }
        map.end()
    }
}

#[cfg(feature = "serde")]
impl serde::Serialize for Location {
    fn serialize<S: serde::Serializer>(
        &self,
        serializer: S,
    ) -> core::result::Result<S::Ok, S::Error> {
        use serde::ser::SerializeMap as _;

        let mut map = serializer.serialize_map(Some(2))?;
        map.serialize_entry("line", &self.line)?;
        map.serialize_entry("column", &self.column)?;
        map.end()
    }
}

#[cfg(feature = "serde")]
struct PathRef<'a>(&'a [PathSegment]);

#[cfg(feature = "serde")]
impl serde::Serialize for PathRef<'_> {
    fn serialize<S: serde::Serializer>(
        &self,
        serializer: S,
    ) -> core::result::Result<S::Ok, S::Error> {
        use serde::ser::SerializeSeq as _;

        let mut seq = serializer.serialize_seq(Some(self.0.len()))?;
        for segment in self.0 {
            match segment {
                PathSegment::Field(name) => seq.serialize_element(&**name)?,
                PathSegment::Index(index) => seq.serialize_element(index)?,
            }
        }
        seq.end()
    }
}

/// A successful or partial GraphQL response.
#[derive(Clone, Debug, Default, PartialEq)]
pub struct Response {
    /// The result data. `None` means execution produced no data at all, which is different from
    /// an explicit `null` value.
    pub data: Option<Value>,
    /// Errors encountered while validating or executing the request.
    pub errors: Vec<GraphQLError>,
    /// Optional response extensions.
    pub extensions: Option<crate::Object>,
}

impl Response {
    /// A response containing only one error.
    #[must_use]
    pub fn error(error: GraphQLError) -> Self {
        Self {
            data: None,
            errors: vec![error],
            extensions: None,
        }
    }

    /// A response with data and no errors.
    #[must_use]
    pub fn data(data: Value) -> Self {
        Self {
            data: Some(data),
            errors: Vec::new(),
            extensions: None,
        }
    }

    /// Whether the response has any errors.
    #[must_use]
    pub fn has_errors(&self) -> bool {
        !self.errors.is_empty()
    }
}

#[cfg(feature = "serde")]
impl serde::Serialize for Response {
    fn serialize<S: serde::Serializer>(
        &self,
        serializer: S,
    ) -> core::result::Result<S::Ok, S::Error> {
        use serde::ser::SerializeMap as _;

        let len = usize::from(self.data.is_some())
            + usize::from(!self.errors.is_empty())
            + usize::from(self.extensions.is_some());
        let mut map = serializer.serialize_map(Some(len))?;
        if let Some(data) = &self.data {
            map.serialize_entry("data", data)?;
        }
        if !self.errors.is_empty() {
            map.serialize_entry("errors", &self.errors)?;
        }
        if let Some(extensions) = &self.extensions {
            map.serialize_entry("extensions", extensions)?;
        }
        map.end()
    }
}
