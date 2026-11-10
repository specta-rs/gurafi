//! Behaviour of the executor's synchronous default-completion fast path.
//!
//! Objects whose fields all use the default property resolver are completed without boxing a
//! future per field. That fast path may abandon a partially completed subtree and restart through
//! the asynchronous path when it meets a resolver, an authorizer or a user-defined scalar, so
//! these tests pin the observable semantics: responses, error paths, null propagation, and the
//! guarantee that caller code runs exactly once.

use std::sync::{
    Arc,
    atomic::{AtomicUsize, Ordering},
};

use futures::executor::block_on;
use gql::{
    AuthorizationContext, Authorizer, ExecutionOptions, Field, GraphQLError, InputValue, Object,
    ObjectType, Request, Response, ScalarType, Schema, TypeRef, Value,
};

fn run(schema: &Schema, query: &str) -> Response {
    block_on(gql::execute(
        schema,
        Request::new(query),
        &ExecutionOptions::default(),
    ))
}

fn object(fields: impl IntoIterator<Item = (&'static str, Value)>) -> Value {
    Value::object(Object::from_fields(fields))
}

fn profile_schema() -> Schema {
    let user = ObjectType::new("User")
        .field(Field::new("id", TypeRef::id().non_null()))
        .field(Field::new("name", TypeRef::string().non_null()))
        .field(Field::new("age", TypeRef::int()))
        .field(Field::new("profile", TypeRef::named("Profile").non_null()))
        .field(Field::new(
            "friends",
            TypeRef::named("User").non_null().list().non_null(),
        ));

    let profile = ObjectType::new("Profile")
        .field(Field::new("bio", TypeRef::string()))
        .field(Field::new("score", TypeRef::int().non_null()));

    let root = ObjectType::new("Query")
        .field(
            Field::new("user", TypeRef::named("User").non_null()).resolve(|_| {
                Box::pin(async {
                    Ok(object([
                        ("id", Value::from("1")),
                        ("name", Value::from("Ada")),
                        ("age", Value::Int(36)),
                        (
                            "profile",
                            object([("bio", Value::from("engineer")), ("score", Value::Int(9))]),
                        ),
                        (
                            "friends",
                            Value::List(vec![
                                object([
                                    ("id", Value::from("2")),
                                    ("name", Value::from("Grace")),
                                    ("age", Value::Int(85)),
                                    (
                                        "profile",
                                        object([
                                            ("bio", Value::from("admiral")),
                                            ("score", Value::Int(10)),
                                        ]),
                                    ),
                                    ("friends", Value::List(Vec::new())),
                                ]),
                                object([
                                    ("id", Value::from("3")),
                                    ("name", Value::from("Alan")),
                                    ("age", Value::Int(41)),
                                    ("profile", object([("score", Value::Int(7))])),
                                    ("friends", Value::List(Vec::new())),
                                ]),
                            ]),
                        ),
                    ]))
                })
            }),
        )
        .field(
            Field::new("empty", TypeRef::named("User").non_null()).resolve(|_| {
                Box::pin(async {
                    Ok(object([
                        ("id", Value::from("0")),
                        ("name", Value::from("")),
                    ]))
                })
            }),
        );

    let mut builder = Schema::builder();
    builder.add_type(user.into());
    builder.add_type(profile.into());
    builder.add_type(root.into());
    builder.query("Query").build().expect("valid schema")
}

#[test]
fn default_resolved_objects_lists_aliases_and_fragments_complete() {
    let schema = profile_schema();
    let response = run(
        &schema,
        r#"
        query {
          user {
            ...UserFields
            profile { bio score }
            friends { id name profile { bio score } }
          }
          aliased: user { id }
        }

        fragment UserFields on User {
          id
          name
          age
        }
        "#,
    );

    assert!(response.errors.is_empty(), "{:?}", response.errors);
    let data = response
        .data
        .as_ref()
        .and_then(Value::as_object)
        .expect("data");
    let user = data.get("user").and_then(Value::as_object).expect("user");
    assert_eq!(user.get("id").and_then(Value::as_str), Some("1"));
    assert_eq!(user.get("name").and_then(Value::as_str), Some("Ada"));
    assert_eq!(user.get("age").and_then(Value::as_i32), Some(36));

    let profile = user
        .get("profile")
        .and_then(Value::as_object)
        .expect("profile");
    assert_eq!(profile.get("bio").and_then(Value::as_str), Some("engineer"));
    assert_eq!(profile.get("score").and_then(Value::as_i32), Some(9));

    let friends = user
        .get("friends")
        .and_then(Value::as_list)
        .expect("friends");
    assert_eq!(friends.len(), 2);
    let second = friends[1].as_object().expect("friend");
    assert_eq!(second.get("name").and_then(Value::as_str), Some("Alan"));
    // A missing nullable property is `null`, exactly as the default resolver produces.
    let second_profile = second
        .get("profile")
        .and_then(Value::as_object)
        .expect("profile");
    assert_eq!(second_profile.get("bio"), Some(&Value::Null));

    let aliased = data
        .get("aliased")
        .and_then(Value::as_object)
        .expect("aliased");
    assert_eq!(aliased.get("id").and_then(Value::as_str), Some("1"));
    // Fields are emitted in the order the document selects them.
    let keys: Vec<&str> = user.keys().map(AsRef::as_ref).collect();
    assert_eq!(keys, ["id", "name", "age", "profile", "friends"]);
}

#[test]
fn missing_non_null_property_reports_the_nested_path_and_nulls_the_parent() {
    let child = ObjectType::new("Child")
        .field(Field::new("id", TypeRef::id().non_null()))
        .field(Field::new("score", TypeRef::int().non_null()));

    let parent = ObjectType::new("Parent")
        .field(Field::new("id", TypeRef::id().non_null()))
        .field(Field::new("child", TypeRef::named("Child").non_null()));

    let root = ObjectType::new("Query").field(
        Field::new(
            "parents",
            TypeRef::named("Parent").non_null().list().non_null(),
        )
        .resolve(|_| {
            Box::pin(async {
                Ok(Value::List(vec![
                    object([
                        ("id", Value::from("1")),
                        (
                            "child",
                            object([("id", Value::from("a")), ("score", Value::Int(1))]),
                        ),
                    ]),
                    // The second child is missing its non-null `score`.
                    object([
                        ("id", Value::from("2")),
                        ("child", object([("id", Value::from("b"))])),
                    ]),
                ]))
            })
        }),
    );

    let mut builder = Schema::builder();
    builder.add_type(child.into());
    builder.add_type(parent.into());
    builder.add_type(root.into());
    let schema = builder.query("Query").build().expect("valid schema");

    let response = run(&schema, "{ parents { id child { score } } }");

    assert_eq!(response.errors.len(), 1, "{:?}", response.errors);
    let path = response.errors[0].path().expect("path");
    let rendered: Vec<String> = path.iter().map(ToString::to_string).collect();
    assert_eq!(rendered, ["parents", "1", "child", "score"]);
    assert!(
        response.errors[0]
            .message()
            .contains("Cannot return null for non-nullable field"),
        "{:?}",
        response.errors
    );

    // `Child.score` is non-null so `child` becomes null, `Child` is non-null on `Parent` so the
    // parent becomes null, `Parent` items are non-null so the list becomes null, and `parents` is
    // a non-null root field, so the whole `data` entry becomes null.
    assert_eq!(response.data, Some(Value::Null), "{:?}", response.data);
}

#[test]
fn a_resolver_inside_a_default_subtree_runs_exactly_once() {
    let calls = Arc::new(AtomicUsize::new(0));
    let counter = Arc::clone(&calls);

    let profile = ObjectType::new("Profile")
        .field(Field::new("bio", TypeRef::string()))
        .field(
            Field::new("shout", TypeRef::string().non_null()).resolve(move |_| {
                let counter = Arc::clone(&counter);
                Box::pin(async move {
                    counter.fetch_add(1, Ordering::SeqCst);
                    Ok(Value::from("HELLO"))
                })
            }),
        );

    let user = ObjectType::new("User")
        .field(Field::new("id", TypeRef::id().non_null()))
        .field(Field::new("profile", TypeRef::named("Profile").non_null()));

    let root = ObjectType::new("Query").field(
        Field::new("user", TypeRef::named("User").non_null()).resolve(|_| {
            Box::pin(async {
                Ok(object([
                    ("id", Value::from("1")),
                    ("profile", object([("bio", Value::from("hi"))])),
                ]))
            })
        }),
    );

    let mut builder = Schema::builder();
    builder.add_type(profile.into());
    builder.add_type(user.into());
    builder.add_type(root.into());
    let schema = builder.query("Query").build().expect("valid schema");

    let response = run(&schema, "{ user { id profile { bio shout } } }");
    assert!(response.errors.is_empty(), "{:?}", response.errors);
    let data = response
        .data
        .as_ref()
        .and_then(Value::as_object)
        .expect("data");
    let profile = data
        .get("user")
        .and_then(Value::as_object)
        .and_then(|user| user.get("profile"))
        .and_then(Value::as_object)
        .expect("profile");
    assert_eq!(profile.get("bio").and_then(Value::as_str), Some("hi"));
    assert_eq!(profile.get("shout").and_then(Value::as_str), Some("HELLO"));
    assert_eq!(calls.load(Ordering::SeqCst), 1);
}

#[test]
fn a_user_scalar_serializer_runs_exactly_once_when_the_subtree_falls_back() {
    let calls = Arc::new(AtomicUsize::new(0));
    let counter = Arc::clone(&calls);

    let odd = ScalarType::new("Odd").serialize(move |value| {
        counter.fetch_add(1, Ordering::SeqCst);
        Ok(value.clone())
    });

    let root = ObjectType::new("Query")
        .field(
            Field::new("thing", TypeRef::named("Thing").non_null()).resolve(|_| {
                Box::pin(async {
                    Ok(object([
                        ("value", Value::from("x")),
                        // A resolver after the custom scalar: completion restarts asynchronously.
                        ("later", Value::from("y")),
                    ]))
                })
            }),
        )
        .field(
            Field::new("async", TypeRef::string())
                .resolve(|_| Box::pin(async { Ok(Value::from("done")) })),
        );

    let thing = ObjectType::new("Thing")
        .field(Field::new("value", TypeRef::named("Odd").non_null()))
        .field(
            Field::new("later", TypeRef::string().non_null())
                .resolve(|_| Box::pin(async { Ok(Value::from("y")) })),
        );

    let mut builder = Schema::builder();
    builder.add_type(thing.into());
    builder.add_type(root.into());
    builder.add_type(odd.into());
    let schema = builder.query("Query").build().expect("valid schema");

    let response = run(&schema, "{ thing { value later } async }");
    assert!(response.errors.is_empty(), "{:?}", response.errors);
    let data = response
        .data
        .as_ref()
        .and_then(Value::as_object)
        .expect("data");
    let thing = data.get("thing").and_then(Value::as_object).expect("thing");
    assert_eq!(thing.get("value").and_then(Value::as_str), Some("x"));
    assert_eq!(thing.get("later").and_then(Value::as_str), Some("y"));
    assert_eq!(data.get("async").and_then(Value::as_str), Some("done"));
    assert_eq!(calls.load(Ordering::SeqCst), 1);
}

#[derive(Clone)]
struct Recorder {
    seen: Arc<std::sync::Mutex<Vec<String>>>,
}

impl Authorizer for Recorder {
    fn authorize(&self, context: &AuthorizationContext<'_>) -> Result<(), GraphQLError> {
        self.seen
            .lock()
            .expect("lock")
            .push(context.field_name.to_owned());
        if context.field_name == "secret" {
            return Err(GraphQLError::new("denied"));
        }
        Ok(())
    }
}

#[test]
fn an_authorizer_still_sees_every_field_of_a_default_resolved_object() {
    let seen = Arc::new(std::sync::Mutex::new(Vec::new()));

    let user = ObjectType::new("User")
        .field(Field::new("id", TypeRef::id().non_null()))
        .field(Field::new("secret", TypeRef::string()));

    let root = ObjectType::new("Query").field(
        Field::new("user", TypeRef::named("User").non_null()).resolve(|_| {
            Box::pin(async {
                Ok(object([
                    ("id", Value::from("1")),
                    ("secret", Value::from("classified")),
                ]))
            })
        }),
    );

    let mut builder = Schema::builder();
    builder.add_type(user.into());
    builder.add_type(root.into());
    let schema = builder.query("Query").build().expect("valid schema");

    let options = ExecutionOptions::default().authorizer(Recorder {
        seen: Arc::clone(&seen),
    });
    let response = block_on(gql::execute(
        &schema,
        Request::new("{ user { id secret } }"),
        &options,
    ));

    let data = response
        .data
        .as_ref()
        .and_then(Value::as_object)
        .expect("data");
    let user = data.get("user").and_then(Value::as_object).expect("user");
    assert_eq!(user.get("id").and_then(Value::as_str), Some("1"));
    assert_eq!(user.get("secret"), Some(&Value::Null));
    assert_eq!(response.errors.len(), 1, "{:?}", response.errors);
    assert_eq!(response.errors[0].message(), "denied");

    let seen = seen.lock().expect("lock");
    assert!(seen.iter().any(|name| name == "id"), "{seen:?}");
    assert!(seen.iter().any(|name| name == "secret"), "{seen:?}");
    assert_eq!(
        seen.iter().filter(|name| *name == "secret").count(),
        1,
        "a denied field must be authorized once: {seen:?}"
    );
}

#[test]
fn arguments_are_still_coerced_inside_a_default_resolved_object() {
    let item = ObjectType::new("Item")
        .field(Field::new("id", TypeRef::id().non_null()))
        .field(
            Field::new("sized", TypeRef::string().non_null())
                .arg(InputValue::new("size", TypeRef::int().non_null()))
                .arg(InputValue::new("label", TypeRef::string())),
        );

    let root = ObjectType::new("Query").field(
        Field::new("items", TypeRef::named("Item").non_null().list().non_null()).resolve(|_| {
            Box::pin(async {
                Ok(Value::List(vec![object([
                    ("id", Value::from("1")),
                    ("sized", Value::from("s")),
                ])]))
            })
        }),
    );

    let mut builder = Schema::builder();
    builder.add_type(item.into());
    builder.add_type(root.into());
    let schema = builder.query("Query").build().expect("valid schema");

    // Literal arguments are coerced (and validated) exactly as before; the default resolver then
    // reads the parent property and ignores them.
    let response = run(&schema, r#"{ items { id sized(size: 2, label: "x") } }"#);
    assert!(response.errors.is_empty(), "{:?}", response.errors);
    let data = response
        .data
        .as_ref()
        .and_then(Value::as_object)
        .expect("data");
    let items = data.get("items").and_then(Value::as_list).expect("items");
    assert_eq!(
        items[0]
            .as_object()
            .and_then(|item| item.get("sized"))
            .and_then(Value::as_str),
        Some("s")
    );

    // An argument of the wrong type is still rejected.
    let response = run(&schema, r#"{ items { id sized(size: "two") } }"#);
    assert_eq!(response.data, None, "{:?}", response.data);
    assert_eq!(response.errors.len(), 1, "{:?}", response.errors);
}

#[test]
fn deeply_nested_default_objects_stay_within_a_small_stack() {
    // The fast path recurses for default-resolved objects, so it must hand over to the boxed
    // asynchronous path before the stack becomes a function of the document's depth.
    fn nest() -> Schema {
        let node = ObjectType::new("Node")
            .field(Field::new("value", TypeRef::int().non_null()))
            .field(Field::new("child", TypeRef::named("Node")));

        fn value(depth: usize) -> Value {
            let mut current = object([("value", Value::Int(1))]);
            for _ in 1..depth {
                current = object([("value", Value::Int(1)), ("child", current)]);
            }
            current
        }

        let root =
            ObjectType::new("Query").field(Field::new("node", TypeRef::named("Node")).resolve({
                let value = value(40);
                move |_| {
                    let value = value.clone();
                    Box::pin(async move { Ok(value) })
                }
            }));

        let mut builder = Schema::builder();
        builder.add_type(node.into());
        builder.add_type(root.into());
        builder.query("Query").build().expect("valid schema")
    }

    let schema = nest();
    let mut query = String::from("{ node ");
    for _ in 0..40 {
        query.push_str("{ child ");
    }
    query.push_str("{ value }");
    for _ in 0..41 {
        query.push_str(" }");
    }

    std::thread::Builder::new()
        .stack_size(2 * 1024 * 1024)
        .spawn(move || {
            let response = run(&schema, &query);
            assert!(response.errors.is_empty(), "{:?}", response.errors);
            assert!(response.data.is_some());
        })
        .expect("spawn")
        .join()
        .expect("join");
}

/// A schema whose objects mix built-in scalar properties, an aliased scalar, and scalars that
/// need the registered serializer to convert the value (`ID` from an integer, `Float` from an
/// integer) or to reject a wrong shape.
fn scalar_schema(extra: impl FnOnce(&mut gql::SchemaBuilder)) -> Schema {
    let thing = ObjectType::new("Thing")
        .field(Field::new("text", TypeRef::string().non_null()))
        .field(Field::new("count", TypeRef::int()))
        .field(Field::new("ratio", TypeRef::float()))
        .field(Field::new("id", TypeRef::id().non_null()))
        .field(Field::new("flag", TypeRef::boolean().non_null()));

    let root = ObjectType::new("Query")
        .field(
            Field::new("thing", TypeRef::named("Thing").non_null()).resolve(|_| {
                Box::pin(async {
                    Ok(Value::object(Object::from_fields([
                        // `ID` from an integer and `Float` from an integer must both be converted
                        // by the scalar's serializer, not passed through.
                        ("id", Value::Int(7)),
                        ("ratio", Value::Int(3)),
                        ("text", Value::from("hello")),
                        ("flag", Value::Boolean(true)),
                    ])))
                })
            }),
        )
        .field(
            Field::new("broken", TypeRef::named("Thing").non_null()).resolve(|_| {
                Box::pin(async {
                    Ok(Value::object(Object::from_fields([
                        ("id", Value::from("1")),
                        ("text", Value::Int(1)),
                        ("flag", Value::Boolean(false)),
                    ])))
                })
            }),
        );

    let mut builder = Schema::builder();
    builder.add_type(thing.into());
    builder.add_type(root.into());
    extra(&mut builder);
    builder.query("Query").build().expect("valid schema")
}

#[test]
fn builtin_scalar_properties_complete_and_still_convert() {
    let schema = scalar_schema(|_| {});
    let response = run(&schema, "{ thing { text count ratio id flag } }");
    assert!(response.errors.is_empty(), "{:?}", response.errors);
    let thing = response
        .data
        .as_ref()
        .and_then(|data| data.as_object())
        .and_then(|data| data.get("thing"))
        .and_then(Value::as_object)
        .expect("thing");

    assert_eq!(thing.get("text").and_then(Value::as_str), Some("hello"));
    // A property that is absent from the parent stays `null` for a nullable field.
    assert_eq!(thing.get("count"), Some(&Value::Null));
    // The serializers for `ID` and `Float` still run: both convert an integer.
    assert_eq!(thing.get("id").and_then(Value::as_str), Some("7"));
    assert_eq!(thing.get("ratio").and_then(Value::as_f64), Some(3.0));
    assert_eq!(thing.get("flag").and_then(Value::as_bool), Some(true));
}

#[test]
fn an_aliased_scalar_uses_its_alias_as_the_response_key() {
    let schema = scalar_schema(|_| {});
    let response = run(&schema, "{ thing { label: text } }");
    assert!(response.errors.is_empty(), "{:?}", response.errors);
    let thing = response
        .data
        .as_ref()
        .and_then(|data| data.as_object())
        .and_then(|data| data.get("thing"))
        .and_then(Value::as_object)
        .expect("thing");
    assert_eq!(thing.get("label").and_then(Value::as_str), Some("hello"));
    assert!(thing.get("text").is_none());
}

#[test]
fn a_builtin_scalar_of_the_wrong_shape_still_reports_the_serializer_error() {
    let schema = scalar_schema(|_| {});
    let response = run(&schema, "{ broken { text } }");
    assert_eq!(response.errors.len(), 1, "{:?}", response.errors);
    let error = &response.errors[0];
    assert_eq!(
        error.message(),
        "Field Thing.text could not be serialized: String cannot represent a value of type number"
    );
    // `text` and `broken` are both non-null, so the failure bubbles to the root data entry.
    assert_eq!(response.data, Some(Value::Null));
    let path: Vec<String> = error
        .path()
        .expect("path")
        .iter()
        .map(ToString::to_string)
        .collect();
    assert_eq!(path, ["broken", "text"]);
    assert_eq!(error.locations().len(), 1);
}

#[test]
fn a_custom_scalar_still_runs_its_serializer() {
    let mut builder = Schema::builder();
    builder.add_type(
        ScalarType::new("Weird")
            .serialize(|value| Ok(Value::String(Arc::from(format!("custom:{value}")))))
            .into(),
    );
    let holder =
        ObjectType::new("Holder").field(Field::new("value", TypeRef::named("Weird").non_null()));
    let root = ObjectType::new("Query").field(
        Field::new("holder", TypeRef::named("Holder").non_null()).resolve(|_| {
            Box::pin(async {
                Ok(Value::object(Object::from_fields([(
                    "value",
                    Value::Boolean(false),
                )])))
            })
        }),
    );
    builder.add_type(holder.into());
    builder.add_type(root.into());
    let schema = builder.query("Query").build().expect("valid schema");

    let response = run(&schema, "{ holder { value } }");
    assert!(response.errors.is_empty(), "{:?}", response.errors);
    let holder = response
        .data
        .as_ref()
        .and_then(|data| data.as_object())
        .and_then(|data| data.get("holder"))
        .and_then(Value::as_object)
        .expect("holder");
    assert_eq!(
        holder.get("value").and_then(Value::as_str),
        Some("custom:false")
    );
}
