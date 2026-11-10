//! Schema composition: merging, filtering and field transformations.
//!
//! These operations rebuild a [`Schema`](super::Schema) through
//! [`SchemaBuilder`](super::SchemaBuilder), so every result is validated exactly like a schema
//! built by hand. Resolver functions travel with the definitions they belong to: merging two
//! schemas keeps resolvers from both, and transforming or filtering a schema keeps the resolvers
//! of every field that survives.

use std::sync::Arc;

use indexmap::IndexMap;
use rustc_hash::FxHashSet;

use crate::types::{
    AppliedDirective, DirectiveDefinition, EnumType, Field, InputObjectType, InputValue,
    InterfaceType, Name, ObjectType, ScalarType, Type, UnionType,
};

use super::{Schema, SchemaBuilder, SchemaError, TypeMap};

/// What to do with a field while transforming a schema.
///
/// Returned by the closure passed to [`Schema::transform`].
#[derive(Clone, Debug)]
#[allow(clippy::large_enum_variant)]
pub enum FieldTransform {
    /// Keep the field unchanged.
    Keep,
    /// Remove the field from its type.
    Remove,
    /// Replace the field with a new definition.
    ///
    /// The replacement must keep the original field's name; renaming a field during a transform
    /// would silently change the response shape instead of extending it, so it is rejected.
    Replace(Field),
}

/// A reusable type filter.
///
/// Build one with [`SchemaTransform::new`], then [`SchemaTransform::keep`] the types you want and
/// [`SchemaTransform::remove`] the ones you do not, and apply it to a schema. Removing a type wins
/// over keeping it.
///
/// Removing a type that is still referenced by a field, a union member or a root operation makes
/// the rebuild fail with the specification's usual validation error; remove the references first
/// (with [`Schema::transform`]) or keep the type.
///
/// ```
/// use gql::Schema;
///
/// let schema = Schema::from_sdl(
///     "type Query { a: String } type Unused { value: String }",
/// )
/// .unwrap();
/// use gql::SchemaTransform;
///
/// let trimmed = SchemaTransform::new()
///     .remove("Unused")
///     .apply(&schema)
///     .unwrap();
/// assert!(trimmed.get_type("Unused").is_none());
/// ```
#[derive(Clone, Debug, Default)]
pub struct SchemaTransform {
    keep: Option<FxHashSet<Name>>,
    remove: FxHashSet<Name>,
}

impl SchemaTransform {
    /// No filter: every type is kept.
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }

    /// Keep a type.
    ///
    /// Calling this method at least once switches the transform into an allow-list: types not
    /// named are removed. A transform with no `keep` calls keeps everything except the types
    /// passed to [`SchemaTransform::remove`].
    #[must_use]
    pub fn keep(mut self, name: impl Into<Name>) -> Self {
        self.keep
            .get_or_insert_with(FxHashSet::default)
            .insert(name.into());
        self
    }

    /// Remove a type.
    #[must_use]
    pub fn remove(mut self, name: impl Into<Name>) -> Self {
        self.remove.insert(name.into());
        self
    }

    /// Apply the filter to a schema.
    ///
    /// # Errors
    ///
    /// Returns the schema validation errors produced by removing a type something still
    /// references.
    pub fn apply(&self, schema: &Schema) -> Result<Schema, SchemaError> {
        schema.retain_types(|ty| {
            let name = ty.name().as_ref();
            !self.remove.contains(name) && self.keep.as_ref().is_none_or(|keep| keep.contains(name))
        })
    }
}

