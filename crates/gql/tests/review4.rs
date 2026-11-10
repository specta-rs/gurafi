//! Review repro: the complexity analyzer still under-charges a response key merged from two
//! *interface* scopes. The `refine_scope` fix in `complexity.rs` only prefers a concrete object
//! scope, so an interface/interface merge keeps the first interface and every field that only
//! the other interface declares is charged 1 and never descended into.
//!

use std::sync::{
    Arc,
    atomic::{AtomicUsize, Ordering},
};

use futures::executor::block_on;
use gql::{
    ExecutionOptions, Field, InterfaceType, Object, ObjectType, Request, Schema, TypeRef, Value,
    complexity::{self, ComplexityOptions},
};

fn user() -> Value {
    Value::object(Object::from_fields([("kind", Value::from("user"))]))
}

fn counting_field(name: &'static str, ty: TypeRef, counter: Arc<AtomicUsize>) -> Field {
    Field::new(name, ty).resolve(move |_| {
        let counter = Arc::clone(&counter);
        Box::pin(async move {
            counter.fetch_add(1, Ordering::Relaxed);
            Ok(user())
        })
    })
}

#[test]
fn complexity_undercounts_interface_interface_merge() {
    let item_calls = Arc::new(AtomicUsize::new(0));
    let wide_calls = Arc::new(AtomicUsize::new(0));

    let node = InterfaceType::new("Node")
        .field(Field::new("id", TypeRef::id().non_null()))
        .field(Field::new("item", TypeRef::named("Node")));
    let special = InterfaceType::new("Special")
        .implements("Node")
        .field(Field::new("id", TypeRef::id().non_null()))
        .field(Field::new("item", TypeRef::named("Special")))
        .field(Field::new("wide", TypeRef::named("User")));

    let user_type = ObjectType::new("User")
        .implements("Special")
        .implements("Node")
        .is_type_of(|value| {
            value
                .as_object()
                .and_then(|object| object.get("kind"))
                .and_then(Value::as_str)
                == Some("user")
        })
        .field(
            Field::new("id", TypeRef::id().non_null())
                .resolve(|_| Box::pin(async { Ok(Value::from("1")) })),
        )
        .field(counting_field(
            "item",
            TypeRef::named("User"),
            Arc::clone(&item_calls),
        ))
        .field(counting_field(
            "wide",
            TypeRef::named("User"),
            Arc::clone(&wide_calls),
        ));

    let schema = Schema::builder()
        .interface(node)
        .interface(special)
        .object(user_type)
        .object(ObjectType::new("Query").field(
            Field::new("node", TypeRef::named("Node")).resolve(|_| Box::pin(async { Ok(user()) })),
        ))
        .query("Query")
        .build()
        .expect("valid schema");

    // `depth` levels of two aliases each: 2^depth leaves for a document ~11 levels deep.
    let depth = 8;
    let mut inner = String::from("id");
    for _ in 0..depth {
        inner = format!("w0: wide {{ {inner} }} w1: wide {{ {inner} }}");
    }
    // `item` is selected under `Node` (item: Node) and under `Special` (item: Special).
    let query = format!(
        "{{ node {{ ... on Node {{ item {{ id }} }} ... on Special {{ item {{ {inner} }} }} }} }}"
    );

    let document = gql::parser::parse_query(&query).expect("parses");
    let reported = complexity::analyze(
        &schema,
        &document,
        None,
        &Object::new(),
        &ComplexityOptions {
            max_complexity: 10,
            field_cost: 1,
            list_factor: 10,
            max_depth: 0,
            max_breadth: 0,
        },
    );
    assert!(
        reported.is_err(),
        "complexity must reject the merged wide selection, got {reported:?}"
    );

    // What the request actually does when the analyzer lets it through.
    let response = block_on(gql::execute(
        &schema,
        Request::new(&query),
        &ExecutionOptions::default().max_complexity(10).max_depth(0),
    ));
    let executed = response.data.is_some();
    println!(
        "executed={executed} item_resolvers={} wide_resolvers={}",
        item_calls.load(Ordering::Relaxed),
        wide_calls.load(Ordering::Relaxed),
    );

    assert!(
        reported.is_err(),
        "the analyzer charged {:?} for {} resolver calls; a 2^{depth} tree must exceed a limit \
         of 10",
        reported.as_ref().map(|complexity| complexity.cost),
        wide_calls.load(Ordering::Relaxed) + item_calls.load(Ordering::Relaxed),
    );
    assert!(!executed, "the over-budget query must not have executed");
}
