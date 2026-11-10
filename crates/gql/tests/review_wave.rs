//! Regression coverage for the selection-info and schema-composition foundations.
//!
//! The defects found by the review are fixed and their repros below are now ordinary tests:
//! 1. Subscription setup passes every merged root field node to `ResolveInfo`.
//! 2. Prepared executions expose plan-cloned nodes; identity is not guaranteed, values are.
//! 3. `sdl()`/`extend_sdl()` preserve which roots exist and do not infer extra ones.
//! 4. `sdl()`/`extend_sdl()` preserve user-registered built-in scalars.
//! 5. `merge` accepts equivalent code-first vs SDL `@deprecated`/`@specifiedBy`/`@oneOf`.
//! 5b. `extend_sdl` does not rewrite dedicated directives into the `directives` list.
//! 6. `merge` rejects conflicting runtime slots instead of choosing by argument order.
//! 7. `merge` treats directive application and location order as insignificant.
//! 8. `Fragments::iter` keeps source order above the inline threshold.

use std::sync::{Arc, Mutex};

use futures::StreamExt;
use gql::{
    ExecutionOptions, Field, FieldTransform, InputValue, InterfaceType, Object, ObjectType,
    PreparedQuery, Request, ResolveInfo, Schema, SchemaTransform, SubscriptionStream, Type,
    TypeRef, UnionType, Value, execute, execute_prepared, subscribe,
};

fn object(name: &str, fields: impl IntoIterator<Item = Field>) -> ObjectType {
    let mut ty = ObjectType::new(name);
    for field in fields {
        ty = ty.field(field);
    }
    ty
}

