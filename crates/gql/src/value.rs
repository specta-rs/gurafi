//! GraphQL values.
//!
//! There are two value models in GraphQL: the *input* value model used by variables and
//! arguments, and the *result* value model embedded in a response. Rust's type system does not
//! need to distinguish them at runtime because both are JSON-shaped, so a single [`Value`] type
//! serves both sides. Scalars are represented by the closest JSON primitive; [`Value::Enum`]
//! exists so enum names stay distinguishable without allocating a second string.

use core::fmt;
use std::{any::Any, borrow::Cow, sync::Arc};

use smallvec::SmallVec;

/// A GraphQL input or result value.
#[derive(Clone, Debug, PartialEq)]
pub enum Value {
    /// The explicit `null` value.
    Null,
    /// A boolean.
    Boolean(bool),
    /// A 32-bit signed integer. GraphQL's built-in `Int` is 32-bit; larger integers belong to a
    /// custom scalar (represent them as a string or float).
    Int(i32),
    /// A 64-bit IEEE-754 floating point number.
    Float(f64),
    /// A string.
    String(Arc<str>),
    /// The name of an enum value.
    Enum(Arc<str>),
    /// An ordered list.
    List(Vec<Value>),
    /// An object: an ordered sequence of distinct field names and values.
    Object(Arc<Object>),
    /// An opaque Rust value produced by a higher-level API.
    ///
    /// Executors never serialize this variant directly. It exists so a type-safe schema builder
    /// can pass its own model types (for example a `User` struct) between resolvers while the
    /// engine stays completely generic. An object type with a resolver accepts it as the parent
    /// value; the high-level layer downcasts it with [`External::downcast_ref`].
    External(External),
}

/// An opaque, thread-safe value used to carry model types through the executor.
#[derive(Clone)]
pub struct External(Arc<dyn Any + Send + Sync>);

impl External {
    /// Wrap a model value.
    #[must_use]
    pub fn new<T: Any + Send + Sync>(value: T) -> Self {
        Self(Arc::new(value))
    }

    /// Borrow the model value as a concrete type.
    #[must_use]
    pub fn downcast_ref<T: Any>(&self) -> Option<&T> {
        self.0.downcast_ref()
    }
}

impl fmt::Debug for External {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str("External(<model>)")
    }
}

impl PartialEq for External {
    fn eq(&self, other: &Self) -> bool {
        Arc::ptr_eq(&self.0, &other.0)
    }
}

impl Value {
    /// Construct a string value from anything string-like.
    #[must_use]
    pub fn string(value: impl Into<Arc<str>>) -> Self {
        Self::String(value.into())
    }

    /// Construct an enum value from an enum variant name.
    #[must_use]
    pub fn enumeration(value: impl Into<Arc<str>>) -> Self {
        Self::Enum(value.into())
    }

    /// Construct an object value.
    #[must_use]
    pub fn object(object: impl Into<Object>) -> Self {
        Self::Object(Arc::new(object.into()))
    }

    /// Whether the value is explicit `null`.
    #[must_use]
    pub const fn is_null(&self) -> bool {
        matches!(self, Self::Null)
    }

    /// The name of the value's GraphQL-ish kind, used in error messages.
    #[must_use]
    pub const fn kind(&self) -> &'static str {
        match self {
            Self::Null => "null",
            Self::Boolean(_) => "boolean",
            Self::Int(_) | Self::Float(_) => "number",
            Self::String(_) => "string",
            Self::Enum(_) => "enum",
            Self::List(_) => "list",
            Self::Object(_) => "object",
            Self::External(_) => "model",
        }
    }

    /// Borrow the value as a string.
    #[must_use]
    pub fn as_str(&self) -> Option<&str> {
        match self {
            Self::String(value) => Some(value),
            Self::Enum(value) => Some(value),
            _ => None,
        }
    }

    /// Borrow the value as a bool.
    #[must_use]
    pub const fn as_bool(&self) -> Option<bool> {
        match self {
            Self::Boolean(value) => Some(*value),
            _ => None,
        }
    }

    /// Borrow the value as an `i32`.
    #[must_use]
    pub const fn as_i32(&self) -> Option<i32> {
        match self {
            Self::Int(value) => Some(*value),
            _ => None,
        }
    }

    /// Borrow the value as an `i64`.
    #[must_use]
    pub const fn as_i64(&self) -> Option<i64> {
        match self {
            Self::Int(value) => Some(*value as i64),
            _ => None,
        }
    }

    /// Borrow the value as an `f64`.
    #[must_use]
    pub fn as_f64(&self) -> Option<f64> {
        match self {
            Self::Float(value) => Some(*value),
            Self::Int(value) => Some(f64::from(*value)),
            _ => None,
        }
    }

    /// Borrow the value as an object.
    #[must_use]
    pub fn as_object(&self) -> Option<&Object> {
        match self {
            Self::Object(object) => Some(object),
            _ => None,
        }
    }

    /// Mutably borrow the value as an object.
    #[must_use]
    pub fn as_object_mut(&mut self) -> Option<&mut Object> {
        match self {
            Self::Object(object) => Some(Arc::make_mut(object)),
            _ => None,
        }
    }

    /// Borrow the value as a list.
    #[must_use]
    pub fn as_list(&self) -> Option<&[Value]> {
        match self {
            Self::List(list) => Some(list),
            _ => None,
        }
    }

    /// Wrap a model value for a type-safe high-level API.
    #[must_use]
    pub fn external<T: Any + Send + Sync>(value: T) -> Self {
        Self::External(External::new(value))
    }

    /// Whether this is an opaque model value.
    #[must_use]
    pub const fn is_external(&self) -> bool {
        matches!(self, Self::External(_))
    }

    /// Borrow an opaque model value as a concrete type.
    #[must_use]
    pub fn as_external<T: Any>(&self) -> Option<&T> {
        match self {
            Self::External(value) => value.downcast_ref(),
            _ => None,
        }
    }

    /// Turn the value into a list, if it is one.
    #[must_use]
    pub fn into_list(self) -> Option<Vec<Value>> {
        match self {
            Self::List(list) => Some(list),
            _ => None,
        }
    }

    /// Turn the value into a string, if it is one.
    #[must_use]
    pub fn into_string(self) -> Option<Arc<str>> {
        match self {
            Self::String(value) | Self::Enum(value) => Some(value),
            _ => None,
        }
    }
}

