//! Per-request shared state.
//!
//! GraphQL resolves a request as a tree, and a single request often needs the same expensive
//! value at several places in that tree. A [`RequestCache`] is a small type-keyed store that
//! lives for exactly one request: the server creates one, puts it in the execution context, and
//! drops it when the response is finished.
//!
//! The usual pattern is to hang a [`Loader`](crate::Loader) off the cache so every resolver in
//! the request shares one batch queue and one result cache:
//!
//! ```
//! use gurafi::{ID, Loader, RequestCache};
//!
//! struct User {
//!     id: ID,
//! }
//!
//! struct Ctx {
//!     users: Loader<ID, User>,
//!     cache: RequestCache,
//! }
//!
//! impl Ctx {
//!     fn new() -> Self {
//!         let cache = RequestCache::new();
//!         let users = cache.get_or_insert_with(|| {
//!             Loader::<ID, User>::new(|ids| async move {
//!                 ids.into_iter()
//!                     .map(|id| Ok(User { id }))
//!                     .collect::<Vec<_>>()
//!             })
//!         });
//!         Self {
//!             users: (*users).clone(),
//!             cache,
//!         }
//!     }
//! }
//! ```
//!
//! Because the cache is built per request, values inserted while resolving one request can never
//! be observed by another. It is deliberately **not** a global cache: a process-wide cache needs
//! invalidation and eviction policy, which is a different problem from sharing work between the
//! fields of one response.
//!
//! [`Loader`](crate::Loader) keeps its own per-instance result cache. When a loader is stored in
//! a request cache (as above), the loader's cache and the request cache are scoped together: one
//! request, one loader, one set of batched reads.

use std::{
    any::{Any, TypeId},
    collections::HashMap,
    fmt,
    sync::{Arc, PoisonError, RwLock},
};

/// A type-erased entry stored under its [`TypeId`].
type Stored = Arc<dyn Any + Send + Sync>;

/// A type-keyed store scoped to one GraphQL request.
///
/// At most one value of each Rust type is stored. Insertion returns an [`Arc`], so cloning a
/// value out of the cache (or handing it to several resolvers) does not clone the value itself.
///
/// `RequestCache` is cheap to clone: every clone refers to the same store, which means passing
/// one into the execution context and one into a resolver is the same cache. A *new* request must
/// call [`RequestCache::new`] to get an isolated store.
///
/// Reads take a shared lock and writes take an exclusive one, so resolvers running on several
/// worker threads can share the cache without copying it. Contention is normally limited to the
/// first access for each type, which is where the value is built.
///
/// ```
/// use gurafi::RequestCache;
///
/// let cache = RequestCache::new();
///
/// let first = cache.get_or_insert_with(|| String::from("shared"));
/// let second = cache.get_or_insert_with(|| String::from("unused"));
///
/// assert!(std::sync::Arc::ptr_eq(&first, &second));
/// assert_eq!(&*first, "shared");
/// ```
#[derive(Clone, Default)]
pub struct RequestCache {
    entries: Arc<RwLock<HashMap<TypeId, Stored>>>,
}

impl RequestCache {
    /// Create an empty request cache.
    ///
    /// Create exactly one of these per GraphQL request. Sharing a cache between requests leaks
    /// values across them; that is sometimes desirable for immutable metadata, but it is not
    /// what this type promises.
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }

    /// Borrow a stored value, if one of type `T` has been inserted.
    ///
    /// The returned [`Arc`] keeps the value alive even if the entry is simultaneously removed from
    /// the cache.
    #[must_use]
    pub fn get<T>(&self) -> Option<Arc<T>>
    where
        T: Any + Send + Sync,
    {
        let entry = {
            let entries = self.entries.read().unwrap_or_else(PoisonError::into_inner);
            entries.get(&TypeId::of::<T>()).map(Arc::clone)
        };
        #[cfg(feature = "tracing")]
        if tracing::enabled!(target: "gurafi::cache", tracing::Level::TRACE) {
            tracing::trace!(
                target: "gurafi::cache",
                hit = entry.is_some(),
                "gurafi.cache.lookup"
            );
        }
        entry?.downcast::<T>().ok()
    }

    /// Return the stored value of type `T`, building it with `init` if it is absent.
    ///
    /// When several threads race for the same missing type exactly one calls `init`; every
    /// caller receives the same [`Arc`]. The common case for a loader is therefore one
    /// construction per request, not one per resolver.
    ///
    /// `init` runs while the cache is write-locked. It must not call back into this cache (that
    /// would deadlock) and should not block; construct the value and return it.
    pub fn get_or_insert_with<T>(&self, init: impl FnOnce() -> T) -> Arc<T>
    where
        T: Any + Send + Sync,
    {
        if let Some(existing) = self.get::<T>() {
            return existing;
        }

        let mut entries = self.entries.write().unwrap_or_else(PoisonError::into_inner);
        if let Some(existing) = entries.get(&TypeId::of::<T>()) {
            if let Ok(typed) = Arc::clone(existing).downcast::<T>() {
                return typed;
            }
        }

        let value = Arc::new(init());
        entries.insert(TypeId::of::<T>(), Arc::clone(&value) as Stored);
        #[cfg(feature = "tracing")]
        if tracing::enabled!(target: "gurafi::cache", tracing::Level::TRACE) {
            tracing::trace!(
                target: "gurafi::cache",
                hit = false,
                inserted = true,
                "gurafi.cache.lookup"
            );
        }
        value
    }

    /// Store `value`, returning the value it replaced.
    ///
    /// Handles already given out for the previous value stay valid; only future lookups observe
    /// the replacement.
    pub fn insert<T>(&self, value: T) -> Option<Arc<T>>
    where
        T: Any + Send + Sync,
    {
        let previous = {
            let mut entries = self.entries.write().unwrap_or_else(PoisonError::into_inner);
            entries.insert(TypeId::of::<T>(), Arc::new(value) as Stored)
        };
        previous.and_then(|entry| entry.downcast::<T>().ok())
    }

    /// Remove and return the stored value of type `T`.
    pub fn remove<T>(&self) -> Option<Arc<T>>
    where
        T: Any + Send + Sync,
    {
        let entry = {
            let mut entries = self.entries.write().unwrap_or_else(PoisonError::into_inner);
            entries.remove(&TypeId::of::<T>())
        };
        entry?.downcast::<T>().ok()
    }

    /// Whether a value of type `T` is currently stored.
    #[must_use]
    pub fn contains<T>(&self) -> bool
    where
        T: Any + Send + Sync,
    {
        self.entries
            .read()
            .unwrap_or_else(PoisonError::into_inner)
            .contains_key(&TypeId::of::<T>())
    }

    /// Remove every stored value.
    ///
    /// Existing [`Arc`] handles stay valid; only future lookups see the cache as empty.
    pub fn clear(&self) {
        self.entries
            .write()
            .unwrap_or_else(PoisonError::into_inner)
            .clear();
    }

    /// The number of distinct types currently stored.
    #[must_use]
    pub fn len(&self) -> usize {
        self.entries
            .read()
            .unwrap_or_else(PoisonError::into_inner)
            .len()
    }

    /// Whether the cache holds no values.
    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.len() == 0
    }
}

impl fmt::Debug for RequestCache {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("RequestCache")
            .field("entries", &self.len())
            .finish()
    }
}
