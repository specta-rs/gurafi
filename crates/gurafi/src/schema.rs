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

use std::{
    any::TypeId,
    collections::{HashMap, HashSet},
    marker::PhantomData,
    sync::Arc,
};

use futures::{Stream, StreamExt};
use gql::{GraphQLError, Name, TypeRef};
use indexmap::IndexMap;

use crate::{
    error::SchemaError,
    error_union::{ErrorUnionModel, ResultSuccess, result_union_type_ref},
    expose::Expose,
    field::{
        AuthGuard, FieldCheck, FieldConfig, InterfaceFieldConfig, RawFieldArgs, ValidationSlot,
        apply_auth_guard, read_raw_field,
    },
    plugin::{Plugin, PluginTargets, apply_plugins},
    registry::TypeRegistry,
    traits::{
        EnumModel, InputObject, IntoValue, ObjectModel, OutputType, ScalarValue, coerce_args,
        coerce_args_without_validation, model_ref,
    },
};

/// A boxed future returned by an asynchronous resolver.
///
/// Asynchronous resolvers accept a closure returning this type, so a resolver is written as
/// `Box::pin(async move { ... })`. The box is allocated once per field resolution; `gql`
/// already boxes every resolver future, so this is the same allocation the executor needs
/// anyway.
pub type BoxFuture<'a, T> = futures::future::BoxFuture<'a, T>;

/// A reference to an object type whose GraphQL name may be computed at runtime.
///
/// An object reference ties a Rust model to a GraphQL type name. The model still drives
/// `is_type_of` and the resolvers a builder receives; the name is just data. Refs are created by
/// [`Schema::object_ref`] (or [`Schema::object`] for the derived name) and consumed by
/// [`Schema::implement`].
///
/// The model parameter means a reference cannot be accidentally used with a different model's
/// resolvers. Two references compare equal when they name the same GraphQL type.
pub struct ObjectRef<M: ObjectModel> {
    name: Name,
    marker: PhantomData<fn() -> M>,
}

impl<M: ObjectModel> ObjectRef<M> {
    fn new(name: Name) -> Self {
        Self {
            name,
            marker: PhantomData,
        }
    }

    /// The object's GraphQL name.
    #[must_use]
    pub fn name(&self) -> &str {
        &self.name
    }

    /// A reference to this object with no list or nullability wrappers.
    #[must_use]
    pub fn type_ref(&self) -> TypeRef {
        TypeRef::named(Arc::clone(&self.name))
    }

    /// Whether this object is a possible type of `abstract_type` in `schema`.
    #[must_use]
    pub fn is_possible_type(&self, schema: &gql::Schema, abstract_type: &str) -> bool {
        schema.is_possible_type(abstract_type, self.name())
    }

    /// The model's compile-time GraphQL name.
    ///
    /// The object may be registered under a different runtime name; this is the name
    /// `#[derive(Object)]` generated.
    #[must_use]
    pub fn model_name(&self) -> &'static str {
        M::NAME
    }
}

impl<M: ObjectModel> Clone for ObjectRef<M> {
    fn clone(&self) -> Self {
        Self::new(Arc::clone(&self.name))
    }
}

impl<M: ObjectModel> std::fmt::Debug for ObjectRef<M> {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_tuple("ObjectRef").field(&self.name).finish()
    }
}

impl<M: ObjectModel> PartialEq for ObjectRef<M> {
    fn eq(&self, other: &Self) -> bool {
        self.name == other.name
    }
}

impl<M: ObjectModel> Eq for ObjectRef<M> {}

impl<M: ObjectModel> std::hash::Hash for ObjectRef<M> {
    fn hash<H: std::hash::Hasher>(&self, state: &mut H) {
        self.name.hash(state);
    }
}

impl<M: ObjectModel> From<&ObjectRef<M>> for TypeRef {
    fn from(reference: &ObjectRef<M>) -> Self {
        reference.type_ref()
    }
}

/// An interface definition created by [`Schema::interface_ref`].
#[derive(Clone, Debug, PartialEq, Eq, Hash)]
pub struct InterfaceRef {
    name: Name,
}

impl InterfaceRef {
    fn new(name: Name) -> Self {
        Self { name }
    }

    /// The interface's GraphQL name.
    #[must_use]
    pub fn name(&self) -> &str {
        &self.name
    }

    /// A reference to this interface with no list or nullability wrappers.
    #[must_use]
    pub fn type_ref(&self) -> TypeRef {
        TypeRef::named(Arc::clone(&self.name))
    }

    /// Whether `object_type` is a possible type of this interface in `schema`.
    #[must_use]
    pub fn is_possible_type(&self, schema: &gql::Schema, object_type: &str) -> bool {
        schema.is_possible_type(self.name(), object_type)
    }

    /// Every object type that can appear where this interface is expected.
    #[must_use]
    pub fn possible_types(&self, schema: &gql::Schema) -> Vec<Name> {
        schema.possible_types(self.name())
    }
}

impl From<&InterfaceRef> for InterfaceRef {
    fn from(reference: &InterfaceRef) -> Self {
        reference.clone()
    }
}

/// A union definition created by [`Schema::union_ref`].
#[derive(Clone, Debug, PartialEq, Eq, Hash)]
pub struct UnionRef {
    name: Name,
}

impl UnionRef {
    fn new(name: Name) -> Self {
        Self { name }
    }

    /// The union's GraphQL name.
    #[must_use]
    pub fn name(&self) -> &str {
        &self.name
    }

    /// A reference to this union with no list or nullability wrappers.
    #[must_use]
    pub fn type_ref(&self) -> TypeRef {
        TypeRef::named(Arc::clone(&self.name))
    }

    /// Whether `object_type` is a possible type of this union in `schema`.
    #[must_use]
    pub fn is_possible_type(&self, schema: &gql::Schema, object_type: &str) -> bool {
        schema.is_possible_type(self.name(), object_type)
    }

    /// Every object type that can appear where this union is expected.
    #[must_use]
    pub fn possible_types(&self, schema: &gql::Schema) -> Vec<Name> {
        schema.possible_types(self.name())
    }
}

impl From<&UnionRef> for UnionRef {
    fn from(reference: &UnionRef) -> Self {
        reference.clone()
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
    object: Name,
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
    objects: IndexMap<Name, gql::ObjectType>,
    /// The Rust model that owns each object name, so two models cannot silently merge.
    object_owners: IndexMap<Name, TypeId>,
    /// Names for which an ownership collision has already been reported.
    object_conflicts: HashSet<Name>,
    /// Fields declared with `raw_field`/`raw_field_with`, per object. A raw object must not also
    /// carry resolver-backed fields, because those would receive the raw map instead of the
    /// model.
    raw_fields: IndexMap<Name, HashSet<Name>>,
    interfaces: IndexMap<Name, gql::InterfaceType>,
    unions: IndexMap<Name, gql::UnionType>,
    query: Vec<gql::Field>,
    mutation: Vec<gql::Field>,
    subscription: Vec<gql::Field>,
    errors: Vec<GraphQLError>,
    query_name: String,
    mutation_name: String,
    subscription_name: String,
    description: Option<Arc<str>>,
    checks: Vec<FieldCheck>,
    object_auth: IndexMap<String, Vec<Arc<AuthGuard<Ctx>>>>,
    /// The latest `auth_error` declared for an object, even when that declaration added no
    /// predicate. It is applied to the existing rule instead of replacing it.
    object_auth_error: IndexMap<String, Arc<str>>,
    interface_auth: IndexMap<String, Vec<Arc<AuthGuard<Ctx>>>>,
    interface_auth_error: IndexMap<String, Arc<str>>,
    interface_field_auth: IndexMap<String, IndexMap<Name, Arc<AuthGuard<Ctx>>>>,
    pending_inherited: Vec<PendingInherited>,
    /// Build-time extensions, applied to every field once when the schema is built.
    plugins: Vec<Arc<dyn Plugin<Ctx>>>,
    marker: PhantomData<fn() -> Ctx>,
}

impl<Ctx: 'static> Clone for Schema<Ctx> {
    fn clone(&self) -> Self {
        Self {
            registry: self.registry.clone(),
            objects: self.objects.clone(),
            object_owners: self.object_owners.clone(),
            object_conflicts: self.object_conflicts.clone(),
            raw_fields: self.raw_fields.clone(),
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
            object_auth_error: self.object_auth_error.clone(),
            interface_auth: self.interface_auth.clone(),
            interface_auth_error: self.interface_auth_error.clone(),
            interface_field_auth: self.interface_field_auth.clone(),
            pending_inherited: self.pending_inherited.clone(),
            plugins: self.plugins.clone(),
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
            .field("plugins", &self.plugins.len())
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
            object_conflicts: HashSet::new(),
            raw_fields: IndexMap::new(),
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
            object_auth_error: IndexMap::new(),
            interface_auth: IndexMap::new(),
            interface_auth_error: IndexMap::new(),
            interface_field_auth: IndexMap::new(),
            pending_inherited: Vec::new(),
            plugins: Vec::new(),
            marker: PhantomData,
        }
    }

    /// Set the schema's description.
    pub fn description(&mut self, description: impl Into<Arc<str>>) -> &mut Self {
        self.description = Some(description.into());
        self
    }

