//! The Relay `Node` interface and root `node`/`nodes` fields.
//!
//! [`Relay::node`] registers a model as a Node, adds its global-ID resolver and makes the model
//! implement the shared `Node` interface. The first registration installs `Query.node` and
//! `Query.nodes`; every later registration adds its type to the same dispatcher.

use std::{
    collections::HashMap,
    sync::{
        Arc, Mutex, PoisonError, RwLock,
        atomic::{AtomicBool, Ordering},
    },
};

use futures::StreamExt;
use gurafi::{
    BoxFuture, ID, Input, InterfaceRef, NoArgs, ObjectModel, OutputType, RequestCache, Schema,
    gql::{GraphQLError, TypeRef, Value},
};

use crate::{GlobalId, RelayError, decode_global_id};

type LoadOne<M, Ctx> = Arc<
    dyn for<'r> Fn(String, &'r Ctx) -> BoxFuture<'r, Result<Option<M>, GraphQLError>> + Send + Sync,
>;
type LoadMany<M, Ctx> = Arc<
    dyn for<'r> Fn(Vec<String>, &'r Ctx) -> BoxFuture<'r, Result<Vec<Option<M>>, GraphQLError>>
        + Send
        + Sync,
>;
type LoadOneValue<Ctx> = Arc<
    dyn for<'r> Fn(String, &'r Ctx) -> BoxFuture<'r, Result<Option<Value>, GraphQLError>>
        + Send
        + Sync,
>;
type LoadManyValue<Ctx> = Arc<
    dyn for<'r> Fn(Vec<String>, &'r Ctx) -> BoxFuture<'r, Result<Vec<Option<Value>>, GraphQLError>>
        + Send
        + Sync,
>;
type CacheAccessor<Ctx> = Arc<dyn Fn(&Ctx) -> RequestCache + Send + Sync + 'static>;
type NodeIdGetter<M> = Arc<dyn Fn(&M) -> String + Send + Sync>;

/// Maximum concurrent `load_one` calls used when a node type has no `load_many`.
const MAX_NODE_FANOUT: usize = 16;

struct NodeTypeEntry<Ctx> {
    typename: String,
    load_one: LoadOneValue<Ctx>,
    load_many: LoadManyValue<Ctx>,
}

impl<Ctx> Clone for NodeTypeEntry<Ctx> {
    fn clone(&self) -> Self {
        Self {
            typename: self.typename.clone(),
            load_one: Arc::clone(&self.load_one),
            load_many: Arc::clone(&self.load_many),
        }
    }
}

/// Arguments of the generated `Query.node` field.
#[derive(Input)]
pub struct NodeArgs {
    /// The global ID of the node.
    pub id: ID,
}

/// Arguments of the generated `Query.nodes` field.
#[derive(Input)]
pub struct NodesArgs {
    /// The global IDs of the nodes.
    pub ids: Vec<ID>,
}

/// The output marker used by the generated root fields.
///
/// The concrete value is always a `gql::Value::External` holding the registered model, so the
/// executor's normal `is_type_of` checks select the right concrete type for the `Node` interface.
#[doc(hidden)]
#[derive(Clone, Copy, Debug)]
pub struct NodeValue;

impl OutputType for NodeValue {
    fn output_type() -> TypeRef {
        TypeRef::named("Node")
    }
}

/// Builder passed to [`Relay::node`].
pub struct NodeBuilder<M: ObjectModel, Ctx: 'static> {
    id: Option<NodeIdGetter<M>>,
    load_one: Option<LoadOne<M, Ctx>>,
    load_many: Option<LoadMany<M, Ctx>>,
}

impl<M: ObjectModel, Ctx: 'static> Default for NodeBuilder<M, Ctx> {
    fn default() -> Self {
        Self {
            id: None,
            load_one: None,
            load_many: None,
        }
    }
}

