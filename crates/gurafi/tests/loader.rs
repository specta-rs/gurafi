//! Batching, deduplication, caching and error behaviour of [`gurafi::Loader`].

use std::sync::{
    Arc, Mutex,
    atomic::{AtomicUsize, Ordering},
};
use std::task::{Context, Poll, Waker};

use futures::{
    StreamExt,
    channel::{mpsc, oneshot},
    executor::block_on,
    future::{BoxFuture, join_all},
};
use gurafi::{GraphQLError, Loader, LoaderConfig};

type TestResult<T> = Result<T, GraphQLError>;
type BatchRequest = (Vec<u32>, oneshot::Sender<Vec<TestResult<u32>>>);

/// Records the keys of every batch function call.
#[derive(Clone, Default)]
struct Log {
    batches: Arc<Mutex<Vec<Vec<u32>>>>,
}

impl Log {
    fn record(&self, keys: Vec<u32>) {
        self.batches.lock().unwrap().push(keys);
    }

    fn batches(&self) -> Vec<Vec<u32>> {
        self.batches.lock().unwrap().clone()
    }

    fn calls(&self) -> usize {
        self.batches.lock().unwrap().len()
    }
}

/// A loader that records calls and doubles its keys.
fn doubling_loader(log: &Log) -> Loader<u32, u32> {
    let log = log.clone();
    Loader::<u32, u32>::new(move |keys: Vec<u32>| {
        log.record(keys.clone());
        async move {
            keys.into_iter()
                .map(|key| Ok(key * 2))
                .collect::<Vec<TestResult<u32>>>()
        }
    })
}

#[test]
fn deduplicates_keys_in_one_tick() {
    let log = Log::default();
    let loader = doubling_loader(&log);

    let values = block_on(join_all(vec![
        loader.load(1),
        loader.load(1),
        loader.load(1),
    ]));

    assert_eq!(values, vec![Ok(2), Ok(2), Ok(2)]);
    assert_eq!(log.batches(), vec![vec![1]]);
}

#[test]
fn batches_distinct_keys_in_one_tick() {
    let log = Log::default();
    let loader = doubling_loader(&log);

    let values = block_on(join_all(vec![
        loader.load(1),
        loader.load(2),
        loader.load(3),
    ]));

    assert_eq!(values, vec![Ok(2), Ok(4), Ok(6)]);
    assert_eq!(log.batches(), vec![vec![1, 2, 3]]);
}

#[test]
fn caches_results_across_calls() {
    let log = Log::default();
    let loader = doubling_loader(&log);

    let first = block_on(loader.load(1));
    let second = block_on(loader.load(1));

    assert_eq!(first, Ok(2));
    assert_eq!(second, Ok(2));
    assert_eq!(log.calls(), 1);
    assert!(loader.is_cached(&1));
    assert_eq!(loader.cached_len(), 1);
}

#[test]
fn separate_ticks_do_not_batch() {
    let log = Log::default();
    let loader = doubling_loader(&log);

    block_on(async {
        assert_eq!(loader.load(1).await, Ok(2));
        assert_eq!(loader.load(2).await, Ok(4));
    });

    assert_eq!(log.batches(), vec![vec![1], vec![2]]);
}

#[test]
fn propagates_errors_per_key() {
    let log = Log::default();
    let recorded = log.clone();
    let loader = Loader::<u32, u32>::new(move |keys: Vec<u32>| {
        recorded.record(keys.clone());
        async move {
            keys.into_iter()
                .map(|key| {
                    if key == 13 {
                        Err(GraphQLError::new("unlucky"))
                    } else {
                        Ok(key)
                    }
                })
                .collect::<Vec<TestResult<u32>>>()
        }
    });

    let values = block_on(join_all(vec![
        loader.load(1),
        loader.load(13),
        loader.load(2),
    ]));

    assert_eq!(values[0], Ok(1));
    assert_eq!(values[1].as_ref().unwrap_err().message(), "unlucky");
    assert_eq!(values[2], Ok(2));
    assert_eq!(log.batches(), vec![vec![1, 13, 2]]);

    // Errors are cached like successes.
    assert!(loader.is_cached(&13));
    assert!(block_on(loader.load(13)).is_err());
    assert_eq!(log.calls(), 1);
}

#[test]
fn whole_batch_error_fans_out_without_deadlock() {
    let calls = Arc::new(AtomicUsize::new(0));
    let counted = Arc::clone(&calls);
    let loader = Loader::<u32, u32>::new_fallible(move |keys: Vec<u32>| {
        counted.fetch_add(1, Ordering::SeqCst);
        let count = keys.len();
        async move {
            assert!(count > 0);
            Err::<Vec<TestResult<u32>>, GraphQLError>(GraphQLError::new("database down"))
        }
    });

    let values = block_on(join_all(vec![loader.load(1), loader.load(2)]));

    assert_eq!(values.len(), 2);
    for value in values {
        assert_eq!(value.unwrap_err().message(), "database down");
    }
    assert_eq!(calls.load(Ordering::SeqCst), 1);
}

#[test]
fn empty_batch_never_calls_the_batch_function() {
    let log = Log::default();
    let loader = doubling_loader(&log);

    let values = block_on(loader.load_many(Vec::new()));

    assert!(values.is_empty());
    assert_eq!(log.calls(), 0);
}

#[test]
fn load_many_deduplicates_and_preserves_order() {
    let log = Log::default();
    let loader = doubling_loader(&log);

    let values = block_on(loader.load_many(vec![3, 1, 3, 2]));

    assert_eq!(values, vec![Ok(6), Ok(2), Ok(6), Ok(4)]);
    assert_eq!(log.batches(), vec![vec![3, 1, 2]]);
}

