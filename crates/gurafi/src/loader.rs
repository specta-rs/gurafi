//! Batched, request-scoped data loading.
//!
//! A [`Loader`] solves the *N+1 query* problem. Instead of one resolver issuing one read per
//! object, resolvers ask a loader for a key; loads issued together are collected into a single
//! call to the loader's batch function, duplicate keys share one read, and the result is cached
//! for the rest of the request.
//!
//! # Example
//!
//! ```
//! use gurafi::{ID, Input, Loader, Object, RequestCache, Schema, expose};
//!
//! #[derive(Clone, Object)]
//! struct User {
//!     id: ID,
//!     name: String,
//! }
//!
//! #[derive(Input)]
//! struct UserArgs {
//!     id: ID,
//! }
//!
//! struct Ctx {
//!     users: Loader<ID, User>,
//!     cache: RequestCache,
//! }
//!
//! impl Ctx {
//!     /// One context per request: a fresh loader and a fresh cache.
//!     fn new() -> Self {
//!         let cache = RequestCache::new();
//!         let users = cache.get_or_insert_with(|| {
//!             Loader::<ID, User>::new(|ids: Vec<ID>| async move {
//!                 // One "database" round-trip for every id collected in this tick.
//!                 ids.into_iter()
//!                     .map(|id| {
//!                         Ok(User {
//!                             name: format!("user-{id}"),
//!                             id,
//!                         })
//!                     })
//!                     .collect()
//!             })
//!         });
//!         Self {
//!             users: (*users).clone(),
//!             cache,
//!         }
//!     }
//! }
//!
//! let mut schema = Schema::<Ctx>::new();
//! schema.object::<User>(|o| {
//!     o.expose(expose!(User::id));
//!     o.expose(expose!(User::name));
//! });
//! schema.query(|q| {
//!     q.field_result_async("user", |args: UserArgs, ctx: &Ctx| {
//!         Box::pin(async move { ctx.users.load(args.id).await })
//!     });
//! });
//!
//! let schema = schema.build().expect("valid schema");
//! assert!(schema.object_type("User").is_some());
//! ```
//!
//! # Batching semantics
//!
//! A load is queued synchronously the first time its future is polled, then yields once before
//! the queue is flushed. The flush happens on the loader's next poll, and it takes every key that
//! was queued in the meantime. In practice that means:
//!
//! - Fields that the executor polls in the same scheduler round share one batch.
//! - A resolver that does `load(a).await` and then `load(b).await` does not batch the two: the
//!   first await completes the tick. Use [`Loader::load_many`] when one resolver needs several
//!   keys.
//! - There is no timer or background task. The batch window is exactly one yield of the
//!   executor, which makes it work under Tokio, `futures`, `async-std`, and a test `block_on`
//!   without spawning anything. Because the window is a yield and not wall-clock time, batches
//!   are deterministic with respect to how the executor schedules tasks; a load that arrives a
//!   full round later starts a new batch.
//! - At most [`LoaderConfig::max_batch_size`] keys go into one batch; a larger queue is split,
//!   and the split batches run concurrently.
//! - A list of objects is completed one item at a time by the executor, so one load per item is
//!   one batch per item rather than one batch for the list. The cache still removes duplicates,
//!   and a parent resolver can call [`Loader::load_many`] with all of its children to batch them
//!   explicitly.
//!
//! The batch function is called with the keys *in queue order*, and must return one
//! [`Result`] per key, in the same order. An extra result is ignored. A missing result is a
//! contract violation: the key is retried in a new batch, up to [`LoaderConfig::max_attempts`],
//! and if the batch function still refuses to produce a result the load fails with an error
//! cloned from the batch (or panics when the batch reported no error at all). In normal use the
//! batch function returns exactly one entry per key and none of this is observable.
//!
//! # Caching and deduplication
//!
//! The loader remembers the outcome of every key it has resolved, so a second load of the same
//! key never calls the batch function again. Loads that arrive while a key is already in flight
//! join that read instead of queueing a second one. Errors are cached too, which keeps a failing
//! read from being retried once per field; call [`Loader::clear`] or [`Loader::clear_key`] to
//! forget them.
//!
//! The cache belongs to the loader instance. Because the loader lives in the request context
//! (see [`RequestCache`](crate::RequestCache)), it is dropped with the request, so results never
//! leak between requests. Use [`Loader::prime`] to seed a value you already have, for example an
//! object the parent resolver fetched.
//!
//! # The batch function
//!
//! Construction accepts any `Fn(Vec<K>) -> impl Future<Output = Vec<Result<V, E>>>`. The future
//! must be `Send + 'static`, because it is driven by whichever load is polled and may move
//! between worker threads. [`Loader::new_fallible`] adapts a batch function that reports a
//! whole-batch failure as `Result<Vec<Result<V, E>>, E>`; that error is cloned into every key in
//! the batch.
//!
//! The batch function must not call back into the same loader for a key that is part of its own
//! batch: that load can never be satisfied before the batch it is waiting for finishes. Loading
//! unrelated keys from inside a batch works, but batching them separately is clearer.
//!
//! Nothing in this module spawns a task or uses a timer, and there is no `tokio` dependency.