impl<M: ObjectModel, Ctx: 'static + Sync> NodeBuilder<M, Ctx> {
    /// Set the model's application-local ID.
    ///
    /// The helper encodes it as a global ID for the `id` field. Any value convertible to
    /// `String` works, so `String`, `&str` and `gurafi::ID` are all accepted.
    pub fn id<F, S>(&mut self, get: F) -> &mut Self
    where
        F: Fn(&M) -> S + Send + Sync + 'static,
        S: Into<String>,
    {
        self.id = Some(Arc::new(move |model| get(model).into()));
        self
    }

    /// Load one node by its local ID.
    ///
    /// The future may borrow `ctx`; build it with `Box::pin(async move { ... })`, matching
    /// `gurafi`'s async resolver convention. Return `None` when the node does not exist.
    ///
    /// ```
    /// # use gurafi::Object;
    /// # use gurafi_relay::Relay;
    /// # #[derive(Object)] struct User { id: String }
    /// # struct Ctx;
    /// # let mut schema = gurafi::Schema::<Ctx>::new();
    /// # let mut relay = Relay::<Ctx>::new();
    /// relay.node::<User>(&mut schema, |n| {
    ///     n.id(|user: &User| user.id.clone());
    ///     n.load_one(|id: String, _ctx: &Ctx| {
    ///         Box::pin(async move { Some(User { id }) })
    ///     });
    /// });
    /// ```
    pub fn load_one<F>(&mut self, loader: F) -> &mut Self
    where
        F: for<'r> Fn(String, &'r Ctx) -> BoxFuture<'r, Option<M>> + Send + Sync + 'static,
    {
        self.load_one = Some(Arc::new(move |id, ctx| {
            let future = loader(id, ctx);
            Box::pin(async move { Ok(future.await) })
        }));
        self
    }

    /// Load one node, reporting a backend failure as a GraphQL error.
    pub fn load_one_result<F>(&mut self, loader: F) -> &mut Self
    where
        F: for<'r> Fn(String, &'r Ctx) -> BoxFuture<'r, Result<Option<M>, GraphQLError>>
            + Send
            + Sync
            + 'static,
    {
        self.load_one = Some(Arc::new(loader));
        self
    }

    /// Load several nodes by local ID, in the same order as `ids`.
    ///
    /// The vector must contain one entry per requested ID. Missing nodes are `None`.
    pub fn load_many<F>(&mut self, loader: F) -> &mut Self
    where
        F: for<'r> Fn(Vec<String>, &'r Ctx) -> BoxFuture<'r, Vec<Option<M>>>
            + Send
            + Sync
            + 'static,
    {
        self.load_many = Some(Arc::new(move |ids, ctx| {
            let future = loader(ids, ctx);
            Box::pin(async move { Ok(future.await) })
        }));
        self
    }

    /// Load several nodes, reporting a backend failure as a GraphQL error.
    pub fn load_many_result<F>(&mut self, loader: F) -> &mut Self
    where
        F: for<'r> Fn(Vec<String>, &'r Ctx) -> BoxFuture<'r, Result<Vec<Option<M>>, GraphQLError>>
            + Send
            + Sync
            + 'static,
    {
        self.load_many = Some(Arc::new(loader));
        self
    }

    fn finish(
        self,
    ) -> Result<(NodeTypeEntry<Ctx>, Arc<dyn Fn(&M) -> String + Send + Sync>), RelayError> {
        let id = self
            .id
            .ok_or_else(|| RelayError::new("a node type must declare `id(...)`"))?;
        if self.load_one.is_none() && self.load_many.is_none() {
            return Err(RelayError::new(
                "a node type must declare `load_one(...)` or `load_many(...)`",
            ));
        }

        let load_one: LoadOneValue<Ctx> = if let Some(loader) = self.load_one {
            Arc::new(move |id, ctx| {
                let loader = Arc::clone(&loader);
                Box::pin(async move { loader(id, ctx).await.map(|node| node.map(Value::external)) })
            })
        } else {
            let loader = Arc::clone(
                self.load_many
                    .as_ref()
                    .ok_or_else(|| RelayError::new("missing node loader"))?,
            );
            Arc::new(move |id, ctx| {
                let loader = Arc::clone(&loader);
                Box::pin(async move {
                    Ok(loader(vec![id], ctx)
                        .await?
                        .into_iter()
                        .next()
                        .flatten()
                        .map(Value::external))
                })
            })
        };

        let load_many: LoadManyValue<Ctx> = if let Some(loader) = self.load_many {
            Arc::new(move |ids, ctx| {
                let loader = Arc::clone(&loader);
                Box::pin(async move {
                    loader(ids, ctx).await.map(|nodes| {
                        nodes
                            .into_iter()
                            .map(|node| node.map(Value::external))
                            .collect()
                    })
                })
            })
        } else {
            let loader = Arc::clone(&load_one);
            Arc::new(move |ids, ctx| {
                let loader = Arc::clone(&loader);
                Box::pin(async move {
                    // A type may configure only `load_one`. Fanning out unbounded would let one
                    // `nodes(ids:)` request issue an arbitrary number of backend calls, so run a
                    // bounded stream and restore the requested order afterwards.
                    let mut results =
                        futures::stream::iter(ids.into_iter().enumerate().map(|(index, id)| {
                            let loader = Arc::clone(&loader);
                            async move { (index, loader(id, ctx).await) }
                        }))
                        .buffer_unordered(MAX_NODE_FANOUT)
                        .collect::<Vec<_>>()
                        .await;
                    results.sort_by_key(|(index, _)| *index);
                    results
                        .into_iter()
                        .map(|(_, result)| result)
                        .collect::<Result<Vec<_>, _>>()
                })
            })
        };

        Ok((
            NodeTypeEntry {
                typename: M::NAME.to_owned(),
                load_one,
                load_many,
            },
            id,
        ))
    }
}