impl Schema {
    /// Merge another schema into this one.
    ///
    /// Types, directives, root operation types, schema-level directive applications and the
    /// schema description are combined. A name defined by both schemas is accepted only when the
    /// definitions are structurally identical; resolver functions are not part of that
    /// comparison. A resolver, subscription, complexity estimator, `is_type_of`, `resolve_type`
    /// or scalar coercion function present in only one schema is kept. If both schemas set the
    /// same runtime slot to different implementations the merge fails rather than choosing one by
    /// argument order, because those slots carry authorization, complexity limits and subscription
    /// behaviour. Any other collision is reported as a [`SchemaError`] naming the definition.
    ///
    /// Directive applications and directive locations are compared without regard to order;
    /// repeatable directives are compared as a multiset.
    ///
    /// # Errors
    ///
    /// Returns merge conflicts, including conflicting runtime implementations, and the ordinary
    /// validation errors a rebuild can produce.
    #[cfg_attr(
        feature = "tracing",
        tracing::instrument(
            skip_all,
            level = "info",
            name = "gql.schema.merge",
            fields(
                types = self.types().len(),
                incoming_types = other.types().len(),
                changed = tracing::field::Empty,
            )
        )
    )]
    pub fn merge(&self, other: &Schema) -> Result<Schema, SchemaError> {
        let mut builder = builder_from(self);
        builder.merge(other);
        let result = builder.build();
        #[cfg(feature = "tracing")]
        if let Ok(schema) = &result {
            let span = tracing::Span::current();
            if !span.is_disabled() {
                span.record("changed", super::diff(self, schema).changes().count());
            }
        }
        result
    }

    /// Remove types by name.
    ///
    /// # Errors
    ///
    /// Returns validation errors when a removed type is still referenced.
    #[cfg_attr(
        feature = "tracing",
        tracing::instrument(
            skip_all,
            level = "info",
            name = "gql.schema.without_types",
            fields(types = self.types().len(), removed = names.len())
        )
    )]
    pub fn without_types(&self, names: &[&str]) -> Result<Schema, SchemaError> {
        let removed: FxHashSet<&str> = names.iter().copied().collect();
        self.retain_types(|ty| !removed.contains(ty.name().as_ref()))
    }

    /// Keep only the types accepted by `retain`.
    ///
    /// Introspection types are always present in the result and are not passed to the predicate.
    ///
    /// # Errors
    ///
    /// Returns validation errors when a removed type is still referenced.
    #[cfg_attr(
        feature = "tracing",
        tracing::instrument(
            skip_all,
            level = "info",
            name = "gql.schema.retain_types",
            fields(types = self.types().len())
        )
    )]
    pub fn retain_types(
        &self,
        mut retain: impl FnMut(&Type) -> bool,
    ) -> Result<Schema, SchemaError> {
        let types = self
            .inner
            .types
            .iter()
            .filter(|(name, _)| !is_introspection(name))
            .filter(|(_, ty)| retain(ty))
            .map(|(name, ty)| (name.clone(), ty.clone()))
            .collect();
        rebuild_with_types(self, types)
    }

    /// Rewrite or remove fields on object and interface types.
    ///
    /// The closure receives the type being transformed and each of its fields. Input object
    /// fields are not visited: they cannot have resolvers and changing an input shape is a
    /// schema-versioning decision better expressed with SDL.
    ///
    /// # Errors
    ///
    /// Returns validation errors when a transformation leaves the schema invalid, and an error
    /// when [`FieldTransform::Replace`] changes a field's name.
    ///
    /// ```
    /// use gql::{FieldTransform, Schema};
    ///
    /// let schema = Schema::from_sdl("type Query { keep: String drop: String }").unwrap();
    /// let trimmed = schema
    ///     .transform(|_, field| {
    ///         if field.name.as_ref() == "drop" {
    ///             FieldTransform::Remove
    ///         } else {
    ///             FieldTransform::Keep
    ///         }
    ///     })
    ///     .unwrap();
    /// assert!(trimmed.query_type().get_field("drop").is_none());
    /// ```
    #[cfg_attr(
        feature = "tracing",
        tracing::instrument(
            skip_all,
            level = "info",
            name = "gql.schema.transform",
            fields(types = self.types().len())
        )
    )]
    pub fn transform(
        &self,
        mut transform: impl FnMut(&Type, &Field) -> FieldTransform,
    ) -> Result<Schema, SchemaError> {
        let mut errors = SchemaError::default();
        let mut types = TypeMap::default();
        for (name, ty) in &self.inner.types {
            if is_introspection(name) {
                continue;
            }
            let transformed = match ty {
                Type::Object(object) => {
                    let mut object = (**object).clone();
                    object.fields =
                        transform_fields(ty, &object.fields, &mut transform, &mut errors);
                    Type::Object(Arc::new(object))
                }
                Type::Interface(interface) => {
                    let mut interface = (**interface).clone();
                    interface.fields =
                        transform_fields(ty, &interface.fields, &mut transform, &mut errors);
                    Type::Interface(Arc::new(interface))
                }
                _ => ty.clone(),
            };
            types.insert(name.clone(), transformed);
        }
        if !errors.is_empty() {
            return Err(errors);
        }
        rebuild_with_types(self, types)
    }

    /// Extend the schema with a GraphQL SDL document.
    ///
    /// The SDL is interpreted as an addition to this schema: it may define new types, apply
    /// `extend type`/`extend interface`/`extend enum` additions, add directives, and extend the
    /// schema's roots. Resolvers of every field that still exists keep working, which makes this
    /// the build-time counterpart of loading a base document plus an extension document.
    ///
    /// The extension is applied by printing and re-parsing the schema, so one call costs
    /// `O(schema size)`. Applying several documents one at a time is therefore `O(k · n)`;
    /// [`Schema::extend_sdl_many`] applies them together in `O(n + total SDL)`.
    ///
    /// # Errors
    ///
    /// Returns parse errors, extension errors such as extending an unknown type, and schema
    /// validation errors.
    ///
    /// ```
    /// use gql::{Field, ObjectType, Schema, TypeRef, Value};
    ///
    /// let mut builder = Schema::builder();
    /// builder.add_type(
    ///     ObjectType::new("Query")
    ///         .field(Field::new("hello", TypeRef::string()).resolve(|_| {
    ///             Box::pin(async { Ok(Value::from("world")) })
    ///         }))
    ///         .into(),
    /// );
    /// let schema = builder.query("Query").build().unwrap();
    ///
    /// let extended = schema.extend_sdl("extend type Query { extra: String }").unwrap();
    /// assert!(extended.query_type().get_field("extra").is_some());
    /// assert!(extended.query_type().get_field("hello").unwrap().resolve.is_some());
    /// ```
    #[cfg_attr(
        feature = "tracing",
        tracing::instrument(
            skip_all,
            level = "info",
            name = "gql.schema.extend_sdl",
            fields(sdl_bytes = sdl.len(), types = self.types().len(), changed = tracing::field::Empty)
        )
    )]
    pub fn extend_sdl(&self, sdl: &str) -> Result<Schema, SchemaError> {
        self.extend_sdl_many([sdl])
    }

    /// Extend the schema with several GraphQL SDL documents in one build.
    ///
    /// This is the batch form of [`Schema::extend_sdl`]: the documents are concatenated and
    /// applied in order, so the schema is printed, parsed and validated once rather than once per
    /// document. The result is the same as calling [`Schema::extend_sdl`] for each document in
    /// turn, except that the documents are validated together (a later document may fix a shape
    /// an earlier one left temporarily invalid).
    ///
    /// Resolvers of every field that still exists keep working.
    ///
    /// # Errors
    ///
    /// Returns parse errors, extension errors such as extending an unknown type, and schema
    /// validation errors.
    ///
    /// ```
    /// use gql::{Field, ObjectType, Schema, TypeRef, Value};
    ///
    /// let mut builder = Schema::builder();
    /// builder.add_type(
    ///     ObjectType::new("Query")
    ///         .field(Field::new("hello", TypeRef::string()).resolve(|_| {
    ///             Box::pin(async { Ok(Value::from("world")) })
    ///         }))
    ///         .into(),
    /// );
    /// let schema = builder.query("Query").build().unwrap();
    ///
    /// let extended = schema
    ///     .extend_sdl_many([
    ///         "type Extra { value: String }",
    ///         "extend type Query { extra: Extra }",
    ///     ])
    ///     .unwrap();
    /// assert!(extended.get_type("Extra").is_some());
    /// assert!(extended.query_type().get_field("extra").is_some());
    /// assert!(extended.query_type().get_field("hello").unwrap().resolve.is_some());
    /// ```
    pub fn extend_sdl_many<'a>(
        &self,
        documents: impl IntoIterator<Item = &'a str>,
    ) -> Result<Schema, SchemaError> {
        let mut source = self.sdl();
        if !source.ends_with('\n') {
            source.push('\n');
        }
        for sdl in documents {
            source.push_str(sdl);
            if !source.ends_with('\n') {
                source.push('\n');
            }
        }
        let mut extended = Schema::from_sdl(&source)?;
        overlay_resolvers(self, &mut extended);
        #[cfg(feature = "tracing")]
        {
            let span = tracing::Span::current();
            if !span.is_disabled() {
                span.record("changed", super::diff(self, &extended).changes().count());
            }
        }
        Ok(extended)
    }
}

