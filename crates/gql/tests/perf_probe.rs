//! Allocation and timing probe for the execution optimization passes.
//!
//! The allocation tests are cheap and assert that every probe query still executes without
//! errors, so they run with the normal suite. The timing tests are `#[ignore]`d because they are
//! only meaningful run alone on an idle machine:
//!
//! ```sh
//! cargo test -q -p gql --release --test perf_probe -- --test-threads=1 --nocapture
//! cargo test -q -p gql --release --test perf_probe -- --test-threads=1 --ignored --nocapture
//! ```
//!
//! Every number is printed as `LABEL key=value` so a before/after run can be diffed. The counting
//! allocator is global to this test binary, so every measuring test holds [`MEASUREMENT`]: the
//! counts stay correct however many threads the harness uses.

use std::{
    alloc::{GlobalAlloc, Layout, System},
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

/// Hold while a test measures allocations.
fn measuring() -> MutexGuard<'static, ()> {
    // A failed assertion in another test must not turn every later measurement into a panic.
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
    for _ in 0..10 {
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

/// `items: [Item!]!` where every item carries scalar properties, one nested child and a
/// resolver-backed field the probes may or may not select.
///
/// The resolver is the point: it makes `Item` a type that defines a resolver, so a type-wide
/// "has a resolver" check would send every object down the asynchronous path even when the query
/// never selects it.
fn item_schema(items: usize) -> Schema {
    let item = ObjectType::new("Item")
        .field(Field::new("id", TypeRef::id().non_null()))
        .field(Field::new("name", TypeRef::string().non_null()))
        .field(Field::new("age", TypeRef::int().non_null()))
        .field(Field::new("child", TypeRef::named("Item")))
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
                    (
                        "child",
                        Value::Object(Arc::new(Object::from_fields([
                            ("id", Value::from("child")),
                            ("name", Value::from("child")),
                            ("age", Value::Int(1)),
                        ]))),
                    ),
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
fn prepared_allocations_with(items: usize, source: &str) -> usize {
    let schema = item_schema(items);
    let prepared = PreparedQuery::new(&schema, "probe", source, None).expect("prepared");
    for _ in 0..5 {
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

fn prepared_allocations(source: &str) -> usize {
    prepared_allocations_with(1_000, source)
}

#[test]
fn per_object_scaling() {
    let _measuring = measuring();
    // Fixed cost versus per-object cost, which separates the response itself from the
    // per-object bookkeeping.
    for items in [0, 1, 2, 10] {
        let leaves = prepared_allocations_with(items, "query Q { items { id name age } }");
        let child =
            prepared_allocations_with(items, "query Q { items { id name age child { id } } }");
        eprintln!("scaling items={items} leaves={leaves} child={child}");
    }
}

#[test]
fn object_shapes() {
    let _measuring = measuring();
    // `Item` defines a resolver (`echo`) that none of these queries select, so the selected
    // properties still complete synchronously.
    let leaves = prepared_allocations("query Q { items { id name age } }");
    let one_child = prepared_allocations("query Q { items { child { id } } }");
    let mixed = prepared_allocations("query Q { items { id name age child { id } } }");
    let merged = prepared_allocations("query Q { items { id child { id } child { id } } }");
    eprintln!("object_shapes leaves={leaves} one_child={one_child} mixed={mixed} merged={merged}");
    // `Item` defines an unselected resolver, so these all complete synchronously: one response
    // object per item, plus its nested child.
    assert!(leaves < one_child && one_child == mixed && mixed == merged);
}

#[test]
fn merged_nodes_per_object() {
    let _measuring = measuring();
    // A group of three merged nodes must not allocate the merged-selection or merged-node
    // buffers once per object.
    let one = prepared_allocations("query Q { items { child { id } } }");
    let two = prepared_allocations("query Q { items { child { id } child { id } } }");
    let three =
        prepared_allocations("query Q { items { child { id } child { id } child { id } } }");
    eprintln!("merged_nodes one={one} two={two} three={three}");
    // Merging more nodes into one group must not add a per-object allocation: the merged-node and
    // merged-selection buffers stay inline for the duplicates documents actually produce.
    assert_eq!(
        three, one,
        "merging three nodes must cost what one node costs"
    );
    assert_eq!(two, one);
}

#[test]
fn resolver_reads_info() {
    let _measuring = measuring();
    // `echo` reads `field_nodes`, so the merged-node slice is observable and always materialized.
    let one = prepared_allocations("query Q { items { echo } }");
    let three = prepared_allocations("query Q { items { echo echo echo } }");
    eprintln!("resolver_info one={one} three={three}");
}

#[test]
fn resolver_mix() {
    let _measuring = measuring();
    // One resolver-backed field among scalar properties: the object needs the asynchronous path
    // for a single field, which is awaited in place instead of through a batch.
    let one = prepared_allocations("query Q { items { id name echo } }");
    let many = prepared_allocations("query Q { items { id name age echo } }");
    eprintln!("resolver_mix one={one} many={many}");
    // One asynchronous field per object is awaited in place, so the number of scalar properties
    // completed alongside it does not change the per-object cost.
    assert_eq!(one, many);
}

#[test]
#[ignore = "timing probe; run alone with --ignored"]
fn deep_prepare() {
    let _measuring = measuring();
    let schema = item_schema(1);
    let source = {
        let mut query = String::from("query Q { items { id ");
        for _ in 0..40 {
            query.push_str("child { id ");
        }
        for _ in 0..40 {
            query.push_str("} ");
        }
        query.push_str("} }");
        query
    };
    let (prepare_ns, prepare_allocs) = measure(200, || {
        let prepared = PreparedQuery::new(&schema, "deep", &source, None).expect("prepared");
        std::hint::black_box(&prepared);
    });
    eprintln!("deep_prepare ns={prepare_ns} allocs={prepare_allocs}");
}

/// A query with `count` aliased copies of the same nested selection.
fn wide_source(count: usize) -> String {
    let mut fields = String::new();
    for index in 0..count {
        fields.push_str(&format!(
            "a{index}: items {{ id name age child {{ id }} }} "
        ));
    }
    format!("query Q {{ {fields} }}")
}

#[test]
#[ignore = "timing probe; run alone with --ignored"]
fn prepare_scaling() {
    let _measuring = measuring();
    let schema = item_schema(1);
    for count in [50, 100, 200, 400] {
        let source = wide_source(count);
        let (ns, allocs) = measure(200, || {
            let prepared = PreparedQuery::new(&schema, "wide", &source, None).expect("prepared");
            std::hint::black_box(&prepared);
        });
        eprintln!("prepare_scaling fields={count} ns={ns} allocs={allocs}");
    }
}

#[test]
#[ignore = "timing probe; run alone with --ignored"]
fn prepare_split() {
    let _measuring = measuring();
    // How much of preparation is parsing/validation and how much is plan building.
    let schema = item_schema(1);
    let sources = [
        ("deep", {
            let mut query = String::from("query Q { items { id ");
            for _ in 0..40 {
                query.push_str("child { id ");
            }
            for _ in 0..40 {
                query.push_str("} ");
            }
            query.push_str("} }");
            query
        }),
        ("wide", wide_source(200)),
    ];
    for (label, source) in sources {
        let (parse_ns, parse_allocs) = measure(500, || {
            let document = gql::parser::parse_query(&source).expect("parse");
            std::hint::black_box(document);
        });
        let document = gql::parser::parse_query(&source).expect("parse");
        let validation_options = gql::ValidationOptions {
            introspection: true,
            max_depth: 0,
            max_errors: 100,
        };
        let (validate_ns, validate_allocs) = measure(500, || {
            let errors = gql::validate(&schema, &document, &validation_options);
            std::hint::black_box(errors);
        });
        let (prepare_ns, prepare_allocs) = measure(500, || {
            let prepared = PreparedQuery::new(&schema, "split", &source, None).expect("prepared");
            std::hint::black_box(&prepared);
        });
        eprintln!(
            "prepare_split {label} parse_ns={parse_ns} parse_allocs={parse_allocs} validate_ns={validate_ns} validate_allocs={validate_allocs} prepare_ns={prepare_ns} prepare_allocs={prepare_allocs}"
        );
    }
}

#[test]
#[ignore = "timing probe; run alone with --ignored"]
fn execute_forms() {
    let _measuring = measuring();
    let schema = item_schema(1_000);
    let source = "query Q { items { id name age child { id } } }";
    let prepared = PreparedQuery::new(&schema, "probe", source, None).expect("prepared");
    let request = Request::new(source);
    let options = ExecutionOptions::default();

    let (normal_ns, normal_allocs) = measure(200, || {
        let response = futures::executor::block_on(execute(&schema, request, &options));
        std::hint::black_box(response.data);
    });
    let (prepared_ns, prepared_allocs) = measure(200, || {
        let response = futures::executor::block_on(execute_prepared(&prepared, None, &options));
        std::hint::black_box(response.data);
    });
    eprintln!(
        "execute_forms normal_ns={normal_ns} normal_allocs={normal_allocs} prepared_ns={prepared_ns} prepared_allocs={prepared_allocs}"
    );
}

fn stream_count(stream: gql::SubscriptionResponse) -> usize {
    futures::executor::block_on(stream.stream.collect::<Vec<_>>()).len()
}

/// A prepared subscription with `fragments_used` chained fragments, and its per-run cost for
/// 1,000 events.
fn subscription_run(fragments_used: usize) -> (u128, usize) {
    let mut builder = Schema::builder();
    builder.add_type(
        ObjectType::new("User")
            .field(Field::new("id", TypeRef::id().non_null()))
            .field(Field::new("name", TypeRef::string().non_null()))
            .into(),
    );
    // Above the inline-fragment threshold the lookup builds an index, which a prepared document
    // computes once instead of once per event.
    let mut fragments = String::new();
    for index in 0..fragments_used {
        let next = if index + 1 < fragments_used {
            format!(" ...F{}", index + 1)
        } else {
            String::new()
        };
        fragments.push_str(&format!("fragment F{index} on User {{ id name{next} }}\n"));
    }
    let query = ObjectType::new("Query")
        .field(Field::new("health", TypeRef::string().non_null()))
        .field(Field::new("user", TypeRef::named("User")));
    let subscription = ObjectType::new("Subscription").field(
        Field::new("tick", TypeRef::named("User"))
            .subscribe(|_| {
                Box::pin(async {
                    let events = futures::stream::repeat_with(|| {
                        Ok(Value::Object(Arc::new(Object::from_fields([
                            ("id", Value::from("1")),
                            ("name", Value::from("name")),
                        ]))))
                    });
                    Ok(events.take(1_000).boxed())
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

    let source = format!("subscription S {{ tick {{ ...F0 }} }}\n{fragments}");
    let prepared = PreparedQuery::new(&schema, "sub", &source, None).expect("prepared");
    let _ = futures::executor::block_on(prepared.subscribe(None, &ExecutionOptions::default()));
    let (ns, allocs) = measure(10, || {
        let response =
            futures::executor::block_on(prepared.subscribe(None, &ExecutionOptions::default()))
                .expect("subscribed");
        std::hint::black_box(stream_count(response));
    });
    (ns, allocs)
}

#[test]
#[ignore = "timing probe; run alone with --ignored"]
fn subscription_events() {
    let _measuring = measuring();
    for count in [4, 9, 20] {
        let (ns, allocs) = subscription_run(count);
        eprintln!("subscription_events fragments={count} ns={ns} allocs_per_run={allocs}");
    }
}