impl fmt::Display for Value {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Null => f.write_str("null"),
            Self::Boolean(value) => write!(f, "{value}"),
            Self::Int(value) => write!(f, "{value}"),
            Self::Float(value) => write!(f, "{value}"),
            Self::String(value) => write!(f, "{value}"),
            Self::Enum(value) => f.write_str(value),
            Self::List(_) => f.write_str("[list]"),
            Self::Object(_) => f.write_str("{object}"),
            Self::External(_) => f.write_str("<model>"),
        }
    }
}

impl From<bool> for Value {
    fn from(value: bool) -> Self {
        Self::Boolean(value)
    }
}

macro_rules! impl_from_int {
    ($($ty:ty),* $(,)?) => {
        $(
            impl From<$ty> for Value {
                fn from(value: $ty) -> Self {
                    Self::Int(value as i32)
                }
            }
        )*
    };
}

impl_from_int!(i8, i16, i32, u8, u16);

impl From<f32> for Value {
    fn from(value: f32) -> Self {
        Self::Float(f64::from(value))
    }
}

impl From<f64> for Value {
    fn from(value: f64) -> Self {
        Self::Float(value)
    }
}

impl From<&str> for Value {
    fn from(value: &str) -> Self {
        Self::String(Arc::from(value))
    }
}

impl From<String> for Value {
    fn from(value: String) -> Self {
        Self::String(Arc::from(value))
    }
}

impl From<Arc<str>> for Value {
    fn from(value: Arc<str>) -> Self {
        Self::String(value)
    }
}

impl From<Cow<'_, str>> for Value {
    fn from(value: Cow<'_, str>) -> Self {
        Self::String(Arc::from(value.as_ref()))
    }
}

impl From<Vec<Value>> for Value {
    fn from(value: Vec<Value>) -> Self {
        Self::List(value)
    }
}

impl From<Object> for Value {
    fn from(value: Object) -> Self {
        Self::Object(Arc::new(value))
    }
}

impl<T: Into<Value>> From<Option<T>> for Value {
    fn from(value: Option<T>) -> Self {
        value.map_or(Self::Null, Into::into)
    }
}

/// An ordered GraphQL object.
///
/// Objects in GraphQL are small: most have fewer than a dozen fields. A linear scan over an
/// inline `SmallVec` beats hashing for those shapes, and it keeps field order stable in responses
/// without an extra `IndexMap` allocation. Larger objects still behave correctly, just with a
/// linear lookup.
#[derive(Clone, Debug, Default, PartialEq)]
pub struct Object {
    fields: SmallVec<[(Arc<str>, Value); 8]>,
}

impl Object {
    /// Create an empty object.
    #[must_use]
    pub fn new() -> Self {
        Self {
            fields: SmallVec::new(),
        }
    }

    /// Create an empty object with capacity for `capacity` fields.
    #[must_use]
    pub fn with_capacity(capacity: usize) -> Self {
        Self {
            fields: SmallVec::with_capacity(capacity),
        }
    }

    /// Create an object from field pairs. Later duplicates replace earlier ones.
    #[must_use]
    pub fn from_fields(fields: impl IntoIterator<Item = (impl Into<Arc<str>>, Value)>) -> Self {
        let mut object = Self::new();
        for (key, value) in fields {
            object.insert(key, value);
        }
        object
    }