impl SchemaBuilder {
    /// Merge an already-built schema into this builder.
    ///
    /// The same compatibility rules as [`Schema::merge`] apply. Conflicts are recorded in the
    /// builder and reported by [`SchemaBuilder::build`], so a partially built schema can be
    /// composed before validation.
    #[cfg_attr(
        feature = "tracing",
        tracing::instrument(
            skip_all,
            level = "info",
            name = "gql.schema.builder.merge",
            fields(types = self.types.len(), incoming_types = other.types().len())
        )
    )]
    pub fn merge(&mut self, other: &Schema) -> &mut Self {
        merge_description(
            &mut self.description,
            other.inner.description.as_ref(),
            &mut self.errors,
        );

        for directive in &other.inner.applied_directives {
            if !self.applied_directives.contains(directive) {
                self.applied_directives.push(directive.clone());
            }
        }

        for (name, definition) in &other.inner.directives {
            match self.directives.get(name) {
                Some(existing) => {
                    if !directive_equivalent(existing, definition) {
                        self.errors.push(format!(
                            "Cannot merge schemas: directive \"@{name}\" is defined differently in both schemas."
                        ));
                    }
                }
                None => {
                    self.directives.insert(name.clone(), definition.clone());
                }
            }
        }

        for (name, ty) in &other.inner.types {
            if is_introspection(name) {
                continue;
            }
            match self.types.get(name) {
                Some(existing) => match merge_types(name, existing, ty) {
                    Ok(merged) => {
                        self.types.insert(name.clone(), merged);
                    }
                    Err(message) => self.errors.push(message),
                },
                None => {
                    self.types.insert(name.clone(), ty.clone());
                }
            }
        }

        merge_root(
            &mut self.query,
            Some(&other.inner.query),
            "query",
            &mut self.errors,
        );
        merge_root(
            &mut self.mutation,
            other.inner.mutation.as_ref(),
            "mutation",
            &mut self.errors,
        );
        merge_root(
            &mut self.subscription,
            other.inner.subscription.as_ref(),
            "subscription",
            &mut self.errors,
        );
        self
    }
}

