//! Resolve a whole object at once instead of field by field.
//!
//! Normally every field of an object is resolved by its own resolver. [`Raw`] is the escape
//! hatch for the case where the data already exists in the executor's value model — a cached
//! JSON document, a row that was serialised once, or an upstream service that returns the whole
//! object — and paying for a resolver per field is wasteful.
//!
//! ```
//! use gurafi::{NoArgs, Object, Raw, RawObject, Schema};
//!
//! struct Ctx;
//!
//! #[derive(Object)]
//! struct Giraffe {
//!     name: String,
//! }
//!
//! let mut schema = Schema::<Ctx>::new();
//! schema.object::<Giraffe>(|o| {
//!     // No resolver: the field is read from the raw object map by name.
//!     o.raw_field::<String>("name");
//! });
//! schema.query(|q| {
//!     q.field("giraffe", |_: NoArgs, _: &Ctx| {
//!         Raw::<Giraffe>::object(
//!             RawObject::new()
//!                 .field("name", "Melman")
//!                 .build(),
//!         )
//!     });
//! });
//! # let _ = schema.build().expect("valid schema");
//! ```
//!
//! # The tradeoff
//!
//! [`Raw<T>`] takes its GraphQL output type from `T`, so the schema is still driven by Rust
//! types. What it cannot check is the *contents* of the [`gql::Value`]: the caller owns field
//! names, field types, nullability and `__typename`. Most wrong values are not a panic — the
//! executor rejects them with a normal field error and null propagation — but they are not a
//! compile error either. A custom scalar's raw value is the exception: the executor passes it
//! through unchanged, so the scalar's own [`crate::ScalarValue::to_value`] does not run (see
//! [Custom scalars](#custom-scalars)).
//!
//! Resolving a whole object skips the per-field resolvers of that object, so those fields must
//! be declared with [`crate::ObjectBuilder::raw_field`] (or
//! [`crate::ObjectBuilder::raw_field_with`] when they take arguments). Mixing a raw field with a
//! resolver-backed field on the same object is a schema-build error: the raw object supplies an
//! object map, so the resolver would not receive the Rust model it expects.
//!
//! An unguarded raw field has no resolver and uses the executor's property lookup. Field-level,
//! object-level and inherited interface authorization still apply: the schema builder installs a
//! property resolver when a guard needs one, so a guarded raw field does run a resolver (the
//! guard around the same property lookup).
//!
//! # Reserved keys
//!
//! `gql` uses object keys starting with `__gql_` for introspection runtime descriptors. They are
//! not user fields, and trusting them on a raw object would let caller data choose its own
//! runtime type, bypassing `resolve_type`, `is_type_of` and object authorization. [`Raw::new`]
//! (and so [`Raw::object`]) removes all `__gql_` keys recursively, and [`RawObject`] ignores
//! them. Build raw maps from untrusted JSON with those constructors, not by hand.
//!
//! An explicit `__typename` key is different: it is the documented way to select the concrete
//! type of an interface or union value, so it is preserved. That also means it is trusted. When a
//! map comes from untrusted JSON and the data must not choose its own runtime type, write
//! `__typename` from trusted data (for example with [`RawObject::typename`], which replaces an
//! existing value) or have `resolve_type` derive the type from data the application has checked.
//!
//! # Custom scalars
//!
//! A raw field does not serialize its value through [`crate::ScalarValue::to_value`]. For a
//! custom scalar, the value in the map is used as-is, so a shape the scalar would reject can
//! still reach the response. Use a normal resolver when a custom scalar must validate or
//! normalize its output, or keep the map truthful.

use std::{marker::PhantomData, sync::Arc};

use gql::{GraphQLError, Name, Object, TypeRef, Value};

use crate::{
    registry::TypeRegistry,
    traits::{IntoValue, OutputType},
};

/// A prebuilt GraphQL value with a compile-time output type.
///
/// `T` only selects the GraphQL output type (through [`OutputType`]); it is not read at runtime.
/// The value is passed to the executor unchanged apart from removing reserved `__gql_*` keys, so
/// the caller is responsible for making the value match `T`: an object map for an object type, a
/// list for `Vec<T>`, a string for `String`, and so on.
///
/// Use [`Raw::object`] for the common case of a whole object, and [`RawObject`] to build the
/// object map. A list of objects is `Raw<Vec<T>>`, built with [`Raw::list`] or
/// `Raw::new(gql::Value::List(..))`.
///
/// ```
/// use gurafi::{gql, Raw, RawObject};
///
/// #[derive(gurafi::Object)]
/// struct Giraffe {
///     name: String,
/// }
///
/// let value = Raw::<Giraffe>::object(
///     RawObject::new().field("name", "Melman").build(),
/// );
/// assert!(matches!(value.raw_value(), gql::Value::Object(_)));
/// ```
pub struct Raw<T> {
    value: Value,
    marker: PhantomData<fn() -> T>,
}

