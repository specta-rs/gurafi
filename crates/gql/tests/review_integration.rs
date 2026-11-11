//! Review probes for the merge of resolver panic isolation and synchronous property resolvers.
//!
//! These tests are deliberately separate from the feature suites: they exercise the *interaction*
//! between the two features, the paths the existing suites do not cover, and the boundary of what
//! `catch_unwind` is allowed to catch.

#![allow(dead_code)]

use std::{
    panic::{AssertUnwindSafe, catch_unwind, set_hook, take_hook},
    sync::{
        Arc, Mutex,
        atomic::{AtomicUsize, Ordering},
    },
};

use futures::{StreamExt, executor::block_on, stream};
use gql::{
    AuthorizationContext, Authorizer, ExecutionOptions, Field, GraphQLError, InterfaceType, Object,
    ObjectType, PathSegment, PreparedQuery, Request, Response, Schema, TypeRef, UnionType, Value,
};

const INTERNAL_MESSAGE: &str = "Internal server error";
const INTERNAL_CODE: &str = "INTERNAL_SERVER_ERROR";

fn run(schema: &Schema, query: &str) -> Response {
    block_on(gql::execute(
        schema,
        Request::new(query),
        &ExecutionOptions::default(),
    ))
}

fn value(fields: impl IntoIterator<Item = (&'static str, Value)>) -> Value {
    Value::object(Object::from_fields(fields))
}

fn code(error: &GraphQLError) -> Option<&str> {
    error.extensions()?.get("code").and_then(Value::as_str)
}

fn path(error: &GraphQLError) -> Vec<String> {
    error
        .path()
        .unwrap_or_default()
        .iter()
        .map(ToString::to_string)
        .collect()
}

fn assert_internal(error: &GraphQLError, expected_path: &[&str]) {
    assert_eq!(error.message(), INTERNAL_MESSAGE, "{error:?}");
    assert_eq!(code(error), Some(INTERNAL_CODE), "{error:?}");
    assert_eq!(
        path(error),
        expected_path
            .iter()
            .map(|segment| (*segment).to_owned())
            .collect::<Vec<_>>(),
        "{error:?}"
    );
    assert!(
        !error.locations().is_empty(),
        "field completion must attach a source location: {error:?}"
    );
}

fn data_object(response: &Response) -> &Object {
    response
        .data
        .as_ref()
        .and_then(Value::as_object)
        .unwrap_or_else(|| panic!("response has no object data: {response:?}"))
}

/// Serialises the tests that swap the process-wide panic hook.
static HOOK_LOCK: Mutex<()> = Mutex::new(());

/// Count panics whose payload is exactly `marker`, ignoring the other tests panicking in this
/// binary at the same time.
fn count_marker_panics<T>(marker: &'static str, run: impl FnOnce() -> T) -> (T, usize) {
    let _guard = HOOK_LOCK
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner());
    let seen = Arc::new(AtomicUsize::new(0));
    let hook_seen = Arc::clone(&seen);
    let previous = take_hook();
    set_hook(Box::new(move |info| {
        if info.payload().downcast_ref::<&str>() == Some(&marker) {
            hook_seen.fetch_add(1, Ordering::Relaxed);
        }
    }));

    let outcome = catch_unwind(AssertUnwindSafe(run));

    let _ = take_hook();
    set_hook(previous);

    let count = seen.load(Ordering::Relaxed);
    match outcome {
        Ok(value) => (value, count),
        Err(payload) => std::panic::resume_unwind(payload),
    }
}

// ---------------------------------------------------------------------------
// Synchronous property resolver: panic isolation
// ---------------------------------------------------------------------------

#[test]
fn sync_resolve_panic_is_a_coded_field_error_and_siblings_complete() {
    let schema = Schema::builder()
        .object(
            ObjectType::new("Query")
                .field(
                    Field::new("boom", TypeRef::string())
                        .sync_resolve(|_| panic!("sync accessor payload")),
                )
                .field(Field::new("ok", TypeRef::string())),
        )
        .query("Query")
        .build()
        .expect("valid schema");
    let options = ExecutionOptions::default().root_value(value([("ok", Value::from("fine"))]));

    let response = block_on(gql::execute(&schema, Request::new("{ boom ok }"), &options));

    assert_eq!(
        response.data,
        Some(value([("boom", Value::Null), ("ok", Value::from("fine"))]))
    );
    assert_eq!(response.errors.len(), 1, "{response:?}");
    assert_internal(&response.errors[0], &["boom"]);
}

#[test]
fn sync_resolve_panic_on_the_leaf_fast_path_is_isolated() {
    // No resolver slot and a built-in scalar type means this field takes the inline leaf path;
    // the panic must still be caught around the synchronous hook.
    let schema = Schema::builder()
        .object(
            ObjectType::new("Query")
                .field(
                    Field::new("boom", TypeRef::string().non_null())
                        .sync_resolve(|_| panic!("leaf sync payload")),
                )
                .field(Field::new("fast", TypeRef::string().non_null())),
        )
        .query("Query")
        .build()
        .expect("valid schema");
    let options = ExecutionOptions::default().root_value(value([("fast", Value::from("leaf"))]));

    let response = block_on(gql::execute(
        &schema,
        Request::new("{ boom fast }"),
        &options,
    ));

    // `boom` is non-null, so the whole root object nulls; `fast` must not be lost from the
    // error path bookkeeping.
    assert_eq!(response.data, Some(Value::Null));
    assert_eq!(response.errors.len(), 1, "{response:?}");
    assert_internal(&response.errors[0], &["boom"]);
}

#[test]
fn sync_resolve_panic_inside_a_list_is_a_field_error_with_an_index_path() {
    let item = ObjectType::new("Item")
        .field(Field::new("id", TypeRef::id().non_null()))
        .field(
            Field::new("boom", TypeRef::string()).sync_resolve(|context| {
                let id = context
                    .parent
                    .as_object()
                    .and_then(|object| object.get("id"))
                    .and_then(Value::as_str)
                    .unwrap_or_default()
                    .to_owned();
                if id == "2" {
                    panic!("list sync payload");
                }
                Ok(Value::from("ok"))
            }),
        );
    let schema = Schema::builder()
        .object(item)
        .object(ObjectType::new("Query").field(
            Field::new("items", TypeRef::named("Item").list()).resolve(|_| {
                Box::pin(async {
                    Ok(Value::List(vec![
                        value([("id", Value::from("1"))]),
                        value([("id", Value::from("2"))]),
                    ]))
                })
            }),
        ))
        .query("Query")
        .build()
        .expect("valid schema");

    let response = run(&schema, "{ items { id boom } }");

    let items = data_object(&response)
        .get("items")
        .and_then(Value::as_list)
        .expect("items list");
    assert_eq!(
        items[0],
        value([("id", Value::from("1")), ("boom", Value::from("ok"))])
    );
    // `boom` is nullable, so only the field nulls; the item keeps its other data.
    assert_eq!(
        items[1],
        value([("id", Value::from("2")), ("boom", Value::Null)])
    );
    assert_eq!(response.errors.len(), 1, "{response:?}");
    assert_internal(&response.errors[0], &["items", "1", "boom"]);
}