/// Turn a schema into a builder without reporting its definitions as duplicates.
fn builder_from(schema: &Schema) -> SchemaBuilder {
    let mut builder = SchemaBuilder::new();
    builder.description = schema.inner.description.clone();
    builder.applied_directives = schema.inner.applied_directives.clone();
    builder.query = Some(schema.inner.query.clone());
    builder.mutation = schema.inner.mutation.clone();
    builder.subscription = schema.inner.subscription.clone();

    for (name, definition) in &schema.inner.directives {
        if !builder.directives.contains_key(name) {
            builder.directives.insert(name.clone(), definition.clone());
        }
    }
    for (name, ty) in &schema.inner.types {
        if !is_introspection(name) {
            builder.types.insert(name.clone(), ty.clone());
        }
    }
    builder
}

/// Rebuild a schema with a replacement type map.
fn rebuild_with_types(schema: &Schema, types: TypeMap<Type>) -> Result<Schema, SchemaError> {
    let mut builder = builder_from(schema);
    builder.types = types;
    builder.build()
}

fn transform_fields(
    ty: &Type,
    fields: &IndexMap<Name, Field>,
    transform: &mut impl FnMut(&Type, &Field) -> FieldTransform,
    errors: &mut SchemaError,
) -> IndexMap<Name, Field> {
    let mut transformed = IndexMap::with_capacity(fields.len());
    for (name, field) in fields {
        match transform(ty, field) {
            FieldTransform::Keep => {
                transformed.insert(name.clone(), field.clone());
            }
            FieldTransform::Remove => {}
            FieldTransform::Replace(replacement) => {
                if &replacement.name == name {
                    transformed.insert(name.clone(), replacement);
                } else {
                    errors.push(format!(
                        "Cannot replace field \"{}.{}\" with a field named \"{}\"; a transform must keep the field name.",
                        ty.name(),
                        name,
                        replacement.name
                    ));
                }
            }
        }
    }
    transformed
}

fn merge_description(
    current: &mut Option<Arc<str>>,
    incoming: Option<&Arc<str>>,
    errors: &mut SchemaError,
) {
    match (current.as_ref(), incoming) {
        (Some(left), Some(right)) if left != right => errors.push(
            "Cannot merge schemas: both schemas define a different schema description.".to_owned(),
        ),
        (None, Some(right)) => *current = Some(right.clone()),
        _ => {}
    }
}

fn merge_root(
    current: &mut Option<Name>,
    incoming: Option<&Name>,
    label: &str,
    errors: &mut SchemaError,
) {
    let Some(incoming) = incoming else {
        return;
    };
    match current.as_ref() {
        Some(existing) if existing == incoming => {}
        Some(existing) => errors.push(format!(
            "Cannot merge schemas: both schemas define a {label} root type, as \"{existing}\" and \"{incoming}\"."
        )),
        None => *current = Some(incoming.clone()),
    }
}

