//! The GraphQL type system.
//!
//! Definitions are immutable once a [`Schema`](crate::Schema) is built. They are wrapped in
//! `Arc`s so cloning a field definition (for example while building an introspection result) is
//! cheap and does not duplicate resolver state.
//!
//! A schema is *name based*: a field's type is a [`TypeRef`] holding the name of another
//! definition. That is what makes recursive and mutually recursive types possible without
//! interior mutability. The schema builder resolves and validates every name once, at build
//! time.

use core::fmt;
use std::sync::Arc;

use futures::stream::BoxStream;
use indexmap::IndexMap;

use crate::{
    Value, ast,
    error::{GraphQLError, PathSegment},
    execution::ExecutionContext,
    schema::Schema,
};

/// An interned GraphQL name.
pub type Name = Arc<str>;

/// Above this many entries, a name-keyed definition map hashes instead of scanning.
///
/// Every lookup hashes otherwise, and almost every GraphQL type declares fewer fields, enum
/// values or arguments than this.
pub(crate) const SMALL_MAP_LIMIT: usize = 12;

/// Compare two GraphQL names.
///
/// Names are short and compared constantly — every field completion, every schema lookup and
/// every field collection compares one — and `str == str` compiles to a `memcmp` call whose
/// overhead dominates the handful of bytes actually compared. Walking the bytes inline is faster
/// for the lengths real documents use, and falls back to the slice comparison for the rare long
/// name.
#[inline]
pub(crate) fn names_eq(left: &str, right: &str) -> bool {
    let left = left.as_bytes();
    let right = right.as_bytes();
    if left.len() != right.len() {
        return false;
    }
    if left.len() > 8 {
        return left == right;
    }
    let mut index = 0;
    while index < left.len() {
        if left[index] != right[index] {
            return false;
        }
        index += 1;
    }
    true
}

/// Look up a name in a definition map, scanning instead of hashing while the map is small.
///
/// Execution performs these lookups for every field it completes and every argument it coerces,
/// and schema definitions overwhelmingly hold fewer entries than [`SMALL_MAP_LIMIT`].
pub(crate) fn map_get<'m, V>(map: &'m IndexMap<Name, V>, name: &str) -> Option<&'m V> {
    if map.len() <= SMALL_MAP_LIMIT {
        return map
            .iter()
            .find(|(key, _)| names_eq(key, name))
            .map(|(_, value)| value);
    }
    map.get(name)
}

/// The result of resolving a field.
pub type FieldResult = Result<Value, GraphQLError>;

/// A boxed future returned by an asynchronous resolver.
pub type ResolveFuture<'a> = futures::future::BoxFuture<'a, FieldResult>;

/// A boxed stream returned by a subscription resolver.
pub type SubscriptionStream = BoxStream<'static, FieldResult>;

/// A directive applied to a schema element.
///
/// `arguments` holds the directive's arguments in source order, already converted to runtime
/// values. Applied directives are preserved so `Schema::from_sdl` round-trips through
/// [`Schema::sdl`](crate::Schema::sdl), and so code-first schemas can describe directives that
/// have no dedicated field in the type model.
///
/// The built-in `@deprecated`, `@specifiedBy` and `@oneOf` directives are also recorded here
/// when they are parsed, but their authoritative data lives in the dedicated
/// `deprecation_reason`, `specified_by_url` and `one_of` fields. The SDL printer reads those
/// fields, so a built-in application is never printed twice.
#[derive(Clone, Debug, PartialEq)]
pub struct AppliedDirective {
    /// The directive's name without `@`.
    pub name: Name,
    /// Arguments in source order.
    pub arguments: IndexMap<Name, Value>,
}

impl AppliedDirective {
    /// Start defining an applied directive.
    #[must_use]
    pub fn new(name: impl Into<Name>) -> Self {
        Self {
            name: name.into(),
            arguments: IndexMap::new(),
        }
    }

    /// Add an argument.
    ///
    /// Directive arguments are ordered, so arguments print in the order they are added.
    #[must_use]
    pub fn argument(mut self, name: impl Into<Name>, value: impl Into<Value>) -> Self {
        self.arguments.insert(name.into(), value.into());
        self
    }

    /// Look up an argument by name.
    #[must_use]
    pub fn get_argument(&self, name: &str) -> Option<&Value> {
        map_get(&self.arguments, name)
    }
}

/// The kind of a named GraphQL type.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum TypeKind {
    /// A scalar type.
    Scalar,
    /// An object type.
    Object,
    /// An interface type.
    Interface,
    /// A union type.
    Union,
    /// An enum type.
    Enum,
    /// An input object type.
    InputObject,
}

impl TypeKind {
    /// The name used by introspection.
    #[must_use]
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Scalar => "SCALAR",
            Self::Object => "OBJECT",
            Self::Interface => "INTERFACE",
            Self::Union => "UNION",
            Self::Enum => "ENUM",
            Self::InputObject => "INPUT_OBJECT",
        }
    }
}

impl fmt::Display for TypeKind {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.as_str())
    }
}

/// A reference to a type, possibly wrapped in list and non-null modifiers.
#[derive(Clone, Debug, PartialEq, Eq, Hash)]
pub enum TypeRef {
    /// A named type.
    Named(Name),
    /// A list of another type.
    List(Box<TypeRef>),
    /// A non-null version of another type.
    NonNull(Box<TypeRef>),
}

/// The interned name of a built-in type, when `name` is one.
///
/// Schema building and variable coercion turn type names from the document into `Arc<str>`s for
/// every request. The built-in names never change, so sharing one allocation for each of them
/// removes a handful of allocations (and their frees) from every request.
pub(crate) fn interned_builtin(name: &str) -> Option<Name> {
    static NAMES: [std::sync::OnceLock<Name>; 6] = [
        std::sync::OnceLock::new(),
        std::sync::OnceLock::new(),
        std::sync::OnceLock::new(),
        std::sync::OnceLock::new(),
        std::sync::OnceLock::new(),
        std::sync::OnceLock::new(),
    ];
    let index = match name {
        "String" => 0,
        "Int" => 1,
        "Float" => 2,
        "Boolean" => 3,
        "ID" => 4,
        "__Type" => 5,
        _ => return None,
    };
    Some(NAMES[index].get_or_init(|| Name::from(name)).clone())
}

