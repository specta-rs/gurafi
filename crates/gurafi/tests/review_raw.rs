//! Adversarial review probes for issue #7 (`Raw` / `RawObject` / `raw_field`).
//!
//! These are scratch probes, not part of the shipped test suite.

#![allow(dead_code)]

use std::sync::Arc;

use futures::{StreamExt, stream};
use gurafi::{
    Enum, ID, IntoValue, NoArgs, Object, Raw, RawObject, Scalar, Schema, Union, Value, expose,
};

#[derive(Clone, Default)]
struct Ctx {
    allowed: bool,
}

#[derive(Object, Clone)]
struct Keeper {
    name: String,
}

#[derive(Object)]
struct Giraffe {
    name: String,
    height: f32,
    nickname: Option<String>,
    keeper: Keeper,
}

#[derive(Object)]
struct User {
    name: String,
}

#[derive(Object)]
struct Post {
    title: String,
}

#[derive(Enum, Clone, Copy)]
enum Color {
    Red,
    Blue,
}

#[derive(Scalar, Clone, Debug, PartialEq)]
#[graphql(name = "Stamp")]
struct Stamp(String);

#[derive(Union)]
enum Search {
    User(User),
    Post(Post),
}

#[derive(Object)]
struct Holder {
    count: i32,
    color: Color,
    stamp: Stamp,
    id: ID,
    search: Search,
    empty: Vec<String>,
    matrix: Vec<Vec<i32>>,
    sparse: Vec<Option<i32>>,
}

fn run(schema: &gql::Schema, query: &str) -> gql::Response {
    run_with(schema, query, Ctx::default())
}

fn run_with(schema: &gql::Schema, query: &str, ctx: Ctx) -> gql::Response {
    futures::executor::block_on(gql::execute(
        schema,
        gql::Request::new(query),
        &gql::ExecutionOptions::with_context(ctx),
    ))
}

fn data(response: &gql::Response) -> &gql::Object {
    response
        .data
        .as_ref()
        .expect("response has data")
        .as_object()
        .expect("data is an object")
}

fn field<'a>(response: &'a gql::Response, name: &str) -> &'a Value {
    data(response)
        .get(name)
        .unwrap_or_else(|| panic!("missing field `{name}` in response: {response:?}"))
}

fn raw_giraffe(name: &str) -> Raw<Giraffe> {
    Raw::object(
        RawObject::new()
            .typename("Giraffe")
            .field("id", name.to_ascii_lowercase())
            .field("name", name)
            .field("height", 3.4)
            .field("picture", "wide.jpg")
            .field("keeper", RawObject::new().field("name", "Bob"))
            .build(),
    )
}

fn raw_giraffe_map(name: &str) -> RawObject {
    RawObject::new()
        .field("name", name)
        .field("height", 3.4)
        .field("keeper", RawObject::new().field("name", "Bob"))
}

fn holder_schema() -> gql::Schema {
    let mut schema = Schema::<Ctx>::new();
    schema.object::<Holder>(|o| {
        o.raw_field::<i32>("count");
        o.raw_field::<Color>("color");
        o.raw_field::<Stamp>("stamp");
        o.raw_field::<ID>("id");
        o.raw_field::<Search>("search");
        o.raw_field::<Vec<String>>("empty");
        o.raw_field::<Vec<Vec<i32>>>("matrix");
        o.raw_field::<Vec<Option<i32>>>("sparse");
    });
    schema.object::<User>(|o| {
        o.raw_field::<String>("name");
    });
    schema.object::<Post>(|o| {
        o.raw_field::<String>("title");
    });
    schema.union("Search", |u| {
        u.member::<User>().member::<Post>();
    });
    schema.object::<Giraffe>(|o| {
        o.raw_field::<String>("name");
        o.raw_field::<f32>("height");
        o.raw_field::<Keeper>("keeper");
    });
    schema.object::<Keeper>(|o| {
        o.raw_field::<String>("name");
    });
    schema.query(|q| {
        q.field("holder", |_: NoArgs, _: &Ctx| {
            Raw::<Holder>::object(
                RawObject::new()
                    .field("count", 3)
                    .field("color", "Red")
                    .field("stamp", "2026-11-10")
                    .field("id", "abc")
                    .field(
                        "search",
                        RawObject::new()
                            .typename("User")
                            .field("name", "Oscar")
                            .build(),
                    )
                    .field("empty", Value::List(Vec::new()))
                    .field(
                        "matrix",
                        Value::List(vec![
                            Value::List(vec![Value::from(1), Value::from(2)]),
                            Value::List(Vec::new()),
                        ]),
                    )
                    .field("sparse", Value::List(vec![Value::Null, Value::from(7)]))
                    .build(),
            )
        });

        q.field("giraffe", |_: NoArgs, _: &Ctx| raw_giraffe("Melman"));

        // A list with an explicit null item where the schema says items are non-null.
        q.field("brokenList", |_: NoArgs, _: &Ctx| {
            Raw::<Vec<String>>::new(Value::List(vec![
                Value::String(Arc::from("ok")),
                Value::Null,
            ]))
        });

        // A nested object missing a non-null field, selected through an alias.
        q.field("brokenObject", |_: NoArgs, _: &Ctx| {
            Raw::<Giraffe>::object(
                RawObject::new()
                    .field("height", 1.0)
                    .field("keeper", RawObject::new().field("name", "Bob").build())
                    .build(),
            )
        });
    });
    schema.build().expect("holder schema builds")
}

