//! Resolver selection context: field nodes, operation, fragments, root value and variables.
//!
//! These tests execute real documents and read the information back out of
//! [`gql::ResolveInfo`]. They are deliberately end-to-end: the interesting failure mode is a
//! resolver seeing stale, empty or dangling execution context, which only showing the response
//! can catch.

use futures::StreamExt;
use gql::{
    ExecutionOptions, Field, GraphQLError, Object, ObjectType, PreparedQuery, Request, ResolveInfo,
    Schema, SubscriptionStream, TypeRef, Value, execute, execute_prepared, subscribe,
};

fn schema_with_query(query: ObjectType) -> Schema {
    let mut builder = Schema::builder();
    builder.add_type(query.into());
    builder.query("Query").build().expect("valid schema")
}

fn string_field_resolver(
    resolve: impl for<'a> Fn(&'a ResolveInfo<'a>) -> String + Send + Sync + 'static,
) -> Field {
    Field::new("echo", TypeRef::string()).resolve(move |ctx| {
        let value = resolve(ctx.info);
        Box::pin(async move { Ok(Value::from(value)) })
    })
}

fn response_string(response: &gql::Response, key: &str) -> String {
    response
        .data
        .as_ref()
        .and_then(Value::as_object)
        .and_then(|data| data.get(key))
        .and_then(Value::as_str)
        .unwrap_or_else(|| panic!("missing string response field {key}: {response:?}"))
        .to_owned()
}

#[test]
fn merged_field_nodes_keep_aliases_and_document_order() {
    let query = ObjectType::new("Query").field(string_field_resolver(|info| {
        info.field_nodes
            .iter()
            .map(|node| format!("{}:{}", node.response_key(), node.name))
            .collect::<Vec<_>>()
            .join(",")
    }));
    let schema = schema_with_query(query);
    let response = futures::executor::block_on(execute(
        &schema,
        Request::new("{ merged: echo merged: echo other: echo }"),
        &ExecutionOptions::default(),
    ));

    assert!(response.errors.is_empty(), "{response:?}");
    assert_eq!(
        response_string(&response, "merged"),
        "merged:echo,merged:echo"
    );
    assert_eq!(response_string(&response, "other"), "other:echo");
}

#[test]
fn field_nodes_from_fragments_are_merged() {
    let query = ObjectType::new("Query").field(string_field_resolver(|info| {
        let direct = info
            .field_nodes
            .iter()
            .filter(|node| node.selection_set.is_none())
            .count();
        format!(
            "{}|{}|{}|{:?}",
            info.field_nodes.len(),
            direct,
            info.fragments.contains("RootFields"),
            info.field_nodes.as_slice(),
        )
    }));
    let schema = schema_with_query(query);
    let response = futures::executor::block_on(execute(
        &schema,
        Request::new("query Q { echo ...RootFields } fragment RootFields on Query { echo }"),
        &ExecutionOptions::default(),
    ));

    assert!(response.errors.is_empty(), "{response:?}");
    let value = response_string(&response, "echo");
    assert!(value.starts_with("2|2|true|"), "{value}");
    assert!(value.contains("echo"), "{value}");
}

#[test]
fn operation_name_fragments_and_variables_are_visible() {
    let lookup = Field::new("lookup", TypeRef::string())
        .arg(gql::InputValue::new("id", TypeRef::named("ID").non_null()))
        .resolve(|ctx| {
            let operation = ctx
                .info
                .operation
                .and_then(|operation| operation.name)
                .unwrap_or("<anonymous>");
            let fragments = ctx.info.fragments.len();
            let variable = ctx
                .info
                .variables
                .get("id")
                .and_then(Value::as_str)
                .unwrap_or("<missing>");
            let value = format!("{operation}|{fragments}|{variable}");
            Box::pin(async move { Ok(Value::from(value)) })
        });
    let schema = schema_with_query(ObjectType::new("Query").field(lookup));

    let variables = Object::from_fields([("id", Value::from("42"))]);
    let request = Request::new(
        "query Fetch($id: ID!) { ...RootFields } \
         fragment RootFields on Query { lookup(id: $id) }",
    )
    .variables(&variables);
    let response =
        futures::executor::block_on(execute(&schema, request, &ExecutionOptions::default()));

    assert!(response.errors.is_empty(), "{response:?}");
    assert_eq!(response_string(&response, "lookup"), "Fetch|1|42");
}