/// Per-request Node result cache.
///
/// Loader failures are cached too: retrying a failing backend once per selection would multiply
/// the damage during an outage.
#[derive(Default)]
struct NodeResultCache {
    entries: Mutex<HashMap<String, Result<Option<Value>, GraphQLError>>>,
}

impl NodeResultCache {
    fn get(&self, key: &str) -> Option<Result<Option<Value>, GraphQLError>> {
        self.entries
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .get(key)
            .cloned()
    }

    fn insert(&self, key: String, value: Result<Option<Value>, GraphQLError>) {
        self.entries
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .insert(key, value);
    }
}

/// The per-schema Relay registry.
///
/// Create one `Relay` per [`Schema`] and keep it alive while the schema is being assembled. It
/// remembers which shared types (`Node`, `PageInfo`) and root fields have already been installed,
/// which is what makes repeated registration idempotent.
///
/// ```
/// use gurafi::Schema;
/// use gurafi_relay::Relay;
///
/// struct Ctx;
/// let mut schema = Schema::<Ctx>::new();
/// let mut relay = Relay::<Ctx>::new();
/// # #[derive(gurafi::Object)]
/// # struct User { id: String }
/// relay.node::<User>(&mut schema, |n| {
///     n.id(|user: &User| user.id.clone());
///     n.load_one(|id, _: &Ctx| Box::pin(async move { Some(User { id }) }));
/// }).unwrap();
/// # let _ = schema;
/// ```
pub struct Relay<Ctx: 'static> {
    node_interface: Option<InterfaceRef>,
    node_root: bool,
    node_types: Arc<RwLock<Vec<NodeTypeEntry<Ctx>>>>,
    cache_accessor: Option<CacheAccessor<Ctx>>,
    allow_local_ids: Arc<AtomicBool>,
    pub(crate) page_info: bool,
    pub(crate) connection_names: Vec<String>,
    pub(crate) payload_names: Vec<String>,
}

impl<Ctx: 'static + Sync> Default for Relay<Ctx> {
    fn default() -> Self {
        Self::new()
    }
}