#[test]
fn raw_scalar_enum_custom_scalar_union_and_list_shapes() {
    let schema = holder_schema();

    let matrix_field = schema
        .object_type("Holder")
        .and_then(|object| object.fields.get("matrix"))
        .map(|field| field.ty.clone())
        .expect("matrix field");
    assert_eq!(
        matrix_field,
        gql::TypeRef::named("Int")
            .non_null()
            .list()
            .non_null()
            .list()
            .non_null()
    );

    let holder = schema
        .object_type("Holder")
        .expect("Holder registered")
        .fields
        .get("sparse")
        .map(|field| field.ty.clone())
        .expect("sparse field");
    assert_eq!(
        holder,
        gql::TypeRef::named("Int").list().non_null(),
        "Vec<Option<i32>> should be [Int]!"
    );

    let response = run(
        &schema,
        "{ holder { count color stamp id search { __typename ... on User { name } ... on Post { title } } empty matrix sparse } }",
    );
    assert!(!response.has_errors(), "unexpected errors: {response:?}");
    let holder = field(&response, "holder")
        .as_object()
        .expect("holder is an object");
    assert_eq!(holder.get("count"), Some(&Value::Int(3)));
    assert_eq!(holder.get("color"), Some(&Value::Enum(Arc::from("Red"))));
    assert_eq!(
        holder.get("stamp").and_then(Value::as_str),
        Some("2026-11-10")
    );
    assert_eq!(holder.get("id").and_then(Value::as_str), Some("abc"));
    assert_eq!(
        holder
            .get("empty")
            .and_then(Value::as_list)
            .map(<[Value]>::len),
        Some(0)
    );
    assert_eq!(
        holder
            .get("matrix")
            .and_then(Value::as_list)
            .map(<[Value]>::len),
        Some(2)
    );
    assert_eq!(
        holder
            .get("sparse")
            .and_then(Value::as_list)
            .map(|items| items[0].clone()),
        Some(Value::Null)
    );
    assert_eq!(
        holder
            .get("search")
            .and_then(Value::as_object)
            .and_then(|object| object.get("__typename"))
            .and_then(Value::as_str),
        Some("User")
    );
}

#[test]
fn null_item_where_items_are_non_null_nulls_the_list_with_a_path() {
    let schema = holder_schema();
    let response = run(&schema, "{ list: brokenList }");
    assert!(response.has_errors(), "expected an error: {response:?}");
    assert!(
        response.data.as_ref().is_none_or(Value::is_null),
        "a non-null list field failing nulls the response data: {response:?}"
    );
    let error = &response.errors[0];
    let path: Vec<String> = error
        .path()
        .expect("error has a path")
        .iter()
        .map(ToString::to_string)
        .collect();
    assert_eq!(path, ["list", "1"]);
}

#[test]
fn missing_nested_non_null_field_uses_the_alias_in_the_path() {
    let schema = holder_schema();
    let response = run(&schema, "{ alias: brokenObject { name } }");
    assert!(response.has_errors(), "expected an error: {response:?}");
    assert_eq!(field(&response, "alias"), &Value::Null);
    let error = &response.errors[0];
    assert!(error.message().contains("Cannot return null"), "{error:?}");
    let path: Vec<String> = error
        .path()
        .expect("error has a path")
        .iter()
        .map(ToString::to_string)
        .collect();
    assert_eq!(path, ["alias", "name"]);
}

#[test]
fn aliases_and_repeated_selections_work_on_raw_objects() {
    let schema = holder_schema();
    let response = run(
        &schema,
        "{ a: giraffe { n: name } b: giraffe { __typename name k: keeper { who: name } } }",
    );
    assert!(!response.has_errors(), "unexpected errors: {response:?}");
    assert_eq!(
        field(&response, "a")
            .as_object()
            .and_then(|object| object.get("n"))
            .and_then(Value::as_str),
        Some("Melman")
    );
    let b = field(&response, "b").as_object().expect("b is an object");
    assert_eq!(b.get("__typename").and_then(Value::as_str), Some("Giraffe"));
    assert_eq!(
        b.get("k")
            .and_then(Value::as_object)
            .and_then(|keeper| keeper.get("who"))
            .and_then(Value::as_str),
        Some("Bob")
    );
}

#[test]
fn raw_type_refs_follow_t() {
    assert_eq!(
        gurafi::type_ref::<Raw<Option<String>>>(),
        gql::TypeRef::string()
    );
    assert_eq!(
        gurafi::type_ref::<Raw<Vec<Option<String>>>>(),
        gql::TypeRef::string().list().non_null(),
        "Vec<Option<String>> is [String]!"
    );
    assert_eq!(
        gurafi::type_ref::<Raw<Option<Vec<String>>>>(),
        gql::TypeRef::string().non_null().list(),
        "Option<Vec<String>> is [String!]"
    );
}

// ---------------------------------------------------------------------------
// Authorization
// ---------------------------------------------------------------------------

fn interface_auth_schema() -> gql::Schema {
    let mut schema = Schema::<Ctx>::new();
    let node = schema.interface("Node", |i| {
        i.auth(|ctx: &Ctx| ctx.allowed);
        i.field::<String>("name");
        i.field::<f32>("height");
    });
    schema.object::<Giraffe>(|o| {
        o.implements(node);
        o.raw_field::<String>("name");
        o.raw_field::<f32>("height");
    });
    schema.query(|q| {
        q.field("node", |_: NoArgs, _: &Ctx| raw_giraffe("Melman"))
            .as_abstract(node);
    });
    schema.build().expect("interface auth schema builds")
}