#[test]
fn sync_resolve_panic_reports_one_error_when_the_fast_path_is_abandoned() {
    // The synchronous subtree attempt runs the panicking hook, then bails out because a sibling
    // needs the async path; the retry must not duplicate the reported error.
    let item = ObjectType::new("Item")
        .field(
            Field::new("boom", TypeRef::string()).sync_resolve(|_| panic!("retried sync payload")),
        )
        .field(
            Field::new("later", TypeRef::string())
                .resolve(|_| Box::pin(async { Ok(Value::from("async")) })),
        );
    let schema = Schema::builder()
        .object(item)
        .object(
            ObjectType::new("Query").field(
                Field::new("item", TypeRef::named("Item").non_null())
                    .resolve(|_| Box::pin(async { Ok(Value::object(Object::new())) })),
            ),
        )
        .query("Query")
        .build()
        .expect("valid schema");

    let (response, panics) = count_marker_panics("retried sync payload", || {
        run(&schema, "{ item { boom later } }")
    });

    assert_eq!(response.errors.len(), 1, "{response:?}");
    assert_internal(&response.errors[0], &["item", "boom"]);
    assert_eq!(
        data_object(&response)
            .get("item")
            .and_then(Value::as_object)
            .and_then(|item| item.get("later")),
        Some(&Value::from("async"))
    );
    assert!(
        panics >= 1,
        "the panic hook must observe the caught panic (saw {panics})"
    );
}

#[test]
fn sync_resolve_hook_does_not_run_when_the_authorizer_denies() {
    let calls = Arc::new(AtomicUsize::new(0));
    let hook_calls = Arc::clone(&calls);
    let schema =
        Schema::builder()
            .object(ObjectType::new("Query").field(
                Field::new("name", TypeRef::string()).sync_resolve(move |_| {
                    hook_calls.fetch_add(1, Ordering::Relaxed);
                    Ok(Value::from("secret"))
                }),
            ))
            .query("Query")
            .build()
            .expect("valid schema");

    struct Deny;
    impl Authorizer for Deny {
        fn authorize(&self, _: &AuthorizationContext<'_>) -> Result<(), GraphQLError> {
            Err(GraphQLError::new("denied"))
        }
    }
    let options = ExecutionOptions::default().authorizer(Deny);

    let response = block_on(gql::execute(&schema, Request::new("{ name }"), &options));

    assert_eq!(response.errors.len(), 1, "{response:?}");
    assert_eq!(response.errors[0].message(), "denied");
    assert_eq!(
        calls.load(Ordering::Relaxed),
        0,
        "authorization must run before the synchronous property read"
    );
}

// ---------------------------------------------------------------------------
// Synchronous property resolver: precedence and fast-path behaviour
// ---------------------------------------------------------------------------

#[test]
fn prepared_plan_runs_the_sync_hook_once_and_does_not_read_the_raw_property() {
    let calls = Arc::new(AtomicUsize::new(0));
    let hook_calls = Arc::clone(&calls);
    let item = ObjectType::new("Item").field(
        Field::new("name", TypeRef::string().non_null()).sync_resolve(move |_| {
            hook_calls.fetch_add(1, Ordering::Relaxed);
            Ok(Value::from("from the hook"))
        }),
    );
    let schema = Schema::builder()
        .object(item)
        .object(
            ObjectType::new("Query").field(
                Field::new("item", TypeRef::named("Item").non_null())
                    .resolve(|_| Box::pin(async { Ok(value([("name", Value::from("raw"))])) })),
            ),
        )
        .query("Query")
        .build()
        .expect("valid schema");
    let prepared =
        PreparedQuery::new(&schema, "sync-plan", "{ item { name } }", None).expect("prepared");

    let response = block_on(prepared.execute(None, &ExecutionOptions::default()));

    assert!(
        response.errors.is_empty(),
        "unexpected errors: {response:?}"
    );
    assert_eq!(
        data_object(&response)
            .get("item")
            .and_then(Value::as_object)
            .and_then(|item| item.get("name")),
        Some(&Value::from("from the hook"))
    );
    assert_eq!(
        calls.load(Ordering::Relaxed),
        1,
        "the planned property shortcut must not bypass or repeat a sync hook"
    );
}

#[test]
fn prepared_plan_prefers_an_async_resolver_over_a_sync_one() {
    let item = ObjectType::new("Item").field(
        Field::new("name", TypeRef::string().non_null())
            .sync_resolve(|_| Ok(Value::from("sync")))
            .resolve(|_| Box::pin(async { Ok(Value::from("async")) })),
    );
    let schema = Schema::builder()
        .object(item)
        .object(
            ObjectType::new("Query").field(
                Field::new("item", TypeRef::named("Item").non_null())
                    .resolve(|_| Box::pin(async { Ok(Value::object(Object::new())) })),
            ),
        )
        .query("Query")
        .build()
        .expect("valid schema");
    let prepared =
        PreparedQuery::new(&schema, "both-plan", "{ item { name } }", None).expect("prepared");

    let response = block_on(prepared.execute(None, &ExecutionOptions::default()));

    assert!(
        response.errors.is_empty(),
        "unexpected errors: {response:?}"
    );
    assert_eq!(
        data_object(&response)
            .get("item")
            .and_then(Value::as_object)
            .and_then(|item| item.get("name")),
        Some(&Value::from("async"))
    );
}

#[test]
fn a_sync_only_type_still_uses_the_synchronous_subtree_path() {
    // `type_has_resolvers` must not treat a sync hook as an async resolver; otherwise this would
    // still work but pay for the async path. The observable proof is that the hook runs once per
    // item and no future-based resolver is installed.
    let calls = Arc::new(AtomicUsize::new(0));
    let hook_calls = Arc::clone(&calls);
    let item = ObjectType::new("Item").field(
        Field::new("name", TypeRef::string().non_null()).sync_resolve(move |_| {
            hook_calls.fetch_add(1, Ordering::Relaxed);
            Ok(Value::from("item"))
        }),
    );
    let schema = Schema::builder()
        .object(item)
        .object(ObjectType::new("Query").field(
            Field::new("items", TypeRef::named("Item").non_null().list().non_null()).resolve(
                |_| {
                    Box::pin(async {
                        Ok(Value::List(vec![
                            Value::object(Object::new()),
                            Value::object(Object::new()),
                            Value::object(Object::new()),
                        ]))
                    })
                },
            ),
        ))
        .query("Query")
        .build()
        .expect("valid schema");

    let response = run(&schema, "{ items { name } }");

    assert!(
        response.errors.is_empty(),
        "unexpected errors: {response:?}"
    );
    assert_eq!(calls.load(Ordering::Relaxed), 3);
}