#[test]
fn root_value_is_visible_for_normal_execution() {
    let query = ObjectType::new("Query").field(string_field_resolver(|info| {
        info.root_value
            .as_object()
            .and_then(|root| root.get("token"))
            .and_then(Value::as_str)
            .unwrap_or("<missing>")
            .to_owned()
    }));
    let schema = schema_with_query(query);
    let root = Value::from(Object::from_fields([("token", Value::from("abc"))]));
    let response = futures::executor::block_on(execute(
        &schema,
        Request::new("{ echo }"),
        &ExecutionOptions::default().root_value(root),
    ));

    assert!(response.errors.is_empty(), "{response:?}");
    assert_eq!(response_string(&response, "echo"), "abc");
}

#[test]
fn prepared_execution_populates_the_same_context() {
    let query = ObjectType::new("Query").field(string_field_resolver(|info| {
        let nodes = info
            .field_nodes
            .iter()
            .map(|node| node.response_key().to_owned())
            .collect::<Vec<_>>()
            .join(",");
        let operation = info
            .operation
            .and_then(|operation| operation.name)
            .unwrap_or("<anonymous>")
            .to_owned();
        let token = info
            .root_value
            .as_object()
            .and_then(|root| root.get("token"))
            .and_then(Value::as_str)
            .unwrap_or("<missing>")
            .to_owned();
        format!("{operation}|{nodes}|{token}")
    }));
    let schema = schema_with_query(query);
    let prepared = PreparedQuery::new(
        &schema,
        "context",
        "query Q { merged: echo merged: echo }",
        None,
    )
    .expect("prepared query");
    let root = Value::from(Object::from_fields([("token", Value::from("prepared"))]));

    let response = futures::executor::block_on(execute_prepared(
        &prepared,
        None,
        &ExecutionOptions::default().root_value(root),
    ));

    assert!(response.errors.is_empty(), "{response:?}");
    assert_eq!(
        response_string(&response, "merged"),
        "Q|merged,merged|prepared"
    );
}

#[test]
fn list_item_resolvers_receive_the_operation_context() {
    let item =
        ObjectType::new("Item").field(Field::new("value", TypeRef::string()).resolve(|ctx| {
            let operation = ctx
                .info
                .operation
                .and_then(|operation| operation.name)
                .unwrap_or("<anonymous>");
            let nodes = ctx.info.field_nodes.len();
            let value = format!("{operation}:{nodes}");
            Box::pin(async move { Ok(Value::from(value)) })
        }));
    let query = ObjectType::new("Query").field(
        Field::new("items", TypeRef::named("Item").non_null().list().non_null()).resolve(|_| {
            Box::pin(async {
                let items: Vec<Value> = (0..2).map(|_| Value::from(Object::new())).collect();
                Ok(Value::List(items))
            })
        }),
    );

    let mut builder = Schema::builder();
    builder.add_type(query.into());
    builder.add_type(item.into());
    let schema = builder.query("Query").build().expect("valid schema");

    let response = futures::executor::block_on(execute(
        &schema,
        Request::new("query Items { items { value value } }"),
        &ExecutionOptions::default(),
    ));

    assert!(response.errors.is_empty(), "{response:?}");
    let items = response
        .data
        .as_ref()
        .and_then(Value::as_object)
        .and_then(|data| data.get("items"))
        .and_then(Value::as_list)
        .expect("items list");
    assert_eq!(items.len(), 2);
    for item in items {
        let value = item
            .as_object()
            .and_then(|item| item.get("value"))
            .and_then(Value::as_str)
            .expect("item value");
        // Two selections share one response key, so the merged node list has two entries.
        assert_eq!(value, "Items:2");
    }
}

