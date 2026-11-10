//! The schema builder.
//!
//! A [`Schema`] owns the definitions being assembled. Types are created through closures that
//! receive a builder, which keeps the definition of a type next to the type itself:
//!
//! ```
//! use gurafi::{Object, Schema, NoArgs};
//!
//! #[derive(Object)]
//! struct Giraffe {
//!     name: String,
//! }
//!
//! struct Ctx;
//!
//! let mut schema = Schema::<Ctx>::new();
//! schema.object::<Giraffe>(|o| {
//!     o.expose(gurafi::expose!(Giraffe::name));
//! });
//! schema.query(|q| {
//!     q.field("giraffe", |_: NoArgs, _: &Ctx| Giraffe { name: "Melman".into() });
//! });
//! let schema = schema.build().expect("valid schema");
//! assert!(schema.object_type("Giraffe").is_some());
//! ```
//!
//! A type's definition can be spread over several [`Schema::object_fields`],
//! [`Schema::interface_fields`], [`Schema::query_fields`], [`Schema::mutation_fields`] and
//! [`Schema::subscription_fields`] calls, including from different modules. Declarations merge;
//! duplicate field names are still build errors.

use std::{any::TypeId, collections::HashMap, marker::PhantomData, sync::Arc};

use futures::{Stream, StreamExt};
use gql::{GraphQLError, Name, TypeRef};
use indexmap::IndexMap;

use crate::{
    error::SchemaError,
    expose::Expose,
    field::{AuthGuard, FieldCheck, FieldConfig, InterfaceFieldConfig, apply_auth_guard},
    registry::TypeRegistry,
    traits::{
        EnumModel, InputObject, IntoValue, ObjectModel, OutputType, ScalarValue, coerce_args,
        model_ref,
    },
};

/// A boxed future returned by an asynchronous resolver.
///
/// Asynchronous resolvers accept a closure returning this type, so a resolver is written as
/// `Box::pin(async move { ... })`. The box is allocated once per field resolution; `gql`
/// already boxes every resolver future, so this is the same allocation the executor needs
/// anyway.
pub type BoxFuture<'a, T> = futures::future::BoxFuture<'a, T>;

/// An interface definition created by [`Schema::interface`].
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub struct InterfaceRef {
    name: &'static str,
}

impl InterfaceRef {
    /// The interface's GraphQL name.
    #[must_use]
    pub fn name(&self) -> &'static str {
        self.name
    }
}

/// A union definition created by [`Schema::union`].
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub struct UnionRef {
    name: &'static str,
}

impl UnionRef {
    /// The union's GraphQL name.
    #[must_use]
    pub fn name(&self) -> &'static str {
        self.name
    }
}

/// Options applied when a schema is built.
///
/// The defaults match [`Schema::build`]: no root descriptions and no reordering.
#[derive(Clone, Debug, Default)]
pub struct BuildOptions {
    query_description: Option<Arc<str>>,
    mutation_description: Option<Arc<str>>,
    subscription_description: Option<Arc<str>>,
    sort_types: bool,
}

impl BuildOptions {
    /// Default options.
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }

    /// Set the query root type's description.
    #[must_use]
    pub fn query_description(mut self, description: impl Into<Arc<str>>) -> Self {
        self.query_description = Some(description.into());
        self
    }

    /// Set the mutation root type's description.
    #[must_use]
    pub fn mutation_description(mut self, description: impl Into<Arc<str>>) -> Self {
        self.mutation_description = Some(description.into());
        self
    }

    /// Set the subscription root type's description.
    #[must_use]
    pub fn subscription_description(mut self, description: impl Into<Arc<str>>) -> Self {
        self.subscription_description = Some(description.into());
        self
    }

    /// Sort named types by name when handing them to `gql`.
    ///
    /// Without this, types are registered in declaration order. Sorting makes SDL output and
    /// introspection snapshots stable across modules and registration order.
    #[must_use]
    pub fn sort_types(mut self, sort: bool) -> Self {
        self.sort_types = sort;
        self
    }
}

/// A field that inherits its type and arguments from an implemented interface's field.
#[derive(Clone, Debug)]
pub(crate) struct PendingInherited {
    object: &'static str,
    field: Name,
    expected_args: Vec<(Name, TypeRef)>,
}

/// A type-safe, code-first GraphQL schema builder.
///
/// `Ctx` is the per-request context type (an authenticated user, database handles, request
/// identifiers). It is supplied by the server through
/// [`gql::ExecutionOptions::with_context`] and passed to every resolver and authorization
/// predicate.
///
/// Build a schema by registering objects, interfaces and unions, then adding root fields:
///
/// ```
/// use gurafi::{Object, Schema, NoArgs};
///
/// #[derive(Object)]
/// struct Greeting {
///     message: String,
/// }
///
/// struct Ctx {}
///
/// let mut schema = Schema::<Ctx>::new();
/// schema.object::<Greeting>(|o| {
///     o.expose(gurafi::expose!(Greeting::message));
/// });
/// schema.query(|q| {
///     q.field("greeting", |_: NoArgs, _: &Ctx| Greeting {
///         message: "Hello".into(),
///     });
/// });
/// let schema = schema.build().expect("valid schema");
/// ```
pub struct Schema<Ctx: 'static = ()> {
    registry: TypeRegistry,
    objects: IndexMap<&'static str, gql::ObjectType>,
    /// The Rust model that owns each object name, so two models cannot silently merge.
    object_owners: IndexMap<&'static str, TypeId>,
    interfaces: IndexMap<&'static str, gql::InterfaceType>,
    unions: IndexMap<&'static str, gql::UnionType>,
    query: Vec<gql::Field>,
    mutation: Vec<gql::Field>,
    subscription: Vec<gql::Field>,
    errors: Vec<GraphQLError>,
    query_name: String,
    mutation_name: String,
    subscription_name: String,
    description: Option<Arc<str>>,
    checks: Vec<FieldCheck>,
    object_auth: IndexMap<String, Arc<AuthGuard<Ctx>>>,
    interface_auth: IndexMap<String, Arc<AuthGuard<Ctx>>>,
    interface_field_auth: IndexMap<String, IndexMap<Name, Arc<AuthGuard<Ctx>>>>,
    pending_inherited: Vec<PendingInherited>,
    marker: PhantomData<fn() -> Ctx>,
}

impl<Ctx: 'static> Clone for Schema<Ctx> {
    fn clone(&self) -> Self {
        Self {
            registry: self.registry.clone(),
            objects: self.objects.clone(),
            object_owners: self.object_owners.clone(),
            interfaces: self.interfaces.clone(),
            unions: self.unions.clone(),
            query: self.query.clone(),
            mutation: self.mutation.clone(),
            subscription: self.subscription.clone(),
            errors: self.errors.clone(),
            query_name: self.query_name.clone(),
            mutation_name: self.mutation_name.clone(),
            subscription_name: self.subscription_name.clone(),
            description: self.description.clone(),
            checks: self.checks.clone(),
            object_auth: self.object_auth.clone(),
            interface_auth: self.interface_auth.clone(),
            interface_field_auth: self.interface_field_auth.clone(),
            pending_inherited: self.pending_inherited.clone(),
            marker: PhantomData,
        }
    }
}

impl<Ctx: 'static> Default for Schema<Ctx> {
    fn default() -> Self {
        Self::new()
    }
}

impl<Ctx: 'static> std::fmt::Debug for Schema<Ctx> {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Schema")
            .field("objects", &self.objects.len())
            .field("interfaces", &self.interfaces.len())
            .field("unions", &self.unions.len())
            .field("query_fields", &self.query.len())
            .field("mutation_fields", &self.mutation.len())
            .field("subscription_fields", &self.subscription.len())
            .finish_non_exhaustive()
    }
}

