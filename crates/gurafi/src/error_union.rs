//! Typed error payloads that a field exposes as a GraphQL union.
//!
//! A fallible field usually reports failure as a GraphQL error. Sometimes the failure *is* part
//! of the API: a `createUser` mutation may fail because the address is taken, and the client
//! needs a typed `AddressTakenError` payload it can switch on, not a string. This module is the
//! Rust half of that pattern — the same shape as Pothos's Errors plugin.
//!
//! [`ErrorUnionModel`] is implemented by `#[derive(ErrorUnion)]` on an enum whose variants each
//! hold one object model. A field is declared with `field_result_union`, which generates:
//!
//! ```graphql
//! union CreateUserResult = CreateUserSuccess | NotFoundError | ConflictError
//!
//! type CreateUserSuccess {
//!   data: User!
//! }
//! ```
//!
//! The success member's `data` field keeps the resolver's `T` type (including its nullability,
//! list wrappers and abstract types); each error variant's own object type is a member. See the
//! builder methods and [`crate::ErrorUnionModel`].

use std::{marker::PhantomData, sync::Arc};

use gql::{Field, GraphQLError, Name, ObjectType, TypeRef, UnionType, Value};

use crate::{
    registry::TypeRegistry,
    traits::{IntoValue, OutputType},
};

/// A Rust enum whose variants are the typed failures of a result-union field.
///
/// Implemented by `#[derive(ErrorUnion)]`. Each variant must hold exactly one object model and
/// the enum must not be generic; the derive reports anything else at compile time. The error
/// payload objects are still registered by the schema author with `schema.object::<M>(...)`, so
/// their fields, authorization and resolvers stay visible where the rest of the schema lives.
///
/// ```
/// use gurafi::{ErrorUnion, Object};
///
/// #[derive(Object)]
/// struct NotFoundError {
///     message: String,
/// }
///
/// #[derive(ErrorUnion)]
/// enum CreateUserError {
///     NotFound(NotFoundError),
/// }
/// ```
pub trait ErrorUnionModel: Sized + Send + Sync + 'static {
    /// The GraphQL object type names of the error variants, in declaration order.
    fn members() -> &'static [&'static str];

    /// Convert a variant into the executor's value for its payload object.
    ///
    /// # Errors
    ///
    /// Returns an error when a payload's [`IntoValue`] conversion fails, exactly like an
    /// ordinary field value.
    fn into_value(self) -> Result<Value, GraphQLError>;
}

/// The model behind a generated `<Parent><Field>Success` object.
///
/// The success value is converted to its GraphQL value once, in the field resolver. Keeping the
/// converted value here means the generated `data` field does not need `T: Clone`; it returns a
/// cheap clone of the already-built [`Value`].
pub(crate) struct ResultSuccess {
    data: Value,
}

impl ResultSuccess {
    pub(crate) fn new(data: Value) -> Self {
        Self { data }
    }
}

/// Marker used to make generated names idempotent without exposing a generic model.
struct SuccessMarker<T>(PhantomData<fn() -> T>);

/// Marker for the generated union, including the error enum so two different result unions that
/// happen to share a name are reported instead of silently merged.
struct UnionMarker<T, E>(PhantomData<fn() -> (T, E)>);

/// Build the generated success object and result union for `parent.field`, and return the
/// non-null union type reference.
///
/// `T::register_output` runs here so the success payload's `data` type is registered with the
/// same field declaration. Error payload objects are only *required* here; the schema author
/// still registers them, which keeps runtime-named payloads and their own fields in user code.
pub(crate) fn result_union_type_ref<T, E>(
    registry: &mut TypeRegistry,
    parent: &str,
    field: &str,
) -> TypeRef
where
    T: OutputType + IntoValue + Send + 'static,
    E: ErrorUnionModel,
{
    T::register_output(registry);

    let suffix = pascal_case(field);
    // Build each runtime name once and share it: the object, the union, its members and the
    // registry key all refer to the same `Arc<str>`.
    let success_name: Name = format!("{parent}{suffix}Success").into();
    let union_name: Name = format!("{parent}{suffix}Result").into();

    // A result union's success payload is always present when the resolver returns `Ok`, so
    // `data` is non-null unless `T` itself is `Option<_>`, in which case the value may be null.
    let data_type = if T::is_optional() {
        T::output_type()
    } else {
        T::output_type().non_null()
    };

    let success = ObjectType::new(Arc::clone(&success_name))
        .description(format!("The successful payload of `{parent}.{field}`."))
        .is_type_of(|value| value.as_external::<ResultSuccess>().is_some())
        .field(Field::new("data", data_type).resolve(|context| {
            Box::pin(async move {
                context
                    .parent
                    .as_external::<ResultSuccess>()
                    .map(|success| success.data.clone())
                    .ok_or_else(|| {
                        GraphQLError::new(format!(
                            "Internal error: `{}.{}` expected a result-union success payload.",
                            context.info.parent_type, context.info.field_name,
                        ))
                    })
            })
        }));
    registry.register_generated_object::<SuccessMarker<T>>(Arc::clone(&success_name), success);

    let mut members: Vec<Name> = vec![success_name];
    for member in E::members() {
        registry.require_object(member, parent, field, "Error union member");
        if !members.iter().any(|existing| existing.as_ref() == *member) {
            members.push(Name::from(*member));
        }
    }

    let mut union = UnionType::new(Arc::clone(&union_name)).description(format!(
        "The result of `{parent}.{field}`: either its success payload or a typed error."
    ));
    union.types = members;
    registry.register_generated_union::<UnionMarker<T, E>>(Arc::clone(&union_name), union);

    TypeRef::named(union_name).non_null()
}

/// Turn a GraphQL field name into the PascalCase fragment used in generated type names.
///
/// `createUser` becomes `CreateUser`, `create_user` becomes `CreateUser`, and a leading
/// underscore (`_id`) becomes `Id`. This is intentionally not a general-purpose case converter:
/// only GraphQL names reach it.
pub(crate) fn pascal_case(field: &str) -> String {
    let mut result = String::with_capacity(field.len());
    let mut uppercase = true;
    for character in field.chars() {
        if character == '_' || character == '-' || character == ' ' {
            uppercase = true;
            continue;
        }
        if uppercase {
            result.extend(character.to_uppercase());
            uppercase = false;
        } else {
            result.push(character);
        }
    }
    if result.is_empty() {
        result.push_str("Field");
    }
    if result
        .chars()
        .next()
        .is_some_and(|character| character.is_ascii_digit())
    {
        result.insert_str(0, "Field");
    }
    result
}

#[cfg(test)]
mod tests {
    use super::pascal_case;

    #[test]
    fn field_names_become_type_name_fragments() {
        assert_eq!(pascal_case("createUser"), "CreateUser");
        assert_eq!(pascal_case("create_user"), "CreateUser");
        assert_eq!(pascal_case("URL"), "URL");
        assert_eq!(pascal_case("_id"), "Id");
        assert_eq!(pascal_case("1st"), "Field1st");
        assert_eq!(pascal_case("_"), "Field");
    }
}