use std::{
    collections::{HashMap, HashSet, VecDeque},
    fmt,
    future::Future,
    hash::Hash,
    pin::Pin,
    sync::{Arc, Mutex, MutexGuard, PoisonError, Weak},
    task::{Context, Poll},
};

use futures::future::{BoxFuture, FutureExt, Shared, join_all};

/// The future a batch function returns.
type BatchFuture<V, E> = BoxFuture<'static, Vec<Result<V, E>>>;

/// A type-erased batch function.
type BatchFn<K, V, E> = Arc<dyn Fn(Vec<K>) -> BatchFuture<V, E> + Send + Sync>;

/// The result of one batch, shared by every key in it.
///
/// The outer [`Arc`] is what the shared batch future hands to all waiters; the inner [`Arc`]s
/// let each key be cached without copying the value again.
type BatchOutput<V, E> = Arc<Vec<Arc<Result<V, E>>>>;

/// A batch future that several loaders can poll concurrently.
type SharedBatch<V, E> = Shared<BoxFuture<'static, BatchOutput<V, E>>>;

/// Limits for a [`Loader`].
///
/// The defaults are intentionally conservative: 1,000 keys per batch is far more than an `IN`
/// query should carry, and the queue limit only exists so a resolver that issues an unbounded
/// number of loads in one tick cannot grow memory without bound.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct LoaderConfig {
    max_batch_size: usize,
    max_queue: usize,
    max_attempts: u8,
}

impl LoaderConfig {
    /// Create the default configuration.
    #[must_use]
    pub const fn new() -> Self {
        Self {
            max_batch_size: 1000,
            max_queue: 10_000,
            max_attempts: 3,
        }
    }

    /// Set the maximum number of keys passed to one call of the batch function.
    ///
    /// Zero is treated as one.
    #[must_use]
    pub const fn with_max_batch_size(mut self, size: usize) -> Self {
        self.max_batch_size = if size == 0 { 1 } else { size };
        self
    }

    /// Set the maximum number of keys queued before a flush is forced.
    ///
    /// When the queue reaches this length the loader starts batches immediately instead of
    /// waiting for the next poll. Zero is treated as one. This bounds work waiting for the batch
    /// window; results are still cached per key for the loader's lifetime, so it is not a cap on
    /// the number of distinct keys a request may load.
    #[must_use]
    pub const fn with_max_queue(mut self, size: usize) -> Self {
        self.max_queue = if size == 0 { 1 } else { size };
        self
    }

    /// Set how many times a key may be retried when a batch function omits its result.
    ///
    /// This only matters if the batch function violates its contract. Zero is treated as one.
    #[must_use]
    pub const fn with_max_attempts(mut self, attempts: u8) -> Self {
        self.max_attempts = if attempts == 0 { 1 } else { attempts };
        self
    }

    /// The maximum number of keys passed to one call of the batch function.
    #[must_use]
    pub const fn max_batch_size(&self) -> usize {
        self.max_batch_size
    }

    /// The queue length that forces an immediate flush.
    #[must_use]
    pub const fn max_queue(&self) -> usize {
        self.max_queue
    }

    /// The maximum number of attempts for a key a batch function failed to answer.
    #[must_use]
    pub const fn max_attempts(&self) -> u8 {
        self.max_attempts
    }
}

impl Default for LoaderConfig {
    fn default() -> Self {
        Self::new()
    }
}

/// What is known about a key right now.
enum Lookup<V, E> {
    /// The key has a resolved result, possibly cached from an earlier call.
    Ready(Arc<Result<V, E>>),
    /// The key is part of an in-flight batch at `index`.
    Pending {
        shared: SharedBatch<V, E>,
        index: usize,
    },
    /// The key is in neither the result cache nor a batch.
    Missing,
}