    /// Register a build-time plugin.
    ///
    /// Plugins are stored and applied once when the schema is built (through [`Schema::build`],
    /// [`Schema::build_with`] or [`Schema::build_clone`]), after every type and field has been
    /// declared. Registering a plugin before or after declaring fields is therefore equivalent,
    /// and fields added later are always covered.
    ///
    /// See the [`crate::plugin`] module documentation for ordering and coverage, and
    /// [`crate::TracingPlugin`], [`crate::ResponseExtensionPlugin`] and
    /// [`crate::MockPlugin`] for the built-ins.
    ///
    /// ```
    /// use gurafi::{FieldMeta, NoArgs, Object, Plugin, Schema};
    ///
    /// struct AddDescription;
    ///
    /// impl Plugin<Ctx> for AddDescription {
    ///     fn field(&self, meta: &FieldMeta<'_>, field: &mut gql::Field) {
    ///         if meta.is_root {
    ///             field.description = Some("A root field".into());
    ///         }
    ///     }
    /// }
    ///
    /// # struct Ctx;
    /// # #[derive(Object)] struct Giraffe { name: String }
    /// let mut schema = Schema::<Ctx>::new();
    /// schema.plugin(AddDescription);
    /// schema.query(|q| {
    ///     q.field("ok", |_: NoArgs, _: &Ctx| true);
    /// });
    /// # let _ = schema.build().expect("valid schema");
    /// ```
    pub fn plugin<P: Plugin<Ctx>>(&mut self, plugin: P) -> &mut Self {
        self.plugins.push(Arc::new(plugin));
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

    /// Register an object type under its derived name.
    ///
    /// The GraphQL name and description come from `M`'s [`ObjectModel`] implementation, which
    /// `#[derive(Object)]` generates. Fields are declared in the closure; a field may return
    /// any type that implements [`OutputType`], including a type registered later, because
    /// `gql` resolves type names when the schema is built.
    ///
    /// Calling this again for the same model reopens the object and adds fields to it. This is
    /// how a type's definition can live in several modules. A field name declared twice is
    /// still a build error.
    ///
    /// The returned [`ObjectRef`] can be used as an output type with
    /// [`ObjectBuilder::field_ref`] and passed back to [`Schema::implement`].
    pub fn object<M: ObjectModel>(
        &mut self,
        build: impl FnOnce(&mut ObjectBuilder<'_, Ctx, M>),
    ) -> ObjectRef<M> {
        let reference = self.object_ref(M::NAME);
        self.implement(&reference, build);
        reference
    }

    /// Create (or reopen) an object type whose GraphQL name is computed at runtime.
    ///
    /// The model still supplies `is_type_of`, descriptions and the type a resolver receives, but
    /// the GraphQL name is `name`. Use it for names that only exist after some application
    /// decision, such as Relay's `UserPostsConnection` or `CreateUserPayload`:
    ///
    /// ```
    /// # use gurafi::{NoArgs, Object, Schema};
    /// # struct Ctx;
    /// # #[derive(Object)] struct User { name: String }
    /// let mut schema = Schema::<Ctx>::new();
    /// let admin = schema.object_ref::<User>("AdminUser");
    /// schema.implement(&admin, |o| {
    ///     o.expose(gurafi::expose!(User::name));
    /// });
    /// schema.query(|q| {
    ///     q.field_ref(&admin, "admin", |_: NoArgs, _: &Ctx| User {
    ///         name: "ada".into(),
    ///     });
    /// });
    /// # schema.build().expect("valid schema");
    /// ```
    ///
    /// The name is owned by the schema. Calling this twice with the same name and model merges
    /// the definitions. A different model claiming the same name is a build error.
    pub fn object_ref<M: ObjectModel>(&mut self, name: impl Into<Name>) -> ObjectRef<M> {
        let name = name.into();
        if self.claim_object_name::<M>(&name) {
            self.ensure_object::<M>(&name);
        }
        ObjectRef::new(name)
    }

    /// Add fields to an object created by [`Schema::object_ref`] (or reopen a derived object).
    ///
    /// The reference carries both the runtime name and the model, so this is the typed
    /// counterpart of [`Schema::object`]. Reopening merges fields and declarations; it never
    /// replaces an existing definition.
    pub fn implement<M: ObjectModel>(
        &mut self,
        reference: &ObjectRef<M>,
        build: impl FnOnce(&mut ObjectBuilder<'_, Ctx, M>),
    ) -> &mut Self {
        let name = Arc::clone(&reference.name);
        if !self.claim_object_name::<M>(&name) {
            return self;
        }
        self.ensure_object::<M>(&name);
        let existing = self.objects.shift_remove(&name);
        let existing_auth_error = self.object_auth_error.get(&*name).cloned();
        let raw_fields = self.raw_fields.entry(Arc::clone(&name)).or_default();
        let mut builder = ObjectBuilder::<Ctx, M>::new(
            existing,
            existing_auth_error,
            &mut self.registry,
            &mut self.checks,
            &mut self.pending_inherited,
            raw_fields,
            name.clone(),
        );
        build(&mut builder);
        let (object, auth, auth_error) = builder.finish();
        if let Some(message) = auth_error {
            self.object_auth_error
                .insert(name.to_string(), Arc::clone(&message));
        }
        if auth.is_empty() {
            // An `auth_error` without a predicate refines the rule already installed by an
            // earlier declaration; it must not make the object less guarded.
            if let Some(message) = self.object_auth_error.get(&*name).cloned() {
                if let Some(previous) = self
                    .object_auth
                    .get_mut(&*name)
                    .and_then(|guards| guards.last_mut())
                {
                    *previous = previous.with_message(Some(message));
                }
            }
        } else {
            self.object_auth
                .entry(name.to_string())
                .or_default()
                .extend(auth);
        }
        self.objects.insert(name, object);
        self
    }

    /// Claim an object name for `M`, reporting a collision once.
    fn claim_object_name<M: ObjectModel>(&mut self, name: &Name) -> bool {
        match self.object_owners.get(name) {
            Some(owner) if *owner != TypeId::of::<M>() => {
                if self.object_conflicts.insert(Arc::clone(name)) {
                    self.errors.push(GraphQLError::new(format!(
                        "Object type `{name}` is registered by more than one Rust type. Give one \
                         of them a distinct GraphQL name, or register this model under its own \
                         dynamic name."
                    )));
                }
                false
            }
            Some(_) => true,
            None => {
                self.object_owners
                    .insert(Arc::clone(name), TypeId::of::<M>());
                true
            }
        }
    }

    /// Create an empty object definition if the name is not present yet.
    fn ensure_object<M: ObjectModel>(&mut self, name: &Name) {
        if self.objects.contains_key(name) {
            return;
        }
        let mut object = gql::ObjectType::new(Arc::clone(name));
        if let Some(description) = M::description() {
            object = object.description(description);
        }
        object = object.is_type_of(|value| value.as_external::<M>().is_some());
        self.objects.insert(Arc::clone(name), object);
    }

    /// Reopen an object type and add more fields to it.
    ///
    /// This is an alias for [`Schema::object`] with a name that says the type may already
    /// exist. Use it when fields are split across modules.
    pub fn object_fields<M: ObjectModel>(
        &mut self,
        build: impl FnOnce(&mut ObjectBuilder<'_, Ctx, M>),
    ) -> ObjectRef<M> {
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
        let object_name: Name = Arc::from(M::NAME);
        if self.claim_object_name::<M>(&object_name) {
            self.ensure_object::<M>(&object_name);
        }
        if !self.objects.contains_key(&object_name) {
            self.objects
                .insert(Arc::clone(&object_name), new_object::<M>());
        }
        let (field, validation) =
            object_field::<M, Ctx, T, A, F>(name, resolver, &mut self.registry);
        let field_name = field.name.clone();
        let object = self
            .objects
            .get_mut(&object_name)
            .expect("the object was just inserted");
        object.insert_field(field);
        FieldConfig::for_object_checked(
            &mut self.registry,
            &mut object.fields,
            &mut self.checks,
            object_name.to_string(),
            field_name,
            T::is_optional(),
            std::any::type_name::<T>(),
            TypeId::of::<A>(),
            validation,
        )
    }

    /// Register an interface type.
    ///
    /// Returns an [`InterfaceRef`] that object builders use with
    /// [`ObjectBuilder::implements`]. Interface fields have no resolvers: execution uses the
    /// concrete object's field resolvers.
    ///
    /// Calling this again for the same name reopens the interface and adds fields.
    pub fn interface(
        &mut self,
        name: impl Into<Name>,
        build: impl FnOnce(&mut InterfaceBuilder<'_, Ctx>),
    ) -> InterfaceRef {
        let reference = self.interface_ref(name);
        self.implement_interface(&reference, build);
        reference
    }

    /// Create (or reopen) an interface whose GraphQL name may be computed at runtime.
    ///
    /// Use [`Schema::implement_interface`] to add fields. Calling this twice with the same name
    /// reopens the same interface.
    pub fn interface_ref(&mut self, name: impl Into<Name>) -> InterfaceRef {
        let name = name.into();
        if !self.interfaces.contains_key(&name) {
            self.interfaces.insert(
                Arc::clone(&name),
                gql::InterfaceType::new(Arc::clone(&name)),
            );
        }
        InterfaceRef::new(name)
    }

    /// Add fields to an interface created by [`Schema::interface_ref`].
    pub fn implement_interface(
        &mut self,
        reference: &InterfaceRef,
        build: impl FnOnce(&mut InterfaceBuilder<'_, Ctx>),
    ) -> &mut Self {
        let name = Arc::clone(&reference.name);
        if !self.interfaces.contains_key(&name) {
            self.interfaces.insert(
                Arc::clone(&name),
                gql::InterfaceType::new(Arc::clone(&name)),
            );
        }
        let existing = self.interfaces.shift_remove(&name);
        let existing_auth_error = self.interface_auth_error.get(&*name).cloned();
        let field_auth = self
            .interface_field_auth
            .shift_remove(&*name)
            .unwrap_or_default();
        let mut builder = InterfaceBuilder::<Ctx>::new(
            existing,
            field_auth,
            existing_auth_error,
            &mut self.registry,
            name.clone(),
        );
        build(&mut builder);
        let (interface, auth, auth_error, field_auth) = builder.finish();
        if let Some(message) = auth_error {
            self.interface_auth_error
                .insert(name.to_string(), Arc::clone(&message));
        }
        if auth.is_empty() {
            if let Some(message) = self.interface_auth_error.get(&*name).cloned() {
                if let Some(previous) = self
                    .interface_auth
                    .get_mut(&*name)
                    .and_then(|guards| guards.last_mut())
                {
                    *previous = previous.with_message(Some(message));
                }
            }
        } else {
            self.interface_auth
                .entry(name.to_string())
                .or_default()
                .extend(auth);
        }
        self.interface_field_auth
            .insert(name.to_string(), field_auth);
        self.interfaces.insert(name, interface);
        self
    }

    /// Reopen an interface and add more fields to it.
    ///
    /// This is an alias for [`Schema::interface`].
    pub fn interface_fields(
        &mut self,
        name: impl Into<Name>,
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
        name: impl Into<Name>,
        build: impl FnOnce(&mut UnionBuilder<Ctx>),
    ) -> UnionRef {
        let reference = self.union_ref(name);
        self.implement_union(&reference, build);
        reference
    }

    /// Create (or reopen) a union whose GraphQL name may be computed at runtime.
    ///
    /// Use [`Schema::implement_union`] to declare members.
    pub fn union_ref(&mut self, name: impl Into<Name>) -> UnionRef {
        let name = name.into();
        if !self.unions.contains_key(&name) {
            self.unions
                .insert(Arc::clone(&name), gql::UnionType::new(Arc::clone(&name)));
        }
        UnionRef::new(name)
    }

    /// Add members to a union created by [`Schema::union_ref`].
    pub fn implement_union(
        &mut self,
        reference: &UnionRef,
        build: impl FnOnce(&mut UnionBuilder<Ctx>),
    ) -> &mut Self {
        let name = Arc::clone(&reference.name);
        if !self.unions.contains_key(&name) {
            self.unions
                .insert(Arc::clone(&name), gql::UnionType::new(Arc::clone(&name)));
        }
        let existing = self.unions.shift_remove(&name);
        let mut builder = UnionBuilder::<Ctx>::new(existing, name.clone(), &mut self.registry);
        build(&mut builder);
        let union = builder.finish();
        self.unions.insert(name, union);
        self
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
        self.merge_generated_types(&mut errors);
        errors.extend(self.registry.take_errors());
        let has_unnamed_input = self.registry.has_unnamed_input();

        for name in self.registry.referenced_objects() {
            if !self.objects.contains_key(name) {
                errors.push(GraphQLError::new(format!(
                    "Object type `{name}` is used by a field but was never registered. Call \
                     `schema.object::<...>(...)` or \
                     `schema.object_ref::<...>(\"{name}\")` before calling `build()`."
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

        self.check_raw_field_mixing(&mut errors);
        self.expand_transitive_interfaces();
        self.resolve_inherited(&mut errors);

        // Plugins are installed before object- and interface-level authorization so those rules
        // stay outside the plugin chain. Taking the list out keeps the rest of the build free to
        // move the type maps.
        let plugins = std::mem::take(&mut self.plugins);
        apply_plugins(
            &plugins,
            PluginTargets {
                objects: &mut self.objects,
                interfaces: &mut self.interfaces,
                query: &mut self.query,
                mutation: &mut self.mutation,
                subscription: &mut self.subscription,
                query_name: &self.query_name,
                mutation_name: &self.mutation_name,
                subscription_name: &self.subscription_name,
            },
        );
        self.apply_guards();

        let mut builder = gql::Schema::builder();
        if let Some(description) = self.description.clone() {
            builder = builder.description(description);
        }

        let mut registry_types: Vec<(String, gql::Type)> =
            self.registry.into_types().into_iter().collect();
        let mut objects: Vec<(Name, gql::ObjectType)> = self.objects.into_iter().collect();
        let mut interfaces: Vec<(Name, gql::InterfaceType)> = self.interfaces.into_iter().collect();
        let mut unions: Vec<(Name, gql::UnionType)> = self.unions.into_iter().collect();
        if options.sort_types {
            registry_types.sort_by(|left, right| left.0.cmp(&right.0));
            objects.sort_by(|left, right| left.0.cmp(&right.0));
            interfaces.sort_by(|left, right| left.0.cmp(&right.0));
            unions.sort_by(|left, right| left.0.cmp(&right.0));
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

        for plugin in &plugins {
            plugin.configure(&mut builder);
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

    /// Merge objects and unions generated by result-union fields into the user-declared maps.
    ///
    /// The generated types are built while fields are declared, but plugins and authorization
    /// run on `objects` and `unions`, so they live in a side table until now. A generated name
    /// that collides with a user-declared type is reported rather than silently replacing it.
    fn merge_generated_types(&mut self, errors: &mut Vec<GraphQLError>) {
        let (objects, unions) = self.registry.take_generated();
        for (name, object) in objects {
            if self.objects.contains_key(name.as_str()) {
                errors.push(GraphQLError::new(format!(
                    "Generated result-union success type `{name}` collides with an object type \
                     that is already registered. Give the field a different name, or register \
                     the generated type explicitly instead of using `field_result_union`."
                )));
                continue;
            }
            self.objects.insert(Arc::from(name.as_str()), object);
        }
        for (name, union) in unions {
            if self.unions.contains_key(name.as_str()) {
                errors.push(GraphQLError::new(format!(
                    "Generated result-union type `{name}` collides with a union type that is \
                     already registered. Give the field a different name, or register the \
                     generated type explicitly instead of using `field_result_union`."
                )));
                continue;
            }
            self.unions.insert(Arc::from(name.as_str()), union);
        }
        for (name, error) in self.registry.take_required_objects() {
            if !self.objects.contains_key(name.as_str()) {
                errors.push(error);
            }
        }
    }

    /// Add interfaces that are only implemented transitively.
    ///
    /// The specification requires an object that implements `Resource` to also declare every
    /// interface `Resource` implements (`Node`). Repeating that by hand is easy to forget, so the
    /// builder closes the relation before it hands the definitions to `gql`.
    fn expand_transitive_interfaces(&mut self) {
        let parents = interface_parents(&self.interfaces);
        for (name, interface) in &mut self.interfaces {
            let reachable = reachable_interfaces(&interface.interfaces, &parents);
            for parent in reachable {
                if parent.as_str() != name.as_ref()
                    && !interface
                        .interfaces
                        .iter()
                        .any(|existing| existing.as_ref() == parent.as_str())
                {
                    interface.interfaces.push(Arc::from(parent));
                }
            }
        }
        let parents = interface_parents(&self.interfaces);
        for object in self.objects.values_mut() {
            let reachable = reachable_interfaces(&object.interfaces, &parents);
            for name in reachable {
                if !object
                    .interfaces
                    .iter()
                    .any(|existing| existing.as_ref() == name.as_str())
                {
                    object.interfaces.push(Arc::from(name));
                }
            }
        }
    }

    /// Reject objects that mix raw fields with resolver-backed fields.
    ///
    /// A raw object is resolved as a whole, so its parent value is an object map. A
    /// resolver-backed field on the same object would be handed that map where its resolver
    /// expects the Rust model, which only fails later, per request, with an internal error.
    fn check_raw_field_mixing(&self, errors: &mut Vec<GraphQLError>) {
        for (object_name, raw_fields) in &self.raw_fields {
            if raw_fields.is_empty() {
                continue;
            }
            let Some(object) = self.objects.get(object_name) else {
                continue;
            };
            for field in object.fields.values() {
                if raw_fields.contains(&field.name) {
                    continue;
                }
                errors.push(GraphQLError::new(format!(
                    "Object `{object_name}` mixes raw fields with resolver-backed field `{}`. A \
                     raw object is resolved as a whole, so that resolver would receive a raw \
                     object map where it expects the `{object_name}` model. Declare every field \
                     with `raw_field`/`raw_field_with`, or give every field a resolver.",
                    field.name,
                )));
            }
        }
    }

    /// Rewrite fields declared with [`ObjectBuilder::resolve_inherited`] from the matching
    /// interface field.
    fn resolve_inherited(&mut self, errors: &mut Vec<GraphQLError>) {
        let parents = interface_parents(&self.interfaces);
        for pending in std::mem::take(&mut self.pending_inherited) {
            let Some(object) = self.objects.get(pending.object.as_ref()) else {
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
                    source = Some((field.clone(), interface_name.clone()));
                    break;
                }
            }
            let Some((interface_field, interface_name)) = source else {
                errors.push(GraphQLError::new(format!(
                    "Object `{}` declares field `{}` with `resolve_inherited`, but no implemented \
                     interface has that field. Declare it on an interface with \
                     `interface.field(...)`, or use `field` to give the object its own type.",
                    pending.object, pending.field
                )));
                continue;
            };
            let interface_ty = interface_field.ty.clone();
            let interface_args = interface_field.args.clone();
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
                .get_mut(pending.object.as_ref())
                .expect("the object was checked above");
            let Some(field) = object.fields.get_mut(&pending.field) else {
                continue;
            };
            let actual = field.ty.clone();
            self.checks.push(FieldCheck::InheritedType {
                owner: pending.object.to_string(),
                field: pending.field.clone(),
                actual,
                expected: interface_ty.clone(),
            });
            field.ty = interface_ty;
            field.args = interface_args;
            if field.description.is_none() {
                field.description = interface_field.description;
            }
            if field.deprecation_reason.is_none() {
                field.deprecation_reason = interface_field.deprecation_reason;
            }
            for directive in interface_field.directives {
                if !field
                    .directives
                    .iter()
                    .any(|existing| existing.name == directive.name)
                {
                    field.directives.push(directive);
                }
            }
        }
    }

    /// Apply object- and interface-level authorization to every affected field.
    fn apply_guards(&mut self) {
        let parents = interface_parents(&self.interfaces);
        for (object_name, object) in &mut self.objects {
            let reachable = reachable_interfaces(&object.interfaces, &parents);
            let object_guards = self.object_auth.get(object_name.as_ref()).cloned();
            for field in object.fields.values_mut() {
                if let Some(guards) = &object_guards {
                    for guard in guards {
                        apply_auth_guard(field, Arc::clone(guard));
                    }
                }
                for interface_name in &reachable {
                    if let Some(guards) = self.interface_auth.get(interface_name.as_str()) {
                        for guard in guards {
                            apply_auth_guard(field, Arc::clone(guard));
                        }
                    }
                    if let Some(guard) = self
                        .interface_field_auth
                        .get(interface_name.as_str())
                        .and_then(|fields| fields.get(&field.name))
                    {
                        apply_auth_guard(field, Arc::clone(guard));
                    }
                }
            }
        }
    }
}

/// The direct super-interfaces of every registered interface, keyed by name.
fn interface_parents(
    interfaces: &IndexMap<Name, gql::InterfaceType>,
) -> HashMap<String, Vec<String>> {
    interfaces
        .iter()
        .map(|(name, interface)| {
            (
                name.to_string(),
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
    raw_fields: &'a mut HashSet<Name>,
    auth: Vec<Arc<AuthGuard<Ctx>>>,
    auth_error: Option<Arc<str>>,
    marker: PhantomData<fn() -> (Ctx, M)>,
}

impl<'a, Ctx: 'static, M: ObjectModel> ObjectBuilder<'a, Ctx, M> {
    fn new(
        existing: Option<gql::ObjectType>,
        existing_auth_error: Option<Arc<str>>,
        registry: &'a mut TypeRegistry,
        checks: &'a mut Vec<FieldCheck>,
        pending_inherited: &'a mut Vec<PendingInherited>,
        raw_fields: &'a mut HashSet<Name>,
        name: Name,
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
            raw_fields,
            auth: Vec::new(),
            auth_error: existing_auth_error,
            marker: PhantomData,
        }
    }

    fn finish(self) -> (gql::ObjectType, Vec<Arc<AuthGuard<Ctx>>>, Option<Arc<str>>) {
        (self.object, self.auth, self.auth_error)
    }

    /// Set the object's description, overriding the one from the derive.
    pub fn description(&mut self, description: impl Into<Arc<str>>) -> &mut Self {
        self.object.description = Some(description.into());
        self
    }

    /// Whether this object already declares a field with `name`.
    ///
    /// This is a read-only inspection for extensions that add a field only when the object has
    /// not declared it yet (for example a Relay `id` field). It never changes the definition.
    #[must_use]
    pub fn has_field(&self, name: &str) -> bool {
        self.object
            .fields
            .keys()
            .any(|field| field.as_ref() == name)
    }

    /// Declare that this object implements an interface.
    ///
    /// Both `node` and `&node` are accepted, so a reference can be reused without cloning.
    pub fn implements(&mut self, interface: impl Into<InterfaceRef>) -> &mut Self {
        let interface = interface.into();
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
    /// [`ObjectBuilder::resolve_inherited`]. Declaring another rule on a reopened object composes
    /// with the existing one (all rules must allow); it never replaces an earlier rule. A denial
    /// follows normal null propagation.
    pub fn auth(&mut self, predicate: impl Fn(&Ctx) -> bool + Send + Sync + 'static) -> &mut Self {
        self.auth
            .push(crate::field::sync_guard(predicate, self.auth_error.clone()));
        self
    }

    /// Like [`ObjectBuilder::auth`], but the predicate is asynchronous.
    pub fn auth_async(
        &mut self,
        predicate: impl for<'r> Fn(&'r Ctx) -> BoxFuture<'r, bool> + Send + Sync + 'static,
    ) -> &mut Self {
        self.auth.push(crate::field::async_guard(
            predicate,
            self.auth_error.clone(),
        ));
        self
    }

    /// Set the message returned when the object-level authorization denies access.
    pub fn auth_error(&mut self, message: impl Into<Arc<str>>) -> &mut Self {
        self.auth_error = Some(message.into());
        if let Some(auth) = self.auth.last_mut() {
            *auth = auth.with_message(self.auth_error.clone());
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
        self.push(
            field,
            T::is_optional(),
            std::any::type_name::<T>(),
            TypeId::of::<()>(),
            ValidationSlot::new(),
        )
    }

    /// Declare a field that is read from a raw object map instead of a resolver.
    ///
    /// No resolver is installed. When the parent value is the object map produced by
    /// [`Raw::object`](crate::Raw::object), execution reads the field with the same name using
    /// the executor's default property lookup. This is the object-side counterpart to
    /// [`Raw`](crate::Raw): declare every field of an object this way when the object is
    /// resolved as a whole.
    ///
    /// ```
    /// use gurafi::{NoArgs, Object, Raw, RawObject, Schema};
    ///
    /// struct Ctx;
    ///
    /// #[derive(Object)]
    /// struct Giraffe {
    ///     name: String,
    /// }
    ///
    /// let mut schema = Schema::<Ctx>::new();
    /// schema.object::<Giraffe>(|o| {
    ///     o.raw_field::<String>("name");
    /// });
    /// schema.query(|q| {
    ///     q.field("giraffe", |_: NoArgs, _: &Ctx| {
    ///         Raw::<Giraffe>::object(RawObject::new().field("name", "Melman").build())
    ///     });
    /// });
    /// # let _ = schema.build().expect("valid schema");
    /// ```
    ///
    /// The type comes from `T`, so the declared GraphQL type is still checked against the rest
    /// of the schema. The value in the map is not: a missing non-null field or a wrong-typed
    /// field is an execution-time error, not a build error. An object that declares a raw field
    /// may not also declare a resolver-backed field; the build rejects the mix because the
    /// resolver would receive a raw object map where it expects the Rust model.
    ///
    /// `raw_field` has no arguments. To implement an interface field that takes arguments (or to
    /// have the executor coerce and validate arguments for a concrete raw field), use
    /// [`ObjectBuilder::raw_field_with`]. A `validate` hook on `raw_field` is a build error, since
    /// there is no resolver in which it could run.
    ///
    /// The map itself is trusted: object keys are read by name, and reserved `__gql_*` keys are
    /// ignored by [`crate::RawObject`] and removed by [`crate::Raw::new`]. Untrusted data must go
    /// through one of those constructors, never through an arbitrary `gql::Value::Object`.
    pub fn raw_field<T: OutputType>(&mut self, name: &'static str) -> FieldConfig<'_, Ctx> {
        T::register_output(self.registry);
        self.raw_fields.insert(Arc::from(name));
        let field = gql::Field::new(name, T::output_type());
        self.push(
            field,
            T::is_optional(),
            std::any::type_name::<T>(),
            TypeId::of::<RawFieldArgs>(),
            ValidationSlot::new(),
        )
    }

    /// Declare a raw field with arguments, read from the raw object map by name.
    ///
    /// This is [`ObjectBuilder::raw_field`] plus `A`'s argument declarations. The executor
    /// coerces and validates the arguments through a normal resolver, then reads the field value
    /// from the parent object map. Use it for an interface field that takes arguments, or when a
    /// concrete raw field needs argument coercion:
    ///
    /// ```
    /// use gurafi::{Input, NoArgs, Object, Raw, RawObject, Schema};
    ///
    /// struct Ctx;
    ///
    /// #[derive(Object)]
    /// struct Giraffe {
    ///     name: String,
    /// }
    ///
    /// #[derive(Input)]
    /// struct PictureArgs {
    ///     width: i32,
    /// }
    ///
    /// let mut schema = Schema::<Ctx>::new();
    /// schema.object::<Giraffe>(|o| {
    ///     o.raw_field_with::<String, PictureArgs>("picture");
    /// });
    /// schema.query(|q| {
    ///     q.field("giraffe", |_: NoArgs, _: &Ctx| {
    ///         Raw::<Giraffe>::object(
    ///             RawObject::new().field("picture", "wide.jpg").build(),
    ///         )
    ///     });
    /// });
    /// # let _ = schema.build().expect("valid schema");
    /// ```
    ///
    /// A `validate` hook is run once per resolution, just as it is on a resolver-backed field.
    /// Authorization installs a guard around the same resolver.
    pub fn raw_field_with<T: OutputType, A: InputObject>(
        &mut self,
        name: &'static str,
    ) -> FieldConfig<'_, Ctx> {
        T::register_output(self.registry);
        A::register_args(self.registry);
        self.raw_fields.insert(Arc::from(name));
        let validation = ValidationSlot::new();
        let hook = validation.clone();
        let field_name = name;
        let field = add_arguments::<A>(gql::Field::new(name, T::output_type()), self.registry)
            .resolve(move |context| {
                let hook = hook.clone();
                Box::pin(async move {
                    let args = coerce_args::<A>(
                        context.args,
                        context.info.parent_type,
                        context.info.field_name,
                    )?;
                    if hook.is_installed() {
                        let ctx = context.context.require_data::<Ctx>()?;
                        hook.run(&args, ctx)?;
                    }
                    read_raw_field(field_name, context.parent)
                })
            });
        self.push(
            field,
            T::is_optional(),
            std::any::type_name::<T>(),
            TypeId::of::<A>(),
            validation,
        )
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

    /// Add a synchronous field whose output type is a runtime-named object.
    ///
    /// `reference` supplies the GraphQL name; `T` supplies the list and nullability wrappers,
    /// just as [`FieldConfig::as_object_ref`] does. The resolver returns the object's model (or a
    /// [`crate::Raw`] of it), so `T`'s named type should be the model the reference was created
    /// for.
    ///
    /// ```
    /// # use gurafi::{NoArgs, Object, Schema};
    /// # struct Ctx;
    /// #[derive(Object)]
    /// struct User { name: String }
    ///
    /// let mut schema = Schema::<Ctx>::new();
    /// let admin = schema.object_ref::<User>("AdminUser");
    /// schema.implement(&admin, |o| {
    ///     o.expose(gurafi::expose!(User::name));
    /// });
    /// schema.object::<User>(|o| {
    ///     o.field_ref(&admin, "bestFriend", |_: &User, _: NoArgs, _: &Ctx| User {
    ///         name: "ada".into(),
    ///     });
    ///     o.field_ref::<Vec<User>, NoArgs, _, _>(
    ///         &admin,
    ///         "friends",
    ///         |_: &User, _: NoArgs, _: &Ctx| vec![User { name: "ada".into() }],
    ///     )
    ///     .nullable_items();
    /// });
    /// # let _ = schema;
    /// ```
    pub fn field_ref<T, A, F, N>(
        &mut self,
        reference: &ObjectRef<N>,
        name: &'static str,
        resolver: F,
    ) -> FieldConfig<'_, Ctx>
    where
        N: ObjectModel,
        T: OutputType + IntoValue + Send + 'static,
        A: InputObject,
        F: Fn(&M, A, &Ctx) -> T + Send + Sync + 'static,
    {
        let (field, validation) = object_field_with_info::<M, Ctx, T, A, _>(
            name,
            move |model: &M, args: A, ctx: &Ctx, _| resolver(model, args, ctx),
            self.registry,
            false,
        );
        let mut config = self.push(
            field,
            T::is_optional(),
            std::any::type_name::<T>(),
            TypeId::of::<A>(),
            validation,
        );
        config.as_object_ref(reference);
        config
    }

    /// Add a synchronous field with an explicitly built runtime type reference.
    ///
    /// Use this for runtime-named interfaces and unions, or for arbitrary wrapper combinations
    /// (`TypeRef::named(name).list().non_null()`). `T` still supplies the Rust type used to
    /// check that the declared wrappers are truthful.
    pub fn field_ref_type<T, A, F>(
        &mut self,
        ty: TypeRef,
        name: &'static str,
        resolver: F,
    ) -> FieldConfig<'_, Ctx>
    where
        T: OutputType + IntoValue + Send + 'static,
        A: InputObject,
        F: Fn(&M, A, &Ctx) -> T + Send + Sync + 'static,
    {
        let (field, validation) = object_field_with_info::<M, Ctx, T, A, _>(
            name,
            move |model: &M, args: A, ctx: &Ctx, _| resolver(model, args, ctx),
            self.registry,
            false,
        );
        let mut config = self.push(
            field,
            T::is_optional(),
            std::any::type_name::<T>(),
            TypeId::of::<A>(),
            validation,
        );
        config.as_dynamic(ty);
        config
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
        let (field, validation) =
            object_field_with_info::<M, Ctx, T, A, F>(name, resolver, self.registry, true);
        self.push(
            field,
            T::is_optional(),
            std::any::type_name::<T>(),
            TypeId::of::<A>(),
            validation,
        )
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
        let (field, validation) =
            object_field_result_with_info::<M, Ctx, T, A, F>(name, resolver, self.registry);
        self.push(
            field,
            T::is_optional(),
            std::any::type_name::<T>(),
            TypeId::of::<A>(),
            validation,
        )
    }

    /// Add a synchronous field whose resolver returns a typed `Result`.
    ///
    /// Unlike [`ObjectBuilder::field_result`], the failure value is part of the schema: the
    /// field returns a generated `<Parent><Field>Result` union whose members are a
    /// `<Parent><Field>Success` payload (`data: T`) and each variant of `E`'s object type. This
    /// is the Pothos Errors pattern: a client can select error payload fields instead of only
    /// reading a message from the GraphQL `errors` array.
    ///
    /// `E` is declared with `#[derive(ErrorUnion)]`; its payload objects must still be
    /// registered with `schema.object::<M>(...)`, otherwise the build reports the missing type.
    /// The generated names come from the object's actual GraphQL name (including a
    /// [`Schema::object_ref`] runtime name) and the field name, PascalCased.
    ///
    /// ```
    /// use gurafi::{ErrorUnion, NoArgs, Object, Schema};
    ///
    /// struct Ctx;
    ///
    /// #[derive(Object)]
    /// struct User {
    ///     name: String,
    /// }
    ///
    /// #[derive(Object)]
    /// struct ConflictError {
    ///     message: String,
    /// }
    ///
    /// #[derive(ErrorUnion)]
    /// enum CreateUserError {
    ///     Conflict(ConflictError),
    /// }
    ///
    /// let mut schema = Schema::<Ctx>::new();
    /// schema.object::<User>(|o| {
    ///     o.field("name", |u: &User, _: NoArgs, _: &Ctx| u.name.clone());
    /// });
    /// schema.object::<ConflictError>(|o| {
    ///     o.field("message", |e: &ConflictError, _: NoArgs, _: &Ctx| e.message.clone());
    /// });
    /// schema.query(|q| {
    ///     q.field("ok", |_: NoArgs, _: &Ctx| true);
    /// });
    /// schema.mutation(|m| {
    ///     m.field_result_union::<User, CreateUserError, NoArgs, _>(
    ///         "createUser",
    ///         |_: NoArgs, _: &Ctx| {
    ///             Ok(User { name: "ada".into() })
    ///         },
    ///     );
    /// });
    /// // `Mutation.createUser: MutationCreateUserResult!`
    /// //   `type MutationCreateUserSuccess { data: User! }`
    /// //   `union MutationCreateUserResult = MutationCreateUserSuccess | ConflictError`
    /// let schema = schema.build().expect("valid schema");
    /// # let _ = schema;
    /// ```
    pub fn field_result_union<T, E, A, F>(
        &mut self,
        name: &'static str,
        resolver: F,
    ) -> FieldConfig<'_, Ctx>
    where
        T: OutputType + IntoValue + Send + 'static,
        E: ErrorUnionModel,
        A: InputObject,
        F: Fn(&M, A, &Ctx) -> core::result::Result<T, E> + Send + Sync + 'static,
    {
        self.field_result_union_with_info::<T, E, A, _>(
            name,
            move |model: &M, args: A, ctx: &Ctx, _| resolver(model, args, ctx),
        )
    }

    /// Add a synchronous typed-result field whose resolver also receives [`gql::ResolveInfo`].
    pub fn field_result_union_with_info<T, E, A, F>(
        &mut self,
        name: &'static str,
        resolver: F,
    ) -> FieldConfig<'_, Ctx>
    where
        T: OutputType + IntoValue + Send + 'static,
        E: ErrorUnionModel,
        A: InputObject,
        F: for<'r> Fn(&'r M, A, &'r Ctx, &'r gql::ResolveInfo<'r>) -> core::result::Result<T, E>
            + Send
            + Sync
            + 'static,
    {
        let parent = self.object.name.to_string();
        let (field, validation) = object_field_result_union_with_info::<M, Ctx, T, E, A, F>(
            name,
            resolver,
            self.registry,
            &parent,
        );
        self.push(
            field,
            false,
            std::any::type_name::<T>(),
            TypeId::of::<A>(),
            validation,
        )
    }

    /// Add an asynchronous typed-result field.
    ///
    /// This is the async form of [`ObjectBuilder::field_result_union`]. The resolver returns a
    /// boxed future, written as `Box::pin(async move { ... })`, exactly like the other async
    /// resolver variants.
    pub fn field_result_union_async<T, E, A, F>(
        &mut self,
        name: &'static str,
        resolver: F,
    ) -> FieldConfig<'_, Ctx>
    where
        T: OutputType + IntoValue + Send + 'static,
        E: ErrorUnionModel,
        A: InputObject,
        F: for<'r> Fn(&'r M, A, &'r Ctx) -> BoxFuture<'r, core::result::Result<T, E>>
            + Send
            + Sync
            + 'static,
    {
        self.field_result_union_async_with_info::<T, E, A, _>(
            name,
            move |model: &M, args: A, ctx: &Ctx, _| resolver(model, args, ctx),
        )
    }

    /// Add an asynchronous typed-result field whose resolver also receives
    /// [`gql::ResolveInfo`].
    pub fn field_result_union_async_with_info<T, E, A, F>(
        &mut self,
        name: &'static str,
        resolver: F,
    ) -> FieldConfig<'_, Ctx>
    where
        T: OutputType + IntoValue + Send + 'static,
        E: ErrorUnionModel,
        A: InputObject,
        F: for<'r> Fn(
                &'r M,
                A,
                &'r Ctx,
                &'r gql::ResolveInfo<'r>,
            ) -> BoxFuture<'r, core::result::Result<T, E>>
            + Send
            + Sync
            + 'static,
    {
        let parent = self.object.name.to_string();
        let (field, validation) = object_field_result_union_async_with_info::<M, Ctx, T, E, A, F>(
            name,
            resolver,
            self.registry,
            &parent,
        );
        self.push(
            field,
            false,
            std::any::type_name::<T>(),
            TypeId::of::<A>(),
            validation,
        )
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
        let (field, validation) =
            object_field_async_with_info::<M, Ctx, T, A, F>(name, resolver, self.registry);
        self.push(
            field,
            T::is_optional(),
            std::any::type_name::<T>(),
            TypeId::of::<A>(),
            validation,
        )
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
        let (field, validation) =
            object_field_result_async_with_info::<M, Ctx, T, A, F>(name, resolver, self.registry);
        self.push(
            field,
            T::is_optional(),
            std::any::type_name::<T>(),
            TypeId::of::<A>(),
            validation,
        )
    }

    /// Add a field whose resolver also receives the executor's [`gql::ResolverContext`].
    ///
    /// This is the escape hatch for request-level response data: call
    /// `execution.context.set_extension("key", value)` from the resolver and the value appears
    /// in the response's `extensions` object. Everything the other resolver variants expose is
    /// reachable from `execution.info` and `execution.args`.
    ///
    /// ```
    /// # use gurafi::{NoArgs, Object, Schema};
    /// # struct Ctx;
    /// # #[derive(Object)] struct Giraffe { name: String }
    /// # let mut schema = Schema::<Ctx>::new();
    /// # schema.object::<Giraffe>(|o| {
    /// o.field_with_execution("name", |g: &Giraffe, _: NoArgs, _: &Ctx, execution| {
    ///     execution.context.set_extension("servedBy", "gurafi");
    ///     g.name.clone()
    /// });
    /// # });
    /// ```
    pub fn field_with_execution<T, A, F>(
        &mut self,
        name: &'static str,
        resolver: F,
    ) -> FieldConfig<'_, Ctx>
    where
        T: OutputType + IntoValue + Send + 'static,
        A: InputObject,
        F: for<'r> Fn(&'r M, A, &'r Ctx, &'r gql::ResolverContext<'r>) -> T + Send + Sync + 'static,
    {
        let (field, validation) =
            object_field_with_execution::<M, Ctx, T, A, F>(name, resolver, self.registry);
        self.push(
            field,
            T::is_optional(),
            std::any::type_name::<T>(),
            TypeId::of::<A>(),
            validation,
        )
    }

    /// Like [`ObjectBuilder::field_with_execution`], but the resolver returns a `Result`.
    pub fn field_result_with_execution<T, A, F>(
        &mut self,
        name: &'static str,
        resolver: F,
    ) -> FieldConfig<'_, Ctx>
    where
        T: OutputType + IntoValue + Send + 'static,
        A: InputObject,
        F: for<'r> Fn(&'r M, A, &'r Ctx, &'r gql::ResolverContext<'r>) -> Result<T, GraphQLError>
            + Send
            + Sync
            + 'static,
    {
        let (field, validation) =
            object_field_result_with_execution::<M, Ctx, T, A, F>(name, resolver, self.registry);
        self.push(
            field,
            T::is_optional(),
            std::any::type_name::<T>(),
            TypeId::of::<A>(),
            validation,
        )
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
    ///     o.implements(&node);
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
                    self.object.name,
                    error.message()
                )));
                Vec::new()
            }
        };
        self.pending_inherited.push(PendingInherited {
            object: Arc::clone(&self.object.name),
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
        args_type: TypeId,
        validation: ValidationSlot<Ctx>,
    ) -> FieldConfig<'_, Ctx> {
        let name = field.name.clone();
        let owner = self.object.name.to_string();
        self.object.insert_field(field);
        FieldConfig::for_object_checked(
            self.registry,
            &mut self.object.fields,
            self.checks,
            owner,
            name,
            rust_nullable,
            rust_type_name,
            args_type,
            validation,
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
    auth: Vec<Arc<AuthGuard<Ctx>>>,
    auth_error: Option<Arc<str>>,
    marker: PhantomData<fn() -> Ctx>,
}

impl<'a, Ctx: 'static> InterfaceBuilder<'a, Ctx> {
    fn new(
        existing: Option<gql::InterfaceType>,
        field_auth: IndexMap<Name, Arc<AuthGuard<Ctx>>>,
        existing_auth_error: Option<Arc<str>>,
        registry: &'a mut TypeRegistry,
        name: Name,
    ) -> Self {
        Self {
            registry,
            interface: existing.unwrap_or_else(|| gql::InterfaceType::new(name)),
            field_auth,
            auth: Vec::new(),
            auth_error: existing_auth_error,
            marker: PhantomData,
        }
    }

    fn finish(
        self,
    ) -> (
        gql::InterfaceType,
        Vec<Arc<AuthGuard<Ctx>>>,
        Option<Arc<str>>,
        IndexMap<Name, Arc<AuthGuard<Ctx>>>,
    ) {
        (self.interface, self.auth, self.auth_error, self.field_auth)
    }

    /// Set the interface's description.
    pub fn description(&mut self, description: impl Into<Arc<str>>) -> &mut Self {
        self.interface.description = Some(description.into());
        self
    }

    /// Whether this interface already declares a field with `name`.
    ///
    /// Read-only, for extensions that add a field only when it is absent.
    #[must_use]
    pub fn has_field(&self, name: &str) -> bool {
        self.interface
            .fields
            .keys()
            .any(|field| field.as_ref() == name)
    }

    /// Declare that this interface implements another interface.
    ///
    /// Both `node` and `&node` are accepted.
    pub fn implements(&mut self, interface: impl Into<InterfaceRef>) -> &mut Self {
        let interface = interface.into();
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
    /// another interface. It composes with object-level and field-level checks. A rule declared
    /// when the interface is reopened adds to the existing rules rather than replacing them.
    pub fn auth(&mut self, predicate: impl Fn(&Ctx) -> bool + Send + Sync + 'static) -> &mut Self {
        self.auth
            .push(crate::field::sync_guard(predicate, self.auth_error.clone()));
        self
    }

    /// Like [`InterfaceBuilder::auth`], but the predicate is asynchronous.
    pub fn auth_async(
        &mut self,
        predicate: impl for<'r> Fn(&'r Ctx) -> BoxFuture<'r, bool> + Send + Sync + 'static,
    ) -> &mut Self {
        self.auth.push(crate::field::async_guard(
            predicate,
            self.auth_error.clone(),
        ));
        self
    }

    /// Set the message returned when the interface-level authorization denies access.
    pub fn auth_error(&mut self, message: impl Into<Arc<str>>) -> &mut Self {
        self.auth_error = Some(message.into());
        if let Some(auth) = self.auth.last_mut() {
            *auth = auth.with_message(self.auth_error.clone());
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

    /// Add a field whose output type is a runtime-named object.
    ///
    /// `T` supplies the list and nullability wrappers; `reference` supplies the name. Use it for
    /// interface fields that return a dynamically named type such as a connection.
    pub fn field_ref<T, N>(
        &mut self,
        reference: &ObjectRef<N>,
        name: &'static str,
    ) -> InterfaceFieldConfig<'_, Ctx>
    where
        T: OutputType,
        N: ObjectModel,
    {
        self.interface
            .insert_field(gql::Field::new(name, T::output_type()));
        let mut config = FieldConfig::for_interface(
            self.registry,
            &mut self.interface.fields,
            &mut self.field_auth,
            Arc::from(name),
            T::is_optional(),
            std::any::type_name::<T>(),
        );
        config.as_object_ref(reference);
        InterfaceFieldConfig::new(config)
    }

    /// Add a field with an explicitly built runtime type reference.
    pub fn field_ref_type<T: OutputType>(
        &mut self,
        ty: TypeRef,
        name: &'static str,
    ) -> InterfaceFieldConfig<'_, Ctx> {
        self.interface
            .insert_field(gql::Field::new(name, T::output_type()));
        let mut config = FieldConfig::for_interface(
            self.registry,
            &mut self.interface.fields,
            &mut self.field_auth,
            Arc::from(name),
            T::is_optional(),
            std::any::type_name::<T>(),
        );
        config.as_dynamic(ty);
        InterfaceFieldConfig::new(config)
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
pub struct UnionBuilder<'a, Ctx: 'static> {
    union: gql::UnionType,
    registry: &'a mut TypeRegistry,
    marker: PhantomData<fn() -> Ctx>,
}

impl<'a, Ctx: 'static> UnionBuilder<'a, Ctx> {
    fn new(existing: Option<gql::UnionType>, name: Name, registry: &'a mut TypeRegistry) -> Self {
        Self {
            union: existing.unwrap_or_else(|| gql::UnionType::new(name)),
            registry,
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
    ///
    /// The model is registered as referenced, so a missing `schema.object::<M>(...)` gives the
    /// usual "was never registered" build error instead of `gql` failing to find the name.
    pub fn member<M: ObjectModel>(&mut self) -> &mut Self {
        self.registry.reference_object(M::NAME);
        let name: Name = Arc::from(M::NAME);
        if !self.union.types.iter().any(|existing| **existing == *name) {
            self.union.types.push(name);
        }
        self
    }

    /// Add an object created with a runtime name as a union member.
    ///
    /// This is [`UnionBuilder::member`] for a [`Schema::object_ref`] definition: the reference's
    /// dynamic name becomes the member name, and the model still supplies `is_type_of`.
    pub fn member_ref<M: ObjectModel>(&mut self, reference: &ObjectRef<M>) -> &mut Self {
        self.registry.reference_object(reference.name());
        let name: Name = Arc::from(reference.name());
        if !self.union.types.iter().any(|existing| **existing == *name) {
            self.union.types.push(name);
        }
        self
    }

    /// Add an object type by GraphQL name, without a Rust model.
    ///
    /// The name must match an object registered with [`Schema::object_ref`]; this is the
    /// escape hatch for members whose model is not in scope.
    pub fn member_name(&mut self, name: impl Into<Name>) -> &mut Self {
        let name = name.into();
        self.registry.reference_object(Arc::clone(&name));
        if !self
            .union
            .types
            .iter()
            .any(|existing| existing.as_ref() == name.as_ref())
        {
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

impl<Ctx> std::fmt::Debug for UnionBuilder<'_, Ctx> {
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

    /// Add a synchronous root field whose output type is a runtime-named object.
    ///
    /// `reference` supplies the GraphQL name; `T` supplies the list and nullability wrappers.
    pub fn field_ref<T, A, F, N>(
        &mut self,
        reference: &ObjectRef<N>,
        name: &'static str,
        resolver: F,
    ) -> FieldConfig<'_, Ctx>
    where
        N: ObjectModel,
        T: OutputType + IntoValue + Send + 'static,
        A: InputObject,
        F: Fn(A, &Ctx) -> T + Send + Sync + 'static,
    {
        let (field, validation) = root_field_with_info::<Ctx, T, A, _>(
            name,
            move |args: A, ctx: &Ctx, _| resolver(args, ctx),
            self.registry,
            false,
        );
        let mut config = self.push(
            field,
            T::is_optional(),
            std::any::type_name::<T>(),
            TypeId::of::<A>(),
            validation,
        );
        config.as_object_ref(reference);
        config
    }

    /// Add a synchronous root field with an explicitly built runtime type reference.
    pub fn field_ref_type<T, A, F>(
        &mut self,
        ty: TypeRef,
        name: &'static str,
        resolver: F,
    ) -> FieldConfig<'_, Ctx>
    where
        T: OutputType + IntoValue + Send + 'static,
        A: InputObject,
        F: Fn(A, &Ctx) -> T + Send + Sync + 'static,
    {
        let (field, validation) = root_field_with_info::<Ctx, T, A, _>(
            name,
            move |args: A, ctx: &Ctx, _| resolver(args, ctx),
            self.registry,
            false,
        );
        let mut config = self.push(
            field,
            T::is_optional(),
            std::any::type_name::<T>(),
            TypeId::of::<A>(),
            validation,
        );
        config.as_dynamic(ty);
        config
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
        let (field, validation) =
            root_field_with_info::<Ctx, T, A, F>(name, resolver, self.registry, true);
        self.push(
            field,
            T::is_optional(),
            std::any::type_name::<T>(),
            TypeId::of::<A>(),
            validation,
        )
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
        let (field, validation) =
            root_field_result_with_info::<Ctx, T, A, F>(name, resolver, self.registry);
        self.push(
            field,
            T::is_optional(),
            std::any::type_name::<T>(),
            TypeId::of::<A>(),
            validation,
        )
    }

    /// Add a synchronous root field whose resolver returns a typed `Result`.
    ///
    /// The field's type is a generated `<Parent><Field>Result` union, as described on
    /// [`ObjectBuilder::field_result_union`]. The parent name is the configured query, mutation
    /// or subscription root name.
    pub fn field_result_union<T, E, A, F>(
        &mut self,
        name: &'static str,
        resolver: F,
    ) -> FieldConfig<'_, Ctx>
    where
        T: OutputType + IntoValue + Send + 'static,
        E: ErrorUnionModel,
        A: InputObject,
        F: Fn(A, &Ctx) -> core::result::Result<T, E> + Send + Sync + 'static,
    {
        self.field_result_union_with_info::<T, E, A, _>(name, move |args: A, ctx: &Ctx, _| {
            resolver(args, ctx)
        })
    }

    /// Add a synchronous typed-result root field whose resolver also receives
    /// [`gql::ResolveInfo`].
    pub fn field_result_union_with_info<T, E, A, F>(
        &mut self,
        name: &'static str,
        resolver: F,
    ) -> FieldConfig<'_, Ctx>
    where
        T: OutputType + IntoValue + Send + 'static,
        E: ErrorUnionModel,
        A: InputObject,
        F: for<'r> Fn(A, &'r Ctx, &'r gql::ResolveInfo<'r>) -> core::result::Result<T, E>
            + Send
            + Sync
            + 'static,
    {
        let (field, validation) = root_field_result_union_with_info::<Ctx, T, E, A, F>(
            name,
            resolver,
            self.registry,
            &self.owner,
        );
        self.push(
            field,
            false,
            std::any::type_name::<T>(),
            TypeId::of::<A>(),
            validation,
        )
    }

    /// Add an asynchronous typed-result root field.
    pub fn field_result_union_async<T, E, A, F>(
        &mut self,
        name: &'static str,
        resolver: F,
    ) -> FieldConfig<'_, Ctx>
    where
        T: OutputType + IntoValue + Send + 'static,
        E: ErrorUnionModel,
        A: InputObject,
        F: for<'r> Fn(A, &'r Ctx) -> BoxFuture<'r, core::result::Result<T, E>>
            + Send
            + Sync
            + 'static,
    {
        self.field_result_union_async_with_info::<T, E, A, _>(name, move |args: A, ctx: &Ctx, _| {
            resolver(args, ctx)
        })
    }

    /// Add an asynchronous typed-result root field whose resolver also receives
    /// [`gql::ResolveInfo`].
    pub fn field_result_union_async_with_info<T, E, A, F>(
        &mut self,
        name: &'static str,
        resolver: F,
    ) -> FieldConfig<'_, Ctx>
    where
        T: OutputType + IntoValue + Send + 'static,
        E: ErrorUnionModel,
        A: InputObject,
        F: for<'r> Fn(
                A,
                &'r Ctx,
                &'r gql::ResolveInfo<'r>,
            ) -> BoxFuture<'r, core::result::Result<T, E>>
            + Send
            + Sync
            + 'static,
    {
        let (field, validation) = root_field_result_union_async_with_info::<Ctx, T, E, A, F>(
            name,
            resolver,
            self.registry,
            &self.owner,
        );
        self.push(
            field,
            false,
            std::any::type_name::<T>(),
            TypeId::of::<A>(),
            validation,
        )
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
        let (field, validation) =
            root_field_async_with_info::<Ctx, T, A, F>(name, resolver, self.registry);
        self.push(
            field,
            T::is_optional(),
            std::any::type_name::<T>(),
            TypeId::of::<A>(),
            validation,
        )
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
        let (field, validation) =
            root_field_result_async_with_info::<Ctx, T, A, F>(name, resolver, self.registry);
        self.push(
            field,
            T::is_optional(),
            std::any::type_name::<T>(),
            TypeId::of::<A>(),
            validation,
        )
    }

    /// Add a root field whose resolver also receives the executor's [`gql::ResolverContext`].
    ///
    /// Use it to set response extensions with `execution.context.set_extension("key", value)`.
    pub fn field_with_execution<T, A, F>(
        &mut self,
        name: &'static str,
        resolver: F,
    ) -> FieldConfig<'_, Ctx>
    where
        T: OutputType + IntoValue + Send + 'static,
        A: InputObject,
        F: for<'r> Fn(A, &'r Ctx, &'r gql::ResolverContext<'r>) -> T + Send + Sync + 'static,
    {
        let (field, validation) =
            root_field_with_execution::<Ctx, T, A, F>(name, resolver, self.registry);
        self.push(
            field,
            T::is_optional(),
            std::any::type_name::<T>(),
            TypeId::of::<A>(),
            validation,
        )
    }

    /// Like [`RootBuilder::field_with_execution`], but the resolver returns a `Result`.
    pub fn field_result_with_execution<T, A, F>(
        &mut self,
        name: &'static str,
        resolver: F,
    ) -> FieldConfig<'_, Ctx>
    where
        T: OutputType + IntoValue + Send + 'static,
        A: InputObject,
        F: for<'r> Fn(A, &'r Ctx, &'r gql::ResolverContext<'r>) -> Result<T, GraphQLError>
            + Send
            + Sync
            + 'static,
    {
        let (field, validation) =
            root_field_result_with_execution::<Ctx, T, A, F>(name, resolver, self.registry);
        self.push(
            field,
            T::is_optional(),
            std::any::type_name::<T>(),
            TypeId::of::<A>(),
            validation,
        )
    }

    fn push(
        &mut self,
        field: gql::Field,
        rust_nullable: bool,
        rust_type_name: &'static str,
        args_type: TypeId,
        validation: ValidationSlot<Ctx>,
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
            args_type,
            validation,
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

    /// Add a subscription field whose output type is a runtime-named object.
    ///
    /// The stream yields the object's model; `reference` supplies the GraphQL name and `T`
    /// supplies the list and nullability wrappers.
    pub fn field_ref<A, T, F, S, N>(
        &mut self,
        reference: &ObjectRef<N>,
        name: &'static str,
        subscribe: F,
    ) -> FieldConfig<'_, Ctx>
    where
        A: InputObject,
        N: ObjectModel,
        T: OutputType + IntoValue + Send + 'static,
        S: Stream<Item = Result<T, GraphQLError>> + Send + 'static,
        F: Fn(A, &Ctx) -> S + Send + Sync + 'static,
    {
        let (field, validation) = subscription_field::<Ctx, A, T, _, S>(
            name,
            move |args: A, ctx: &Ctx, _| subscribe(args, ctx),
            self.registry,
            false,
        );
        let mut config = self.push(
            field,
            T::is_optional(),
            std::any::type_name::<T>(),
            TypeId::of::<A>(),
            validation,
        );
        config.as_object_ref(reference);
        config
    }

    /// Add a subscription field with an explicitly built runtime type reference.
    pub fn field_ref_type<A, T, F, S>(
        &mut self,
        ty: TypeRef,
        name: &'static str,
        subscribe: F,
    ) -> FieldConfig<'_, Ctx>
    where
        A: InputObject,
        T: OutputType + IntoValue + Send + 'static,
        S: Stream<Item = Result<T, GraphQLError>> + Send + 'static,
        F: Fn(A, &Ctx) -> S + Send + Sync + 'static,
    {
        let (field, validation) = subscription_field::<Ctx, A, T, _, S>(
            name,
            move |args: A, ctx: &Ctx, _| subscribe(args, ctx),
            self.registry,
            false,
        );
        let mut config = self.push(
            field,
            T::is_optional(),
            std::any::type_name::<T>(),
            TypeId::of::<A>(),
            validation,
        );
        config.as_dynamic(ty);
        config
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
        let (field, validation) =
            subscription_field::<Ctx, A, T, F, S>(name, subscribe, self.registry, true);
        self.push(
            field,
            T::is_optional(),
            std::any::type_name::<T>(),
            TypeId::of::<A>(),
            validation,
        )
    }

    /// Add a subscription whose events are typed results.
    ///
    /// Each stream item is either `Ok(T)` (the generated success payload with `data: T`) or
    /// `Err(E)` (the matching error payload object), exactly like
    /// [`ObjectBuilder::field_result_union`]. The field type is the generated result union, so
    /// a client can select error payload fields from an event.
    pub fn field_result_union<T, E, A, F, S>(
        &mut self,
        name: &'static str,
        subscribe: F,
    ) -> FieldConfig<'_, Ctx>
    where
        A: InputObject,
        T: OutputType + IntoValue + Send + 'static,
        E: ErrorUnionModel,
        S: Stream<Item = core::result::Result<T, E>> + Send + 'static,
        F: Fn(A, &Ctx) -> S + Send + Sync + 'static,
    {
        self.field_result_union_with_info::<T, E, A, _, S>(name, move |args: A, ctx: &Ctx, _| {
            subscribe(args, ctx)
        })
    }

    /// Add a typed-result subscription whose `subscribe` also receives [`gql::ResolveInfo`].
    pub fn field_result_union_with_info<T, E, A, F, S>(
        &mut self,
        name: &'static str,
        subscribe: F,
    ) -> FieldConfig<'_, Ctx>
    where
        A: InputObject,
        T: OutputType + IntoValue + Send + 'static,
        E: ErrorUnionModel,
        S: Stream<Item = core::result::Result<T, E>> + Send + 'static,
        F: for<'r> Fn(A, &'r Ctx, &'r gql::ResolveInfo<'r>) -> S + Send + Sync + 'static,
    {
        let (field, validation) = subscription_field_result_union::<Ctx, T, E, A, F, S>(
            name,
            subscribe,
            self.registry,
            &self.owner,
        );
        self.push(
            field,
            false,
            std::any::type_name::<T>(),
            TypeId::of::<A>(),
            validation,
        )
    }

    /// Add a subscription whose events are mapped to the field value by a resolver.
    ///
    /// This is the Pothos-style form: the stream produces an event `E`, and `resolve` turns each
    /// event into the field's value `T`. Use it when the event carries more than the field's
    /// own value (an envelope with metadata, or several correlated updates).
    ///
    /// ```
    /// use futures::stream;
    /// use gurafi::{NoArgs, Schema};
    ///
    /// struct Ctx;
    ///
    /// #[derive(Clone)]
    /// struct BornEvent {
    ///     name: String,
    /// }
    ///
    /// let mut schema = Schema::<Ctx>::new();
    /// schema.query(|q| {
    ///     q.field("ok", |_: NoArgs, _: &Ctx| true);
    /// });
    /// schema.subscription(|s| {
    ///     s.field_map::<NoArgs, BornEvent, String, _, _, _>(
    ///         "giraffeBorn",
    ///         |_: NoArgs, _: &Ctx| {
    ///             stream::iter(vec![Ok(BornEvent {
    ///                 name: "Melman".into(),
    ///             })])
    ///         },
    ///         |event: &BornEvent, _: NoArgs, _: &Ctx| Ok(event.name.clone()),
    ///     );
    /// });
    /// let schema = schema.build().expect("schema builds");
    /// # let _ = schema;
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
        let validation = ValidationSlot::new();
        let resolve_validation = validation.clone();
        let subscribe = Arc::new(subscribe);
        let resolve = Arc::new(resolve);
        let field = add_arguments::<A>(gql::Field::new(name, T::output_type()), self.registry)
            .resolve(move |context| {
                let resolve = Arc::clone(&resolve);
                let validation = resolve_validation.clone();
                Box::pin(async move {
                    let event = context.parent.as_external::<E>().ok_or_else(|| {
                        GraphQLError::new(format!(
                            "Subscription event for \"{}\" had an unexpected type.",
                            context.info.field_name
                        ))
                    })?;
                    // The subscription already validated the input once. Per-event resolution
                    // converts the same coerced arguments without re-running input-level
                    // validation, then runs the field validator exactly once per event.
                    let args = coerce_args_without_validation::<A>(
                        context.args,
                        context.info.parent_type,
                        context.info.field_name,
                    )?;
                    let ctx = context.context.require_data::<Ctx>()?;
                    validation.run(&args, ctx)?;
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
        self.push(
            field,
            T::is_optional(),
            std::any::type_name::<T>(),
            TypeId::of::<A>(),
            validation,
        )
    }

    fn push(
        &mut self,
        field: gql::Field,
        rust_nullable: bool,
        rust_type_name: &'static str,
        args_type: TypeId,
        validation: ValidationSlot<Ctx>,
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
            args_type,
            validation,
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
    if A::one_of() {
        registry.push_error(GraphQLError::new(format!(
            "`{}` is a `@oneOf` input and cannot be used directly as field `{}`'s arguments. Wrap \
             it in an arguments type so it stays one argument: `struct Args {{ by: {} }}`.",
            std::any::type_name::<A>(),
            field.name,
            std::any::type_name::<A>(),
        )));
    }
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
) -> (gql::Field, ValidationSlot<Ctx>)
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
        true,
    )
}

fn object_field_with_info<M, Ctx, T, A, F>(
    name: &'static str,
    resolver: F,
    registry: &mut TypeRegistry,
    register_output: bool,
) -> (gql::Field, ValidationSlot<Ctx>)
where
    M: ObjectModel,
    Ctx: 'static,
    T: OutputType + IntoValue + Send + 'static,
    A: InputObject,
    F: for<'r> Fn(&'r M, A, &'r Ctx, &'r gql::ResolveInfo<'r>) -> T + Send + Sync + 'static,
{
    if register_output {
        T::register_output(registry);
    }
    A::register_args(registry);
    let resolver = Arc::new(resolver);
    let validation = ValidationSlot::new();
    let hook = validation.clone();
    let field = add_arguments::<A>(gql::Field::new(name, T::output_type()), registry).resolve(
        move |context| {
            let resolver = Arc::clone(&resolver);
            let hook = hook.clone();
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
                hook.run(&args, ctx)?;
                (resolver)(model, args, ctx, context.info).into_value()
            })
        },
    );
    (field, validation)
}

fn object_field_result_with_info<M, Ctx, T, A, F>(
    name: &'static str,
    resolver: F,
    registry: &mut TypeRegistry,
) -> (gql::Field, ValidationSlot<Ctx>)
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
    let validation = ValidationSlot::new();
    let hook = validation.clone();
    let field = add_arguments::<A>(gql::Field::new(name, T::output_type()), registry).resolve(
        move |context| {
            let resolver = Arc::clone(&resolver);
            let hook = hook.clone();
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
                hook.run(&args, ctx)?;
                match (resolver)(model, args, ctx, context.info) {
                    Ok(value) => value.into_value(),
                    Err(error) => Err(error),
                }
            })
        },
    );
    (field, validation)
}

fn object_field_result_union_with_info<M, Ctx, T, E, A, F>(
    name: &'static str,
    resolver: F,
    registry: &mut TypeRegistry,
    parent: &str,
) -> (gql::Field, ValidationSlot<Ctx>)
where
    M: ObjectModel,
    Ctx: 'static,
    T: OutputType + IntoValue + Send + 'static,
    E: ErrorUnionModel,
    A: InputObject,
    F: for<'r> Fn(&'r M, A, &'r Ctx, &'r gql::ResolveInfo<'r>) -> core::result::Result<T, E>
        + Send
        + Sync
        + 'static,
{
    let ty = result_union_type_ref::<T, E>(registry, parent, name);
    A::register_args(registry);
    let resolver = Arc::new(resolver);
    let validation = ValidationSlot::new();
    let hook = validation.clone();
    let field = add_arguments::<A>(gql::Field::new(name, ty), registry).resolve(move |context| {
        let resolver = Arc::clone(&resolver);
        let hook = hook.clone();
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
            hook.run(&args, ctx)?;
            match (resolver)(model, args, ctx, context.info) {
                Ok(value) => value
                    .into_value()
                    .map(|data| gql::Value::external(ResultSuccess::new(data))),
                Err(error) => error.into_value(),
            }
        })
    });
    (field, validation)
}

fn object_field_result_union_async_with_info<M, Ctx, T, E, A, F>(
    name: &'static str,
    resolver: F,
    registry: &mut TypeRegistry,
    parent: &str,
) -> (gql::Field, ValidationSlot<Ctx>)
where
    M: ObjectModel,
    Ctx: 'static,
    T: OutputType + IntoValue + Send + 'static,
    E: ErrorUnionModel,
    A: InputObject,
    F: for<'r> Fn(
            &'r M,
            A,
            &'r Ctx,
            &'r gql::ResolveInfo<'r>,
        ) -> BoxFuture<'r, core::result::Result<T, E>>
        + Send
        + Sync
        + 'static,
{
    let ty = result_union_type_ref::<T, E>(registry, parent, name);
    A::register_args(registry);
    let resolver = Arc::new(resolver);
    let validation = ValidationSlot::new();
    let hook = validation.clone();
    let field = add_arguments::<A>(gql::Field::new(name, ty), registry).resolve(move |context| {
        let resolver = Arc::clone(&resolver);
        let hook = hook.clone();
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
            hook.run(&args, ctx)?;
            match (resolver)(model, args, ctx, context.info).await {
                Ok(value) => value
                    .into_value()
                    .map(|data| gql::Value::external(ResultSuccess::new(data))),
                Err(error) => error.into_value(),
            }
        })
    });
    (field, validation)
}

fn object_field_async_with_info<M, Ctx, T, A, F>(
    name: &'static str,
    resolver: F,
    registry: &mut TypeRegistry,
) -> (gql::Field, ValidationSlot<Ctx>)
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
    let validation = ValidationSlot::new();
    let hook = validation.clone();
    let field = add_arguments::<A>(gql::Field::new(name, T::output_type()), registry).resolve(
        move |context| {
            let resolver = Arc::clone(&resolver);
            let hook = hook.clone();
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
                hook.run(&args, ctx)?;
                (resolver)(model, args, ctx, context.info)
                    .await
                    .into_value()
            })
        },
    );
    (field, validation)
}

fn object_field_result_async_with_info<M, Ctx, T, A, F>(
    name: &'static str,
    resolver: F,
    registry: &mut TypeRegistry,
) -> (gql::Field, ValidationSlot<Ctx>)
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
    let validation = ValidationSlot::new();
    let hook = validation.clone();
    let field = add_arguments::<A>(gql::Field::new(name, T::output_type()), registry).resolve(
        move |context| {
            let resolver = Arc::clone(&resolver);
            let hook = hook.clone();
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
                hook.run(&args, ctx)?;
                match (resolver)(model, args, ctx, context.info).await {
                    Ok(value) => value.into_value(),
                    Err(error) => Err(error),
                }
            })
        },
    );
    (field, validation)
}

