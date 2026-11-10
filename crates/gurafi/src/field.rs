//! Field options: descriptions, deprecation, complexity, authorization and type overrides.

use std::{
    any::TypeId,
    sync::{Arc, OnceLock},
};

use futures::future::BoxFuture;
use gql::{ComplexityFn, Field, GraphQLError, Name, TypeRef};
use indexmap::IndexMap;

use crate::{
    registry::TypeRegistry,
    schema::ObjectRef,
    traits::{AbstractType, InputObject, ObjectModel, OutputType},
};

/// A deferred check that a field's declared type is compatible with its Rust type.
#[derive(Clone, Debug)]
pub(crate) enum FieldCheck {
    /// `.as_abstract(...)` declared an interface or union output.
    Abstract {
        owner: String,
        field: Name,
        original: TypeRef,
        abstract_name: Name,
    },
    /// `.as_type::<T>()` replaced the field's GraphQL type.
    TypeOverride {
        owner: String,
        field: Name,
        original: TypeRef,
        replacement: TypeRef,
    },
    /// `.non_null()` was applied to a resolver that can return `None`.
    NonNullOnOption {
        owner: String,
        field: Name,
        rust_type: &'static str,
    },
    /// `.nullable_items()` was applied to a field that is not a list.
    NullableItemsOnNonList {
        owner: String,
        field: Name,
        ty: TypeRef,
    },
    /// `.non_null_items()` was applied to a field that is not a list.
    NonNullItemsOnNonList {
        owner: String,
        field: Name,
        ty: TypeRef,
    },
    /// A `.resolve_inherited(...)` field's Rust type must be a covariant match for the
    /// interface field it inherits.
    InheritedType {
        owner: String,
        field: Name,
        actual: TypeRef,
        expected: TypeRef,
    },
    /// `.as_object_ref(...)` declared a runtime-named object output.
    DynamicObject {
        owner: String,
        field: Name,
        original: TypeRef,
        model: &'static str,
    },
    /// `.as_dynamic(...)` declared an explicit runtime-named output type.
    DynamicType {
        owner: String,
        field: Name,
        rust: TypeRef,
        declared: TypeRef,
    },
}

/// Where a field was inserted, so its options can be applied once the builder call ends.
pub(crate) enum FieldSink<'a> {
    /// A field inside an object or interface definition.
    Object(&'a mut IndexMap<Name, Field>),
    /// The most recently pushed field of a root operation.
    Fields(&'a mut Vec<Field>),
}

impl FieldSink<'_> {
    fn get_mut(&mut self, name: &str) -> Option<&mut Field> {
        match self {
            Self::Object(fields) => fields.get_mut(name),
            Self::Fields(fields) => fields.iter_mut().rev().find(|field| &*field.name == name),
        }
    }
}

/// The predicate behind an authorization guard.
enum GuardPredicate<Ctx: 'static> {
    Sync(Arc<dyn Fn(&Ctx) -> bool + Send + Sync>),
    Async(Arc<dyn for<'r> Fn(&'r Ctx) -> BoxFuture<'r, bool> + Send + Sync>),
}

impl<Ctx: 'static> Clone for GuardPredicate<Ctx> {
    fn clone(&self) -> Self {
        match self {
            Self::Sync(predicate) => Self::Sync(Arc::clone(predicate)),
            Self::Async(predicate) => Self::Async(Arc::clone(predicate)),
        }
    }
}

/// A per-field authorization check, optionally inherited from an object or interface.
pub(crate) struct AuthGuard<Ctx: 'static> {
    predicate: GuardPredicate<Ctx>,
    message: Arc<str>,
}

impl<Ctx: 'static> Clone for AuthGuard<Ctx> {
    fn clone(&self) -> Self {
        Self {
            predicate: self.predicate.clone(),
            message: Arc::clone(&self.message),
        }
    }
}