/// The cached state of a key.
enum Entry<V, E> {
    /// The key is in an in-flight batch.
    Pending { batch: u64, index: usize },
    /// The key has a result.
    Ready(Arc<Result<V, E>>),
}

struct State<K, V, E> {
    entries: HashMap<K, Entry<V, E>>,
    /// Keys waiting for a batch, in the order the loads were issued.
    queue: VecDeque<K>,
    /// Fast membership test for `queue`.
    queued: HashSet<K>,
    /// In-flight batches, keyed by id.
    batches: HashMap<u64, SharedBatch<V, E>>,
    next_batch: u64,
    /// Loads that joined a key already queued or in flight, for the batch event.
    #[cfg(feature = "tracing")]
    deduplicated: usize,
}

struct Inner<K, V, E> {
    batch_fn: BatchFn<K, V, E>,
    config: LoaderConfig,
    state: Mutex<State<K, V, E>>,
    /// A weak handle so the batch future can report completion without keeping the loader alive.
    weak: Weak<Inner<K, V, E>>,
}

/// A batched, cached loader for values of type `V` keyed by `K`.
///
/// Cloning is cheap: every clone shares the same queue, in-flight batches and result cache. Store
/// the loader in the request context and clone it into resolvers; do not construct a loader per
/// resolver unless that is the scope you want.
///
/// ```
/// use gurafi::Loader;
///
/// #[derive(Clone, Debug, PartialEq)]
/// struct User(String);
///
/// let loader = Loader::<u64, User>::new(|ids: Vec<u64>| async move {
///     ids.into_iter().map(|id| Ok(User(format!("user-{id}")))).collect()
/// });
///
/// let user = futures::executor::block_on(loader.load(7)).expect("load succeeds");
/// assert_eq!(user, User("user-7".into()));
/// ```
pub struct Loader<K, V, E = gql::GraphQLError> {
    inner: Arc<Inner<K, V, E>>,
}

impl<K, V, E> Clone for Loader<K, V, E> {
    fn clone(&self) -> Self {
        Self {
            inner: Arc::clone(&self.inner),
        }
    }
}

impl<K, V, E> fmt::Debug for Loader<K, V, E> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        let state = self
            .inner
            .state
            .lock()
            .unwrap_or_else(PoisonError::into_inner);
        f.debug_struct("Loader")
            .field("max_batch_size", &self.inner.config.max_batch_size)
            .field("queued", &state.queue.len())
            .field("in_flight", &state.batches.len())
            .finish()
    }
}

impl<K, V, E> Loader<K, V, E> {
    /// Create a loader from a batch function.
    ///
    /// The batch function receives the keys of one batch and returns one result per key, in the
    /// same order:
    ///
    /// ```
    /// use gurafi::Loader;
    ///
    /// let loader = Loader::<u32, String>::new(|keys: Vec<u32>| async move {
    ///     keys.into_iter().map(|key| Ok(key.to_string())).collect()
    /// });
    /// ```
    pub fn new<F, Fut>(batch_fn: F) -> Self
    where
        F: Fn(Vec<K>) -> Fut + Send + Sync + 'static,
        Fut: Future<Output = Vec<Result<V, E>>> + Send + 'static,
    {
        Self::with_config(batch_fn, LoaderConfig::default())
    }

    /// Create a loader with explicit limits.
    pub fn with_config<F, Fut>(batch_fn: F, config: LoaderConfig) -> Self
    where
        F: Fn(Vec<K>) -> Fut + Send + Sync + 'static,
        Fut: Future<Output = Vec<Result<V, E>>> + Send + 'static,
    {
        let batch_fn: BatchFn<K, V, E> = Arc::new(move |keys| Box::pin(batch_fn(keys)));
        let inner = Arc::new_cyclic(|weak| Inner {
            batch_fn,
            config,
            state: Mutex::new(State {
                entries: HashMap::new(),
                queue: VecDeque::new(),
                queued: HashSet::new(),
                batches: HashMap::new(),
                next_batch: 0,
                #[cfg(feature = "tracing")]
                deduplicated: 0,
            }),
            weak: weak.clone(),
        });
        Self { inner }
    }

