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

use std::{marker::PhantomData, sync::Arc};

use futures::{Stream, StreamExt};
use gql::{GraphQLError, TypeRef};
use indexmap::IndexMap;

use crate::{
    error::SchemaError,
    expose::Expose,
    field::{AbstractCheck, FieldConfig, InterfaceFieldConfig},
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
    abstract_checks: Vec<AbstractCheck>,
    marker: PhantomData<fn() -> Ctx>,
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
            abstract_checks: Vec::new(),
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
    /// Registering the same type twice is reported by [`Schema::build`].
    pub fn object<M: ObjectModel>(
        &mut self,
        build: impl FnOnce(&mut ObjectBuilder<'_, Ctx, M>),
    ) -> &mut Self {
        if self.objects.contains_key(M::NAME) {
            self.errors.push(GraphQLError::new(format!(
                "Object type `{}` was registered more than once.",
                M::NAME
            )));
            return self;
        }
        let mut builder =
            ObjectBuilder::<Ctx, M>::new(&mut self.registry, &mut self.abstract_checks, M::NAME);
        build(&mut builder);
        self.objects.insert(M::NAME, builder.finish());
        self
    }

    /// Register an interface type.
    ///
    /// Returns a [`InterfaceRef`] that object builders use with
    /// [`ObjectBuilder::implements`]. Interface fields have no resolvers: execution uses the
    /// concrete object's field resolvers.
    pub fn interface(
        &mut self,
        name: &'static str,
        build: impl FnOnce(&mut InterfaceBuilder<'_, Ctx>),
    ) -> InterfaceRef {
        if self.interfaces.contains_key(name) {
            self.errors.push(GraphQLError::new(format!(
                "Interface type `{name}` was registered more than once."
            )));
            return InterfaceRef { name };
        }
        let mut builder = InterfaceBuilder::<Ctx>::new(&mut self.registry, name);
        build(&mut builder);
        self.interfaces.insert(name, builder.finish());
        InterfaceRef { name }
    }

    /// Register a union type.
    ///
    /// Returns a [`UnionRef`] that fields can use with
    /// [`FieldConfig::as_abstract`], or that [`crate::UnionModel`] output types reference.
    /// Runtime type resolution relies on each object's generated `is_type_of` check, unless the
    /// union installs its own resolver with [`UnionBuilder::resolve_type`].
    pub fn union(
        &mut self,
        name: &'static str,
        build: impl FnOnce(&mut UnionBuilder<Ctx>),
    ) -> UnionRef {
        if self.unions.contains_key(name) {
            self.errors.push(GraphQLError::new(format!(
                "Union type `{name}` was registered more than once."
            )));
            return UnionRef { name };
        }
        let mut builder = UnionBuilder::<Ctx>::new(name);
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

    /// Add a query root field.
    pub fn query(&mut self, build: impl FnOnce(&mut RootBuilder<'_, Ctx>)) -> &mut Self {
        let mut builder = RootBuilder::<Ctx>::new(
            &mut self.registry,
            &mut self.query,
            &mut self.abstract_checks,
            self.query_name.clone(),
        );
        build(&mut builder);
        self
    }

    /// Add a mutation root field.
    pub fn mutation(&mut self, build: impl FnOnce(&mut RootBuilder<'_, Ctx>)) -> &mut Self {
        let mut builder = RootBuilder::<Ctx>::new(
            &mut self.registry,
            &mut self.mutation,
            &mut self.abstract_checks,
            self.mutation_name.clone(),
        );
        build(&mut builder);
        self
    }

    /// Add a subscription root field.
    pub fn subscription(
        &mut self,
        build: impl FnOnce(&mut SubscriptionBuilder<'_, Ctx>),
    ) -> &mut Self {
        let mut builder = SubscriptionBuilder::<Ctx>::new(
            &mut self.registry,
            &mut self.subscription,
            &mut self.abstract_checks,
            self.subscription_name.clone(),
        );
        build(&mut builder);
        self
    }

    /// Validate the schema and freeze it into a [`gql::Schema`].
    ///
    /// # Errors
    ///
    /// Returns every problem found: duplicate type names, fields that reference an object that
    /// was never registered, invalid interface implementations, and so on. See
    /// [`SchemaError`].
    pub fn build(mut self) -> Result<gql::Schema, SchemaError> {
        let mut errors = std::mem::take(&mut self.errors);
        errors.extend(self.registry.take_errors());

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

        let mut builder = gql::Schema::builder();
        if let Some(description) = self.description.clone() {
            builder = builder.description(description);
        }
        for (_, ty) in self.registry.into_types() {
            builder.add_type(ty);
        }
        for (_, object) in self.objects {
            builder.add_type(gql::Type::from(object));
        }
        for (_, interface) in self.interfaces {
            builder.add_type(gql::Type::from(interface));
        }
        for (_, union) in self.unions {
            builder.add_type(gql::Type::from(union));
        }
        if !self.query.is_empty() {
            let mut root = gql::ObjectType::new(self.query_name.clone());
            for field in self.query {
                root = root.field(field);
            }
            builder.add_type(gql::Type::from(root));
            builder = builder.query(self.query_name.clone());
        }
        if !self.mutation.is_empty() {
            let mut root = gql::ObjectType::new(self.mutation_name.clone());
            for field in self.mutation {
                root = root.field(field);
            }
            builder.add_type(gql::Type::from(root));
            builder = builder.mutation(self.mutation_name.clone());
        }
        if !self.subscription.is_empty() {
            let mut root = gql::ObjectType::new(self.subscription_name.clone());
            for field in self.subscription {
                root = root.field(field);
            }
            builder.add_type(gql::Type::from(root));
            builder = builder.subscription(self.subscription_name.clone());
        }

        match builder.build() {
            Ok(schema) => {
                validate_abstract_checks(&schema, &self.abstract_checks, &mut errors);
                if errors.is_empty() {
                    Ok(schema)
                } else {
                    Err(SchemaError::from_errors(errors))
                }
            }
            Err(error) => {
                errors.extend(error.into_errors());
                Err(SchemaError::from_errors(errors))
            }
        }
    }
}

/// Builder for one object type, received by [`Schema::object`].
pub struct ObjectBuilder<'a, Ctx: 'static, M: ObjectModel> {
    registry: &'a mut TypeRegistry,
    object: gql::ObjectType,
    abstract_checks: &'a mut Vec<AbstractCheck>,
    marker: PhantomData<fn() -> (Ctx, M)>,
}

impl<'a, Ctx: 'static, M: ObjectModel> ObjectBuilder<'a, Ctx, M> {
    fn new(
        registry: &'a mut TypeRegistry,
        abstract_checks: &'a mut Vec<AbstractCheck>,
        name: &'static str,
    ) -> Self {
        let mut object = gql::ObjectType::new(name);
        if let Some(description) = M::description() {
            object = object.description(description);
        }
        object = object.is_type_of(|value| value.as_external::<M>().is_some());
        Self {
            registry,
            object,
            abstract_checks,
            marker: PhantomData,
        }
    }

    fn finish(self) -> gql::ObjectType {
        self.object
    }

    /// Set the object's description, overriding the one from the derive.
    pub fn description(&mut self, description: impl Into<Arc<str>>) -> &mut Self {
        self.object.description = Some(description.into());
        self
    }

    /// Declare that this object implements an interface.
    pub fn implements(&mut self, interface: InterfaceRef) -> &mut Self {
        self.object.interfaces.push(Arc::from(interface.name()));
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
        self.push(field)
    }

    /// Add a synchronous field whose resolver cannot fail.
    pub fn field<T, A, F>(&mut self, name: &'static str, resolver: F) -> FieldConfig<'_, Ctx>
    where
        T: OutputType + IntoValue + Send + 'static,
        A: InputObject,
        F: Fn(&M, A, &Ctx) -> T + Send + Sync + 'static,
    {
        let field = object_field::<M, Ctx, T, A, F>(name, resolver, self.registry);
        self.push(field)
    }

    /// Add a synchronous field whose resolver returns a `Result`.
    pub fn field_result<T, A, F>(&mut self, name: &'static str, resolver: F) -> FieldConfig<'_, Ctx>
    where
        T: OutputType + IntoValue + Send + 'static,
        A: InputObject,
        F: Fn(&M, A, &Ctx) -> Result<T, GraphQLError> + Send + Sync + 'static,
    {
        let field = object_field_result::<M, Ctx, T, A, F>(name, resolver, self.registry);
        self.push(field)
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
        let field = object_field_async::<M, Ctx, T, A, F>(name, resolver, self.registry);
        self.push(field)
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
        let field = object_field_result_async::<M, Ctx, T, A, F>(name, resolver, self.registry);
        self.push(field)
    }

    fn push(&mut self, field: gql::Field) -> FieldConfig<'_, Ctx> {
        let name = field.name.clone();
        self.object.insert_field(field);
        FieldConfig::for_object_checked(
            &mut self.object.fields,
            self.abstract_checks,
            M::NAME,
            name,
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
    marker: PhantomData<fn() -> Ctx>,
}

impl<'a, Ctx: 'static> InterfaceBuilder<'a, Ctx> {
    fn new(registry: &'a mut TypeRegistry, name: &'static str) -> Self {
        Self {
            registry,
            interface: gql::InterfaceType::new(name),
            marker: PhantomData,
        }
    }

    fn finish(self) -> gql::InterfaceType {
        self.interface
    }

    /// Set the interface's description.
    pub fn description(&mut self, description: impl Into<Arc<str>>) -> &mut Self {
        self.interface.description = Some(description.into());
        self
    }

    /// Declare that this interface implements another interface.
    pub fn implements(&mut self, interface: InterfaceRef) -> &mut Self {
        self.interface.interfaces.push(Arc::from(interface.name()));
        self
    }

    /// Add a field with no arguments.
    ///
    /// Interface fields have no resolver: the concrete objects supply one. The field's type must
    /// be a covariant match for every implementing object's field.
    pub fn field<T: OutputType>(&mut self, name: &'static str) -> InterfaceFieldConfig<'_, Ctx> {
        T::register_output(self.registry);
        self.interface
            .insert_field(gql::Field::new(name, T::output_type()));
        InterfaceFieldConfig::new(FieldConfig::for_object(
            &mut self.interface.fields,
            Arc::from(name),
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
        InterfaceFieldConfig::new(FieldConfig::for_object(
            &mut self.interface.fields,
            Arc::from(name),
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
    fn new(name: &'static str) -> Self {
        Self {
            union: gql::UnionType::new(name),
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
        self.union.types.push(Arc::from(M::NAME));
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
    abstract_checks: &'a mut Vec<AbstractCheck>,
    owner: String,
    marker: PhantomData<fn() -> Ctx>,
}

impl<'a, Ctx: 'static> RootBuilder<'a, Ctx> {
    fn new(
        registry: &'a mut TypeRegistry,
        fields: &'a mut Vec<gql::Field>,
        abstract_checks: &'a mut Vec<AbstractCheck>,
        owner: String,
    ) -> Self {
        Self {
            registry,
            fields,
            abstract_checks,
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
        let field = root_field::<Ctx, T, A, F>(name, resolver, self.registry);
        self.push(field)
    }

    /// Add a synchronous root field whose resolver returns a `Result`.
    pub fn field_result<T, A, F>(&mut self, name: &'static str, resolver: F) -> FieldConfig<'_, Ctx>
    where
        T: OutputType + IntoValue + Send + 'static,
        A: InputObject,
        F: Fn(A, &Ctx) -> Result<T, GraphQLError> + Send + Sync + 'static,
    {
        let field = root_field_result::<Ctx, T, A, F>(name, resolver, self.registry);
        self.push(field)
    }

    /// Add an asynchronous root field whose resolver cannot fail.
    pub fn field_async<T, A, F>(&mut self, name: &'static str, resolver: F) -> FieldConfig<'_, Ctx>
    where
        T: OutputType + IntoValue + Send + 'static,
        A: InputObject,
        F: for<'r> Fn(A, &'r Ctx) -> BoxFuture<'r, T> + Send + Sync + 'static,
    {
        let field = root_field_async::<Ctx, T, A, F>(name, resolver, self.registry);
        self.push(field)
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
        let field = root_field_result_async::<Ctx, T, A, F>(name, resolver, self.registry);
        self.push(field)
    }

    fn push(&mut self, field: gql::Field) -> FieldConfig<'_, Ctx> {
        let name = field.name.clone();
        self.fields.push(field);
        FieldConfig::for_fields_checked(self.fields, self.abstract_checks, self.owner.clone(), name)
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
/// A subscription field's resolver returns a stream. The executor resolves the field's selection
/// set for every event the stream yields.
pub struct SubscriptionBuilder<'a, Ctx: 'static> {
    registry: &'a mut TypeRegistry,
    fields: &'a mut Vec<gql::Field>,
    abstract_checks: &'a mut Vec<AbstractCheck>,
    owner: String,
    marker: PhantomData<fn() -> Ctx>,
}

impl<'a, Ctx: 'static> SubscriptionBuilder<'a, Ctx> {
    fn new(
        registry: &'a mut TypeRegistry,
        fields: &'a mut Vec<gql::Field>,
        abstract_checks: &'a mut Vec<AbstractCheck>,
        owner: String,
    ) -> Self {
        Self {
            registry,
            fields,
            abstract_checks,
            owner,
            marker: PhantomData,
        }
    }

    /// Add a subscription field.
    ///
    /// `subscribe` is called once per subscription to create the event stream. Each item is
    /// either a value or an error; the stream must be `Send + 'static`, so anything it borrows
    /// from the context must be cloned first (channels and `Arc`s are the usual choice).
    pub fn field<A, T, F, S>(&mut self, name: &'static str, subscribe: F) -> FieldConfig<'_, Ctx>
    where
        A: InputObject,
        T: OutputType + IntoValue + Send + 'static,
        S: Stream<Item = Result<T, GraphQLError>> + Send + 'static,
        F: Fn(A, &Ctx) -> S + Send + Sync + 'static,
    {
        T::register_output(self.registry);
        A::register_args(self.registry);
        let subscribe = Arc::new(subscribe);
        let field = add_arguments::<A>(gql::Field::new(name, T::output_type()), self.registry)
            .resolve(|context| {
                // The subscription source produced this event; the root field's value is the
                // event itself, and `gql` then completes the field's selection set against it.
                Box::pin(async move { Ok(context.parent.clone()) })
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
                    let stream = (subscribe)(args, ctx);
                    let mapped = stream.map(|item| match item {
                        Ok(value) => value.into_value(),
                        Err(error) => Err(error),
                    });
                    Ok(Box::pin(mapped) as gql::SubscriptionStream)
                })
            });
        let name = field.name.clone();
        self.fields.push(field);
        FieldConfig::for_fields_checked(self.fields, self.abstract_checks, self.owner.clone(), name)
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
            (resolver)(model, args, ctx).into_value()
        })
    })
}

fn object_field_result<M, Ctx, T, A, F>(
    name: &'static str,
    resolver: F,
    registry: &mut TypeRegistry,
) -> gql::Field
where
    M: ObjectModel,
    Ctx: 'static,
    T: OutputType + IntoValue + Send + 'static,
    A: InputObject,
    F: Fn(&M, A, &Ctx) -> Result<T, GraphQLError> + Send + Sync + 'static,
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
            match (resolver)(model, args, ctx) {
                Ok(value) => value.into_value(),
                Err(error) => Err(error),
            }
        })
    })
}

fn object_field_async<M, Ctx, T, A, F>(
    name: &'static str,
    resolver: F,
    registry: &mut TypeRegistry,
) -> gql::Field
where
    M: ObjectModel,
    Ctx: 'static,
    T: OutputType + IntoValue + Send + 'static,
    A: InputObject,
    F: for<'r> Fn(&'r M, A, &'r Ctx) -> BoxFuture<'r, T> + Send + Sync + 'static,
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
            (resolver)(model, args, ctx).await.into_value()
        })
    })
}

fn object_field_result_async<M, Ctx, T, A, F>(
    name: &'static str,
    resolver: F,
    registry: &mut TypeRegistry,
) -> gql::Field
where
    M: ObjectModel,
    Ctx: 'static,
    T: OutputType + IntoValue + Send + 'static,
    A: InputObject,
    F: for<'r> Fn(&'r M, A, &'r Ctx) -> BoxFuture<'r, Result<T, GraphQLError>>
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
            match (resolver)(model, args, ctx).await {
                Ok(value) => value.into_value(),
                Err(error) => Err(error),
            }
        })
    })
}

