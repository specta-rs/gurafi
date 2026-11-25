//! Regression coverage for bounded fragment-expansion state in complexity analysis.

use std::{
    alloc::{GlobalAlloc, Layout, System},
    fmt::Write as _,
    sync::{
        Arc, Mutex,
        atomic::{AtomicUsize, Ordering},
    },
};

use futures::executor::block_on;
use gql::{
    ExecutionOptions, Field, Object, ObjectType, Request, Response, Schema, TypeRef, Value,
    complexity::{Complexity, ComplexityOptions, analyze},
};

struct CountingAllocator;

static CURRENT: AtomicUsize = AtomicUsize::new(0);
static PEAK: AtomicUsize = AtomicUsize::new(0);
static TEST_LOCK: Mutex<()> = Mutex::new(());

fn allocated(size: usize) {
    let current = CURRENT.fetch_add(size, Ordering::Relaxed) + size;
    let mut peak = PEAK.load(Ordering::Relaxed);
    while current > peak {
        match PEAK.compare_exchange_weak(peak, current, Ordering::Relaxed, Ordering::Relaxed) {
            Ok(_) => break,
            Err(observed) => peak = observed,
        }
    }
}

fn deallocated(size: usize) {
    CURRENT.fetch_sub(size, Ordering::Relaxed);
}

unsafe impl GlobalAlloc for CountingAllocator {
    unsafe fn alloc(&self, layout: Layout) -> *mut u8 {
        let pointer = unsafe { System.alloc(layout) };
        if !pointer.is_null() {
            allocated(layout.size());
        }
        pointer
    }

    unsafe fn dealloc(&self, pointer: *mut u8, layout: Layout) {
        unsafe { System.dealloc(pointer, layout) };
        deallocated(layout.size());
    }

    unsafe fn realloc(&self, pointer: *mut u8, layout: Layout, new_size: usize) -> *mut u8 {
        let new_pointer = unsafe { System.realloc(pointer, layout, new_size) };
        if !new_pointer.is_null() {
            deallocated(layout.size());
            allocated(new_size);
        }
        new_pointer
    }
}

#[global_allocator]
static ALLOCATOR: CountingAllocator = CountingAllocator;

const MAX_PEAK_GROWTH: usize = 8 * 1024 * 1024;

fn schema(resolver_calls: &Arc<AtomicUsize>) -> Schema {
    let calls = Arc::clone(resolver_calls);
    let node = ObjectType::new("Node").field(Field::new("value", TypeRef::string()));
    let query = ObjectType::new("Query").field(Field::new("node", TypeRef::named("Node")).resolve(
        move |_| {
            calls.fetch_add(1, Ordering::SeqCst);
            Box::pin(async {
                Ok(Value::object(Object::from_fields([(
                    "value",
                    Value::from("ok"),
                )])))
            })
        },
    ));
    let mut builder = Schema::builder();
    builder.add_type(node.into());
    builder.add_type(query.into());
    builder.query("Query").build().expect("valid schema")
}

fn fragment_source(levels: usize) -> String {
    let mut source = String::from("fragment F0 on Query { node { value } }\n");
    for level in 1..=levels {
        source.push_str(&format!(
            "fragment F{level} on Query {{ ...F{} ...F{} }}\n",
            level - 1,
            level - 1
        ));
    }
    source.push_str(&format!("query Q {{ ...F{levels} }}\n"));
    source
}

fn distinct_fragment_source(levels: usize, children_per_level: usize) -> String {
    let mut source = String::from("fragment F0 on Query { node { value } }\n");
    for level in 1..=levels {
        write!(
            source,
            "fragment F{level} on Query {{ ...F{} ...F{} node {{ ",
            level - 1,
            level - 1
        )
        .unwrap();
        for child in 0..children_per_level {
            write!(source, "c{level}_{child}: value ").unwrap();
        }
        source.push_str("} }\n");
    }
    source.push_str(&format!("query Q {{ ...F{levels} }}\n"));
    source
}

fn wide_fragment_source(fields: usize, spreads: usize) -> String {
    let mut source = String::from("fragment F on Query {\n");
    for field in 0..fields {
        write!(source, "f{field}: node {{ value }}\n").unwrap();
    }
    source.push_str("}\nquery Q { ");
    for _ in 0..spreads {
        source.push_str("...F ");
    }
    source.push_str("}\n");
    source
}

