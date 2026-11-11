//! A small, build-time plugin surface for every field in a schema.
//!
//! A plugin sees each field once while the schema is built, can change its metadata, and can wrap
//! the resolver and subscription functions. That is enough for cross-cutting concerns such as
//! tracing, authorization, mocks, loaders and response extensions without introducing a dynamic
//! type system or a per-request plugin loop.
//!
//! ```
//! use gurafi::{FieldMeta, NoArgs, Object, Plugin, Schema};
//!
//! struct LogFields;
//!
//! impl Plugin<Ctx> for LogFields {
//!     fn field(&self, meta: &FieldMeta<'_>, field: &mut gql::Field) {
//!         if meta.is_root {
//!             field.description = Some(format!("Root field {}", meta.field_name).into());
//!         }
//!     }
//! }
//!
//! #[derive(Object)]
//! struct Giraffe {
//!     name: String,
//! }
//!
//! struct Ctx;
//!
//! let mut schema = Schema::<Ctx>::new();
//! schema.plugin(LogFields);
//! schema.object::<Giraffe>(|o| {
//!     o.expose(gurafi::expose!(Giraffe::name));
//! });
//! schema.query(|q| {
//!     q.field("giraffe", |_: NoArgs, _: &Ctx| Giraffe {
//!         name: "Melman".into(),
//!     });
//! });
//! let schema = schema.build().expect("valid schema");
//! ```
//!
//! # Ordering
//!
//! Plugins run in registration order. When several plugins wrap a resolver, the first one
//! registered is the outermost wrapper: it runs first, calls the next plugin, and runs its
//! trailing code last.
//!
//! Plugins are applied after field declarations and before object- and interface-level
//! authorization is installed. Object and interface rules are therefore outside the plugin
//! chain and always run first. A field-level `.auth(...)` rule is installed when the field is
//! declared, so it sits inside the plugin chain: a plugin that calls `next` preserves it, while a
//! plugin that replaces the resolver (such as [`MockPlugin`]) bypasses it.
//!
//! # Coverage
//!
//! Plugins apply to every object field, interface field, query field, mutation field and
//! subscription field, including fields declared before or after [`crate::Schema::plugin`],
//! fields on reopened types, runtime-named types, and raw fields. Interface fields have no
//! resolvers, so only [`Plugin::field`] runs for them.
//!
//! Types added directly through [`Plugin::configure`] are not visited: they did not come through
//! the [`Schema`](crate::Schema) builders and have no model-backed field declaration.

use std::{
    sync::Arc,
    time::{Duration, Instant},
};

use gql::{Name, TypeRef};
use indexmap::IndexMap;

use crate::field::property_resolver;

/// Information about one field, available while a schema is built.
///
/// The metadata is a snapshot taken immediately before [`Plugin::field`] runs. Changing the
/// field through the `&mut gql::Field` does not change this snapshot. Each plugin receives a
/// fresh snapshot, so a later `field` hook sees an earlier plugin's changes, and a later
/// [`Plugin::wrap_resolve`] or [`Plugin::wrap_subscribe`] sees the fresh field.
#[derive(Clone, Copy, Debug)]
pub struct FieldMeta<'a> {
    /// The GraphQL name of the type that declares the field.
    ///
    /// For a root field this is the configured root name (`Query` by default).
    pub type_name: &'a str,
    /// The field's GraphQL name.
    pub field_name: &'a str,
    /// The field's final declared return type, after interface inheritance and type overrides.
    pub return_type: &'a TypeRef,
    /// The field's final arguments, in declaration order.
    pub args: &'a IndexMap<Name, gql::InputValue>,
    /// Whether the field belongs to a query, mutation or subscription root.
    pub is_root: bool,
    /// Whether the field is a subscription root field.
    pub is_subscription: bool,
}

impl FieldMeta<'_> {
    /// The field's path as `Type.field`, the same spelling [`MockPlugin`] accepts.
    #[must_use]
    pub fn path(&self) -> String {
        format!("{}.{}", self.type_name, self.field_name)
    }
}

