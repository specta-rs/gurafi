//! The traits that connect Rust types to GraphQL types.
//!
//! Every trait in this module is implemented by a derive macro or by hand for the built-in
//! types. Together they are what makes a `gurafi` schema type-driven: a field's GraphQL type is
//! derived from its Rust return type and its GraphQL arguments are derived from its Rust `Args`
//! type.

use crate::registry::TypeRegistry;
use gql::{GraphQLError, TypeRef, Value};

/// A Rust type that may appear in an output position.
///
/// The mapping is:
///
/// | Rust type | GraphQL type |
/// | --- | --- |
/// | `String`, `&str`, `Cow<str>` | `String!` |
/// | `bool` | `Boolean!` |
/// | `i8`, `i16`, `i32`, `u8`, `u16` | `Int!` |
/// | `f32`, `f64` | `Float!` |
/// | [`crate::ID`] | `ID!` |
/// | `Option<T>` | `T` with the outer non-null removed |
/// | `Vec<T>` | `[T]!`, keeping `T`'s own nullability |
/// | `Box<T>`, `Arc<T>` | `T` |
/// | a derived object, enum, scalar or union | its GraphQL name |
///
/// There is deliberately no mapping from `Result<T, E>`: a fallible field is declared with one
/// of the `*_result` methods so the schema builder can tell the two cases apart.
pub trait OutputType {
    /// The GraphQL type of values produced by this Rust type.
    fn output_type() -> TypeRef;

    /// Whether the Rust type is an `Option`.
    ///
    /// This is what lets [`crate::FieldConfig::non_null`] reject an override that claims a field
    /// is non-null while its resolver can still return `None`. It is hidden from the docs because
    /// it is an implementation detail of the builder, not something a schema author writes.
    #[doc(hidden)]
    fn is_optional() -> bool {
        false
    }

    /// Register every named type this type depends on with the schema being built.
    #[doc(hidden)]
    fn register_output(registry: &mut TypeRegistry) {
        let _ = registry;
    }
}

/// A Rust type that may appear in an input position.
///
/// The mapping mirrors [`OutputType`]. `Option<T>` makes a field optional in the GraphQL schema,
/// and `Vec<T>` becomes a list.
pub trait InputType {
    /// The GraphQL input type for values of this Rust type.
    fn input_type() -> TypeRef;

    /// Register every named type this type depends on with the schema being built.
    #[doc(hidden)]
    fn register_input(registry: &mut TypeRegistry) {
        let _ = registry;
    }
}

/// Convert a Rust value into the executor's value model.
///
/// Most conversions cannot fail. Custom scalars can: their output form may need validation, and
/// a failure is reported as a field error rather than silently serialised as `null`.
pub trait IntoValue {
    /// Convert `self` into a GraphQL value.
    ///
    /// # Errors
    ///
    /// Returns an error when a custom scalar cannot represent the value.
    fn into_value(self) -> Result<Value, GraphQLError>;
}

/// Convert an input value into a Rust type.
pub trait FromValue: Sized {
    /// Convert a GraphQL value into `Self`.
    ///
    /// # Errors
    ///
    /// Returns an error when the value does not match the type. The executor turns this into a
    /// normal per-field GraphQL error.
    fn from_value(value: &Value) -> Result<Self, GraphQLError>;
}

/// A Rust struct that is a GraphQL object type.
///
/// Implemented by `#[derive(Object)]`. The resolver for each field is declared on the schema
/// builder, not here, because fields need access to context, arguments and authorization.
pub trait ObjectModel: Sized + Send + Sync + 'static {
    /// The GraphQL type name.
    const NAME: &'static str;

    /// The type's description, if any.
    fn description() -> Option<&'static str> {
        None
    }
}

/// A Rust enum that is a GraphQL enum.
///
/// Implemented by `#[derive(Enum)]`.
pub trait EnumModel: Sized + Send + Sync + 'static {
    /// The GraphQL type name.
    const NAME: &'static str;

    /// The type's description, if any.
    fn description() -> Option<&'static str> {
        None
    }

    /// Every value, in declaration order.
    fn values() -> Vec<gql::EnumValue>;

    /// The GraphQL name of this value.
    fn value_name(&self) -> &'static str;

    /// Look up a value by its GraphQL name.
    fn from_name(name: &str) -> Option<Self>;
}

/// A Rust enum that is a GraphQL union.
///
/// Implemented by `#[derive(Union)]`. The union's members are still declared on the schema with
/// [`crate::Schema::union`], where the rest of the schema is visible.
pub trait UnionModel: Sized + Send + Sync + 'static {
    /// The GraphQL type name.
    const NAME: &'static str;

    /// The type's description, if any.
    fn description() -> Option<&'static str> {
        None
    }
}

