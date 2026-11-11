//! Success-path cost of resolver panic isolation.
//!
//! The isolation wrapper is only supposed to cost the resolver boundary, never an allocation,
//! so this probe runs the same workloads with and without resolver-backed fields and prints
//! timings and allocation counts as `LABEL key=value`. Run it on the current tree and on the
//! commit before the wrapper to compare:
//!
//! ```sh
//! cargo test -q -p gql --release --test perf_panic -- --test-threads=1 --nocapture
//! cargo test -q -p gql --release --test perf_panic -- --test-threads=1 --ignored --nocapture
//! ```
//!
//! The counting allocator is global to this test binary, so the measuring tests hold
//! [`MEASUREMENT`]. The allocation tests run in the normal suite; the timing tests are ignored
//! because they are only meaningful alone on an idle machine.

use std::{
    alloc::{GlobalAlloc, Layout, System},
    hint::black_box,
    sync::{
        Arc, Mutex, MutexGuard,
        atomic::{AtomicUsize, Ordering},
    },
    time::Instant,
};

use futures::StreamExt;
use gql::{
    ExecutionOptions, Field, Object, ObjectType, PreparedQuery, Request, Schema, TypeRef, Value,
    execute, execute_prepared,
};

/// Serializes the measuring tests: the counters below are process-wide.
static MEASUREMENT: Mutex<()> = Mutex::new(());

fn measuring() -> MutexGuard<'static, ()> {
    MEASUREMENT
        .lock()
        .unwrap_or_else(|error| error.into_inner())
}

struct CountingAllocator;

static ALLOCATIONS: AtomicUsize = AtomicUsize::new(0);

unsafe impl GlobalAlloc for CountingAllocator {
    unsafe fn alloc(&self, layout: Layout) -> *mut u8 {
        ALLOCATIONS.fetch_add(1, Ordering::Relaxed);
        unsafe { System.alloc(layout) }
    }

    unsafe fn alloc_zeroed(&self, layout: Layout) -> *mut u8 {
        ALLOCATIONS.fetch_add(1, Ordering::Relaxed);
        unsafe { System.alloc_zeroed(layout) }
    }

    unsafe fn dealloc(&self, ptr: *mut u8, layout: Layout) {
        unsafe { System.dealloc(ptr, layout) }
    }

    unsafe fn realloc(&self, ptr: *mut u8, layout: Layout, new_size: usize) -> *mut u8 {
        ALLOCATIONS.fetch_add(1, Ordering::Relaxed);
        unsafe { System.realloc(ptr, layout, new_size) }
    }
}

#[global_allocator]
static ALLOCATOR: CountingAllocator = CountingAllocator;

/// Time and allocation count of one call, with the caches already warm.
fn measure<F: FnMut()>(iterations: usize, mut operation: F) -> (u128, usize) {
    for _ in 0..K {
        operation();
    }
    let mut best = u128::MAX;
    let mut allocations = 0;
    for _ in 0..3 {
        let started = Instant::now();
        let before = ALLOCATIONS.load(Ordering::Relaxed);
        for _ in 0..iterations {
            operation();
        }
        allocations = ALLOCATIONS.load(Ordering::Relaxed) - before;
        best = best.min(started.elapsed().as_nanos() / iterations as u128);
    }
    (best, allocations / iterations)
}

/// Warmup calls before a measurement.
const K: usize = 10;