fn merge_types(name: &str, left: &Type, right: &Type) -> Result<Type, String> {
    // The same definition allocation on both sides means there is nothing to merge. This is the
    // common case when a schema is merged with a schema derived from it, and it must not pay for
    // a deep clone or an equivalence comparison.
    if type_ptr_eq(left, right) {
        return Ok(left.clone());
    }
    match (left, right) {
        (Type::Scalar(left), Type::Scalar(right)) => {
            if !scalar_equivalent(left, right) {
                return Err(type_conflict(name));
            }
            Ok(Type::Scalar(merge_scalar(name, left, right)?))
        }
        (Type::Object(left), Type::Object(right)) => {
            if !object_equivalent(left, right) {
                return Err(type_conflict(name));
            }
            Ok(Type::Object(merge_object(name, left, right)?))
        }
        (Type::Interface(left), Type::Interface(right)) => {
            if !interface_equivalent(left, right) {
                return Err(type_conflict(name));
            }
            Ok(Type::Interface(merge_interface(name, left, right)?))
        }
        (Type::Union(left), Type::Union(right)) => {
            if !union_equivalent(left, right) {
                return Err(type_conflict(name));
            }
            Ok(Type::Union(merge_union(name, left, right)?))
        }
        (Type::Enum(left), Type::Enum(right)) => {
            if !enum_equivalent(left, right) {
                return Err(type_conflict(name));
            }
            Ok(Type::Enum(left.clone()))
        }
        (Type::InputObject(left), Type::InputObject(right)) => {
            if !input_object_equivalent(left, right) {
                return Err(type_conflict(name));
            }
            Ok(Type::InputObject(left.clone()))
        }
        _ => Err(format!(
            "Cannot merge schemas: type \"{name}\" has a different kind in each schema ({} and {}).",
            left.kind(),
            right.kind()
        )),
    }
}

/// Whether both sides point at the same definition allocation.
fn type_ptr_eq(left: &Type, right: &Type) -> bool {
    match (left, right) {
        (Type::Scalar(left), Type::Scalar(right)) => Arc::ptr_eq(left, right),
        (Type::Object(left), Type::Object(right)) => Arc::ptr_eq(left, right),
        (Type::Interface(left), Type::Interface(right)) => Arc::ptr_eq(left, right),
        (Type::Union(left), Type::Union(right)) => Arc::ptr_eq(left, right),
        (Type::Enum(left), Type::Enum(right)) => Arc::ptr_eq(left, right),
        (Type::InputObject(left), Type::InputObject(right)) => Arc::ptr_eq(left, right),
        _ => false,
    }
}

fn type_conflict(name: &str) -> String {
    format!("Cannot merge schemas: type \"{name}\" is defined differently in both schemas.")
}

fn scalar_equivalent(left: &ScalarType, right: &ScalarType) -> bool {
    left.name == right.name
        && left.description == right.description
        && left.specified_by_url == right.specified_by_url
        && applied_directives_equivalent(&left.directives, &right.directives)
}

fn object_equivalent(left: &ObjectType, right: &ObjectType) -> bool {
    left.name == right.name
        && left.description == right.description
        && applied_directives_equivalent(&left.directives, &right.directives)
        && same_names(&left.interfaces, &right.interfaces)
        && fields_equivalent(&left.fields, &right.fields)
}

fn interface_equivalent(left: &InterfaceType, right: &InterfaceType) -> bool {
    left.name == right.name
        && left.description == right.description
        && applied_directives_equivalent(&left.directives, &right.directives)
        && same_names(&left.interfaces, &right.interfaces)
        && fields_equivalent(&left.fields, &right.fields)
}

fn union_equivalent(left: &UnionType, right: &UnionType) -> bool {
    left.name == right.name
        && left.description == right.description
        && applied_directives_equivalent(&left.directives, &right.directives)
        && same_names(&left.types, &right.types)
}

fn enum_equivalent(left: &EnumType, right: &EnumType) -> bool {
    left.name == right.name
        && left.description == right.description
        && applied_directives_equivalent(&left.directives, &right.directives)
        && left.values.len() == right.values.len()
        && left.values.iter().all(|(name, value)| {
            right.values.get(name).is_some_and(|other| {
                value.name == other.name
                    && value.description == other.description
                    && value.deprecation_reason == other.deprecation_reason
                    && applied_directives_equivalent(&value.directives, &other.directives)
            })
        })
}

fn input_object_equivalent(left: &InputObjectType, right: &InputObjectType) -> bool {
    left.name == right.name
        && left.description == right.description
        && applied_directives_equivalent(&left.directives, &right.directives)
        && left.one_of == right.one_of
        && left.fields.len() == right.fields.len()
        && left.fields.iter().all(|(name, value)| {
            right
                .fields
                .get(name)
                .is_some_and(|other| input_value_equivalent(value, other))
        })
}

fn fields_equivalent(left: &IndexMap<Name, Field>, right: &IndexMap<Name, Field>) -> bool {
    left.len() == right.len()
        && left.iter().all(|(name, field)| {
            right.get(name).is_some_and(|other| {
                field.name == other.name
                    && field.description == other.description
                    && field.ty == other.ty
                    && field.deprecation_reason == other.deprecation_reason
                    && applied_directives_equivalent(&field.directives, &other.directives)
                    && input_values_equivalent(&field.args, &other.args)
            })
        })
}