#[test]
fn a_sync_resolve_error_keeps_normal_null_propagation() {
    let item = ObjectType::new("Item")
        .field(
            Field::new("name", TypeRef::string().non_null())
                .sync_resolve(|_| Err(GraphQLError::new("hook failed"))),
        )
        .field(Field::new("ok", TypeRef::string()));
    let schema = Schema::builder()
        .object(item)
        .object(
            ObjectType::new("Query").field(
                Field::new("item", TypeRef::named("Item").non_null())
                    .resolve(|_| Box::pin(async { Ok(value([("ok", Value::from("fine"))])) })),
            ),
        )
        .query("Query")
        .build()
        .expect("valid schema");

    let response = run(&schema, "{ item { name ok } }");

    // `item` itself is non-null, so the non-null `name` error bubbles to the root.
    assert_eq!(response.data, Some(Value::Null));
    assert_eq!(response.errors.len(), 1, "{response:?}");
    assert_eq!(response.errors[0].message(), "hook failed");
    assert_eq!(path(&response.errors[0]), vec!["item", "name"]);
}

// ---------------------------------------------------------------------------
// Abstract-type panic isolation
// ---------------------------------------------------------------------------

fn abstract_schema(interface: InterfaceType, field_ty: TypeRef) -> Schema {
    Schema::builder()
        .object(
            ObjectType::new("User")
                .implements("Node")
                .field(Field::new("id", TypeRef::id().non_null())),
        )
        .interface(interface)
        .object(
            ObjectType::new("Query").field(
                Field::new("node", field_ty)
                    .resolve(|_| Box::pin(async { Ok(value([("id", Value::from("1"))])) })),
            ),
        )
        .query("Query")
        .build()
        .expect("valid schema")
}

#[test]
fn resolve_type_panic_on_a_non_null_abstract_field_nulls_the_parent() {
    let node = InterfaceType::new("Node")
        .field(Field::new("id", TypeRef::id().non_null()))
        .resolve_type(|_, _| panic!("resolve_type non-null payload"));
    let schema = abstract_schema(node, TypeRef::named("Node").non_null());

    let response = run(&schema, "{ node { id } }");

    assert_eq!(response.data, Some(Value::Null));
    assert_eq!(response.errors.len(), 1, "{response:?}");
    assert_internal(&response.errors[0], &["node"]);
}

#[test]
fn is_type_of_panic_for_one_list_item_only_nulls_that_item() {
    let node = InterfaceType::new("Node").field(Field::new("id", TypeRef::id().non_null()));
    let schema = Schema::builder()
        .object(
            ObjectType::new("User")
                .implements("Node")
                .is_type_of(|value| {
                    let id = value
                        .as_object()
                        .and_then(|object| object.get("id"))
                        .and_then(Value::as_str)
                        .unwrap_or_default();
                    if id == "2" {
                        panic!("is_type_of list payload");
                    }
                    true
                })
                .field(Field::new("id", TypeRef::id().non_null())),
        )
        .interface(node)
        .object(ObjectType::new("Query").field(
            Field::new("nodes", TypeRef::named("Node").list()).resolve(|_| {
                Box::pin(async {
                    Ok(Value::List(vec![
                        value([("id", Value::from("1"))]),
                        value([("id", Value::from("2"))]),
                        value([("id", Value::from("3"))]),
                    ]))
                })
            }),
        ))
        .query("Query")
        .build()
        .expect("valid schema");

    let response = run(&schema, "{ nodes { id } }");

    let nodes = data_object(&response)
        .get("nodes")
        .and_then(Value::as_list)
        .expect("nodes list");
    assert_eq!(nodes.len(), 3);
    assert_eq!(nodes[0], value([("id", Value::from("1"))]));
    assert_eq!(nodes[1], Value::Null);
    assert_eq!(nodes[2], value([("id", Value::from("3"))]));
    assert_eq!(response.errors.len(), 1, "{response:?}");
    assert_internal(&response.errors[0], &["nodes", "1"]);
}

#[test]
fn resolve_type_panic_is_isolated_in_a_prepared_plan() {
    let node = InterfaceType::new("Node")
        .field(Field::new("id", TypeRef::id().non_null()))
        .resolve_type(|_, _| panic!("prepared resolve_type payload"));
    let schema = abstract_schema(node, TypeRef::named("Node"));
    let prepared =
        PreparedQuery::new(&schema, "abstract-plan", "{ node { id } }", None).expect("prepared");

    let response = block_on(prepared.execute(None, &ExecutionOptions::default()));

    assert_eq!(data_object(&response).get("node"), Some(&Value::Null));
    assert_eq!(response.errors.len(), 1, "{response:?}");
    assert_internal(&response.errors[0], &["node"]);
}

#[test]
fn union_resolve_type_panic_is_isolated() {
    let union = UnionType::new("Search")
        .member("User")
        .resolve_type(|_, _| panic!("union resolve_type payload"));
    let schema = Schema::builder()
        .object(ObjectType::new("User").field(Field::new("id", TypeRef::id().non_null())))
        .union(union)
        .object(
            ObjectType::new("Query").field(
                Field::new("search", TypeRef::named("Search"))
                    .resolve(|_| Box::pin(async { Ok(value([("id", Value::from("1"))])) })),
            ),
        )
        .query("Query")
        .build()
        .expect("valid schema");

    let response = run(&schema, "{ search { ... on User { id } } }");

    assert_eq!(data_object(&response).get("search"), Some(&Value::Null));
    assert_eq!(response.errors.len(), 1, "{response:?}");
    assert_internal(&response.errors[0], &["search"]);
}

// ---------------------------------------------------------------------------
// Subscription events with synchronous resolvers
// ---------------------------------------------------------------------------

fn subscription_schema(field: Field) -> Schema {
    Schema::builder()
        .object(ObjectType::new("Query").field(Field::new("ping", TypeRef::string())))
        .object(ObjectType::new("Subscription").field(field))
        .query("Query")
        .subscription("Subscription")
        .build()
        .expect("valid schema")
}

