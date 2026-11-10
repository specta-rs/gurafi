//! Relay global IDs.
//!
//! A Relay global ID is an opaque string that identifies an object across the whole schema.
//! [`GlobalId<T>`] encodes it as `Base64("TypeName:localId")`; the Rust model in the type
//! parameter validates the type name on input, so one object's ID cannot be passed where another
//! object is expected.

use std::{fmt, marker::PhantomData, sync::Arc};

use gurafi::{
    FromValue, InputType, IntoValue, ObjectModel, OutputType,
    gql::{GraphQLError, TypeRef, Value},
};

use crate::base64;

/// A Rust model that can be addressed by a Relay global ID.
///
/// The blanket implementation uses [`ObjectModel::NAME`] as the encoded type name. Because it
/// covers every `ObjectModel`, a model cannot override the embedded type name through this trait;
/// give the model a distinct GraphQL name if it must change.
pub trait GlobalIdType: ObjectModel {
    /// The type name embedded in encoded global IDs.
    fn global_typename() -> &'static str {
        Self::NAME
    }
}

impl<T: ObjectModel> GlobalIdType for T {}

/// The two components of a decoded global ID.
#[derive(Clone, Debug, PartialEq, Eq, Hash)]
pub struct DecodedGlobalId {
    /// The GraphQL type name.
    pub typename: String,
    /// The application-local identifier.
    pub local_id: String,
}

/// A typed Relay global ID.
///
/// The local identifier is stored as a plain string; encoding happens when the value crosses the
/// GraphQL boundary, so resolvers and loaders work with ordinary local IDs.
///
/// ```
/// use gurafi::{Object, Schema};
/// use gurafi_relay::GlobalId;
///
/// #[derive(Object)]
/// struct User {
///     id: String,
/// }
///
/// let id = GlobalId::<User>::new("42");
/// assert_eq!(id.encode(), "VXNlcjo0Mg==");
/// assert_eq!(GlobalId::<User>::decode("VXNlcjo0Mg==").unwrap().as_local(), "42");
/// assert!(GlobalId::<User>::decode("UG9zdDo0Mg==").is_err()); // Post:42
/// ```
pub struct GlobalId<T: GlobalIdType> {
    local_id: String,
    marker: PhantomData<fn() -> T>,
}

impl<T: GlobalIdType> Clone for GlobalId<T> {
    fn clone(&self) -> Self {
        Self::new(self.local_id.clone())
    }
}

impl<T: GlobalIdType> PartialEq for GlobalId<T> {
    fn eq(&self, other: &Self) -> bool {
        self.local_id == other.local_id
    }
}

impl<T: GlobalIdType> Eq for GlobalId<T> {}

impl<T: GlobalIdType> PartialOrd for GlobalId<T> {
    fn partial_cmp(&self, other: &Self) -> Option<std::cmp::Ordering> {
        Some(self.cmp(other))
    }
}

impl<T: GlobalIdType> Ord for GlobalId<T> {
    fn cmp(&self, other: &Self) -> std::cmp::Ordering {
        self.local_id.cmp(&other.local_id)
    }
}

impl<T: GlobalIdType> std::hash::Hash for GlobalId<T> {
    fn hash<H: std::hash::Hasher>(&self, state: &mut H) {
        self.local_id.hash(state);
    }
}

impl<T: GlobalIdType> GlobalId<T> {
    /// Create an ID from an application-local identifier.
    pub fn new(local_id: impl Into<String>) -> Self {
        Self {
            local_id: local_id.into(),
            marker: PhantomData,
        }
    }

    /// Alias for [`GlobalId::new`].
    pub fn from_local(local_id: impl Into<String>) -> Self {
        Self::new(local_id)
    }

    /// The application-local identifier.
    #[must_use]
    pub fn as_local(&self) -> &str {
        &self.local_id
    }

    /// The application-local identifier, as an owned string.
    #[must_use]
    pub fn local_id(&self) -> &str {
        &self.local_id
    }

