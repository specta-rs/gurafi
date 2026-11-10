//! Fixed prepared-query benchmark used as the baseline for the execution optimization passes.
//!
//! Run with:
//!
//! ```sh
//! cargo bench -q -p gql --bench prepared
//! ```
//!
//! The workload is the same representative query in both cases: normal `gql::execute` pays parse,
//! validation, operation selection, variable coercion, complexity analysis and execution;
//! `PreparedQuery::execute` pays variable coercion, complexity analysis and execution. Numbers are
//! printed as `KEY=VALUE` so runs can be compared without a plotting dependency.
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

    println!(
        "BENCH normal_ns={} normal_allocs={} prepared_ns={} prepared_allocs={} prepare_ns={} prepare_allocs={} parse_ns={} parse_allocs={} validate_ns={} validate_allocs={}",
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
    );
}