/// A Rust newtype that is a custom GraphQL scalar.
///
/// Implement it by hand when the scalar's representation is not simply its inner type's; the
/// `#[derive(Scalar)]` macro implements it for newtypes that delegate to their inner type. A
/// manual implementation also needs the conversion impls; the [`crate::impl_scalar_traits`]
/// macro writes them.
pub trait ScalarValue: Sized + Send + Sync + 'static {
    /// The GraphQL type name.
    const NAME: &'static str;

    /// The type's description, if any.
    fn description() -> Option<&'static str> {
        None
    }

    /// The `@specifiedBy` URL, if any.
    fn specified_by() -> Option<&'static str> {
        None
    }

    /// Serialize this value into its GraphQL representation.
    ///
    /// # Errors
    ///
    /// Returns an error when the value cannot be represented. It becomes a field error.
    fn to_value(&self) -> Result<Value, GraphQLError>;

    /// Parse an input value into `Self`.
    ///
    /// # Errors
    ///
    /// Returns an error when the value is not a valid scalar value.
    fn from_value(value: &Value) -> Result<Self, GraphQLError>;

    /// Coerce a GraphQL literal into the scalar's runtime representation.
    ///
    /// The default accepts the ordinary scalar literals. Override it for scalars whose literals
    /// need a different representation.
    ///
    /// # Errors
    ///
    /// Returns an error when the literal is not valid for this scalar.
    fn from_literal(literal: &gql::ast::Value<'_>) -> Result<Value, GraphQLError> {
        crate::value::literal_to_value(literal)
    }
}

/// A Rust struct that is a GraphQL input object, and can be used as a field's arguments.
///
/// Implemented by `#[derive(Input)]`. Use [`crate::NoArgs`] (which is `()`) for fields that
/// take no arguments.
pub trait InputObject: Sized + Send + 'static {
    /// The GraphQL input object name, or `None` when the type only describes arguments.
    ///
    /// `#[derive(Input)]` deliberately produces `None` unless the type carries
    /// `#[graphql(name = "...")]`. An args type is not a named input object: its fields become
    /// the field's arguments, and no `FilterInput` type leaks into the schema on its own. Add a
    /// name when the type is also used nested inside another input object (or by several
    /// fields), so it becomes a reusable GraphQL input object.
    const NAME: Option<&'static str>;

    /// The type's description, if any.
    fn description() -> Option<&'static str> {
        None
    }

    /// Whether this input object is a `@oneOf` input.
    ///
    /// A one-of input has only nullable fields, no defaults, and requires callers to provide
    /// exactly one non-null field. Set it with `#[graphql(one_of)]` (which also requires a
    /// GraphQL name, because the type must exist in the schema to carry the directive).
    fn one_of() -> bool {
        false
    }

    /// Validate coerced input after every field has been read.
    ///
    /// Called by the schema builder after [`InputObject::from_object`] and before the resolver.
    /// Implementations can check cross-field rules that individual field coercion cannot express.
    /// The default accepts every value.
    ///
    /// # Errors
    ///
    /// Returns an error when the value is invalid. The error becomes a normal GraphQL field
    /// error with the input type and field path attached.
    fn validate(&self) -> Result<(), GraphQLError> {
        Ok(())
    }

    /// The input fields, with descriptions, defaults and deprecations applied.
    ///
    /// # Errors
    ///
    /// Returns an error when a default value cannot be serialised.
    fn fields() -> Result<Vec<gql::InputValue>, GraphQLError>;

    /// Read the arguments from the executor's coerced argument object.
    ///
    /// # Errors
    ///
    /// Returns an error when a field is missing or has the wrong shape.
    fn from_object(input: &gql::Object) -> Result<Self, GraphQLError>;

    /// Register this type and the nested input types used by its fields.
    ///
    /// Used when the type appears as a field's arguments. [`crate::NoArgs`] registers nothing.
    #[doc(hidden)]
    fn register_args(registry: &mut TypeRegistry) {
        let _ = registry;
    }
}

impl InputObject for () {
    const NAME: Option<&'static str> = None;

    fn fields() -> Result<Vec<gql::InputValue>, GraphQLError> {
        Ok(Vec::new())
    }

    fn from_object(_: &gql::Object) -> Result<Self, GraphQLError> {
        Ok(())
    }
}

/// The arguments type for a field that takes no arguments.
pub type NoArgs = ();

/// A GraphQL abstract type: an interface or a union.
///
/// Values of this type are cheap witnesses returned by the schema builder; they are used by
/// [`crate::FieldConfig::as_abstract`] and by [`crate::ObjectBuilder::implements`].
pub trait AbstractType {
    /// The GraphQL type name.
    fn abstract_name(&self) -> &'static str;
}

/// A safe downcast from the executor's parent value to an object model.
pub(crate) fn model_ref<'a, M: ObjectModel>(
    parent: &'a Value,
    parent_type: &str,
    field_name: &str,
) -> Result<&'a M, GraphQLError> {
    parent.as_external::<M>().ok_or_else(|| {
        GraphQLError::new(format!(
            "Internal error: field \"{parent_type}.{field_name}\" expected a `{}` parent.",
            M::NAME
        ))
    })
}

/// Coerce the executor's argument object into a typed args value, adding field context to any
/// error.
pub(crate) fn coerce_args<A: InputObject>(
    args: &gql::Object,
    parent_type: &str,
    field_name: &str,
) -> Result<A, GraphQLError> {
    let value = A::from_object(args).map_err(|error| {
        GraphQLError::new(format!(
            "Invalid arguments for \"{parent_type}.{field_name}\": {}",
            error.message()
        ))
    })?;
    value.validate().map_err(|error| {
        GraphQLError::new(format!(
            "Invalid arguments for \"{parent_type}.{field_name}\": {}",
            error.message()
        ))
    })?;
    Ok(value)
}