fn item_schema(items: usize) -> Schema {
    let item = ObjectType::new("Item")
        .field(Field::new("id", TypeRef::id().non_null()))
        .field(Field::new("name", TypeRef::string().non_null()))
        .field(Field::new("age", TypeRef::int().non_null()))
        .field(Field::new("echo", TypeRef::string()).resolve(|ctx| {
            let len = ctx.info.field_nodes.len();
            Box::pin(async move { Ok(Value::from(format!("x{len}"))) })
        }));

    let values: Arc<Vec<Arc<Object>>> = Arc::new(
        (0..items)
            .map(|index| {
                Arc::new(Object::from_fields([
                    ("id", Value::from(format!("item-{index}"))),
                    ("name", Value::from("name")),
                    ("age", Value::Int(42)),
                ]))
            })
            .collect(),
    );

    let root = ObjectType::new("Query").field(
        Field::new("items", TypeRef::named("Item").non_null().list().non_null()).resolve({
            let values = Arc::clone(&values);
            move |_| {
                let values = Arc::clone(&values);
                Box::pin(async move {
                    Ok(Value::List(
                        values
                            .iter()
                            .map(|item| Value::Object(Arc::clone(item)))
                            .collect(),
                    ))
                })
            }
        }),
    );
    let mut builder = Schema::builder();
    builder.add_type(item.into());
    builder.add_type(root.into());
    builder.query("Query").build().expect("probe schema")
}

/// The allocations of one prepared execution of `source` over `items` objects.
fn prepared_allocations(items: usize, source: &str) -> usize {
    let schema = item_schema(items);
    let prepared = PreparedQuery::new(&schema, "probe", source, None).expect("prepared");
    for _ in 0..K {
        let _ = futures::executor::block_on(execute_prepared(
            &prepared,
            None,
            &ExecutionOptions::default(),
        ));
    }
    let before = ALLOCATIONS.load(Ordering::Relaxed);
    let response = futures::executor::block_on(execute_prepared(
        &prepared,
        None,
        &ExecutionOptions::default(),
    ));
    let allocations = ALLOCATIONS.load(Ordering::Relaxed) - before;
    assert!(response.errors.is_empty(), "{response:?}");
    allocations
}

const PLAIN: &str = "query Q { items { id name age } }";
const RESOLVER: &str = "query Q { items { id name age echo } }";

#[test]
fn resolver_cost_per_object() {
    let _measuring = measuring();
    let plain = prepared_allocations(1_000, PLAIN);
    let resolver = prepared_allocations(1_000, RESOLVER);
    eprintln!("resolver_cost plain={plain} resolver={resolver}");
    // Selecting one resolver-backed field on every object adds one response string per object;
    // the panic wrapper itself must add nothing.
    assert!(resolver > plain);
}

#[test]
#[ignore = "timing probe; run alone with --ignored"]
fn execution_forms() {
    let _measuring = measuring();
    let schema = item_schema(1_000);
    let options = ExecutionOptions::default();
    let plain = PreparedQuery::new(&schema, "plain", PLAIN, None).expect("prepared");
    let resolver = PreparedQuery::new(&schema, "resolver", RESOLVER, None).expect("prepared");

    let request_plain = Request::new(PLAIN);
    let request_resolver = Request::new(RESOLVER);

    let (plain_ns, plain_allocs) = measure(200, || {
        let response = futures::executor::block_on(execute(&schema, request_plain, &options));
        black_box(response.data);
    });
    let (resolver_ns, resolver_allocs) = measure(200, || {
        let response = futures::executor::block_on(execute(&schema, request_resolver, &options));
        black_box(response.data);
    });
    let (prepared_plain_ns, prepared_plain_allocs) = measure(200, || {
        let response = futures::executor::block_on(execute_prepared(&plain, None, &options));
        black_box(response.data);
    });
    let (prepared_resolver_ns, prepared_resolver_allocs) = measure(200, || {
        let response = futures::executor::block_on(execute_prepared(&resolver, None, &options));
        black_box(response.data);
    });

    eprintln!(
        "execution_forms plain_ns={plain_ns} plain_allocs={plain_allocs} resolver_ns={resolver_ns} resolver_allocs={resolver_allocs} prepared_plain_ns={prepared_plain_ns} prepared_plain_allocs={prepared_plain_allocs} prepared_resolver_ns={prepared_resolver_ns} prepared_resolver_allocs={prepared_resolver_allocs}"
    );
}

