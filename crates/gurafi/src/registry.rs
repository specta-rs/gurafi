//! Named-type registration.
//!
//! Types are identified by name in `gql`, so the schema does not need to know a definition's
//! position to reference it. The registry collects each named definition exactly once while
//! fields and arguments are declared, then hands the set to `gql` at build time.

use std::{
    any::TypeId,
    collections::{HashMap, HashSet},
};

use gql::{GraphQLError, InputObjectType, Type};
use indexmap::IndexMap;

use crate::{
    EnumModel, InputObject, ScalarValue,
    traits::{AbstractType, InputType},
};

/// Collects the named GraphQL types a schema references.
///
/// A schema owns one registry. Every field, argument and expose declaration registers the types
/// it mentions; duplicate registrations of the same name are ignored, so a type can be used by
/// any number of fields without extra bookkeeping in user code.
#[derive(Clone, Debug, Default)]
pub struct TypeRegistry {
    types: IndexMap<String, Type>,
    /// Object names referenced by fields but registered separately by `schema.object`.
    referenced_objects: HashSet<&'static str>,
    /// Union names referenced by an output type but registered by `schema.union`.
    referenced_unions: HashSet<&'static str>,
    /// Input types currently being registered, used to stop recursion on self-referential
    /// input objects.
    in_progress: HashSet<String>,
    /// The Rust type that owns each GraphQL name, so two different Rust types cannot silently
    /// claim the same GraphQL name.
    type_ids: HashMap<String, TypeId>,
    /// Names for which a collision has already been reported.
    conflicts: HashSet<String>,
    /// Args-only Rust types that were used in a nested input position, reported once each.
    unnamed_inputs: HashSet<TypeId>,
    errors: Vec<GraphQLError>,
}