fn directive_equivalent(left: &DirectiveDefinition, right: &DirectiveDefinition) -> bool {
    left.name == right.name
        && left.description == right.description
        && left.repeatable == right.repeatable
        && same_slice(left.locations.as_slice(), right.locations.as_slice())
        && applied_directives_equivalent(&left.directives, &right.directives)
        && left.deprecation_reason == right.deprecation_reason
        && input_values_equivalent(&left.args, &right.args)
}

fn input_values_equivalent(
    left: &IndexMap<Name, InputValue>,
    right: &IndexMap<Name, InputValue>,
) -> bool {
    left.len() == right.len()
        && left.iter().all(|(name, value)| {
            right
                .get(name)
                .is_some_and(|other| input_value_equivalent(value, other))
        })
}

fn input_value_equivalent(left: &InputValue, right: &InputValue) -> bool {
    left.name == right.name
        && left.description == right.description
        && left.ty == right.ty
        && left.default_value == right.default_value
        && left.deprecation_reason == right.deprecation_reason
        && applied_directives_equivalent(&left.directives, &right.directives)
}

/// Compare two applied-directive lists as multisets.
///
/// The order of different directives, and the argument order of one directive, carry no meaning.
/// Repeatable directives may still be applied several times, so the comparison is a multiset one
/// rather than a set one.
///
/// `@deprecated`, `@specifiedBy` and `@oneOf` are ignored: their authoritative data lives in the
/// dedicated model fields, and a schema may record the application in the directive list, the
/// dedicated field, or both.
fn applied_directives_equivalent(left: &[AppliedDirective], right: &[AppliedDirective]) -> bool {
    const DEDICATED: [&str; 3] = ["deprecated", "specifiedBy", "oneOf"];
    let left: Vec<&AppliedDirective> = left
        .iter()
        .filter(|directive| !DEDICATED.contains(&directive.name.as_ref()))
        .collect();
    let right: Vec<&AppliedDirective> = right
        .iter()
        .filter(|directive| !DEDICATED.contains(&directive.name.as_ref()))
        .collect();
    if left.len() != right.len() {
        return false;
    }
    let mut matched = vec![false; right.len()];
    'left: for directive in left {
        for (index, other) in right.iter().enumerate() {
            if !matched[index]
                && directive.name == other.name
                && directive.arguments == other.arguments
            {
                matched[index] = true;
                continue 'left;
            }
        }
        return false;
    }
    true
}

/// Compare two name lists as sets: GraphQL type members and interfaces have no meaningful order.
fn same_names(left: &[Name], right: &[Name]) -> bool {
    same_slice(left, right)
}

/// Compare two slices as sets.
fn same_slice<T: Eq>(left: &[T], right: &[T]) -> bool {
    left.len() == right.len() && left.iter().all(|value| right.contains(value))
}

fn merge_scalar(
    name: &str,
    left: &Arc<ScalarType>,
    right: &ScalarType,
) -> Result<Arc<ScalarType>, String> {
    // Copying a function pointer is only necessary when the right side supplies one that the
    // left side lacks. If it supplies none (or the same allocation), the left definition is
    // already the merged result, so the shared `Arc` can be reused instead of deep-cloned.
    //
    // Two built-in coercion functions are interchangeable even when they are distinct
    // allocations: they are the engine's own code, and the specification defines their behaviour.
    let both_builtin = left.builtin && right.builtin;
    let mut needs_copy = builtin_slot_needed(&left.serialize, &right.serialize, both_builtin)
        .map_err(|()| runtime_conflict(name, "serialize"))?;
    needs_copy |= builtin_slot_needed(&left.parse_value, &right.parse_value, both_builtin)
        .map_err(|()| runtime_conflict(name, "parse_value"))?;
    needs_copy |= builtin_slot_needed(&left.parse_literal, &right.parse_literal, both_builtin)
        .map_err(|()| runtime_conflict(name, "parse_literal"))?;
    if !needs_copy {
        return Ok(left.clone());
    }
    let mut merged = (**left).clone();
    if merged.serialize.is_none() {
        merged.serialize = right.serialize.clone();
        merged.builtin = right.builtin;
    }
    if merged.parse_value.is_none() {
        merged.parse_value = right.parse_value.clone();
    }
    if merged.parse_literal.is_none() {
        merged.parse_literal = right.parse_literal.clone();
    }
    Ok(Arc::new(merged))
}

/// Whether `source` fills a runtime slot `target` leaves empty, and fail on a real conflict.
///
/// `needs_copy` records that a merge is required; a conflict is reported exactly as the merge
/// loop reports it, so the fast path cannot change which error wins.
fn fill_slot<T: ?Sized>(
    owner: &str,
    slot: &str,
    target: &Option<Arc<T>>,
    source: &Option<Arc<T>>,
    needs_copy: &mut bool,
) -> Result<(), String> {
    match scalar_slot_needed(target, source) {
        Ok(fills) => *needs_copy |= fills,
        Err(()) => return Err(runtime_conflict(owner, slot)),
    }
    Ok(())
}