impl TypeRef {
    /// A reference to a named type.
    #[must_use]
    pub fn named(name: impl Into<Name>) -> Self {
        Self::Named(name.into())
    }

    /// A reference to a named type, reusing the interned allocation for built-in names.
    pub(crate) fn named_str(name: &str) -> Self {
        Self::Named(interned_builtin(name).unwrap_or_else(|| Name::from(name)))
    }

    /// The built-in `String` type.
    #[must_use]
    pub fn string() -> Self {
        Self::Named("String".into())
    }

    /// The built-in `Int` type.
    #[must_use]
    pub fn int() -> Self {
        Self::Named("Int".into())
    }

    /// The built-in `Float` type.
    #[must_use]
    pub fn float() -> Self {
        Self::Named("Float".into())
    }

    /// The built-in `Boolean` type.
    #[must_use]
    pub fn boolean() -> Self {
        Self::Named("Boolean".into())
    }

    /// The built-in `ID` type.
    #[must_use]
    pub fn id() -> Self {
        Self::Named("ID".into())
    }

    /// Wrap this type in a list.
    #[must_use]
    pub fn list(self) -> Self {
        Self::List(Box::new(self))
    }

    /// Mark this type as non-null.
    #[must_use]
    pub fn non_null(self) -> Self {
        match self {
            Self::NonNull(_) => self,
            other => Self::NonNull(Box::new(other)),
        }
    }

    /// Remove a non-null wrapper, if present.
    #[must_use]
    pub fn nullable(self) -> Self {
        match self {
            Self::NonNull(inner) => *inner,
            other => other,
        }
    }

    /// The innermost named type.
    #[must_use]
    pub fn named_type(&self) -> &Name {
        match self {
            Self::Named(name) => name,
            Self::List(inner) | Self::NonNull(inner) => inner.named_type(),
        }
    }

    /// Whether the outermost modifier is non-null.
    #[must_use]
    pub const fn is_non_null(&self) -> bool {
        matches!(self, Self::NonNull(_))
    }

    /// Whether the outermost modifier is a list.
    #[must_use]
    pub const fn is_list(&self) -> bool {
        matches!(self, Self::List(_))
    }

    /// Whether this is a bare named type with no modifiers.
    #[must_use]
    pub const fn is_named(&self) -> bool {
        matches!(self, Self::Named(_))
    }

    /// The type of one list item, or `None` when this is not a list.
    #[must_use]
    pub fn list_item(&self) -> Option<&TypeRef> {
        match self {
            Self::List(inner) => Some(inner),
            Self::NonNull(inner) => inner.list_item(),
            Self::Named(_) => None,
        }
    }
}

impl fmt::Display for TypeRef {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Named(name) => f.write_str(name),
            Self::List(inner) => write!(f, "[{inner}]"),
            Self::NonNull(inner) => write!(f, "{inner}!"),
        }
    }
}

impl From<Name> for TypeRef {
    fn from(value: Name) -> Self {
        Self::Named(value)
    }
}

impl From<&str> for TypeRef {
    fn from(value: &str) -> Self {
        Self::Named(value.into())
    }
}

impl From<String> for TypeRef {
    fn from(value: String) -> Self {
        Self::Named(value.into())
    }
}

/// The signature of a scalar's output coercion function.
pub type ScalarSerializeFn = Arc<dyn Fn(&Value) -> Result<Value, String> + Send + Sync>;

/// The signature of a scalar's input coercion function for already-parsed runtime values.
pub type ScalarParseValueFn = Arc<dyn Fn(&Value) -> Result<Value, String> + Send + Sync>;

/// The signature of a scalar's input coercion function for literals.
pub type ScalarParseLiteralFn =
    Arc<dyn for<'a> Fn(&ast::Value<'a>) -> Result<Value, String> + Send + Sync>;

/// A scalar type.
#[derive(Clone)]
pub struct ScalarType {
    /// The scalar's name.
    pub name: Name,
    /// An optional description.
    pub description: Option<Arc<str>>,
    /// An optional `@specifiedBy` URL.
    pub specified_by_url: Option<Arc<str>>,
    /// Directives applied to the scalar, in application order.
    pub directives: Vec<AppliedDirective>,
    pub(crate) serialize: Option<ScalarSerializeFn>,
    pub(crate) parse_value: Option<ScalarParseValueFn>,
    pub(crate) parse_literal: Option<ScalarParseLiteralFn>,
    /// Whether `serialize` is one of the specification's built-in coercion functions.
    ///
    /// Execution may run the built-in functions more than once (a fast path that later falls back
    /// re-completes a field), which is only sound for code the engine owns. Any scalar built or
    /// customised by the user is not marked, so its function runs exactly once.
    pub(crate) builtin: bool,
}

impl fmt::Debug for ScalarType {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("ScalarType")
            .field("name", &self.name)
            .field("description", &self.description)
            .field("specified_by_url", &self.specified_by_url)
            .field("directives", &self.directives)
            .finish_non_exhaustive()
    }
}

impl ScalarType {
    /// Start defining a custom scalar.
    ///
    /// By default the scalar is transparent: values pass through unchanged. Attach coercion
    /// functions with [`ScalarType::parse_value`], [`ScalarType::parse_literal`] and
    /// [`ScalarType::serialize`] when the representation needs validating.
    #[must_use]
    pub fn new(name: impl Into<Name>) -> Self {
        Self {
            name: name.into(),
            description: None,
            specified_by_url: None,
            directives: Vec::new(),
            serialize: None,
            parse_value: None,
            parse_literal: None,
            builtin: false,
        }
    }

    /// Mark this scalar as carrying one of the specification's built-in coercion functions.
    ///
    /// Only the built-in constructors call this, and only after installing their own `serialize`.
    fn builtin(mut self) -> Self {
        self.builtin = true;
        self
    }

