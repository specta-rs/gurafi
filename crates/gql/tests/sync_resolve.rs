//! The synchronous property-resolver slot.
//!
//! A field with `sync_resolve` reads its value inline: no future is allocated, so it runs on
//! both the leaf fast path and the general completion path without a box. These tests pin the
//! observable behaviour of the slot itself; `gurafi`'s `expose!` tests cover the public macro.

use std::sync::{
    Arc,
    atomic::{AtomicUsize, Ordering},
};

use futures::executor::block_on;
use gql::{
    ExecutionOptions, Field, GraphQLError, Object, ObjectType, PropertyContext, Request, Response,
    Schema, TypeRef, Value,
};

fn run(schema: &Schema, query: &str) -> Response {
    block_on(gql::execute(
        schema,
        Request::new(query),
        &ExecutionOptions::default(),
    ))
}

fn value(response: &Response, field: &str) -> Value {
    response
        .data
        .as_ref()
        .and_then(Value::as_object)
        .and_then(|data| data.get(field))
        .cloned()
        .unwrap_or_else(|| panic!("missing `{field}` in {response:?}"))
}

#[derive(Debug)]
struct Model {
    name: String,
    child: Child,
}

#[derive(Debug)]
struct Child {
    value: i64,
}

fn read_model<'a, T: 'static>(
    context: PropertyContext<'a>,
    field: &'static str,
) -> Result<&'a T, GraphQLError> {
    context.parent.as_external::<T>().ok_or_else(|| {
        GraphQLError::new(format!(
            "expected `{}` for {}.{}",
            std::any::type_name::<T>(),
            context.parent_type,
            field
        ))
    })
}

fn model_schema(calls: Arc<AtomicUsize>) -> Schema {
    let item = ObjectType::new("Item")
        .field(
            Field::new("name", TypeRef::string().non_null()).sync_resolve({
                let calls = Arc::clone(&calls);
                move |context| {
                    calls.fetch_add(1, Ordering::Relaxed);
                    let model = read_model::<Model>(context, "name")?;
                    Ok(Value::from(model.name.clone()))
                }
            }),
        )
        .field(
            Field::new("child", TypeRef::named("Child").non_null()).sync_resolve({
                let calls = Arc::clone(&calls);
                move |context| {
                    calls.fetch_add(1, Ordering::Relaxed);
                    let model = read_model::<Model>(context, "child")?;
                    Ok(Value::external(Child {
                        value: model.child.value,
                    }))
                }
            }),
        );

    let child = ObjectType::new("Child").field(
        Field::new("value", TypeRef::int().non_null()).sync_resolve({
            let calls = Arc::clone(&calls);
            move |context| {
                calls.fetch_add(1, Ordering::Relaxed);
                let child = read_model::<Child>(context, "value")?;
                Ok(Value::Int(i32::try_from(child.value).expect("fits")))
            }
        }),
    );

    let query = ObjectType::new("Query").field(
        Field::new("item", TypeRef::named("Item").non_null()).resolve(|_| {
            Box::pin(async {
                Ok(Value::external(Model {
                    name: String::from("Melman"),
                    child: Child { value: 7 },
                }))
            })
        }),
    );

    let mut builder = Schema::builder();
    builder.add_type(query.into());
    builder.add_type(item.into());
    builder.add_type(child.into());
    builder.query("Query").build().expect("schema builds")
}

#[test]
fn sync_resolve_reads_models_and_nested_models() {
    let calls = Arc::new(AtomicUsize::new(0));
    let schema = model_schema(Arc::clone(&calls));
    let response = run(&schema, "{ item { name child { value } } }");

    assert!(
        response.errors.is_empty(),
        "unexpected errors: {response:?}"
    );
    let item = value(&response, "item");
    assert_eq!(
        item.as_object().unwrap().get("name"),
        Some(&Value::from("Melman"))
    );
    assert_eq!(calls.load(Ordering::Relaxed), 3);
}

#[test]
fn sync_resolve_runs_once_per_selected_field() {
    let calls = Arc::new(AtomicUsize::new(0));
    let schema = model_schema(Arc::clone(&calls));
    let response = run(&schema, "{ item { name name child { value } } }");

    assert!(
        response.errors.is_empty(),
        "unexpected errors: {response:?}"
    );
    assert_eq!(
        calls.load(Ordering::Relaxed),
        3,
        "merged selections must resolve each field once"
    );
}

#[test]
fn sync_resolve_errors_follow_normal_null_propagation() {
    // The parent is a raw object, not the model the hook expects, so every hook call fails.
    let schema = {
        let item = ObjectType::new("Item").field(
            Field::new("name", TypeRef::string()).sync_resolve(|context| {
                if context.parent.as_external::<Model>().is_none() {
                    return Err(GraphQLError::new("not a model"));
                }
                Ok(Value::from("unreachable"))
            }),
        );
        let query = ObjectType::new("Query").field(
            Field::new("item", TypeRef::named("Item").non_null())
                .resolve(|_| Box::pin(async { Ok(Value::Object(Arc::new(Object::new()))) })),
        );
        let mut builder = Schema::builder();
        builder.add_type(query.into());
        builder.add_type(item.into());
        builder.query("Query").build().expect("schema builds")
    };

    let response = run(&schema, "{ item { name } }");
    assert_eq!(
        value(&response, "item").as_object().unwrap().get("name"),
        Some(&Value::Null),
        "a nullable field must become null"
    );
    assert_eq!(response.errors.len(), 1, "unexpected errors: {response:?}");
}