fn root_field<Ctx, T, A, F>(
    name: &'static str,
    resolver: F,
    registry: &mut TypeRegistry,
) -> gql::Field
where
    Ctx: 'static,
    T: OutputType + IntoValue + Send + 'static,
    A: InputObject,
    F: Fn(A, &Ctx) -> T + Send + Sync + 'static,
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
            (resolver)(args, ctx).into_value()
        })
    })
}

fn root_field_result<Ctx, T, A, F>(
    name: &'static str,
    resolver: F,
    registry: &mut TypeRegistry,
) -> gql::Field
where
    Ctx: 'static,
    T: OutputType + IntoValue + Send + 'static,
    A: InputObject,
    F: Fn(A, &Ctx) -> Result<T, GraphQLError> + Send + Sync + 'static,
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
            match (resolver)(args, ctx) {
                Ok(value) => value.into_value(),
                Err(error) => Err(error.into()),
            }
        })
    })
}

fn root_field_async<Ctx, T, A, F>(
    name: &'static str,
    resolver: F,
    registry: &mut TypeRegistry,
) -> gql::Field
where
    Ctx: 'static,
    T: OutputType + IntoValue + Send + 'static,
    A: InputObject,
    F: for<'r> Fn(A, &'r Ctx) -> BoxFuture<'r, T> + Send + Sync + 'static,
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
            (resolver)(args, ctx).await.into_value()
        })
    })
}

