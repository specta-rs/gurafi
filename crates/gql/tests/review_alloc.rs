//! Allocation-counting probe for the selection-info foundation.
//!
//! Run alone (the counting allocator is global):
//!
//! ```sh
//! cargo test -p gql --test review_alloc -- --test-threads=1 --nocapture
//! ```

use std::{
    alloc::{GlobalAlloc, Layout, System},
    sync::{
        Arc,
        atomic::{AtomicUsize, Ordering},
    },
};

use gql::{
    ExecutionOptions, Field, Object, ObjectType, PreparedQuery, Request, Schema, TypeRef, Value,
    execute, execute_prepared,
};

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

fn schema(items: usize) -> Schema {
    let item =
        ObjectType::new("Item").field(Field::new("echo", TypeRef::string()).resolve(|ctx| {
            let len = ctx.info.field_nodes.len();
            Box::pin(async move { Ok(Value::from(format!("x{len}"))) })
        }));
    let query = ObjectType::new("Query").field(
        Field::new("items", TypeRef::named("Item").non_null().list().non_null()).resolve(
            move |_| {
                Box::pin(async move {
                    Ok(Value::List(
                        (0..items)
                            .map(|_| Value::Object(Arc::new(Object::new())))
                            .collect(),
                    ))
                })
            },
        ),
    );
    let mut builder = Schema::builder();
    builder.add_type(query.into());
    builder.add_type(item.into());
    builder.query("Query").build().expect("schema")
}

fn measure(source: &str, label: &str) {
    const ITEMS: usize = 1_000;
    let schema = schema(ITEMS);

    // Warm every lazily allocated internal.
    let request = Request::new(source);
    let options = ExecutionOptions::default();
    for _ in 0..10 {
        let _ = futures::executor::block_on(execute(&schema, request, &options));
    }
    let prepared = PreparedQuery::new(&schema, "alloc", source, None).expect("prepared");
    for _ in 0..10 {
        let _ = futures::executor::block_on(execute_prepared(
            &prepared,
            None,
            &ExecutionOptions::default(),
        ));
    }

    let before = ALLOCATIONS.load(Ordering::Relaxed);
    let response = futures::executor::block_on(execute(&schema, request, &options));
    let dynamic = ALLOCATIONS.load(Ordering::Relaxed) - before;
    assert!(response.errors.is_empty(), "{response:?}");

    let before = ALLOCATIONS.load(Ordering::Relaxed);
    let response = futures::executor::block_on(execute_prepared(
        &prepared,
        None,
        &ExecutionOptions::default(),
    ));
    let planned = ALLOCATIONS.load(Ordering::Relaxed) - before;
    assert!(response.errors.is_empty(), "{response:?}");

    eprintln!("{label}: dynamic_allocs={dynamic} prepared_allocs={planned}");
}

#[test]
fn merged_field_node_allocation_cost() {
    measure("query Q { items { echo } }", "one node");
    measure("query Q { items { echo echo } }", "two nodes");
    measure("query Q { items { echo echo echo } }", "three nodes");
    measure(
        "query Q { items { ...F } } fragment F on Item { echo echo echo }",
        "three nodes via fragment",
    );
}

#[test]
fn merge_clone_cost() {
    use gql::{Field, ObjectType, Schema, TypeRef};

    let mut builder = Schema::builder();
    builder.add_type(
        ObjectType::new("Query")
            .field(Field::new("root", TypeRef::named("T0")))
            .into(),
    );
    for index in 0..100 {
        builder.add_type(
            ObjectType::new(format!("T{index}"))
                .field(Field::new("a", TypeRef::string()))
                .field(Field::new("b", TypeRef::string()))
                .field(Field::new("c", TypeRef::int()))
                .field(Field::new("children", TypeRef::named("T0").list()))
                .into(),
        );
    }
    let schema = builder.query("Query").build().expect("schema");

    for _ in 0..10 {
        let _ = schema.merge(&schema).expect("merge");
    }

    let before = ALLOCATIONS.load(Ordering::Relaxed);
    let merged = schema.merge(&schema).expect("merge");
    let merge_allocs = ALLOCATIONS.load(Ordering::Relaxed) - before;

    let before = ALLOCATIONS.load(Ordering::Relaxed);
    let cloned = schema.clone();
    let clone_allocs = ALLOCATIONS.load(Ordering::Relaxed) - before;
    std::hint::black_box(merged);
    std::hint::black_box(cloned);

    eprintln!("merge_allocs={merge_allocs} clone_allocs={clone_allocs}");
}