    /// Create a loader from a batch function that can fail as a whole.
    ///
    /// A whole-batch error is cloned into every key of the batch, so each load observes it as its
    /// own error and the loader cannot strand a waiter. Prefer [`Loader::new`] when failures are
    /// per key; use this when the underlying client has one error type for a request.
    pub fn new_fallible<F, Fut>(batch_fn: F) -> Self
    where
        F: Fn(Vec<K>) -> Fut + Send + Sync + 'static,
        Fut: Future<Output = Result<Vec<Result<V, E>>, E>> + Send + 'static,
        E: Clone + Send + 'static,
    {
        Self::with_config_fallible(batch_fn, LoaderConfig::default())
    }

    /// Create a fallible loader with explicit limits.
    pub fn with_config_fallible<F, Fut>(batch_fn: F, config: LoaderConfig) -> Self
    where
        F: Fn(Vec<K>) -> Fut + Send + Sync + 'static,
        Fut: Future<Output = Result<Vec<Result<V, E>>, E>> + Send + 'static,
        E: Clone + Send + 'static,
    {
        Self::with_config(
            move |keys: Vec<K>| {
                let len = keys.len();
                batch_fn(keys).map(move |outcome| match outcome {
                    Ok(results) => results,
                    Err(error) => (0..len).map(|_| Err(error.clone())).collect(),
                })
            },
            config,
        )
    }
}

impl<K, V, E> Loader<K, V, E>
where
    K: Clone + Eq + Hash + Send + Sync + 'static,
    V: Clone + Send + Sync + 'static,
    E: Clone + Send + Sync + 'static,
{
    /// Load one key.
    ///
    /// The returned future owns a handle to the loader, so it is `'static` and can be stored in
    /// a boxed resolver future. A cached key resolves without yielding; a new key joins the next
    /// batch.
    pub fn load(&self, key: K) -> impl Future<Output = Result<V, E>> + Send + 'static {
        Arc::clone(&self.inner).load(key)
    }

    /// Load several keys, batching them explicitly.
    ///
    /// This is the right call when a single resolver knows the whole key set (for example a
    /// parent object's children), because it does not depend on the executor polling sibling
    /// resolvers in the same tick. Duplicate keys are deduplicated like any other load, and the
    /// output is in input order.
    pub fn load_many<I>(&self, keys: I) -> impl Future<Output = Vec<Result<V, E>>> + Send + 'static
    where
        I: IntoIterator<Item = K>,
    {
        let futures: Vec<_> = keys.into_iter().map(|key| self.load(key)).collect();
        async move { join_all(futures).await }
    }

    /// Insert a value as if it had been loaded.
    ///
    /// Useful when the parent resolver already has the value. A primed key is removed from the
    /// queue, so it will not be sent to the batch function. An in-flight batch for the same key is
    /// not cancelled, so loads that already joined it still resolve from it; later loads see the
    /// primed value.
    pub fn prime(&self, key: K, value: V) {
        let mut state = self.inner.lock();
        state.queue.retain(|queued| queued != &key);
        state.queued.remove(&key);
        state.entries.insert(key, Entry::Ready(Arc::new(Ok(value))));
    }

    /// Forget every completed result.
    ///
    /// Errors are forgotten as well, so the next load of a failed key retries it. A batch that is
    /// still in flight keeps its pending state: it resolves normally and its results are cached
    /// when it finishes. Call `clear` after the reads you want to refresh have completed, or call
    /// it before them and again afterwards.
    pub fn clear(&self) {
        let mut state = self.inner.lock();
        state
            .entries
            .retain(|_, entry| matches!(entry, Entry::Pending { .. }));
    }

    /// Forget the completed result for one key.
    ///
    /// Queued and in-flight loads for the key are unaffected; a batch that finishes afterwards
    /// caches its result again.
    pub fn clear_key(&self, key: &K) {
        let mut state = self.inner.lock();
        if matches!(state.entries.get(key), Some(Entry::Ready(_))) {
            state.entries.remove(key);
        }
    }

    /// Whether the key has a completed result in the cache.
    #[must_use]
    pub fn is_cached(&self, key: &K) -> bool {
        matches!(self.inner.lock().entries.get(key), Some(Entry::Ready(_)))
    }

    /// The number of keys with a completed result in the cache.
    #[must_use]
    pub fn cached_len(&self) -> usize {
        self.inner
            .lock()
            .entries
            .values()
            .filter(|entry| matches!(entry, Entry::Ready(_)))
            .count()
    }
}

