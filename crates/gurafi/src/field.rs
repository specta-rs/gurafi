//! Field options: descriptions, deprecation, complexity and authorization.

use std::sync::Arc;

use gql::{ComplexityFn, Field, GraphQLError, Name, TypeRef};
use indexmap::IndexMap;

use crate::traits::AbstractType;

/// A deferred check that a field's declared abstract output type can actually be produced by its
/// Rust type.
#[derive(Debug)]
pub(crate) struct AbstractCheck {
    pub(crate) owner: String,
    pub(crate) field: Name,
    pub(crate) original: TypeRef,
    pub(crate) abstract_name: Name,
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

/// A per-field authorization check.
struct AuthGuard<Ctx: 'static> {
    predicate: Arc<dyn Fn(&Ctx) -> bool + Send + Sync>,
    message: Arc<str>,
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
    sink: FieldSink<'a>,
    name: Name,
    description: Option<Arc<str>>,
    deprecation: Option<Arc<str>>,
    complexity: Option<Complexity>,
    auth: Option<Arc<dyn Fn(&Ctx) -> bool + Send + Sync>>,
    auth_error: Option<Arc<str>>,
    abstract_override: Option<Name>,
    checks: Option<&'a mut Vec<AbstractCheck>>,
    owner: String,
    original: TypeRef,
    marker: std::marker::PhantomData<fn() -> Ctx>,
}

impl<'a, Ctx: 'static> FieldConfig<'a, Ctx> {
    pub(crate) fn for_object(fields: &'a mut IndexMap<Name, Field>, name: Name) -> Self {
        Self::new(FieldSink::Object(fields), name)
    }

    pub(crate) fn for_object_checked(
        fields: &'a mut IndexMap<Name, Field>,
        checks: &'a mut Vec<AbstractCheck>,
        owner: impl Into<String>,
        name: Name,
    ) -> Self {
        Self::new_checked(FieldSink::Object(fields), name, Some(checks), owner.into())
    }

    pub(crate) fn for_fields_checked(
        fields: &'a mut Vec<Field>,
        checks: &'a mut Vec<AbstractCheck>,
        owner: impl Into<String>,
        name: Name,
    ) -> Self {
        Self::new_checked(FieldSink::Fields(fields), name, Some(checks), owner.into())
    }

    fn new(sink: FieldSink<'a>, name: Name) -> Self {
        Self::new_checked(sink, name, None, String::new())
    }

    fn new_checked(
        mut sink: FieldSink<'a>,
        name: Name,
        checks: Option<&'a mut Vec<AbstractCheck>>,
        owner: String,
    ) -> Self {
        let original = sink
            .get_mut(&name)
            .map_or_else(|| TypeRef::Named(name.clone()), |field| field.ty.clone());
        Self {
            sink,
            name,
            description: None,
            deprecation: None,
            complexity: None,
            auth: None,
            auth_error: None,
            abstract_override: None,
            checks,
            owner,
            original,
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
        self.auth = Some(Arc::new(predicate));
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

    /// Declare the field's GraphQL type as an interface or union instead of its concrete Rust
    /// type.
    ///
    /// The resolver still returns concrete model values; the executor's `is_type_of` check (or
    /// the abstract type's `resolve_type`) selects the concrete type per value. List and
    /// nullability wrappers are preserved.
    pub fn as_abstract(&mut self, reference: impl AbstractType) -> &mut Self {
        let abstract_name: Name = Arc::from(reference.abstract_name());
        if let Some(checks) = &mut self.checks {
            checks.push(AbstractCheck {
                owner: self.owner.clone(),
                field: self.name.clone(),
                original: self.original.clone(),
                abstract_name: abstract_name.clone(),
            });
        }
        self.abstract_override = Some(abstract_name);
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
        if let Some(abstract_name) = self.abstract_override.take() {
            field.ty = replace_named_type(&field.ty, abstract_name);
        }
        if let Some(predicate) = self.auth.take() {
            let guard = Arc::new(AuthGuard {
                predicate,
                message: self
                    .auth_error
                    .take()
                    .unwrap_or_else(|| Arc::from("Not authorized")),
            });
            if let Some(resolve) = field.resolve.take() {
                field.resolve = Some(guard_resolve(resolve, Arc::clone(&guard)));
            }
            if let Some(subscribe) = field.subscribe.take() {
                field.subscribe = Some(guard_subscribe(subscribe, guard));
            }
        }
    }
}

/// Options for an interface field.
///
/// Interface fields have no resolver: execution uses the concrete object's field. That also means
/// authorization, complexity and abstract overrides cannot apply here, so this wrapper only
/// exposes the metadata an interface field can carry. Put `.auth(...)` on each implementing
/// object's field instead; the type system rejects it here rather than silently dropping it.
///
/// ```compile_fail
/// use gurafi::Schema;
///
/// struct Ctx;
/// let mut schema = Schema::<Ctx>::new();
/// schema.interface("Node", |i| {
///     // Interface fields have no resolver, so authorization cannot be attached here.
///     i.field::<String>("id").auth(|_: &Ctx| true);
/// });
/// ```
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

fn replace_named_type(ty: &TypeRef, name: Name) -> TypeRef {
    match ty {
        TypeRef::Named(_) => TypeRef::Named(name),
        TypeRef::List(inner) => TypeRef::List(Box::new(replace_named_type(inner, name))),
        TypeRef::NonNull(inner) => TypeRef::NonNull(Box::new(replace_named_type(inner, name))),
    }
}

fn authorize<Ctx: 'static>(
    context: &gql::ExecutionContext,
    guard: &AuthGuard<Ctx>,
) -> Result<(), GraphQLError> {
    let data = context.require_data::<Ctx>()?;
    if (guard.predicate)(data) {
        Ok(())
    } else {
        Err(GraphQLError::new(guard.message.to_string()))
    }
}

fn guard_resolve<Ctx: 'static>(
    inner: gql::ResolveFn,
    guard: Arc<AuthGuard<Ctx>>,
) -> gql::ResolveFn {
    Arc::new(move |context| {
        let inner = Arc::clone(&inner);
        let guard = Arc::clone(&guard);
        Box::pin(async move {
            authorize(context.context, &guard)?;
            inner(context).await
        })
    })
}

fn guard_subscribe<Ctx: 'static>(
    inner: gql::SubscribeFn,
    guard: Arc<AuthGuard<Ctx>>,
) -> gql::SubscribeFn {
    Arc::new(move |context| {
        let inner = Arc::clone(&inner);
        let guard = Arc::clone(&guard);
        Box::pin(async move {
            authorize(context.context, &guard)?;
            inner(context).await
        })
    })
}