#[test]
fn interface_level_auth_applies_to_raw_fields() {
    let schema = interface_auth_schema();

    let denied = run(&schema, "{ node { name height } }");
    assert!(denied.has_errors(), "expected a denial: {denied:?}");
    assert!(
        denied
            .errors
            .iter()
            .all(|error| error.message().contains("Not authorized")),
        "unexpected errors: {denied:?}"
    );

    let allowed = run_with(&schema, "{ node { name height } }", Ctx { allowed: true });
    assert!(!allowed.has_errors(), "unexpected errors: {allowed:?}");
}

#[test]
fn interface_field_auth_applies_to_raw_fields() {
    let mut schema = Schema::<Ctx>::new();
    let node = schema.interface("Node", |i| {
        i.field::<Option<String>>("name").auth(|_: &Ctx| false);
        i.field::<String>("id");
    });
    schema.object::<Giraffe>(|o| {
        o.implements(node);
        o.raw_field::<Option<String>>("name");
        o.raw_field::<String>("id");
    });
    schema.query(|q| {
        q.field("node", |_: NoArgs, _: &Ctx| raw_giraffe("Melman"))
            .as_abstract(node);
    });
    let schema = schema.build().expect("schema builds");

    // `id` is unguarded and still resolves, but `name` is denied.
    let response = run(&schema, "{ node { id name } }");
    assert!(response.has_errors(), "expected a denial: {response:?}");
    assert!(
        response
            .errors
            .iter()
            .all(|error| { error.message().contains("Not authorized") }),
        "unexpected errors: {response:?}"
    );
}

#[test]
fn async_field_auth_applies_to_a_raw_field_that_has_no_resolver() {
    let mut schema = Schema::<Ctx>::new();
    schema.object::<Giraffe>(|o| {
        o.raw_field::<Option<String>>("name")
            .auth_async(|_: &Ctx| Box::pin(async { false }));
        o.raw_field::<f32>("height");
    });
    schema.query(|q| {
        q.field("giraffe", |_: NoArgs, _: &Ctx| raw_giraffe("Melman"));
    });
    let schema = schema.build().expect("schema builds");

    let response = run(&schema, "{ giraffe { name height } }");
    assert!(response.has_errors(), "expected a denial: {response:?}");
    let giraffe = field(&response, "giraffe")
        .as_object()
        .expect("giraffe is an object");
    assert_eq!(giraffe.get("name"), Some(&Value::Null));
    assert_eq!(giraffe.get("height"), Some(&Value::Float(3.4)));
}

#[test]
fn object_auth_is_not_bypassed_by_selecting_only_typename() {
    #[derive(Object)]
    struct Classified {
        name: String,
    }
    let mut schema = Schema::<Ctx>::new();
    schema.object::<Classified>(|o| {
        o.auth(|_: &Ctx| false);
        o.raw_field::<String>("name");
    });
    schema.query(|q| {
        q.field("classified", |_: NoArgs, _: &Ctx| {
            Raw::<Classified>::object(RawObject::new().field("name", "classified").build())
        });
    });
    let schema = schema.build().expect("schema builds");

    let response = run(&schema, "{ classified { __typename } }");
    // Recorded behaviour: the object-level rule is attached to fields, so `__typename` is
    // still returned even though every real field would be denied.
    let classified = field(&response, "classified")
        .as_object()
        .expect("classified is an object");
    assert_eq!(
        classified.get("__typename").and_then(Value::as_str),
        Some("Classified"),
        "typename leak is pre-existing object-auth behaviour: {response:?}"
    );
}

#[test]
fn subscription_events_can_be_raw_objects() {
    let mut schema = Schema::<Ctx>::new();
    schema.object::<Giraffe>(|o| {
        o.raw_field::<String>("name");
        o.raw_field::<f32>("height");
    });
    schema.query(|q| {
        q.field("ok", |_: NoArgs, _: &Ctx| true);
    });
    schema.subscription(|s| {
        s.field("giraffe", |_: NoArgs, _: &Ctx| {
            stream::iter(vec![Ok::<_, gurafi::GraphQLError>(raw_giraffe("Melman"))])
        });
    });
    let schema = schema.build().expect("schema builds");
    let options = gql::ExecutionOptions::with_context(Ctx::default());

    let subscription = futures::executor::block_on(gql::subscribe(
        &schema,
        gql::Request::new("subscription { giraffe { name height } }"),
        &options,
    ))
    .expect("subscription starts");
    let events = futures::executor::block_on(subscription.stream.take(1).collect::<Vec<_>>());
    assert_eq!(events.len(), 1);
    assert!(
        !events[0].has_errors(),
        "unexpected errors: {:?}",
        events[0]
    );
    let giraffe = data(&events[0])
        .get("giraffe")
        .and_then(Value::as_object)
        .expect("giraffe is an object");
    assert_eq!(giraffe.get("name").and_then(Value::as_str), Some("Melman"));
}

// ---------------------------------------------------------------------------
// Build-time and misuse behaviour
// ---------------------------------------------------------------------------

#[test]
fn raw_field_requires_the_object_to_be_registered() {
    #[derive(Object)]
    struct Unregistered {
        name: String,
    }
    #[derive(Object)]
    struct UsesUnregistered {
        nested: Unregistered,
    }
    let mut schema = Schema::<Ctx>::new();
    schema.object::<UsesUnregistered>(|o| {
        o.raw_field::<Unregistered>("nested");
    });
    schema.query(|q| {
        q.field("x", |_: NoArgs, _: &Ctx| {
            Raw::<UsesUnregistered>::object(RawObject::new().build())
        });
    });
    let error = schema
        .build()
        .expect_err("unregistered object is a build error");
    assert!(
        error.to_string().contains("never registered"),
        "unexpected error: {error}"
    );
}