/// A schema extension applied to every field when the schema is built.
///
/// Implement this trait to inspect or change field metadata, wrap resolvers, wrap subscriptions,
/// or configure the underlying [`gql::SchemaBuilder`]. The default methods make every hook
/// optional.
///
/// `Ctx` is the schema's context type, the same one resolvers receive.
pub trait Plugin<Ctx: 'static>: Send + Sync + 'static {
    /// Inspect or change one field's definition.
    ///
    /// This runs once per field at build time, in registration order. It can change the
    /// description, deprecation reason, complexity estimate, directives, arguments or type.
    fn field(&self, _meta: &FieldMeta<'_>, _field: &mut gql::Field) {}

    /// Wrap a field's resolver.
    ///
    /// Wrappers compose in registration order with the first registered plugin outermost. The
    /// returned function must eventually call `next` unless the plugin deliberately replaces the
    /// resolver (for example a mock).
    fn wrap_resolve(&self, _meta: &FieldMeta<'_>, next: gql::ResolveFn) -> gql::ResolveFn {
        next
    }

    /// Wrap a subscription field's `subscribe` resolver.
    ///
    /// The per-event `resolve` function is wrapped by [`Plugin::wrap_resolve`]; this hook wraps
    /// the call that creates the event stream.
    fn wrap_subscribe(&self, _meta: &FieldMeta<'_>, next: gql::SubscribeFn) -> gql::SubscribeFn {
        next
    }

    /// Configure the underlying `gql` schema builder.
    ///
    /// Runs once, after every type and root object has been registered. Use it to add directive
    /// definitions, extra types or schema-level directives. It cannot visit or reprocess the
    /// fields the builder already contains.
    fn configure(&self, _builder: &mut gql::SchemaBuilder) {}
}

/// The schema collections a plugin pass visits.
pub(crate) struct PluginTargets<'a> {
    pub objects: &'a mut IndexMap<Name, gql::ObjectType>,
    pub interfaces: &'a mut IndexMap<Name, gql::InterfaceType>,
    pub query: &'a mut [gql::Field],
    pub mutation: &'a mut [gql::Field],
    pub subscription: &'a mut [gql::Field],
    pub query_name: &'a str,
    pub mutation_name: &'a str,
    pub subscription_name: &'a str,
}

/// Apply every plugin to the schema's fields before the schema is frozen.
///
/// Object- and interface-level authorization is installed after this function, so those rules
/// remain outside the plugin chain.
#[cfg_attr(
    feature = "tracing",
    tracing::instrument(
        skip_all,
        level = "debug",
        name = "gurafi.schema.plugins",
        fields(plugins = plugins.len(), fields_visited = tracing::field::Empty)
    )
)]
pub(crate) fn apply_plugins<Ctx: 'static>(
    plugins: &[Arc<dyn Plugin<Ctx>>],
    targets: PluginTargets<'_>,
) {
    if plugins.is_empty() {
        return;
    }

    #[cfg(feature = "tracing")]
    let fields_visited = {
        let targets = &targets;
        targets
            .objects
            .values()
            .map(|object| object.fields.len())
            .sum::<usize>()
            + targets
                .interfaces
                .values()
                .map(|interface| interface.fields.len())
                .sum::<usize>()
            + targets.query.len()
            + targets.mutation.len()
            + targets.subscription.len()
    };

    let PluginTargets {
        objects,
        interfaces,
        query,
        mutation,
        subscription,
        query_name,
        mutation_name,
        subscription_name,
    } = targets;

    for (name, object) in objects.iter_mut() {
        for field in object.fields.values_mut() {
            apply_to_field(plugins, name, field, false, false, true);
        }
    }
    for (name, interface) in interfaces.iter_mut() {
        for field in interface.fields.values_mut() {
            apply_to_field(plugins, name, field, false, false, false);
        }
    }
    for field in query.iter_mut() {
        apply_to_field(plugins, query_name, field, true, false, true);
    }
    for field in mutation.iter_mut() {
        apply_to_field(plugins, mutation_name, field, true, false, true);
    }
    for field in subscription.iter_mut() {
        apply_to_field(plugins, subscription_name, field, true, true, true);
    }

    #[cfg(feature = "tracing")]
    {
        let span = tracing::Span::current();
        if !span.is_disabled() {
            span.record("fields_visited", fields_visited);
        }
    }
}