fn measure_execute(
    schema: &Schema,
    resolver_calls: &AtomicUsize,
    source: &str,
) -> (usize, Response) {
    let options = ExecutionOptions::default()
        .max_complexity(1)
        .max_depth(32)
        .max_breadth(1_000);
    resolver_calls.store(0, Ordering::SeqCst);
    let baseline = CURRENT.load(Ordering::Relaxed);
    PEAK.store(baseline, Ordering::Relaxed);
    let response = block_on(gql::execute(schema, Request::new(source), &options));
    let growth = PEAK.load(Ordering::Relaxed).saturating_sub(baseline);
    assert!(
        growth < MAX_PEAK_GROWTH,
        "a {}-byte document allocated {growth} bytes before rejection",
        source.len()
    );
    (source.len(), response)
}

#[test]
fn repeated_fragment_expansion_stays_bounded() {
    let _guard = TEST_LOCK
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner());
    let resolver_calls = Arc::new(AtomicUsize::new(0));
    let schema = schema(&resolver_calls);

    for levels in [0, 16, 20] {
        let source = fragment_source(levels);
        let (bytes, response) = measure_execute(&schema, &resolver_calls, &source);
        assert!(bytes < 4096, "fixture must stay compact: {bytes}");
        assert!(response.data.is_none(), "rejected query returned data");
        assert_eq!(resolver_calls.load(Ordering::SeqCst), 0);
        assert_eq!(response.errors.len(), 1, "{response:?}");
        assert!(
            response.errors[0]
                .message()
                .contains("Query complexity 2 exceeds the maximum allowed complexity of 1"),
            "{response:?}"
        );
    }
}

#[test]
fn distinct_fragment_children_hit_the_retained_budget_before_large_allocations() {
    let _guard = TEST_LOCK
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner());
    let resolver_calls = Arc::new(AtomicUsize::new(0));
    let schema = schema(&resolver_calls);
    let source = distinct_fragment_source(127, 8);
    assert!(source.len() < 64 * 1024, "fixture must stay compact");
    let document = gql::parser::parse_query(&source).expect("valid query");

    let baseline = CURRENT.load(Ordering::Relaxed);
    PEAK.store(baseline, Ordering::Relaxed);
    let error = analyze(
        &schema,
        &document,
        None,
        &Object::new(),
        &ComplexityOptions::default(),
    )
    .expect_err("distinct retained children must exhaust the aggregate budget");
    let growth = PEAK.load(Ordering::Relaxed).saturating_sub(baseline);
    assert!(
        growth < MAX_PEAK_GROWTH,
        "a {}-byte document allocated {growth} bytes before the retained-state rejection",
        source.len()
    );
    assert!(
        error.message().contains("retains too many selections"),
        "{error:?}"
    );
}

#[test]
fn repeated_wide_fragment_cache_hits_hit_the_analysis_work_budget() {
    let _guard = TEST_LOCK
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner());
    let resolver_calls = Arc::new(AtomicUsize::new(0));
    let schema = schema(&resolver_calls);
    let source = wide_fragment_source(600, 256);
    assert!(source.len() < 64 * 1024, "fixture must stay compact");

    resolver_calls.store(0, Ordering::SeqCst);
    let response = block_on(gql::execute(
        &schema,
        Request::new(&source),
        &ExecutionOptions::default(),
    ));

    assert!(response.data.is_none(), "rejected query returned data");
    assert_eq!(resolver_calls.load(Ordering::SeqCst), 0);
    assert_eq!(response.errors.len(), 1, "{response:?}");
    assert!(
        response.errors[0]
            .message()
            .contains("too many fields to analyse safely"),
        "{response:?}"
    );
}

#[test]
fn fragment_deduplication_preserves_distinct_children() {
    let _guard = TEST_LOCK
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner());
    let resolver_calls = Arc::new(AtomicUsize::new(0));
    let schema = schema(&resolver_calls);

    let repeated = fragment_source(20);
    let document = gql::parser::parse_query(&repeated).expect("valid query");
    assert_eq!(
        analyze(
            &schema,
            &document,
            None,
            &Object::new(),
            &ComplexityOptions::default(),
        )
        .expect("repeated AST nodes merge to one child selection"),
        Complexity {
            cost: 2,
            depth: 2,
            breadth: 1,
        }
    );

    let distinct = distinct_fragment_source(4, 2);
    let document = gql::parser::parse_query(&distinct).expect("valid query");
    assert_eq!(
        analyze(
            &schema,
            &document,
            None,
            &Object::new(),
            &ComplexityOptions::default(),
        )
        .expect("distinct AST children must all be retained"),
        Complexity {
            cost: 10,
            depth: 2,
            breadth: 9,
        }
    );
}