#[test]
fn duplicate_raw_fields_are_a_build_error() {
    let mut schema = Schema::<Ctx>::new();
    schema.object::<Giraffe>(|o| {
        o.raw_field::<String>("name");
        o.raw_field::<String>("name");
    });
    schema.query(|q| {
        q.field("giraffe", |_: NoArgs, _: &Ctx| raw_giraffe("Melman"));
    });
    let error = schema
        .build()
        .expect_err("duplicate field is a build error");
    assert!(
        error.to_string().contains("more than once"),
        "unexpected error: {error}"
    );
}

#[test]
fn mixing_raw_and_resolver_fields_is_a_build_error() {
    #[derive(Object)]
    struct Mixed {
        name: String,
        height: Option<f32>,
    }
    let mut schema = Schema::<Ctx>::new();
    schema.object::<Mixed>(|o| {
        o.raw_field::<String>("name");
        o.expose(expose!(Mixed::height));
    });
    schema.query(|q| {
        q.field("mixed", |_: NoArgs, _: &Ctx| {
            Raw::<Mixed>::object(
                RawObject::new()
                    .field("name", "Melman")
                    .field("height", 3.4)
                    .build(),
            )
        });
    });
    let error = schema
        .build()
        .expect_err("mixing raw and resolver fields is rejected at build time");
    assert!(
        error.to_string().contains("mixes raw fields"),
        "unexpected error: {error}"
    );
}

#[test]
fn non_null_on_an_optional_raw_field_is_a_build_error() {
    let mut schema = Schema::<Ctx>::new();
    schema.object::<Giraffe>(|o| {
        o.raw_field::<Option<String>>("nickname").non_null();
    });
    schema.query(|q| {
        q.field("giraffe", |_: NoArgs, _: &Ctx| raw_giraffe("Melman"));
    });
    let error = schema
        .build()
        .expect_err("non_null on Option is a build error");
    assert!(
        error.to_string().contains("optional"),
        "unexpected error: {error}"
    );
}

#[test]
fn validate_on_a_raw_field_is_a_build_error() {
    let mut schema = Schema::<Ctx>::new();
    schema.object::<Giraffe>(|o| {
        o.raw_field::<String>("name")
            .validate::<NoArgs>(|_, _| Ok(()));
    });
    schema.query(|q| {
        q.field("giraffe", |_: NoArgs, _: &Ctx| raw_giraffe("Melman"));
    });
    let error = schema
        .build()
        .expect_err("a validator on a resolver-less raw field is rejected");
    assert!(
        error.to_string().contains("raw_field_with"),
        "the error should name the supported alternative: {error}"
    );
}

#[test]
fn a_second_object_declaration_can_add_raw_fields() {
    let mut schema = Schema::<Ctx>::new();
    schema.object::<Giraffe>(|o| {
        o.raw_field::<String>("name");
    });
    schema.object_fields::<Giraffe>(|o| {
        o.raw_field::<f32>("height");
    });
    schema.query(|q| {
        q.field("giraffe", |_: NoArgs, _: &Ctx| raw_giraffe("Melman"));
    });
    let schema = schema.build().expect("schema builds");
    let response = run(&schema, "{ giraffe { name height } }");
    assert!(!response.has_errors(), "unexpected errors: {response:?}");
}

// ---------------------------------------------------------------------------
// Abstract types and type identity
// ---------------------------------------------------------------------------

#[test]
fn raw_abstract_value_without_a_type_marker_is_an_error() {
    let mut schema = Schema::<Ctx>::new();
    let node = schema.interface("Node", |i| {
        i.field::<String>("name");
    });
    schema.object::<Giraffe>(|o| {
        o.implements(node);
        o.raw_field::<String>("name");
    });
    schema.query(|q| {
        q.field("node", |_: NoArgs, _: &Ctx| {
            Raw::<Giraffe>::object(RawObject::new().field("name", "Melman").build())
        })
        .as_abstract(node);
    });
    let schema = schema.build().expect("schema builds");
    let response = run(&schema, "{ node { name } }");
    assert!(response.has_errors(), "expected an error: {response:?}");
    assert_eq!(field(&response, "node"), &Value::Null);
}

#[test]
fn explicit_typename_wins_and_the_internal_key_is_ignored() {
    let mut schema = Schema::<Ctx>::new();
    schema.object::<User>(|o| {
        o.raw_field::<String>("name");
    });
    schema.object::<Post>(|o| {
        o.raw_field::<String>("title");
    });
    let search = schema.union("Search", |u| {
        u.member::<User>().member::<Post>();
    });
    schema.query(|q| {
        q.field("search", |_: NoArgs, _: &Ctx| {
            Raw::<Search>::object(
                RawObject::new()
                    // The documented way to select the concrete type.
                    .typename("User")
                    .field("name", "Oscar")
                    // An untrusted internal key cannot override it.
                    .field("__gql_typename", "Post")
                    .field("title", "attacker chosen")
                    .build(),
            )
        })
        .as_abstract(search);
    });
    let schema = schema.build().expect("schema builds");
    let response = run(
        &schema,
        "{ search { __typename ... on User { name } ... on Post { title } } }",
    );
    assert!(!response.has_errors(), "unexpected errors: {response:?}");
    let search = field(&response, "search")
        .as_object()
        .expect("search is an object");
    assert_eq!(
        search.get("__typename").and_then(Value::as_str),
        Some("User"),
        "explicit `__typename` selects the type: {response:?}"
    );
    assert_eq!(search.get("name").and_then(Value::as_str), Some("Oscar"));
    assert!(search.get("title").is_none());
}