#[test]
fn subscription_setup_and_events_receive_context() {
    let tick = Field::new("tick", TypeRef::string())
        .arg(gql::InputValue::new("id", TypeRef::named("ID").non_null()))
        .subscribe(|ctx| {
            let operation = ctx
                .info
                .operation
                .and_then(|operation| operation.name)
                .unwrap_or("<anonymous>")
                .to_owned();
            let fragments = ctx.info.fragments.contains("TickFields");
            let variable = ctx
                .info
                .variables
                .get("id")
                .and_then(Value::as_str)
                .unwrap_or("<missing>")
                .to_owned();
            let root = ctx
                .info
                .root_value
                .as_object()
                .and_then(|root| root.get("token"))
                .and_then(Value::as_str)
                .unwrap_or("<missing>")
                .to_owned();
            let context = format!("{operation}|{fragments}|{variable}|{root}");
            Box::pin(async move {
                assert_eq!(context, "Watch|true|7|setup");
                let events: Vec<Result<Value, GraphQLError>> = vec![Ok(Value::from("event-root"))];
                let stream: SubscriptionStream = futures::stream::iter(events).boxed();
                Ok(stream)
            })
        })
        .resolve(|ctx| {
            let operation = ctx
                .info
                .operation
                .and_then(|operation| operation.name)
                .unwrap_or("<anonymous>");
            let fragments = ctx.info.fragments.contains("TickFields");
            let variable = ctx
                .info
                .variables
                .get("id")
                .and_then(Value::as_str)
                .unwrap_or("<missing>");
            let root_value = ctx.info.root_value.as_str().unwrap_or("<missing>");
            let value = format!("{operation}|{fragments}|{variable}|{root_value}");
            Box::pin(async move { Ok(Value::from(value)) })
        });
    let subscription = ObjectType::new("Subscription").field(tick);

    let mut builder = Schema::builder();
    builder.add_type(
        ObjectType::new("Query")
            .field(Field::new("noop", TypeRef::string()))
            .into(),
    );
    builder.add_type(subscription.into());
    let schema = builder
        .query("Query")
        .subscription("Subscription")
        .build()
        .expect("valid schema");

    let variables = Object::from_fields([("id", Value::from("7"))]);
    let root = Value::from(Object::from_fields([("token", Value::from("setup"))]));
    let response = futures::executor::block_on(subscribe(
        &schema,
        Request::new(
            "subscription Watch($id: ID!) { ...TickFields } \
             fragment TickFields on Subscription { tick(id: $id) }",
        )
        .variables(&variables),
        &ExecutionOptions::default().root_value(root),
    ))
    .expect("subscription starts");

    let events: Vec<_> = futures::executor::block_on(response.stream.collect::<Vec<_>>());
    assert_eq!(events.len(), 1, "{events:?}");
    assert!(events[0].errors.is_empty(), "{:?}", events[0]);
    assert_eq!(
        response_string(&events[0], "tick"),
        "Watch|true|7|event-root"
    );
}

#[test]
fn field_nodes_can_be_empty_on_a_manual_info() {
    let schema =
        schema_with_query(ObjectType::new("Query").field(Field::new("echo", TypeRef::string())));
    let ty = TypeRef::string();
    let info = ResolveInfo::new("echo", "Query", &ty, &[], &schema);
    assert!(info.field_nodes.is_empty());
    assert!(info.field_nodes.as_slice().is_empty());
    assert!(info.operation.is_none());
    assert!(info.fragments.is_empty());
    assert_eq!(info.root_value, &Value::Null);
    assert!(info.variables.is_empty());
}