fn object_field_with_execution<M, Ctx, T, A, F>(
    name: &'static str,
    resolver: F,
    registry: &mut TypeRegistry,
) -> (gql::Field, ValidationSlot<Ctx>)
where
    M: ObjectModel,
    Ctx: 'static,
    T: OutputType + IntoValue + Send + 'static,
    A: InputObject,
    F: for<'r> Fn(&'r M, A, &'r Ctx, &'r gql::ResolverContext<'r>) -> T + Send + Sync + 'static,
{
    T::register_output(registry);
    A::register_args(registry);
    let resolver = Arc::new(resolver);
    let validation = ValidationSlot::new();
    let hook = validation.clone();
    let field = add_arguments::<A>(gql::Field::new(name, T::output_type()), registry).resolve(
        move |context| {
            let resolver = Arc::clone(&resolver);
            let hook = hook.clone();
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
                hook.run(&args, ctx)?;
                (resolver)(model, args, ctx, &context).into_value()
            })
        },
    );
    (field, validation)
}

fn object_field_result_with_execution<M, Ctx, T, A, F>(
    name: &'static str,
    resolver: F,
    registry: &mut TypeRegistry,
) -> (gql::Field, ValidationSlot<Ctx>)
where
    M: ObjectModel,
    Ctx: 'static,
    T: OutputType + IntoValue + Send + 'static,
    A: InputObject,
    F: for<'r> Fn(&'r M, A, &'r Ctx, &'r gql::ResolverContext<'r>) -> Result<T, GraphQLError>
        + Send
        + Sync
        + 'static,
{
    T::register_output(registry);
    A::register_args(registry);
    let resolver = Arc::new(resolver);
    let validation = ValidationSlot::new();
    let hook = validation.clone();
    let field = add_arguments::<A>(gql::Field::new(name, T::output_type()), registry).resolve(
        move |context| {
            let resolver = Arc::clone(&resolver);
            let hook = hook.clone();
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
                hook.run(&args, ctx)?;
                match (resolver)(model, args, ctx, &context) {
                    Ok(value) => value.into_value(),
                    Err(error) => Err(error),
                }
            })
        },
    );
    (field, validation)
}