impl<Ctx: 'static> AuthGuard<Ctx> {
    fn new(predicate: GuardPredicate<Ctx>, message: Option<Arc<str>>) -> Self {
        Self {
            predicate,
            message: message.unwrap_or_else(|| Arc::from("Not authorized")),
        }
    }

    /// Return a copy of this guard with a different denial message.
    pub(crate) fn with_message(&self, message: Option<Arc<str>>) -> Arc<Self> {
        Arc::new(Self {
            predicate: self.predicate.clone(),
            message: message.unwrap_or_else(|| Arc::clone(&self.message)),
        })
    }

    fn check<'r>(&self, context: &'r Ctx) -> BoxFuture<'r, Result<(), GraphQLError>> {
        let message = Arc::clone(&self.message);
        match &self.predicate {
            GuardPredicate::Sync(predicate) => {
                // Evaluate before creating the future so no reference to the context is held
                // across an await point.
                let allowed = predicate(context);
                Box::pin(async move {
                    if allowed {
                        Ok(())
                    } else {
                        Err(GraphQLError::new(message.to_string()))
                    }
                })
            }
            GuardPredicate::Async(predicate) => {
                let future = predicate(context);
                Box::pin(async move {
                    if future.await {
                        Ok(())
                    } else {
                        Err(GraphQLError::new(message.to_string()))
                    }
                })
            }
        }
    }
}

/// Sentinel arguments type stored by [`crate::ObjectBuilder::raw_field`].
///
/// A raw field without arguments has no resolver, so a `validate` hook could never run. Using a
/// dedicated `TypeId` lets [`FieldConfig::validate`] report that clearly instead of accepting the
/// hook and silently dropping it.
pub(crate) struct RawFieldArgs;

/// The arguments-type check installed by [`FieldConfig::validate`].
///
/// The slot is cloned into the field's resolver before the builder returns it, so a validation
/// hook installed afterwards can run inside the resolver's single argument-coercion step. That
/// keeps input-level validation from running a second time and keeps the validator independent
/// of the resolver's concrete `A` at the type level (the arguments [`TypeId`] is checked when the
/// hook is installed).
pub(crate) struct ValidationSlot<Ctx: 'static> {
    hook: Arc<OnceLock<ErasedValidator<Ctx>>>,
}

impl<Ctx: 'static> Clone for ValidationSlot<Ctx> {
    fn clone(&self) -> Self {
        Self {
            hook: Arc::clone(&self.hook),
        }
    }
}

type ErasedValidator<Ctx> =
    Arc<dyn Fn(&dyn std::any::Any, &Ctx) -> Result<(), GraphQLError> + Send + Sync>;

impl<Ctx: 'static> Default for ValidationSlot<Ctx> {
    fn default() -> Self {
        Self::new()
    }
}

impl<Ctx: 'static> ValidationSlot<Ctx> {
    pub(crate) fn new() -> Self {
        Self {
            hook: Arc::new(OnceLock::new()),
        }
    }

    /// Install a validator for arguments of type `A`.
    pub(crate) fn install<A: 'static>(
        &self,
        validator: impl Fn(&A, &Ctx) -> Result<(), GraphQLError> + Send + Sync + 'static,
    ) {
        let validator = Arc::new(validator);
        let _ = self
            .hook
            .set(Arc::new(move |args: &dyn std::any::Any, ctx: &Ctx| {
                let args = args.downcast_ref::<A>().ok_or_else(|| {
                    GraphQLError::new(
                        "Internal error: a field validator received arguments of a different type.",
                    )
                })?;
                validator(args, ctx)
            }));
    }

    /// Whether a validator was already installed.
    pub(crate) fn is_installed(&self) -> bool {
        self.hook.get().is_some()
    }

    /// Run the installed validator, if any.
    pub(crate) fn run<A: 'static>(&self, args: &A, ctx: &Ctx) -> Result<(), GraphQLError> {
        match self.hook.get() {
            Some(hook) => hook(args, ctx),
            None => Ok(()),
        }
    }
}

enum Complexity {
    Fixed(u32),
    Custom(ComplexityFn),
}