#[test]
fn subscription_event_sync_hook_panic_keeps_the_stream_running() {
    let schema = subscription_schema(
        Field::new("tick", TypeRef::int().non_null())
            .subscribe(|_| {
                Box::pin(async {
                    Ok(stream::iter(vec![
                        Ok::<Value, GraphQLError>(Value::Int(1)),
                        Ok(Value::Int(2)),
                        Ok(Value::Int(3)),
                    ])
                    .boxed())
                })
            })
            .sync_resolve(|context| {
                if matches!(context.parent, Value::Int(2)) {
                    panic!("event sync payload");
                }
                Ok(context.parent.clone())
            }),
    );

    let subscription = block_on(gql::subscribe(
        &schema,
        Request::new("subscription { tick }"),
        &ExecutionOptions::default(),
    ))
    .expect("subscription starts");
    let responses = block_on(subscription.stream.collect::<Vec<_>>());

    assert_eq!(responses.len(), 3, "the panic must not end the stream");
    assert_eq!(responses[0].data, Some(value([("tick", Value::Int(1))])));
    assert_eq!(responses[1].data, Some(Value::Null));
    assert_internal(&responses[1].errors[0], &["tick"]);
    assert_eq!(responses[2].data, Some(value([("tick", Value::Int(3))])));
}

#[test]
fn subscription_event_nested_sync_hook_panic_only_nulls_that_field() {
    let item = ObjectType::new("Item")
        .field(Field::new("id", TypeRef::id().non_null()))
        .field(
            Field::new("boom", TypeRef::string()).sync_resolve(|context| {
                let id = context
                    .parent
                    .as_object()
                    .and_then(|object| object.get("id"))
                    .and_then(Value::as_str)
                    .unwrap_or_default()
                    .to_owned();
                if id == "2" {
                    panic!("nested event sync payload");
                }
                Ok(Value::from("ok"))
            }),
        );
    let schema = Schema::builder()
        .object(item)
        .object(ObjectType::new("Query").field(Field::new("ping", TypeRef::string())))
        .object(
            ObjectType::new("Subscription").field(
                Field::new("event", TypeRef::named("Item").non_null())
                    .subscribe(|_| {
                        Box::pin(async {
                            Ok(stream::iter(vec![
                                Ok::<Value, GraphQLError>(value([("id", Value::from("1"))])),
                                Ok(value([("id", Value::from("2"))])),
                            ])
                            .boxed())
                        })
                    })
                    // Each source event is the operation's root value.
                    .resolve(|context| Box::pin(async { Ok(context.parent.clone()) })),
            ),
        )
        .query("Query")
        .subscription("Subscription")
        .build()
        .expect("valid schema");

    let subscription = block_on(gql::subscribe(
        &schema,
        Request::new("subscription { event { id boom } }"),
        &ExecutionOptions::default(),
    ))
    .expect("subscription starts");
    let responses = block_on(subscription.stream.collect::<Vec<_>>());

    assert_eq!(responses.len(), 2, "the panic must not end the stream");
    assert_eq!(
        responses[0].data,
        Some(value([(
            "event",
            value([("id", Value::from("1")), ("boom", Value::from("ok"))])
        )]))
    );
    // `boom` is nullable, so only that field nulls and the event keeps its other data.
    assert_eq!(
        responses[1].data,
        Some(value([(
            "event",
            value([("id", Value::from("2")), ("boom", Value::Null)])
        )]))
    );
    assert_eq!(responses[1].errors.len(), 1, "{:?}", responses[1]);
    assert_internal(&responses[1].errors[0], &["event", "boom"]);
}

// ---------------------------------------------------------------------------
// Payload shapes / boundary of catch_unwind
// ---------------------------------------------------------------------------

struct PanickingDebug;

impl std::fmt::Debug for PanickingDebug {
    fn fmt(&self, _: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        panic!("Debug impl of the panic payload ran");
    }
}

#[test]
fn a_payload_whose_debug_panics_does_not_cause_a_second_panic() {
    let schema = Schema::builder()
        .object(
            ObjectType::new("Query").field(
                Field::new("boom", TypeRef::string())
                    .resolve(|_| Box::pin(async { std::panic::panic_any(PanickingDebug) })),
            ),
        )
        .query("Query")
        .build()
        .expect("valid schema");

    let (response, _) = count_marker_panics("unused marker", || run(&schema, "{ boom }"));

    assert_eq!(response.errors.len(), 1, "{response:?}");
    assert_internal(&response.errors[0], &["boom"]);
    assert_eq!(data_object(&response).get("boom"), Some(&Value::Null));
}

#[test]
fn an_authorizer_panic_is_not_caught_by_resolver_isolation() {
    // The documented boundary is the resolver call. An authorizer is caller code too, but it is
    // deliberately outside the isolated region, so this probe records the current behaviour: the
    // panic escapes `execute` rather than becoming a field error.
    struct Explode;
    impl Authorizer for Explode {
        fn authorize(&self, _: &AuthorizationContext<'_>) -> Result<(), GraphQLError> {
            panic!("authorizer exploded");
        }
    }

    let schema = Schema::builder()
        .object(ObjectType::new("Query").field(Field::new("ok", TypeRef::string())))
        .query("Query")
        .build()
        .expect("valid schema");
    let options = ExecutionOptions::default().authorizer(Explode);

    let outcome = catch_unwind(AssertUnwindSafe(|| {
        block_on(gql::execute(&schema, Request::new("{ ok }"), &options))
    }));

    assert!(
        outcome.is_err(),
        "the authorizer panic escaped the resolver boundary, as documented"
    );
}

#[test]
fn a_resolver_panic_is_caught_even_when_a_sync_hook_runs_in_the_same_subtree() {
    // Regression probe for the merge: the async sibling's panic must not be swallowed by the
    // synchronous attempt, and the sync hook's value must survive the retry.
    let calls = Arc::new(AtomicUsize::new(0));
    let hook_calls = Arc::clone(&calls);
    let item = ObjectType::new("Item")
        .field(
            Field::new("name", TypeRef::string()).sync_resolve(move |_| {
                hook_calls.fetch_add(1, Ordering::Relaxed);
                Ok(Value::from("sync value"))
            }),
        )
        .field(
            Field::new("boom", TypeRef::string())
                .resolve(|_| Box::pin(async { panic!("async sibling payload") })),
        );
    let schema = Schema::builder()
        .object(item)
        .object(
            ObjectType::new("Query").field(
                Field::new("item", TypeRef::named("Item").non_null())
                    .resolve(|_| Box::pin(async { Ok(Value::object(Object::new())) })),
            ),
        )
        .query("Query")
        .build()
        .expect("valid schema");

    let response = run(&schema, "{ item { name boom } }");

    let item = data_object(&response)
        .get("item")
        .and_then(Value::as_object)
        .expect("item object");
    assert_eq!(item.get("name"), Some(&Value::from("sync value")));
    assert_eq!(item.get("boom"), Some(&Value::Null));
    assert_eq!(response.errors.len(), 1, "{response:?}");
    assert_internal(&response.errors[0], &["item", "boom"]);
    assert!(
        calls.load(Ordering::Relaxed) >= 1,
        "the sync hook must have produced the value"
    );
}

