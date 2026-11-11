//! Resolver panic isolation.
//!
//! A panic in user resolver code is caught at the resolver boundary and reported as a normal
//! field error. These tests cover every resolver invocation site — dynamic and planned execution,
//! object, root, list-item and abstract field completion, subscription setup and per-event
//! resolvers — plus payload shapes, sibling completion and the sync leaf fast path.

use futures::{StreamExt, executor::block_on, stream};
use gql::{
    ExecutionOptions, Field, GraphQLError, InterfaceType, Object, ObjectType, Request, Response,
    Schema, TypeRef, Value, prepared::PreparedQuery,
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
    assert_eq!(error.message(), INTERNAL_MESSAGE);
    assert_eq!(code(error), Some(INTERNAL_CODE));
    assert_eq!(
        path(error),
        expected_path
            .iter()
            .map(|segment| (*segment).to_owned())
            .collect::<Vec<_>>()
    );
    assert!(
        !error.locations().is_empty(),
        "field completion attaches a source location: {error:?}"
    );
}

fn error(response: &Response, index: usize) -> &GraphQLError {
    response
        .errors
        .get(index)
        .unwrap_or_else(|| panic!("no error at index {index}: {response:?}"))
}

fn data_object(response: &Response) -> &Object {
    response
        .data
        .as_ref()
        .and_then(Value::as_object)
        .unwrap_or_else(|| panic!("response has no object data: {response:?}"))
}

#[test]
fn root_resolver_panic_is_a_coded_field_error_and_siblings_complete() {
    let schema = Schema::builder()
        .object(
            ObjectType::new("Query")
                .field(
                    Field::new("boom", TypeRef::string())
                        .resolve(|_| Box::pin(async { panic!("root resolver payload") })),
                )
                .field(
                    Field::new("ok", TypeRef::string().non_null())
                        .resolve(|_| Box::pin(async { Ok(Value::from("fine")) })),
                ),
        )
        .query("Query")
        .build()
        .expect("valid schema");

    let response = run(&schema, "{ boom ok }");

    assert_eq!(
        response.data,
        Some(value([("boom", Value::Null), ("ok", Value::from("fine")),]))
    );
    assert_eq!(response.errors.len(), 1, "{response:?}");
    assert_internal(error(&response, 0), &["boom"]);
}

#[test]
fn object_field_resolver_panic_is_isolated() {
    let user = ObjectType::new("User").field(
        Field::new("name", TypeRef::string())
            .resolve(|_| Box::pin(async { panic!("object resolver payload") })),
    );
    let schema = Schema::builder()
        .object(user)
        .object(
            ObjectType::new("Query")
                .field(
                    Field::new("user", TypeRef::named("User"))
                        .resolve(|_| Box::pin(async { Ok(value([("name", Value::from("Ada"))])) })),
                )
                .field(
                    Field::new("sibling", TypeRef::string())
                        .resolve(|_| Box::pin(async { Ok(Value::from("present")) })),
                ),
        )
        .query("Query")
        .build()
        .expect("valid schema");

    let response = run(&schema, "{ user { name } sibling }");

    let data = data_object(&response);
    assert_eq!(
        data.get("user")
            .and_then(Value::as_object)
            .and_then(|user| user.get("name")),
        Some(&Value::Null)
    );
    assert_eq!(data.get("sibling"), Some(&Value::from("present")));
    assert_internal(error(&response, 0), &["user", "name"]);
}