impl<Ctx: 'static + Sync> Relay<Ctx> {
    /// Create a Relay registry for one schema.
    #[must_use]
    pub fn new() -> Self {
        Self {
            node_interface: None,
            node_root: false,
            node_types: Arc::new(RwLock::new(Vec::new())),
            cache_accessor: None,
            allow_local_ids: Arc::new(AtomicBool::new(false)),
            page_info: false,
            connection_names: Vec::new(),
            payload_names: Vec::new(),
        }
    }

    /// Share a [`RequestCache`] from the request context.
    ///
    /// When configured, `node` and `nodes` cache results by global ID for the duration of one
    /// request. The accessor should return the cache stored in the context; without one, the
    /// generated fields still deduplicate within a single `nodes` call but do not remember
    /// results between separate fields.
    ///
    /// Configure this before the first [`Relay::node`] call: that call installs the root fields
    /// and captures the accessor.
    #[must_use]
    pub fn with_request_cache(
        mut self,
        accessor: impl Fn(&Ctx) -> RequestCache + Send + Sync + 'static,
    ) -> Self {
        self.cache_accessor = Some(Arc::new(accessor));
        self
    }

    /// Whether bare local IDs are accepted when a value is not a valid global ID.
    ///
    /// Disabled by default: a Relay client sends global IDs, and accepting bare local IDs lets a
    /// client fetch a node type it did not name and costs one loader call per registered type.
    /// Enable this only as a migration aid, and only for the period clients need it.
    ///
    /// The setting is shared with the already-installed `node`/`nodes` fields, so it can be
    /// changed after registration.
    #[must_use]
    pub fn allow_local_ids(self, allow: bool) -> Self {
        self.allow_local_ids.store(allow, Ordering::Relaxed);
        self
    }

    /// Register `M` as a Relay Node.
    ///
    /// The first call installs the `Node` interface and the root `node`/`nodes` fields; later
    /// calls add types to the same dispatcher. The model gets an `id: ID!` field resolving to its
    /// global ID unless it already declares one.
    ///
    /// # Errors
    ///
    /// Returns an error when the builder omits `id`/loaders, or when the same GraphQL type is
    /// registered twice with this Relay.
    pub fn node<M: ObjectModel>(
        &mut self,
        schema: &mut Schema<Ctx>,
        build: impl FnOnce(&mut NodeBuilder<M, Ctx>),
    ) -> Result<(), RelayError> {
        let typename = M::NAME.to_owned();
        if self
            .node_types
            .read()
            .unwrap_or_else(PoisonError::into_inner)
            .iter()
            .any(|entry| entry.typename == typename)
        {
            return Err(RelayError::new(format!(
                "node type `{typename}` is already registered with this Relay"
            )));
        }

        self.ensure_node_interface(schema);
        self.ensure_node_root(schema);

        let mut builder = NodeBuilder::<M, Ctx>::default();
        build(&mut builder);
        let (entry, id) = builder.finish()?;

        let node = self
            .node_interface
            .clone()
            .ok_or_else(|| RelayError::new("the Node interface was not installed"))?;
        schema.object::<M>(|object| {
            object.implements(&node);
            if !object.has_field("id") {
                let id = Arc::clone(&id);
                object.field("id", move |model: &M, _: NoArgs, _: &Ctx| {
                    ID::new(GlobalId::<M>::new((id)(model)).encode())
                });
            }
        });

        self.node_types
            .write()
            .unwrap_or_else(PoisonError::into_inner)
            .push(entry);
        Ok(())
    }

    fn ensure_node_interface(&mut self, schema: &mut Schema<Ctx>) {
        if self.node_interface.is_some() {
            return;
        }
        let node = schema.interface("Node", |interface| {
            interface.description("An object with a globally unique ID.");
            if !interface.has_field("id") {
                interface.field::<ID>("id");
            }
        });
        self.node_interface = Some(node);
    }

    fn ensure_node_root(&mut self, schema: &mut Schema<Ctx>) {
        if self.node_root {
            return;
        }
        self.node_root = true;

        let types = Arc::clone(&self.node_types);
        let cache = self.cache_accessor.clone();
        let allow_local = Arc::clone(&self.allow_local_ids);
        schema.query(|query| {
            let one_types = Arc::clone(&types);
            let one_cache = cache.clone();
            let one_allow_local = Arc::clone(&allow_local);
            query.field_result_async("node", move |args: NodeArgs, ctx: &Ctx| {
                let types = Arc::clone(&one_types);
                let cache = one_cache.clone();
                let allow_local = Arc::clone(&one_allow_local);
                Box::pin(async move {
                    let entries = snapshot(&types);
                    let cache = cache
                        .as_ref()
                        .map(|accessor| accessor(ctx).get_or_insert_with(NodeResultCache::default));
                    let value = resolve_one(
                        &entries,
                        cache.as_ref(),
                        ctx,
                        args.id.0,
                        allow_local.load(Ordering::Relaxed),
                    )
                    .await?;
                    Ok(value.map(gurafi::Raw::<NodeValue>::from))
                })
            });

            query.field_result_async("nodes", move |args: NodesArgs, ctx: &Ctx| {
                let types = Arc::clone(&types);
                let cache = cache.clone();
                let allow_local = Arc::clone(&allow_local);
                Box::pin(async move {
                    let entries = snapshot(&types);
                    let cache = cache
                        .as_ref()
                        .map(|accessor| accessor(ctx).get_or_insert_with(NodeResultCache::default));
                    let values = resolve_many(
                        &entries,
                        cache.as_ref(),
                        ctx,
                        &args.ids,
                        allow_local.load(Ordering::Relaxed),
                    )
                    .await?;
                    Ok(values
                        .into_iter()
                        .map(|value| value.map(gurafi::Raw::<NodeValue>::from))
                        .collect::<Vec<_>>())
                })
            });
        });
    }
}