fn root_field_with_info<Ctx, T, A, F>(
    name: &'static str,
    resolver: F,
    registry: &mut TypeRegistry,
    register_output: bool,
) -> (gql::Field, ValidationSlot<Ctx>)
where
    Ctx: 'static,
    T: OutputType + IntoValue + Send + 'static,
    A: InputObject,
    F: for<'r> Fn(A, &'r Ctx, &'r gql::ResolveInfo<'r>) -> T + Send + Sync + 'static,
{
    if register_output {
        T::register_output(registry);
    }
    A::register_args(registry);
    let resolver = Arc::new(resolver);
    let validation = ValidationSlot::new();
    let hook = validation.clone();
    let field = add_arguments::<A>(gql::Field::new(name, T::output_type()), registry).resolve(
        move |context| {
            let resolver = Arc::clone(&resolver);
            let hook = hook.clone();
            Box::pin(async move {
                let args = coerce_args::<A>(
                    context.args,
                    context.info.parent_type,
                    context.info.field_name,
                )?;
                let ctx = context.context.require_data::<Ctx>()?;
                hook.run(&args, ctx)?;
                (resolver)(args, ctx, context.info).into_value()
            })
        },
    );
    (field, validation)
}

fn root_field_result_with_info<Ctx, T, A, F>(
    name: &'static str,
    resolver: F,
    registry: &mut TypeRegistry,
) -> (gql::Field, ValidationSlot<Ctx>)
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
    let validation = ValidationSlot::new();
    let hook = validation.clone();
    let field = add_arguments::<A>(gql::Field::new(name, T::output_type()), registry).resolve(
        move |context| {
            let resolver = Arc::clone(&resolver);
            let hook = hook.clone();
            Box::pin(async move {
                let args = coerce_args::<A>(
                    context.args,
                    context.info.parent_type,
                    context.info.field_name,
                )?;
                let ctx = context.context.require_data::<Ctx>()?;
                hook.run(&args, ctx)?;
                match (resolver)(args, ctx, context.info) {
                    Ok(value) => value.into_value(),
                    Err(error) => Err(error),
                }
            })
        },
    );
    (field, validation)
}