fn apply_to_field<Ctx: 'static>(
    plugins: &[Arc<dyn Plugin<Ctx>>],
    type_name: &str,
    field: &mut gql::Field,
    is_root: bool,
    is_subscription: bool,
    has_resolvers: bool,
) {
    // `FieldMeta` borrows the field's type and arguments, so the `field` hook needs a snapshot:
    // it receives both the metadata and a mutable reference to the same field. Rebuild the
    // snapshot for each plugin so a later plugin sees changes made by an earlier one.
    for plugin in plugins {
        let field_name = field.name.clone();
        let return_type = field.ty.clone();
        let args = field.args.clone();
        let meta = FieldMeta {
            type_name,
            field_name: &field_name,
            return_type: &return_type,
            args: &args,
            is_root,
            is_subscription,
        };
        plugin.field(&meta, field);
    }

    if !has_resolvers {
        return;
    }

    // The `field` hook may have changed the type or arguments; wrappers see the fresh definition.
    let field_name = field.name.clone();
    let return_type = field.ty.clone();
    let args = field.args.clone();
    let meta = FieldMeta {
        type_name,
        field_name: &field_name,
        return_type: &return_type,
        args: &args,
        is_root,
        is_subscription,
    };

    // A raw field may have no resolver. Give wrappers a property lookup to call, but only keep it
    // if some plugin actually wraps: a metadata-only plugin must not disable the executor's leaf
    // fast path.
    //
    // A synchronous property resolver cannot be handed to `wrap_resolve` without boxing it, so it
    // is materialized as the chain's base. When no plugin wraps, the synchronous hook is restored:
    // metadata-only plugins keep the allocation-free path.
    let sync = field.sync_resolve.clone();
    let had_async = field.resolve.is_some();
    let had_resolver = had_async || sync.is_some();
    let base = field
        .take_resolve()
        .unwrap_or_else(|| property_resolver(field.name.clone()));
    let mut resolve = Arc::clone(&base);
    for plugin in plugins.iter().rev() {
        resolve = plugin.wrap_resolve(&meta, resolve);
    }
    if Arc::ptr_eq(&resolve, &base) {
        match sync {
            Some(sync) if !had_async => field.sync_resolve = Some(sync),
            _ if had_resolver => field.resolve = Some(resolve),
            _ => {}
        }
    } else {
        field.resolve = Some(resolve);
    }
    if let Some(mut subscribe) = field.subscribe.take() {
        for plugin in plugins.iter().rev() {
            subscribe = plugin.wrap_subscribe(&meta, subscribe);
        }
        field.subscribe = Some(subscribe);
    }
}

/// One event emitted by [`TracingPlugin`].
#[derive(Clone, Copy, Debug)]
pub struct TraceEvent<'a> {
    /// Whether the field is starting or finishing.
    pub phase: TracePhase,
    /// The declaring type's GraphQL name.
    pub type_name: &'a str,
    /// The field's GraphQL name.
    pub field_name: &'a str,
    /// The field's declared return type.
    pub return_type: &'a TypeRef,
    /// Whether the field is a root field.
    pub is_root: bool,
    /// Whether the field is a subscription root field.
    pub is_subscription: bool,
    /// Time spent in the field, present on [`TracePhase::Finish`].
    pub elapsed: Option<Duration>,
    /// Whether the field returned an error, present on [`TracePhase::Finish`].
    pub failed: bool,
}

/// The point in a field's execution a [`TraceEvent`] describes.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum TracePhase {
    /// The resolver or subscription factory is about to run.
    Start,
    /// The resolver or subscription factory completed.
    Finish,
}