impl<Ctx: 'static> Schema<Ctx> {
    /// Create an empty schema.
    ///
    /// Root type names default to `Query`, `Mutation` and `Subscription`. A root type is only
    /// added to the schema when it has at least one field.
    #[must_use]
    pub fn new() -> Self {
        Self {
            registry: TypeRegistry::new(),
            objects: IndexMap::new(),
            object_owners: IndexMap::new(),
            interfaces: IndexMap::new(),
            unions: IndexMap::new(),
            query: Vec::new(),
            mutation: Vec::new(),
            subscription: Vec::new(),
            errors: Vec::new(),
            query_name: "Query".to_owned(),
            mutation_name: "Mutation".to_owned(),
            subscription_name: "Subscription".to_owned(),
            description: None,
            checks: Vec::new(),
            object_auth: IndexMap::new(),
            interface_auth: IndexMap::new(),
            interface_field_auth: IndexMap::new(),
            pending_inherited: Vec::new(),
            marker: PhantomData,
        }
    }

    /// Set the schema's description.
    pub fn description(&mut self, description: impl Into<Arc<str>>) -> &mut Self {
        self.description = Some(description.into());
        self
    }

    /// Override the query root type's name. Defaults to `Query`.
    pub fn query_name(&mut self, name: impl Into<String>) -> &mut Self {
        self.query_name = name.into();
        self
    }

    /// Override the mutation root type's name. Defaults to `Mutation`.
    pub fn mutation_name(&mut self, name: impl Into<String>) -> &mut Self {
        self.mutation_name = name.into();
        self
    }

    /// Override the subscription root type's name. Defaults to `Subscription`.
    pub fn subscription_name(&mut self, name: impl Into<String>) -> &mut Self {
        self.subscription_name = name.into();
        self
    }

    /// Register an object type.
    ///
    /// The GraphQL name and description come from `M`'s [`ObjectModel`] implementation, which
    /// `#[derive(Object)]` generates. Fields are declared in the closure; a field may return
    /// any type that implements [`OutputType`], including a type registered later, because
    /// `gql` resolves type names when the schema is built.
    ///
    /// Calling this again for the same model reopens the object and adds fields to it. This is
    /// how a type's definition can live in several modules. A field name declared twice is
    /// still a build error.
    pub fn object<M: ObjectModel>(
        &mut self,
        build: impl FnOnce(&mut ObjectBuilder<'_, Ctx, M>),
    ) -> &mut Self {
        match self.object_owners.get(M::NAME) {
            Some(owner) if *owner != TypeId::of::<M>() => {
                self.errors.push(GraphQLError::new(format!(
                    "Object type `{}` is registered by more than one Rust type. Give one of \
                     them a distinct `#[graphql(name = \"...\")]`.",
                    M::NAME
                )));
                return self;
            }
            Some(_) => {}
            None => {
                self.object_owners.insert(M::NAME, TypeId::of::<M>());
            }
        }
        let existing = self.objects.shift_remove(M::NAME);
        let mut builder = ObjectBuilder::<Ctx, M>::new(
            existing,
            &mut self.registry,
            &mut self.checks,
            &mut self.pending_inherited,
            M::NAME,
        );
        build(&mut builder);
        let (object, auth) = builder.finish();
        if let Some(auth) = auth {
            self.object_auth.insert(M::NAME.to_owned(), auth);
        }
        self.objects.insert(M::NAME, object);
        self
    }

    /// Reopen an object type and add more fields to it.
    ///
    /// This is an alias for [`Schema::object`] with a name that says the type may already
    /// exist. Use it when fields are split across modules.
    pub fn object_fields<M: ObjectModel>(
        &mut self,
        build: impl FnOnce(&mut ObjectBuilder<'_, Ctx, M>),
    ) -> &mut Self {
        self.object::<M>(build)
    }

    /// Add one field to an object without opening a builder closure.
    ///
    /// The object is created if it does not exist yet. The returned [`FieldConfig`] can carry
    /// the usual descriptions, deprecation and authorization options.
    pub fn object_field<M, T, A, F>(
        &mut self,
        name: &'static str,
        resolver: F,
    ) -> FieldConfig<'_, Ctx>
    where
        M: ObjectModel,
        T: OutputType + IntoValue + Send + 'static,
        A: InputObject,
        F: Fn(&M, A, &Ctx) -> T + Send + Sync + 'static,
    {
        match self.object_owners.get(M::NAME) {
            Some(owner) if *owner != TypeId::of::<M>() => {
                self.errors.push(GraphQLError::new(format!(
                    "Object type `{}` is registered by more than one Rust type. Give one of \
                     them a distinct `#[graphql(name = \"...\")]`.",
                    M::NAME
                )));
            }
            Some(_) => {}
            None => {
                self.object_owners.insert(M::NAME, TypeId::of::<M>());
                self.objects.insert(M::NAME, new_object::<M>());
            }
        }
        if !self.objects.contains_key(M::NAME) {
            self.objects.insert(M::NAME, new_object::<M>());
        }
        let field = object_field::<M, Ctx, T, A, F>(name, resolver, &mut self.registry);
        let field_name = field.name.clone();
        let object = self
            .objects
            .get_mut(M::NAME)
            .expect("the object was just inserted");
        object.insert_field(field);
        FieldConfig::for_object_checked(
            &mut self.registry,
            &mut object.fields,
            &mut self.checks,
            M::NAME,
            field_name,
            T::is_optional(),
            std::any::type_name::<T>(),
        )
    }

    /// Register an interface type.
    ///
    /// Returns a [`InterfaceRef`] that object builders use with
    /// [`ObjectBuilder::implements`]. Interface fields have no resolvers: execution uses the
    /// concrete object's field resolvers.
    ///
    /// Calling this again for the same name reopens the interface and adds fields.
    pub fn interface(
        &mut self,
        name: &'static str,
        build: impl FnOnce(&mut InterfaceBuilder<'_, Ctx>),
    ) -> InterfaceRef {
        let existing = self.interfaces.shift_remove(name);
        let field_auth = self
            .interface_field_auth
            .shift_remove(name)
            .unwrap_or_default();
        let mut builder =
            InterfaceBuilder::<Ctx>::new(existing, field_auth, &mut self.registry, name);
        build(&mut builder);
        let (interface, auth, field_auth) = builder.finish();
        if let Some(auth) = auth {
            self.interface_auth.insert(name.to_owned(), auth);
        }
        self.interface_field_auth
            .insert(name.to_owned(), field_auth);
        self.interfaces.insert(name, interface);
        InterfaceRef { name }
    }

    /// Reopen an interface and add more fields to it.
    ///
    /// This is an alias for [`Schema::interface`].
    pub fn interface_fields(
        &mut self,
        name: &'static str,
        build: impl FnOnce(&mut InterfaceBuilder<'_, Ctx>),
    ) -> InterfaceRef {
        self.interface(name, build)
    }

    /// Register a union type.
    ///
    /// Returns a [`UnionRef`] that fields can use with
    /// [`FieldConfig::as_abstract`], or that [`crate::UnionModel`] output types reference.
    /// Runtime type resolution relies on each object's generated `is_type_of` check, unless the
    /// union installs its own resolver with [`UnionBuilder::resolve_type`].
    ///
    /// Calling this again for the same name appends members.
    pub fn union(
        &mut self,
        name: &'static str,
        build: impl FnOnce(&mut UnionBuilder<Ctx>),
    ) -> UnionRef {
        let existing = self.unions.shift_remove(name);
        let mut builder = UnionBuilder::<Ctx>::new(existing, name);
        build(&mut builder);
        self.unions.insert(name, builder.finish());
        UnionRef { name }
    }

    /// Register a custom scalar explicitly.
    ///
    /// Scalars used by a field's input or output type are registered automatically; this is for
    /// scalars that are part of the public schema without appearing in a field signature.
    pub fn scalar<T: ScalarValue>(&mut self) -> &mut Self {
        self.registry.register_scalar::<T>();
        self
    }

    /// Register an enum type explicitly.
    ///
    /// Enums used by a field's input or output type are registered automatically.
    pub fn enum_type<T: EnumModel>(&mut self) -> &mut Self {
        self.registry.register_enum::<T>();
        self
    }

    /// Add query root fields.
    ///
    /// Every call appends, so a root can be declared from several modules.
    pub fn query(&mut self, build: impl FnOnce(&mut RootBuilder<'_, Ctx>)) -> &mut Self {
        let mut builder = RootBuilder::<Ctx>::new(
            &mut self.registry,
            &mut self.query,
            &mut self.checks,
            self.query_name.clone(),
        );
        build(&mut builder);
        self
    }

    /// Add more query root fields. An alias for [`Schema::query`].
    pub fn query_fields(&mut self, build: impl FnOnce(&mut RootBuilder<'_, Ctx>)) -> &mut Self {
        self.query(build)
    }

    /// Add mutation root fields. Every call appends.
    pub fn mutation(&mut self, build: impl FnOnce(&mut RootBuilder<'_, Ctx>)) -> &mut Self {
        let mut builder = RootBuilder::<Ctx>::new(
            &mut self.registry,
            &mut self.mutation,
            &mut self.checks,
            self.mutation_name.clone(),
        );
        build(&mut builder);
        self
    }

    /// Add more mutation root fields. An alias for [`Schema::mutation`].
    pub fn mutation_fields(&mut self, build: impl FnOnce(&mut RootBuilder<'_, Ctx>)) -> &mut Self {
        self.mutation(build)
    }

    /// Add subscription root fields. Every call appends.
    pub fn subscription(
        &mut self,
        build: impl FnOnce(&mut SubscriptionBuilder<'_, Ctx>),
    ) -> &mut Self {
        let mut builder = SubscriptionBuilder::<Ctx>::new(
            &mut self.registry,
            &mut self.subscription,
            &mut self.checks,
            self.subscription_name.clone(),
        );
        build(&mut builder);
        self
    }

    /// Add more subscription root fields. An alias for [`Schema::subscription`].
    pub fn subscription_fields(
        &mut self,
        build: impl FnOnce(&mut SubscriptionBuilder<'_, Ctx>),
    ) -> &mut Self {
        self.subscription(build)
    }

    /// Validate the schema and freeze it into a [`gql::Schema`].
    ///
    /// # Errors
    ///
    /// Returns every problem found: duplicate type names, fields that reference an object that
    /// was never registered, invalid interface implementations, and so on. See
    /// [`SchemaError`].
    pub fn build(self) -> Result<gql::Schema, SchemaError> {
        self.build_with(BuildOptions::default())
    }

    /// Validate the schema with [`BuildOptions`] and freeze it into a [`gql::Schema`].
    ///
    /// `build_with` consumes the builder. Use [`Schema::build_clone`] when the builder is still
    /// needed, or call `schema.clone().build_with(options)`.
    pub fn build_with(mut self, options: BuildOptions) -> Result<gql::Schema, SchemaError> {
        let mut errors = std::mem::take(&mut self.errors);
        errors.extend(self.registry.take_errors());
        let has_unnamed_input = self.registry.has_unnamed_input();

        for name in self.registry.referenced_objects() {
            if !self.objects.contains_key(name) {
                errors.push(GraphQLError::new(format!(
                    "Object type `{name}` is used by a field but was never registered. Call \
                     `schema.object::<{name}>(...)` before calling `build()`."
                )));
            }
        }
        for name in self.registry.referenced_unions() {
            if !self.unions.contains_key(name) {
                errors.push(GraphQLError::new(format!(
                    "Union type `{name}` is used by a field but was never registered. Call \
                     `schema.union(\"{name}\", ...)` before calling `build()`."
                )));
            }
        }

        self.resolve_inherited(&mut errors);
        self.apply_guards();

        let mut builder = gql::Schema::builder();
        if let Some(description) = self.description.clone() {
            builder = builder.description(description);
        }

        let mut registry_types: Vec<(String, gql::Type)> =
            self.registry.into_types().into_iter().collect();
        let mut objects: Vec<(&'static str, gql::ObjectType)> = self.objects.into_iter().collect();
        let mut interfaces: Vec<(&'static str, gql::InterfaceType)> =
            self.interfaces.into_iter().collect();
        let mut unions: Vec<(&'static str, gql::UnionType)> = self.unions.into_iter().collect();
        if options.sort_types {
            registry_types.sort_by(|left, right| left.0.cmp(&right.0));
            objects.sort_by_key(|(name, _)| *name);
            interfaces.sort_by_key(|(name, _)| *name);
            unions.sort_by_key(|(name, _)| *name);
        }
        for (_, ty) in registry_types {
            builder.add_type(ty);
        }
        for (_, object) in objects {
            builder.add_type(gql::Type::from(object));
        }
        for (_, interface) in interfaces {
            builder.add_type(gql::Type::from(interface));
        }
        for (_, union) in unions {
            builder.add_type(gql::Type::from(union));
        }

        if !self.query.is_empty() {
            let mut root = gql::ObjectType::new(self.query_name.clone());
            if let Some(description) = options.query_description.as_ref() {
                root = root.description(description.to_string());
            }
            for field in self.query {
                root = root.field(field);
            }
            builder.add_type(gql::Type::from(root));
            builder = builder.query(self.query_name.clone());
        }
        if !self.mutation.is_empty() {
            let mut root = gql::ObjectType::new(self.mutation_name.clone());
            if let Some(description) = options.mutation_description.as_ref() {
                root = root.description(description.to_string());
            }
            for field in self.mutation {
                root = root.field(field);
            }
            builder.add_type(gql::Type::from(root));
            builder = builder.mutation(self.mutation_name.clone());
        }
        if !self.subscription.is_empty() {
            let mut root = gql::ObjectType::new(self.subscription_name.clone());
            if let Some(description) = options.subscription_description.as_ref() {
                root = root.description(description.to_string());
            }
            for field in self.subscription {
                root = root.field(field);
            }
            builder.add_type(gql::Type::from(root));
            builder = builder.subscription(self.subscription_name.clone());
        }

        match builder.build() {
            Ok(schema) => {
                validate_checks(&schema, &self.checks, &mut errors);
                if errors.is_empty() {
                    Ok(schema)
                } else {
                    Err(SchemaError::from_errors(errors))
                }
            }
            Err(error) => {
                // An unnamed nested input already has an actionable error above. `gql` also
                // reports the internal placeholder name; drop that follow-up instead of showing
                // users an implementation detail next to the real fix.
                errors.extend(error.into_errors().into_iter().filter(|error| {
                    !(has_unnamed_input && error.message().contains("__GurafiUnnamedInput"))
                }));
                Err(SchemaError::from_errors(errors))
            }
        }
    }

    /// Build a schema from a clone, leaving this builder reusable.
    ///
    /// Registry state discovered while building is not written back to `self`, so declared
    /// types, fields and options are unchanged. This is what makes it possible to build the
    /// same schema twice, for example with different [`BuildOptions`].
    ///
    /// # Errors
    ///
    /// Same as [`Schema::build`].
    pub fn build_clone(&self) -> Result<gql::Schema, SchemaError> {
        self.clone().build()
    }

    /// Rewrite fields declared with [`ObjectBuilder::resolve_inherited`] from the matching
    /// interface field.
    fn resolve_inherited(&mut self, errors: &mut Vec<GraphQLError>) {
        let parents = interface_parents(&self.interfaces);
        for pending in std::mem::take(&mut self.pending_inherited) {
            let Some(object) = self.objects.get(pending.object) else {
                errors.push(GraphQLError::new(format!(
                    "Object `{}` was not registered, but field `{}` inherits from an interface.",
                    pending.object, pending.field
                )));
                continue;
            };
            let reachable = reachable_interfaces(&object.interfaces, &parents);
            let mut source = None;
            for interface_name in &reachable {
                let Some(interface) = self.interfaces.get(interface_name.as_str()) else {
                    continue;
                };
                if let Some(field) = interface.fields.get(&pending.field) {
                    source = Some((
                        field.ty.clone(),
                        field.args.clone(),
                        field.description.clone(),
                        interface_name.clone(),
                    ));
                    break;
                }
            }
            let Some((interface_ty, interface_args, interface_description, interface_name)) =
                source
            else {
                errors.push(GraphQLError::new(format!(
                    "Object `{}` declares field `{}` with `resolve_inherited`, but no implemented \
                     interface has that field. Declare it on an interface with \
                     `interface.field(...)`, or use `field` to give the object its own type.",
                    pending.object, pending.field
                )));
                continue;
            };
            if pending.expected_args.len() != interface_args.len()
                || pending
                    .expected_args
                    .iter()
                    .any(|(name, ty)| interface_args.get(name).is_none_or(|arg| arg.ty != *ty))
            {
                errors.push(GraphQLError::new(format!(
                    "Object field `{}.{}` inherits from `{}.{}`, but its resolver's arguments do \
                     not match. Declare an args type with the same argument names and types as the \
                     interface field.",
                    pending.object, pending.field, interface_name, pending.field
                )));
                continue;
            }

            let object = self
                .objects
                .get_mut(pending.object)
                .expect("the object was checked above");
            let Some(field) = object.fields.get_mut(&pending.field) else {
                continue;
            };
            let actual = field.ty.clone();
            self.checks.push(FieldCheck::InheritedType {
                owner: pending.object.to_owned(),
                field: pending.field.clone(),
                actual,
                expected: interface_ty.clone(),
            });
            field.ty = interface_ty;
            field.args = interface_args;
            if field.description.is_none() {
                field.description = interface_description;
            }
        }
    }

    /// Apply object- and interface-level authorization to every affected field.
    fn apply_guards(&mut self) {
        let parents = interface_parents(&self.interfaces);
        for (object_name, object) in &mut self.objects {
            let reachable = reachable_interfaces(&object.interfaces, &parents);
            let object_guard = self.object_auth.get(*object_name).cloned();
            for field in object.fields.values_mut() {
                if let Some(guard) = &object_guard {
                    apply_auth_guard(field, Arc::clone(guard));
                }
                for interface_name in &reachable {
                    if let Some(guard) = self.interface_auth.get(interface_name.as_str()) {
                        apply_auth_guard(field, Arc::clone(guard));
                    }
                    if let Some(fields) = self
                        .interface_field_auth
                        .get(interface_name.as_str())
                        .and_then(|fields| fields.get(&field.name))
                    {
                        apply_auth_guard(field, Arc::clone(fields));
                    }
                }
            }
        }
    }
}

/// The direct super-interfaces of every registered interface, keyed by name.
fn interface_parents(
    interfaces: &IndexMap<&'static str, gql::InterfaceType>,
) -> HashMap<String, Vec<String>> {
    interfaces
        .iter()
        .map(|(name, interface)| {
            (
                (*name).to_owned(),
                interface
                    .interfaces
                    .iter()
                    .map(|parent| parent.to_string())
                    .collect(),
            )
        })
        .collect()
}

/// Interface names implemented by `direct` and by the interfaces those interfaces implement.
fn reachable_interfaces(direct: &[Name], parents: &HashMap<String, Vec<String>>) -> Vec<String> {
    let mut result = Vec::new();
    let mut pending: Vec<String> = direct.iter().map(|name| name.to_string()).collect();
    while let Some(name) = pending.pop() {
        if result.contains(&name) {
            continue;
        }
        if let Some(parents) = parents.get(&name) {
            pending.extend(parents.iter().cloned());
        }
        result.push(name);
    }
    result
}

fn new_object<M: ObjectModel>() -> gql::ObjectType {
    let mut object = gql::ObjectType::new(M::NAME);
    if let Some(description) = M::description() {
        object = object.description(description);
    }
    object.is_type_of(|value| value.as_external::<M>().is_some())
}

/// Builder for one object type, received by [`Schema::object`].
pub struct ObjectBuilder<'a, Ctx: 'static, M: ObjectModel> {
    registry: &'a mut TypeRegistry,
    object: gql::ObjectType,
    checks: &'a mut Vec<FieldCheck>,
    pending_inherited: &'a mut Vec<PendingInherited>,
    auth: Option<Arc<AuthGuard<Ctx>>>,
    auth_error: Option<Arc<str>>,
    marker: PhantomData<fn() -> (Ctx, M)>,
}

impl<'a, Ctx: 'static, M: ObjectModel> ObjectBuilder<'a, Ctx, M> {
    fn new(
        existing: Option<gql::ObjectType>,
        registry: &'a mut TypeRegistry,
        checks: &'a mut Vec<FieldCheck>,
        pending_inherited: &'a mut Vec<PendingInherited>,
        name: &'static str,
    ) -> Self {
        let object = existing.unwrap_or_else(|| {
            let mut object = gql::ObjectType::new(name);
            if let Some(description) = M::description() {
                object = object.description(description);
            }
            object.is_type_of(|value| value.as_external::<M>().is_some())
        });
        Self {
            registry,
            object,
            checks,
            pending_inherited,
            auth: None,
            auth_error: None,
            marker: PhantomData,
        }
    }

    fn finish(self) -> (gql::ObjectType, Option<Arc<AuthGuard<Ctx>>>) {
        (self.object, self.auth)
    }

    /// Set the object's description, overriding the one from the derive.
    pub fn description(&mut self, description: impl Into<Arc<str>>) -> &mut Self {
        self.object.description = Some(description.into());
        self
    }

    /// Declare that this object implements an interface.
    pub fn implements(&mut self, interface: InterfaceRef) -> &mut Self {
        let name: Name = Arc::from(interface.name());
        if !self
            .object
            .interfaces
            .iter()
            .any(|existing| **existing == *name)
        {
            self.object.interfaces.push(name);
        }
        self
    }

    /// Require every field of this object to satisfy `predicate`.
    ///
    /// The check is applied to fields declared in every `object`/`object_fields` call for this
    /// model, including calls in other modules, and to fields added through
    /// [`ObjectBuilder::resolve_inherited`]. A denial follows normal null propagation.
    pub fn auth(&mut self, predicate: impl Fn(&Ctx) -> bool + Send + Sync + 'static) -> &mut Self {
        self.auth = Some(crate::field::sync_guard(predicate, self.auth_error.clone()));
        self
    }

    /// Like [`ObjectBuilder::auth`], but the predicate is asynchronous.
    pub fn auth_async(
        &mut self,
        predicate: impl for<'r> Fn(&'r Ctx) -> BoxFuture<'r, bool> + Send + Sync + 'static,
    ) -> &mut Self {
        self.auth = Some(crate::field::async_guard(
            predicate,
            self.auth_error.clone(),
        ));
        self
    }

    /// Set the message returned when the object-level authorization denies access.
    pub fn auth_error(&mut self, message: impl Into<Arc<str>>) -> &mut Self {
        self.auth_error = Some(message.into());
        if let Some(auth) = self.auth.take() {
            self.auth = Some(auth.with_message(self.auth_error.clone()));
        }
        self
    }

    /// Expose a model property as a field.
    ///
    /// The field name and type are inferred by `gurafi::expose!`.
    pub fn expose<T>(&mut self, expose: Expose<M, T>) -> FieldConfig<'_, Ctx>
    where
        T: OutputType + IntoValue + Send + 'static,
    {
        let (name, get) = expose.into_parts();
        T::register_output(self.registry);
        let field = gql::Field::new(name, T::output_type()).resolve(move |context| {
            let get = Arc::clone(&get);
            Box::pin(async move {
                let model = model_ref::<M>(
                    context.parent,
                    context.info.parent_type,
                    context.info.field_name,
                )?;
                (get)(model).into_value()
            })
        });
        self.push(field, T::is_optional(), std::any::type_name::<T>())
    }

    /// Add a synchronous field whose resolver cannot fail.
    pub fn field<T, A, F>(&mut self, name: &'static str, resolver: F) -> FieldConfig<'_, Ctx>
    where
        T: OutputType + IntoValue + Send + 'static,
        A: InputObject,
        F: Fn(&M, A, &Ctx) -> T + Send + Sync + 'static,
    {
        self.field_with_info::<T, A, _>(name, move |model: &M, args: A, ctx: &Ctx, _| {
            resolver(model, args, ctx)
        })
    }

    /// Add a synchronous field whose resolver also receives the [`gql::ResolveInfo`].
    ///
    /// `gql::ResolveInfo` currently exposes the field name, parent type, return type, response
    /// path and schema; it does not expose the selection AST, so a resolver cannot inspect the
    /// requested sub-fields through it yet.
    pub fn field_with_info<T, A, F>(
        &mut self,
        name: &'static str,
        resolver: F,
    ) -> FieldConfig<'_, Ctx>
    where
        T: OutputType + IntoValue + Send + 'static,
        A: InputObject,
        F: for<'r> Fn(&'r M, A, &'r Ctx, &'r gql::ResolveInfo<'r>) -> T + Send + Sync + 'static,
    {
        let field = object_field_with_info::<M, Ctx, T, A, F>(name, resolver, self.registry);
        self.push(field, T::is_optional(), std::any::type_name::<T>())
    }

    /// Add a synchronous field whose resolver returns a `Result`.
    pub fn field_result<T, A, F>(&mut self, name: &'static str, resolver: F) -> FieldConfig<'_, Ctx>
    where
        T: OutputType + IntoValue + Send + 'static,
        A: InputObject,
        F: Fn(&M, A, &Ctx) -> Result<T, GraphQLError> + Send + Sync + 'static,
    {
        self.field_result_with_info::<T, A, _>(name, move |model: &M, args: A, ctx: &Ctx, _| {
            resolver(model, args, ctx)
        })
    }

    /// Add a synchronous fallible field whose resolver also receives the [`gql::ResolveInfo`].
    pub fn field_result_with_info<T, A, F>(
        &mut self,
        name: &'static str,
        resolver: F,
    ) -> FieldConfig<'_, Ctx>
    where
        T: OutputType + IntoValue + Send + 'static,
        A: InputObject,
        F: for<'r> Fn(&'r M, A, &'r Ctx, &'r gql::ResolveInfo<'r>) -> Result<T, GraphQLError>
            + Send
            + Sync
            + 'static,
    {
        let field = object_field_result_with_info::<M, Ctx, T, A, F>(name, resolver, self.registry);
        self.push(field, T::is_optional(), std::any::type_name::<T>())
    }

    /// Add an asynchronous field whose resolver cannot fail.
    ///
    /// The resolver returns a boxed future, written as `Box::pin(async move { ... })`.
    pub fn field_async<T, A, F>(&mut self, name: &'static str, resolver: F) -> FieldConfig<'_, Ctx>
    where
        T: OutputType + IntoValue + Send + 'static,
        A: InputObject,
        F: for<'r> Fn(&'r M, A, &'r Ctx) -> BoxFuture<'r, T> + Send + Sync + 'static,
    {
        self.field_async_with_info::<T, A, _>(name, move |model: &M, args: A, ctx: &Ctx, _| {
            resolver(model, args, ctx)
        })
    }

    /// Add an asynchronous field whose resolver also receives the [`gql::ResolveInfo`].
    pub fn field_async_with_info<T, A, F>(
        &mut self,
        name: &'static str,
        resolver: F,
    ) -> FieldConfig<'_, Ctx>
    where
        T: OutputType + IntoValue + Send + 'static,
        A: InputObject,
        F: for<'r> Fn(&'r M, A, &'r Ctx, &'r gql::ResolveInfo<'r>) -> BoxFuture<'r, T>
            + Send
            + Sync
            + 'static,
    {
        let field = object_field_async_with_info::<M, Ctx, T, A, F>(name, resolver, self.registry);
        self.push(field, T::is_optional(), std::any::type_name::<T>())
    }

    /// Add an asynchronous field whose resolver returns a `Result`.
    pub fn field_result_async<T, A, F>(
        &mut self,
        name: &'static str,
        resolver: F,
    ) -> FieldConfig<'_, Ctx>
    where
        T: OutputType + IntoValue + Send + 'static,
        A: InputObject,
        F: for<'r> Fn(&'r M, A, &'r Ctx) -> BoxFuture<'r, Result<T, GraphQLError>>
            + Send
            + Sync
            + 'static,
    {
        self.field_result_async_with_info::<T, A, _>(
            name,
            move |model: &M, args: A, ctx: &Ctx, _| resolver(model, args, ctx),
        )
    }

    /// Add an asynchronous fallible field whose resolver also receives the [`gql::ResolveInfo`].
    pub fn field_result_async_with_info<T, A, F>(
        &mut self,
        name: &'static str,
        resolver: F,
    ) -> FieldConfig<'_, Ctx>
    where
        T: OutputType + IntoValue + Send + 'static,
        A: InputObject,
        F: for<'r> Fn(
                &'r M,
                A,
                &'r Ctx,
                &'r gql::ResolveInfo<'r>,
            ) -> BoxFuture<'r, Result<T, GraphQLError>>
            + Send
            + Sync
            + 'static,
    {
        let field =
            object_field_result_async_with_info::<M, Ctx, T, A, F>(name, resolver, self.registry);
        self.push(field, T::is_optional(), std::any::type_name::<T>())
    }

    /// Add a field whose type and arguments come from an implemented interface.
    ///
    /// Only the resolver is supplied. The schema build looks up the matching interface field
    /// (including interfaces that the object implements indirectly), copies its type and
    /// arguments, and checks that `A`'s arguments match its declaration. This is the small,
    /// resolver-only half of interface inheritance; it does not synthesize a model-backed trait.
    ///
    /// ```
    /// # use gurafi::{ID, NoArgs, Object, Schema};
    /// # struct Ctx;
    /// #[derive(Object)]
    /// struct Giraffe { name: String }
    ///
    /// let mut schema = Schema::<Ctx>::new();
    /// let node = schema.interface("Node", |i| {
    ///     i.field::<ID>("id");
    /// });
    /// schema.object::<Giraffe>(|o| {
    ///     o.implements(node);
    ///     o.resolve_inherited("id", |g: &Giraffe, _: NoArgs, _: &Ctx| ID::new(g.name.clone()));
    /// });
    /// ```
    pub fn resolve_inherited<T, A, F>(
        &mut self,
        name: &'static str,
        resolver: F,
    ) -> FieldConfig<'_, Ctx>
    where
        T: OutputType + IntoValue + Send + 'static,
        A: InputObject,
        F: Fn(&M, A, &Ctx) -> T + Send + Sync + 'static,
    {
        let expected_args = match A::fields() {
            Ok(fields) => fields
                .into_iter()
                .map(|argument| (argument.name, argument.ty))
                .collect(),
            Err(error) => {
                self.registry.push_error(GraphQLError::new(format!(
                    "Arguments for inherited field `{}.{name}` could not be built: {}",
                    M::NAME,
                    error.message()
                )));
                Vec::new()
            }
        };
        self.pending_inherited.push(PendingInherited {
            object: M::NAME,
            field: Arc::from(name),
            expected_args,
        });
        self.field(name, resolver)
    }

    fn push(
        &mut self,
        field: gql::Field,
        rust_nullable: bool,
        rust_type_name: &'static str,
    ) -> FieldConfig<'_, Ctx> {
        let name = field.name.clone();
        self.object.insert_field(field);
        FieldConfig::for_object_checked(
            self.registry,
            &mut self.object.fields,
            self.checks,
            M::NAME,
            name,
            rust_nullable,
            rust_type_name,
        )
    }
}

impl<Ctx: 'static, M: ObjectModel> std::fmt::Debug for ObjectBuilder<'_, Ctx, M> {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("ObjectBuilder")
            .field("object", &self.object)
            .finish_non_exhaustive()
    }
}

/// Builder for one interface type, received by [`Schema::interface`].
pub struct InterfaceBuilder<'a, Ctx: 'static> {
    registry: &'a mut TypeRegistry,
    interface: gql::InterfaceType,
    field_auth: IndexMap<Name, Arc<AuthGuard<Ctx>>>,
    auth: Option<Arc<AuthGuard<Ctx>>>,
    auth_error: Option<Arc<str>>,
    marker: PhantomData<fn() -> Ctx>,
}

impl<'a, Ctx: 'static> InterfaceBuilder<'a, Ctx> {
    fn new(
        existing: Option<gql::InterfaceType>,
        field_auth: IndexMap<Name, Arc<AuthGuard<Ctx>>>,
        registry: &'a mut TypeRegistry,
        name: &'static str,
    ) -> Self {
        Self {
            registry,
            interface: existing.unwrap_or_else(|| gql::InterfaceType::new(name)),
            field_auth,
            auth: None,
            auth_error: None,
            marker: PhantomData,
        }
    }

    fn finish(
        self,
    ) -> (
        gql::InterfaceType,
        Option<Arc<AuthGuard<Ctx>>>,
        IndexMap<Name, Arc<AuthGuard<Ctx>>>,
    ) {
        (self.interface, self.auth, self.field_auth)
    }

    /// Set the interface's description.
    pub fn description(&mut self, description: impl Into<Arc<str>>) -> &mut Self {
        self.interface.description = Some(description.into());
        self
    }

    /// Declare that this interface implements another interface.
    pub fn implements(&mut self, interface: InterfaceRef) -> &mut Self {
        let name: Name = Arc::from(interface.name());
        if !self
            .interface
            .interfaces
            .iter()
            .any(|existing| **existing == *name)
        {
            self.interface.interfaces.push(name);
        }
        self
    }

    /// Require every field of every implementing object to satisfy `predicate`.
    ///
    /// The rule is inherited by objects that implement this interface, directly or through
    /// another interface. It composes with object-level and field-level checks.
    pub fn auth(&mut self, predicate: impl Fn(&Ctx) -> bool + Send + Sync + 'static) -> &mut Self {
        self.auth = Some(crate::field::sync_guard(predicate, self.auth_error.clone()));
        self
    }

    /// Like [`InterfaceBuilder::auth`], but the predicate is asynchronous.
    pub fn auth_async(
        &mut self,
        predicate: impl for<'r> Fn(&'r Ctx) -> BoxFuture<'r, bool> + Send + Sync + 'static,
    ) -> &mut Self {
        self.auth = Some(crate::field::async_guard(
            predicate,
            self.auth_error.clone(),
        ));
        self
    }

    /// Set the message returned when the interface-level authorization denies access.
    pub fn auth_error(&mut self, message: impl Into<Arc<str>>) -> &mut Self {
        self.auth_error = Some(message.into());
        if let Some(auth) = self.auth.take() {
            self.auth = Some(auth.with_message(self.auth_error.clone()));
        }
        self
    }

    /// Add a field with no arguments.
    ///
    /// Interface fields have no resolver: the concrete objects supply one. The field's type must
    /// be a covariant match for every implementing object's field. Authorization declared on the
    /// field is inherited by those objects.
    pub fn field<T: OutputType>(&mut self, name: &'static str) -> InterfaceFieldConfig<'_, Ctx> {
        T::register_output(self.registry);
        self.interface
            .insert_field(gql::Field::new(name, T::output_type()));
        InterfaceFieldConfig::new(FieldConfig::for_interface(
            self.registry,
            &mut self.interface.fields,
            &mut self.field_auth,
            Arc::from(name),
            T::is_optional(),
            std::any::type_name::<T>(),
        ))
    }

    /// Add a field with arguments.
    pub fn field_with<T, A>(&mut self, name: &'static str) -> InterfaceFieldConfig<'_, Ctx>
    where
        T: OutputType,
        A: InputObject,
    {
        T::register_output(self.registry);
        A::register_args(self.registry);
        let field = add_arguments::<A>(gql::Field::new(name, T::output_type()), self.registry);
        self.interface.insert_field(field);
        InterfaceFieldConfig::new(FieldConfig::for_interface(
            self.registry,
            &mut self.interface.fields,
            &mut self.field_auth,
            Arc::from(name),
            T::is_optional(),
            std::any::type_name::<T>(),
        ))
    }

    /// Install a custom abstract type resolver.
    ///
    /// By default the executor uses each object's `is_type_of` check.
    pub fn resolve_type(
        &mut self,
        resolver: impl Fn(&gql::Value, &gql::ExecutionContext) -> Result<gql::Name, GraphQLError>
        + Send
        + Sync
        + 'static,
    ) -> &mut Self {
        self.interface.resolve_type = Some(Arc::new(resolver));
        self
    }
}

impl<Ctx> std::fmt::Debug for InterfaceBuilder<'_, Ctx> {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("InterfaceBuilder")
            .field("interface", &self.interface)
            .finish_non_exhaustive()
    }
}

/// Builder for one union type, received by [`Schema::union`].
pub struct UnionBuilder<Ctx: 'static> {
    union: gql::UnionType,
    marker: PhantomData<fn() -> Ctx>,
}

impl<Ctx: 'static> UnionBuilder<Ctx> {
    fn new(existing: Option<gql::UnionType>, name: &'static str) -> Self {
        Self {
            union: existing.unwrap_or_else(|| gql::UnionType::new(name)),
            marker: PhantomData,
        }
    }

    fn finish(self) -> gql::UnionType {
        self.union
    }

    /// Set the union's description.
    pub fn description(&mut self, description: impl Into<Arc<str>>) -> &mut Self {
        self.union.description = Some(description.into());
        self
    }

    /// Add an object type as a union member.
    pub fn member<M: ObjectModel>(&mut self) -> &mut Self {
        let name: Name = Arc::from(M::NAME);
        if !self.union.types.iter().any(|existing| **existing == *name) {
            self.union.types.push(name);
        }
        self
    }

    /// Install a custom abstract type resolver.
    ///
    /// By default the executor uses each object's `is_type_of` check.
    pub fn resolve_type(
        &mut self,
        resolver: impl Fn(&gql::Value, &gql::ExecutionContext) -> Result<gql::Name, GraphQLError>
        + Send
        + Sync
        + 'static,
    ) -> &mut Self {
        self.union.resolve_type = Some(Arc::new(resolver));
        self
    }
}

impl<Ctx> std::fmt::Debug for UnionBuilder<Ctx> {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("UnionBuilder")
            .field("union", &self.union)
            .finish_non_exhaustive()
    }
}

/// Builder for query and mutation root fields.
pub struct RootBuilder<'a, Ctx: 'static> {
    registry: &'a mut TypeRegistry,
    fields: &'a mut Vec<gql::Field>,
    checks: &'a mut Vec<FieldCheck>,
    owner: String,
    marker: PhantomData<fn() -> Ctx>,
}

impl<'a, Ctx: 'static> RootBuilder<'a, Ctx> {
    fn new(
        registry: &'a mut TypeRegistry,
        fields: &'a mut Vec<gql::Field>,
        checks: &'a mut Vec<FieldCheck>,
        owner: String,
    ) -> Self {
        Self {
            registry,
            fields,
            checks,
            owner,
            marker: PhantomData,
        }
    }

    /// Add a synchronous root field whose resolver cannot fail.
    pub fn field<T, A, F>(&mut self, name: &'static str, resolver: F) -> FieldConfig<'_, Ctx>
    where
        T: OutputType + IntoValue + Send + 'static,
        A: InputObject,
        F: Fn(A, &Ctx) -> T + Send + Sync + 'static,
    {
        self.field_with_info::<T, A, _>(name, move |args: A, ctx: &Ctx, _| resolver(args, ctx))
    }

    /// Add a synchronous root field whose resolver also receives the [`gql::ResolveInfo`].
    pub fn field_with_info<T, A, F>(
        &mut self,
        name: &'static str,
        resolver: F,
    ) -> FieldConfig<'_, Ctx>
    where
        T: OutputType + IntoValue + Send + 'static,
        A: InputObject,
        F: for<'r> Fn(A, &'r Ctx, &'r gql::ResolveInfo<'r>) -> T + Send + Sync + 'static,
    {
        let field = root_field_with_info::<Ctx, T, A, F>(name, resolver, self.registry);
        self.push(field, T::is_optional(), std::any::type_name::<T>())
    }

    /// Add a synchronous root field whose resolver returns a `Result`.
    pub fn field_result<T, A, F>(&mut self, name: &'static str, resolver: F) -> FieldConfig<'_, Ctx>
    where
        T: OutputType + IntoValue + Send + 'static,
        A: InputObject,
        F: Fn(A, &Ctx) -> Result<T, GraphQLError> + Send + Sync + 'static,
    {
        self.field_result_with_info::<T, A, _>(name, move |args: A, ctx: &Ctx, _| {
            resolver(args, ctx)
        })
    }

    /// Add a synchronous fallible root field whose resolver also receives the [`gql::ResolveInfo`].
    pub fn field_result_with_info<T, A, F>(
        &mut self,
        name: &'static str,
        resolver: F,
    ) -> FieldConfig<'_, Ctx>
    where
        T: OutputType + IntoValue + Send + 'static,
        A: InputObject,
        F: for<'r> Fn(A, &'r Ctx, &'r gql::ResolveInfo<'r>) -> Result<T, GraphQLError>
            + Send
            + Sync
            + 'static,
    {
        let field = root_field_result_with_info::<Ctx, T, A, F>(name, resolver, self.registry);
        self.push(field, T::is_optional(), std::any::type_name::<T>())
    }

    /// Add an asynchronous root field whose resolver cannot fail.
    pub fn field_async<T, A, F>(&mut self, name: &'static str, resolver: F) -> FieldConfig<'_, Ctx>
    where
        T: OutputType + IntoValue + Send + 'static,
        A: InputObject,
        F: for<'r> Fn(A, &'r Ctx) -> BoxFuture<'r, T> + Send + Sync + 'static,
    {
        self.field_async_with_info::<T, A, _>(name, move |args: A, ctx: &Ctx, _| {
            resolver(args, ctx)
        })
    }

    /// Add an asynchronous root field whose resolver also receives the [`gql::ResolveInfo`].
    pub fn field_async_with_info<T, A, F>(
        &mut self,
        name: &'static str,
        resolver: F,
    ) -> FieldConfig<'_, Ctx>
    where
        T: OutputType + IntoValue + Send + 'static,
        A: InputObject,
        F: for<'r> Fn(A, &'r Ctx, &'r gql::ResolveInfo<'r>) -> BoxFuture<'r, T>
            + Send
            + Sync
            + 'static,
    {
        let field = root_field_async_with_info::<Ctx, T, A, F>(name, resolver, self.registry);
        self.push(field, T::is_optional(), std::any::type_name::<T>())
    }

    /// Add an asynchronous root field whose resolver returns a `Result`.
    pub fn field_result_async<T, A, F>(
        &mut self,
        name: &'static str,
        resolver: F,
    ) -> FieldConfig<'_, Ctx>
    where
        T: OutputType + IntoValue + Send + 'static,
        A: InputObject,
        F: for<'r> Fn(A, &'r Ctx) -> BoxFuture<'r, Result<T, GraphQLError>> + Send + Sync + 'static,
    {
        self.field_result_async_with_info::<T, A, _>(name, move |args: A, ctx: &Ctx, _| {
            resolver(args, ctx)
        })
    }

    /// Add an asynchronous fallible root field whose resolver also receives the
    /// [`gql::ResolveInfo`].
    pub fn field_result_async_with_info<T, A, F>(
        &mut self,
        name: &'static str,
        resolver: F,
    ) -> FieldConfig<'_, Ctx>
    where
        T: OutputType + IntoValue + Send + 'static,
        A: InputObject,
        F: for<'r> Fn(
                A,
                &'r Ctx,
                &'r gql::ResolveInfo<'r>,
            ) -> BoxFuture<'r, Result<T, GraphQLError>>
            + Send
            + Sync
            + 'static,
    {
        let field =
            root_field_result_async_with_info::<Ctx, T, A, F>(name, resolver, self.registry);
        self.push(field, T::is_optional(), std::any::type_name::<T>())
    }

    fn push(
        &mut self,
        field: gql::Field,
        rust_nullable: bool,
        rust_type_name: &'static str,
    ) -> FieldConfig<'_, Ctx> {
        let name = field.name.clone();
        self.fields.push(field);
        FieldConfig::for_fields_checked(
            self.registry,
            self.fields,
            self.checks,
            self.owner.clone(),
            name,
            rust_nullable,
            rust_type_name,
        )
    }
}