fn root_field_result_union_with_info<Ctx, T, E, A, F>(
    name: &'static str,
    resolver: F,
    registry: &mut TypeRegistry,
    parent: &str,
) -> (gql::Field, ValidationSlot<Ctx>)
where
    Ctx: 'static,
    T: OutputType + IntoValue + Send + 'static,
    E: ErrorUnionModel,
    A: InputObject,
    F: for<'r> Fn(A, &'r Ctx, &'r gql::ResolveInfo<'r>) -> core::result::Result<T, E>
        + Send
        + Sync
        + 'static,
{
    let ty = result_union_type_ref::<T, E>(registry, parent, name);
    A::register_args(registry);
    let resolver = Arc::new(resolver);
    let validation = ValidationSlot::new();
    let hook = validation.clone();
    let field = add_arguments::<A>(gql::Field::new(name, ty), registry).resolve(move |context| {
        let resolver = Arc::clone(&resolver);
        let hook = hook.clone();
        Box::pin(async move {
            let args = coerce_args::<A>(
                context.args,
                context.info.parent_type,
                context.info.field_name,
            )?;
            let ctx = context.context.require_data::<Ctx>()?;
            hook.run(&args, ctx)?;
            match (resolver)(args, ctx, context.info) {
                Ok(value) => value
                    .into_value()
                    .map(|data| gql::Value::external(ResultSuccess::new(data))),
                Err(error) => error.into_value(),
            }
        })
    });
    (field, validation)
}