    /// Set the description.
    #[must_use]
    pub fn description(mut self, description: impl Into<Arc<str>>) -> Self {
        self.description = Some(description.into());
        self
    }

    /// Set the `@specifiedBy` URL.
    #[must_use]
    pub fn specified_by_url(mut self, url: impl Into<Arc<str>>) -> Self {
        self.specified_by_url = Some(url.into());
        self
    }

    /// Add an applied directive.
    #[must_use]
    pub fn directive(mut self, directive: AppliedDirective) -> Self {
        self.directives.push(directive);
        self
    }

    /// Set the output coercion function.
    #[must_use]
    pub fn serialize(
        mut self,
        function: impl Fn(&Value) -> Result<Value, String> + Send + Sync + 'static,
    ) -> Self {
        self.serialize = Some(Arc::new(function));
        // A user-provided (or user-replaced) function is not the built-in one, so the fast paths
        // must not run it speculatively.
        self.builtin = false;
        self
    }

    /// Set the input coercion function for runtime values.
    #[must_use]
    pub fn parse_value(
        mut self,
        function: impl Fn(&Value) -> Result<Value, String> + Send + Sync + 'static,
    ) -> Self {
        self.parse_value = Some(Arc::new(function));
        self
    }

    /// Set the input coercion function for literals.
    #[must_use]
    pub fn parse_literal(
        mut self,
        function: impl for<'a> Fn(&ast::Value<'a>) -> Result<Value, String> + Send + Sync + 'static,
    ) -> Self {
        self.parse_literal = Some(Arc::new(function));
        self
    }

    /// The built-in `String` scalar.
    #[must_use]
    pub fn string() -> Self {
        Self::new("String")
            .description("The `String` scalar type represents textual data, represented as UTF-8 character sequences. The String type is most often used by GraphQL to represent free-form human-readable text.")
            .serialize(|value| match value {
                Value::String(value) => Ok(Value::String(value.clone())),
                _ => Err(format!("String cannot represent a value of type {}", value.kind())),
            })
            .parse_value(|value| match value {
                Value::String(value) => Ok(Value::String(value.clone())),
                _ => Err(format!("String cannot represent a value of type {}", value.kind())),
            })
            .parse_literal(|value| match value {
                ast::Value::String(value) => Ok(Value::String(Arc::from(value.as_ref()))),
                _ => Err("String cannot represent a non-string literal".into()),
            })
            .builtin()
    }

    /// The built-in `Int` scalar.
    #[must_use]
    pub fn int() -> Self {
        Self::new("Int")
            .description("The `Int` scalar type represents non-fractional signed whole numeric values. Int can represent values between -(2^31) and 2^31 - 1.")
            .serialize(|value| match value {
                Value::Int(value) => Ok(Value::Int(*value)),
                _ => Err(format!("Int cannot represent a value of type {}", value.kind())),
            })
            .parse_value(|value| match value {
                Value::Int(value) => Ok(Value::Int(*value)),
                _ => Err(format!("Int cannot represent a value of type {}", value.kind())),
            })
            .parse_literal(|value| match value {
                ast::Value::Int(value) => i32::try_from(*value)
                    .map(Value::Int)
                    .map_err(|_| "Int cannot represent non 32-bit signed integer value".into()),
                _ => Err("Int cannot represent a non-integer literal".into()),
            })
            .builtin()
    }

    /// The built-in `Float` scalar.
    #[must_use]
    pub fn float() -> Self {
        Self::new("Float")
            .description("The `Float` scalar type represents signed double-precision fractional values as specified by IEEE 754.")
            .serialize(|value| match value {
                Value::Float(value) if value.is_finite() => Ok(Value::Float(*value)),
                Value::Int(value) => Ok(Value::Float(f64::from(*value))),
                Value::Float(_) => Err("Float cannot represent a non-finite value".into()),
                _ => Err(format!("Float cannot represent a value of type {}", value.kind())),
            })
            .parse_value(|value| match value {
                Value::Float(value) if value.is_finite() => Ok(Value::Float(*value)),
                Value::Int(value) => Ok(Value::Float(f64::from(*value))),
                Value::Float(_) => Err("Float cannot represent a non-finite value".into()),
                _ => Err(format!("Float cannot represent a value of type {}", value.kind())),
            })
            .parse_literal(|value| match value {
                ast::Value::Float(value) if value.is_finite() => Ok(Value::Float(*value)),
                ast::Value::Float(_) => Err("Float cannot represent a non-finite value".into()),
                ast::Value::Int(value) => Ok(Value::Float(*value as f64)),
                _ => Err("Float cannot represent a non-numeric literal".into()),
            })
            .builtin()
    }

    /// The built-in `Boolean` scalar.
    #[must_use]
    pub fn boolean() -> Self {
        Self::new("Boolean")
            .description("The `Boolean` scalar type represents `true` or `false`.")
            .serialize(|value| match value {
                Value::Boolean(value) => Ok(Value::Boolean(*value)),
                _ => Err(format!(
                    "Boolean cannot represent a value of type {}",
                    value.kind()
                )),
            })
            .parse_value(|value| match value {
                Value::Boolean(value) => Ok(Value::Boolean(*value)),
                _ => Err(format!(
                    "Boolean cannot represent a value of type {}",
                    value.kind()
                )),
            })
            .parse_literal(|value| match value {
                ast::Value::Boolean(value) => Ok(Value::Boolean(*value)),
                _ => Err("Boolean cannot represent a non-boolean literal".into()),
            })
            .builtin()
    }

    /// The built-in `ID` scalar.
    ///
    /// The specification serializes IDs as strings even when provided as integers.
    #[must_use]
    pub fn id() -> Self {
        Self::new("ID")
            .description("The `ID` scalar type represents a unique identifier, often used to refetch an object or as key for a cache. The ID type appears in a JSON response as a String; however, it is not intended to be human-readable. When expected as an input type, any string (such as `\"4\"`) or integer (such as `4`) input value will be accepted as an ID.")
            .serialize(|value| match value {
                Value::String(value) | Value::Enum(value) => Ok(Value::String(value.clone())),
                Value::Int(value) => Ok(Value::String(Arc::from(value.to_string()))),
                _ => Err(format!("ID cannot represent a value of type {}", value.kind())),
            })
            .parse_value(|value| match value {
                Value::String(value) | Value::Enum(value) => Ok(Value::String(value.clone())),
                Value::Int(value) => Ok(Value::String(Arc::from(value.to_string()))),
                _ => Err(format!("ID cannot represent a value of type {}", value.kind())),
            })
            .parse_literal(|value| match value {
                ast::Value::String(value) => Ok(Value::String(Arc::from(value.as_ref()))),
                ast::Value::Int(value) => Ok(Value::String(Arc::from(value.to_string()))),
                _ => Err("ID cannot represent a non-string and non-integer literal".into()),
            })
            .builtin()
    }

    /// The output coercion function, if one was registered.
    #[must_use]
    pub fn serialize_fn(&self) -> Option<&ScalarSerializeFn> {
        self.serialize.as_ref()
    }

    /// The runtime input coercion function, if one was registered.
    #[must_use]
    pub fn parse_value_fn(&self) -> Option<&ScalarParseValueFn> {
        self.parse_value.as_ref()
    }

    /// The literal input coercion function, if one was registered.
    #[must_use]
    pub fn parse_literal_fn(&self) -> Option<&ScalarParseLiteralFn> {
        self.parse_literal.as_ref()
    }
}

