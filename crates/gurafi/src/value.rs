//! Implementations of the type traits for Rust's standard types.
//!
//! These are the impls that make `String`, `i32`, `Option<T>`, `Vec<T>` and friends usable as
//! GraphQL types without any ceremony.

use std::{borrow::Cow, sync::Arc};

use gql::{GraphQLError, TypeRef, Value};

use crate::traits::{FromValue, InputType, IntoValue, OutputType};

impl OutputType for String {
    fn output_type() -> TypeRef {
        TypeRef::string().non_null()
    }
}

impl IntoValue for String {
    fn into_value(self) -> Result<Value, GraphQLError> {
        Ok(Value::String(Arc::from(self)))
    }
}

impl InputType for String {
    fn input_type() -> TypeRef {
        TypeRef::string().non_null()
    }
}

impl FromValue for String {
    fn from_value(value: &Value) -> Result<Self, GraphQLError> {
        match value {
            Value::String(string) => Ok(string.to_string()),
            other => Err(GraphQLError::new(format!(
                "Expected a string but found {}.",
                other.kind()
            ))),
        }
    }
}

impl OutputType for &str {
    fn output_type() -> TypeRef {
        TypeRef::string().non_null()
    }
}

impl IntoValue for &str {
    fn into_value(self) -> Result<Value, GraphQLError> {
        Ok(Value::String(Arc::from(self)))
    }
}

impl OutputType for Cow<'_, str> {
    fn output_type() -> TypeRef {
        TypeRef::string().non_null()
    }
}

impl IntoValue for Cow<'_, str> {
    fn into_value(self) -> Result<Value, GraphQLError> {
        Ok(Value::String(Arc::from(self.as_ref())))
    }
}

impl OutputType for bool {
    fn output_type() -> TypeRef {
        TypeRef::boolean().non_null()
    }
}

impl IntoValue for bool {
    fn into_value(self) -> Result<Value, GraphQLError> {
        Ok(Value::Boolean(self))
    }
}

impl InputType for bool {
    fn input_type() -> TypeRef {
        TypeRef::boolean().non_null()
    }
}

impl FromValue for bool {
    fn from_value(value: &Value) -> Result<Self, GraphQLError> {
        match value {
            Value::Boolean(boolean) => Ok(*boolean),
            other => Err(GraphQLError::new(format!(
                "Expected a boolean but found {}.",
                other.kind()
            ))),
        }
    }
}

macro_rules! impl_int {
    ($($ty:ty),* $(,)?) => {
        $(
            impl OutputType for $ty {
                fn output_type() -> TypeRef {
                    TypeRef::int().non_null()
                }
            }

            impl IntoValue for $ty {
                fn into_value(self) -> Result<Value, GraphQLError> {
                    Ok(Value::Int(i32::from(self)))
                }
            }

            impl InputType for $ty {
                fn input_type() -> TypeRef {
                    TypeRef::int().non_null()
                }
            }

            impl FromValue for $ty {
                fn from_value(value: &Value) -> Result<Self, GraphQLError> {
                    match value {
                        Value::Int(number) => <$ty>::try_from(*number).map_err(|_| {
                            GraphQLError::new(format!(
                                "{} is outside the range of GraphQL `Int` for `{}`.",
                                number,
                                stringify!($ty)
                            ))
                        }),
                        Value::BigInt(number) => <$ty>::try_from(**number).map_err(|_| {
                            GraphQLError::new(format!(
                                "{} is outside the range of GraphQL `Int` for `{}`.",
                                number,
                                stringify!($ty)
                            ))
                        }),
                        other => Err(GraphQLError::new(format!(
                            "Expected an integer for `{}` but found {}.",
                            stringify!($ty),
                            other.kind()
                        ))),
                    }
                }
            }
        )*
    };
}

impl_int!(i8, i16, i32, u8, u16);

macro_rules! impl_float {
    ($($ty:ty),* $(,)?) => {
        $(
            impl OutputType for $ty {
                fn output_type() -> TypeRef {
                    TypeRef::float().non_null()
                }
            }

            impl IntoValue for $ty {
                fn into_value(self) -> Result<Value, GraphQLError> {
                    Ok(Value::Float(f64::from(self)))
                }
            }

            impl InputType for $ty {
                fn input_type() -> TypeRef {
                    TypeRef::float().non_null()
                }
            }

            impl FromValue for $ty {
                fn from_value(value: &Value) -> Result<Self, GraphQLError> {
                    match value {
                        Value::Float(number) => Ok(*number as $ty),
                        Value::Int(number) => Ok(*number as $ty),
                        Value::BigInt(number) => Ok(**number as $ty),
                        other => Err(GraphQLError::new(format!(
                            "Expected a number for `{}` but found {}.",
                            stringify!($ty),
                            other.kind()
                        ))),
                    }
                }
            }
        )*
    };
}

impl_float!(f32, f64);

impl<T: OutputType> OutputType for Option<T> {
    fn output_type() -> TypeRef {
        T::output_type().nullable()
    }

    fn is_optional() -> bool {
        true
    }