/// Builder returned by every `field` and `expose` method.
///
/// The options are applied when the value is dropped, which is at the end of the statement, so
/// they can be chained in any order and without an explicit `build` call:
///
/// ```
/// # use gurafi::{Object, Schema, NoArgs};
/// # struct Ctx { is_logged_in: bool }
/// # #[derive(Object)] struct Giraffe { name: String }
/// # let mut schema = Schema::<Ctx>::new();
/// # schema.object::<Giraffe>(|o| {
/// o.field("name", |g: &Giraffe, _: NoArgs, _: &Ctx| g.name.clone())
///     .description("The giraffe's name.")
///     .deprecated("Use `displayName`.")
///     .auth(|ctx: &Ctx| ctx.is_logged_in)
///     .auth_error("Sign in to read names.")
///     .complexity(2);
/// # });
/// ```
pub struct FieldConfig<'a, Ctx: 'static> {
    registry: &'a mut TypeRegistry,
    sink: FieldSink<'a>,
    name: Name,
    description: Option<Arc<str>>,
    deprecation: Option<Arc<str>>,
    complexity: Option<Complexity>,
    auth: Option<GuardPredicate<Ctx>>,
    auth_error: Option<Arc<str>>,
    /// Interface fields store authorization here so implementing objects inherit it.
    field_auth: Option<&'a mut IndexMap<Name, Arc<AuthGuard<Ctx>>>>,
    checks: Option<&'a mut Vec<FieldCheck>>,
    owner: String,
    original: TypeRef,
    rust_nullable: bool,
    rust_type_name: &'static str,
    /// The Rust arguments type the field's resolvers coerce into.
    args_type: TypeId,
    /// Validator installed by [`FieldConfig::validate`], read inside the resolver.
    validation: ValidationSlot<Ctx>,
    marker: std::marker::PhantomData<fn() -> Ctx>,
}