/// A dependency-free tracing plugin.
///
/// The observer receives a `Start` event and a `Finish` event for every field resolution, plus
/// the same pair around a subscription's `subscribe` call. Because `gurafi` does not depend on a
/// particular logging crate, the observer is just a closure: bridge it to `tracing`, OpenTelemetry
/// or a metrics registry in one line.
///
/// A panic cannot be caught here, but the observer runs in the resolver future, so a dropped
/// future simply emits no `Finish` event; there is no guard to leak.
///
/// ```
/// use gurafi::{Schema, TraceEvent, TracePhase, TracingPlugin};
///
/// struct Ctx;
///
/// let mut schema = Schema::<Ctx>::new();
/// schema.plugin(TracingPlugin::new(|event: &TraceEvent<'_>| {
///     if event.phase == TracePhase::Start {
///         // `tracing::info_span!(...)` or a metrics counter would go here.
///         eprintln!("start {}.{}", event.type_name, event.field_name);
///     }
/// }));
/// # schema.query(|q| {
/// #     q.field("ok", |_: gurafi::NoArgs, _: &Ctx| true);
/// # });
/// # let _ = schema.build().expect("valid schema");
/// ```
pub struct TracingPlugin {
    observer: Arc<dyn Fn(&TraceEvent<'_>) + Send + Sync>,
}

impl TracingPlugin {
    /// Create a tracing plugin from an observer closure.
    ///
    /// The closure is called synchronously in the field's future. Keep it cheap, and do not
    /// block: it runs on every resolution.
    pub fn new(observer: impl Fn(&TraceEvent<'_>) + Send + Sync + 'static) -> Self {
        Self {
            observer: Arc::new(observer),
        }
    }

    /// Create a tracing plugin that writes one line per event to stderr.
    ///
    /// This is a convenience for examples and local debugging; production servers should pass
    /// their own observer to [`TracingPlugin::new`].
    #[must_use]
    pub fn stderr() -> Self {
        Self::new(|event| match event.phase {
            TracePhase::Start => eprintln!(
                "graphql start {}.{} ({})",
                event.type_name, event.field_name, event.return_type
            ),
            TracePhase::Finish => eprintln!(
                "graphql finish {}.{} in {:?}{}",
                event.type_name,
                event.field_name,
                event.elapsed.unwrap_or_default(),
                if event.failed { " (error)" } else { "" }
            ),
        })
    }
}

impl<Ctx: 'static> Plugin<Ctx> for TracingPlugin {
    fn wrap_resolve(&self, meta: &FieldMeta<'_>, next: gql::ResolveFn) -> gql::ResolveFn {
        let observer = Arc::clone(&self.observer);
        let type_name: Arc<str> = Arc::from(meta.type_name);
        let field_name: Arc<str> = Arc::from(meta.field_name);
        let return_type = meta.return_type.clone();
        let is_root = meta.is_root;
        let is_subscription = meta.is_subscription;
        Arc::new(move |context| {
            let next = Arc::clone(&next);
            let observer = Arc::clone(&observer);
            let type_name = Arc::clone(&type_name);
            let field_name = Arc::clone(&field_name);
            let return_type = return_type.clone();
            Box::pin(async move {
                observer(&TraceEvent {
                    phase: TracePhase::Start,
                    type_name: &type_name,
                    field_name: &field_name,
                    return_type: &return_type,
                    is_root,
                    is_subscription,
                    elapsed: None,
                    failed: false,
                });
                let started = Instant::now();
                let result = next(context).await;
                observer(&TraceEvent {
                    phase: TracePhase::Finish,
                    type_name: &type_name,
                    field_name: &field_name,
                    return_type: &return_type,
                    is_root,
                    is_subscription,
                    elapsed: Some(started.elapsed()),
                    failed: result.is_err(),
                });
                result
            })
        })
    }

    fn wrap_subscribe(&self, meta: &FieldMeta<'_>, next: gql::SubscribeFn) -> gql::SubscribeFn {
        let observer = Arc::clone(&self.observer);
        let type_name: Arc<str> = Arc::from(meta.type_name);
        let field_name: Arc<str> = Arc::from(meta.field_name);
        let return_type = meta.return_type.clone();
        let is_root = meta.is_root;
        let is_subscription = meta.is_subscription;
        Arc::new(move |context| {
            let next = Arc::clone(&next);
            let observer = Arc::clone(&observer);
            let type_name = Arc::clone(&type_name);
            let field_name = Arc::clone(&field_name);
            let return_type = return_type.clone();
            Box::pin(async move {
                observer(&TraceEvent {
                    phase: TracePhase::Start,
                    type_name: &type_name,
                    field_name: &field_name,
                    return_type: &return_type,
                    is_root,
                    is_subscription,
                    elapsed: None,
                    failed: false,
                });
                let started = Instant::now();
                let result = next(context).await;
                observer(&TraceEvent {
                    phase: TracePhase::Finish,
                    type_name: &type_name,
                    field_name: &field_name,
                    return_type: &return_type,
                    is_root,
                    is_subscription,
                    elapsed: Some(started.elapsed()),
                    failed: result.is_err(),
                });
                result
            })
        })
    }

    fn configure(&self, _builder: &mut gql::SchemaBuilder) {}
}

impl std::fmt::Debug for TracingPlugin {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("TracingPlugin").finish_non_exhaustive()
    }
}