#[test]
fn concrete_raw_object_typename_uses_its_declared_type() {
    let mut schema = Schema::<Ctx>::new();
    schema.object::<Giraffe>(|o| {
        o.raw_field::<String>("name");
    });
    schema.query(|q| {
        q.field("giraffe", |_: NoArgs, _: &Ctx| {
            Raw::<Giraffe>::object(
                RawObject::new()
                    .typename("Keeper")
                    .field("name", "Melman")
                    .build(),
            )
        });
    });
    let schema = schema.build().expect("schema builds");
    let response = run(&schema, "{ giraffe { __typename name } }");
    assert!(!response.has_errors(), "unexpected errors: {response:?}");
    assert_eq!(
        field(&response, "giraffe")
            .as_object()
            .and_then(|object| object.get("__typename"))
            .and_then(Value::as_str),
        Some("Giraffe")
    );
}

#[test]
fn raw_value_into_value_is_verbatim() {
    let raw = Raw::<String>::new(Value::Int(1));
    assert_eq!(raw.clone().into_value().expect("infallible"), Value::Int(1));
    assert_eq!(raw.raw_value(), &Value::Int(1));
    assert_eq!(raw, Raw::<String>::new(Value::Int(1)));
}

#[test]
fn wrong_scalar_value_kinds_are_rejected_not_silently_retyped() {
    #[derive(Object)]
    struct Scalars {
        text: String,
        number: i32,
        flag: bool,
        id: ID,
        color: Color,
    }

    fn build(value_for: &'static str) -> gql::Schema {
        let mut schema = Schema::<Ctx>::new();
        schema.object::<Scalars>(|o| {
            o.raw_field::<String>("text");
            o.raw_field::<i32>("number");
            o.raw_field::<bool>("flag");
            o.raw_field::<ID>("id");
            o.raw_field::<Color>("color");
        });
        schema.query(move |q| {
            let offending = match value_for {
                "text" => ("text", Value::Int(1)),
                "number" => ("number", Value::String(Arc::from("nope"))),
                "flag" => ("flag", Value::Int(0)),
                "id" => ("id", Value::Int(7)),
                "color" => ("color", Value::Int(1)),
                _ => unreachable!(),
            };
            q.field("scalars", move |_: NoArgs, _: &Ctx| {
                Raw::<Scalars>::object(
                    RawObject::new()
                        .field("text", "ok")
                        .field("number", 1)
                        .field("flag", true)
                        .field("id", "id-1")
                        .field("color", "Red")
                        .field(offending.0, offending.1.clone())
                        .build(),
                )
            });
        });
        schema.build().expect("schema builds")
    }

    let response = run(&build("text"), "{ scalars { text } }");
    // Record whatever the engine actually does with an Int in a String position.
    eprintln!("wrong text: {response:?}");

    let response = run(&build("number"), "{ scalars { number } }");
    eprintln!("wrong number: {response:?}");

    let response = run(&build("flag"), "{ scalars { flag } }");
    eprintln!("wrong flag: {response:?}");

    let response = run(&build("id"), "{ scalars { id } }");
    eprintln!("wrong id: {response:?}");

    let response = run(&build("color"), "{ scalars { color } }");
    assert!(
        response.has_errors(),
        "an Int enum value should error: {response:?}"
    );
}

#[test]
fn custom_scalar_raw_values_bypass_the_scalars_own_validation() {
    #[derive(Object)]
    struct Stamped {
        stamp: Stamp,
    }
    let mut schema = Schema::<Ctx>::new();
    schema.object::<Stamped>(|o| {
        o.raw_field::<Stamp>("stamp");
    });
    schema.query(|q| {
        q.field("stamped", |_: NoArgs, _: &Ctx| {
            Raw::<Stamped>::object(RawObject::new().field("stamp", Value::Int(1)).build())
        });
    });
    let schema = schema.build().expect("schema builds");
    let response = run(&schema, "{ stamped { stamp } }");
    // Recorded behaviour: a custom scalar's raw value is passed through unchanged, so the
    // docs' "wrong-typed field is an execution-time error" claim does not hold for them.
    eprintln!("custom scalar wrong kind: {response:?}");
    assert_eq!(
        field(&response, "stamped")
            .as_object()
            .and_then(|object| object.get("stamp")),
        Some(&Value::Int(1))
    );
}

#[test]
fn a_concrete_raw_object_cannot_report_an_internal_typename_it_was_not_declared_as() {
    let mut schema = Schema::<Ctx>::new();
    schema.object::<Giraffe>(|o| {
        o.raw_field::<String>("name");
    });
    schema.query(|q| {
        q.field("giraffe", |_: NoArgs, _: &Ctx| {
            Raw::<Giraffe>::object(
                RawObject::new()
                    .field("__gql_typename", "Keeper")
                    .field("name", "Melman")
                    .build(),
            )
        });
    });
    let schema = schema.build().expect("schema builds");
    let response = run(&schema, "{ giraffe { __typename name } }");
    assert!(!response.has_errors(), "unexpected errors: {response:?}");
    assert_eq!(
        field(&response, "giraffe")
            .as_object()
            .and_then(|object| object.get("__typename"))
            .and_then(Value::as_str),
        Some("Giraffe"),
        "the internal key must not override the declared object type: {response:?}"
    );
}

#[test]
fn async_resolvers_can_return_raw_values() {
    let mut schema = Schema::<Ctx>::new();
    schema.object::<Giraffe>(|o| {
        o.raw_field::<String>("name");
    });
    schema.query(|q| {
        q.field_async("giraffe", |_: NoArgs, _: &Ctx| {
            Box::pin(async { raw_giraffe("Melman") })
        });
    });
    let schema = schema.build().expect("schema builds");
    let response = run(&schema, "{ giraffe { name } }");
    assert!(!response.has_errors(), "unexpected errors: {response:?}");
}

