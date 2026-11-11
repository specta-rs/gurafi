//! The process-wide panic hook still observes caught resolver panics.

use std::{
    panic::{set_hook, take_hook},
    sync::{Arc, Mutex},
};

use futures::executor::block_on;
use gql::{ExecutionOptions, Field, ObjectType, Request, Schema, TypeRef};

#[test]
fn the_panic_hook_still_runs_for_a_caught_resolver_panic() {
    let schema = Schema::builder()
        .object(
            ObjectType::new("Query").field(
                Field::new("boom", TypeRef::string())
                    .resolve(|_| Box::pin(async { panic!("hook payload") })),
            ),
        )
        .query("Query")
        .build()
        .expect("valid schema");

    let seen = Arc::new(Mutex::new(Vec::new()));
    let hook_seen = Arc::clone(&seen);
    let previous = take_hook();
    set_hook(Box::new(move |info| {
        hook_seen
            .lock()
            .expect("hook lock")
            .push(info.payload().downcast_ref::<&str>().copied());
    }));

    let response = block_on(gql::execute(
        &schema,
        Request::new("{ boom }"),
        &ExecutionOptions::default(),
    ));

    // Restore before asserting so a failed assertion keeps the test harness's hook.
    let _ = take_hook();
    set_hook(previous);

    assert_eq!(response.errors.len(), 1, "{response:?}");
    assert_eq!(response.errors[0].message(), "Internal server error");
    let seen = seen.lock().expect("hook lock");
    assert_eq!(*seen, vec![Some("hook payload")]);
}
