//! Type-keyed, request-scoped storage and its interaction with loaders.

use std::{
    sync::{
        Arc, Barrier,
        atomic::{AtomicUsize, Ordering},
    },
    thread,
};

use futures::{executor::block_on, future::join_all};
use gurafi::{Loader, RequestCache};

#[test]
fn builds_a_value_once_per_type() {
    let cache = RequestCache::new();

    assert!(cache.is_empty());
    assert!(cache.get::<String>().is_none());

    let first = cache.get_or_insert_with(|| String::from("first"));
    let second = cache.get_or_insert_with(|| String::from("second"));

    assert!(Arc::ptr_eq(&first, &second));
    assert_eq!(&*first, "first");
    assert!(cache.contains::<String>());
    assert_eq!(cache.len(), 1);
}

#[test]
fn types_are_stored_independently() {
    let cache = RequestCache::new();

    cache.insert(7_u32);
    cache.insert(String::from("text"));

    assert_eq!(cache.get::<u32>().as_deref(), Some(&7));
    assert_eq!(
        cache.get::<String>().as_deref().map(String::as_str),
        Some("text")
    );
    assert!(cache.get::<bool>().is_none());
    assert_eq!(cache.len(), 2);
}

#[test]
fn clones_share_the_same_store() {
    let cache = RequestCache::new();
    let clone = cache.clone();

    cache.insert(1_u32);

    assert_eq!(clone.get::<u32>().as_deref(), Some(&1));
    assert!(Arc::ptr_eq(
        &cache.get_or_insert_with(|| 2_u32),
        &clone.get_or_insert_with(|| 3_u32),
    ));
}

#[test]
fn separate_caches_are_isolated() {
    let first = RequestCache::new();
    let second = RequestCache::new();

    first.insert(String::from("first"));

    assert!(second.get::<String>().is_none());
    assert!(!second.contains::<String>());
    assert!(first.contains::<String>());
}

#[test]
fn insert_replaces_and_returns_the_previous_value() {
    let cache = RequestCache::new();

    assert!(cache.insert(1_u32).is_none());
    let previous = cache.insert(2_u32).expect("the first value comes back");

    assert_eq!(*previous, 1);
    assert_eq!(cache.get::<u32>().as_deref(), Some(&2));
}

#[test]
fn remove_and_clear_drop_entries() {
    let cache = RequestCache::new();

    cache.insert(1_u32);
    let removed = cache.remove::<u32>().expect("the value was present");
    assert_eq!(*removed, 1);
    assert!(cache.get::<u32>().is_none());

    cache.insert(2_u32);
    cache.insert(String::from("text"));
    cache.clear();
    assert!(cache.is_empty());
}

#[test]
fn concurrent_lookups_initialize_once() {
    const THREADS: usize = 8;

    let cache = RequestCache::new();
    let barrier = Arc::new(Barrier::new(THREADS));
    let initialized = Arc::new(AtomicUsize::new(0));
    let mut handles = Vec::new();

    for _ in 0..THREADS {
        let cache = cache.clone();
        let barrier = Arc::clone(&barrier);
        let initialized = Arc::clone(&initialized);
        handles.push(thread::spawn(move || {
            barrier.wait();
            cache.get_or_insert_with(|| {
                initialized.fetch_add(1, Ordering::SeqCst);
                String::from("once")
            })
        }));
    }

    let values = handles
        .into_iter()
        .map(|handle| handle.join().expect("the thread did not panic"))
        .collect::<Vec<_>>();

    assert_eq!(initialized.load(Ordering::SeqCst), 1);
    assert!(
        values
            .windows(2)
            .all(|pair| Arc::ptr_eq(&pair[0], &pair[1]))
    );
    assert_eq!(
        cache.get::<String>().as_deref().map(String::as_str),
        Some("once")
    );
}

#[test]
fn loaders_in_separate_request_caches_do_not_share_results() {
    let batches = Arc::new(AtomicUsize::new(0));
    let make_loader = || {
        let batches = Arc::clone(&batches);
        Loader::<u32, u32>::new(move |keys: Vec<u32>| {
            batches.fetch_add(1, Ordering::SeqCst);
            async move {
                keys.into_iter()
                    .map(Ok)
                    .collect::<Vec<Result<u32, gurafi::GraphQLError>>>()
            }
        })
    };

    // Two requests, each with its own cache and therefore its own loader.
    let first_request = RequestCache::new();
    let second_request = RequestCache::new();

    let first = first_request.get_or_insert_with(make_loader);
    let first_again = first_request.get_or_insert_with(make_loader);
    let second = second_request.get_or_insert_with(make_loader);

    assert!(Arc::ptr_eq(&first, &first_again));
    assert!(!Arc::ptr_eq(&first, &second));

    block_on(join_all(vec![
        first.load(1),
        first_again.load(1),
        second.load(1),
    ]));

    // One batch for the first request (the duplicate key is deduplicated) and one for the second.
    assert_eq!(batches.load(Ordering::SeqCst), 2);
    assert!(first.is_cached(&1));
    assert!(second.is_cached(&1));
}

#[test]
fn request_cache_is_send_and_sync() {
    fn assert_send_sync<T: Send + Sync>() {}
    assert_send_sync::<RequestCache>();
}