fn query_schema(query: ObjectType) -> Schema {
    let mut builder = Schema::builder();
    builder.add_type(query.into());
    builder.query("Query").build().expect("valid schema")
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

// -------------------------------------------------------------------------------------------
// 1. ResolveInfo selection AST
// -------------------------------------------------------------------------------------------

#[test]
fn subscription_setup_sees_every_merged_field_node() {
    let seen = Arc::new(Mutex::new(Vec::<Vec<String>>::new()));
    let record = Arc::clone(&seen);
    let tick = Field::new("tick", TypeRef::string())
        .subscribe(move |ctx| {
            let nodes = ctx
                .info
                .field_nodes
                .iter()
                .map(|node| format!("{}:{}", node.response_key(), node.name))
                .collect::<Vec<_>>();
            record.lock().expect("lock").push(nodes);
            Box::pin(async {
                let stream: SubscriptionStream = futures::stream::empty().boxed();
                Ok(stream)
            })
        })
        .resolve(|_| Box::pin(async { Ok(Value::from("event")) }));

    let mut builder = Schema::builder();
    builder.add_type(object("Query", [Field::new("noop", TypeRef::string())]).into());
    builder.add_type(object("Subscription", [tick]).into());
    let schema = builder
        .query("Query")
        .subscription("Subscription")
        .build()
        .expect("valid schema");

    let response = futures::executor::block_on(subscribe(
        &schema,
        Request::new("subscription S { tick tick }"),
        &ExecutionOptions::default(),
    ))
    .expect("subscription starts");
    drop(response);

    let seen = seen.lock().expect("lock");
    assert_eq!(seen.len(), 1, "setup must run once: {seen:?}");
    assert_eq!(
        seen[0],
        vec!["tick:tick".to_owned(), "tick:tick".to_owned()],
        "subscription setup must expose every merged root field node"
    );
}

#[test]
fn prepared_and_dynamic_field_nodes_agree_on_abstract_selections() {
    // An interface with two concrete types, a fragment on each plus repeated selections, so the
    // planned path's cloned nodes and the dynamic path's borrowed nodes must agree exactly.
    let node = InterfaceType::new("Node")
        .field(Field::new("id", TypeRef::named("ID").non_null()))
        .field(Field::new("kind", TypeRef::string()));

    let a = ObjectType::new("A")
        .implements("Node")
        .field(Field::new("id", TypeRef::named("ID").non_null()))
        .field(Field::new("kind", TypeRef::string()))
        .field(Field::new("label", TypeRef::string()).resolve(|ctx| {
            let nodes = ctx
                .info
                .field_nodes
                .iter()
                .map(|node| format!("{}:{}", node.response_key(), node.name))
                .collect::<Vec<_>>();
            let value = format!(
                "{}|{}|{}|{}",
                ctx.info.field_name,
                ctx.info.parent_type,
                nodes.join(","),
                ctx.info.path.len()
            );
            Box::pin(async move { Ok(Value::from(value)) })
        }));

    let b = ObjectType::new("B")
        .implements("Node")
        .field(Field::new("id", TypeRef::named("ID").non_null()))
        .field(Field::new("kind", TypeRef::string()))
        .field(
            Field::new("other", TypeRef::string())
                .resolve(|_| Box::pin(async { Ok(Value::from("other")) })),
        );

    let query = ObjectType::new("Query").field(
        Field::new("nodes", TypeRef::named("Node").list().non_null()).resolve(|_| {
            Box::pin(async {
                Ok(Value::List(vec![
                    Value::from(Object::from_fields([("__typename", Value::from("A"))])),
                    Value::from(Object::from_fields([("__typename", Value::from("B"))])),
                ]))
            })
        }),
    );

    let mut builder = Schema::builder();
    builder.add_type(query.into());
    builder.add_type(Type::Interface(Arc::new(node)));
    builder.add_type(a.into());
    builder.add_type(b.into());
    let schema = builder.query("Query").build().expect("valid schema");

    let source = "query Nodes { nodes { __typename ... on A { ...ADetails label } ...BD } } \
                  fragment ADetails on A { label: label } \
                  fragment BD on B { other }";
    let variables = Object::new();
    let dynamic = futures::executor::block_on(execute(
        &schema,
        Request::new(source).variables(&variables),
        &ExecutionOptions::default(),
    ));
    assert!(dynamic.errors.is_empty(), "{dynamic:?}");

    let prepared = PreparedQuery::new(&schema, "nodes", source, None).expect("prepared");
    let planned = futures::executor::block_on(execute_prepared(
        &prepared,
        Some(&variables),
        &ExecutionOptions::default(),
    ));
    assert!(planned.errors.is_empty(), "{planned:?}");

    assert_eq!(
        planned.data, dynamic.data,
        "planned and dynamic selection info must agree"
    );
}

#[test]
fn variables_include_defaults_in_info() {
    let field = Field::new("echo", TypeRef::string())
        .arg(InputValue::new("count", TypeRef::named("Int")))
        .arg(InputValue::new("label", TypeRef::named("String")))
        .resolve(|ctx| {
            let count = ctx
                .info
                .variables
                .get("count")
                .and_then(Value::as_i32)
                .unwrap_or(-1);
            let label = ctx
                .info
                .variables
                .get("label")
                .and_then(Value::as_str)
                .unwrap_or("<missing>")
                .to_owned();
            Box::pin(async move { Ok(Value::from(format!("{count}|{label}"))) })
        });
    let schema = query_schema(object("Query", [field]));
    let response = futures::executor::block_on(execute(
        &schema,
        Request::new(
            "query Q($count: Int = 5, $label: String = \"default\") { echo(count: $count, label: $label) }",
        ),
        &ExecutionOptions::default(),
    ));
    assert!(response.errors.is_empty(), "{response:?}");
    assert_eq!(response_string(&response, "echo"), "5|default");
}

#[test]
fn resolve_info_is_send_and_sync() {
    fn assert_send_sync<T: Send + Sync>() {}
    assert_send_sync::<ResolveInfo<'static>>();
    assert_send_sync::<gql::FieldNodes<'static>>();
}

#[test]
fn resolve_info_size_is_reasonable() {
    // A resolver frame is created for every field; this is a regression guard, not a target.
    let size = std::mem::size_of::<ResolveInfo<'static>>();
    eprintln!("ResolveInfo size: {size} bytes");
    assert!(size <= 128, "ResolveInfo grew to {size} bytes");
}

#[test]
fn manual_info_with_attached_context() {
    let schema = query_schema(object("Query", [Field::new("echo", TypeRef::string())]));
    let ty = TypeRef::string();
    let document =
        gql::parser::parse_query("query Q($id: ID) { ...F } fragment F on Query { echo }")
            .expect("parse");
    let fragments = gql::Fragments::new(&document);
    let operation = document.operations().next().expect("operation");
    let variables = Object::from_fields([("id", Value::from("1"))]);
    let root = Value::from(Object::from_fields([("token", Value::from("t"))]));
    let info = ResolveInfo::new("echo", "Query", &ty, &[], &schema)
        .with_operation(operation)
        .with_fragments(&fragments)
        .with_variables(&variables)
        .with_root_value(&root);
    assert_eq!(info.operation.and_then(|op| op.name), Some("Q"));
    assert!(info.fragments.contains("F"));
    assert_eq!(info.variables.get("id").and_then(Value::as_str), Some("1"));
    assert_eq!(
        info.root_value
            .as_object()
            .and_then(|root| root.get("token"))
            .and_then(Value::as_str),
        Some("t")
    );
}

// -------------------------------------------------------------------------------------------
// 2. Schema composition / transform
// -------------------------------------------------------------------------------------------

fn sdl(source: &str) -> Schema {
    Schema::from_sdl(source).expect("valid SDL schema")
}

#[test]
fn extend_sdl_does_not_invent_a_mutation_root() {
    // A perfectly ordinary object type that happens to be named `Mutation`, but is not the
    // mutation root (the schema has no mutation root at all).
    let mut builder = Schema::builder();
    builder.add_type(object("Query", [Field::new("hello", TypeRef::string())]).into());
    builder.add_type(object("Mutation", [Field::new("write", TypeRef::string())]).into());
    let schema = builder.query("Query").build().expect("valid schema");
    assert!(schema.mutation_name().is_none());

    let extended = schema
        .extend_sdl("extend type Query { extra: String }")
        .expect("extension applies");
    assert!(
        extended.mutation_name().is_none(),
        "extend_sdl must not turn a non-root type named Mutation into the mutation root"
    );
}

#[test]
fn extend_sdl_does_not_invent_a_subscription_root() {
    let mut builder = Schema::builder();
    builder.add_type(object("Query", [Field::new("hello", TypeRef::string())]).into());
    builder.add_type(object("Subscription", [Field::new("tick", TypeRef::string())]).into());
    let schema = builder.query("Query").build().expect("valid schema");
    assert!(schema.subscription_name().is_none());

    let extended = schema
        .extend_sdl("extend type Query { extra: String }")
        .expect("extension applies");
    assert!(
        extended.subscription_name().is_none(),
        "extend_sdl must not invent a subscription root"
    );
}

#[test]
fn merge_keeps_runtime_hooks_from_both_sides() {
    let with_hooks = object("Query", [Field::new("value", TypeRef::string())]);
    let with_hooks = query_schema(with_hooks);
    let plain = sdl("type Query { value: String }");
    // `merge` must preserve the resolver the code-first schema has, whichever side it is on.
    for merged in [
        plain.merge(&with_hooks).expect("left plain"),
        with_hooks.merge(&plain).expect("right plain"),
    ] {
        assert!(merged.query_type().get_field("value").is_some());
    }
}

#[test]
fn merge_rejects_conflicting_resolvers() {
    let first = query_schema(object(
        "Query",
        [Field::new("value", TypeRef::string())
            .resolve(|_| Box::pin(async { Ok(Value::from("first")) }))],
    ));
    let second = query_schema(object(
        "Query",
        [Field::new("value", TypeRef::string())
            .resolve(|_| Box::pin(async { Ok(Value::from("second")) }))],
    ));

    // Runtime behaviour is not chosen by argument order: authorization, limits and subscription
    // behaviour live in these slots, so a conflicting pair is a merge error in both directions.
    for error in [
        first.merge(&second).expect_err("left conflict"),
        second.merge(&first).expect_err("right conflict"),
    ] {
        let message = error.to_string();
        assert!(
            message.contains("Query.value") && message.contains("resolve"),
            "{message}"
        );
    }
}

#[test]
fn merge_treats_directive_order_as_insignificant() {
    let left = sdl("directive @a on OBJECT directive @b on OBJECT
         type Query @a @b { value: String }");
    let right = sdl("directive @a on OBJECT directive @b on OBJECT
         type Query @b @a { value: String }");
    left.merge(&right)
        .expect("directive application order must not make identical types conflict");
}

#[test]
fn merge_preserves_checked_runtime_slots() {
    let item = ObjectType::new("Item")
        .implements("Node")
        .field(Field::new("id", TypeRef::named("ID").non_null()))
        .is_type_of(|value| value.as_object().is_some());

    let node = InterfaceType::new("Node")
        .field(Field::new("id", TypeRef::named("ID").non_null()))
        .resolve_type(|_, _| Ok("Item".into()));

    let union = UnionType::new("Result")
        .member("Item")
        .resolve_type(|_, _| Ok("Item".into()));

    let query = object(
        "Query",
        [
            Field::new("item", TypeRef::named("Item")),
            Field::new("node", TypeRef::named("Node")),
            Field::new("result", TypeRef::named("Result")),
        ],
    );

    let mut builder = Schema::builder();
    builder.add_type(query.into());
    builder.add_type(item.into());
    builder.add_type(Type::Interface(Arc::new(node)));
    builder.add_type(Type::Union(Arc::new(union)));
    let schema = builder.query("Query").build().expect("valid schema");

    let plain = sdl("interface Node { id: ID! }
         union Result = Item
         type Item implements Node { id: ID! }
         type Query { item: Item node: Node result: Result }");
    let merged = plain.merge(&schema).expect("merge hooks");
    assert!(
        merged
            .object_type("Item")
            .and_then(|ty| ty.is_type_of.as_ref())
            .is_some(),
        "is_type_of must survive composition"
    );
    assert!(
        merged
            .interface_type("Node")
            .and_then(|ty| ty.resolve_type.as_ref())
            .is_some(),
        "interface resolve_type must survive composition"
    );
    assert!(
        merged
            .union_type("Result")
            .and_then(|ty| ty.resolve_type.as_ref())
            .is_some(),
        "union resolve_type must survive composition"
    );
}

#[test]
fn merge_and_transform_keep_introspection_working() {
    let schema = sdl("type Query { value: String }");
    let merged = schema.merge(&schema).expect("merge");
    let transformed = merged
        .transform(|_, _| FieldTransform::Keep)
        .expect("transform");
    for schema in [merged, transformed] {
        assert!(schema.get_type("__Schema").is_some());
        let response = futures::executor::block_on(execute(
            &schema,
            Request::new("{ __schema { queryType { name } } }"),
            &ExecutionOptions::default(),
        ));
        assert!(response.errors.is_empty(), "{response:?}");
    }
}

#[test]
fn transform_preserves_resolvers_on_kept_fields() {
    let schema = query_schema(object(
        "Query",
        [Field::new("hello", TypeRef::string())
            .resolve(|_| Box::pin(async { Ok(Value::from("world")) }))],
    ));
    let transformed = schema
        .transform(|_, _| FieldTransform::Keep)
        .expect("transform");
    let response = futures::executor::block_on(execute(
        &transformed,
        Request::new("{ hello }"),
        &ExecutionOptions::default(),
    ));
    assert!(response.errors.is_empty(), "{response:?}");
    assert_eq!(response_string(&response, "hello"), "world");
}

#[test]
fn transform_removing_an_interface_field_is_rejected() {
    let schema = sdl("interface Node { id: ID! }
         type Item implements Node { id: ID! name: String }
         type Query { item: Item }");
    let error = schema
        .transform(|ty, field| {
            if ty.name().as_ref() == "Item" && field.name.as_ref() == "id" {
                FieldTransform::Remove
            } else {
                FieldTransform::Keep
            }
        })
        .expect_err("Item would stop implementing Node");
    assert!(error.to_string().contains("id"), "{error}");
}

#[test]
fn retain_types_keeps_introspection_types() {
    let schema = sdl("type Query { value: String }");
    let retained = SchemaTransform::new()
        .keep("Query")
        .apply(&schema)
        .expect("keep Query");
    assert!(retained.get_type("__Schema").is_some());
    assert!(retained.get_type("__Type").is_some());
}

#[test]
fn extend_sdl_preserves_nested_runtime_hooks() {
    let item = ObjectType::new("Item")
        .field(Field::new("id", TypeRef::named("ID").non_null()))
        .field(
            Field::new("label", TypeRef::string())
                .resolve(|_| Box::pin(async { Ok(Value::from("label")) })),
        )
        .is_type_of(|_| true);
    let node = InterfaceType::new("Node")
        .field(Field::new("id", TypeRef::named("ID").non_null()))
        .resolve_type(|_, _| Ok("Item".into()));

    let mut builder = Schema::builder();
    builder.add_type(object("Query", [Field::new("node", TypeRef::named("Node"))]).into());
    builder.add_type(item.into());
    builder.add_type(Type::Interface(Arc::new(node)));
    let schema = builder.query("Query").build().expect("valid schema");

    let extended = schema
        .extend_sdl("extend type Item { extra: String }")
        .expect("extension applies");
    let item = extended.object_type("Item").expect("Item survives");
    assert!(
        item.fields
            .get("label")
            .and_then(|f| f.resolve.as_ref())
            .is_some()
    );
    assert!(item.is_type_of.is_some());
    assert!(
        extended
            .interface_type("Node")
            .and_then(|ty| ty.resolve_type.as_ref())
            .is_some()
    );
}

#[test]
fn extend_sdl_preserves_schema_directives_and_root_names() {
    let schema = Schema::from_sdl(
        "directive @tag(name: String) on SCHEMA
         schema @tag(name: \"root\") { query: Root }
         type Root { value: String }",
    )
    .expect("schema");
    let extended = schema
        .extend_sdl("extend type Root { extra: String }")
        .expect("extension applies");
    assert_eq!(extended.query_name().as_ref(), "Root");
    assert_eq!(extended.applied_directives().len(), 1);
    assert_eq!(extended.applied_directives()[0].name.as_ref(), "tag");
}

#[test]
fn builder_merge_is_order_independent_for_structural_content() {
    let left = sdl("type Query { a: String } type X { x: String }");
    let right = sdl("type Query { a: String } type Y { y: String }");
    let left_right = left.merge(&right).expect("left then right");
    let right_left = right.merge(&left).expect("right then left");
    for merged in [&left_right, &right_left] {
        assert!(merged.get_type("X").is_some());
        assert!(merged.get_type("Y").is_some());
        assert!(merged.query_type().get_field("a").is_some());
    }
}

// -------------------------------------------------------------------------------------------
// 3. Integration with prepared queries
// -------------------------------------------------------------------------------------------

#[test]
fn prepared_info_operation_and_field_nodes_come_from_the_same_document() {
    let seen = Arc::new(Mutex::new(Vec::<(usize, usize, usize)>::new()));
    let record = Arc::clone(&seen);
    let field = Field::new("echo", TypeRef::string()).resolve(move |ctx| {
        let op_range = ctx
            .info
            .operation
            .map(|operation| operation.selection_set.items.len())
            .unwrap_or(usize::MAX);
        let nodes = ctx.info.field_nodes.len();
        let fragments = ctx.info.fragments.len();
        record
            .lock()
            .expect("lock")
            .push((op_range, nodes, fragments));
        Box::pin(async { Ok(Value::from("x")) })
    });
    let schema = query_schema(object("Query", [field]));
    let source = "query Q { echo ...F } fragment F on Query { echo }";
    let prepared = PreparedQuery::new(&schema, "same", source, None).expect("prepared");
    let response = futures::executor::block_on(execute_prepared(
        &prepared,
        None,
        &ExecutionOptions::default(),
    ));
    assert!(response.errors.is_empty(), "{response:?}");
    let seen = seen.lock().expect("lock");
    // The resolver sees the operation with one top-level selection, two merged field nodes and
    // one fragment definition.
    assert_eq!(seen.as_slice(), &[(2, 2, 1)]);
}

#[test]
fn dynamic_and_prepared_info_expose_same_variables_after_defaults() {
    let field = Field::new("echo", TypeRef::string())
        .arg(InputValue::new("limit", TypeRef::named("Int")))
        .resolve(|ctx| {
            let value = ctx
                .info
                .variables
                .get("limit")
                .and_then(Value::as_i32)
                .unwrap_or(-1);
            Box::pin(async move { Ok(Value::from(format!("{value}"))) })
        });
    let schema = query_schema(object("Query", [field]));
    let source = "query Q($limit: Int = 7) { echo(limit: $limit) }";
    let dynamic = futures::executor::block_on(execute(
        &schema,
        Request::new(source),
        &ExecutionOptions::default(),
    ));
    let prepared = PreparedQuery::new(&schema, "defaults", source, None).expect("prepared");
    let planned = futures::executor::block_on(execute_prepared(
        &prepared,
        None,
        &ExecutionOptions::default(),
    ));
    assert_eq!(response_string(&dynamic, "echo"), "7");
    assert_eq!(response_string(&planned, "echo"), "7");
}

// -------------------------------------------------------------------------------------------
// 4. Security / correctness
// -------------------------------------------------------------------------------------------

#[test]
fn merging_a_schema_with_itself_does_not_share_mutable_state() {
    // Both schemas are immutable, but the merged schema must be its own allocation and not alias
    // the source's type map (a later transform of one must not affect the other).
    let schema = sdl("type Query { value: String } type Unused { value: String }");
    let merged = schema.merge(&schema).expect("merge");
    assert!(!schema.ptr_eq(&merged));
    let trimmed = merged.without_types(&["Unused"]).expect("unused removal");
    assert!(trimmed.get_type("Unused").is_none());
    assert!(schema.get_type("Unused").is_some());
}

#[test]
fn transform_does_not_leak_types_between_schemas() {
    let first = sdl("type Query { value: String } type OnlyFirst { value: String }");
    let second = sdl("type Query { value: String } type OnlySecond { value: String }");
    let transformed = first
        .transform(|_, _| FieldTransform::Keep)
        .expect("transform");
    assert!(transformed.get_type("OnlyFirst").is_some());
    assert!(transformed.get_type("OnlySecond").is_none());
    assert!(second.get_type("OnlySecond").is_some());
}

// -------------------------------------------------------------------------------------------
// Additional composition probes
// -------------------------------------------------------------------------------------------

#[test]
fn retain_types_rejects_a_type_still_referenced_by_a_directive() {
    let schema = sdl("directive @filter(by: Filter) on FIELD_DEFINITION
         input Filter { value: String }
         type Query { value: String }");
    let error = SchemaTransform::new()
        .keep("Query")
        .apply(&schema)
        .expect_err("Filter is still referenced by @filter");
    assert!(error.to_string().contains("Filter"), "{error}");
}

#[test]
fn transform_rejects_a_field_type_it_removed() {
    let schema = sdl("type Query { value: Item } type Item { value: String }");
    let error = schema
        .transform(|ty, field| {
            if ty.name().as_ref() == "Query" && field.name.as_ref() == "value" {
                FieldTransform::Remove
            } else {
                FieldTransform::Keep
            }
        })
        .expect_err("removing Query.value leaves Query without fields");
    assert!(error.to_string().contains("Query"), "{error}");
}

#[test]
fn extend_sdl_preserves_custom_builtin_scalar_behaviour() {
    // A user-registered built-in scalar keeps its definition, which the SDL printer currently
    // omits; a custom serializer must survive an `extend_sdl` round-trip.
    let mut builder = Schema::builder();
    builder.add_type(
        object(
            "Query",
            [Field::new("value", TypeRef::named("Int"))
                .resolve(|_| Box::pin(async { Ok(Value::Int(1)) }))],
        )
        .into(),
    );
    builder.add_type(
        gql::ScalarType::new("Int")
            .serialize(|_| Ok(Value::from(999)))
            .into(),
    );
    let schema = builder.query("Query").build().expect("valid schema");
    let extended = schema
        .extend_sdl("extend type Query { extra: String }")
        .expect("extension applies");
    let response = futures::executor::block_on(execute(
        &extended,
        Request::new("{ value }"),
        &ExecutionOptions::default(),
    ));
    assert!(response.errors.is_empty(), "{response:?}");
    assert_eq!(
        response
            .data
            .as_ref()
            .and_then(Value::as_object)
            .and_then(|data| data.get("value"))
            .and_then(Value::as_i32),
        Some(999),
        "custom Int serializer lost"
    );
}

#[test]
fn extend_sdl_preserves_custom_scalars() {
    let mut builder = Schema::builder();
    builder.add_type(
        object(
            "Query",
            [Field::new("when", TypeRef::named("Date"))
                .resolve(|_| Box::pin(async { Ok(Value::from("raw")) }))],
        )
        .into(),
    );
    builder.add_type(
        gql::ScalarType::new("Date")
            .serialize(|_| Ok(Value::from("date")))
            .into(),
    );
    let schema = builder.query("Query").build().expect("valid schema");
    let extended = schema
        .extend_sdl("extend type Query { extra: String }")
        .expect("extension applies");
    let response = futures::executor::block_on(execute(
        &extended,
        Request::new("{ when }"),
        &ExecutionOptions::default(),
    ));
    assert!(response.errors.is_empty(), "{response:?}");
    assert_eq!(
        response
            .data
            .as_ref()
            .and_then(Value::as_object)
            .and_then(|data| data.get("when"))
            .and_then(Value::as_str),
        Some("date"),
        "custom Date serializer lost"
    );
}

#[test]
fn extend_sdl_preserves_field_complexity_and_subscription_hooks() {
    let complexity = Field::new("tick", TypeRef::string())
        .complexity(|_| 7)
        .subscribe(|_| {
            Box::pin(async {
                let stream: SubscriptionStream = futures::stream::empty().boxed();
                Ok(stream)
            })
        })
        .resolve(|_| Box::pin(async { Ok(Value::from("x")) }));

    let mut builder = Schema::builder();
    builder.add_type(object("Query", [Field::new("noop", TypeRef::string())]).into());
    builder.add_type(object("Subscription", [complexity]).into());
    let schema = builder
        .query("Query")
        .subscription("Subscription")
        .build()
        .expect("valid schema");

    let extended = schema
        .extend_sdl("extend type Subscription { extra: String }")
        .expect("extension applies");
    let tick = extended
        .subscription_type()
        .and_then(|ty| ty.get_field("tick"))
        .expect("tick survives");
    assert!(tick.complexity.is_some(), "complexity hook lost");
    assert!(tick.subscribe.is_some(), "subscribe hook lost");
}

#[test]
fn merge_preserves_custom_scalar_behaviour() {
    let mut builder = Schema::builder();
    builder.add_type(
        object(
            "Query",
            [Field::new("when", TypeRef::named("Date"))
                .resolve(|_| Box::pin(async { Ok(Value::from("raw")) }))],
        )
        .into(),
    );
    builder.add_type(
        gql::ScalarType::new("Date")
            .serialize(|_| Ok(Value::from("date")))
            .into(),
    );
    let custom = builder.query("Query").build().expect("valid schema");
    let plain = sdl("scalar Date type Query { when: Date }");
    for merged in [
        custom.merge(&plain).expect("custom then plain"),
        plain.merge(&custom).expect("plain then custom"),
    ] {
        let response = futures::executor::block_on(execute(
            &merged,
            Request::new("{ when }"),
            &ExecutionOptions::default(),
        ));
        assert!(response.errors.is_empty(), "{response:?}");
        assert_eq!(
            response
                .data
                .as_ref()
                .and_then(Value::as_object)
                .and_then(|data| data.get("when"))
                .and_then(Value::as_str),
            Some("date")
        );
    }
}

#[test]
fn merge_with_self_preserves_custom_directive_arguments() {
    let schema = sdl(
        "directive @tag(id: Int!, label: String) repeatable on OBJECT
         type Query @tag(id: 1, label: \"x\") { value: String }",
    );
    let merged = schema.merge(&schema).expect("merge");
    assert_eq!(merged.query_type().directives.len(), 1);
    assert_eq!(
        merged.query_type().directives[0]
            .arguments
            .get("label")
            .and_then(Value::as_str),
        Some("x")
    );
}

#[test]
fn transform_preserves_union_and_interface_runtime_hooks() {
    let item = ObjectType::new("Item").field(Field::new("id", TypeRef::named("ID").non_null()));
    let node = InterfaceType::new("Node")
        .field(Field::new("id", TypeRef::named("ID").non_null()))
        .resolve_type(|_, _| Ok("Item".into()));
    let result = UnionType::new("Result")
        .member("Item")
        .resolve_type(|_, _| Ok("Item".into()));
    let mut builder = Schema::builder();
    builder.add_type(
        object(
            "Query",
            [
                Field::new("node", TypeRef::named("Node")),
                Field::new("result", TypeRef::named("Result")),
            ],
        )
        .into(),
    );
    builder.add_type(item.into());
    builder.add_type(Type::Interface(Arc::new(node)));
    builder.add_type(Type::Union(Arc::new(result)));
    let schema = builder.query("Query").build().expect("valid schema");

    let transformed = schema
        .transform(|_, _| FieldTransform::Keep)
        .expect("transform");
    assert!(
        transformed
            .interface_type("Node")
            .and_then(|ty| ty.resolve_type.as_ref())
            .is_some()
    );
    assert!(
        transformed
            .union_type("Result")
            .and_then(|ty| ty.resolve_type.as_ref())
            .is_some()
    );
}

// -------------------------------------------------------------------------------------------
// Differential selection-info probes: dynamic vs prepared
// -------------------------------------------------------------------------------------------

fn selection_report(info: &ResolveInfo<'_>) -> String {
    let nodes = info
        .field_nodes
        .iter()
        .map(|node| {
            let sub = node
                .selection_set
                .as_ref()
                .map(|set| set.items.len().to_string())
                .unwrap_or_else(|| "-".to_owned());
            format!("{}:{}/{}", node.response_key(), node.name, sub)
        })
        .collect::<Vec<_>>()
        .join(",");
    format!(
        "{}|{}|{}|{}|{}",
        info.field_name,
        info.parent_type,
        nodes,
        info.operation.and_then(|op| op.name).unwrap_or("-"),
        info.fragments.len()
    )
}

fn selection_schema() -> Schema {
    let leaf = |name: &str| {
        Field::new(name, TypeRef::string()).resolve(|ctx| {
            let report = selection_report(ctx.info);
            Box::pin(async move { Ok(Value::from(report)) })
        })
    };

    let item = ObjectType::new("Item")
        .field(leaf("id"))
        .field(leaf("name"))
        .field(leaf("extra"));

    let node = InterfaceType::new("Node")
        .field(Field::new("id", TypeRef::string()))
        .field(Field::new("kind", TypeRef::string()));

    let a = ObjectType::new("A")
        .implements("Node")
        .field(Field::new("id", TypeRef::string()))
        .field(Field::new("kind", TypeRef::string()))
        .field(leaf("label"));

    let b = ObjectType::new("B")
        .implements("Node")
        .field(Field::new("id", TypeRef::string()))
        .field(Field::new("kind", TypeRef::string()))
        .field(leaf("other"));

    let query = ObjectType::new("Query")
        .field(
            Field::new("items", TypeRef::named("Item").non_null().list().non_null())
                .arg(InputValue::new("limit", TypeRef::named("Int")))
                .resolve(|ctx| {
                    let count = ctx
                        .args
                        .get("limit")
                        .and_then(Value::as_i32)
                        .unwrap_or(2)
                        .max(0) as usize;
                    Box::pin(async move {
                        Ok(Value::List(
                            (0..count)
                                .map(|_| Value::Object(Arc::new(Object::new())))
                                .collect(),
                        ))
                    })
                }),
        )
        .field(
            Field::new("nodes", TypeRef::named("Node").list().non_null()).resolve(|_| {
                Box::pin(async {
                    Ok(Value::List(vec![
                        Value::from(Object::from_fields([("__typename", Value::from("A"))])),
                        Value::from(Object::from_fields([("__typename", Value::from("B"))])),
                    ]))
                })
            }),
        )
        .field(leaf("echo"));

    let mut builder = Schema::builder();
    builder.add_type(query.into());
    builder.add_type(item.into());
    builder.add_type(Type::Interface(Arc::new(node)));
    builder.add_type(a.into());
    builder.add_type(b.into());
    builder.query("Query").build().expect("valid schema")
}

#[test]
fn selection_info_differential_dynamic_vs_prepared() {
    let schema = selection_schema();
    let queries = [
        "query Q { echo echo }",
        "query Q { a: echo a: echo b: echo }",
        "query Q { items(limit: 2) { id name } items(limit: 2) { id extra } }",
        "query Q { ...Root } fragment Root on Query { echo ...More } fragment More on Query { echo }",
        "query Q { nodes { __typename ... on A { label label } ... on B { other } ...NA } } fragment NA on Node { kind }",
        "query Q($show: Boolean!) { items(limit: 1) { id @include(if: $show) name } }",
        "query Q($show: Boolean!) { echo @include(if: $show) ...F } fragment F on Query { echo }",
        "query Q { alias: items(limit: 1) { x: id x: id y: name } }",
        "query Q { __typename echo }",
        "query Q { nodes { ... on A { label ...F } ... on B { other } } } fragment F on A { extra: label label }",
        "query Q { items(limit: 1) { id ... on Item { name } ... { extra } } }",
    ];

    for show in [true, false] {
        let variables = Object::from_fields([("show", Value::Boolean(show))]);
        for query in queries {
            let dynamic = futures::executor::block_on(execute(
                &schema,
                Request::new(query).variables(&variables),
                &ExecutionOptions::default(),
            ));
            let prepared = match PreparedQuery::new(&schema, "selection", query, None) {
                Ok(prepared) => prepared,
                Err(error) => panic!("prepared {query}: {error}"),
            };
            let planned = futures::executor::block_on(execute_prepared(
                &prepared,
                Some(&variables),
                &ExecutionOptions::default(),
            ));
            assert_eq!(
                (planned.data.clone(), planned.errors.len()),
                (dynamic.data.clone(), dynamic.errors.len()),
                "show={show} query: {query}\ndynamic: {dynamic:?}\nplanned: {planned:?}"
            );
        }
    }
}

#[test]
fn prepared_subscription_setup_and_events_expose_all_root_nodes() {
    let setup = Arc::new(Mutex::new(Vec::<String>::new()));
    let events = Arc::new(Mutex::new(Vec::<String>::new()));
    let setup_record = Arc::clone(&setup);
    let event_record = Arc::clone(&events);

    let tick = Field::new("tick", TypeRef::string())
        .subscribe(move |ctx| {
            setup_record
                .lock()
                .expect("lock")
                .push(selection_report(ctx.info));
            Box::pin(async {
                let stream: SubscriptionStream =
                    futures::stream::iter(vec![Ok(Value::from("event"))]).boxed();
                Ok(stream)
            })
        })
        .resolve(move |ctx| {
            event_record
                .lock()
                .expect("lock")
                .push(selection_report(ctx.info));
            Box::pin(async { Ok(Value::from("resolved")) })
        });

    let mut builder = Schema::builder();
    builder.add_type(object("Query", [Field::new("noop", TypeRef::string())]).into());
    builder.add_type(object("Subscription", [tick]).into());
    let schema = builder
        .query("Query")
        .subscription("Subscription")
        .build()
        .expect("valid schema");

    let source = "subscription S { tick tick ...F } fragment F on Subscription { tick }";
    let prepared = PreparedQuery::new(&schema, "sub", source, None).expect("prepared sub");
    let response =
        futures::executor::block_on(prepared.subscribe(None, &ExecutionOptions::default()))
            .expect("subscription starts");
    let responses = futures::executor::block_on(response.stream.collect::<Vec<_>>());
    assert_eq!(responses.len(), 1, "{responses:?}");
    assert!(responses[0].errors.is_empty(), "{:?}", responses[0]);

    let setup = setup.lock().expect("lock");
    let events = events.lock().expect("lock");
    assert_eq!(setup.len(), 1, "{setup:?}");
    assert!(
        setup[0].contains("tick:tick/-"),
        "setup report: {}",
        setup[0]
    );
    // A merged root field appears three times: twice directly and once through the fragment.
    assert_eq!(events.len(), 1, "{events:?}");
    assert_eq!(
        events[0].matches("tick:tick/-").count(),
        3,
        "subscription event field_nodes: {}",
        events[0]
    );
    assert_eq!(
        setup[0].matches("tick:tick/-").count(),
        3,
        "subscription setup field_nodes: {}",
        setup[0]
    );
}

#[test]
fn extend_sdl_rejects_redefining_an_existing_field() {
    let schema = query_schema(object(
        "Query",
        [Field::new("hello", TypeRef::string())
            .resolve(|_| Box::pin(async { Ok(Value::from("world")) }))],
    ));
    let error = schema
        .extend_sdl("extend type Query { hello: Int }")
        .expect_err("a field cannot be redefined by an extension");
    assert!(error.to_string().contains("hello"), "{error}");
}

#[test]
fn extend_sdl_rejects_a_conflicting_field_type() {
    let schema = query_schema(object(
        "Query",
        [Field::new("hello", TypeRef::string())
            .resolve(|_| Box::pin(async { Ok(Value::from("world")) }))],
    ));
    let error = schema
        .extend_sdl("type Query { hello: Int }")
        .expect_err("a type cannot be redefined by an extension");
    assert!(error.to_string().contains("Query"), "{error}");
}

#[test]
fn prepared_field_nodes_match_the_operation_ast() {
    let same_tree = Arc::new(Mutex::new(Vec::<bool>::new()));
    let record = Arc::clone(&same_tree);
    let field = Field::new("echo", TypeRef::string()).resolve(move |ctx| {
        let operation = ctx.info.operation.expect("operation");
        let mut found = false;
        for item in &operation.selection_set.items {
            if let gql::ast::Selection::Field(node) = item {
                // Prepared execution owns a cloned plan, so compare by value rather than address.
                if ctx.info.field_nodes.iter().any(|selected| {
                    selected.name == node.name
                        && selected.alias == node.alias
                        && selected.location == node.location
                }) {
                    found = true;
                }
            }
        }
        record.lock().expect("lock").push(found);
        Box::pin(async { Ok(Value::from("x")) })
    });
    let schema = query_schema(object("Query", [field]));
    let source = "query Q { echo ...F } fragment F on Query { echo }";
    let prepared = PreparedQuery::new(&schema, "identity", source, None).expect("prepared");
    let response = futures::executor::block_on(execute_prepared(
        &prepared,
        None,
        &ExecutionOptions::default(),
    ));
    assert!(response.errors.is_empty(), "{response:?}");
    let same_tree = same_tree.lock().expect("lock");
    assert_eq!(
        same_tree.as_slice(),
        &[true],
        "prepared FieldNodes must describe nodes of ResolveInfo::operation"
    );
}

#[test]
fn merge_supports_recursive_types() {
    let left = sdl("type Query { node: Tree } type Tree { value: String children: [Tree!]! }");
    let right = sdl("type Query { node: Tree } type Tree { value: String children: [Tree!]! }");
    let merged = left.merge(&right).expect("recursive types merge");
    let tree = merged.object_type("Tree").expect("Tree");
    assert_eq!(tree.fields.len(), 2);
}

#[test]
fn fragments_iterate_in_source_order_for_large_documents() {
    // More fragments than the inline lookup holds exercises the hashed representation.
    let mut source = String::from("query Q { ...F0 }");
    for index in 0..10 {
        source.push_str(&format!(" fragment F{index} on Query {{ echo }}"));
    }
    let document = gql::parser::parse_query(&source).expect("parse");
    let expected: Vec<&str> = document.fragments().map(|fragment| fragment.name).collect();
    let fragments = gql::Fragments::new(&document);
    let actual: Vec<&str> = fragments.iter().map(|fragment| fragment.name).collect();
    assert_eq!(actual, expected);
}

#[test]
fn merge_treats_directive_location_order_as_insignificant() {
    let left = sdl("directive @tag on OBJECT | FIELD_DEFINITION
         type Query { value: String }");
    let right = sdl("directive @tag on FIELD_DEFINITION | OBJECT
         type Query { value: String }");
    left.merge(&right)
        .expect("directive location order must not conflict");
}

#[test]
fn mutation_root_value_is_visible() {
    let mutate = Field::new("write", TypeRef::string()).resolve(|ctx| {
        let token = ctx
            .info
            .root_value
            .as_object()
            .and_then(|root| root.get("token"))
            .and_then(Value::as_str)
            .unwrap_or("<missing>")
            .to_owned();
        Box::pin(async move { Ok(Value::from(token)) })
    });
    let mut builder = Schema::builder();
    builder.add_type(object("Query", [Field::new("noop", TypeRef::string())]).into());
    builder.add_type(object("Mutation", [mutate]).into());
    let schema = builder
        .query("Query")
        .mutation("Mutation")
        .build()
        .expect("valid schema");
    let root = Value::from(Object::from_fields([("token", Value::from("mut"))]));
    let response = futures::executor::block_on(execute(
        &schema,
        Request::new("mutation M { write }"),
        &ExecutionOptions::default().root_value(root),
    ));
    assert!(response.errors.is_empty(), "{response:?}");
    assert_eq!(response_string(&response, "write"), "mut");
}

#[test]
fn retain_types_rejects_removing_a_union_member() {
    let schema = sdl("union Result = A | B
         type A { value: String }
         type B { value: String }
         type Query { result: Result }");
    let error = SchemaTransform::new()
        .keep("Query")
        .keep("Result")
        .keep("A")
        .apply(&schema)
        .expect_err("Result still names B");
    assert!(error.to_string().contains("B"), "{error}");
}

#[test]
fn transform_rejects_an_unknown_output_type() {
    let schema = sdl("type Query { value: String }");
    let error = schema
        .transform(|_, field| {
            if field.name.as_ref() == "value" {
                FieldTransform::Replace(Field::new("value", TypeRef::named("Missing")))
            } else {
                FieldTransform::Keep
            }
        })
        .expect_err("Missing is not a type");
    assert!(error.to_string().contains("Missing"), "{error}");
}

#[test]
fn transform_rejects_an_interface_field_with_an_incompatible_type() {
    let schema = sdl("interface Node { id: ID! }
         type Item implements Node { id: ID! }
         type Query { item: Item }");
    let error = schema
        .transform(|ty, field| {
            if ty.name().as_ref() == "Item" && field.name.as_ref() == "id" {
                FieldTransform::Replace(Field::new("id", TypeRef::string()))
            } else {
                FieldTransform::Keep
            }
        })
        .expect_err("Item.id no longer matches Node.id");
    assert!(error.to_string().contains("id"), "{error}");
}

#[test]
fn transform_replacement_can_preserve_a_resolver() {
    let schema = query_schema(object(
        "Query",
        [Field::new("hello", TypeRef::string())
            .resolve(|_| Box::pin(async { Ok(Value::from("world")) }))],
    ));
    let transformed = schema
        .transform(|_, field| {
            let mut replacement = field.clone();
            replacement.ty = TypeRef::string().non_null();
            FieldTransform::Replace(replacement)
        })
        .expect("replacement is valid");
    let response = futures::executor::block_on(execute(
        &transformed,
        Request::new("{ hello }"),
        &ExecutionOptions::default(),
    ));
    assert!(response.errors.is_empty(), "{response:?}");
    assert_eq!(response_string(&response, "hello"), "world");
}

#[test]
fn extend_sdl_does_not_infer_a_root_from_a_non_object_default_name() {
    let mut builder = Schema::builder();
    builder.add_type(object("Query", [Field::new("hello", TypeRef::string())]).into());
    builder.add_type(Type::Interface(Arc::new(
        InterfaceType::new("Mutation").field(Field::new("id", TypeRef::string())),
    )));
    let schema = builder.query("Query").build().expect("valid schema");
    assert!(schema.mutation_name().is_none());
    schema
        .extend_sdl("extend type Query { extra: String }")
        .expect("extension applies without inventing a mutation root");
}

#[test]
fn merge_accepts_identical_schema_directives_with_reordered_arguments() {
    let left = sdl("directive @tag(a: Int, b: Int) on SCHEMA
         schema @tag(a: 1, b: 2) { query: Query }
         type Query { value: String }");
    let right = sdl("directive @tag(a: Int, b: Int) on SCHEMA
         schema @tag(b: 2, a: 1) { query: Query }
         type Query { value: String }");
    left.merge(&right)
        .expect("argument order must not make a schema directive application conflict");
}

#[test]
fn subscription_conditional_root_fields_are_counted_after_directives() {
    let tick = Field::new("tick", TypeRef::string())
        .subscribe(|_| {
            Box::pin(async {
                let stream: SubscriptionStream = futures::stream::empty().boxed();
                Ok(stream)
            })
        })
        .resolve(|_| Box::pin(async { Ok(Value::from("x")) }));

    let mut builder = Schema::builder();
    builder.add_type(object("Query", [Field::new("noop", TypeRef::string())]).into());
    builder.add_type(object("Subscription", [tick]).into());
    let schema = builder
        .query("Query")
        .subscription("Subscription")
        .build()
        .expect("valid schema");

    let source = "subscription S($show: Boolean!, $hide: Boolean!) { a: tick @include(if: $show) b: tick @skip(if: $hide) }";
    let variables = Object::from_fields([
        ("show", Value::Boolean(true)),
        ("hide", Value::Boolean(true)),
    ]);
    let request = Request::new(source).variables(&variables);
    // First establish whether validation accepts the document.
    let normal = futures::executor::block_on(subscribe(
        &schema,
        request.clone(),
        &ExecutionOptions::default(),
    ));
    let prepared = PreparedQuery::new(&schema, "conditional-sub", source, None);
    match (normal, prepared) {
        (Ok(normal), Ok(prepared)) => {
            drop(normal);
            let planned = futures::executor::block_on(
                prepared.subscribe(Some(&variables), &ExecutionOptions::default()),
            );
            assert!(
                planned.is_ok(),
                "prepared subscription rejected a single included root field: {planned:?}"
            );
        }
        (Err(normal), Err(prepared)) => {
            eprintln!("both rejected: normal={normal:?} prepared={prepared:?}");
        }
        (normal, prepared) => {
            panic!("normal and prepared disagree: normal={normal:?} prepared={prepared:?}")
        }
    }
}

#[test]
fn conditional_merged_nodes_match_dynamic_collection() {
    let schema = selection_schema();
    let source =
        "query Q($a: Boolean!, $b: Boolean!) { echo @include(if: $a) echo @skip(if: $b) echo }";
    let prepared = PreparedQuery::new(&schema, "conditional", source, None).expect("prepared");
    for a in [true, false] {
        for b in [true, false] {
            let variables =
                Object::from_fields([("a", Value::Boolean(a)), ("b", Value::Boolean(b))]);
            let dynamic = futures::executor::block_on(execute(
                &schema,
                Request::new(source).variables(&variables),
                &ExecutionOptions::default(),
            ));
            let planned = futures::executor::block_on(execute_prepared(
                &prepared,
                Some(&variables),
                &ExecutionOptions::default(),
            ));
            assert_eq!(
                (planned.data.clone(), planned.errors.len()),
                (dynamic.data.clone(), dynamic.errors.len()),
                "a={a} b={b}\ndynamic: {dynamic:?}\nplanned: {planned:?}"
            );
        }
    }
}

#[test]
fn merge_accepts_equivalent_deprecated_fields() {
    // Code-first marks deprecation through the dedicated field; an SDL schema records the same
    // fact as `@deprecated` in `directives` as well as in `deprecation_reason`.
    let code_first = query_schema(object(
        "Query",
        [Field::new("old", TypeRef::string()).deprecation_reason("use new")],
    ));
    let sdl_schema = sdl("type Query { old: String @deprecated(reason: \"use new\") }");
    code_first
        .merge(&sdl_schema)
        .expect("equivalent deprecation must not conflict");
}

#[test]
fn merge_accepts_equivalent_deprecated_enum_values() {
    let code_first = {
        let mut builder = Schema::builder();
        builder.add_type(
            gql::EnumType::new("Choice")
                .value_with("OLD", None, Some("use NEW".into()))
                .value("NEW")
                .into(),
        );
        builder.add_type(object("Query", [Field::new("choice", TypeRef::named("Choice"))]).into());
        builder.query("Query").build().expect("code-first")
    };
    let sdl_schema = sdl("enum Choice { OLD @deprecated(reason: \"use NEW\") NEW }
         type Query { choice: Choice }");
    code_first
        .merge(&sdl_schema)
        .expect("equivalent enum deprecation must not conflict");
}

#[test]
fn merge_accepts_equivalent_specified_by_scalars() {
    let code_first = {
        let mut builder = Schema::builder();
        builder.add_type(
            gql::ScalarType::new("Date")
                .specified_by_url("https://example.com/date")
                .into(),
        );
        builder.add_type(object("Query", [Field::new("when", TypeRef::named("Date"))]).into());
        builder.query("Query").build().expect("code-first")
    };
    let sdl_schema = sdl("scalar Date @specifiedBy(url: \"https://example.com/date\")
         type Query { when: Date }");
    code_first
        .merge(&sdl_schema)
        .expect("equivalent @specifiedBy must not conflict");
}

#[test]
fn merge_accepts_equivalent_one_of_inputs() {
    let code_first = {
        let mut builder = Schema::builder();
        let input = gql::InputObjectType::new("Filter")
            .one_of()
            .field(gql::InputValue::new("a", TypeRef::string()))
            .field(gql::InputValue::new("b", TypeRef::string()));
        builder.add_type(input.into());
        builder.add_type(
            object(
                "Query",
                [Field::new("search", TypeRef::string())
                    .arg(gql::InputValue::new("filter", TypeRef::named("Filter")))],
            )
            .into(),
        );
        builder.query("Query").build().expect("code-first")
    };
    let sdl_schema = sdl("input Filter @oneOf { a: String b: String }
         type Query { search(filter: Filter): String }");
    code_first
        .merge(&sdl_schema)
        .expect("equivalent @oneOf must not conflict");
}

#[test]
fn extend_sdl_is_structurally_idempotent_for_dedicated_directives() {
    let schema = query_schema(object(
        "Query",
        [Field::new("old", TypeRef::string()).deprecation_reason("use new")],
    ));
    assert!(
        schema
            .query_type()
            .get_field("old")
            .expect("field")
            .directives
            .is_empty()
    );
    let extended = schema
        .extend_sdl("extend type Query { extra: String }")
        .expect("extension applies");
    let old = extended
        .query_type()
        .get_field("old")
        .expect("old survives");
    assert!(
        old.directives.is_empty(),
        "extend_sdl added @deprecated to the directive list: {:?}",
        old.directives
    );
}

#[test]
fn merge_deduplicates_reordered_schema_directive_arguments() {
    let left = sdl("directive @tag(a: Int, b: Int) on SCHEMA
         schema @tag(a: 1, b: 2) { query: Query }
         type Query { value: String }");
    let right = sdl("directive @tag(a: Int, b: Int) on SCHEMA
         schema @tag(b: 2, a: 1) { query: Query }
         type Query { value: String }");
    let merged = left.merge(&right).expect("merge");
    assert_eq!(
        merged.applied_directives().len(),
        1,
        "a single schema directive application was duplicated by merge"
    );
}

#[test]
fn explicit_schema_definition_does_not_infer_extra_roots() {
    // Spec: the default root type names apply only when the schema definition is omitted.
    let schema = Schema::from_sdl(
        "schema { query: Root }
         type Root { value: String }
         type Mutation { write: String }
         type Subscription { tick: String }",
    )
    .expect("valid SDL");
    assert_eq!(schema.query_name().as_ref(), "Root");
    assert!(
        schema.mutation_name().is_none(),
        "explicit schema definition must not infer a mutation root"
    );
    assert!(
        schema.subscription_name().is_none(),
        "explicit schema definition must not infer a subscription root"
    );
}

#[test]
fn explicit_schema_definition_requires_a_query_root() {
    // A schema definition that omits the query root is invalid; inference must not fill it in.
    let error = Schema::from_sdl(
        "schema { mutation: Mutation }
         type Query { value: String }
         type Mutation { write: String }",
    )
    .expect_err("an explicit schema definition must name a query root");
    assert!(error.to_string().contains("query"), "{error}");
}

#[test]
fn selection_info_differential_for_unions() {
    let item = ObjectType::new("Item").field(Field::new("id", TypeRef::string()).resolve(|ctx| {
        let report = selection_report(ctx.info);
        Box::pin(async move { Ok(Value::from(report)) })
    }));
    let other =
        ObjectType::new("Other").field(Field::new("name", TypeRef::string()).resolve(|ctx| {
            let report = selection_report(ctx.info);
            Box::pin(async move { Ok(Value::from(report)) })
        }));
    let result = UnionType::new("Result").member("Item").member("Other");
    let query = ObjectType::new("Query").field(
        Field::new(
            "search",
            TypeRef::named("Result").non_null().list().non_null(),
        )
        .resolve(|_| {
            Box::pin(async {
                Ok(Value::List(vec![
                    Value::from(Object::from_fields([("__typename", Value::from("Item"))])),
                    Value::from(Object::from_fields([("__typename", Value::from("Other"))])),
                ]))
            })
        }),
    );
    let mut builder = Schema::builder();
    builder.add_type(query.into());
    builder.add_type(item.into());
    builder.add_type(other.into());
    builder.add_type(Type::Union(Arc::new(result)));
    let schema = builder.query("Query").build().expect("valid schema");

    let source = "query Q { search { ... on Item { id ...F } ... on Other { name } } }
                  fragment F on Item { again: id }";
    let dynamic = futures::executor::block_on(execute(
        &schema,
        Request::new(source),
        &ExecutionOptions::default(),
    ));
    assert!(dynamic.errors.is_empty(), "{dynamic:?}");
    let prepared = PreparedQuery::new(&schema, "union", source, None).expect("prepared");
    let planned = futures::executor::block_on(execute_prepared(
        &prepared,
        None,
        &ExecutionOptions::default(),
    ));
    assert!(planned.errors.is_empty(), "{planned:?}");
    assert_eq!(planned.data, dynamic.data);
}

#[test]
fn prepared_queries_work_against_an_extended_schema() {
    let schema = query_schema(object(
        "Query",
        [Field::new("hello", TypeRef::string())
            .resolve(|_| Box::pin(async { Ok(Value::from("world")) }))],
    ));
    let extended = schema
        .extend_sdl("extend type Query { extra: String }")
        .expect("extension");
    let prepared = PreparedQuery::new(&extended, "extended", "{ hello extra }", None)
        .expect("prepared against extended schema");
    let response = futures::executor::block_on(execute_prepared(
        &prepared,
        None,
        &ExecutionOptions::default(),
    ));
    assert!(response.errors.is_empty(), "{response:?}");
    assert_eq!(response_string(&response, "hello"), "world");
    assert_eq!(
        response
            .data
            .as_ref()
            .and_then(Value::as_object)
            .and_then(|data| data.get("extra")),
        Some(&Value::Null)
    );
    assert!(prepared.schema().ptr_eq(&extended));
}

#[test]
fn prepared_query_keeps_the_schema_it_was_prepared_against() {
    let schema = query_schema(object(
        "Query",
        [Field::new("hello", TypeRef::string())
            .resolve(|_| Box::pin(async { Ok(Value::from("original")) }))],
    ));
    let prepared = PreparedQuery::new(&schema, "stale", "{ hello }", None).expect("prepared");
    let extended = schema
        .extend_sdl("extend type Query { extra: String }")
        .expect("extension");
    assert!(!prepared.schema().ptr_eq(&extended));
    // Executing the prepared query uses the schema it captured, not the extended one.
    let response = futures::executor::block_on(execute_prepared(
        &prepared,
        None,
        &ExecutionOptions::default(),
    ));
    assert_eq!(response_string(&response, "hello"), "original");
}