#[test]
fn a_nullable_raw_field_can_be_missing() {
    let mut schema = Schema::<Ctx>::new();
    schema.object::<Giraffe>(|o| {
        o.raw_field::<String>("name").nullable();
    });
    schema.query(|q| {
        q.field("giraffe", |_: NoArgs, _: &Ctx| {
            Raw::<Giraffe>::object(RawObject::new().build())
        });
    });
    let schema = schema.build().expect("schema builds");
    let response = run(&schema, "{ giraffe { name } }");
    assert!(!response.has_errors(), "unexpected errors: {response:?}");
    assert_eq!(
        field(&response, "giraffe")
            .as_object()
            .and_then(|object| object.get("name")),
        Some(&Value::Null)
    );
}

#[test]
fn subscription_field_auth_denies_before_events() {
    let mut schema = Schema::<Ctx>::new();
    schema.object::<Giraffe>(|o| {
        o.raw_field::<String>("name");
    });
    schema.query(|q| {
        q.field("ok", |_: NoArgs, _: &Ctx| true);
    });
    schema.subscription(|s| {
        s.field("giraffe", |_: NoArgs, _: &Ctx| {
            stream::iter(vec![Ok::<_, gurafi::GraphQLError>(raw_giraffe("Melman"))])
        })
        .auth(|_: &Ctx| false);
    });
    let schema = schema.build().expect("schema builds");
    let options = gql::ExecutionOptions::with_context(Ctx::default());
    let result = futures::executor::block_on(gql::subscribe(
        &schema,
        gql::Request::new("subscription { giraffe { name } }"),
        &options,
    ));
    assert!(
        result.is_err(),
        "expected the subscription to be denied: {result:?}"
    );
}

#[test]
fn object_auth_on_a_raw_subscription_event_denies_every_field() {
    #[derive(Object)]
    struct SecretEvent {
        value: String,
    }
    let mut schema = Schema::<Ctx>::new();
    schema.object::<SecretEvent>(|o| {
        o.auth(|_: &Ctx| false);
        o.raw_field::<String>("value");
    });
    schema.query(|q| {
        q.field("ok", |_: NoArgs, _: &Ctx| true);
    });
    schema.subscription(|s| {
        s.field("secret", |_: NoArgs, _: &Ctx| {
            stream::iter(vec![Ok::<_, gurafi::GraphQLError>(
                Raw::<SecretEvent>::object(RawObject::new().field("value", "hidden").build()),
            )])
        });
    });
    let schema = schema.build().expect("schema builds");
    let options = gql::ExecutionOptions::with_context(Ctx::default());
    let subscription = futures::executor::block_on(gql::subscribe(
        &schema,
        gql::Request::new("subscription { secret { value } }"),
        &options,
    ))
    .expect("subscription starts");
    let events = futures::executor::block_on(subscription.stream.take(1).collect::<Vec<_>>());
    assert_eq!(events.len(), 1);
    assert!(events[0].has_errors(), "expected a denial: {:?}", events[0]);
    assert!(
        events[0]
            .errors
            .iter()
            .any(|error| error.message().contains("Not authorized")),
        "unexpected errors: {:?}",
        events[0]
    );
}

#[test]
fn internal_typename_key_cannot_contradict_a_resolve_type_result() {
    let mut schema = Schema::<Ctx>::new();
    schema.object::<User>(|o| {
        o.raw_field::<String>("name");
    });
    schema.object::<Post>(|o| {
        o.raw_field::<String>("title");
    });
    let search = schema.union("Search", |u| {
        u.member::<User>().member::<Post>();
        u.resolve_type(|value, _| {
            let object = value
                .as_object()
                .ok_or_else(|| gurafi::GraphQLError::new("not an object"))?;
            let kind = object
                .get("kind")
                .and_then(Value::as_str)
                .ok_or_else(|| gurafi::GraphQLError::new("no kind"))?;
            Ok(kind.into())
        });
    });
    schema.query(|q| {
        q.field("search", |_: NoArgs, _: &Ctx| {
            Raw::<Search>::object(
                RawObject::new()
                    .field("kind", "User")
                    .field("name", "Oscar")
                    // Attacker-controlled internal key. It is stripped by `RawObject`, and the
                    // executor would not trust it on a plain object anyway.
                    .field("__gql_typename", "Post")
                    .build(),
            )
        })
        .as_abstract(search);
    });
    let schema = schema.build().expect("schema builds");
    let response = run(
        &schema,
        "{ search { __typename ... on User { name } ... on Post { title } } }",
    );
    assert!(!response.has_errors(), "unexpected errors: {response:?}");
    let search = field(&response, "search")
        .as_object()
        .expect("search is an object");
    eprintln!("resolve_type + internal key: {response:?}");
    assert_eq!(
        search.get("__typename").and_then(Value::as_str),
        Some("User"),
        "`__typename` must agree with the type the executor completed as: {response:?}"
    );
}