impl<Ctx> std::fmt::Debug for RootBuilder<'_, Ctx> {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("RootBuilder")
            .field("fields", &self.fields.len())
            .finish_non_exhaustive()
    }
}

/// Builder for subscription root fields.
///
/// A subscription field's `subscribe` resolver returns a stream of events. The executor treats
/// each event as the root value of the subscription field and runs the field's `resolve`
/// resolver to extract the value of the field. `gurafi` uses that to keep the simple API:
/// [`SubscriptionBuilder::field`] takes a stream of the field's values and arranges the root
/// object and the extracting resolver for you.
///
/// Use [`SubscriptionBuilder::field_map`] when the event type differs from the field's type.
pub struct SubscriptionBuilder<'a, Ctx: 'static> {
    registry: &'a mut TypeRegistry,
    fields: &'a mut Vec<gql::Field>,
    checks: &'a mut Vec<FieldCheck>,
    owner: String,
    marker: PhantomData<fn() -> Ctx>,
}

impl<'a, Ctx: 'static> SubscriptionBuilder<'a, Ctx> {
    fn new(
        registry: &'a mut TypeRegistry,
        fields: &'a mut Vec<gql::Field>,
        checks: &'a mut Vec<FieldCheck>,
        owner: String,
    ) -> Self {
        Self {
            registry,
            fields,
            checks,
            owner,
            marker: PhantomData,
        }
    }

    /// Add a subscription field.
    ///
    /// `subscribe` is called once per subscription to create the event stream. Each item is the
    /// field's own value (or an error), not a root object: `gurafi` wraps it and extracts it
    /// again when the executor resolves the field, so the common case stays simple. The stream
    /// must be `Send + 'static`, so anything it borrows from the context must be cloned first
    /// (channels and `Arc`s are the usual choice).
    pub fn field<A, T, F, S>(&mut self, name: &'static str, subscribe: F) -> FieldConfig<'_, Ctx>
    where
        A: InputObject,
        T: OutputType + IntoValue + Send + 'static,
        S: Stream<Item = Result<T, GraphQLError>> + Send + 'static,
        F: Fn(A, &Ctx) -> S + Send + Sync + 'static,
    {
        self.field_with_info::<A, T, _, S>(name, move |args: A, ctx: &Ctx, _| subscribe(args, ctx))
    }

    /// Add a subscription field whose `subscribe` also receives the [`gql::ResolveInfo`].
    pub fn field_with_info<A, T, F, S>(
        &mut self,
        name: &'static str,
        subscribe: F,
    ) -> FieldConfig<'_, Ctx>
    where
        A: InputObject,
        T: OutputType + IntoValue + Send + 'static,
        S: Stream<Item = Result<T, GraphQLError>> + Send + 'static,
        F: for<'r> Fn(A, &'r Ctx, &'r gql::ResolveInfo<'r>) -> S + Send + Sync + 'static,
    {
        let field = subscription_field::<Ctx, A, T, F, S>(name, subscribe, self.registry);
        self.push(field, T::is_optional(), std::any::type_name::<T>())
    }

    /// Add a subscription whose events are mapped to the field value by a resolver.
    ///
    /// This is the Pothos-style form: the stream produces an event `E`, and `resolve` turns each
    /// event into the field's value `T`. Use it when the event carries more than the field's
    /// own value (an envelope with metadata, or several correlated updates).
    ///
    /// ```ignore
    /// s.field_map(
    ///     "giraffeBorn",
    ///     |_: NoArgs, _: &Ctx| events(),
    ///     |event: &BornEvent, _: NoArgs, _: &Ctx| Ok(event.giraffe.clone()),
    /// );
    /// ```
    pub fn field_map<A, E, T, F, S, R>(
        &mut self,
        name: &'static str,
        subscribe: F,
        resolve: R,
    ) -> FieldConfig<'_, Ctx>
    where
        A: InputObject,
        E: Send + Sync + 'static,
        T: OutputType + IntoValue + Send + 'static,
        S: Stream<Item = Result<E, GraphQLError>> + Send + 'static,
        F: Fn(A, &Ctx) -> S + Send + Sync + 'static,
        R: for<'r> Fn(&'r E, A, &'r Ctx) -> Result<T, GraphQLError> + Send + Sync + 'static,
    {
        T::register_output(self.registry);
        A::register_args(self.registry);
        let subscribe = Arc::new(subscribe);
        let resolve = Arc::new(resolve);
        let field = add_arguments::<A>(gql::Field::new(name, T::output_type()), self.registry)
            .resolve(move |context| {
                let resolve = Arc::clone(&resolve);
                Box::pin(async move {
                    let event = context.parent.as_external::<E>().ok_or_else(|| {
                        GraphQLError::new(format!(
                            "Subscription event for \"{}\" had an unexpected type.",
                            context.info.field_name
                        ))
                    })?;
                    let args = coerce_args::<A>(
                        context.args,
                        context.info.parent_type,
                        context.info.field_name,
                    )?;
                    let ctx = context.context.require_data::<Ctx>()?;
                    match resolve(event, args, ctx) {
                        Ok(value) => value.into_value(),
                        Err(error) => Err(error),
                    }
                })
            })
            .subscribe(move |context| {
                let subscribe = Arc::clone(&subscribe);
                Box::pin(async move {
                    let args = coerce_args::<A>(
                        context.args,
                        context.info.parent_type,
                        context.info.field_name,
                    )?;
                    let ctx = context.context.require_data::<Ctx>()?;
                    let stream = subscribe(args, ctx);
                    let mapped = stream.map(|item| match item {
                        Ok(event) => Ok(gql::Value::external(event)),
                        Err(error) => Err(error),
                    });
                    Ok(Box::pin(mapped) as gql::SubscriptionStream)
                })
            });
        self.push(field, T::is_optional(), std::any::type_name::<T>())
    }

    fn push(
        &mut self,
        field: gql::Field,
        rust_nullable: bool,
        rust_type_name: &'static str,
    ) -> FieldConfig<'_, Ctx> {
        let name = field.name.clone();
        self.fields.push(field);
        FieldConfig::for_fields_checked(
            self.registry,
            self.fields,
            self.checks,
            self.owner.clone(),
            name,
            rust_nullable,
            rust_type_name,
        )
    }
}