fn root_field_result_union_async_with_info<Ctx, T, E, A, F>(
    name: &'static str,
    resolver: F,
    registry: &mut TypeRegistry,
    parent: &str,
) -> (gql::Field, ValidationSlot<Ctx>)
where
    Ctx: 'static,
    T: OutputType + IntoValue + Send + 'static,
    E: ErrorUnionModel,
    A: InputObject,
    F: for<'r> Fn(
            A,
            &'r Ctx,
            &'r gql::ResolveInfo<'r>,
        ) -> BoxFuture<'r, core::result::Result<T, E>>
        + Send
        + Sync
        + 'static,
{
    let ty = result_union_type_ref::<T, E>(registry, parent, name);
    A::register_args(registry);
    let resolver = Arc::new(resolver);
    let validation = ValidationSlot::new();
    let hook = validation.clone();
    let field = add_arguments::<A>(gql::Field::new(name, ty), registry).resolve(move |context| {
        let resolver = Arc::clone(&resolver);
        let hook = hook.clone();
        Box::pin(async move {
            let args = coerce_args::<A>(
                context.args,
                context.info.parent_type,
                context.info.field_name,
            )?;
            let ctx = context.context.require_data::<Ctx>()?;
            hook.run(&args, ctx)?;
            match (resolver)(args, ctx, context.info).await {
                Ok(value) => value
                    .into_value()
                    .map(|data| gql::Value::external(ResultSuccess::new(data))),
                Err(error) => error.into_value(),
            }
        })
    });
    (field, validation)
}