/// A plugin that writes a response extension from a resolver wrapper.
///
/// This is the smallest useful example of a plugin that calls
/// [`gql::ExecutionContext::set_extension`]. It can write on every field or only on root fields.
/// The extension key is the same on every write, so the last write wins; use a custom plugin when
/// per-field values must accumulate.
///
/// ```
/// use gurafi::{NoArgs, ResponseExtensionPlugin, Schema};
///
/// struct Ctx;
///
/// let mut schema = Schema::<Ctx>::new();
/// schema.plugin(ResponseExtensionPlugin::new("servedBy", "gurafi").root_only());
/// schema.query(|q| {
///     q.field("ok", |_: NoArgs, _: &Ctx| true);
/// });
/// # let _ = schema.build().expect("valid schema");
/// ```
#[derive(Clone, Debug)]
pub struct ResponseExtensionPlugin {
    key: Arc<str>,
    value: gql::Value,
    root_only: bool,
}

impl ResponseExtensionPlugin {
    /// Write `value` under `key` into the response's `extensions`.
    pub fn new(key: impl Into<Arc<str>>, value: impl Into<gql::Value>) -> Self {
        Self {
            key: key.into(),
            value: value.into(),
            root_only: false,
        }
    }

    /// Only set the extension for root query, mutation and subscription fields.
    #[must_use]
    pub fn root_only(mut self) -> Self {
        self.root_only = true;
        self
    }
}

impl<Ctx: 'static> Plugin<Ctx> for ResponseExtensionPlugin {
    fn wrap_resolve(&self, meta: &FieldMeta<'_>, next: gql::ResolveFn) -> gql::ResolveFn {
        if self.root_only && !meta.is_root {
            return next;
        }
        let key = Arc::clone(&self.key);
        let value = self.value.clone();
        Arc::new(move |context| {
            context
                .context
                .set_extension(Arc::clone(&key), value.clone());
            next(context)
        })
    }
}

/// A test/dev plugin that replaces named fields with fixed values.
///
/// A mock is an exact `Type.field` match, for example `Query.giraffe` or
/// `Giraffe.name`. The replacement resolver still receives coerced arguments and the request
/// context, but it does not call the original resolver, so the original argument validator and
/// any field-level `.auth(...)` rule do not run. Object- and interface-level authorization is
/// installed after plugins and still runs before the mock.
///
/// Only `resolve` is replaced. A mocked subscription still uses the real `subscribe` stream, and
/// then returns the mocked value for every event.
///
/// ```
/// use gurafi::{MockPlugin, NoArgs, Object, Schema};
///
/// struct Ctx;
///
/// #[derive(Object)]
/// struct Giraffe {
///     name: String,
/// }
///
/// let mut schema = Schema::<Ctx>::new();
/// schema.plugin(
///     MockPlugin::new()
///         .mock("Query", "giraffe", "Melman")
///         .mock("Giraffe", "name", "Mocked"),
/// );
/// schema.object::<Giraffe>(|o| {
///     o.expose(gurafi::expose!(Giraffe::name));
/// });
/// schema.query(|q| {
///     q.field("giraffe", |_: NoArgs, _: &Ctx| Giraffe {
///         name: "real".into(),
///     });
/// });
/// # let _ = schema.build().expect("valid schema");
/// ```
#[derive(Clone, Debug, Default)]
pub struct MockPlugin {
    mocks: IndexMap<(String, String), gql::Value>,
}

impl MockPlugin {
    /// Create a plugin with no mocks.
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }

    /// Replace `Type.field` with `value`.
    #[must_use]
    pub fn mock(
        mut self,
        type_name: impl Into<String>,
        field_name: impl Into<String>,
        value: impl Into<gql::Value>,
    ) -> Self {
        self.mocks
            .insert((type_name.into(), field_name.into()), value.into());
        self
    }

    /// Replace a field by its `Type.field` path.
    ///
    /// Returns the plugin unchanged when the path has no `.`; use [`MockPlugin::mock`] to be
    /// explicit about a type and field name.
    #[must_use]
    pub fn mock_path(self, path: &str, value: impl Into<gql::Value>) -> Self {
        match path.split_once('.') {
            Some((type_name, field_name)) => self.mock(type_name, field_name, value),
            None => self,
        }
    }
}

impl<Ctx: 'static> Plugin<Ctx> for MockPlugin {
    fn wrap_resolve(&self, meta: &FieldMeta<'_>, next: gql::ResolveFn) -> gql::ResolveFn {
        if let Some(value) = self
            .mocks
            .get(&(meta.type_name.to_owned(), meta.field_name.to_owned()))
        {
            let value = value.clone();
            return Arc::new(move |_context| {
                let value = value.clone();
                Box::pin(async move { Ok(value) })
            });
        }
        next
    }
}