impl<Ctx> std::fmt::Debug for SubscriptionBuilder<'_, Ctx> {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("SubscriptionBuilder")
            .field("fields", &self.fields.len())
            .finish_non_exhaustive()
    }
}

fn add_arguments<A: InputObject>(mut field: gql::Field, registry: &mut TypeRegistry) -> gql::Field {
    match A::fields() {
        Ok(arguments) => {
            for argument in arguments {
                field = field.arg(argument);
            }
        }
        Err(error) => registry.push_error(GraphQLError::new(format!(
            "Arguments for field `{}` could not be built: {}",
            field.name,
            error.message()
        ))),
    }
    field
}

fn object_field<M, Ctx, T, A, F>(
    name: &'static str,
    resolver: F,
    registry: &mut TypeRegistry,
) -> gql::Field
where
    M: ObjectModel,
    Ctx: 'static,
    T: OutputType + IntoValue + Send + 'static,
    A: InputObject,
    F: Fn(&M, A, &Ctx) -> T + Send + Sync + 'static,
{
    object_field_with_info::<M, Ctx, T, A, _>(
        name,
        move |model: &M, args: A, ctx: &Ctx, _| resolver(model, args, ctx),
        registry,
    )
}

fn object_field_with_info<M, Ctx, T, A, F>(
    name: &'static str,
    resolver: F,
    registry: &mut TypeRegistry,
) -> gql::Field
where
    M: ObjectModel,
    Ctx: 'static,
    T: OutputType + IntoValue + Send + 'static,
    A: InputObject,
    F: for<'r> Fn(&'r M, A, &'r Ctx, &'r gql::ResolveInfo<'r>) -> T + Send + Sync + 'static,
{
    T::register_output(registry);
    A::register_args(registry);
    let resolver = Arc::new(resolver);
    add_arguments::<A>(gql::Field::new(name, T::output_type()), registry).resolve(move |context| {
        let resolver = Arc::clone(&resolver);
        Box::pin(async move {
            let model = model_ref::<M>(
                context.parent,
                context.info.parent_type,
                context.info.field_name,
            )?;
            let args = coerce_args::<A>(
                context.args,
                context.info.parent_type,
                context.info.field_name,
            )?;
            let ctx = context.context.require_data::<Ctx>()?;
            (resolver)(model, args, ctx, context.info).into_value()
        })
    })
}

