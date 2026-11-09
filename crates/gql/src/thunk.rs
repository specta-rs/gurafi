use core::fmt;

/// Thunks are used to delay a calculation until its result is needed
///
/// <https://en.wikipedia.org/wiki/Thunk>
pub enum Thunk<T> {
    Lazy(Box<dyn Fn() -> T + Send + Sync>),
    Eager(T),
}

impl<T: fmt::Debug> fmt::Debug for Thunk<T> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Thunk::Lazy(func) => f.debug_tuple("Thunk::Lazy").field(&(func)()).finish(),
            Thunk::Eager(v) => f.debug_tuple("Thunk::Eager").field(v).finish(),
        }
    }
}

impl<T> Thunk<T> {
    pub fn get(self) -> T {
        match self {
            Thunk::Lazy(func) => (func)(),
            Thunk::Eager(v) => v,
        }
    }
}

// Rust won't allow a blanket impl due to it being potentially conflicting so we do it for some common container types.

impl<V> From<Vec<V>> for Thunk<Vec<V>> {
    fn from(v: Vec<V>) -> Self {
        Thunk::Eager(v)
    }
}

use std::collections::HashMap;
#[allow(clippy::implicit_hasher)] // We can't be generic over the hasher without the whole data structure being generic.
impl<K, V> From<HashMap<K, V>> for Thunk<HashMap<K, V>> {
    fn from(v: HashMap<K, V>) -> Self {
        Thunk::Eager(v)
    }
}