impl<T> Clone for Raw<T> {
    fn clone(&self) -> Self {
        Self {
            value: self.value.clone(),
            marker: PhantomData,
        }
    }
}

impl<T> PartialEq for Raw<T> {
    fn eq(&self, other: &Self) -> bool {
        self.value == other.value
    }
}

impl<T> Raw<T> {
    /// Wrap a prebuilt value, declaring it to be `T` in the schema.
    ///
    /// Any `__gql_*` key in an object — at any depth — is removed first. Those keys are
    /// introspection metadata, and a raw value containing them could otherwise switch its own
    /// runtime type and bypass `resolve_type`, `is_type_of` and object authorization. This makes
    /// the constructor safe for a value decoded from untrusted JSON. An explicit `__typename`
    /// key is kept, because it is the documented type marker for abstract values.
    ///
    /// The value's shape is otherwise not inspected. If it does not have the shape `T` describes,
    /// execution reports a field error (except for custom scalar values, which pass through
    /// unchanged; see the [module docs](self)).
    #[must_use]
    pub fn new(value: Value) -> Self {
        Self {
            value: strip_internal_keys(value),
            marker: PhantomData,
        }
    }

    /// Wrap an object map as a whole object of type `T`.
    ///
    /// This is the fast path: use [`RawObject`] (or `gql::Object` directly) to build the map,
    /// then hand it over without touching each field again. Like [`Raw::new`], reserved
    /// `__gql_*` keys are removed recursively.
    #[must_use]
    pub fn object(object: Object) -> Self {
        Self::new(Value::Object(Arc::new(object)))
    }

    /// A raw GraphQL `null`.
    ///
    /// This is only valid when `T` is nullable (`Option<...>`), or when the containing field is
    /// nullable. Returning `Raw::<Giraffe>::null()` for a non-null field is a normal non-null
    /// violation at execution time.
    #[must_use]
    pub fn null() -> Self {
        Self::new(Value::Null)
    }

    /// Borrow the prebuilt value.
    #[must_use]
    pub fn raw_value(&self) -> &Value {
        &self.value
    }

    /// Consume this wrapper and return the prebuilt value.
    #[must_use]
    pub fn into_raw(self) -> Value {
        self.value
    }
}

impl<T> Raw<Vec<T>> {
    /// Wrap a list of raw values.
    ///
    /// ```
    /// use gurafi::{Raw, RawObject};
    ///
    /// #[derive(gurafi::Object)]
    /// struct Giraffe {
    ///     name: String,
    /// }
    ///
    /// let giraffes = Raw::<Vec<Giraffe>>::list([
    ///     Raw::object(RawObject::new().field("name", "Melman").build()),
    ///     Raw::object(RawObject::new().field("name", "Karen").build()),
    /// ]);
    /// assert_eq!(giraffes.raw_value().as_list().map(<[gurafi::Value]>::len), Some(2));
    /// ```
    #[must_use]
    pub fn list(items: impl IntoIterator<Item = Raw<T>>) -> Self {
        Self::new(Value::List(
            items.into_iter().map(|item| item.value).collect(),
        ))
    }
}

impl<T> From<Value> for Raw<T> {
    fn from(value: Value) -> Self {
        Self::new(value)
    }
}

impl<T> From<Object> for Raw<T> {
    fn from(object: Object) -> Self {
        Self::object(object)
    }
}

impl<T> std::fmt::Debug for Raw<T> {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Raw").field("value", &self.value).finish()
    }
}

impl<T: OutputType> OutputType for Raw<T> {
    fn output_type() -> TypeRef {
        T::output_type()
    }

    fn is_optional() -> bool {
        T::is_optional()
    }

    fn register_output(registry: &mut TypeRegistry) {
        T::register_output(registry);
    }
}

impl<T> IntoValue for Raw<T> {
    fn into_value(self) -> Result<Value, GraphQLError> {
        Ok(self.value)
    }
}

/// Build an object map for [`Raw::object`].
///
/// The builder is optional sugar over `gql::Object`; it keeps the common case readable:
///
/// ```
/// use gurafi::{gql, Raw, RawObject};
///
/// #[derive(gurafi::Object)]
/// struct Giraffe {
///     name: String,
/// }
///
/// let raw = Raw::<Giraffe>::object(
///     RawObject::new()
///         .field("name", "Melman")
///         .field("height", 3.4)
///         .build(),
/// );
/// assert!(matches!(raw.raw_value(), gql::Value::Object(_)));
/// ```
///
/// A field's value can be any `Into<Value>`: `&str`, `String`, numbers, booleans,
/// `Option<T>`, `Value`, `gql::Object`, or another [`RawObject`] (nested objects). Use
/// [`RawObject::typename`] when the object is resolved through an interface or union; do not set
/// `__gql_typename`.
///
/// Reserved `__gql_*` keys are ignored, including inside nested values. If the map comes from
/// untrusted JSON, prefer [`Raw::new`] or [`Raw::object`], which apply the same rule recursively
/// to an already-built value.
#[derive(Clone, Debug, Default, PartialEq)]
pub struct RawObject {
    object: Object,
}