impl<'a, Ctx: 'static> FieldConfig<'a, Ctx> {
    pub(crate) fn for_object_checked(
        registry: &'a mut TypeRegistry,
        fields: &'a mut IndexMap<Name, Field>,
        checks: &'a mut Vec<FieldCheck>,
        owner: impl Into<String>,
        name: Name,
        rust_nullable: bool,
        rust_type_name: &'static str,
        args_type: TypeId,
        validation: ValidationSlot<Ctx>,
    ) -> Self {
        Self::new(
            registry,
            FieldSink::Object(fields),
            name,
            Some(checks),
            owner.into(),
            rust_nullable,
            rust_type_name,
            args_type,
            validation,
        )
    }

    pub(crate) fn for_interface(
        registry: &'a mut TypeRegistry,
        fields: &'a mut IndexMap<Name, Field>,
        field_auth: &'a mut IndexMap<Name, Arc<AuthGuard<Ctx>>>,
        name: Name,
        rust_nullable: bool,
        rust_type_name: &'static str,
    ) -> Self {
        let mut config = Self::new(
            registry,
            FieldSink::Object(fields),
            name,
            None,
            String::new(),
            rust_nullable,
            rust_type_name,
            TypeId::of::<()>(),
            ValidationSlot::new(),
        );
        config.field_auth = Some(field_auth);
        config
    }

    pub(crate) fn for_fields_checked(
        registry: &'a mut TypeRegistry,
        fields: &'a mut Vec<Field>,
        checks: &'a mut Vec<FieldCheck>,
        owner: impl Into<String>,
        name: Name,
        rust_nullable: bool,
        rust_type_name: &'static str,
        args_type: TypeId,
        validation: ValidationSlot<Ctx>,
    ) -> Self {
        Self::new(
            registry,
            FieldSink::Fields(fields),
            name,
            Some(checks),
            owner.into(),
            rust_nullable,
            rust_type_name,
            args_type,
            validation,
        )
    }

    fn new(
        registry: &'a mut TypeRegistry,
        mut sink: FieldSink<'a>,
        name: Name,
        checks: Option<&'a mut Vec<FieldCheck>>,
        owner: String,
        rust_nullable: bool,
        rust_type_name: &'static str,
        args_type: TypeId,
        validation: ValidationSlot<Ctx>,
    ) -> Self {
        let original = sink
            .get_mut(&name)
            .map_or_else(|| TypeRef::Named(name.clone()), |field| field.ty.clone());
        Self {
            registry,
            sink,
            name,
            description: None,
            deprecation: None,
            complexity: None,
            auth: None,
            auth_error: None,
            field_auth: None,
            checks,
            owner,
            original,
            rust_nullable,
            rust_type_name,
            args_type,
            validation,
            marker: std::marker::PhantomData,
        }
    }

    /// The GraphQL field name.
    #[must_use]
    pub fn name(&self) -> &str {
        &self.name
    }

    /// Set the field's description.
    pub fn description(&mut self, description: impl Into<Arc<str>>) -> &mut Self {
        self.description = Some(description.into());
        self
    }

    /// Mark the field as deprecated with a reason shown by introspection.
    pub fn deprecated(&mut self, reason: impl Into<Arc<str>>) -> &mut Self {
        self.deprecation = Some(reason.into());
        self
    }

    /// Set a constant complexity estimate for the field.
    ///
    /// The executor uses estimates to reject expensive operations before running them.
    pub fn complexity(&mut self, complexity: u32) -> &mut Self {
        self.complexity = Some(Complexity::Fixed(complexity));
        self
    }

    /// Set a complexity estimate computed from the field's coerced arguments.
    pub fn complexity_with(
        &mut self,
        estimator: impl Fn(&gql::Object) -> u32 + Send + Sync + 'static,
    ) -> &mut Self {
        self.complexity = Some(Complexity::Custom(Arc::new(estimator)));
        self
    }

    /// Require the caller to satisfy `predicate` before the resolver runs.
    ///
    /// The predicate receives the schema's context type, where a server usually stores the
    /// authenticated user. A denied field returns a GraphQL error, so it follows normal null
    /// propagation: a nullable field becomes `null`, a non-null field nulls its parent.
    pub fn auth(&mut self, predicate: impl Fn(&Ctx) -> bool + Send + Sync + 'static) -> &mut Self {
        self.auth = Some(GuardPredicate::Sync(Arc::new(predicate)));
        self
    }

    /// Like [`FieldConfig::auth`], but the predicate is asynchronous.
    ///
    /// Use this when the check needs a database or an external policy service. The future must
    /// borrow the context; write it as `Box::pin(async move { ... })`.
    pub fn auth_async(
        &mut self,
        predicate: impl for<'r> Fn(&'r Ctx) -> BoxFuture<'r, bool> + Send + Sync + 'static,
    ) -> &mut Self {
        self.auth = Some(GuardPredicate::Async(Arc::new(predicate)));
        self
    }

    /// Set the message returned when [`FieldConfig::auth`] denies access.
    ///
    /// The default is `"Not authorized"`. Avoid leaking sensitive details: clients see this
    /// message.
    pub fn auth_error(&mut self, message: impl Into<Arc<str>>) -> &mut Self {
        self.auth_error = Some(message.into());
        self
    }

    /// Validate coerced arguments before the resolver runs.
    ///
    /// `A` must be the field's arguments type. Naming a different type is a schema build error,
    /// because the validator would otherwise be handed arguments the resolver never sees. A
    /// validation error is reported as a normal field error, exactly like a failed coercion.
    /// This is the builder-side counterpart to `#[graphql(validate = "...")]`, for rules that
    /// need the request context.
    ///
    /// The hook runs inside the resolver's single argument-coercion step, after input-level
    /// validation and before the resolver closure. It never coerces or re-validates the
    /// arguments a second time. For subscription fields it runs once per event, after the
    /// arguments were validated once when the subscription was set up.
    ///
    /// ```
    /// use gurafi::{Input, Schema};
    ///
    /// struct Ctx {
    ///     logged_in: bool,
    /// }
    ///
    /// #[derive(Input)]
    /// struct GreetArgs {
    ///     name: String,
    /// }
    ///
    /// let mut schema = Schema::<Ctx>::new();
    /// schema.query(|q| {
    ///     q.field("greet", |args: GreetArgs, _: &Ctx| args.name)
    ///         .validate::<GreetArgs>(|args, ctx| {
    ///             if args.name.is_empty() {
    ///                 Err(gurafi::GraphQLError::new("name must not be empty"))
    ///             } else if !ctx.logged_in {
    ///                 Err(gurafi::GraphQLError::new("sign in first"))
    ///             } else {
    ///                 Ok(())
    ///             }
    ///         });
    /// });
    /// let schema = schema.build().expect("schema builds");
    /// # let _ = schema;
    /// ```
    pub fn validate<A: InputObject>(
        &mut self,
        validator: impl Fn(&A, &Ctx) -> Result<(), GraphQLError> + Send + Sync + 'static,
    ) -> &mut Self {
        if self.args_type == TypeId::of::<RawFieldArgs>() {
            self.registry.push_error(GraphQLError::new(format!(
                "Field `{}.{}` is declared with `raw_field`, which installs no resolver, so a \
                 `validate` hook could never run. Declare the field with `raw_field_with::<_, \
                 Args>(...)` to coerce arguments through a resolver, then call `validate` on that.",
                self.owner, self.name,
            )));
            return self;
        }
        if TypeId::of::<A>() != self.args_type {
            self.registry.push_error(GraphQLError::new(format!(
                "Field `{}.{}` calls `validate::<{}>()`, but its resolver's arguments type is \
                 not `{}`. Use the field's actual arguments type, or change the resolver to take \
                 `{}`.",
                self.owner,
                self.name,
                std::any::type_name::<A>(),
                std::any::type_name::<A>(),
                std::any::type_name::<A>(),
            )));
            return self;
        }
        if self.validation.is_installed() {
            self.registry.push_error(GraphQLError::new(format!(
                "Field `{}.{}` installs more than one `validate` hook. Combine the checks into \
                 one validator.",
                self.owner, self.name,
            )));
            return self;
        }
        self.validation.install::<A>(validator);
        self
    }

    /// Declare the field's GraphQL type as an interface or union instead of its concrete Rust
    /// type.
    ///
    /// The resolver still returns concrete model values; the executor's `is_type_of` check (or
    /// the abstract type's `resolve_type`) selects the concrete type per value. List and
    /// nullability wrappers are preserved.
    ///
    /// The compatibility check uses the field's current named type, so it composes with
    /// [`FieldConfig::as_object_ref`]: declare a runtime-named object first, then widen it to an
    /// interface or union that lists that object as a possible type.
    pub fn as_abstract(&mut self, reference: impl AbstractType) -> &mut Self {
        let abstract_name: Name = Arc::from(reference.abstract_name());
        let original = self
            .sink
            .get_mut(&self.name)
            .map_or_else(|| self.original.clone(), |field| field.ty.clone());
        if let Some(checks) = &mut self.checks {
            checks.push(FieldCheck::Abstract {
                owner: self.owner.clone(),
                field: self.name.clone(),
                original,
                abstract_name: abstract_name.clone(),
            });
        }
        if let Some(field) = self.sink.get_mut(&self.name) {
            field.ty = replace_named_type(&field.ty, abstract_name);
        }
        self
    }

    /// Declare the field's output type as a runtime-named object.
    ///
    /// The named type is replaced, keeping the list and nullability wrappers of the field's Rust
    /// type. This is what [`crate::ObjectBuilder::field_ref`] uses internally; it is public for
    /// fields whose type is declared separately, such as `raw_field`.
    ///
    /// The Rust type must be the model the reference was created for (`M` itself, or a wrapper
    /// such as [`crate::Raw<M>`] that delegates its output type to `M`). A mismatch is a build
    /// error.
    pub fn as_object_ref<M: ObjectModel>(&mut self, reference: &ObjectRef<M>) -> &mut Self {
        self.registry.reference_object(reference.name());
        if let Some(checks) = &mut self.checks {
            checks.push(FieldCheck::DynamicObject {
                owner: self.owner.clone(),
                field: self.name.clone(),
                original: self.original.clone(),
                model: M::NAME,
            });
        }
        if let Some(field) = self.sink.get_mut(&self.name) {
            field.ty = replace_named_type(&field.ty, Arc::from(reference.name()));
        }
        self
    }

    /// Replace the field's GraphQL type with an explicitly built runtime type reference.
    ///
    /// Use this when the output type's name is computed at runtime and there is no object model
    /// to pass to [`FieldConfig::as_object_ref`] — for example a runtime-named interface or
    /// union, or a list such as `TypeRef::named(name).list().non_null()`.
    ///
    /// The wrappers must still be truthful for the resolver's Rust type: nullability may be
    /// loosened and never tightened, and the list shape must match. The named type must exist in
    /// the schema by build time.
    pub fn as_dynamic(&mut self, ty: TypeRef) -> &mut Self {
        let current = self
            .sink
            .get_mut(&self.name)
            .map_or_else(|| self.original.clone(), |field| field.ty.clone());
        if let Some(checks) = &mut self.checks {
            checks.push(FieldCheck::DynamicType {
                owner: self.owner.clone(),
                field: self.name.clone(),
                rust: current,
                declared: ty.clone(),
            });
        }
        if let Some(field) = self.sink.get_mut(&self.name) {
            field.ty = ty;
        }
        self
    }

    /// Replace the field's GraphQL type with `T`'s output type.
    ///
    /// This is the escape hatch for the rare case where a Rust type is serialized differently
    /// from how its own [`OutputType`] describes it — for example resolving a `String` but
    /// declaring a custom scalar, or resolving a model as an interface. `returns` is an alias.
    ///
    /// # Compatibility
    ///
    /// The override is checked when the schema is built. It is only accepted between scalars,
    /// between enums, or to and from an abstract type when the Rust type is a possible type of
    /// it. Wrappers are taken from `T`, unlike [`FieldConfig::as_abstract`] which preserves them,
    /// but they must be truthful for the resolver's Rust type: nullability may be loosened and
    /// never tightened, and the list shape must match. `Option<String>` cannot be declared
    /// `String!`, `Vec<Option<String>>` cannot be declared `[String!]!`, and `Vec<String>` cannot
    /// be declared `String!`.
    ///
    /// # Runtime risk
    ///
    /// Schema validation cannot see the runtime value of a scalar that is serialized by a custom
    /// conversion. If the resolver returns a value the new named type cannot serialize, the
    /// request still fails with a field error at execution time. Keep the Rust return type and
    /// the override honest.
    pub fn as_type<T: OutputType>(&mut self) -> &mut Self {
        self.override_type::<T>()
    }

    /// Alias for [`FieldConfig::as_type`].
    pub fn returns<T: OutputType>(&mut self) -> &mut Self {
        self.override_type::<T>()
    }

    fn override_type<T: OutputType>(&mut self) -> &mut Self {
        T::register_output(self.registry);
        let replacement = T::output_type();
        if let Some(checks) = &mut self.checks {
            checks.push(FieldCheck::TypeOverride {
                owner: self.owner.clone(),
                field: self.name.clone(),
                original: self.original.clone(),
                replacement: replacement.clone(),
            });
        }
        if let Some(field) = self.sink.get_mut(&self.name) {
            field.ty = replacement;
        }
        self
    }

    /// Remove the outer non-null wrapper, making the field nullable.
    ///
    /// This is the safe direction: a resolver that always returns a value can still be declared
    /// nullable. The opposite direction, [`FieldConfig::non_null`], is checked against the Rust
    /// type.
    pub fn nullable(&mut self) -> &mut Self {
        if let Some(field) = self.sink.get_mut(&self.name) {
            if let TypeRef::NonNull(inner) = &field.ty {
                field.ty = (**inner).clone();
            }
        }
        self
    }

    /// Add an outer non-null wrapper.
    ///
    /// Applying this to a resolver whose Rust return type is `Option<T>` is a schema build
    /// error, because a `None` would then violate the declared type.
    pub fn non_null(&mut self) -> &mut Self {
        if self.rust_nullable {
            if let Some(checks) = &mut self.checks {
                checks.push(FieldCheck::NonNullOnOption {
                    owner: self.owner.clone(),
                    field: self.name.clone(),
                    rust_type: self.rust_type_name,
                });
            }
        }
        if let Some(field) = self.sink.get_mut(&self.name) {
            if !matches!(field.ty, TypeRef::NonNull(_)) {
                field.ty = TypeRef::NonNull(Box::new(field.ty.clone()));
            }
        }
        self
    }

    /// Remove the item-level non-null wrapper from a list field.
    ///
    /// `[Giraffe!]!` becomes `[Giraffe]!`; the field's own nullability is unchanged. Applying
    /// this to a field that is not a list is a schema build error.
    pub fn nullable_items(&mut self) -> &mut Self {
        let Some(field) = self.sink.get_mut(&self.name) else {
            return self;
        };
        match nullable_items(&field.ty) {
            Some(ty) => field.ty = ty,
            None => {
                let ty = field.ty.clone();
                if let Some(checks) = &mut self.checks {
                    checks.push(FieldCheck::NullableItemsOnNonList {
                        owner: self.owner.clone(),
                        field: self.name.clone(),
                        ty,
                    });
                }
            }
        }
        self
    }

    /// Add the item-level non-null wrapper to a list field.
    ///
    /// `[Giraffe]!` becomes `[Giraffe!]!`; the field's own nullability is unchanged. Applying
    /// this to a field that is not a list is a schema build error.
    ///
    /// Only use this when the Rust collection's items cannot be `None`. Rust's type system is
    /// not reflected in the GraphQL wrapper, so this cannot be checked automatically: a
    /// `Vec<Option<T>>` must not be declared with non-null items.
    pub fn non_null_items(&mut self) -> &mut Self {
        let Some(field) = self.sink.get_mut(&self.name) else {
            return self;
        };
        match non_null_items(&field.ty) {
            Some(ty) => field.ty = ty,
            None => {
                let ty = field.ty.clone();
                if let Some(checks) = &mut self.checks {
                    checks.push(FieldCheck::NonNullItemsOnNonList {
                        owner: self.owner.clone(),
                        field: self.name.clone(),
                        ty,
                    });
                }
            }
        }
        self
    }
}

