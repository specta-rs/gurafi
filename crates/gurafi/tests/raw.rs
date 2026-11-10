//! Tests for resolving a whole object through [`gurafi::Raw`].

#![allow(dead_code)]

use gurafi::{GraphQLError, ID, Input, NoArgs, Object, Raw, RawObject, Schema, Value, expose};

struct Ctx;

#[derive(Input)]
struct PictureArgs {
    width: i32,
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

#[derive(Object)]
struct Classified {
    name: String,
}

#[derive(Object)]
struct Secret {
    value: String,
    visible: String,
}

impl Giraffe {
    fn sample(name: &str, height: f32) -> Self {
        Self {
            name: name.into(),
            height,
            nickname: None,
            keeper: Keeper { name: "Bob".into() },
        }
    }
}

fn raw_giraffe(name: &str, height: f32) -> Raw<Giraffe> {
    Raw::object(
        RawObject::new()
            .field("id", name.to_ascii_lowercase())
            .field("name", name)
            .field("height", height)
            .field("keeper", RawObject::new().field("name", "Bob"))
            .build(),
    )
}

fn raw_interface_giraffe(name: &str, height: f32) -> Raw<Giraffe> {
    Raw::object(
        RawObject::new()
            .typename("Giraffe")
            .field("id", name.to_ascii_lowercase())
            .field("name", name)
            .field("height", height)
            .build(),
    )
}

fn build_raw_schema() -> gql::Schema {
    let mut schema = Schema::<Ctx>::new();

    let node = schema.interface("Node", |i| {
        i.field::<ID>("id");
        i.field::<String>("name");
    });

    schema.object::<Keeper>(|o| {
        o.raw_field::<String>("name");
    });

    schema.object::<Giraffe>(|o| {
        o.implements(node);
        o.raw_field::<ID>("id");
        o.raw_field::<String>("name");
        o.raw_field::<f32>("height");
        o.raw_field::<Option<String>>("nickname");
        o.raw_field::<Keeper>("keeper");
    });

    schema.object::<User>(|o| {
        o.raw_field::<String>("name");
    });
    schema.object::<Post>(|o| {
        o.raw_field::<String>("title");
    });

    let search = schema.union("SearchResult", |u| {
        u.member::<User>().member::<Post>();
        u.resolve_type(|value, _| {
            let object = value
                .as_object()
                .ok_or_else(|| GraphQLError::new("search result is not an object"))?;
            let name = object
                .get("__typename")
                .or_else(|| object.get("kind"))
                .and_then(Value::as_str)
                .ok_or_else(|| GraphQLError::new("search result has no type marker"))?;
            Ok(name.into())
        });
    });

    schema.query(|q| {
        q.field("giraffe", |_: NoArgs, _: &Ctx| raw_giraffe("Melman", 3.4));

        q.field("giraffes", |_: NoArgs, _: &Ctx| {
            Raw::<Vec<Giraffe>>::list([raw_giraffe("Melman", 3.4), raw_giraffe("Karen", 3.9)])
        });

        q.field("nullableGiraffe", |_: NoArgs, _: &Ctx| {
            Raw::<Option<Giraffe>>::null()
        });

        q.field("scalar", |_: NoArgs, _: &Ctx| {
            Raw::<String>::new(Value::String("hello".into()))
        });

        q.field("node", |_: NoArgs, _: &Ctx| {
            raw_interface_giraffe("Melman", 3.4)
        })
        .as_abstract(node);

        q.field("search", |_: NoArgs, _: &Ctx| {
            Raw::<User>::object(
                RawObject::new()
                    .typename("User")
                    .field("name", "Oscar")
                    .build(),
            )
        })
        .as_abstract(search);

        // No `__typename`: the union's `resolve_type` reads the extra `kind` field instead.
        q.field("searchByKind", |_: NoArgs, _: &Ctx| {
            Raw::<Post>::object(
                RawObject::new()
                    .field("kind", "Post")
                    .field("title", "Hello raw")
                    .build(),
            )
        })
        .as_abstract(search);

        // A declared object whose value is not an object: the executor reports a field error.
        q.field("wrongShape", |_: NoArgs, _: &Ctx| {
            Raw::<Giraffe>::new(Value::String("not an object".into()))
        });

        // A non-null field is missing from the map: normal null propagation applies.
        q.field("missingName", |_: NoArgs, _: &Ctx| {
            Raw::<Giraffe>::object(RawObject::new().field("height", 3.4).build())
        });
    });

    schema.build().expect("raw schema builds")
}

fn build_auth_schema() -> gql::Schema {
    let mut schema = Schema::<Ctx>::new();

    schema.object::<Classified>(|o| {
        o.auth(|_: &Ctx| false);
        o.raw_field::<String>("name");
    });

    schema.object::<Secret>(|o| {
        o.raw_field::<String>("value").auth(|_: &Ctx| false);
        o.raw_field::<String>("visible");
    });

    schema.query(|q| {
        q.field("classified", |_: NoArgs, _: &Ctx| {
            Raw::<Classified>::object(RawObject::new().field("name", "classified").build())
        });

        q.field("secret", |_: NoArgs, _: &Ctx| {
            Raw::<Secret>::object(
                RawObject::new()
                    .field("value", "hidden")
                    .field("visible", "public")
                    .build(),
            )
        });
    });

    schema.build().expect("auth schema builds")
}

fn build_resolved_schema() -> gql::Schema {
    let mut schema = Schema::<Ctx>::new();

    schema.object::<Keeper>(|o| {
        o.expose(expose!(Keeper::name));
    });

    schema.object::<Giraffe>(|o| {
        o.expose(expose!(Giraffe::name));
        o.expose(expose!(Giraffe::height));
        o.expose(expose!(Giraffe::nickname));
        o.expose(expose!(Giraffe::keeper::name as "keeperName"));
        o.field("keeper", |g: &Giraffe, _: NoArgs, _: &Ctx| g.keeper.clone());
    });

    schema.query(|q| {
        q.field("giraffe", |_: NoArgs, _: &Ctx| {
            Giraffe::sample("Melman", 3.4)
        });

        q.field("giraffes", |_: NoArgs, _: &Ctx| {
            vec![
                Giraffe::sample("Melman", 3.4),
                Giraffe::sample("Karen", 3.9),
            ]
        });

        q.field("nullableGiraffe", |_: NoArgs, _: &Ctx| None::<Giraffe>);
    });

    schema.build().expect("resolved schema builds")
}

fn run(schema: &gql::Schema, query: &str) -> gql::Response {
    futures::executor::block_on(gql::execute(
        schema,
        gql::Request::new(query),
        &gql::ExecutionOptions::with_context(Ctx),
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

#[test]
fn raw_fields_have_no_resolvers() {
    let schema = build_raw_schema();
    let giraffe = schema
        .object_type("Giraffe")
        .expect("Giraffe is registered");
    assert!(!giraffe.fields.is_empty());
    for field in giraffe.fields.values() {
        assert!(
            field.resolve.is_none(),
            "raw field `{}` should not have a resolver",
            field.name
        );
    }
}

#[test]
fn raw_object_resolves_without_field_resolvers() {
    let schema = build_raw_schema();
    let response = run(
        &schema,
        "{ giraffe { __typename name height nickname keeper { name } } }",
    );
    assert!(!response.has_errors(), "unexpected errors: {response:?}");

    let giraffe = field(&response, "giraffe")
        .as_object()
        .expect("giraffe is an object");
    assert_eq!(
        giraffe.get("__typename").and_then(Value::as_str),
        Some("Giraffe")
    );
    assert_eq!(giraffe.get("name").and_then(Value::as_str), Some("Melman"));
    assert_eq!(giraffe.get("nickname"), Some(&Value::Null));
    assert_eq!(
        giraffe
            .get("keeper")
            .and_then(Value::as_object)
            .and_then(|keeper| keeper.get("name"))
            .and_then(Value::as_str),
        Some("Bob")
    );
}

#[test]
fn raw_list_of_objects_resolves() {
    let schema = build_raw_schema();
    let response = run(&schema, "{ giraffes { name } }");
    assert!(!response.has_errors(), "unexpected errors: {response:?}");

    let names: Vec<_> = field(&response, "giraffes")
        .as_list()
        .expect("giraffes is a list")
        .iter()
        .map(|giraffe| {
            giraffe
                .as_object()
                .and_then(|giraffe| giraffe.get("name"))
                .and_then(Value::as_str)
                .expect("name")
        })
        .collect();
    assert_eq!(names, ["Melman", "Karen"]);
}

#[test]
fn raw_values_keep_the_type_declared_by_t() {
    let schema = build_raw_schema();

    let giraffe = schema
        .query_type()
        .fields
        .get("giraffe")
        .map(|field| field.ty.clone())
        .expect("giraffe field exists");
    assert_eq!(giraffe, gql::TypeRef::named("Giraffe"));

    let giraffes = schema
        .query_type()
        .fields
        .get("giraffes")
        .map(|field| field.ty.clone())
        .expect("giraffes field exists");
    assert_eq!(giraffes, gql::TypeRef::named("Giraffe").list().non_null());

    let nullable = schema
        .query_type()
        .fields
        .get("nullableGiraffe")
        .map(|field| field.ty.clone())
        .expect("nullableGiraffe field exists");
    assert_eq!(nullable, gql::TypeRef::named("Giraffe"));

    let scalar = schema
        .query_type()
        .fields
        .get("scalar")
        .map(|field| field.ty.clone())
        .expect("scalar field exists");
    assert_eq!(scalar, gql::TypeRef::string().non_null());
}

#[test]
fn raw_object_under_an_interface_uses_typename() {
    let schema = build_raw_schema();
    let response = run(
        &schema,
        "{ node { __typename id name ... on Giraffe { height } } }",
    );
    assert!(!response.has_errors(), "unexpected errors: {response:?}");

    let node = field(&response, "node")
        .as_object()
        .expect("node is an object");
    assert_eq!(
        node.get("__typename").and_then(Value::as_str),
        Some("Giraffe")
    );
    assert_eq!(node.get("id").and_then(Value::as_str), Some("melman"));
    assert_eq!(node.get("name").and_then(Value::as_str), Some("Melman"));
    let height = node
        .get("height")
        .and_then(Value::as_f64)
        .expect("height is a float");
    assert!((height - 3.4).abs() < 0.001);
}

#[test]
fn raw_object_under_a_union_uses_typename_or_resolve_type() {
    let schema = build_raw_schema();

    let by_typename = run(
        &schema,
        "{ search { __typename ... on User { name } ... on Post { title } } }",
    );
    assert!(
        !by_typename.has_errors(),
        "unexpected errors: {by_typename:?}"
    );
    let search = field(&by_typename, "search")
        .as_object()
        .expect("search is an object");
    assert_eq!(
        search.get("__typename").and_then(Value::as_str),
        Some("User")
    );
    assert_eq!(search.get("name").and_then(Value::as_str), Some("Oscar"));

    let by_resolver = run(
        &schema,
        "{ searchByKind { __typename ... on User { name } ... on Post { title } } }",
    );
    assert!(
        !by_resolver.has_errors(),
        "unexpected errors: {by_resolver:?}"
    );
    let post = field(&by_resolver, "searchByKind")
        .as_object()
        .expect("post is an object");
    assert_eq!(post.get("__typename").and_then(Value::as_str), Some("Post"));
    assert_eq!(post.get("title").and_then(Value::as_str), Some("Hello raw"));
}

#[test]
fn non_object_raw_value_is_an_execution_error() {
    let schema = build_raw_schema();
    let response = run(&schema, "{ wrongShape { name } }");
    assert!(response.has_errors(), "expected an error: {response:?}");
    assert!(
        response
            .errors
            .iter()
            .any(|error| error.message().contains("expects an object")),
        "unexpected errors: {response:?}"
    );
    assert_eq!(field(&response, "wrongShape"), &Value::Null);
}

#[test]
fn missing_non_null_field_nulls_the_raw_object() {
    let schema = build_raw_schema();
    let response = run(&schema, "{ missingName { name } }");
    assert!(response.has_errors(), "expected an error: {response:?}");
    assert!(
        response
            .errors
            .iter()
            .any(|error| error.message().contains("Cannot return null")),
        "unexpected errors: {response:?}"
    );
    assert_eq!(field(&response, "missingName"), &Value::Null);
}

#[test]
fn raw_and_per_field_resolution_produce_the_same_response() {
    let raw_schema = build_raw_schema();
    let resolved_schema = build_resolved_schema();
    let query = "{ giraffe { name height nickname keeper { name } } }";

    let raw = run(&raw_schema, query);
    let resolved = run(&resolved_schema, query);
    assert!(!raw.has_errors(), "raw errors: {raw:?}");
    assert!(!resolved.has_errors(), "resolved errors: {resolved:?}");
    assert_eq!(
        field(&raw, "giraffe"),
        field(&resolved, "giraffe"),
        "raw and per-field resolution should agree"
    );
}

#[test]
fn authorization_still_applies_to_raw_fields() {
    let schema = build_auth_schema();

    // Object-level rule on a raw object.
    let classified = run(&schema, "{ classified { name } }");
    assert!(classified.has_errors(), "expected a denial: {classified:?}");
    assert!(
        classified
            .errors
            .iter()
            .any(|error| error.message().contains("Not authorized")),
        "unexpected errors: {classified:?}"
    );
    assert_eq!(field(&classified, "classified"), &Value::Null);

    // Field-level rule on one raw field; the unguarded sibling still resolves.
    let secret = run(&schema, "{ secret { value visible } }");
    assert!(secret.has_errors(), "expected a denial: {secret:?}");
    assert!(
        secret
            .errors
            .iter()
            .any(|error| error.message().contains("Not authorized")),
        "unexpected errors: {secret:?}"
    );
    assert_eq!(field(&secret, "secret"), &Value::Null);
}

#[test]
fn raw_object_insert_reports_the_replaced_value_and_len() {
    let mut object = RawObject::new()
        .field("name", "Melman")
        .field("height", 3.4);
    assert_eq!(object.len(), 2);
    assert!(!object.is_empty());

    assert_eq!(
        object.insert("name", "Karen"),
        Some(Value::String("Melman".into())),
        "insert returns the value it replaced"
    );
    assert_eq!(
        object.insert("nickname", "K"),
        None,
        "insert returns None for a new field"
    );
    assert_eq!(object.len(), 3);
    assert_eq!(object.get("name").and_then(Value::as_str), Some("Karen"));
}

#[test]
fn reserved_internal_keys_are_ignored_and_stripped() {
    let mut object = RawObject::new()
        .field("__gql_typename", "Post")
        .field("name", "Oscar");
    assert_eq!(object.len(), 1);
    assert!(object.get("__gql_typename").is_none());
    assert_eq!(
        object.insert("__gql_typename", "Post"),
        None,
        "a reserved key is never inserted"
    );

    // `Raw::new` also sanitizes values that were not built through `RawObject`, at any depth.
    let raw = Raw::<Giraffe>::new(Value::object(gql::Object::from_fields([
        ("__gql_typename", Value::from("Post")),
        (
            "keeper",
            Value::object(gql::Object::from_fields([
                ("__gql_typename", Value::from("Post")),
                ("name", Value::from("Bob")),
            ])),
        ),
        (
            "nickname",
            Value::List(vec![Value::object(gql::Object::from_fields([(
                "__gql_typename",
                Value::from("Post"),
            )]))]),
        ),
    ])));

    let Value::Object(object) = raw.raw_value() else {
        panic!("raw value is an object");
    };
    assert!(!object.contains_key("__gql_typename"));
    let keeper = object
        .get("keeper")
        .and_then(Value::as_object)
        .expect("keeper is an object");
    assert!(!keeper.contains_key("__gql_typename"));
    assert_eq!(keeper.get("name").and_then(Value::as_str), Some("Bob"));
    let nickname = object
        .get("nickname")
        .and_then(Value::as_list)
        .and_then(|items| items.first())
        .and_then(Value::as_object)
        .expect("nickname item is an object");
    assert!(!nickname.contains_key("__gql_typename"));
}

#[test]
fn a_raw_abstract_value_cannot_select_its_type_with_the_internal_key() {
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
            // A value decoded straight from untrusted JSON, not built through `RawObject`.
            Raw::<Post>::new(Value::object(gql::Object::from_fields([
                ("__gql_typename", Value::from("User")),
                ("title", Value::from("attacker chosen")),
            ])))
        })
        .as_abstract(search);
    });
    let schema = schema.build().expect("schema builds");

