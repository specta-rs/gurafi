//! Selection-set execution helpers.

use crate::{error::GraphQLError, schema::Schema, types::Type, types::TypeRef};

/// Whether a fragment's type condition can apply to a runtime object type.
///
/// An object condition matches only that object. An interface or union condition matches every
/// object that implements or belongs to it.
pub(crate) fn type_condition_matches(
    schema: &Schema,
    condition: &str,
    runtime_type: &str,
) -> Result<bool, GraphQLError> {
    if condition == runtime_type {
        return Ok(true);
    }
    match schema.get_type(condition) {
        Some(Type::Object(_)) => Ok(false),
        Some(Type::Interface(_)) | Some(Type::Union(_)) => {
            Ok(schema.is_possible_type(condition, runtime_type))
        }
        Some(_) => Err(GraphQLError::new(format!(
            "Fragment type condition \"{condition}\" is not a composite type."
        ))),
        None => Err(GraphQLError::new(format!(
            "Fragment type condition \"{condition}\" is not defined by the schema."
        ))),
    }
}

/// Convert an AST type reference into a schema type reference.
pub(crate) fn type_ref(ty: &crate::ast::Type<'_>) -> TypeRef {
    match ty {
        crate::ast::Type::Named(name) => TypeRef::named(*name),
        crate::ast::Type::List(inner) => type_ref(inner).list(),
        crate::ast::Type::NonNull(inner) => type_ref(inner).non_null(),
    }
}