/// A prepared subscription whose every event selects an async resolver.
fn subscription_run(events: usize) -> (u128, usize) {
    let mut builder = Schema::builder();
    builder.add_type(
        ObjectType::new("User")
            .field(Field::new("id", TypeRef::id().non_null()))
            .field(Field::new("name", TypeRef::string().non_null()))
            .field(Field::new("echo", TypeRef::string()).resolve(|ctx| {
                let len = ctx.info.field_nodes.len();
                Box::pin(async move { Ok(Value::from(format!("x{len}"))) })
            }))
            .into(),
    );
    let query = ObjectType::new("Query").field(Field::new("health", TypeRef::string().non_null()));
    let subscription = ObjectType::new("Subscription").field(
        Field::new("tick", TypeRef::named("User"))
            .subscribe(move |_| {
                Box::pin(async move {
                    let stream = futures::stream::repeat_with(|| {
                        Ok(Value::Object(Arc::new(Object::from_fields([
                            ("id", Value::from("1")),
                            ("name", Value::from("name")),
                        ]))))
                    });
                    Ok(stream.take(events).boxed())
                })
            })
            .resolve(|context| {
                let parent = context.parent.clone();
                Box::pin(async move { Ok(parent) })
            }),
    );
    builder.add_type(query.into());
    builder.add_type(subscription.into());
    let schema = builder
        .query("Query")
        .subscription("Subscription")
        .build()
        .expect("schema");

    let source = "subscription S { tick { id name echo } }";
    let prepared = PreparedQuery::new(&schema, "sub", source, None).expect("prepared");
    let _ = futures::executor::block_on(prepared.subscribe(None, &ExecutionOptions::default()));
    let (ns, allocs) = measure(5, || {
        let response =
            futures::executor::block_on(prepared.subscribe(None, &ExecutionOptions::default()))
                .expect("subscribed");
        let count = futures::executor::block_on(response.stream.collect::<Vec<_>>()).len();
        black_box(count);
    });
    (ns, allocs)
}

#[test]
#[ignore = "timing probe; run alone with --ignored"]
fn subscription_events() {
    let _measuring = measuring();
    let (ns, allocs) = subscription_run(1_000);
    eprintln!("subscription_events events=1000 ns={ns} allocs_per_run={allocs}");
}

/// Micro-cost of the per-poll `catch_unwind` boundary, with no executor in between.
#[test]
#[ignore = "timing probe; run alone with --ignored"]
fn wrapper_micro() {
    use core::{
        future::Future,
        task::{Context, Poll, Waker},
    };
    use std::panic::{AssertUnwindSafe, catch_unwind};

    let mut future: std::pin::Pin<Box<dyn Future<Output = u64>>> =
        Box::pin(futures::future::poll_fn(|_| Poll::Ready(7u64)));
    let waker = Waker::noop();
    let mut context = Context::from_waker(waker);

    const N: u32 = 50_000_000;
    let mut best_plain = u128::MAX;
    let mut best_caught = u128::MAX;
    for _ in 0..3 {
        let started = Instant::now();
        for _ in 0..N {
            match future.as_mut().poll(&mut context) {
                Poll::Ready(value) => black_box(value),
                Poll::Pending => unreachable!(),
            };
        }
        best_plain = best_plain.min(started.elapsed().as_nanos());

        let started = Instant::now();
        for _ in 0..N {
            match catch_unwind(AssertUnwindSafe(|| future.as_mut().poll(&mut context))) {
                Ok(Poll::Ready(value)) => black_box(value),
                Ok(Poll::Pending) => unreachable!(),
                Err(_) => unreachable!(),
            };
        }
        best_caught = best_caught.min(started.elapsed().as_nanos());
    }
    eprintln!(
        "wrapper_micro plain_ps={} caught_ps={} delta_ps={}",
        best_plain * 1000 / u128::from(N),
        best_caught * 1000 / u128::from(N),
        (best_caught - best_plain) * 1000 / u128::from(N),
    );
}