fn object_field_result_with_info<M, Ctx, T, A, F>(
    name: &'static str,
    resolver: F,
    registry: &mut TypeRegistry,
) -> gql::Field
where
    M: ObjectModel,
    Ctx: 'static,
    T: OutputType + IntoValue + Send + 'static,
    A: InputObject,
    F: for<'r> Fn(&'r M, A, &'r Ctx, &'r gql::ResolveInfo<'r>) -> Result<T, GraphQLError>
        + Send
        + Sync
        + 'static,
{
    T::register_output(registry);
    A::register_args(registry);
    let resolver = Arc::new(resolver);
    add_arguments::<A>(gql::Field::new(name, T::output_type()), registry).resolve(move |context| {
        let resolver = Arc::clone(&resolver);
        Box::pin(async move {
            let model = model_ref::<M>(
                context.parent,
                context.info.parent_type,
                context.info.field_name,
            )?;
            let args = coerce_args::<A>(
                context.args,
                context.info.parent_type,
                context.info.field_name,
            )?;
            let ctx = context.context.require_data::<Ctx>()?;
            match (resolver)(model, args, ctx, context.info) {
                Ok(value) => value.into_value(),
                Err(error) => Err(error),
            }
        })
    })
}

fn object_field_async_with_info<M, Ctx, T, A, F>(
    name: &'static str,
    resolver: F,
    registry: &mut TypeRegistry,
) -> gql::Field
where
    M: ObjectModel,
    Ctx: 'static,
    T: OutputType + IntoValue + Send + 'static,
    A: InputObject,
    F: for<'r> Fn(&'r M, A, &'r Ctx, &'r gql::ResolveInfo<'r>) -> BoxFuture<'r, T>
        + Send
        + Sync
        + 'static,
{
    T::register_output(registry);
    A::register_args(registry);
    let resolver = Arc::new(resolver);
    add_arguments::<A>(gql::Field::new(name, T::output_type()), registry).resolve(move |context| {
        let resolver = Arc::clone(&resolver);
        Box::pin(async move {
            let model = model_ref::<M>(
                context.parent,
                context.info.parent_type,
                context.info.field_name,
            )?;
            let args = coerce_args::<A>(
                context.args,
                context.info.parent_type,
                context.info.field_name,
            )?;
            let ctx = context.context.require_data::<Ctx>()?;
            (resolver)(model, args, ctx, context.info)
                .await
                .into_value()
        })
    })
}