impl<Ctx: 'static> Drop for FieldConfig<'_, Ctx> {
    fn drop(&mut self) {
        let Some(field) = self.sink.get_mut(&self.name) else {
            return;
        };
        if let Some(description) = self.description.take() {
            field.description = Some(description);
        }
        if let Some(deprecation) = self.deprecation.take() {
            field.deprecation_reason = Some(deprecation);
        }
        if let Some(complexity) = self.complexity.take() {
            field.complexity = Some(match complexity {
                Complexity::Fixed(value) => Arc::new(move |_| value),
                Complexity::Custom(function) => function,
            });
        }
        if let Some(predicate) = self.auth.take() {
            let guard = Arc::new(AuthGuard::new(predicate, self.auth_error.take()));
            if let Some(field_auth) = &mut self.field_auth {
                field_auth.insert(self.name.clone(), guard);
            } else {
                apply_auth_guard(field, guard);
            }
        }
    }
}

/// Options for an interface field.
///
/// Interface fields have no resolver: execution uses the concrete object's field. Authorization
/// declared here is inherited by every implementing object's matching field, so a rule can be
/// stated once on the interface. Descriptions and deprecation are metadata on the interface
/// field itself.
pub struct InterfaceFieldConfig<'a, Ctx: 'static> {
    inner: FieldConfig<'a, Ctx>,
}