    /// Consume the ID into its local identifier.
    #[must_use]
    pub fn into_local(self) -> String {
        self.local_id
    }

    /// The type name embedded in encoded IDs.
    #[must_use]
    pub fn typename() -> &'static str {
        T::global_typename()
    }

    /// Encode as `Base64("TypeName:localId")`.
    #[must_use]
    pub fn encode(&self) -> String {
        let raw = format!("{}:{}", Self::typename(), self.local_id);
        base64::encode(raw.as_bytes())
    }

    /// Decode and verify that the global ID belongs to `T`.
    ///
    /// # Errors
    ///
    /// Returns a GraphQL error when the value is not canonical Base64, does not decode to
    /// `TypeName:localId`, or names a different type.
    pub fn decode(value: &str) -> Result<Self, GraphQLError> {
        let decoded = decode_global_id(value)?;
        if decoded.typename != Self::typename() {
            return Err(GraphQLError::new(format!(
                "Global ID is for type `{}`, but `{}` was expected.",
                decoded.typename,
                Self::typename()
            )));
        }
        Ok(Self::new(decoded.local_id))
    }
}

/// Decode any global ID without checking its type name.
///
/// This is the untyped primitive used by the Node dispatcher. Prefer [`GlobalId::decode`] when
/// the expected type is known.
///
/// # Errors
///
/// Returns a GraphQL error when `value` is not canonical Base64 UTF-8 or does not have the form
/// `TypeName:localId`.
pub fn decode_global_id(value: &str) -> Result<DecodedGlobalId, GraphQLError> {
    let bytes = base64::decode(value)
        .map_err(|error| GraphQLError::new(format!("Invalid global ID: {error}")))?;
    let decoded = String::from_utf8(bytes)
        .map_err(|_| GraphQLError::new("Invalid global ID: it does not decode to UTF-8."))?;
    let (typename, local_id) = decoded.split_once(':').ok_or_else(|| {
        GraphQLError::new(
            "Malformed global ID: after Base64 decoding it must have the form \
             `TypeName:localId`.",
        )
    })?;
    if typename.is_empty() {
        return Err(GraphQLError::new(
            "Malformed global ID: the type name before `:` is empty.",
        ));
    }
    Ok(DecodedGlobalId {
        typename: typename.to_owned(),
        local_id: local_id.to_owned(),
    })
}

impl<T: GlobalIdType> fmt::Debug for GlobalId<T> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_tuple("GlobalId")
            .field(&format_args!("{}:{}", Self::typename(), self.local_id))
            .finish()
    }
}

impl<T: GlobalIdType> fmt::Display for GlobalId<T> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.encode())
    }
}

impl<T: GlobalIdType> From<GlobalId<T>> for String {
    fn from(value: GlobalId<T>) -> Self {
        value.encode()
    }
}

impl<T: GlobalIdType> OutputType for GlobalId<T> {
    fn output_type() -> TypeRef {
        TypeRef::id().non_null()
    }
}

impl<T: GlobalIdType> InputType for GlobalId<T> {
    fn input_type() -> TypeRef {
        TypeRef::id().non_null()
    }
}

impl<T: GlobalIdType> IntoValue for GlobalId<T> {
    fn into_value(self) -> Result<Value, GraphQLError> {
        Ok(Value::String(Arc::from(self.encode())))
    }
}

impl<T: GlobalIdType> FromValue for GlobalId<T> {
    fn from_value(value: &Value) -> Result<Self, GraphQLError> {
        // `ID` accepts integer literals as well as strings; global IDs are strings after
        // coercion, but keeping the same literal handling makes variables and inline literals
        // behave identically.
        let raw = match value {
            Value::String(value) | Value::Enum(value) => value.to_string(),
            Value::Int(value) => value.to_string(),
            Value::BigInt(value) => value.to_string(),
            other => {
                return Err(GraphQLError::new(format!(
                    "`ID` cannot represent a value of kind {}.",
                    other.kind()
                )));
            }
        };
        Self::decode(&raw)
    }
}