/// One value of an enum type.
#[derive(Clone, Debug, PartialEq)]
pub struct EnumValue {
    /// The value's name.
    pub name: Name,
    /// An optional description.
    pub description: Option<Arc<str>>,
    /// Why the value is deprecated, when it is.
    pub deprecation_reason: Option<Arc<str>>,
    /// Directives applied to the value, in application order.
    pub directives: Vec<AppliedDirective>,
}

impl EnumValue {
    /// Add an applied directive.
    #[must_use]
    pub fn directive(mut self, directive: AppliedDirective) -> Self {
        self.directives.push(directive);
        self
    }
}

/// An enum type.
#[derive(Clone, Debug)]
pub struct EnumType {
    /// The enum's name.
    pub name: Name,
    /// An optional description.
    pub description: Option<Arc<str>>,
    /// Values in definition order.
    pub values: IndexMap<Name, EnumValue>,
    /// Directives applied to the enum, in application order.
    pub directives: Vec<AppliedDirective>,
    /// Duplicate value names encountered while building, reported by schema validation.
    pub(crate) duplicates: Vec<Name>,
}

impl EnumType {
    /// Start defining an enum.
    #[must_use]
    pub fn new(name: impl Into<Name>) -> Self {
        Self {
            name: name.into(),
            description: None,
            values: IndexMap::new(),
            directives: Vec::new(),
            duplicates: Vec::new(),
        }
    }

    /// Set the description.
    #[must_use]
    pub fn description(mut self, description: impl Into<Arc<str>>) -> Self {
        self.description = Some(description.into());
        self
    }

    /// Add an applied directive.
    #[must_use]
    pub fn directive(mut self, directive: AppliedDirective) -> Self {
        self.directives.push(directive);
        self
    }

    /// Add a value.
    #[must_use]
    pub fn value(mut self, name: impl Into<Name>) -> Self {
        let name = name.into();
        if self.values.contains_key(&name) {
            self.duplicates.push(name.clone());
        }
        self.values.insert(
            name.clone(),
            EnumValue {
                name,
                description: None,
                deprecation_reason: None,
                directives: Vec::new(),
            },
        );
        self
    }

    /// Add a described value.
    #[must_use]
    pub fn value_with(
        mut self,
        name: impl Into<Name>,
        description: Option<Arc<str>>,
        deprecation_reason: Option<Arc<str>>,
    ) -> Self {
        let name = name.into();
        if self.values.contains_key(&name) {
            self.duplicates.push(name.clone());
        }
        self.values.insert(
            name.clone(),
            EnumValue {
                name,
                description,
                deprecation_reason,
                directives: Vec::new(),
            },
        );
        self
    }

    /// Whether a value name is defined.
    #[must_use]
    pub fn has_value(&self, name: &str) -> bool {
        map_get(&self.values, name).is_some()
    }
}

/// An input value: a field argument or an input object field.
#[derive(Clone, Debug)]
pub struct InputValue {
    /// The name.
    pub name: Name,
    /// An optional description.
    pub description: Option<Arc<str>>,
    /// The input type.
    pub ty: TypeRef,
    /// The default value, already in runtime form.
    pub default_value: Option<Value>,
    /// Whether the input value is deprecated.
    pub deprecation_reason: Option<Arc<str>>,
    /// Directives applied to the input value, in application order.
    pub directives: Vec<AppliedDirective>,
}

impl InputValue {
    /// Start defining an input value.
    #[must_use]
    pub fn new(name: impl Into<Name>, ty: TypeRef) -> Self {
        Self {
            name: name.into(),
            description: None,
            ty,
            default_value: None,
            deprecation_reason: None,
            directives: Vec::new(),
        }
    }

    /// Set the description.
    #[must_use]
    pub fn description(mut self, description: impl Into<Arc<str>>) -> Self {
        self.description = Some(description.into());
        self
    }

    /// Add an applied directive.
    #[must_use]
    pub fn directive(mut self, directive: AppliedDirective) -> Self {
        self.directives.push(directive);
        self
    }

    /// Set the default value.
    #[must_use]
    pub fn default_value(mut self, value: impl Into<Value>) -> Self {
        self.default_value = Some(value.into());
        self
    }

    /// Mark the input value as deprecated.
    #[must_use]
    pub fn deprecation_reason(mut self, reason: impl Into<Arc<str>>) -> Self {
        self.deprecation_reason = Some(reason.into());
        self
    }
}

/// An input object type.
#[derive(Clone, Debug)]
pub struct InputObjectType {
    /// The type's name.
    pub name: Name,
    /// An optional description.
    pub description: Option<Arc<str>>,
    /// Fields in definition order.
    pub fields: IndexMap<Name, InputValue>,
    /// Whether this is a `@oneOf` input object.
    pub one_of: bool,
    /// Directives applied to the input object, in application order.
    pub directives: Vec<AppliedDirective>,
    /// Duplicate field names encountered while building, reported by schema validation.
    pub(crate) duplicates: Vec<Name>,
}