#[test]
fn sync_resolve_takes_precedence_over_the_default_property_lookup() {
    let item = ObjectType::new("Item").field(
        Field::new("name", TypeRef::string().non_null())
            .sync_resolve(|_| Ok(Value::from("from the hook"))),
    );
    let query = ObjectType::new("Query").field(
        Field::new("item", TypeRef::named("Item").non_null()).resolve(|_| {
            Box::pin(async {
                Ok(Value::object(Object::from_fields([(
                    "name",
                    Value::from("from the object"),
                )])))
            })
        }),
    );
    let mut builder = Schema::builder();
    builder.add_type(query.into());
    builder.add_type(item.into());
    let schema = builder.query("Query").build().expect("schema builds");

    let response = run(&schema, "{ item { name } }");
    assert!(
        response.errors.is_empty(),
        "unexpected errors: {response:?}"
    );
    assert_eq!(
        value(&response, "item").as_object().unwrap().get("name"),
        Some(&Value::from("from the hook"))
    );
}

#[test]
fn an_async_resolver_wins_over_a_sync_resolver() {
    let item = ObjectType::new("Item").field(
        Field::new("name", TypeRef::string().non_null())
            .sync_resolve(|_| Ok(Value::from("sync")))
            .resolve(|_| Box::pin(async { Ok(Value::from("async")) })),
    );
    let query = ObjectType::new("Query").field(
        Field::new("item", TypeRef::named("Item").non_null())
            .resolve(|_| Box::pin(async { Ok(Value::Object(Arc::new(Object::new()))) })),
    );
    let mut builder = Schema::builder();
    builder.add_type(query.into());
    builder.add_type(item.into());
    let schema = builder.query("Query").build().expect("schema builds");

    let response = run(&schema, "{ item { name } }");
    assert!(
        response.errors.is_empty(),
        "unexpected errors: {response:?}"
    );
    assert_eq!(
        value(&response, "item").as_object().unwrap().get("name"),
        Some(&Value::from("async"))
    );
}

#[test]
fn sync_resolvers_complete_external_lists_without_a_resolver_on_the_type() {
    // Models in a list are completed through the synchronous subtree path, because none of the
    // selected fields needs a future.
    let calls = Arc::new(AtomicUsize::new(0));
    let item = ObjectType::new("Item").field(
        Field::new("name", TypeRef::string().non_null()).sync_resolve({
            let calls = Arc::clone(&calls);
            move |context| {
                calls.fetch_add(1, Ordering::Relaxed);
                let model = read_model::<Model>(context, "name")?;
                Ok(Value::from(model.name.clone()))
            }
        }),
    );
    let query = ObjectType::new("Query").field(
        Field::new("items", TypeRef::named("Item").non_null().list().non_null()).resolve(
            move |_| {
                Box::pin(async {
                    Ok(Value::List(vec![
                        Value::external(Model {
                            name: String::from("a"),
                            child: Child { value: 1 },
                        }),
                        Value::external(Model {
                            name: String::from("b"),
                            child: Child { value: 2 },
                        }),
                    ]))
                })
            },
        ),
    );
    let mut builder = Schema::builder();
    builder.add_type(query.into());
    builder.add_type(item.into());
    let schema = builder.query("Query").build().expect("schema builds");

    let response = run(&schema, "{ items { name } }");
    assert!(
        response.errors.is_empty(),
        "unexpected errors: {response:?}"
    );
    assert_eq!(calls.load(Ordering::Relaxed), 2);
    let items = value(&response, "items");
    assert_eq!(
        items.as_list().unwrap()[0].as_object().unwrap().get("name"),
        Some(&Value::from("a"))
    );
}

#[test]
fn take_resolve_materializes_the_sync_hook_and_clears_both_slots() {
    let mut field =
        Field::new("name", TypeRef::string()).sync_resolve(|_| Ok(Value::from("value")));
    assert!(field.has_resolver());
    assert!(field.resolve.is_none());
    assert!(field.sync_resolve.is_some());

    let _resolve = field.take_resolve().expect("materialized resolver");
    assert!(field.resolve.is_none());
    assert!(field.sync_resolve.is_none());
    assert!(!field.has_resolver());
    assert!(field.take_resolve().is_none());
}

#[test]
fn take_resolve_keeps_an_async_resolver_and_drops_the_sync_hook() {
    let mut field = Field::new("name", TypeRef::string())
        .sync_resolve(|_| Ok(Value::from("sync")))
        .resolve(|_| Box::pin(async { Ok(Value::from("async")) }));

    let _resolve = field.take_resolve().expect("async resolver");
    assert!(field.resolve.is_none());
    assert!(field.sync_resolve.is_none());
}