impl<'a, Ctx: 'static> InterfaceFieldConfig<'a, Ctx> {
    pub(crate) fn new(inner: FieldConfig<'a, Ctx>) -> Self {
        Self { inner }
    }

    /// The GraphQL field name.
    #[must_use]
    pub fn name(&self) -> &str {
        self.inner.name()
    }

    /// Set the field's description.
    pub fn description(&mut self, description: impl Into<Arc<str>>) -> &mut Self {
        self.inner.description(description);
        self
    }

    /// Mark the field as deprecated with a reason shown by introspection.
    pub fn deprecated(&mut self, reason: impl Into<Arc<str>>) -> &mut Self {
        self.inner.deprecated(reason);
        self
    }

    /// Require every implementing object's field to satisfy `predicate`.
    ///
    /// The rule is attached to the interface field and applied when the schema is built, so an
    /// object does not have to repeat it.
    pub fn auth(&mut self, predicate: impl Fn(&Ctx) -> bool + Send + Sync + 'static) -> &mut Self {
        self.inner.auth(predicate);
        self
    }

    /// Like [`InterfaceFieldConfig::auth`], but asynchronous.
    pub fn auth_async(
        &mut self,
        predicate: impl for<'r> Fn(&'r Ctx) -> BoxFuture<'r, bool> + Send + Sync + 'static,
    ) -> &mut Self {
        self.inner.auth_async(predicate);
        self
    }

    /// Set the message returned when the inherited authorization denies access.
    pub fn auth_error(&mut self, message: impl Into<Arc<str>>) -> &mut Self {
        self.inner.auth_error(message);
        self
    }
}