    let response = run(&schema, "{ search { __typename ... on User { name } } }");
    assert!(
        response.has_errors(),
        "an internal key is not a type marker: {response:?}"
    );
    assert!(
        response
            .errors
            .iter()
            .any(|error| error.message().contains("could not determine")),
        "unexpected errors: {response:?}"
    );
}

#[test]
fn raw_field_with_coerces_arguments_for_a_concrete_object_field() {
    #[derive(Object)]
    struct Picture {
        url: String,
    }

    let mut schema = Schema::<Ctx>::new();
    schema.object::<Picture>(|o| {
        o.raw_field_with::<String, PictureArgs>("url")
            .validate::<PictureArgs>(|args, _| {
                if args.width > 0 {
                    Ok(())
                } else {
                    Err(GraphQLError::new("width must be positive"))
                }
            });
    });
    schema.query(|q| {
        q.field("picture", |_: NoArgs, _: &Ctx| {
            Raw::<Picture>::object(
                RawObject::new()
                    .field("url", "wide.jpg")
                    .field("width", 999)
                    .build(),
            )
        });
    });
    let schema = schema.build().expect("schema builds");

    let ok = run(&schema, "{ picture { url(width: 1280) } }");
    assert!(!ok.has_errors(), "unexpected errors: {ok:?}");
    assert_eq!(
        field(&ok, "picture")
            .as_object()
            .and_then(|picture| picture.get("url"))
            .and_then(Value::as_str),
        Some("wide.jpg"),
        "the object map supplies the value; the argument is only coerced"
    );

    let denied = run(&schema, "{ picture { url(width: 0) } }");
    assert!(
        denied.has_errors(),
        "expected a validation error: {denied:?}"
    );
    assert!(
        denied
            .errors
            .iter()
            .any(|error| error.message().contains("width must be positive")),
        "the raw_field_with validator should run: {denied:?}"
    );
}