fn object_field_result_async_with_info<M, Ctx, T, A, F>(
    name: &'static str,
    resolver: F,
    registry: &mut TypeRegistry,
) -> gql::Field
where
    M: ObjectModel,
    Ctx: 'static,
    T: OutputType + IntoValue + Send + 'static,
    A: InputObject,
    F: for<'r> Fn(
            &'r M,
            A,
            &'r Ctx,
            &'r gql::ResolveInfo<'r>,
        ) -> BoxFuture<'r, Result<T, GraphQLError>>
        + Send
        + Sync
        + 'static,
{
    T::register_output(registry);
    A::register_args(registry);
    let resolver = Arc::new(resolver);
    add_arguments::<A>(gql::Field::new(name, T::output_type()), registry).resolve(move |context| {
        let resolver = Arc::clone(&resolver);
        Box::pin(async move {
            let model = model_ref::<M>(
                context.parent,
                context.info.parent_type,
                context.info.field_name,
            )?;
            let args = coerce_args::<A>(
                context.args,
                context.info.parent_type,
                context.info.field_name,
            )?;
            let ctx = context.context.require_data::<Ctx>()?;
            match (resolver)(model, args, ctx, context.info).await {
                Ok(value) => value.into_value(),
                Err(error) => Err(error),
            }
        })
    })
}