impl<Ctx> std::fmt::Debug for FieldConfig<'_, Ctx> {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("FieldConfig")
            .field("name", &self.name)
            .field("description", &self.description)
            .field("deprecation", &self.deprecation)
            .field("authorized", &self.auth.is_some())
            .finish_non_exhaustive()
    }
}

/// Read a raw field's value from the executor parent value.
///
/// A raw object field has no resolver: its value comes from the object map produced by
/// [`crate::Raw::object`]. A model parent (`Value::External`) means the object was resolved
/// normally, so the map is absent and the field cannot be read.
pub(crate) fn read_raw_field(
    field_name: &str,
    parent: &gql::Value,
) -> Result<gql::Value, GraphQLError> {
    match parent {
        gql::Value::Object(object) => {
            Ok(object.get(field_name).cloned().unwrap_or(gql::Value::Null))
        }
        gql::Value::Null => Ok(gql::Value::Null),
        gql::Value::External(_) => Err(GraphQLError::new(format!(
            "Field \"{field_name}\" is a raw field with no resolver, but its parent is a model \
             value, not a raw object. Resolve the parent with `Raw`, or declare the field with a \
             resolver."
        ))),
        other => Err(GraphQLError::new(format!(
            "Field \"{field_name}\" is a raw field with no resolver and its parent is a {}, not \
             an object.",
            other.kind()
        ))),
    }
}