/// Whether a merge has to copy anything, without allocating or deep-cloning.
///
/// `Ok(true)` means `source` sets a slot `target` leaves unset. `Err(())` means both sides set
/// the slot to different implementations; the caller decides which slot name to report.
fn scalar_slot_needed<T: ?Sized>(
    target: &Option<Arc<T>>,
    source: &Option<Arc<T>>,
) -> Result<bool, ()> {
    match (target, source) {
        (_, None) => Ok(false),
        (None, Some(_)) => Ok(true),
        (Some(left), Some(right)) if Arc::ptr_eq(left, right) => Ok(false),
        (Some(_), Some(_)) => Err(()),
    }
}

/// [`scalar_slot_needed`] for a scalar's coercion functions.
///
/// The specification defines the built-in functions, so two built-in coercers are compatible
/// even when each scalar instance allocated its own copy.
fn builtin_slot_needed<T: ?Sized>(
    target: &Option<Arc<T>>,
    source: &Option<Arc<T>>,
    both_builtin: bool,
) -> Result<bool, ()> {
    match (target, source) {
        (_, None) => Ok(false),
        (None, Some(_)) => Ok(true),
        (Some(left), Some(right)) if Arc::ptr_eq(left, right) || both_builtin => Ok(false),
        (Some(_), Some(_)) => Err(()),
    }
}

/// Whether merging a field needs to copy runtime behaviour, and whether it is possible.
///
/// The three slots are checked in the same order [`merge_field_runtime`] copies them, so a
/// conflict reports the same slot it would have reported on the slow path.
fn merge_field_slots(
    owner: &str,
    name: &str,
    target: &Field,
    source: &Field,
) -> Result<bool, String> {
    let resolve = scalar_slot_needed(&target.resolve, &source.resolve);
    let subscribe = scalar_slot_needed(&target.subscribe, &source.subscribe);
    let complexity = scalar_slot_needed(&target.complexity, &source.complexity);
    if resolve.is_err() || subscribe.is_err() || complexity.is_err() {
        let slot = if resolve.is_err() {
            "resolve"
        } else if subscribe.is_err() {
            "subscribe"
        } else {
            "complexity"
        };
        return Err(runtime_conflict(&format!("{owner}.{name}"), slot));
    }
    Ok(resolve == Ok(true) || subscribe == Ok(true) || complexity == Ok(true))
}

fn merge_object(
    name: &str,
    left: &Arc<ObjectType>,
    right: &ObjectType,
) -> Result<Arc<ObjectType>, String> {
    let mut needs_copy = false;
    fill_slot(
        name,
        "is_type_of",
        &left.is_type_of,
        &right.is_type_of,
        &mut needs_copy,
    )?;
    for (field_name, field) in &left.fields {
        if let Some(other) = right.fields.get(field_name) {
            if merge_field_slots(name, field_name, field, other)? {
                needs_copy = true;
            }
        }
    }
    if !needs_copy {
        return Ok(left.clone());
    }
    let mut merged = (**left).clone();
    if merged.is_type_of.is_none() {
        merged.is_type_of = right.is_type_of.clone();
    }
    for (field_name, field) in &mut merged.fields {
        if let Some(other) = right.fields.get(field_name) {
            merge_field_runtime(name, field_name, field, other)?;
        }
    }
    Ok(Arc::new(merged))
}

fn merge_interface(
    name: &str,
    left: &Arc<InterfaceType>,
    right: &InterfaceType,
) -> Result<Arc<InterfaceType>, String> {
    let mut needs_copy = false;
    fill_slot(
        name,
        "resolve_type",
        &left.resolve_type,
        &right.resolve_type,
        &mut needs_copy,
    )?;
    for (field_name, field) in &left.fields {
        if let Some(other) = right.fields.get(field_name) {
            if merge_field_slots(name, field_name, field, other)? {
                needs_copy = true;
            }
        }
    }
    if !needs_copy {
        return Ok(left.clone());
    }
    let mut merged = (**left).clone();
    if merged.resolve_type.is_none() {
        merged.resolve_type = right.resolve_type.clone();
    }
    for (field_name, field) in &mut merged.fields {
        if let Some(other) = right.fields.get(field_name) {
            merge_field_runtime(name, field_name, field, other)?;
        }
    }
    Ok(Arc::new(merged))
}