#[test]
fn resolve_none_still_reads_the_parent_property() {
    let schema = Schema::builder()
        .object(ObjectType::new("Query").field(Field::new("name", TypeRef::string())))
        .query("Query")
        .build()
        .expect("valid schema");
    let options = ExecutionOptions::default().root_value(value([("name", Value::from("default"))]));

    let response = block_on(gql::execute(&schema, Request::new("{ name }"), &options));

    assert!(
        response.errors.is_empty(),
        "unexpected errors: {response:?}"
    );
    assert_eq!(
        data_object(&response).get("name"),
        Some(&Value::from("default"))
    );
}

#[test]
fn missing_parent_property_completes_as_null_without_a_resolver() {
    let schema = Schema::builder()
        .object(
            ObjectType::new("Query")
                .field(Field::new("name", TypeRef::string()))
                .field(Field::new("ok", TypeRef::string().non_null())),
        )
        .query("Query")
        .build()
        .expect("valid schema");
    let options = ExecutionOptions::default().root_value(value([("ok", Value::from("fine"))]));

    let response = block_on(gql::execute(&schema, Request::new("{ name ok }"), &options));

    assert!(
        response.errors.is_empty(),
        "unexpected errors: {response:?}"
    );
    assert_eq!(
        response.data,
        Some(value([("name", Value::Null), ("ok", Value::from("fine"))]))
    );
}

#[test]
fn sync_hook_panic_path_has_no_extra_segments() {
    // A panic caught on the leaf fast path must not leave the response key on the shared path,
    // otherwise later sibling errors would carry an extra segment.
    let item = ObjectType::new("Item")
        .field(
            Field::new("boom", TypeRef::string())
                .sync_resolve(|_| panic!("leaf path pollution payload")),
        )
        .field(
            Field::new("later", TypeRef::string())
                .resolve(|_| Box::pin(async { Err(GraphQLError::new("later failed")) })),
        );
    let schema = Schema::builder()
        .object(item)
        .object(
            ObjectType::new("Query").field(
                Field::new("item", TypeRef::named("Item").non_null())
                    .resolve(|_| Box::pin(async { Ok(Value::object(Object::new())) })),
            ),
        )
        .query("Query")
        .build()
        .expect("valid schema");

    let response = run(&schema, "{ item { boom later } }");

    assert_eq!(response.errors.len(), 2, "{response:?}");
    assert_internal(&response.errors[0], &["item", "boom"]);
    assert_eq!(path(&response.errors[1]), vec!["item", "later"]);
}

#[test]
fn a_panicking_sync_hook_on_an_interface_field_is_isolated() {
    let node = InterfaceType::new("Node").field(Field::new("name", TypeRef::string()));
    let schema =
        Schema::builder()
            .object(
                ObjectType::new("User")
                    .implements("Node")
                    .field(Field::new("id", TypeRef::id().non_null()))
                    .field(
                        Field::new("name", TypeRef::string())
                            .sync_resolve(|_| panic!("interface sync payload")),
                    ),
            )
            .interface(node)
            .object(ObjectType::new("Query").field(
                Field::new("node", TypeRef::named("Node")).resolve(|_| {
                    Box::pin(async {
                        Ok(value([
                            ("id", Value::from("1")),
                            ("__typename", Value::from("User")),
                        ]))
                    })
                }),
            ))
            .query("Query")
            .build()
            .expect("valid schema");

    let response = run(&schema, "{ node { name } }");

    assert_eq!(
        data_object(&response).get("node"),
        Some(&value([("name", Value::Null)]))
    );
    assert_eq!(response.errors.len(), 1, "{response:?}");
    assert_internal(&response.errors[0], &["node", "name"]);
}

#[test]
fn sync_resolve_and_async_resolve_can_both_be_set_without_leaking_the_hook() {
    let mut field = Field::new("name", TypeRef::string())
        .sync_resolve(|_| Ok(Value::from("sync")))
        .resolve(|_| Box::pin(async { Ok(Value::from("async")) }));
    assert!(field.has_resolver());
    let _resolve = field.take_resolve().expect("async resolver");
    assert!(field.resolve.is_none());
    assert!(
        field.sync_resolve.is_none(),
        "take_resolve must drop the lower-precedence sync hook"
    );
    assert!(!field.has_resolver());
}

#[test]
fn a_field_with_only_a_sync_hook_reports_has_resolver() {
    let field = Field::new("name", TypeRef::string()).sync_resolve(|_| Ok(Value::Null));
    assert!(field.has_resolver());
    assert!(field.resolve.is_none());
    assert!(field.sync_resolve.is_some());
}

#[test]
fn path_segments_used_by_the_probes_match_the_public_type() {
    // Compile-time guard: the probes assume `PathSegment::field` is the public constructor.
    let segment = PathSegment::field("name");
    assert_eq!(segment.to_string(), "name");
}

// ---------------------------------------------------------------------------
// How often a sync hook runs when the synchronous attempt cannot finish
// ---------------------------------------------------------------------------

#[test]
fn a_sync_hook_on_a_custom_scalar_field_runs_once() {
    let calls = Arc::new(AtomicUsize::new(0));
    let hook_calls = Arc::clone(&calls);
    let scalar = gql::ScalarType::new("Odd").serialize(|value| Ok(value.clone()));
    let schema = Schema::builder()
        .object(ObjectType::new("Query").field(
            Field::new("odd", TypeRef::named("Odd")).sync_resolve(move |_| {
                hook_calls.fetch_add(1, Ordering::Relaxed);
                Ok(Value::from("value"))
            }),
        ))
        .scalar(scalar)
        .query("Query")
        .build()
        .expect("valid schema");

    let response = run(&schema, "{ odd }");

    assert!(
        response.errors.is_empty(),
        "unexpected errors: {response:?}"
    );
    assert_eq!(
        data_object(&response).get("odd"),
        Some(&Value::from("value"))
    );
    assert_eq!(
        calls.load(Ordering::Relaxed),
        1,
        "a custom scalar is completed by the async path without retrying the hook"
    );
}