impl RawObject {
    /// Create an empty object map.
    #[must_use]
    pub fn new() -> Self {
        Self {
            object: Object::new(),
        }
    }

    /// Create an empty object map with room for `capacity` fields.
    #[must_use]
    pub fn with_capacity(capacity: usize) -> Self {
        Self {
            object: Object::with_capacity(capacity),
        }
    }

    /// Add a field, replacing an existing field with the same name.
    ///
    /// Reserved `__gql_*` names are ignored, and `__gql_*` keys inside an object or list value
    /// are removed. The value is not otherwise checked against the schema: providing a missing
    /// non-null field, or one of the wrong type, is reported by the executor as a field error.
    #[must_use]
    pub fn field(mut self, name: impl Into<Name>, value: impl Into<Value>) -> Self {
        self.insert(name, value);
        self
    }

    /// Add a field by mutable reference, returning the value it replaced.
    ///
    /// Returns `None` when the field was absent, and also when `name` is reserved (`__gql_*`), in
    /// which case nothing is inserted.
    pub fn insert(&mut self, name: impl Into<Name>, value: impl Into<Value>) -> Option<Value> {
        let name = name.into();
        if is_internal_key(&name) {
            return None;
        }
        self.object.insert(name, strip_internal_keys(value.into()))
    }

    /// Set `__typename`, which interfaces and unions use to select the concrete object type.
    ///
    /// Only needed for abstract output types. A concrete object field does not read it; `gql`
    /// derives `__typename` from the declared object type (after abstract resolution, the type
    /// the executor completed the value as). This is the supported type marker; `__gql_typename`
    /// is internal and is ignored.
    #[must_use]
    pub fn typename(mut self, name: impl Into<Name>) -> Self {
        self.insert("__typename", Value::String(name.into()));
        self
    }

    /// Remove a field, if present.
    #[must_use]
    pub fn without(mut self, name: &str) -> Self {
        self.object.remove(name);
        self
    }

    /// Look up a field.
    #[must_use]
    pub fn get(&self, name: &str) -> Option<&Value> {
        self.object.get(name)
    }

    /// The number of fields.
    #[must_use]
    pub fn len(&self) -> usize {
        self.object.len()
    }

    /// Whether the object has no fields.
    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.object.is_empty()
    }

    /// Finish the map, ready for [`Raw::object`].
    #[must_use]
    pub fn build(self) -> Object {
        self.object
    }

    /// Finish the map as a GraphQL value.
    #[must_use]
    pub fn into_value(self) -> Value {
        Value::Object(Arc::new(self.object))
    }

    /// Finish the map as a typed raw value.
    #[must_use]
    pub fn into_raw<T>(self) -> Raw<T> {
        Raw::object(self.object)
    }
}

impl From<RawObject> for Value {
    fn from(value: RawObject) -> Self {
        value.into_value()
    }
}

impl From<RawObject> for Object {
    fn from(value: RawObject) -> Self {
        value.object
    }
}

/// The prefix `gql` reserves for introspection runtime metadata.
const INTERNAL_PREFIX: &str = "__gql_";

fn is_internal_key(name: &str) -> bool {
    name.starts_with(INTERNAL_PREFIX)
}

/// Whether a value contains a reserved key anywhere in its object tree.
fn contains_internal_key(value: &Value) -> bool {
    match value {
        Value::Object(object) => {
            object.keys().any(|key| is_internal_key(key))
                || object.values().any(contains_internal_key)
        }
        Value::List(items) => items.iter().any(contains_internal_key),
        _ => false,
    }
}

/// Remove every reserved `__gql_*` key, recursively.
///
/// Clean values are returned unchanged, so sanitizing an already-clean map costs one scan and no
/// allocation. Only a value that contains a reserved key is rebuilt.
fn strip_internal_keys(value: Value) -> Value {
    if !contains_internal_key(&value) {
        return value;
    }
    match value {
        Value::Object(object) => {
            let mut clean = Object::with_capacity(object.len());
            let object = Arc::try_unwrap(object).unwrap_or_else(|shared| (*shared).clone());
            for (key, value) in object.into_fields() {
                if !is_internal_key(&key) {
                    clean.push_unchecked(key, strip_internal_keys(value));
                }
            }
            Value::Object(Arc::new(clean))
        }
        Value::List(items) => Value::List(items.into_iter().map(strip_internal_keys).collect()),
        other => other,
    }
}
