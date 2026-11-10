//! Fixed prepared-query benchmark used as the baseline for the execution optimization passes.
//!
//! Run with:
//!
//! ```sh
//! cargo bench -q -p gql --bench prepared
//! ```
//!
//! The workload is the same representative query in both cases, but not the same work: normal
//! `gql::execute` pays parse, validation, operation selection, variable coercion, complexity
//! analysis and execution, while `PreparedQuery::execute` skips parsing, validation and operation
//! selection and pays variable coercion, complexity checking and execution. For a document whose
//! static complexity bound is safe (no field estimators, no unresolved expansion), the prepared
//! path can also skip the exact per-request complexity walk; the bound is conservative, so a
//! request outside it still runs the exact analysis. Numbers are printed as `KEY=VALUE` so runs can
//! be compared without a plotting dependency.
//!
//! Two workloads run. `Dashboard` is fragment-, alias- and variable-heavy and its types define no
//! resolvers. `Records` selects plain properties and one nested object from a type that *does*
//! define a resolver the query never selects: its prepared numbers show whether a selected-fields
//! resolver check lets such a list complete synchronously, and its normal numbers are the control
//! (normal execution always parses and validates, so it moves only with execution changes).
//!
//! Allocations are counted by a thin wrapper around the system allocator. The wrapper only
//! delegates to `System`; there is no custom memory management.

use std::{
    alloc::{GlobalAlloc, Layout, System},
    hint::black_box,
    sync::{
        Arc,
        atomic::{AtomicUsize, Ordering},
    },
    time::{Duration, Instant},
};