impl TypeRegistry {
    /// An empty registry.
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }

    /// Whether a named type has already been registered.
    #[must_use]
    pub fn contains(&self, name: &str) -> bool {
        self.types.contains_key(name)
    }

    /// Record that an object type is used by an output position.
    #[doc(hidden)]
    pub fn reference_object(&mut self, name: &'static str) {
        self.referenced_objects.insert(name);
    }

    /// Record that a union type is used by an output position.
    #[doc(hidden)]
    pub fn reference_union(&mut self, name: &'static str) {
        self.referenced_unions.insert(name);
    }

    /// Register an enum type.
    #[doc(hidden)]
    pub fn register_enum<T: EnumModel>(&mut self) {
        if self.already_registered(T::NAME, TypeId::of::<T>()) {
            return;
        }
        let mut enumeration = gql::EnumType::new(T::NAME);
        if let Some(description) = T::description() {
            enumeration = enumeration.description(description);
        }
        for value in T::values() {
            enumeration.values.insert(value.name.clone(), value);
        }
        self.type_ids.insert(T::NAME.to_owned(), TypeId::of::<T>());
        self.types.insert(
            T::NAME.to_owned(),
            Type::Enum(std::sync::Arc::new(enumeration)),
        );
    }

    /// Register a custom scalar type.
    #[doc(hidden)]
    pub fn register_scalar<T: ScalarValue>(&mut self) {
        if self.already_registered(T::NAME, TypeId::of::<T>()) {
            return;
        }
        let mut scalar = gql::ScalarType::new(T::NAME)
            .serialize(|value| Ok(value.clone()))
            .parse_value(|value| match T::from_value(value) {
                Ok(_) => Ok(value.clone()),
                Err(error) => Err(error.message().to_owned()),
            })
            .parse_literal(|literal| match T::from_literal(literal) {
                Ok(value) => Ok(value),
                Err(error) => Err(error.message().to_owned()),
            });
        if let Some(description) = T::description() {
            scalar = scalar.description(description);
        }
        if let Some(url) = T::specified_by() {
            scalar = scalar.specified_by_url(url);
        }
        self.type_ids.insert(T::NAME.to_owned(), TypeId::of::<T>());
        self.types.insert(
            T::NAME.to_owned(),
            Type::Scalar(std::sync::Arc::new(scalar)),
        );
    }

    /// Register a derived input object type and its fields.
    #[doc(hidden)]
    pub fn register_input_object<T: InputObject>(&mut self) {
        let Some(name) = T::NAME else {
            return;
        };
        if self.already_registered(name, TypeId::of::<T>()) || self.in_progress.contains(name) {
            return;
        }
        self.in_progress.insert(name.to_owned());
        let fields = match T::fields() {
            Ok(fields) => fields,
            Err(error) => {
                self.errors.push(GraphQLError::new(format!(
                    "Failed to register input object `{name}`: {}",
                    error.message()
                )));
                self.in_progress.remove(name);
                return;
            }
        };
        let mut input = InputObjectType::new(name);
        if let Some(description) = T::description() {
            input = input.description(description);
        }
        if T::one_of() {
            input = input.one_of();
        }
        for field in fields {
            input.insert_field(field);
        }
        self.type_ids.insert(name.to_owned(), TypeId::of::<T>());
        self.types.insert(
            name.to_owned(),
            Type::InputObject(std::sync::Arc::new(input)),
        );
        self.in_progress.remove(name);
    }

    /// Return `true` when this exact Rust type already registered the name, and record an error
    /// when a different Rust type claims the same GraphQL name.
    fn already_registered(&mut self, name: &str, type_id: TypeId) -> bool {
        if !self.types.contains_key(name) {
            return false;
        }
        if self.type_ids.get(name) == Some(&type_id) {
            return true;
        }
        if self.conflicts.insert(name.to_owned()) {
            self.errors.push(GraphQLError::new(format!(
                "GraphQL type `{name}` is registered by more than one Rust type. Give one of them a distinct `#[graphql(name = \"...\")]`."
            )));
        }
        true
    }

    /// Report an args-only [`crate::InputObject`] used where a named input type is required.
    #[doc(hidden)]
    pub fn unnamed_input<T: InputObject>(&mut self) {
        if !self.unnamed_inputs.insert(TypeId::of::<T>()) {
            return;
        }
        self.errors.push(GraphQLError::new(format!(
            "`{}` is used as a nested input type, but it has no GraphQL name. Add \
             `#[graphql(name = \"{}\")]` to make it a reusable input object, or move its fields \
             into the outer input type.",
            std::any::type_name::<T>(),
            std::any::type_name::<T>()
                .rsplit("::")
                .next()
                .unwrap_or("Input"),
        )));
    }

    /// Register a field or argument's input type, and the types nested inside it.
    #[doc(hidden)]
    pub fn register_input<T: InputType>(&mut self) {
        T::register_input(self);
    }

    /// Record a schema-level problem found while registering types.
    pub(crate) fn push_error(&mut self, error: GraphQLError) {
        self.errors.push(error);
    }

    /// Every error recorded while registering types.
    pub(crate) fn take_errors(&mut self) -> Vec<GraphQLError> {
        std::mem::take(&mut self.errors)
    }

    /// Whether an args-only input was used in a nested input position.
    pub(crate) fn has_unnamed_input(&self) -> bool {
        !self.unnamed_inputs.is_empty()
    }

    /// The names of object types referenced by output positions.
    pub(crate) fn referenced_objects(&self) -> &HashSet<&'static str> {
        &self.referenced_objects
    }

    /// The names of union types referenced by output positions.
    pub(crate) fn referenced_unions(&self) -> &HashSet<&'static str> {
        &self.referenced_unions
    }

    /// Consume the registry into its named definitions, in registration order.
    pub(crate) fn into_types(self) -> IndexMap<String, Type> {
        self.types
    }
}

impl AbstractType for crate::schema::InterfaceRef {
    fn abstract_name(&self) -> &'static str {
        self.name()
    }
}

impl AbstractType for crate::schema::UnionRef {
    fn abstract_name(&self) -> &'static str {
        self.name()
    }
}
