//! Allocation and timing probe for schema build and composition.
//!
//! This is a measurement harness, not a correctness test. It is `#[ignore]`d so the default
//! `cargo test` run does not pay for the global counting allocator; run it explicitly:
//!
//! ```sh
//! cargo test -p gql --release --test schema_perf -- --ignored --test-threads=1 --nocapture
//! ```

use std::{
    alloc::{GlobalAlloc, Layout, System},
    hint::black_box,
    sync::{
        Arc,
        atomic::{AtomicUsize, Ordering},
    },
    time::Instant,
};

use gql::{Field, ObjectType, Schema, TypeRef};

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

/// How many generated object types a probe schema holds.
const TYPES: usize = 100;

/// A schema shaped like a real one: a query root plus many small object types, some connected.
fn build_schema(offset: usize) -> Schema {
    let mut builder = Schema::builder();
    builder.add_type(
        ObjectType::new("Query")
            .field(Field::new("version", TypeRef::string()))
            .into(),
    );
    for index in 0..TYPES {
        builder.add_type(
            ObjectType::new(format!("T{}", index + offset))
                .field(Field::new("a", TypeRef::string()))
                .field(Field::new("b", TypeRef::string()))
                .field(Field::new("c", TypeRef::int()))
                .field(Field::new(
                    "children",
                    TypeRef::named(format!("T{}", index + offset)).list(),
                ))
                .into(),
        );
    }
    builder.query("Query").build().expect("valid probe schema")
}

fn sdl_schema(offset: usize) -> Schema {
    let mut source = String::from("type Query { root: T0 version: String }\n");
    for index in 0..TYPES {
        source.push_str(&format!(
            "type T{} {{ a: String b: String c: Int children: [T{}] }}\n",
            index + offset,
            index + offset
        ));
    }
    Schema::from_sdl(&source).expect("valid SDL probe schema")
}

fn measure<T>(label: &str, runs: usize, mut f: impl FnMut() -> T) -> (usize, f64) {
    // Warm every lazily allocated internal before the measured run.
    for _ in 0..3 {
        black_box(f());
    }
    let before = ALLOCATIONS.load(Ordering::Relaxed);
    let start = Instant::now();
    for _ in 0..runs {
        black_box(f());
    }
    let elapsed = start.elapsed();
    let allocations = ALLOCATIONS.load(Ordering::Relaxed) - before;
    let per_run = allocations as f64 / runs as f64;
    let micros = elapsed.as_secs_f64() * 1e6 / runs as f64;
    println!(
        "{label}: allocs/run={per_run:.1} time/run={micros:.1}us (runs={runs}, total_allocs={allocations})"
    );
    (allocations, micros)
}

#[test]
#[ignore = "measurement probe; run explicitly with --ignored"]
fn schema_merge_costs() {
    let base = build_schema(0);
    let disjoint = build_schema(TYPES);
    let identical = build_schema(0);
    let cloned = base.clone();
    black_box(&cloned);

    measure("merge_self", 20, || base.merge(&base).expect("merge"));
    measure("merge_disjoint", 20, || {
        base.merge(&disjoint).expect("merge disjoint")
    });
    measure("merge_structurally_identical", 20, || {
        base.merge(&identical).expect("merge identical")
    });
    measure("clone", 20, || base.clone());
    measure("sdl_print", 20, || base.sdl());
}

#[test]
#[ignore = "measurement probe; run explicitly with --ignored"]
fn schema_build_costs() {
    measure("build_code_first", 20, || build_schema(0));
    measure("build_from_sdl", 20, || sdl_schema(0));
    let sdl = sdl_schema(0).sdl();
    measure("print_then_parse_roundtrip", 20, || {
        Schema::from_sdl(&sdl).expect("parse")
    });
}

#[test]
#[ignore = "measurement probe; run explicitly with --ignored"]
fn schema_extend_costs() {
    const STEPS: usize = 8;

    // One extension of an 8-type schema, applied `STEPS` times in a chain.
    let base = sdl_schema(0);
    let mut chain = base.clone();
    for step in 0..STEPS {
        chain = chain
            .extend_sdl(&format!("extend type T0 {{ extra{step}: String }}"))
            .expect("extend");
    }
    black_box(&chain);
    measure("extend_sdl_chain_8", 3, || {
        let mut schema = base.clone();
        for step in 0..STEPS {
            schema = schema
                .extend_sdl(&format!("extend type T0 {{ extra{step}: String }}"))
                .expect("extend");
        }
        schema
    });
    measure("extend_sdl_many_8", 3, || {
        let documents: Vec<String> = (0..STEPS)
            .map(|step| format!("extend type T0 {{ extra{step}: String }}"))
            .collect();
        base.extend_sdl_many(documents.iter().map(String::as_str))
            .expect("extend")
    });
    measure("extend_sdl_one", 20, || {
        base.extend_sdl("extend type T0 { extra: String }")
            .expect("extend")
    });
}

#[test]
#[ignore = "measurement probe; run explicitly with --ignored"]
fn merge_self_scaling() {
    fn schema_of(types: usize) -> Schema {
        let mut builder = Schema::builder();
        builder.add_type(
            ObjectType::new("Query")
                .field(Field::new("version", TypeRef::string()))
                .into(),
        );
        for index in 0..types {
            builder.add_type(
                ObjectType::new(format!("T{index}"))
                    .field(Field::new("a", TypeRef::string()))
                    .field(Field::new("b", TypeRef::int()))
                    .into(),
            );
        }
        builder.query("Query").build().expect("valid probe schema")
    }
    for types in [0usize, 1, 10, 100] {
        let schema = schema_of(types);
        measure(&format!("merge_self_{types}_types"), 20, || {
            schema.merge(&schema).expect("merge self")
        });
    }
}

#[test]
#[ignore = "measurement probe; run explicitly with --ignored"]
fn schema_parse_costs() {
    let sdl = sdl_schema(0).sdl();
    measure("parse_only", 20, || {
        gql::parser::parse_schema(&sdl).expect("parse")
    });
    measure("from_sdl_total", 20, || {
        Schema::from_sdl(&sdl).expect("build")
    });
}

#[test]
#[ignore = "measurement probe; run explicitly with --ignored"]
fn allocator_is_counting() {
    let before = ALLOCATIONS.load(Ordering::Relaxed);
    let _ = Arc::<str>::from("x");
    assert!(ALLOCATIONS.load(Ordering::Relaxed) > before);
}
