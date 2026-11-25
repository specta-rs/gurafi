use std::{
    alloc::{GlobalAlloc, Layout, System},
    fmt::Write as _,
    sync::atomic::{AtomicUsize, Ordering},
};

use gql::{GraphQLError, Schema, ValidationOptions, parser::parse_query, validate};

struct CountingAllocator;

static ALLOCATED_BYTES: AtomicUsize = AtomicUsize::new(0);

unsafe impl GlobalAlloc for CountingAllocator {
    unsafe fn alloc(&self, layout: Layout) -> *mut u8 {
        let pointer = unsafe { System.alloc(layout) };
        if !pointer.is_null() {
            ALLOCATED_BYTES.fetch_add(layout.size(), Ordering::Relaxed);
        }
        pointer
    }

    unsafe fn alloc_zeroed(&self, layout: Layout) -> *mut u8 {
        let pointer = unsafe { System.alloc_zeroed(layout) };
        if !pointer.is_null() {
            ALLOCATED_BYTES.fetch_add(layout.size(), Ordering::Relaxed);
        }
        pointer
    }

    unsafe fn dealloc(&self, pointer: *mut u8, layout: Layout) {
        unsafe { System.dealloc(pointer, layout) }
    }

    unsafe fn realloc(&self, pointer: *mut u8, layout: Layout, new_size: usize) -> *mut u8 {
        let pointer = unsafe { System.realloc(pointer, layout, new_size) };
        if !pointer.is_null() {
            ALLOCATED_BYTES.fetch_add(new_size, Ordering::Relaxed);
        }
        pointer
    }
}

#[global_allocator]
static ALLOCATOR: CountingAllocator = CountingAllocator;

fn schema() -> Schema {
    Schema::from_sdl(
        "type Query { x: String left: String right: Int box: Box } type Box { x: String }",
    )
    .expect("valid schema")
}

fn validate_source(source: &str) -> Vec<GraphQLError> {
    let document = parse_query(source).expect("document parses");
    validate(
        &schema(),
        &document,
        &ValidationOptions {
            introspection: true,
            max_depth: 32,
            max_errors: 100,
        },
    )
}

fn measure_validation(source: &str) -> (Vec<GraphQLError>, usize) {
    let document = parse_query(source).expect("document parses");
    let schema = schema();
    ALLOCATED_BYTES.store(0, Ordering::Relaxed);
    let errors = validate(
        &schema,
        &document,
        &ValidationOptions {
            introspection: true,
            max_depth: 32,
            max_errors: 100,
        },
    );
    let allocated = ALLOCATED_BYTES.load(Ordering::Relaxed);
    (errors, allocated)
}

fn duplicate_alias_query(count: usize) -> String {
    let mut source = String::with_capacity(count * 5 + 3);
    source.push('{');
    for _ in 0..count {
        source.push_str(" x: x");
    }
    source.push_str(" }");
    source
}

fn recursive_duplicate_query(count: usize) -> String {
    let mut source = String::with_capacity(count * 10 + 30);
    source.push_str("{ box: box {");
    for _ in 0..count {
        source.push_str(" x: x");
    }
    source.push_str(" } box: box {");
    for _ in 0..count {
        source.push_str(" x: x");
    }
    source.push_str(" } }");
    source
}

fn grouped_duplicate_query(groups: usize, per_group: usize) -> String {
    let mut source = String::with_capacity(groups * per_group * 8 + 3);
    source.push('{');
    for group in 0..groups {
        for _ in 0..per_group {
            write!(&mut source, " k{group}: x").expect("write to query");
        }
    }
    source.push_str(" }");
    source
}

fn assert_budget_error(errors: &[GraphQLError], allocated: usize) {
    assert_eq!(errors.len(), 1, "expected one budget error: {errors:?}");
    assert!(
        errors[0].message().contains("comparison budget"),
        "unexpected budget error: {}",
        errors[0]
    );
    assert!(
        allocated < 128 * 1024 * 1024,
        "overlap validation allocated {allocated} bytes"
    );
}

#[test]
fn overlap_comparison_budget_covers_direct_and_recursive_pairs() {
    assert!(validate_source(&duplicate_alias_query(32)).is_empty());

    let conflict = validate_source("{ x: left x: right }");
    assert_eq!(conflict.len(), 1, "{conflict:?}");
    assert!(
        conflict[0]
            .message()
            .contains("they have differing field names"),
        "{conflict:?}"
    );
    assert_eq!(conflict[0].locations().len(), 2);

    let (errors, allocated) = measure_validation(&duplicate_alias_query(4_000));
    assert_budget_error(&errors, allocated);

    let (errors, allocated) = measure_validation(&recursive_duplicate_query(2_000));
    assert_budget_error(&errors, allocated);

    let (errors, allocated) = measure_validation(&grouped_duplicate_query(10, 150));
    assert_budget_error(&errors, allocated);
}