#[test]
fn a_normal_parent_can_resolve_a_raw_child() {
    #[derive(Object)]
    struct Parent {
        name: String,
    }
    let mut schema = Schema::<Ctx>::new();
    schema.object::<Parent>(|o| {
        o.expose(expose!(Parent::name));
    });
    schema.object::<Giraffe>(|o| {
        o.raw_field::<String>("name");
        o.raw_field::<f32>("height");
    });
    schema.query(|q| {
        q.field("parent", |_: NoArgs, _: &Ctx| Parent { name: "p".into() });
        q.field("rawChild", |_: NoArgs, _: &Ctx| {
            Raw::<Giraffe>::object(
                RawObject::new()
                    .field("name", "Melman")
                    .field("height", 3.4)
                    .build(),
            )
        });
    });
    let schema = schema.build().expect("schema builds");
    let response = run(&schema, "{ parent { name } rawChild { name height } }");
    assert!(!response.has_errors(), "unexpected errors: {response:?}");
}

#[test]
fn repeated_root_selections_merge_on_a_raw_object() {
    let schema = holder_schema();
    let response = run(
        &schema,
        "{ giraffe { name } giraffe { height __typename } }",
    );
    assert!(!response.has_errors(), "unexpected errors: {response:?}");
    let giraffe = field(&response, "giraffe")
        .as_object()
        .expect("giraffe is an object");
    assert_eq!(giraffe.get("name").and_then(Value::as_str), Some("Melman"));
    assert_eq!(giraffe.get("height"), Some(&Value::Float(3.4)));
    assert_eq!(
        giraffe.get("__typename").and_then(Value::as_str),
        Some("Giraffe")
    );
}

#[test]
fn wrong_nested_shapes_are_field_errors() {
    #[derive(Object)]
    struct Nested {
        child: Giraffe,
        list: Vec<Giraffe>,
    }
    let mut schema = Schema::<Ctx>::new();
    schema.object::<Giraffe>(|o| {
        o.raw_field::<String>("name");
    });
    schema.object::<Nested>(|o| {
        o.raw_field::<Giraffe>("child");
        o.raw_field::<Vec<Giraffe>>("list");
    });
    schema.query(|q| {
        q.field("nested", |_: NoArgs, _: &Ctx| {
            Raw::<Nested>::object(
                RawObject::new()
                    .field("child", Value::String(Arc::from("not an object")))
                    .field("list", RawObject::new().field("name", "not a list"))
                    .build(),
            )
        });
    });
    let schema = schema.build().expect("schema builds");
    let response = run(&schema, "{ nested { child { name } list { name } } }");
    assert!(response.has_errors(), "expected shape errors: {response:?}");
    assert!(
        response
            .errors
            .iter()
            .any(|error| error.message().contains("expects an object")),
        "unexpected errors: {response:?}"
    );
    // The list field is non-null, so its shape violation nulls the parent; the nullable child
    // field is null independently. No panic either way.
    assert_eq!(field(&response, "nested"), &Value::Null);
}

#[test]
fn a_raw_list_of_abstract_values_resolves_each_item() {
    let mut schema = Schema::<Ctx>::new();
    let node = schema.interface("Node", |i| {
        i.field::<String>("name");
    });
    schema.object::<Giraffe>(|o| {
        o.implements(node);
        o.raw_field::<String>("name");
        o.raw_field::<f32>("height");
    });
    schema.query(|q| {
        q.field("nodes", |_: NoArgs, _: &Ctx| {
            Raw::<Vec<Giraffe>>::list([
                raw_giraffe("Melman"),
                Raw::object(
                    RawObject::new()
                        .typename("Giraffe")
                        .field("name", "Karen")
                        .field("height", 3.9)
                        .build(),
                ),
            ])
        })
        .as_abstract(node);
    });
    let schema = schema.build().expect("schema builds");
    let response = run(
        &schema,
        "{ nodes { __typename name ... on Giraffe { height } } }",
    );
    assert!(!response.has_errors(), "unexpected errors: {response:?}");
    let nodes = field(&response, "nodes")
        .as_list()
        .expect("nodes is a list");
    assert_eq!(nodes.len(), 2);
    assert_eq!(
        nodes[0]
            .as_object()
            .and_then(|object| object.get("name"))
            .and_then(Value::as_str),
        Some("Melman")
    );
}

#[test]
fn missing_field_inside_a_raw_list_reports_the_item_index() {
    #[derive(Object)]
    struct Simple {
        name: String,
    }
    let mut schema = Schema::<Ctx>::new();
    schema.object::<Simple>(|o| {
        o.raw_field::<String>("name");
    });
    schema.query(|q| {
        q.field("simples", |_: NoArgs, _: &Ctx| {
            Raw::<Vec<Simple>>::list([
                Raw::object(RawObject::new().field("name", "one").build()),
                Raw::object(RawObject::new().build()),
            ])
        });
    });
    let schema = schema.build().expect("schema builds");
    let response = run(&schema, "{ simples { name } }");
    assert!(response.has_errors(), "expected an error: {response:?}");
    let items = field(&response, "simples")
        .as_list()
        .expect("simples is a list");
    assert_eq!(items.len(), 2);
    assert_eq!(items[1], Value::Null);
    let path: Vec<String> = response.errors[0]
        .path()
        .expect("error has a path")
        .iter()
        .map(ToString::to_string)
        .collect();
    assert_eq!(path, ["simples", "1", "name"]);
}