impl InputObjectType {
    /// Start defining an input object.
    #[must_use]
    pub fn new(name: impl Into<Name>) -> Self {
        Self {
            name: name.into(),
            description: None,
            fields: IndexMap::new(),
            one_of: false,
            directives: Vec::new(),
            duplicates: Vec::new(),
        }
    }

    /// Set the description.
    #[must_use]
    pub fn description(mut self, description: impl Into<Arc<str>>) -> Self {
        self.description = Some(description.into());
        self
    }

    /// Add an applied directive.
    #[must_use]
    pub fn directive(mut self, directive: AppliedDirective) -> Self {
        self.directives.push(directive);
        self
    }

    /// Add a field.
    #[must_use]
    pub fn field(mut self, field: InputValue) -> Self {
        if self.fields.contains_key(&field.name) {
            self.duplicates.push(field.name.clone());
        }
        self.fields.insert(field.name.clone(), field);
        self
    }

    /// Insert a field into an existing input object definition, reporting duplicates through
    /// schema validation.
    pub fn insert_field(&mut self, field: InputValue) {
        if self.fields.contains_key(&field.name) {
            self.duplicates.push(field.name.clone());
        }
        self.fields.insert(field.name.clone(), field);
    }

    /// Mark the input object as `@oneOf`.
    #[must_use]
    pub fn one_of(mut self) -> Self {
        self.one_of = true;
        self
    }
}

/// A field definition.
#[derive(Clone)]
pub struct Field {
    /// The field's name.
    pub name: Name,
    /// An optional description.
    pub description: Option<Arc<str>>,
    /// Arguments in definition order.
    pub args: IndexMap<Name, InputValue>,
    /// The field's output type.
    pub ty: TypeRef,
    /// Why the field is deprecated, when it is.
    pub deprecation_reason: Option<Arc<str>>,
    /// Directives applied to the field, in application order.
    pub directives: Vec<AppliedDirective>,
    /// The field's resolver. `None` uses the default property resolver.
    pub resolve: Option<ResolveFn>,
    /// The field's subscription resolver, used when the field is a subscription root field.
    pub subscribe: Option<SubscribeFn>,
    /// An optional complexity estimator for this field.
    pub complexity: Option<ComplexityFn>,
    /// Duplicate argument names encountered while building, reported by schema validation.
    pub(crate) duplicate_args: Vec<Name>,
    /// Where the output type's innermost named type lives in the schema's type map.
    ///
    /// Execution needs the resolved type for every field it completes, and a positional lookup
    /// avoids hashing the type name each time. `None` means the reference was never resolved (the
    /// field does not belong to a built schema, or names a type the schema does not define).
    pub(crate) type_index: Option<u32>,
}

impl fmt::Debug for Field {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("Field")
            .field("name", &self.name)
            .field("description", &self.description)
            .field("args", &self.args)
            .field("ty", &self.ty)
            .field("deprecation_reason", &self.deprecation_reason)
            .field("directives", &self.directives)
            .field("resolve", &self.resolve.is_some())
            .field("subscribe", &self.subscribe.is_some())
            .finish()
    }
}

impl Field {
    /// Start defining a field.
    #[must_use]
    pub fn new(name: impl Into<Name>, ty: TypeRef) -> Self {
        Self {
            name: name.into(),
            description: None,
            args: IndexMap::new(),
            ty,
            deprecation_reason: None,
            directives: Vec::new(),
            resolve: None,
            subscribe: None,
            complexity: None,
            duplicate_args: Vec::new(),
            type_index: None,
        }
    }

    /// Set the description.
    #[must_use]
    pub fn description(mut self, description: impl Into<Arc<str>>) -> Self {
        self.description = Some(description.into());
        self
    }

    /// Add an argument.
    #[must_use]
    pub fn arg(mut self, argument: InputValue) -> Self {
        if self.args.contains_key(&argument.name) {
            self.duplicate_args.push(argument.name.clone());
        }
        self.args.insert(argument.name.clone(), argument);
        self
    }

    /// Mark the field as deprecated.
    #[must_use]
    pub fn deprecation_reason(mut self, reason: impl Into<Arc<str>>) -> Self {
        self.deprecation_reason = Some(reason.into());
        self
    }

    /// Add an applied directive.
    #[must_use]
    pub fn directive(mut self, directive: AppliedDirective) -> Self {
        self.directives.push(directive);
        self
    }

    /// Set the resolver.
    #[must_use]
    pub fn resolve(
        mut self,
        resolve: impl for<'a> Fn(ResolverContext<'a>) -> ResolveFuture<'a> + Send + Sync + 'static,
    ) -> Self {
        self.resolve = Some(Arc::new(resolve));
        self
    }

    /// Set the subscription resolver.
    #[must_use]
    pub fn subscribe(
        mut self,
        subscribe: impl for<'a> Fn(
            ResolverContext<'a>,
        ) -> futures::future::BoxFuture<
            'a,
            Result<SubscriptionStream, GraphQLError>,
        > + Send
        + Sync
        + 'static,
    ) -> Self {
        self.subscribe = Some(Arc::new(subscribe));
        self
    }

    /// Set the field's complexity estimator.
    #[must_use]
    pub fn complexity(
        mut self,
        estimator: impl Fn(&crate::Object) -> u32 + Send + Sync + 'static,
    ) -> Self {
        self.complexity = Some(Arc::new(estimator));
        self
    }
}

/// The signature of a field resolver.
pub type ResolveFn =
    Arc<dyn for<'a> Fn(ResolverContext<'a>) -> ResolveFuture<'a> + Send + Sync + 'static>;

/// The signature of a subscription resolver.
pub type SubscribeFn = Arc<
    dyn for<'a> Fn(
            ResolverContext<'a>,
        )
            -> futures::future::BoxFuture<'a, Result<SubscriptionStream, GraphQLError>>
        + Send
        + Sync
        + 'static,
>;