/// Install the executor's default property resolver on a field that has none.
///
/// Raw object fields intentionally have no resolver, so the executor reads the value from the
/// object map without a per-field call. Authorization, however, is applied by wrapping the
/// field's resolver. When an authorization rule reaches such a field, install an equivalent
/// property resolver first so the guard cannot be silently skipped.
pub(crate) fn install_property_resolver(field: &mut Field) {
    if field.resolve.is_some() || field.subscribe.is_some() {
        return;
    }
    field.resolve = Some(property_resolver(field.name.clone()));
}

/// The resolver a raw field would use if the executor ran its default property lookup.
///
/// Plugins use it as `next` while deciding whether they wrap a raw field. If no plugin wraps,
/// the caller leaves the field without a resolver and the executor keeps its leaf fast path.
pub(crate) fn property_resolver(name: Name) -> gql::ResolveFn {
    Arc::new(move |context| {
        let name = name.clone();
        Box::pin(async move { read_raw_field(&name, context.parent) })
    })
}

/// Wrap a field's resolvers with an authorization check.
///
/// Used for field-level auth as the config is dropped, and for object- and interface-level auth
/// when the schema is built. Wrapping here rather than relying on the executor's authorizer keeps
/// the feature available even when a server does not install one.
pub(crate) fn apply_auth_guard<Ctx: 'static>(field: &mut Field, guard: Arc<AuthGuard<Ctx>>) {
    install_property_resolver(field);
    let resolve_guard = Arc::clone(&guard);
    if let Some(resolve) = field.resolve.take() {
        field.resolve = Some(Arc::new(move |context| {
            let resolve = Arc::clone(&resolve);
            let guard = Arc::clone(&resolve_guard);
            Box::pin(async move {
                let ctx = context.context.require_data::<Ctx>()?;
                guard.check(ctx).await?;
                resolve(context).await
            })
        }));
    }
    if let Some(subscribe) = field.subscribe.take() {
        field.subscribe = Some(Arc::new(move |context| {
            let subscribe = Arc::clone(&subscribe);
            let guard = Arc::clone(&guard);
            Box::pin(async move {
                let ctx = context.context.require_data::<Ctx>()?;
                guard.check(ctx).await?;
                subscribe(context).await
            })
        }));
    }
}

pub(crate) fn sync_guard<Ctx: 'static>(
    predicate: impl Fn(&Ctx) -> bool + Send + Sync + 'static,
    message: Option<Arc<str>>,
) -> Arc<AuthGuard<Ctx>> {
    Arc::new(AuthGuard::new(
        GuardPredicate::Sync(Arc::new(predicate)),
        message,
    ))
}

pub(crate) fn async_guard<Ctx: 'static>(
    predicate: impl for<'r> Fn(&'r Ctx) -> BoxFuture<'r, bool> + Send + Sync + 'static,
    message: Option<Arc<str>>,
) -> Arc<AuthGuard<Ctx>> {
    Arc::new(AuthGuard::new(
        GuardPredicate::Async(Arc::new(predicate)),
        message,
    ))
}

fn replace_named_type(ty: &TypeRef, name: Name) -> TypeRef {
    match ty {
        TypeRef::Named(_) => TypeRef::Named(name),
        TypeRef::List(inner) => TypeRef::List(Box::new(replace_named_type(inner, name))),
        TypeRef::NonNull(inner) => TypeRef::NonNull(Box::new(replace_named_type(inner, name))),
    }
}

/// Add the item non-null to a list type, preserving the outer nullability.
fn non_null_items(ty: &TypeRef) -> Option<TypeRef> {
    match ty {
        TypeRef::NonNull(inner) => non_null_items(inner).map(|ty| TypeRef::NonNull(Box::new(ty))),
        TypeRef::List(inner) => Some(TypeRef::List(match inner.as_ref() {
            TypeRef::NonNull(_) => inner.clone(),
            other => Box::new(TypeRef::NonNull(Box::new(other.clone()))),
        })),
        TypeRef::Named(_) => None,
    }
}

/// Remove the item non-null from a list type, preserving the outer nullability.
fn nullable_items(ty: &TypeRef) -> Option<TypeRef> {
    match ty {
        TypeRef::NonNull(inner) => nullable_items(inner).map(|ty| TypeRef::NonNull(Box::new(ty))),
        TypeRef::List(inner) => Some(TypeRef::List(match inner.as_ref() {
            TypeRef::NonNull(item) => item.clone(),
            other => Box::new(other.clone()),
        })),
        TypeRef::Named(_) => None,
    }
}