#[test]
fn a_sync_hook_on_an_abstract_field_runs_once() {
    let calls = Arc::new(AtomicUsize::new(0));
    let hook_calls = Arc::clone(&calls);
    let node = InterfaceType::new("Node").field(Field::new("id", TypeRef::id()));
    let schema = Schema::builder()
        .object(
            ObjectType::new("User")
                .implements("Node")
                .field(Field::new("id", TypeRef::id())),
        )
        .interface(node)
        .object(ObjectType::new("Query").field(
            Field::new("node", TypeRef::named("Node")).sync_resolve(move |_| {
                hook_calls.fetch_add(1, Ordering::Relaxed);
                Ok(value([
                    ("id", Value::from("1")),
                    ("__typename", Value::from("User")),
                ]))
            }),
        ))
        .query("Query")
        .build()
        .expect("valid schema");

    let response = run(&schema, "{ node { id } }");

    assert!(
        response.errors.is_empty(),
        "unexpected errors: {response:?}"
    );
    assert_eq!(
        calls.load(Ordering::Relaxed),
        1,
        "an abstract type is completed by the async path without retrying the hook"
    );
}

#[test]
fn a_retried_sync_hook_that_panics_reports_one_error() {
    let schema = Schema::builder()
        .object(
            ObjectType::new("Query").field(
                Field::new("odd", TypeRef::named("Odd"))
                    .sync_resolve(|_| panic!("retried custom scalar payload")),
            ),
        )
        .scalar(gql::ScalarType::new("Odd").serialize(|value| Ok(value.clone())))
        .query("Query")
        .build()
        .expect("valid schema");

    let (response, panics) =
        count_marker_panics("retried custom scalar payload", || run(&schema, "{ odd }"));

    assert_eq!(response.errors.len(), 1, "{response:?}");
    assert_internal(&response.errors[0], &["odd"]);
    assert_eq!(
        panics, 1,
        "a panicking hook returns a field error, so it is not retried"
    );
}

// ---------------------------------------------------------------------------
// Subscription setup construction and boundary probes
// ---------------------------------------------------------------------------

#[test]
fn a_subscribe_closure_that_panics_before_returning_a_stream_is_isolated() {
    let schema = subscription_schema(
        Field::new("tick", TypeRef::int().non_null())
            .subscribe(|_| panic!("subscribe construction payload")),
    );

    let response = block_on(gql::subscribe(
        &schema,
        Request::new("subscription { tick }"),
        &ExecutionOptions::default(),
    ))
    .expect_err("a setup panic must not start a stream");

    assert_eq!(response.data, None);
    assert_eq!(response.errors.len(), 1, "{response:?}");
    assert_eq!(response.errors[0].message(), INTERNAL_MESSAGE);
    assert_eq!(code(&response.errors[0]), Some(INTERNAL_CODE));
    assert_eq!(response.errors[0].path(), None);
}

#[test]
fn a_resolver_panicking_after_an_await_point_is_isolated() {
    let schema = Schema::builder()
        .object(
            ObjectType::new("Query").field(Field::new("boom", TypeRef::string()).resolve(|_| {
                Box::pin(async {
                    futures::future::ready(()).await;
                    panic!("post-await payload")
                })
            })),
        )
        .query("Query")
        .build()
        .expect("valid schema");

    let response = run(&schema, "{ boom }");

    assert_eq!(data_object(&response).get("boom"), Some(&Value::Null));
    assert_eq!(response.errors.len(), 1, "{response:?}");
    assert_internal(&response.errors[0], &["boom"]);
}

#[test]
fn prepared_subscription_event_sync_hook_panic_keeps_the_stream_running() {
    let schema = subscription_schema(
        Field::new("tick", TypeRef::int().non_null())
            .subscribe(|_| {
                Box::pin(async {
                    Ok(stream::iter(vec![
                        Ok::<Value, GraphQLError>(Value::Int(1)),
                        Ok(Value::Int(2)),
                    ])
                    .boxed())
                })
            })
            .sync_resolve(|context| {
                if matches!(context.parent, Value::Int(2)) {
                    panic!("prepared event sync payload");
                }
                Ok(context.parent.clone())
            }),
    );
    let prepared = PreparedQuery::new(&schema, "sync-subscription", "subscription { tick }", None)
        .expect("prepared subscription");

    let subscription = block_on(prepared.subscribe(None, &ExecutionOptions::default()))
        .expect("subscription starts");
    let responses = block_on(subscription.stream.collect::<Vec<_>>());

    assert_eq!(responses.len(), 2, "the panic must not end the stream");
    assert_eq!(responses[0].data, Some(value([("tick", Value::Int(1))])));
    assert_eq!(responses[1].data, Some(Value::Null));
    assert_internal(&responses[1].errors[0], &["tick"]);
}

#[test]
fn a_panic_in_the_source_stream_poll_is_outside_the_isolation_boundary() {
    // Only resolver calls are isolated. A panic while polling the subscription source stream is
    // not a resolver call, so it unwinds the subscriber; this probe records that boundary.
    struct BoomStream;

    impl futures::Stream for BoomStream {
        type Item = Result<Value, GraphQLError>;

        fn poll_next(
            self: std::pin::Pin<&mut Self>,
            _: &mut std::task::Context<'_>,
        ) -> std::task::Poll<Option<Self::Item>> {
            panic!("source stream poll payload");
        }
    }

    let schema = subscription_schema(
        Field::new("tick", TypeRef::int().non_null())
            .subscribe(|_| Box::pin(async { Ok(Box::pin(BoomStream) as gql::SubscriptionStream) })),
    );

    let subscription = block_on(gql::subscribe(
        &schema,
        Request::new("subscription { tick }"),
        &ExecutionOptions::default(),
    ))
    .expect("subscription setup succeeds");

    let outcome = catch_unwind(AssertUnwindSafe(|| {
        block_on(subscription.stream.collect::<Vec<_>>())
    }));
    assert!(
        outcome.is_err(),
        "the source stream poll is not a resolver boundary"
    );
}

#[test]
fn a_panic_in_a_custom_scalar_serializer_is_outside_the_isolation_boundary() {
    // Scalar serializers are caller code invoked by the engine, but not through a resolver slot,
    // so they are not isolated. This probe records the documented boundary.
    let schema = Schema::builder()
        .object(
            ObjectType::new("Query").field(
                Field::new("odd", TypeRef::named("Odd"))
                    .resolve(|_| Box::pin(async { Ok(Value::from("boom")) })),
            ),
        )
        .scalar(gql::ScalarType::new("Odd").serialize(|_| panic!("serializer payload")))
        .query("Query")
        .build()
        .expect("valid schema");

    let outcome = catch_unwind(AssertUnwindSafe(|| run(&schema, "{ odd }")));
    assert!(
        outcome.is_err(),
        "a scalar serializer panic is not caught by resolver isolation"
    );
}

// ---------------------------------------------------------------------------
// Differential: a sync hook must behave exactly like an equivalent async resolver
// ---------------------------------------------------------------------------