fn root_field_async_with_info<Ctx, T, A, F>(
    name: &'static str,
    resolver: F,
    registry: &mut TypeRegistry,
) -> (gql::Field, ValidationSlot<Ctx>)
where
    Ctx: 'static,
    T: OutputType + IntoValue + Send + 'static,
    A: InputObject,
    F: for<'r> Fn(A, &'r Ctx, &'r gql::ResolveInfo<'r>) -> BoxFuture<'r, T> + Send + Sync + 'static,
{
    T::register_output(registry);
    A::register_args(registry);
    let resolver = Arc::new(resolver);
    let validation = ValidationSlot::new();
    let hook = validation.clone();
    let field = add_arguments::<A>(gql::Field::new(name, T::output_type()), registry).resolve(
        move |context| {
            let resolver = Arc::clone(&resolver);
            let hook = hook.clone();
            Box::pin(async move {
                let args = coerce_args::<A>(
                    context.args,
                    context.info.parent_type,
                    context.info.field_name,
                )?;
                let ctx = context.context.require_data::<Ctx>()?;
                hook.run(&args, ctx)?;
                (resolver)(args, ctx, context.info).await.into_value()
            })
        },
    );
    (field, validation)
}

fn root_field_result_async_with_info<Ctx, T, A, F>(
    name: &'static str,
    resolver: F,
    registry: &mut TypeRegistry,
) -> (gql::Field, ValidationSlot<Ctx>)
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
    let validation = ValidationSlot::new();
    let hook = validation.clone();
    let field = add_arguments::<A>(gql::Field::new(name, T::output_type()), registry).resolve(
        move |context| {
            let resolver = Arc::clone(&resolver);
            let hook = hook.clone();
            Box::pin(async move {
                let args = coerce_args::<A>(
                    context.args,
                    context.info.parent_type,
                    context.info.field_name,
                )?;
                let ctx = context.context.require_data::<Ctx>()?;
                hook.run(&args, ctx)?;
                match (resolver)(args, ctx, context.info).await {
                    Ok(value) => value.into_value(),
                    Err(error) => Err(error),
                }
            })
        },
    );
    (field, validation)
}