fn snapshot<Ctx>(types: &Arc<RwLock<Vec<NodeTypeEntry<Ctx>>>>) -> Vec<NodeTypeEntry<Ctx>> {
    types.read().unwrap_or_else(PoisonError::into_inner).clone()
}

fn cache_key(typename: &str, local: &str) -> String {
    format!("{typename}:{local}")
}

#[cfg_attr(
    feature = "tracing",
    tracing::instrument(
        skip_all,
        level = "debug",
        name = "relay.node",
        fields(
            requested = 1usize,
            allow_local,
            cache_hit = tracing::field::Empty,
            loader_calls = tracing::field::Empty,
            resolved = tracing::field::Empty,
            errors = tracing::field::Empty,
        )
    )
)]
async fn resolve_one<Ctx: 'static>(
    entries: &[NodeTypeEntry<Ctx>],
    cache: Option<&Arc<NodeResultCache>>,
    ctx: &Ctx,
    raw: String,
    allow_local: bool,
) -> Result<Option<Value>, GraphQLError> {
    match decode_global_id(&raw) {
        Ok(decoded) => {
            let Some(entry) = entries
                .iter()
                .find(|entry| entry.typename == decoded.typename)
            else {
                return Err(GraphQLError::new(format!(
                    "Global ID names unknown node type `{}`.",
                    decoded.typename
                )));
            };
            let key = cache_key(&entry.typename, &decoded.local_id);
            if let Some(cached) = cache.and_then(|cache| cache.get(&key)) {
                #[cfg(feature = "tracing")]
                {
                    let span = tracing::Span::current();
                    if !span.is_disabled() {
                        span.record("cache_hit", true);
                    }
                }
                return cached;
            }
            let value = (entry.load_one)(decoded.local_id, ctx).await;
            #[cfg(feature = "tracing")]
            {
                let span = tracing::Span::current();
                if !span.is_disabled() {
                    span.record("cache_hit", false);
                    span.record("loader_calls", 1usize);
                    span.record("resolved", matches!(&value, Ok(Some(_))));
                    if value.is_err() {
                        span.record("errors", 1usize);
                    }
                }
            }
            if let Some(cache) = cache {
                cache.insert(key, value.clone());
            }
            value
        }
        Err(error) => {
            if !allow_local {
                return Err(error);
            }
            for entry in entries {
                let key = cache_key(&entry.typename, &raw);
                if let Some(cached) = cache.and_then(|cache| cache.get(&key)) {
                    match cached {
                        Ok(Some(value)) => return Ok(Some(value)),
                        Ok(None) => continue,
                        Err(error) => return Err(error),
                    }
                }
                let value = (entry.load_one)(raw.clone(), ctx).await;
                if let Some(cache) = cache {
                    cache.insert(key, value.clone());
                }
                match value {
                    Ok(Some(value)) => return Ok(Some(value)),
                    Ok(None) => {}
                    Err(error) => return Err(error),
                }
            }
            Ok(None)
        }
    }
}