#[test]
fn raw_field_with_satisfies_an_interface_field_with_arguments() {
    let mut schema = Schema::<Ctx>::new();
    let node = schema.interface("Node", |i| {
        i.field::<String>("name");
        i.field_with::<String, PictureArgs>("picture");
    });
    schema.object::<Giraffe>(|o| {
        o.implements(node);
        o.raw_field::<String>("name");
        o.raw_field_with::<String, PictureArgs>("picture");
    });
    schema.query(|q| {
        q.field("node", |_: NoArgs, _: &Ctx| {
            Raw::<Giraffe>::object(
                RawObject::new()
                    .typename("Giraffe")
                    .field("name", "Melman")
                    .field("picture", "wide.jpg")
                    .build(),
            )
        })
        .as_abstract(node);
    });
    let schema = schema.build().expect("schema builds");

    let response = run(&schema, "{ node { picture(width: 1280) } }");
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
fn validate_on_a_raw_field_is_a_build_error() {
    let mut schema = Schema::<Ctx>::new();
    schema.object::<Giraffe>(|o| {
        o.raw_field::<String>("name")
            .validate::<NoArgs>(|_, _| Ok(()));
    });
    schema.query(|q| {
        q.field("giraffe", |_: NoArgs, _: &Ctx| {
            Raw::<Giraffe>::object(RawObject::new().field("name", "Melman").build())
        });
    });
    let error = schema
        .build()
        .expect_err("validate cannot run on a resolver-less raw field");
    assert!(
        error.to_string().contains("raw_field_with"),
        "the error should point at the supported alternative: {error}"
    );
}

#[test]
fn mixing_raw_and_resolver_fields_is_a_build_error() {
    #[derive(Object)]
    struct Mixed {
        name: String,
        height: f32,
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
        .expect_err("mixing raw and resolver fields is rejected");
    assert!(
        error.to_string().contains("mixes raw fields"),
        "unexpected error: {error}"
    );
}