#[test]
fn internal_typename_key_cannot_switch_to_a_less_protected_possible_type() {
    #[derive(Object)]
    struct Secret {
        name: String,
    }
    #[derive(Object)]
    struct Public {
        name: String,
    }

    let mut schema = Schema::<Ctx>::new();
    let node = schema.interface("Node", |i| {
        i.field::<String>("name");
    });
    schema.object::<Secret>(|o| {
        o.implements(node);
        o.auth(|_: &Ctx| false);
        o.raw_field::<String>("name");
    });
    schema.object::<Public>(|o| {
        o.implements(node);
        o.raw_field::<String>("name");
    });
    schema.query(|q| {
        q.field("node", |_: NoArgs, _: &Ctx| {
            Raw::<Secret>::object(
                RawObject::new()
                    // Caller says this is a Secret, which object auth denies...
                    .typename("Secret")
                    .field("name", "hidden")
                    // ...but an untrusted key switches the runtime type to Public.
                    .field("__gql_typename", "Public")
                    .build(),
            )
        })
        .as_abstract(node);
    });
    let schema = schema.build().expect("schema builds");
    let response = run(
        &schema,
        "{ node { __typename ... on Public { name } ... on Secret { name } } }",
    );
    eprintln!("type switch: {response:?}");
    assert!(
        response.has_errors(),
        "the Secret value's object-level auth should have denied the read: {response:?}"
    );
    assert_eq!(field(&response, "node"), &Value::Null);
}

#[test]
fn raw_objects_and_interface_fields_with_arguments() {
    use gurafi::Input;

    #[derive(Input)]
    struct PictureArgs {
        width: i32,
    }

    let mut schema = Schema::<Ctx>::new();
    let node = schema.interface("Node", |i| {
        i.field::<String>("name");
        i.field_with::<String, PictureArgs>("picture");
    });
    schema.object::<Giraffe>(|o| {
        o.implements(node);
        o.raw_field::<String>("name");
        // `raw_field_with` declares the argument type and installs a resolver, so the object
        // still satisfies the interface field while the value comes from the map.
        o.raw_field_with::<String, PictureArgs>("picture");
    });
    schema.query(|q| {
        q.field("node", |_: NoArgs, _: &Ctx| raw_giraffe("Melman"))
            .as_abstract(node);
    });
    let schema = schema.build().expect("schema builds");
    let response = run(&schema, "{ node { name picture(width: 1280) } }");
    assert!(!response.has_errors(), "unexpected errors: {response:?}");
    assert_eq!(
        field(&response, "node")
            .as_object()
            .and_then(|node| node.get("picture"))
            .and_then(Value::as_str),
        Some("wide.jpg")
    );
}

#[test]
fn abstract_resolution_failure_has_a_path() {
    let mut schema = Schema::<Ctx>::new();
    let node = schema.interface("Node", |i| {
        i.field::<String>("name");
    });
    schema.object::<Giraffe>(|o| {
        o.implements(node);
        o.raw_field::<String>("name");
    });
    schema.query(|q| {
        q.field("node", |_: NoArgs, _: &Ctx| {
            Raw::<Giraffe>::object(RawObject::new().field("name", "Melman").build())
        })
        .as_abstract(node);
    });
    let schema = schema.build().expect("schema builds");
    let response = run(&schema, "{ alias: node { name } }");
    assert!(response.has_errors(), "expected an error: {response:?}");
    let path: Vec<String> = response.errors[0]
        .path()
        .expect("abstract resolution error should have a path")
        .iter()
        .map(ToString::to_string)
        .collect();
    assert_eq!(path, ["alias"]);
}

#[test]
fn a_normal_model_resolver_can_return_a_raw_subtree() {
    #[derive(Object)]
    struct Parent {
        label: String,
    }
    let mut schema = Schema::<Ctx>::new();
    schema.object::<Parent>(|o| {
        o.expose(expose!(Parent::label));
        o.field("child", |_: &Parent, _: NoArgs, _: &Ctx| {
            Raw::<Giraffe>::object(
                RawObject::new()
                    .field("name", "Melman")
                    .field("height", 3.4)
                    .build(),
            )
        });
    });
    schema.object::<Giraffe>(|o| {
        o.raw_field::<String>("name");
        o.raw_field::<f32>("height");
    });
    schema.query(|q| {
        q.field("parent", |_: NoArgs, _: &Ctx| Parent { label: "p".into() });
    });
    let schema = schema.build().expect("schema builds");
    let response = run(&schema, "{ parent { label child { name height } } }");
    assert!(!response.has_errors(), "unexpected errors: {response:?}");
    let parent = field(&response, "parent")
        .as_object()
        .expect("parent is an object");
    assert_eq!(
        parent
            .get("child")
            .and_then(Value::as_object)
            .and_then(|child| child.get("name"))
            .and_then(Value::as_str),
        Some("Melman")
    );
}

#[test]
fn a_raw_object_field_can_be_declared_abstract() {
    #[derive(Object)]
    struct Wrapper {
        node: Giraffe,
    }
    let mut schema = Schema::<Ctx>::new();
    let node = schema.interface("Node", |i| {
        i.field::<String>("name");
    });
    schema.object::<Giraffe>(|o| {
        o.implements(node);
        o.raw_field::<String>("name");
    });
    schema.object::<Wrapper>(|o| {
        o.raw_field::<Giraffe>("node").as_abstract(node);
    });
    schema.query(|q| {
        q.field("wrapper", |_: NoArgs, _: &Ctx| {
            Raw::<Wrapper>::object(
                RawObject::new()
                    .field("node", raw_giraffe_map("Melman").typename("Giraffe"))
                    .build(),
            )
        });
    });
    let schema = schema.build().expect("schema builds");
    let response = run(
        &schema,
        "{ wrapper { node { __typename name ... on Giraffe { name } } } }",
    );
    assert!(!response.has_errors(), "unexpected errors: {response:?}");
    assert_eq!(
        field(&response, "wrapper")
            .as_object()
            .and_then(|object| object.get("node"))
            .and_then(Value::as_object)
            .and_then(|node| node.get("name"))
            .and_then(Value::as_str),
        Some("Melman")
    );
}