fn root_field_with_execution<Ctx, T, A, F>(
    name: &'static str,
    resolver: F,
    registry: &mut TypeRegistry,
) -> (gql::Field, ValidationSlot<Ctx>)
where
    Ctx: 'static,
    T: OutputType + IntoValue + Send + 'static,
    A: InputObject,
    F: for<'r> Fn(A, &'r Ctx, &'r gql::ResolverContext<'r>) -> T + Send + Sync + 'static,
{
    T::register_output(registry);
    A::register_args(registry);
    let resolver = Arc::new(resolver);
    let validation = ValidationSlot::new();
    let hook = validation.clone();
    let field = add_arguments::<A>(gql::Field::new(name, T::output_type()), registry).resolve(
        move |context| {
            let resolver = Arc::clone(&resolver);
            let hook = hook.clone();
            Box::pin(async move {
                let args = coerce_args::<A>(
                    context.args,
                    context.info.parent_type,
                    context.info.field_name,
                )?;
                let ctx = context.context.require_data::<Ctx>()?;
                hook.run(&args, ctx)?;
                (resolver)(args, ctx, &context).into_value()
            })
        },
    );
    (field, validation)
}

fn root_field_result_with_execution<Ctx, T, A, F>(
    name: &'static str,
    resolver: F,
    registry: &mut TypeRegistry,
) -> (gql::Field, ValidationSlot<Ctx>)
where
    Ctx: 'static,
    T: OutputType + IntoValue + Send + 'static,
    A: InputObject,
    F: for<'r> Fn(A, &'r Ctx, &'r gql::ResolverContext<'r>) -> Result<T, GraphQLError>
        + Send
        + Sync
        + 'static,
{
    T::register_output(registry);
    A::register_args(registry);
    let resolver = Arc::new(resolver);
    let validation = ValidationSlot::new();
    let hook = validation.clone();
    let field = add_arguments::<A>(gql::Field::new(name, T::output_type()), registry).resolve(
        move |context| {
            let resolver = Arc::clone(&resolver);
            let hook = hook.clone();
            Box::pin(async move {
                let args = coerce_args::<A>(
                    context.args,
                    context.info.parent_type,
                    context.info.field_name,
                )?;
                let ctx = context.context.require_data::<Ctx>()?;
                hook.run(&args, ctx)?;
                match (resolver)(args, ctx, &context) {
                    Ok(value) => value.into_value(),
                    Err(error) => Err(error),
                }
            })
        },
    );
    (field, validation)
}

fn subscription_field_result_union<Ctx, T, E, A, F, S>(
    name: &'static str,
    subscribe: F,
    registry: &mut TypeRegistry,
    parent: &str,
) -> (gql::Field, ValidationSlot<Ctx>)
where
    Ctx: 'static,
    A: InputObject,
    T: OutputType + IntoValue + Send + 'static,
    E: ErrorUnionModel,
    S: Stream<Item = core::result::Result<T, E>> + Send + 'static,
    F: for<'r> Fn(A, &'r Ctx, &'r gql::ResolveInfo<'r>) -> S + Send + Sync + 'static,
{
    let ty = result_union_type_ref::<T, E>(registry, parent, name);
    A::register_args(registry);
    let subscribe = Arc::new(subscribe);
    let validation = ValidationSlot::new();
    let hook = validation.clone();
    let field = add_arguments::<A>(gql::Field::new(name, ty), registry)
        .resolve(move |context| {
            let hook = hook.clone();
            Box::pin(async move {
                let args = coerce_args_without_validation::<A>(
                    context.args,
                    context.info.parent_type,
                    context.info.field_name,
                )?;
                let ctx = context.context.require_data::<Ctx>()?;
                hook.run(&args, ctx)?;
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
                let mapped = stream.map(move |item| {
                    let value = match item {
                        Ok(value) => value
                            .into_value()
                            .map(|data| gql::Value::external(ResultSuccess::new(data))),
                        Err(error) => error.into_value(),
                    }?;
                    let mut object = gql::Object::new();
                    object.insert(field_name.clone(), value);
                    Ok(gql::Value::Object(Arc::new(object)))
                });
                Ok(Box::pin(mapped) as gql::SubscriptionStream)
            })
        });
    (field, validation)
}

fn subscription_field<Ctx, A, T, F, S>(
    name: &'static str,
    subscribe: F,
    registry: &mut TypeRegistry,
    register_output: bool,
) -> (gql::Field, ValidationSlot<Ctx>)
where
    Ctx: 'static,
    A: InputObject,
    T: OutputType + IntoValue + Send + 'static,
    S: Stream<Item = Result<T, GraphQLError>> + Send + 'static,
    F: for<'r> Fn(A, &'r Ctx, &'r gql::ResolveInfo<'r>) -> S + Send + Sync + 'static,
{
    if register_output {
        T::register_output(registry);
    }
    A::register_args(registry);
    let subscribe = Arc::new(subscribe);
    let validation = ValidationSlot::new();
    let hook = validation.clone();
    let field = add_arguments::<A>(gql::Field::new(name, T::output_type()), registry)
        .resolve(move |context| {
            let hook = hook.clone();
            Box::pin(async move {
                // Arguments were validated once when the subscription started. Conversion still
                // happens per event so the field validator sees the same typed arguments, but
                // input-level validation is not repeated.
                let args = coerce_args_without_validation::<A>(
                    context.args,
                    context.info.parent_type,
                    context.info.field_name,
                )?;
                let ctx = context.context.require_data::<Ctx>()?;
                hook.run(&args, ctx)?;
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
        });
    (field, validation)
}

/// Whether `actual`'s wrappers are at least as strict as `declared`'s.
///
/// `actual` is what the resolver's Rust type can produce; `declared` is what the schema claims.
/// Every non-null position in `declared` must also be non-null in `actual`, because a `None`
/// would violate the schema. Nullable positions may replace non-null ones (loosening), and the
/// list structure must match exactly.
fn wrapper_compatible(actual: &TypeRef, declared: &TypeRef) -> bool {
    match (actual, declared) {
        (TypeRef::NonNull(actual), TypeRef::NonNull(declared)) => {
            wrapper_compatible(actual, declared)
        }
        // A non-null value satisfies a nullable declaration.
        (TypeRef::NonNull(actual), declared) => wrapper_compatible(actual, declared),
        // A value the Rust type may omit cannot satisfy a non-null declaration.
        (_, TypeRef::NonNull(_)) => false,
        (TypeRef::List(actual), TypeRef::List(declared)) => wrapper_compatible(actual, declared),
        (TypeRef::Named(_), TypeRef::Named(_)) => true,
        _ => false,
    }
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
            FieldCheck::DynamicObject {
                owner,
                field,
                original,
                model,
            } => {
                if original.named_type().as_ref() != *model {
                    errors.push(GraphQLError::new(format!(
                        "Field `{owner}.{field}` declares a runtime-named object, but its Rust \
                         type `{original}` is not `{model}`. Resolve the object's model (or a \
                         `Raw<{model}>`), or pass the reference for the model that is returned."
                    )));
                }
            }
            FieldCheck::DynamicType {
                owner,
                field,
                rust,
                declared,
            } => {
                if !wrapper_compatible(rust, declared) {
                    errors.push(GraphQLError::new(format!(
                        "Field `{owner}.{field}` declares its type as `{declared}`, which is not \
                         compatible with the resolver's Rust type `{rust}`. The declared type \
                         must not make a nullable position non-null, and its list wrappers must \
                         match the Rust collection."
                    )));
                    continue;
                }
                let name = declared.named_type();
                match schema.get_type(name) {
                    None => errors.push(GraphQLError::new(format!(
                        "Field `{owner}.{field}` declares `{name}` as its type, but that type is \
                         not in the schema. Register it before calling `build()`."
                    ))),
                    Some(gql::Type::InputObject(_)) => errors.push(GraphQLError::new(format!(
                        "Field `{owner}.{field}` declares `{name}` as its output type, but it is \
                         an input object. Input objects cannot be used in output positions."
                    ))),
                    Some(_) => {}
                }
            }
            FieldCheck::TypeOverride {
                owner,
                field,
                original,
                replacement,
            } => {
                // The override may loosen nullability, but it must never claim a non-null
                // position the resolver can fill with `None`, and it cannot turn a list into a
                // scalar (or vice versa) because the resolver still returns the Rust collection.
                if !wrapper_compatible(original, replacement) {
                    errors.push(GraphQLError::new(format!(
                        "Field `{owner}.{field}` overrides its type from `{original}` to \
                         `{replacement}`, which is not compatible with the resolver's Rust type. \
                         The override must not make a nullable position non-null, and its list \
                         wrappers must match the Rust collection. Remove the `Option` or `Vec` \
                         from the override, or change the resolver's return type."
                    )));
                    continue;
                }
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
            FieldCheck::NonNullItemsOnNonList { owner, field, ty } => {
                errors.push(GraphQLError::new(format!(
                    "Field `{owner}.{field}` calls `non_null_items()`, but its type `{ty}` is not \
                     a list. Use `non_null()` to make the field itself non-null."
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
