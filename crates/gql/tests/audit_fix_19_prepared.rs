//! Prepared execution must apply the same analyzer safety budgets as dynamic execution.

use std::{
    fmt::Write as _,
    sync::{
        Arc,
        atomic::{AtomicUsize, Ordering},
    },
};

use futures::executor::block_on;
use gql::{
    ExecutionOptions, Field, Object, ObjectType, Request, Response, Schema, TypeRef, Value,
    prepared::PreparedQuery,
};

fn schema(resolver_calls: &Arc<AtomicUsize>) -> Schema {
    let node_calls = Arc::clone(resolver_calls);
    let node = ObjectType::new("Node")
        .field(Field::new("value", TypeRef::string()))
        .field(
            Field::new("node", TypeRef::named("Node")).resolve(move |_| {
                node_calls.fetch_add(1, Ordering::SeqCst);
                Box::pin(async {
                    Ok(Value::object(Object::from_fields([(
                        "value",
                        Value::from("ok"),
                    )])))
                })
            }),
        );
    let query_calls = Arc::clone(resolver_calls);
    let query = ObjectType::new("Query").field(Field::new("node", TypeRef::named("Node")).resolve(
        move |_| {
            query_calls.fetch_add(1, Ordering::SeqCst);
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

/// Each level retains more distinct children than the previous fragment. The source and
/// static complexity walk stay linear, while the exact analyzer's cumulative retained state is
/// quadratic.
fn retained_fragment_source(levels: usize, children_per_level: usize) -> String {
    let mut source = String::from("fragment F0 on Node { ");
    for child in 0..children_per_level {
        write!(source, "n0_{child}: node {{ value }} ").unwrap();
    }
    source.push_str("}\n");
    for level in 1..=levels {
        write!(source, "fragment F{level} on Node {{ ...F{} ", level - 1).unwrap();
        for child in 0..children_per_level {
            write!(source, "n{level}_{child}: node {{ value }} ").unwrap();
        }
        source.push_str("}\n");
    }
    source.push_str(&format!("query Q {{ node {{ ...F{levels} }} }}\n"));
    source
}

fn error_messages(response: &Response) -> Vec<&str> {
    response
        .errors
        .iter()
        .map(gql::GraphQLError::message)
        .collect()
}

#[test]
fn prepared_matches_dynamic_retained_budget_with_default_and_permissive_limits() {
    for (max_complexity, max_depth, max_breadth) in [(0, 0, 0), (u32::MAX, 32, 32)] {
        let resolver_calls = Arc::new(AtomicUsize::new(0));
        let schema = schema(&resolver_calls);
        let source = retained_fragment_source(64, 17);
        assert!(source.len() < 64 * 1024, "fixture must stay compact");
        let prepared = PreparedQuery::new(&schema, "retained", &source, None).expect("prepared");
        let options = ExecutionOptions::default()
            .max_complexity(max_complexity)
            .max_depth(max_depth)
            .max_breadth(max_breadth);

        resolver_calls.store(0, Ordering::SeqCst);
        let dynamic = block_on(gql::execute(&schema, Request::new(&source), &options));
        let after = block_on(prepared.execute(None, &options));

        assert_eq!(error_messages(&dynamic), error_messages(&after));
        assert!(dynamic.data.is_none(), "{dynamic:?}");
        assert!(after.data.is_none(), "{after:?}");
        assert_eq!(dynamic.errors.len(), 1, "{dynamic:?}");
        assert!(
            dynamic.errors[0]
                .message()
                .contains("retains too many selections"),
            "{dynamic:?}"
        );
        assert_eq!(resolver_calls.load(Ordering::SeqCst), 0);
    }
}

#[test]
fn retained_safety_proof_does_not_reject_safe_fragment_chains() {
    let resolver_calls = Arc::new(AtomicUsize::new(0));
    let schema = schema(&resolver_calls);
    let source = retained_fragment_source(4, 2);
    let prepared = PreparedQuery::new(&schema, "safe", &source, None).expect("prepared");
    let options = ExecutionOptions::default()
        .max_complexity(u32::MAX)
        .max_depth(32)
        .max_breadth(32);

    let dynamic = block_on(gql::execute(&schema, Request::new(&source), &options));
    let after = block_on(prepared.execute(None, &options));

    assert!(dynamic.errors.is_empty(), "{dynamic:?}");
    assert!(after.errors.is_empty(), "{after:?}");
    assert_eq!(dynamic.data, after.data);
    assert!(resolver_calls.load(Ordering::SeqCst) > 0);
}