/// The signature of a field complexity estimator.
pub type ComplexityFn = Arc<dyn Fn(&crate::Object) -> u32 + Send + Sync + 'static>;

/// Information about the field being resolved.
#[derive(Debug)]
pub struct ResolveInfo<'a> {
    /// The field's name in the schema.
    pub field_name: &'a str,
    /// The name of the parent type.
    pub parent_type: &'a str,
    /// The field's declared return type.
    pub return_type: &'a TypeRef,
    /// The path from the response root to this field.
    pub path: &'a [PathSegment],
    /// The schema being executed.
    pub schema: &'a Schema,
}

/// Everything a resolver can see about the current field.
pub struct ResolverContext<'a> {
    /// The value of the parent object, or the operation root value.
    pub parent: &'a Value,
    /// Coerced arguments, with defaults applied.
    pub args: &'a crate::Object,
    /// Per-request execution state.
    pub context: &'a ExecutionContext,
    /// Static and dynamic information about the field.
    pub info: &'a ResolveInfo<'a>,
}

impl fmt::Debug for ResolverContext<'_> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("ResolverContext")
            .field("parent", &self.parent)
            .field("args", &self.args)
            .field("info", &self.info)
            .finish_non_exhaustive()
    }
}

/// The signature of an abstract type resolver.
pub type ResolveTypeFn =
    Arc<dyn Fn(&Value, &ExecutionContext) -> Result<Name, GraphQLError> + Send + Sync + 'static>;

/// An object type.
#[derive(Clone)]
pub struct ObjectType {
    /// The type's name.
    pub name: Name,
    /// An optional description.
    pub description: Option<Arc<str>>,
    /// Fields in definition order.
    pub fields: IndexMap<Name, Field>,
    /// Names of interfaces implemented by this object.
    pub interfaces: Vec<Name>,
    /// Directives applied to the object, in application order.
    pub directives: Vec<AppliedDirective>,
    /// Optional runtime type check used when resolving abstract types.
    pub is_type_of: Option<Arc<dyn Fn(&Value) -> bool + Send + Sync>>,
    /// Duplicate field names encountered while building, reported by schema validation.
    pub(crate) duplicates: Vec<Name>,
}

impl fmt::Debug for ObjectType {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("ObjectType")
            .field("name", &self.name)
            .field("description", &self.description)
            .field("fields", &self.fields)
            .field("interfaces", &self.interfaces)
            .field("directives", &self.directives)
            .finish_non_exhaustive()
    }
}

impl ObjectType {
    /// Start defining an object type.
    #[must_use]
    pub fn new(name: impl Into<Name>) -> Self {
        Self {
            name: name.into(),
            description: None,
            fields: IndexMap::new(),
            interfaces: Vec::new(),
            directives: Vec::new(),
            is_type_of: None,
            duplicates: Vec::new(),
        }
    }

    /// Set the description.
    #[must_use]
    pub fn description(mut self, description: impl Into<Arc<str>>) -> Self {
        self.description = Some(description.into());
        self
    }

    /// Add an applied directive.
    #[must_use]
    pub fn directive(mut self, directive: AppliedDirective) -> Self {
        self.directives.push(directive);
        self
    }

    /// Add a field.
    #[must_use]
    pub fn field(mut self, field: Field) -> Self {
        if self.fields.contains_key(&field.name) {
            self.duplicates.push(field.name.clone());
        }
        self.fields.insert(field.name.clone(), field);
        self
    }

    /// Add an implemented interface by name.
    #[must_use]
    pub fn implements(mut self, interface: impl Into<Name>) -> Self {
        self.interfaces.push(interface.into());
        self
    }

    /// Set the runtime type check.
    #[must_use]
    pub fn is_type_of(mut self, check: impl Fn(&Value) -> bool + Send + Sync + 'static) -> Self {
        self.is_type_of = Some(Arc::new(check));
        self
    }

    /// Insert a field into an existing object definition, reporting duplicates through schema
    /// validation.
    pub fn insert_field(&mut self, field: Field) {
        if self.fields.contains_key(&field.name) {
            self.duplicates.push(field.name.clone());
        }
        self.fields.insert(field.name.clone(), field);
    }

    /// Look up a field by name.
    ///
    /// Most object types declare a handful of fields, where comparing interned names in order
    /// beats computing a hash for every lookup — and execution looks up a field's definition for
    /// every field it completes. Larger types keep the hash map's constant lookup.
    #[must_use]
    pub fn get_field(&self, name: &str) -> Option<&Field> {
        map_get(&self.fields, name)
    }
}

/// An interface type.
#[derive(Clone)]
pub struct InterfaceType {
    /// The type's name.
    pub name: Name,
    /// An optional description.
    pub description: Option<Arc<str>>,
    /// Fields in definition order.
    pub fields: IndexMap<Name, Field>,
    /// Names of interfaces implemented by this interface.
    pub interfaces: Vec<Name>,
    /// Directives applied to the interface, in application order.
    pub directives: Vec<AppliedDirective>,
    /// Resolves a runtime value to the name of its concrete object type.
    pub resolve_type: Option<ResolveTypeFn>,
    /// Duplicate field names encountered while building, reported by schema validation.
    pub(crate) duplicates: Vec<Name>,
}

impl fmt::Debug for InterfaceType {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("InterfaceType")
            .field("name", &self.name)
            .field("description", &self.description)
            .field("fields", &self.fields)
            .field("interfaces", &self.interfaces)
            .field("directives", &self.directives)
            .finish_non_exhaustive()
    }
}

impl InterfaceType {
    /// Start defining an interface.
    #[must_use]
    pub fn new(name: impl Into<Name>) -> Self {
        Self {
            name: name.into(),
            description: None,
            fields: IndexMap::new(),
            interfaces: Vec::new(),
            directives: Vec::new(),
            resolve_type: None,
            duplicates: Vec::new(),
        }
    }

    /// Set the description.
    #[must_use]
    pub fn description(mut self, description: impl Into<Arc<str>>) -> Self {
        self.description = Some(description.into());
        self
    }