    /// The number of fields.
    #[must_use]
    pub fn len(&self) -> usize {
        self.fields.len()
    }

    /// Whether the object has no fields.
    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.fields.is_empty()
    }

    /// Look up a field by name.
    #[must_use]
    pub fn get(&self, key: &str) -> Option<&Value> {
        self.fields
            .iter()
            .find(|(name, _)| &**name == key)
            .map(|(_, value)| value)
    }

    /// Mutably look up a field by name.
    pub fn get_mut(&mut self, key: &str) -> Option<&mut Value> {
        self.fields
            .iter_mut()
            .find(|(name, _)| &**name == key)
            .map(|(_, value)| value)
    }

    /// Whether a field is present.
    #[must_use]
    pub fn contains_key(&self, key: &str) -> bool {
        self.get(key).is_some()
    }

    /// Insert a field, replacing an existing value while preserving its position.
    pub fn insert(&mut self, key: impl Into<Arc<str>>, value: impl Into<Value>) -> Option<Value> {
        let key = key.into();
        let value = value.into();
        if let Some((_, existing)) = self.fields.iter_mut().find(|(name, _)| *name == key) {
            return Some(core::mem::replace(existing, value));
        }
        self.fields.push((key, value));
        None
    }

    /// Append a field without checking for an existing key.
    ///
    /// Callers such as the executor know response keys are unique; skipping the linear scan keeps
    /// large result objects linear instead of quadratic.
    pub(crate) fn push_unchecked(&mut self, key: impl Into<Arc<str>>, value: Value) {
        self.fields.push((key.into(), value));
    }

    /// Remove a field by name.
    pub fn remove(&mut self, key: &str) -> Option<Value> {
        let index = self.fields.iter().position(|(name, _)| &**name == key)?;
        Some(self.fields.remove(index).1)
    }

    /// Iterate over the fields in insertion order.
    pub fn iter(&self) -> impl ExactSizeIterator<Item = (&Arc<str>, &Value)> {
        self.fields.iter().map(|(key, value)| (key, value))
    }

    /// Iterate over the field names in insertion order.
    pub fn keys(&self) -> impl ExactSizeIterator<Item = &Arc<str>> {
        self.fields.iter().map(|(key, _)| key)
    }

    /// Iterate over the field values in insertion order.
    pub fn values(&self) -> impl ExactSizeIterator<Item = &Value> {
        self.fields.iter().map(|(_, value)| value)
    }

    /// Consume the object into its fields.
    pub fn into_fields(self) -> impl Iterator<Item = (Arc<str>, Value)> {
        self.fields.into_iter()
    }

    /// Reserve capacity for additional fields.
    pub fn reserve(&mut self, additional: usize) {
        self.fields.reserve(additional);
    }
}

impl FromIterator<(Arc<str>, Value)> for Object {
    fn from_iter<T: IntoIterator<Item = (Arc<str>, Value)>>(iter: T) -> Self {
        let mut object = Self::new();
        for (key, value) in iter {
            object.insert(key, value);
        }
        object
    }
}

impl IntoIterator for Object {
    type Item = (Arc<str>, Value);
    type IntoIter = smallvec::IntoIter<[(Arc<str>, Value); 8]>;

    fn into_iter(self) -> Self::IntoIter {
        self.fields.into_iter()
    }
}

#[cfg(feature = "serde")]
impl serde::Serialize for Value {
    fn serialize<S: serde::Serializer>(
        &self,
        serializer: S,
    ) -> core::result::Result<S::Ok, S::Error> {
        match self {
            Self::Null => serializer.serialize_none(),
            Self::Boolean(value) => serializer.serialize_bool(*value),
            Self::Int(value) => serializer.serialize_i32(*value),
            Self::Float(value) => serializer.serialize_f64(*value),
            Self::String(value) | Self::Enum(value) => serializer.serialize_str(value),
            Self::List(values) => {
                use serde::ser::SerializeSeq as _;
                let mut seq = serializer.serialize_seq(Some(values.len()))?;
                for value in values {
                    seq.serialize_element(value)?;
                }
                seq.end()
            }
            Self::Object(object) => (**object).serialize(serializer),
            Self::External(_) => Err(serde::ser::Error::custom(
                "an opaque model value reached the serializer; was an object field resolved?",
            )),
        }
    }
}

#[cfg(feature = "serde")]
impl serde::Serialize for Object {
    fn serialize<S: serde::Serializer>(
        &self,
        serializer: S,
    ) -> core::result::Result<S::Ok, S::Error> {
        use serde::ser::SerializeMap as _;

        let mut map = serializer.serialize_map(Some(self.fields.len()))?;
        for (key, value) in &self.fields {
            map.serialize_entry(&**key, value)?;
        }
        map.end()
    }
}