#[test]
fn list_item_resolver_panic_is_isolated() {
    let item = ObjectType::new("Item")
        .field(Field::new("id", TypeRef::id().non_null()))
        .field(
            Field::new("value", TypeRef::string().non_null()).resolve(|context| {
                let id = context
                    .parent
                    .as_object()
                    .and_then(|object| object.get("id"))
                    .and_then(Value::as_str)
                    .unwrap_or_default()
                    .to_owned();
                Box::pin(async move {
                    if id == "2" {
                        panic!("list item payload");
                    }
                    Ok(Value::from("ok"))
                })
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

    let response = run(&schema, "{ items { id value } }");

    let items = data_object(&response)
        .get("items")
        .and_then(Value::as_list)
        .expect("items list");
    assert_eq!(items.len(), 2);
    assert_eq!(
        items[0],
        value([("id", Value::from("1")), ("value", Value::from("ok"))])
    );
    assert_eq!(items[1], Value::Null);
    assert_internal(error(&response, 0), &["items", "1", "value"]);
}

#[test]
fn prepared_planned_resolver_panic_is_isolated() {
    let schema = Schema::builder()
        .object(
            ObjectType::new("Query")
                .field(
                    Field::new("boom", TypeRef::string())
                        .resolve(|_| Box::pin(async { panic!("planned payload") })),
                )
                .field(
                    Field::new("ok", TypeRef::string())
                        .resolve(|_| Box::pin(async { Ok(Value::from("fine")) })),
                ),
        )
        .query("Query")
        .build()
        .expect("valid schema");

    let prepared =
        PreparedQuery::new(&schema, "panic-planned", "{ boom ok }", None).expect("prepared query");
    let response = block_on(prepared.execute(None, &ExecutionOptions::default()));

    assert_eq!(
        response.data,
        Some(value([("boom", Value::Null), ("ok", Value::from("fine")),]))
    );
    assert_eq!(response.errors.len(), 1, "{response:?}");
    assert_internal(error(&response, 0), &["boom"]);
}

fn abstract_schema(interface: InterfaceType) -> Schema {
    Schema::builder()
        .object(
            ObjectType::new("User")
                .implements("Node")
                .field(Field::new("id", TypeRef::id().non_null())),
        )
        .interface(interface)
        .object(
            ObjectType::new("Query").field(
                Field::new("node", TypeRef::named("Node"))
                    .resolve(|_| Box::pin(async { Ok(value([("id", Value::from("1"))])) })),
            ),
        )
        .query("Query")
        .build()
        .expect("valid schema")
}

#[test]
fn abstract_resolve_type_panic_is_isolated() {
    let node = InterfaceType::new("Node")
        .field(Field::new("id", TypeRef::id().non_null()))
        .resolve_type(|_, _| panic!("resolve_type payload"));
    let schema = abstract_schema(node);

    let response = run(&schema, "{ node { id } }");

    assert_eq!(data_object(&response).get("node"), Some(&Value::Null));
    assert_internal(error(&response, 0), &["node"]);
}

#[test]
fn abstract_is_type_of_panic_is_isolated() {
    let node = InterfaceType::new("Node").field(Field::new("id", TypeRef::id().non_null()));
    let schema = Schema::builder()
        .object(
            ObjectType::new("User")
                .implements("Node")
                .is_type_of(|_| panic!("is_type_of payload"))
                .field(Field::new("id", TypeRef::id().non_null())),
        )
        .interface(node)
        .object(
            ObjectType::new("Query").field(
                Field::new("node", TypeRef::named("Node"))
                    .resolve(|_| Box::pin(async { Ok(value([("id", Value::from("1"))])) })),
            ),
        )
        .query("Query")
        .build()
        .expect("valid schema");

    let response = run(&schema, "{ node { id } }");

    assert_eq!(data_object(&response).get("node"), Some(&Value::Null));
    assert_internal(error(&response, 0), &["node"]);
}

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
fn subscription_setup_panic_returns_an_error_response() {
    let schema = subscription_schema(
        Field::new("tick", TypeRef::int().non_null())
            .subscribe(|_| Box::pin(async { panic!("setup resolver payload") })),
    );

    let response = block_on(gql::subscribe(
        &schema,
        Request::new("subscription { tick }"),
        &ExecutionOptions::default(),
    ))
    .expect_err("setup panic must not start a stream");

    assert_eq!(response.data, None);
    assert_eq!(response.errors.len(), 1, "{response:?}");
    assert_eq!(response.errors[0].message(), INTERNAL_MESSAGE);
    assert_eq!(code(&response.errors[0]), Some(INTERNAL_CODE));
    assert_eq!(response.errors[0].path(), None);
}

#[test]
fn prepared_subscription_setup_panic_returns_an_error_response() {
    let schema = subscription_schema(
        Field::new("tick", TypeRef::int().non_null())
            .subscribe(|_| Box::pin(async { panic!("prepared setup payload") })),
    );
    let prepared = PreparedQuery::new(&schema, "panic-subscription", "subscription { tick }", None)
        .expect("prepared subscription");

    let response = block_on(prepared.subscribe(None, &ExecutionOptions::default()))
        .expect_err("setup panic must not start a stream");

    assert_eq!(response.errors.len(), 1, "{response:?}");
    assert_eq!(response.errors[0].message(), INTERNAL_MESSAGE);
    assert_eq!(code(&response.errors[0]), Some(INTERNAL_CODE));
}

#[test]
fn subscription_event_resolver_panic_keeps_the_stream_running() {
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
            .resolve(|context| {
                Box::pin(async move {
                    if matches!(context.parent, Value::Int(2)) {
                        panic!("event resolver payload");
                    }
                    Ok(context.parent.clone())
                })
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
    assert_internal(error(&responses[1], 0), &["tick"]);
    assert_eq!(responses[2].data, Some(value([("tick", Value::Int(3))])));
}

#[test]
fn prepared_subscription_event_resolver_panic_keeps_the_stream_running() {
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
            .resolve(|context| {
                Box::pin(async move {
                    if matches!(context.parent, Value::Int(2)) {
                        panic!("prepared event payload");
                    }
                    Ok(context.parent.clone())
                })
            }),
    );
    let prepared = PreparedQuery::new(&schema, "panic-event", "subscription { tick }", None)
        .expect("prepared subscription");

    let subscription = block_on(prepared.subscribe(None, &ExecutionOptions::default()))
        .expect("subscription starts");
    let responses = block_on(subscription.stream.collect::<Vec<_>>());

    assert_eq!(responses.len(), 2, "the panic must not end the stream");
    assert_eq!(responses[0].data, Some(value([("tick", Value::Int(1))])));
    assert_eq!(responses[1].data, Some(Value::Null));
    assert_internal(error(&responses[1], 0), &["tick"]);
}

#[test]
fn panic_payload_shapes_are_discarded() {
    let schema = Schema::builder()
        .object(
            ObjectType::new("Query")
                .field(
                    Field::new("str_payload", TypeRef::string())
                        .resolve(|_| Box::pin(async { panic!("borrowed payload") })),
                )
                .field(
                    Field::new("string_payload", TypeRef::string()).resolve(|_| {
                        Box::pin(async { panic!("{}", String::from("owned payload")) })
                    }),
                )
                .field(
                    Field::new("other_payload", TypeRef::string())
                        .resolve(|_| Box::pin(async { std::panic::panic_any(42_u32) })),
                ),
        )
        .query("Query")
        .build()
        .expect("valid schema");

    let response = run(&schema, "{ str_payload string_payload other_payload }");

    assert_eq!(response.errors.len(), 3, "{response:?}");
    for (index, path) in ["str_payload", "string_payload", "other_payload"]
        .into_iter()
        .enumerate()
    {
        assert_internal(error(&response, index), &[path]);
        let message = response.errors[index].message();
        assert!(
            !message.contains("payload") && !message.contains("42"),
            "{message}"
        );
    }
}

#[test]
fn resolver_panicking_before_returning_a_future_is_isolated() {
    let schema =
        Schema::builder()
            .object(ObjectType::new("Query").field(
                Field::new("sync_boom", TypeRef::string()).resolve(|_| panic!("sync payload")),
            ))
            .query("Query")
            .build()
            .expect("valid schema");

    let response = run(&schema, "{ sync_boom }");

    assert_eq!(data_object(&response).get("sync_boom"), Some(&Value::Null));
    assert_internal(error(&response, 0), &["sync_boom"]);
}

#[test]
fn sync_leaf_fast_path_still_completes_beside_a_panicking_resolver() {
    let schema = Schema::builder()
        .object(
            ObjectType::new("Query")
                // No resolver, no arguments, scalar type: completed by the executor's inline
                // synchronous leaf path.
                .field(Field::new("fast", TypeRef::string().non_null()))
                .field(
                    Field::new("boom", TypeRef::string())
                        .resolve(|_| Box::pin(async { panic!("fast path sibling payload") })),
                ),
        )
        .query("Query")
        .build()
        .expect("valid schema");
    let options = ExecutionOptions::default().root_value(value([("fast", Value::from("leaf"))]));

    let response = block_on(gql::execute(
        &schema,
        Request::new("{ fast boom }"),
        &options,
    ));

    assert_eq!(
        response.data,
        Some(value([
            ("fast", Value::from("leaf")),
            ("boom", Value::Null),
        ]))
    );
    assert_eq!(response.errors.len(), 1, "{response:?}");
    assert_internal(error(&response, 0), &["boom"]);
}