impl<K, V, E> Inner<K, V, E>
where
    K: Clone + Eq + Hash + Send + Sync + 'static,
    V: Clone + Send + Sync + 'static,
    E: Clone + Send + Sync + 'static,
{
    fn lock(&self) -> MutexGuard<'_, State<K, V, E>> {
        self.state.lock().unwrap_or_else(PoisonError::into_inner)
    }

    /// Queue a key unless it is already cached or in flight.
    ///
    /// Returns the cached result when the key is already resolved. Doing the cache check and the
    /// queue insertion under one lock keeps the common load path at two lock acquisitions (queue,
    /// then flush/lookup) instead of four.
    fn enqueue(&self, key: &K) -> Option<Arc<Result<V, E>>> {
        let mut state = self.lock();
        match state.entries.get(key) {
            Some(Entry::Ready(result)) => return Some(Arc::clone(result)),
            // Queued or part of an in-flight batch: the existing entry already covers this load.
            Some(_) => {
                #[cfg(feature = "tracing")]
                {
                    state.deduplicated += 1;
                }
                return None;
            }
            None => {}
        }
        if state.queue.len() >= self.config.max_queue {
            self.flush_locked(&mut state);
            if state.entries.contains_key(key) {
                #[cfg(feature = "tracing")]
                {
                    state.deduplicated += 1;
                }
                return None;
            }
        }
        if state.queued.insert(key.clone()) {
            state.queue.push_back(key.clone());
        } else {
            #[cfg(feature = "tracing")]
            {
                state.deduplicated += 1;
            }
        }
        None
    }

    /// Flush the queue and report what is known about `key`, under one lock.
    fn flush_and_lookup(&self, key: &K) -> Lookup<V, E> {
        let mut state = self.lock();
        self.flush_locked(&mut state);
        Self::lookup_locked(&state, key)
    }

    /// Turn every queued key into a batch.
    ///
    /// Splitting respects `max_batch_size`. Each batch is started as a shared future; whichever
    /// waiter is polled drives it, which is what makes the loader spawn-free.
    fn flush_locked(&self, state: &mut State<K, V, E>) {
        while !state.queue.is_empty() {
            let queued = state.queue.len();
            #[cfg(feature = "tracing")]
            let deduplicated = std::mem::take(&mut state.deduplicated);
            #[cfg(not(feature = "tracing"))]
            let deduplicated = 0usize;
            let take = queued.min(self.config.max_batch_size);
            let keys: Vec<K> = state.queue.drain(..take).collect();
            for key in &keys {
                state.queued.remove(key);
            }

            let batch = state.next_batch;
            state.next_batch = state.next_batch.wrapping_add(1);
            for (index, key) in keys.iter().enumerate() {
                state
                    .entries
                    .insert(key.clone(), Entry::Pending { batch, index });
            }

            let shared = self.start_batch(batch, keys, queued, deduplicated);
            state.batches.insert(batch, shared);
        }
    }

    /// Build the shared future that runs one call of the batch function.
    ///
    /// The closure is only invoked once a waiter polls this future, so no user code runs while
    /// the state lock is held (Flush constructs the future under the lock).
    fn start_batch(
        &self,
        batch: u64,
        keys: Vec<K>,
        _queued: usize,
        _deduplicated: usize,
    ) -> SharedBatch<V, E> {
        let batch_fn = Arc::clone(&self.batch_fn);
        let weak = self.weak.clone();
        async move {
            #[cfg(feature = "tracing")]
            let started = std::time::Instant::now();
            let results: BatchOutput<V, E> = Arc::new(
                batch_fn(keys.clone())
                    .await
                    .into_iter()
                    .map(Arc::new)
                    .collect(),
            );
            #[cfg(feature = "tracing")]
            if tracing::enabled!(target: "gurafi::loader", tracing::Level::DEBUG) {
                let errors = results.iter().filter(|result| result.is_err()).count();
                tracing::debug!(
                    target: "gurafi::loader",
                    queue_len = _queued,
                    batch_size = keys.len(),
                    deduplicated = _deduplicated,
                    duration_us = started.elapsed().as_micros() as u64,
                    errors,
                    "gurafi.loader.batch"
                );
                if errors > 0 {
                    tracing::debug!(
                        target: "gurafi::loader",
                        batch_size = keys.len(),
                        errors,
                        "gurafi.loader.error"
                    );
                }
            }
            if let Some(inner) = weak.upgrade() {
                inner.complete(batch, keys, &results);
            }
            results
        }
        .boxed()
        .shared()
    }

    /// Record the outcome of a batch, waking every waiter through the shared future.
    fn complete(&self, batch: u64, keys: Vec<K>, results: &BatchOutput<V, E>) {
        let mut state = self.lock();
        state.batches.remove(&batch);

        for (index, key) in keys.into_iter().enumerate() {
            let owns_key = matches!(
                state.entries.get(&key),
                Some(Entry::Pending { batch: owner, .. }) if *owner == batch
            );
            if !owns_key {
                // The entry was primed or replaced while the batch was in flight.
                continue;
            }

            match results.get(index) {
                Some(result) => {
                    state.entries.insert(key, Entry::Ready(Arc::clone(result)));
                }
                None => {
                    // The batch function omitted this key. Retry it in the next batch.
                    state.entries.remove(&key);
                    if state.queued.insert(key.clone()) {
                        state.queue.push_back(key);
                    }
                }
            }
        }
    }

    fn lookup_locked(state: &State<K, V, E>, key: &K) -> Lookup<V, E> {
        match state.entries.get(key) {
            Some(Entry::Ready(result)) => Lookup::Ready(Arc::clone(result)),
            Some(Entry::Pending { batch, index }) => match state.batches.get(batch) {
                Some(shared) => Lookup::Pending {
                    shared: shared.clone(),
                    index: *index,
                },
                None => Lookup::Missing,
            },
            None => Lookup::Missing,
        }
    }

    #[cfg_attr(
        feature = "tracing",
        tracing::instrument(
            skip_all,
            level = "debug",
            name = "gurafi.loader.load",
            fields(
                key_type = std::any::type_name::<K>(),
                cache_hit = tracing::field::Empty,
            )
        )
    )]
    async fn load(self: Arc<Self>, key: K) -> Result<V, E> {
        // One lock covers both "already resolved?" and "queue it for the next batch".
        if let Some(result) = self.enqueue(&key) {
            #[cfg(feature = "tracing")]
            {
                let span = tracing::Span::current();
                if !span.is_disabled() {
                    span.record("cache_hit", true);
                }
            }
            return (*result).clone();
        }

        #[cfg(feature = "tracing")]
        {
            let span = tracing::Span::current();
            if !span.is_disabled() {
                span.record("cache_hit", false);
            }
        }


        // One yield is the batch window: every load polled before this task is polled again is
        // queued and will be flushed into the same batch.
        YieldNow::new().await;

        let mut attempts: u8 = 0;
        loop {
            match self.flush_and_lookup(&key) {
                Lookup::Ready(result) => return (*result).clone(),
                Lookup::Pending { shared, index } => {
                    let results = shared.await;
                    if let Some(result) = results.get(index) {
                        return (**result).clone();
                    }
                    attempts = attempts.saturating_add(1);
                    if attempts >= self.config.max_attempts {
                        if let Some(error) =
                            results.iter().find_map(|result| match result.as_ref() {
                                Err(error) => Some(error.clone()),
                                Ok(_) => None,
                            })
                        {
                            return Err(error.clone());
                        }
                        panic!(
                            "gurafi::Loader batch function returned only {} results; a batch \
                             function must return exactly one result per key",
                            results.len(),
                        );
                    }
                }
                Lookup::Missing => {
                    attempts = attempts.saturating_add(1);
                    if attempts >= self.config.max_attempts {
                        panic!(
                            "gurafi::Loader lost the result for a key and could not retry it; \
                             this indicates a bug in the loader"
                        );
                    }
                    self.enqueue(&key);
                }
            }
        }
    }
}

/// Yield exactly once, waking the current task so it is polled again.
///
/// This is the entire batching mechanism: no timer, no spawned task, and no executor-specific
/// API. It works under any executor because `wake_by_ref` is part of the `Waker` contract.
struct YieldNow {
    yielded: bool,
}

impl YieldNow {
    fn new() -> Self {
        Self { yielded: false }
    }
}

impl Future for YieldNow {
    type Output = ();

    fn poll(self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<()> {
        let this = self.get_mut();
        if this.yielded {
            Poll::Ready(())
        } else {
            this.yielded = true;
            cx.waker().wake_by_ref();
            Poll::Pending
        }
    }
}