    /// Add an applied directive.
    #[must_use]
    pub fn directive(mut self, directive: AppliedDirective) -> Self {
        self.directives.push(directive);
        self
    }

    /// Add a field definition.
    #[must_use]
    pub fn field(mut self, field: Field) -> Self {
        if self.fields.contains_key(&field.name) {
            self.duplicates.push(field.name.clone());
        }
        self.fields.insert(field.name.clone(), field);
        self
    }

    /// Add an implemented interface by name.
    #[must_use]
    pub fn implements(mut self, interface: impl Into<Name>) -> Self {
        self.interfaces.push(interface.into());
        self
    }

    /// Insert a field into an existing interface definition, reporting duplicates through schema
    /// validation.
    pub fn insert_field(&mut self, field: Field) {
        if self.fields.contains_key(&field.name) {
            self.duplicates.push(field.name.clone());
        }
        self.fields.insert(field.name.clone(), field);
    }

    /// Set the abstract type resolver.
    #[must_use]
    pub fn resolve_type(
        mut self,
        resolve: impl Fn(&Value, &ExecutionContext) -> Result<Name, GraphQLError>
        + Send
        + Sync
        + 'static,
    ) -> Self {
        self.resolve_type = Some(Arc::new(resolve));
        self
    }
    /// Look up a field by name.
    #[must_use]
    pub fn get_field(&self, name: &str) -> Option<&Field> {
        map_get(&self.fields, name)
    }
}

/// A union type.
#[derive(Clone)]
pub struct UnionType {
    /// The type's name.
    pub name: Name,
    /// An optional description.
    pub description: Option<Arc<str>>,
    /// Member object type names.
    pub types: Vec<Name>,
    /// Directives applied to the union, in application order.
    pub directives: Vec<AppliedDirective>,
    /// Resolves a runtime value to the name of its concrete object type.
    pub resolve_type: Option<ResolveTypeFn>,
}

impl fmt::Debug for UnionType {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("UnionType")
            .field("name", &self.name)
            .field("description", &self.description)
            .field("types", &self.types)
            .field("directives", &self.directives)
            .finish_non_exhaustive()
    }
}

impl UnionType {
    /// Start defining a union.
    #[must_use]
    pub fn new(name: impl Into<Name>) -> Self {
        Self {
            name: name.into(),
            description: None,
            types: Vec::new(),
            directives: Vec::new(),
            resolve_type: None,
        }
    }

    /// Set the description.
    #[must_use]
    pub fn description(mut self, description: impl Into<Arc<str>>) -> Self {
        self.description = Some(description.into());
        self
    }

    /// Add an applied directive.
    #[must_use]
    pub fn directive(mut self, directive: AppliedDirective) -> Self {
        self.directives.push(directive);
        self
    }

    /// Add a member object type by name.
    #[must_use]
    pub fn member(mut self, ty: impl Into<Name>) -> Self {
        self.types.push(ty.into());
        self
    }

    /// Set the abstract type resolver.
    #[must_use]
    pub fn resolve_type(
        mut self,
        resolve: impl Fn(&Value, &ExecutionContext) -> Result<Name, GraphQLError>
        + Send
        + Sync
        + 'static,
    ) -> Self {
        self.resolve_type = Some(Arc::new(resolve));
        self
    }
}

/// A named type definition.
#[derive(Clone, Debug)]
pub enum Type {
    /// A scalar.
    Scalar(Arc<ScalarType>),
    /// An object.
    Object(Arc<ObjectType>),
    /// An interface.
    Interface(Arc<InterfaceType>),
    /// A union.
    Union(Arc<UnionType>),
    /// An enum.
    Enum(Arc<EnumType>),
    /// An input object.
    InputObject(Arc<InputObjectType>),
}

impl Type {
    /// The type's name.
    #[must_use]
    pub fn name(&self) -> &Name {
        match self {
            Self::Scalar(ty) => &ty.name,
            Self::Object(ty) => &ty.name,
            Self::Interface(ty) => &ty.name,
            Self::Union(ty) => &ty.name,
            Self::Enum(ty) => &ty.name,
            Self::InputObject(ty) => &ty.name,
        }
    }

    /// The type's kind.
    #[must_use]
    pub const fn kind(&self) -> TypeKind {
        match self {
            Self::Scalar(_) => TypeKind::Scalar,
            Self::Object(_) => TypeKind::Object,
            Self::Interface(_) => TypeKind::Interface,
            Self::Union(_) => TypeKind::Union,
            Self::Enum(_) => TypeKind::Enum,
            Self::InputObject(_) => TypeKind::InputObject,
        }
    }

    /// Whether the type may be used as an input type.
    #[must_use]
    pub const fn is_input_type(&self) -> bool {
        matches!(self, Self::Scalar(_) | Self::Enum(_) | Self::InputObject(_))
    }

    /// Whether the type may be used as an output type.
    #[must_use]
    pub const fn is_output_type(&self) -> bool {
        !matches!(self, Self::InputObject(_))
    }

    /// Whether the type is a leaf type (scalar or enum).
    #[must_use]
    pub const fn is_leaf(&self) -> bool {
        matches!(self, Self::Scalar(_) | Self::Enum(_))
    }

    /// Whether the type is a composite type (object, interface or union).
    #[must_use]
    pub const fn is_composite(&self) -> bool {
        matches!(self, Self::Object(_) | Self::Interface(_) | Self::Union(_))
    }

    /// Borrow as a scalar, when this is one.
    #[must_use]
    pub fn as_scalar(&self) -> Option<&Arc<ScalarType>> {
        match self {
            Self::Scalar(ty) => Some(ty),
            _ => None,
        }
    }

    /// Borrow as an object, when this is one.
    #[must_use]
    pub fn as_object(&self) -> Option<&Arc<ObjectType>> {
        match self {
            Self::Object(ty) => Some(ty),
            _ => None,
        }
    }

    /// Borrow as an interface, when this is one.
    #[must_use]
    pub fn as_interface(&self) -> Option<&Arc<InterfaceType>> {
        match self {
            Self::Interface(ty) => Some(ty),
            _ => None,
        }
    }

