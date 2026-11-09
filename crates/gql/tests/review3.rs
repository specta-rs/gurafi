//! Adversarial review probe: complexity accounting for a response key merged from two
//! different (overlapping) parent scopes.

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
fn complexity_undercounts_merged_cross_scope_field() {
    let item_calls = Arc::new(AtomicUsize::new(0));
    let wide_calls = Arc::new(AtomicUsize::new(0));

    let user_type = ObjectType::new("User")
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
        .interface(
            InterfaceType::new("Node")
                .field(Field::new("id", TypeRef::id().non_null()))
                .field(Field::new("item", TypeRef::named("Node"))),
        )
        .object(user_type)
        .object(ObjectType::new("Query").field(
            Field::new("node", TypeRef::named("Node")).resolve(|_| Box::pin(async { Ok(user()) })),
        ))
        .query("Query")
        .build()
        .expect("valid schema");

    // A tree of `wide` aliases: `depth` levels, two aliases each, so 2^depth leaves.
    let depth = 8;
    let mut inner = String::from("id");
    for _ in 0..depth {
        inner = format!("w0: wide {{ {inner} }} w1: wide {{ {inner} }}");
    }
    let query = format!(
        "{{ node {{ ... on Node {{ item {{ id }} }} ... on User {{ item {{ {inner} }} }} }} }}"
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
        },
    );
    println!("analyzer (max_complexity=10): {reported:?}");

    let response = block_on(gql::execute(
        &schema,
        Request::new(&query),
        &ExecutionOptions::default().max_complexity(10).max_depth(0),
    ));
    println!(
        "executed: data_present={} errors={} item_resolvers={} wide_resolvers={}",
        response.data.is_some(),
        response.errors.len(),
        item_calls.load(Ordering::Relaxed),
        wide_calls.load(Ordering::Relaxed),
    );
}