fn item_values() -> Vec<Value> {
    vec![
        value([
            ("id", Value::from("1")),
            ("name", Value::from("one")),
            ("tags", Value::List(vec![Value::from("a")])),
            ("child", value([("value", Value::from("c1"))])),
        ]),
        value([
            ("id", Value::from("2")),
            ("name", Value::from("two")),
            ("tags", Value::List(vec![Value::from("b")])),
            ("child", value([("value", Value::from("c2"))])),
        ]),
    ]
}

/// Read `field` from the parent value, or return the same error for both variants.
fn read_value(parent: &Value, field: &str) -> gql::FieldResult {
    let object = parent
        .as_object()
        .ok_or_else(|| GraphQLError::new("expected an object"))?;
    if field == "boom" {
        return Err(GraphQLError::new("boom failed"));
    }
    Ok(object.get(field).cloned().unwrap_or(Value::Null))
}

fn differential_schema(sync: bool) -> Schema {
    let mut item = ObjectType::new("Item").field(Field::new("id", TypeRef::id().non_null()));
    let mut child = ObjectType::new("Child");

    if sync {
        item = item
            .field(
                Field::new("name", TypeRef::string())
                    .sync_resolve(|c| read_value(c.parent, "name")),
            )
            .field(
                Field::new("tags", TypeRef::string().list())
                    .sync_resolve(|c| read_value(c.parent, "tags")),
            )
            .field(
                Field::new("child", TypeRef::named("Child"))
                    .sync_resolve(|c| read_value(c.parent, "child")),
            )
            .field(
                Field::new("boom", TypeRef::string())
                    .sync_resolve(|c| read_value(c.parent, "boom")),
            );
        child = child.field(
            Field::new("value", TypeRef::string()).sync_resolve(|c| read_value(c.parent, "value")),
        );
    } else {
        item = item
            .field(
                Field::new("name", TypeRef::string())
                    .resolve(|c| Box::pin(async move { read_value(c.parent, "name") })),
            )
            .field(
                Field::new("tags", TypeRef::string().list())
                    .resolve(|c| Box::pin(async move { read_value(c.parent, "tags") })),
            )
            .field(
                Field::new("child", TypeRef::named("Child"))
                    .resolve(|c| Box::pin(async move { read_value(c.parent, "child") })),
            )
            .field(
                Field::new("boom", TypeRef::string())
                    .resolve(|c| Box::pin(async move { read_value(c.parent, "boom") })),
            );
        child = child.field(
            Field::new("value", TypeRef::string())
                .resolve(|c| Box::pin(async move { read_value(c.parent, "value") })),
        );
    }

    let query = ObjectType::new("Query")
        .field(
            Field::new("items", TypeRef::named("Item").list())
                .resolve(|_| Box::pin(async { Ok(Value::List(item_values())) })),
        )
        .field(
            Field::new("viewer", TypeRef::named("Item"))
                .resolve(|_| Box::pin(async { Ok(item_values().into_iter().next().unwrap()) })),
        );
    Schema::builder()
        .object(item)
        .object(child)
        .object(query)
        .query("Query")
        .build()
        .expect("valid schema")
}

#[test]
fn a_sync_hook_matches_an_async_resolver_across_query_shapes() {
    let async_schema = differential_schema(false);
    let sync_schema = differential_schema(true);

    let cases: &[(&str, Option<Object>)] = &[
        ("{ items { id name } }", None),
        ("{ items { n: name } }", None),
        ("{ items { id name @skip(if: true) } }", None),
        ("{ items { id child { value } } }", None),
        ("{ items { id tags } }", None),
        ("{ items { id boom } }", None),
        ("{ viewer { id name } }", None),
        ("{ items { ...F } } fragment F on Item { id name }", None),
        (
            "query Q($includeName: Boolean!) { items { id name @include(if: $includeName) } }",
            Some(Object::from_fields([(
                "includeName",
                Value::Boolean(false),
            )])),
        ),
    ];

    for (query, variables) in cases {
        let variables = variables.as_ref();
        let async_response = block_on(gql::execute(
            &async_schema,
            Request {
                query,
                operation_name: None,
                variables,
            },
            &ExecutionOptions::default(),
        ));
        let sync_response = block_on(gql::execute(
            &sync_schema,
            Request {
                query,
                operation_name: None,
                variables,
            },
            &ExecutionOptions::default(),
        ));
        assert_eq!(
            format!("{async_response:?}"),
            format!("{sync_response:?}"),
            "unprepared responses differ for `{query}`"
        );

        let async_prepared =
            PreparedQuery::new(&async_schema, "diff-async", query, None).expect("prepared");
        let sync_prepared =
            PreparedQuery::new(&sync_schema, "diff-sync", query, None).expect("prepared");
        let async_response =
            block_on(async_prepared.execute(variables, &ExecutionOptions::default()));
        let sync_response =
            block_on(sync_prepared.execute(variables, &ExecutionOptions::default()));
        assert_eq!(
            format!("{async_response:?}"),
            format!("{sync_response:?}"),
            "prepared responses differ for `{query}`"
        );
    }
}

#[test]
fn a_sync_hook_matches_the_default_property_lookup() {
    fn build(sync: bool) -> Schema {
        let item = ObjectType::new("Item")
            .field(Field::new("id", TypeRef::id().non_null()))
            .field(Field::new("score", TypeRef::int()))
            .field(Field::new("name", TypeRef::string().non_null()));
        let query = ObjectType::new("Query")
            .field(
                Field::new("items", TypeRef::named("Item").list()).resolve(|_| {
                    Box::pin(async {
                        Ok(Value::List(vec![
                            value([
                                ("id", Value::from("1")),
                                ("name", Value::from("one")),
                                ("score", Value::Int(7)),
                            ]),
                            value([("id", Value::from("2")), ("name", Value::from("two"))]),
                        ]))
                    })
                }),
            )
            .field(Field::new("viewer", TypeRef::named("Item")).resolve(|_| {
                Box::pin(async {
                    Ok(value([
                        ("id", Value::from("3")),
                        ("name", Value::from("three")),
                        ("score", Value::Int(9)),
                    ]))
                })
            }));
        let schema = Schema::builder()
            .object(item)
            .object(query)
            .query("Query")
            .build()
            .expect("valid schema");
        if !sync {
            return schema;
        }
        schema
            .transform(|_, field| match field.name.as_ref() {
                "name" | "score" => gql::FieldTransform::Replace(field.clone().sync_resolve(|c| {
                    Ok(c.parent
                        .as_object()
                        .and_then(|object| object.get(c.field_name))
                        .cloned()
                        .unwrap_or(Value::Null))
                })),
                _ => gql::FieldTransform::Keep,
            })
            .expect("transform applies")
    }

    let default_schema = build(false);
    let sync_schema = build(true);

    for query in [
        "{ items { id name score } }",
        "{ items { s: score name } }",
        "{ viewer { id name score } }",
        "{ items { ... on Item { id score } } }",
        "{ items { id name @skip(if: true) score } }",
    ] {
        let default_response = block_on(gql::execute(
            &default_schema,
            Request::new(query),
            &ExecutionOptions::default(),
        ));
        let sync_response = block_on(gql::execute(
            &sync_schema,
            Request::new(query),
            &ExecutionOptions::default(),
        ));
        assert_eq!(
            format!("{default_response:?}"),
            format!("{sync_response:?}"),
            "responses differ for `{query}`"
        );
    }
}