#[test]
fn splits_batches_at_the_configured_size() {
    let log = Log::default();
    let recorded = log.clone();
    let loader = Loader::<u32, u32>::with_config(
        move |keys: Vec<u32>| {
            recorded.record(keys.clone());
            async move { keys.into_iter().map(Ok).collect::<Vec<TestResult<u32>>>() }
        },
        LoaderConfig::new().with_max_batch_size(2),
    );

    let futures = (0..5).map(|key| loader.load(key)).collect::<Vec<_>>();
    let values = block_on(join_all(futures));

    assert_eq!(values, vec![Ok(0), Ok(1), Ok(2), Ok(3), Ok(4)]);
    let batches = log.batches();
    assert_eq!(batches.len(), 3);
    assert_eq!(
        batches.iter().map(Vec::len).collect::<Vec<_>>(),
        vec![2, 2, 1]
    );
}

#[test]
fn queue_limit_forces_early_flushes() {
    let log = Log::default();
    let recorded = log.clone();
    let loader = Loader::<u32, u32>::with_config(
        move |keys: Vec<u32>| {
            recorded.record(keys.clone());
            async move { keys.into_iter().map(Ok).collect::<Vec<TestResult<u32>>>() }
        },
        LoaderConfig::new()
            .with_max_batch_size(100)
            .with_max_queue(2),
    );

    let futures = (0..4).map(|key| loader.load(key)).collect::<Vec<_>>();
    block_on(join_all(futures));

    assert_eq!(log.batches(), vec![vec![0, 1], vec![2, 3]]);
}

#[test]
fn prime_and_clear_control_the_cache() {
    let log = Log::default();
    let loader = doubling_loader(&log);

    loader.prime(1, 99);
    assert!(loader.is_cached(&1));
    assert_eq!(block_on(loader.load(1)), Ok(99));
    assert_eq!(log.calls(), 0);

    loader.clear();
    assert!(!loader.is_cached(&1));
    assert_eq!(block_on(loader.load(1)), Ok(2));
    assert_eq!(log.calls(), 1);

    loader.clear_key(&1);
    assert!(!loader.is_cached(&1));
}

#[test]
fn clones_share_batches_and_cache() {
    let log = Log::default();
    let loader = doubling_loader(&log);
    let clone = loader.clone();

    let values = block_on(join_all(vec![loader.load(1), clone.load(1), clone.load(2)]));

    assert_eq!(values, vec![Ok(2), Ok(2), Ok(4)]);
    assert_eq!(log.batches(), vec![vec![1, 2]]);
}

#[test]
fn concurrent_loads_are_driven_without_spawning() {
    let requests = Arc::new(AtomicUsize::new(0));
    let counted = Arc::clone(&requests);
    let (sender, mut receiver) = mpsc::unbounded::<BatchRequest>();

    let loader = Loader::<u32, u32>::new(move |keys: Vec<u32>| {
        counted.fetch_add(1, Ordering::SeqCst);
        let sender = sender.clone();
        async move {
            let (respond, received) = oneshot::channel();
            sender
                .unbounded_send((keys, respond))
                .expect("the test server is still alive");
            received.await.expect("the test server responded")
        }
    });

    // The "database" runs as another future in the same executor. There is no spawned task and no
    // background thread: the loader's waiters drive the batch, and the batch's future drives the
    // request to this server.
    let server: BoxFuture<'static, ()> = Box::pin(async move {
        let (keys, respond) = receiver.next().await.expect("one batch request");
        let values = keys.into_iter().map(|key| Ok(key + 1)).collect();
        respond.send(values).ok();
    });

    let mut futures: Vec<BoxFuture<'static, ()>> = vec![server];
    for key in 0..4 {
        let loader = loader.clone();
        futures.push(Box::pin(async move {
            assert_eq!(loader.load(key).await, Ok(key + 1));
        }));
    }
    block_on(join_all(futures));

    assert_eq!(requests.load(Ordering::SeqCst), 1);
}

#[test]
fn short_batch_is_retried() {
    let calls = Arc::new(AtomicUsize::new(0));
    let counted = Arc::clone(&calls);
    let loader = Loader::<u32, u32>::new(move |keys: Vec<u32>| {
        let call = counted.fetch_add(1, Ordering::SeqCst);
        async move {
            if call == 0 {
                // Deliberately omit the last key on the first call.
                keys.into_iter()
                    .take(1)
                    .map(Ok)
                    .collect::<Vec<TestResult<u32>>>()
            } else {
                keys.into_iter().map(Ok).collect::<Vec<TestResult<u32>>>()
            }
        }
    });

    let values = block_on(join_all(vec![loader.load(1), loader.load(2)]));

    assert_eq!(values, vec![Ok(1), Ok(2)]);
    assert_eq!(calls.load(Ordering::SeqCst), 2);
}

#[test]
fn runs_on_a_minimal_manual_executor() {
    // This executor only knows how to poll: it has no reactor, no timer and no task queue, and
    // its waker does nothing. That is enough, because a loader schedules work purely through
    // `Waker::wake` and never depends on a runtime.
    let mut context = Context::from_waker(Waker::noop());

    let log = Log::default();
    let loader = doubling_loader(&log);
    let mut future = Box::pin(async move { join_all(vec![loader.load(4), loader.load(5)]).await });

    let values = loop {
        match future.as_mut().poll(&mut context) {
            Poll::Ready(values) => break values,
            Poll::Pending => std::thread::yield_now(),
        }
    };

    assert_eq!(values, vec![Ok(8), Ok(10)]);
    assert_eq!(log.batches(), vec![vec![4, 5]]);
}

#[test]
fn loader_is_send_and_sync() {
    fn assert_send_sync<T: Send + Sync>() {}
    assert_send_sync::<Loader<u32, u32>>();
    assert_send_sync::<Loader<String, String, String>>();
}