fn root_field_with_info<Ctx, T, A, F>(
    name: &'static str,
    resolver: F,
    registry: &mut TypeRegistry,
) -> gql::Field
where
    Ctx: 'static,
    T: OutputType + IntoValue + Send + 'static,
    A: InputObject,
    F: for<'r> Fn(A, &'r Ctx, &'r gql::ResolveInfo<'r>) -> T + Send + Sync + 'static,
{
    T::register_output(registry);
    A::register_args(registry);
    let resolver = Arc::new(resolver);
    add_arguments::<A>(gql::Field::new(name, T::output_type()), registry).resolve(move |context| {
        let resolver = Arc::clone(&resolver);
        Box::pin(async move {
            let args = coerce_args::<A>(
                context.args,
                context.info.parent_type,
                context.info.field_name,
            )?;
            let ctx = context.context.require_data::<Ctx>()?;
            (resolver)(args, ctx, context.info).into_value()
        })
    })
}

fn root_field_result_with_info<Ctx, T, A, F>(
    name: &'static str,
    resolver: F,
    registry: &mut TypeRegistry,
) -> gql::Field
where
    Ctx: 'static,
    T: OutputType + IntoValue + Send + 'static,
    A: InputObject,
    F: for<'r> Fn(A, &'r Ctx, &'r gql::ResolveInfo<'r>) -> Result<T, GraphQLError>
        + Send
        + Sync
        + 'static,
{
    T::register_output(registry);
    A::register_args(registry);
    let resolver = Arc::new(resolver);
    add_arguments::<A>(gql::Field::new(name, T::output_type()), registry).resolve(move |context| {
        let resolver = Arc::clone(&resolver);
        Box::pin(async move {
            let args = coerce_args::<A>(
                context.args,
                context.info.parent_type,
                context.info.field_name,
            )?;
            let ctx = context.context.require_data::<Ctx>()?;
            match (resolver)(args, ctx, context.info) {
                Ok(value) => value.into_value(),
                Err(error) => Err(error),
            }
        })
    })
}