#[test]
fn merging_a_sync_hook_with_an_async_resolver_silently_prefers_async() {
    // Two schemas define the same field with two different implementations, one synchronous and
    // one asynchronous. They occupy different slots, so the merge does not report a conflict and
    // the field ends up with both; execution then prefers `resolve`.
    let item_resolver: gql::ResolveFn =
        Arc::new(|_| Box::pin(async { Ok(Value::object(Object::new())) }));

    let build = |sync: bool| {
        let field = if sync {
            Field::new("value", TypeRef::string()).sync_resolve(|_| Ok(Value::from("sync")))
        } else {
            Field::new("value", TypeRef::string())
                .resolve(|_| Box::pin(async { Ok(Value::from("async")) }))
        };
        let item = ObjectType::new("Item").field(field);
        let mut item_field = Field::new("item", TypeRef::named("Item"));
        item_field.resolve = Some(Arc::clone(&item_resolver));
        let query = ObjectType::new("Query").field(item_field);
        Schema::builder()
            .object(item)
            .object(query)
            .query("Query")
            .build()
            .expect("valid schema")
    };

    let merged = build(true).merge(&build(false)).expect("schemas merge");
    let field = merged
        .object_type("Item")
        .and_then(|object| object.get_field("value"))
        .expect("field exists");
    eprintln!(
        "MERGE PROBE: resolve={} sync_resolve={}",
        field.resolve.is_some(),
        field.sync_resolve.is_some()
    );

    let response = run(&merged, "{ item { value } }");
    assert_eq!(
        data_object(&response)
            .get("item")
            .and_then(Value::as_object)
            .and_then(|item| item.get("value")),
        Some(&Value::from("async")),
        "the async slot wins when merge leaves both slots set"
    );
}

#[test]
fn a_complexity_estimator_panic_is_outside_the_isolation_boundary() {
    // Complexity estimators are caller code, but they run during analysis, not through a
    // resolver slot, so resolver isolation does not cover them. This probe records the boundary.
    let schema = Schema::builder()
        .object(
            ObjectType::new("Query").field(
                Field::new("value", TypeRef::string())
                    .resolve(|_| Box::pin(async { Ok(Value::from("ok")) }))
                    .complexity(|_| panic!("complexity estimator payload")),
            ),
        )
        .query("Query")
        .build()
        .expect("valid schema");
    let options = ExecutionOptions::default().max_complexity(100);

    let outcome = catch_unwind(AssertUnwindSafe(|| {
        block_on(gql::execute(&schema, Request::new("{ value }"), &options))
    }));
    assert!(
        outcome.is_err(),
        "a complexity estimator panic is not caught by resolver isolation"
    );
}

#[test]
fn a_scalar_list_beside_a_scalar_property_on_a_resolver_type_does_not_panic() {
    // `leaf_field` classifies a field by its *named* type, so a list of scalars looks like a
    // leaf. When the object can only complete on the asynchronous path (any field of the type
    // defines a resolver), the all-leaves batch asserts the inline predicate and the leaf fast
    // path agree.
    let item = ObjectType::new("Item")
        .field(Field::new("id", TypeRef::id().non_null()))
        .field(Field::new("tags", TypeRef::string().list()))
        .field(
            Field::new("later", TypeRef::string())
                .resolve(|_| Box::pin(async { Ok(Value::from("async")) })),
        );
    let schema = Schema::builder()
        .object(item)
        .object(ObjectType::new("Query").field(
            Field::new("item", TypeRef::named("Item").non_null()).resolve(|_| {
                Box::pin(async {
                    Ok(value([
                        ("id", Value::from("1")),
                        ("tags", Value::List(vec![Value::from("a")])),
                    ]))
                })
            }),
        ))
        .query("Query")
        .build()
        .expect("valid schema");

    let response = run(&schema, "{ item { id tags } }");

    assert!(
        response.errors.is_empty(),
        "unexpected errors: {response:?}"
    );
    assert_eq!(
        data_object(&response)
            .get("item")
            .and_then(Value::as_object)
            .and_then(|item| item.get("tags")),
        Some(&Value::List(vec![Value::from("a")]))
    );

    // The planned path has the same all-leaves batch and the same assertion.
    let prepared = PreparedQuery::new(&schema, "leaf-list", "{ item { id tags } }", None)
        .expect("prepared query");
    let response = block_on(prepared.execute(None, &ExecutionOptions::default()));
    assert!(response.errors.is_empty(), "prepared errors: {response:?}");
    assert_eq!(
        data_object(&response)
            .get("item")
            .and_then(Value::as_object)
            .and_then(|item| item.get("tags")),
        Some(&Value::List(vec![Value::from("a")]))
    );
}

#[test]
fn a_prepared_scalar_list_beside_a_scalar_property_on_a_resolver_type_does_not_panic() {
    let item = ObjectType::new("Item")
        .field(Field::new("id", TypeRef::id().non_null()))
        .field(Field::new("tags", TypeRef::string().list()))
        .field(
            Field::new("later", TypeRef::string())
                .resolve(|_| Box::pin(async { Ok(Value::from("async")) })),
        );
    let schema = Schema::builder()
        .object(item)
        .object(ObjectType::new("Query").field(
            Field::new("item", TypeRef::named("Item").non_null()).resolve(|_| {
                Box::pin(async {
                    Ok(value([
                        ("id", Value::from("1")),
                        ("tags", Value::List(vec![Value::from("a")])),
                    ]))
                })
            }),
        ))
        .query("Query")
        .build()
        .expect("valid schema");
    let prepared = PreparedQuery::new(&schema, "prepared-leaf-list", "{ item { id tags } }", None)
        .expect("prepared query");

    let response = block_on(prepared.execute(None, &ExecutionOptions::default()));

    assert!(response.errors.is_empty(), "prepared errors: {response:?}");
    assert_eq!(
        data_object(&response)
            .get("item")
            .and_then(Value::as_object)
            .and_then(|item| item.get("tags")),
        Some(&Value::List(vec![Value::from("a")]))
    );
}