#[cfg_attr(
    feature = "tracing",
    tracing::instrument(
        skip_all,
        level = "debug",
        name = "relay.nodes",
        fields(
            requested = ids.len(),
            allow_local,
            cache_hits = tracing::field::Empty,
            loader_calls = tracing::field::Empty,
            resolved = tracing::field::Empty,
            nulls = tracing::field::Empty,
            errors = tracing::field::Empty,
        )
    )
)]
async fn resolve_many<Ctx: 'static>(
    entries: &[NodeTypeEntry<Ctx>],
    cache: Option<&Arc<NodeResultCache>>,
    ctx: &Ctx,
    ids: &[ID],
    allow_local: bool,
) -> Result<Vec<Option<Value>>, GraphQLError> {
    #[cfg(feature = "tracing")]
    let mut cache_hits = 0usize;
    #[cfg(feature = "tracing")]
    let mut loader_calls = 0usize;
    let mut results: Vec<Option<Value>> = vec![None; ids.len()];
    let mut resolved = vec![false; ids.len()];
    let mut global: Vec<(&str, Vec<(usize, String)>)> = Vec::new();
    let mut local: Vec<(usize, String)> = Vec::new();

    for (index, id) in ids.iter().enumerate() {
        match decode_global_id(&id.0) {
            Ok(decoded) => {
                let Some(entry) = entries
                    .iter()
                    .find(|entry| entry.typename == decoded.typename)
                else {
                    // A list of nullable nodes can report one unusable item without nulling the
                    // whole field. There is no per-item GraphQL error channel from a resolver, so
                    // the slot stays `null`; singular `node` still reports the unknown type.
                    continue;
                };
                let key = cache_key(&entry.typename, &decoded.local_id);
                if let Some(cached) = cache.and_then(|cache| cache.get(&key)) {
                    #[cfg(feature = "tracing")]
                    {
                        cache_hits += 1;
                    }
                    match cached {
                        Ok(value) => {
                            results[index] = value;
                            resolved[index] = true;
                        }
                        Err(error) => return Err(error),
                    }
                } else if let Some((_, requested)) = global
                    .iter_mut()
                    .find(|(typename, _)| *typename == entry.typename)
                {
                    requested.push((index, decoded.local_id));
                } else {
                    global.push((entry.typename.as_str(), vec![(index, decoded.local_id)]));
                }
            }
            Err(_) => {
                if !allow_local {
                    // As above: a malformed ID is one bad list item, not a whole-field failure.
                    continue;
                }
                local.push((index, id.0.clone()));
            }
        }
    }

    for (typename, requested) in global {
        let Some(entry) = entries.iter().find(|entry| entry.typename == typename) else {
            continue;
        };
        let mut unique: Vec<String> = Vec::new();
        for (_, local_id) in &requested {
            if !unique.contains(local_id) {
                unique.push(local_id.clone());
            }
        }
        #[cfg(feature = "tracing")]
        {
            loader_calls += 1;
        }
        let values = (entry.load_many)(unique.clone(), ctx).await?;
        let by_id: HashMap<&str, &Option<Value>> = unique
            .iter()
            .map(String::as_str)
            .zip(values.iter())
            .collect();
        for (index, local_id) in requested {
            let value = by_id
                .get(local_id.as_str())
                .and_then(|value| (*value).clone());
            if let Some(cache) = cache {
                cache.insert(cache_key(typename, &local_id), Ok(value.clone()));
            }
            results[index] = value;
            resolved[index] = true;
        }
    }

    // Local IDs are not tied to a type, so try each registered loader. The first loader with a
    // value wins; this is deliberately a compatibility path rather than the primary dispatch.
    if !local.is_empty() {
        let unique: Vec<String> =
            local
                .iter()
                .map(|(_, id)| id.clone())
                .fold(Vec::new(), |mut unique, id| {
                    if !unique.contains(&id) {
                        unique.push(id);
                    }
                    unique
                });
        for entry in entries {
            if local.iter().all(|(index, _)| resolved[*index]) {
                break;
            }
            for (index, local_id) in &local {
                if resolved[*index] {
                    continue;
                }
                let key = cache_key(&entry.typename, local_id);
                if let Some(cached) = cache.and_then(|cache| cache.get(&key)) {
                    match cached {
                        Ok(Some(value)) => {
                            results[*index] = Some(value);
                            resolved[*index] = true;
                        }
                        Ok(None) => {}
                        Err(error) => return Err(error),
                    }
                    continue;
                }
            }
            if local.iter().all(|(index, _)| resolved[*index]) {
                break;
            }

            #[cfg(feature = "tracing")]
            {
                loader_calls += 1;
            }
            let values = (entry.load_many)(unique.clone(), ctx).await?;
            for (index, local_id) in &local {
                if resolved[*index] {
                    continue;
                }
                let value = unique
                    .iter()
                    .position(|id| id == local_id)
                    .and_then(|position| values.get(position))
                    .and_then(Clone::clone);
                if value.is_some() {
                    if let Some(cache) = cache {
                        cache.insert(cache_key(&entry.typename, local_id), Ok(value.clone()));
                    }
                    results[*index] = value;
                    resolved[*index] = true;
                }
            }
        }
    }

    #[cfg(feature = "tracing")]
    {
        let span = tracing::Span::current();
        if !span.is_disabled() {
            let resolved = results.iter().filter(|value| value.is_some()).count();
            span.record("cache_hits", cache_hits);
            span.record("loader_calls", loader_calls);
            span.record("resolved", resolved);
            span.record("nulls", results.len().saturating_sub(resolved));
        }
    }
    Ok(results)
}