    /// Borrow as a union, when this is one.
    #[must_use]
    pub fn as_union(&self) -> Option<&Arc<UnionType>> {
        match self {
            Self::Union(ty) => Some(ty),
            _ => None,
        }
    }

    /// Borrow as an enum, when this is one.
    #[must_use]
    pub fn as_enum(&self) -> Option<&Arc<EnumType>> {
        match self {
            Self::Enum(ty) => Some(ty),
            _ => None,
        }
    }

    /// Borrow as an input object, when this is one.
    #[must_use]
    pub fn as_input_object(&self) -> Option<&Arc<InputObjectType>> {
        match self {
            Self::InputObject(ty) => Some(ty),
            _ => None,
        }
    }
}

impl From<ScalarType> for Type {
    fn from(value: ScalarType) -> Self {
        Self::Scalar(Arc::new(value))
    }
}

impl From<ObjectType> for Type {
    fn from(value: ObjectType) -> Self {
        Self::Object(Arc::new(value))
    }
}

impl From<InterfaceType> for Type {
    fn from(value: InterfaceType) -> Self {
        Self::Interface(Arc::new(value))
    }
}

impl From<UnionType> for Type {
    fn from(value: UnionType) -> Self {
        Self::Union(Arc::new(value))
    }
}

impl From<EnumType> for Type {
    fn from(value: EnumType) -> Self {
        Self::Enum(Arc::new(value))
    }
}

impl From<InputObjectType> for Type {
    fn from(value: InputObjectType) -> Self {
        Self::InputObject(Arc::new(value))
    }
}

/// A directive definition.
#[derive(Clone, Debug)]
pub struct DirectiveDefinition {
    /// The directive's name without `@`.
    pub name: Name,
    /// An optional description.
    pub description: Option<Arc<str>>,
    /// Arguments in definition order.
    pub args: IndexMap<Name, InputValue>,
    /// Locations where the directive may appear.
    pub locations: Vec<ast::DirectiveLocation>,
    /// Whether the directive may be repeated in a single location.
    pub repeatable: bool,
    /// Why the directive is deprecated, when it is.
    pub deprecation_reason: Option<Arc<str>>,
    /// Duplicate argument names encountered while building, reported by schema validation.
    pub(crate) duplicate_args: Vec<Name>,
}

impl DirectiveDefinition {
    /// Start defining a directive.
    #[must_use]
    pub fn new(name: impl Into<Name>) -> Self {
        Self {
            name: name.into(),
            description: None,
            args: IndexMap::new(),
            locations: Vec::new(),
            repeatable: false,
            deprecation_reason: None,
            duplicate_args: Vec::new(),
        }
    }

    /// Set the description.
    #[must_use]
    pub fn description(mut self, description: impl Into<Arc<str>>) -> Self {
        self.description = Some(description.into());
        self
    }

    /// Add an argument.
    #[must_use]
    pub fn arg(mut self, argument: InputValue) -> Self {
        if self.args.contains_key(&argument.name) {
            self.duplicate_args.push(argument.name.clone());
        }
        self.args.insert(argument.name.clone(), argument);
        self
    }

    /// Add a permitted location.
    #[must_use]
    pub fn location(mut self, location: ast::DirectiveLocation) -> Self {
        self.locations.push(location);
        self
    }

    /// Mark the directive as repeatable.
    #[must_use]
    pub fn repeatable(mut self) -> Self {
        self.repeatable = true;
        self
    }

    /// Mark the directive as deprecated.
    #[must_use]
    pub fn deprecation_reason(mut self, reason: impl Into<Arc<str>>) -> Self {
        self.deprecation_reason = Some(reason.into());
        self
    }

    /// The built-in `@skip` directive.
    #[must_use]
    pub fn skip() -> Self {
        Self::new("skip")
            .description("Directs the executor to skip this field or fragment when the `if` argument is true.")
            .arg(
                InputValue::new("if", TypeRef::boolean().non_null())
                    .description("Skipped when true."),
            )
            .location(ast::DirectiveLocation::Field)
            .location(ast::DirectiveLocation::FragmentSpread)
            .location(ast::DirectiveLocation::InlineFragment)
    }

    /// The built-in `@include` directive.
    #[must_use]
    pub fn include() -> Self {
        Self::new("include")
            .description("Directs the executor to include this field or fragment only when the `if` argument is true.")
            .arg(
                InputValue::new("if", TypeRef::boolean().non_null())
                    .description("Included when true."),
            )
            .location(ast::DirectiveLocation::Field)
            .location(ast::DirectiveLocation::FragmentSpread)
            .location(ast::DirectiveLocation::InlineFragment)
    }

    /// The built-in `@deprecated` directive.
    #[must_use]
    pub fn deprecated() -> Self {
        Self::new("deprecated")
            .description("Marks an element of a GraphQL schema as no longer supported.")
            .arg(
                InputValue::new("reason", TypeRef::string())
                    .description("Explains why this element was deprecated, usually also including a suggestion for how to access supported similar data. Formatted using the Markdown syntax, as specified by [CommonMark](https://commonmark.org/).")
                    .default_value("No longer supported"),
            )
            .location(ast::DirectiveLocation::FieldDefinition)
            .location(ast::DirectiveLocation::ArgumentDefinition)
            .location(ast::DirectiveLocation::InputFieldDefinition)
            .location(ast::DirectiveLocation::EnumValue)
            .location(ast::DirectiveLocation::DirectiveDefinition)
    }

    /// The built-in `@specifiedBy` directive.
    #[must_use]
    pub fn specified_by() -> Self {
        Self::new("specifiedBy")
            .description("Exposes a URL that specifies the behavior of this scalar.")
            .arg(
                InputValue::new("url", TypeRef::string().non_null())
                    .description("The URL that specifies the behavior of this scalar."),
            )
            .location(ast::DirectiveLocation::Scalar)
    }

    /// The `@oneOf` directive for input objects.
    #[must_use]
    pub fn one_of() -> Self {
        Self::new("oneOf")
            .description(
                "Indicates exactly one field must be supplied and this field must not be `null`.",
            )
            .location(ast::DirectiveLocation::InputObject)
    }
}