fn merge_union(
    name: &str,
    left: &Arc<UnionType>,
    right: &UnionType,
) -> Result<Arc<UnionType>, String> {
    let mut needs_copy = false;
    fill_slot(
        name,
        "resolve_type",
        &left.resolve_type,
        &right.resolve_type,
        &mut needs_copy,
    )?;
    if !needs_copy {
        return Ok(left.clone());
    }
    let mut merged = (**left).clone();
    if merged.resolve_type.is_none() {
        merged.resolve_type = right.resolve_type.clone();
    }
    Ok(Arc::new(merged))
}

/// Whether two optional function slots refer to the same implementation.
fn same_slot<T: ?Sized>(left: &Option<Arc<T>>, right: &Option<Arc<T>>) -> bool {
    match (left, right) {
        (Some(left), Some(right)) => Arc::ptr_eq(left, right),
        _ => true,
    }
}

fn runtime_conflict(owner: &str, slot: &str) -> String {
    format!(
        "Cannot merge schemas: \"{owner}\" has different {slot} implementations in each schema. Runtime behaviour is never chosen by merge order; keep one definition or merge a schema that leaves the slot unset."
    )
}

/// Fill resolver slots a field has not set from its counterpart.
///
/// Two different implementations are a conflict rather than a precedence rule: authorization,
/// complexity limits and subscription behaviour all live in these slots, so silently picking the
/// receiver's function would make the merged schema depend on argument order.
fn merge_field_runtime(
    owner: &str,
    name: &str,
    target: &mut Field,
    source: &Field,
) -> Result<(), String> {
    let path = format!("{owner}.{name}");
    if target.resolve.is_none() {
        target.resolve = source.resolve.clone();
    } else if source.resolve.is_some() && !same_slot(&target.resolve, &source.resolve) {
        return Err(runtime_conflict(&path, "resolve"));
    }
    if target.subscribe.is_none() {
        target.subscribe = source.subscribe.clone();
    } else if source.subscribe.is_some() && !same_slot(&target.subscribe, &source.subscribe) {
        return Err(runtime_conflict(&path, "subscribe"));
    }
    if target.complexity.is_none() {
        target.complexity = source.complexity.clone();
    } else if source.complexity.is_some() && !same_slot(&target.complexity, &source.complexity) {
        return Err(runtime_conflict(&path, "complexity"));
    }
    Ok(())
}

/// Copy runtime behaviour from an original schema onto a schema rebuilt from its SDL.
fn overlay_resolvers(source: &Schema, target: &mut Schema) {
    let inner = Arc::make_mut(&mut target.inner);
    for (_, ty) in &mut inner.types {
        match ty {
            Type::Scalar(target) => {
                let Some(source) = source.scalar_type(target.name.as_ref()) else {
                    continue;
                };
                let target = Arc::make_mut(target);
                if target.serialize.is_none() {
                    target.serialize = source.serialize.clone();
                    target.builtin = source.builtin;
                }
                if target.parse_value.is_none() {
                    target.parse_value = source.parse_value.clone();
                }
                if target.parse_literal.is_none() {
                    target.parse_literal = source.parse_literal.clone();
                }
            }
            Type::Object(target) => {
                let Some(source) = source.object_type(target.name.as_ref()) else {
                    continue;
                };
                let target = Arc::make_mut(target);
                if target.is_type_of.is_none() {
                    target.is_type_of = source.is_type_of.clone();
                }
                for (name, field) in &mut target.fields {
                    if let Some(source) = source.fields.get(name) {
                        fill_field_runtime(field, source);
                    }
                }
            }
            Type::Interface(target) => {
                let Some(source) = source.interface_type(target.name.as_ref()) else {
                    continue;
                };
                let target = Arc::make_mut(target);
                if target.resolve_type.is_none() {
                    target.resolve_type = source.resolve_type.clone();
                }
                for (name, field) in &mut target.fields {
                    if let Some(source) = source.fields.get(name) {
                        fill_field_runtime(field, source);
                    }
                }
            }
            Type::Union(target) => {
                let Some(source) = source.union_type(target.name.as_ref()) else {
                    continue;
                };
                if target.resolve_type.is_none() {
                    Arc::make_mut(target).resolve_type = source.resolve_type.clone();
                }
            }
            Type::Enum(_) | Type::InputObject(_) => {}
        }
    }
}

/// Fill runtime slots without checking for conflicts.
///
/// The target is rebuilt from SDL, so it has no runtime behaviour of its own; every slot it
/// receives comes from `source`.
fn fill_field_runtime(target: &mut Field, source: &Field) {
    if target.resolve.is_none() {
        target.resolve = source.resolve.clone();
    }
    if target.subscribe.is_none() {
        target.subscribe = source.subscribe.clone();
    }
    if target.complexity.is_none() {
        target.complexity = source.complexity.clone();
    }
}

/// Whether a type name belongs to the introspection system.
fn is_introspection(name: &str) -> bool {
    name.starts_with("__")
}