fn root_field_async_with_info<Ctx, T, A, F>(
    name: &'static str,
    resolver: F,
    registry: &mut TypeRegistry,
) -> gql::Field
where
    Ctx: 'static,
    T: OutputType + IntoValue + Send + 'static,
    A: InputObject,
    F: for<'r> Fn(A, &'r Ctx, &'r gql::ResolveInfo<'r>) -> BoxFuture<'r, T> + Send + Sync + 'static,
{
    T::register_output(registry);
    A::register_args(registry);
    let resolver = Arc::new(resolver);
    add_arguments::<A>(gql::Field::new(name, T::output_type()), registry).resolve(move |context| {
        let resolver = Arc::clone(&resolver);
        Box::pin(async move {
            let args = coerce_args::<A>(
                context.args,
                context.info.parent_type,
                context.info.field_name,
            )?;
            let ctx = context.context.require_data::<Ctx>()?;
            (resolver)(args, ctx, context.info).await.into_value()
        })
    })
}

fn root_field_result_async_with_info<Ctx, T, A, F>(
    name: &'static str,
    resolver: F,
    registry: &mut TypeRegistry,
) -> gql::Field
where
    Ctx: 'static,
    T: OutputType + IntoValue + Send + 'static,
    A: InputObject,
    F: for<'r> Fn(A, &'r Ctx, &'r gql::ResolveInfo<'r>) -> BoxFuture<'r, Result<T, GraphQLError>>
        + Send
        + Sync
        + 'static,
{
    T::register_output(registry);
    A::register_args(registry);
    let resolver = Arc::new(resolver);
    add_arguments::<A>(gql::Field::new(name, T::output_type()), registry).resolve(move |context| {
        let resolver = Arc::clone(&resolver);
        Box::pin(async move {
            let args = coerce_args::<A>(
                context.args,
                context.info.parent_type,
                context.info.field_name,
            )?;
            let ctx = context.context.require_data::<Ctx>()?;
            match (resolver)(args, ctx, context.info).await {
                Ok(value) => value.into_value(),
                Err(error) => Err(error),
            }
        })
    })
}

fn subscription_field<Ctx, A, T, F, S>(
    name: &'static str,
    subscribe: F,
    registry: &mut TypeRegistry,
) -> gql::Field
where
    Ctx: 'static,
    A: InputObject,
    T: OutputType + IntoValue + Send + 'static,
    S: Stream<Item = Result<T, GraphQLError>> + Send + 'static,
    F: for<'r> Fn(A, &'r Ctx, &'r gql::ResolveInfo<'r>) -> S + Send + Sync + 'static,
{
    T::register_output(registry);
    A::register_args(registry);
    let subscribe = Arc::new(subscribe);
    add_arguments::<A>(gql::Field::new(name, T::output_type()), registry)
        .resolve(move |context| {
            Box::pin(async move {
                context
                    .parent
                    .as_object()
                    .and_then(|object| object.get(context.info.field_name))
                    .cloned()
                    .ok_or_else(|| {
                        GraphQLError::new(format!(
                            "Subscription event for \"{}\" did not contain the field value.",
                            context.info.field_name
                        ))
                    })
            })
        })
        .subscribe(move |context| {
            let subscribe = Arc::clone(&subscribe);
            Box::pin(async move {
                let args = coerce_args::<A>(
                    context.args,
                    context.info.parent_type,
                    context.info.field_name,
                )?;
                let ctx = context.context.require_data::<Ctx>()?;
                let stream = subscribe(args, ctx, context.info);
                let field_name: Name = Arc::from(context.info.field_name);
                let mapped = stream.map(move |item| match item {
                    Ok(value) => {
                        let value = value.into_value()?;
                        let mut object = gql::Object::new();
                        object.insert(field_name.clone(), value);
                        Ok(gql::Value::Object(Arc::new(object)))
                    }
                    Err(error) => Err(error),
                });
                Ok(Box::pin(mapped) as gql::SubscriptionStream)
            })
        })
}

/// Whether a value of `actual` can be completed as `expected`.
///
/// Non-null may narrow a nullable declaration and list items may narrow the same way. A concrete
/// object may stand in for an abstract type that lists it as a possible type.
fn is_output_subtype(schema: &gql::Schema, actual: &TypeRef, expected: &TypeRef) -> bool {
    match (actual, expected) {
        (TypeRef::NonNull(actual), TypeRef::NonNull(expected)) => {
            is_output_subtype(schema, actual, expected)
        }
        (TypeRef::NonNull(actual), expected) => is_output_subtype(schema, actual, expected),
        (_, TypeRef::NonNull(_)) => false,
        (TypeRef::List(actual), TypeRef::List(expected)) => {
            is_output_subtype(schema, actual, expected)
        }
        (TypeRef::Named(actual), TypeRef::Named(expected)) => {
            actual == expected
                || (schema
                    .get_type(expected)
                    .is_some_and(|ty| matches!(ty, gql::Type::Interface(_) | gql::Type::Union(_)))
                    && schema.is_possible_type(expected, actual))
        }
        _ => false,
    }
}

/// Verify that every deferred field declaration is truthful.
fn validate_checks(schema: &gql::Schema, checks: &[FieldCheck], errors: &mut Vec<GraphQLError>) {
    for check in checks {
        match check {
            FieldCheck::Abstract {
                owner,
                field,
                original,
                abstract_name,
            } => {
                let Some(abstract_type) = schema.get_type(abstract_name) else {
                    errors.push(GraphQLError::new(format!(
                        "Field `{owner}.{field}` declares unknown abstract type `{abstract_name}`."
                    )));
                    continue;
                };
                if !matches!(abstract_type, gql::Type::Interface(_) | gql::Type::Union(_)) {
                    errors.push(GraphQLError::new(format!(
                        "Field `{owner}.{field}` declares `{abstract_name}` as abstract, but it \
                         is a {}.",
                        abstract_type.kind()
                    )));
                    continue;
                }
                let original = original.named_type();
                if original == abstract_name {
                    continue;
                }
                if !schema.is_possible_type(abstract_name, original) {
                    errors.push(GraphQLError::new(format!(
                        "Field `{owner}.{field}` declares abstract type `{abstract_name}`, but \
                         its Rust type `{original}` is not a possible type of `{abstract_name}`."
                    )));
                }
            }
            FieldCheck::TypeOverride {
                owner,
                field,
                original,
                replacement,
            } => {
                let original_name = original.named_type();
                let replacement_name = replacement.named_type();
                if original_name == replacement_name {
                    continue;
                }
                let Some(original_type) = schema.get_type(original_name) else {
                    errors.push(GraphQLError::new(format!(
                        "Field `{owner}.{field}` overrides a type that is not in the schema \
                         (`{original_name}`). Register it before calling `build()`."
                    )));
                    continue;
                };
                let Some(replacement_type) = schema.get_type(replacement_name) else {
                    errors.push(GraphQLError::new(format!(
                        "Field `{owner}.{field}` overrides its type to `{replacement_name}`, but \
                         that type is not in the schema. Register it before calling `build()`."
                    )));
                    continue;
                };
                let compatible = match (original_type, replacement_type) {
                    (gql::Type::Scalar(_), gql::Type::Scalar(_))
                    | (gql::Type::Enum(_), gql::Type::Enum(_)) => true,
                    (gql::Type::Interface(_) | gql::Type::Union(_), _) => {
                        schema.is_possible_type(original_name, replacement_name)
                    }
                    (_, gql::Type::Interface(_) | gql::Type::Union(_)) => {
                        schema.is_possible_type(replacement_name, original_name)
                    }
                    _ => false,
                };
                if !compatible {
                    errors.push(GraphQLError::new(format!(
                        "Field `{owner}.{field}` overrides its type from `{original}` to \
                         `{replacement}`, but `{original_name}` and `{replacement_name}` are not \
                         compatible. `as_type`/`returns` can only change between scalars, between \
                         enums, or to and from an abstract type whose possible types include the \
                         Rust type."
                    )));
                }
            }
            FieldCheck::NonNullOnOption {
                owner,
                field,
                rust_type,
            } => errors.push(GraphQLError::new(format!(
                "Field `{owner}.{field}` calls `non_null()`, but its resolver returns \
                 `{rust_type}`, which is optional. Remove the call or make the Rust type \
                 non-optional."
            ))),
            FieldCheck::NullableItemsOnNonList { owner, field, ty } => {
                errors.push(GraphQLError::new(format!(
                    "Field `{owner}.{field}` calls `nullable_items()`, but its type `{ty}` is not \
                     a list. Use `nullable()` to make the field itself nullable."
                )));
            }
            FieldCheck::InheritedType {
                owner,
                field,
                actual,
                expected,
            } => {
                if !is_output_subtype(schema, actual, expected) {
                    errors.push(GraphQLError::new(format!(
                        "Field `{owner}.{field}` inherits `{expected}` from its interface, but its \
                         resolver returns `{actual}`. Both the Rust type and the resolver's return \
                         type must match the interface field."
                    )));
                }
            }
        }
    }
}

/// A type reference for tests and tooling: the GraphQL type a Rust type maps to.
#[must_use]
pub fn type_ref<T: OutputType>() -> TypeRef {
    T::output_type()
}