fn root_field_result_async<Ctx, T, A, F>(
    name: &'static str,
    resolver: F,
    registry: &mut TypeRegistry,
) -> gql::Field
where
    Ctx: 'static,
    T: OutputType + IntoValue + Send + 'static,
    A: InputObject,
    F: for<'r> Fn(A, &'r Ctx) -> BoxFuture<'r, Result<T, GraphQLError>> + Send + Sync + 'static,
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
            match (resolver)(args, ctx).await {
                Ok(value) => value.into_value(),
                Err(error) => Err(error.into()),
            }
        })
    })
}

/// Verify that every `.as_abstract(...)` declaration is truthful.
///
/// The executor can only resolve a concrete object type that is a possible type of the abstract
/// type. Checking that here turns a runtime "expected Node but received a string" failure into a
/// schema build error.
fn validate_abstract_checks(
    schema: &gql::Schema,
    checks: &[AbstractCheck],
    errors: &mut Vec<GraphQLError>,
) {
    for check in checks {
        let Some(abstract_type) = schema.get_type(&check.abstract_name) else {
            errors.push(GraphQLError::new(format!(
                "Field `{}.{}` declares unknown abstract type `{}`.",
                check.owner, check.field, check.abstract_name
            )));
            continue;
        };
        if !matches!(abstract_type, gql::Type::Interface(_) | gql::Type::Union(_)) {
            errors.push(GraphQLError::new(format!(
                "Field `{}.{}` declares `{}` as abstract, but it is a {}.",
                check.owner,
                check.field,
                check.abstract_name,
                abstract_type.kind()
            )));
            continue;
        }
        let original = check.original.named_type();
        if original == &check.abstract_name {
            continue;
        }
        if !schema.is_possible_type(&check.abstract_name, original) {
            errors.push(GraphQLError::new(format!(
                "Field `{}.{}` declares abstract type `{}`, but its Rust type `{original}` is not a possible type of `{}`.",
                check.owner, check.field, check.abstract_name, check.abstract_name
            )));
        }
    }
}

/// A type reference for tests and tooling: the GraphQL type a Rust type maps to.
#[must_use]
pub fn type_ref<T: OutputType>() -> TypeRef {
    T::output_type()
}