use gql::{
    ExecutionOptions, Field, InputValue, Object, ObjectType, Request, Schema, TypeRef, Value,
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

fn user_object(name: &str) -> Arc<Object> {
    let friend = Arc::new(Object::from_fields([
        ("id", Value::from("friend-1")),
        ("name", Value::from("Friend")),
        ("age", Value::Int(30)),
    ]));
    let post = Arc::new(Object::from_fields([
        ("id", Value::from("post-1")),
        ("title", Value::from("Hello")),
        ("published", Value::Boolean(true)),
    ]));
    Arc::new(Object::from_fields([
        ("id", Value::from(name)),
        ("name", Value::from(name)),
        ("age", Value::Int(42)),
        ("posts", Value::List(vec![Value::Object(post)])),
        ("friends", Value::List(vec![Value::Object(friend)])),
    ]))
}

fn user_type() -> ObjectType {
    ObjectType::new("User")
        .field(Field::new("id", TypeRef::id().non_null()))
        .field(Field::new("name", TypeRef::string().non_null()))
        .field(Field::new("age", TypeRef::int().non_null()))
        .field(Field::new(
            "posts",
            TypeRef::named("Post").non_null().list().non_null(),
        ))
        .field(
            Field::new(
                "friends",
                TypeRef::named("User").non_null().list().non_null(),
            )
            .arg(InputValue::new("first", TypeRef::int())),
        )
}

fn post_type() -> ObjectType {
    ObjectType::new("Post")
        .field(Field::new("id", TypeRef::id().non_null()))
        .field(Field::new("title", TypeRef::string().non_null()))
        .field(Field::new("published", TypeRef::boolean().non_null()))
}

/// A record type that defines a resolver the benchmark query does not select.
///
/// Completion has to stay synchronous for the selected properties even though the type has
/// resolver-backed fields elsewhere: a type-wide "has a resolver" check would send every object
/// of this list down the asynchronous path for nothing.
fn record_type() -> ObjectType {
    ObjectType::new("Record")
        .field(Field::new("id", TypeRef::id().non_null()))
        .field(Field::new("name", TypeRef::string().non_null()))
        .field(Field::new("count", TypeRef::int().non_null()))
        .field(Field::new("child", TypeRef::named("Record")))
        .field(
            Field::new("computed", TypeRef::string().non_null())
                .resolve(|_| Box::pin(async { Ok(Value::from("computed")) })),
        )
}

fn mixed_schema() -> Schema {
    let child = Arc::new(Object::from_fields([
        ("id", Value::from("child")),
        ("name", Value::from("Child")),
        ("count", Value::Int(1)),
    ]));
    let records: Arc<Vec<Arc<Object>>> = Arc::new(
        (0..50)
            .map(|index| {
                Arc::new(Object::from_fields([
                    ("id", Value::from(format!("record-{index}"))),
                    ("name", Value::from("Record")),
                    ("count", Value::Int(index)),
                    ("child", Value::Object(Arc::clone(&child))),
                ]))
            })
            .collect(),
    );
    let root = ObjectType::new("Query").field(
        Field::new(
            "records",
            TypeRef::named("Record").non_null().list().non_null(),
        )
        .resolve({
            let records = Arc::clone(&records);
            move |_| {
                let records = Arc::clone(&records);
                Box::pin(async move {
                    Ok(Value::List(
                        records
                            .iter()
                            .map(|record| Value::Object(Arc::clone(record)))
                            .collect(),
                    ))
                })
            }
        }),
    );

    let mut builder = Schema::builder();
    builder.add_type(record_type().into());
    builder.add_type(root.into());
    builder.query("Query").build().expect("benchmark schema")
}

fn query_schema() -> Schema {
    let users: Arc<Vec<Arc<Object>>> = Arc::new(
        (0..5)
            .map(|index| user_object(&format!("user-{index}")))
            .collect(),
    );

    let root = ObjectType::new("Query")
        .field(
            Field::new("users", TypeRef::named("User").non_null().list().non_null())
                .arg(InputValue::new("limit", TypeRef::int()))
                .arg(InputValue::new("active", TypeRef::boolean()))
                .resolve({
                    let users = Arc::clone(&users);
                    move |_| {
                        let users = Arc::clone(&users);
                        Box::pin(async move {
                            Ok(Value::List(
                                users
                                    .iter()
                                    .map(|user| Value::Object(Arc::clone(user)))
                                    .collect(),
                            ))
                        })
                    }
                }),
        )
        .field(
            Field::new("health", TypeRef::string().non_null())
                .resolve(|_| Box::pin(async { Ok(Value::from("ok")) })),
        );

    let mut builder = Schema::builder();
    builder.add_type(user_type().into());
    builder.add_type(post_type().into());
    builder.add_type(root.into());
    builder.query("Query").build().expect("benchmark schema")
}

const QUERY: &str = r#"
query Dashboard($limit: Int = 5, $active: Boolean = true) {
  users(limit: $limit, active: $active) {
    ...UserFields
    posts {
      id
      title
      published
    }
    friends(first: 2) {
      id
      name
    }
  }
  aliased: users(limit: 1) {
    id
    name
  }
  health
}

fragment UserFields on User {
  id
  name
  age @include(if: $active)
}
"#;

/// The `record_type` query: scalar properties, one nested object, and a resolver field the query
/// never selects.
const MIXED_QUERY: &str = r#"
query Records {
  records {
    id
    name
    count
    child {
      id
      name
    }
  }
}
"#;

fn options() -> ExecutionOptions {
    ExecutionOptions::default()
        .max_depth(32)
        .max_complexity(100_000)
        .max_concurrency(16)
}

fn measure<F>(iterations: usize, mut operation: F) -> (Duration, usize)
where
    F: FnMut(),
{
    // Warm caches and lazily allocated internals.
    for _ in 0..100 {
        operation();
    }

    let mut best = Duration::MAX;
    let mut allocations = 0;
    for _ in 0..3 {
        let started = Instant::now();
        let before = ALLOCATIONS.load(Ordering::Relaxed);
        for _ in 0..iterations {
            operation();
        }
        allocations = ALLOCATIONS.load(Ordering::Relaxed) - before;
        best = best.min(started.elapsed());
    }
    (best / iterations as u32, allocations / iterations)
}

fn main() {
    let schema = query_schema();
    let variables =
        Object::from_fields([("limit", Value::Int(5)), ("active", Value::Boolean(true))]);
    let request = Request::new(QUERY).variables(&variables);
    let execution_options = options();

    let prepared = gql::prepared::PreparedQuery::new(&schema, "dashboard", QUERY, None)
        .expect("prepared query");

    let (parse_time, parse_allocs) = measure(5_000, || {
        let document = gql::parser::parse_query(QUERY).expect("valid query");
        black_box(document);
    });

    let document = gql::parser::parse_query(QUERY).expect("valid query");
    let validation_options = gql::ValidationOptions {
        introspection: true,
        max_depth: 32,
        max_errors: 100,
    };
    let (validate_time, validate_allocs) = measure(5_000, || {
        let errors = gql::validate(&schema, &document, &validation_options);
        black_box(errors);
    });

    let (prepare_time, prepare_allocs) = measure(500, || {
        let query = gql::prepared::PreparedQuery::new(&schema, "dashboard", QUERY, None)
            .expect("prepared query");
        black_box(query);
    });

    let (normal_time, normal_allocs) = measure(2_000, || {
        let response =
            futures::executor::block_on(gql::execute(&schema, request, &execution_options));
        black_box(response.data);
    });

    let (prepared_time, prepared_allocs) = measure(2_000, || {
        let response =
            futures::executor::block_on(prepared.execute(Some(&variables), &execution_options));
        black_box(response.data);
    });

    let mixed_schema = mixed_schema();
    let mixed_request = Request::new(MIXED_QUERY);
    let mixed_prepared =
        gql::prepared::PreparedQuery::new(&mixed_schema, "records", MIXED_QUERY, None)
            .expect("prepared mixed query");
    let (mixed_normal_time, mixed_normal_allocs) = measure(2_000, || {
        let response = futures::executor::block_on(gql::execute(
            &mixed_schema,
            mixed_request,
            &execution_options,
        ));
        black_box(response.data);
    });
    let (mixed_prepared_time, mixed_prepared_allocs) = measure(2_000, || {
        let response =
            futures::executor::block_on(mixed_prepared.execute(None, &execution_options));
        black_box(response.data);
    });

    println!(
        "BENCH normal_ns={} normal_allocs={} prepared_ns={} prepared_allocs={} prepare_ns={} prepare_allocs={} parse_ns={} parse_allocs={} validate_ns={} validate_allocs={} mixed_normal_ns={} mixed_normal_allocs={} mixed_prepared_ns={} mixed_prepared_allocs={}",
        normal_time.as_nanos(),
        normal_allocs,
        prepared_time.as_nanos(),
        prepared_allocs,
        prepare_time.as_nanos(),
        prepare_allocs,
        parse_time.as_nanos(),
        parse_allocs,
        validate_time.as_nanos(),
        validate_allocs,
        mixed_normal_time.as_nanos(),
        mixed_normal_allocs,
        mixed_prepared_time.as_nanos(),
        mixed_prepared_allocs,
    );
}
