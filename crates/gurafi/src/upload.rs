//! File upload inputs.
//!
//! [`Upload`] is the shared value produced by `suku`'s GraphQL multipart transport. It is an
//! input-only scalar: an argument or input field of this type receives the uploaded bytes and
//! metadata, while output fields are rejected at build time because an upload has no
//! representation in a GraphQL response.
//!
//! ```
//! use gurafi::{Input, Upload};
//!
//! #[derive(Input)]
//! struct UploadArgs {
//!     file: Upload,
//! }
//! ```

use crate::{
    FromValue, InputType, IntoValue, ScalarValue, TypeRegistry,
    gql::{GraphQLError, TypeRef, Value},
};

pub use gql::Upload;

impl ScalarValue for Upload {
    const NAME: &'static str = "Upload";

    fn description() -> Option<&'static str> {
        Some("A file uploaded with a GraphQL multipart request.")
    }

    fn to_value(&self) -> Result<Value, GraphQLError> {
        Err(GraphQLError::new(
            "Upload is an input-only scalar and cannot be returned from a field.",
        ))
    }

    fn from_value(value: &Value) -> Result<Self, GraphQLError> {
        value.as_external::<Upload>().cloned().ok_or_else(|| {
            GraphQLError::new("Upload must be supplied by a multipart/form-data request.")
        })
    }

    fn from_literal(_literal: &gql::ast::Value<'_>) -> Result<Value, GraphQLError> {
        Err(GraphQLError::new(
            "Upload cannot be provided as a GraphQL literal; use a multipart request.",
        ))
    }
}

impl InputType for Upload {
    fn input_type() -> TypeRef {
        TypeRef::named(<Self as ScalarValue>::NAME).non_null()
    }

    fn register_input(registry: &mut TypeRegistry) {
        registry.register_scalar::<Self>();
    }
}

impl IntoValue for Upload {
    fn into_value(self) -> Result<Value, GraphQLError> {
        <Self as ScalarValue>::to_value(&self)
    }
}

impl FromValue for Upload {
    fn from_value(value: &Value) -> Result<Self, GraphQLError> {
        <Self as ScalarValue>::from_value(value)
    }
}