    fn register_output(registry: &mut crate::registry::TypeRegistry) {
        T::register_output(registry);
    }
}

impl<T: IntoValue> IntoValue for Option<T> {
    fn into_value(self) -> Result<Value, GraphQLError> {
        match self {
            Some(value) => value.into_value(),
            None => Ok(Value::Null),
        }
    }
}

impl<T: InputType> InputType for Option<T> {
    fn input_type() -> TypeRef {
        T::input_type().nullable()
    }

    fn register_input(registry: &mut crate::registry::TypeRegistry) {
        T::register_input(registry);
    }
}

impl<T: FromValue> FromValue for Option<T> {
    fn from_value(value: &Value) -> Result<Self, GraphQLError> {
        if value.is_null() {
            Ok(None)
        } else {
            T::from_value(value).map(Some)
        }
    }
}

impl<T: OutputType> OutputType for Vec<T> {
    fn output_type() -> TypeRef {
        T::output_type().list().non_null()
    }

    fn register_output(registry: &mut crate::registry::TypeRegistry) {
        T::register_output(registry);
    }
}

impl<T: IntoValue> IntoValue for Vec<T> {
    fn into_value(self) -> Result<Value, GraphQLError> {
        let mut values = Vec::with_capacity(self.len());
        for item in self {
            values.push(item.into_value()?);
        }
        Ok(Value::List(values))
    }
}

impl<T: InputType> InputType for Vec<T> {
    fn input_type() -> TypeRef {
        T::input_type().list().non_null()
    }

    fn register_input(registry: &mut crate::registry::TypeRegistry) {
        T::register_input(registry);
    }
}

impl<T: FromValue> FromValue for Vec<T> {
    fn from_value(value: &Value) -> Result<Self, GraphQLError> {
        let Some(values) = value.as_list() else {
            return Err(GraphQLError::new(format!(
                "Expected a list but found {}.",
                value.kind()
            )));
        };
        values.iter().map(T::from_value).collect()
    }
}

impl<T: OutputType> OutputType for Box<T> {
    fn output_type() -> TypeRef {
        T::output_type()
    }

    fn is_optional() -> bool {
        T::is_optional()
    }

    fn register_output(registry: &mut crate::registry::TypeRegistry) {
        T::register_output(registry);
    }
}

impl<T: IntoValue> IntoValue for Box<T> {
    fn into_value(self) -> Result<Value, GraphQLError> {
        (*self).into_value()
    }
}

impl<T: InputType> InputType for Box<T> {
    fn input_type() -> TypeRef {
        T::input_type()
    }

    fn register_input(registry: &mut crate::registry::TypeRegistry) {
        T::register_input(registry);
    }
}

impl<T: FromValue> FromValue for Box<T> {
    fn from_value(value: &Value) -> Result<Self, GraphQLError> {
        T::from_value(value).map(Box::new)
    }
}

impl<T: OutputType> OutputType for Arc<T> {
    fn output_type() -> TypeRef {
        T::output_type()
    }

    fn is_optional() -> bool {
        T::is_optional()
    }

    fn register_output(registry: &mut crate::registry::TypeRegistry) {
        T::register_output(registry);
    }
}

impl<T: IntoValue + Clone> IntoValue for Arc<T> {
    fn into_value(self) -> Result<Value, GraphQLError> {
        (*self).clone().into_value()
    }
}

impl<T: InputType> InputType for Arc<T> {
    fn input_type() -> TypeRef {
        T::input_type()
    }

    fn register_input(registry: &mut crate::registry::TypeRegistry) {
        T::register_input(registry);
    }
}

impl<T: FromValue> FromValue for Arc<T> {
    fn from_value(value: &Value) -> Result<Self, GraphQLError> {
        T::from_value(value).map(Arc::new)
    }
}

/// Convert a GraphQL literal into the runtime value used by custom scalars.
pub(crate) fn literal_to_value(literal: &gql::ast::Value<'_>) -> Result<Value, GraphQLError> {
    match literal {
        gql::ast::Value::Null => Ok(Value::Null),
        gql::ast::Value::Boolean(boolean) => Ok(Value::Boolean(*boolean)),
        gql::ast::Value::Int(number) => Ok(Value::integer(*number)),
        gql::ast::Value::Float(number) => Ok(Value::Float(*number)),
        gql::ast::Value::String(string) => Ok(Value::String(Arc::from(string.as_ref()))),
        gql::ast::Value::Enum(name) => Ok(Value::Enum(Arc::from(*name))),
        gql::ast::Value::List(items) => {
            let mut values = Vec::with_capacity(items.len());
            for item in items {
                values.push(literal_to_value(item)?);
            }
            Ok(Value::List(values))
        }
        gql::ast::Value::Object(fields) => {
            let mut object = gql::Object::with_capacity(fields.len());
            for field in fields {
                object.insert(field.name, literal_to_value(&field.value)?);
            }
            Ok(Value::Object(Arc::new(object)))
        }
        gql::ast::Value::Variable(name) => Err(GraphQLError::new(format!(
            "Unexpected variable `${name}` in a scalar literal."
        ))),
    }
}
