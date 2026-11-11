//! `expose!` uses a synchronous property resolver, and the options that need a future convert it
//! to the boxed path exactly when they are applied.

#![allow(dead_code)]

use std::sync::{Arc, Mutex};

use gurafi::{
    FieldMeta, GraphQLError, ID, NoArgs, Object, Plugin, Raw, Schema, TraceEvent, TracePhase,
    TracingPlugin, expose,
};

#[derive(Clone, Debug)]
struct Ctx {
    logged_in: bool,
    allow_validation: bool,
}

impl Ctx {
    fn allowed() -> Self {
        Self {
            logged_in: true,
            allow_validation: true,
        }
    }

    fn denied() -> Self {
        Self {
            logged_in: false,
            allow_validation: false,
        }
    }
}

#[derive(Clone, Object)]
struct Keeper {
    name: String,
}

#[derive(Clone, Object)]
struct Giraffe {
    name: String,
    height: f64,
    nickname: Option<String>,
    spots: Vec<i32>,
    keeper: Keeper,
}

fn giraffe() -> Giraffe {
    Giraffe {
        name: String::from("Melman"),
        height: 3.4,
        nickname: None,
        spots: vec![1, 2, 3],
        keeper: Keeper {
            name: String::from("Bob"),
        },
    }
}

fn run(schema: &gql::Schema, query: &str, ctx: Ctx) -> gql::Response {
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
        .and_then(gql::Value::as_object)
        .unwrap_or_else(|| panic!("missing response data: {response:?}"))
}

fn field<'a>(schema: &'a gql::Schema, ty: &str, name: &str) -> &'a gql::Field {
    schema
        .object_type(ty)
        .and_then(|object| object.get_field(name))
        .unwrap_or_else(|| panic!("missing field {ty}.{name}"))
}

fn base_schema() -> Schema<Ctx> {
    let mut schema = Schema::<Ctx>::new();
    schema.object::<Keeper>(|o| {
        o.expose(expose!(Keeper::name));
    });
    schema.object::<Giraffe>(|o| {
        o.expose(expose!(Giraffe::name));
        o.expose(expose!(Giraffe::height));
        o.expose(expose!(Giraffe::nickname));
        o.expose(expose!(Giraffe::spots));
        o.expose(expose!(Giraffe::keeper::name as "keeperName"));
    });
    schema.query(|q| {
        q.field("giraffe", |_: NoArgs, _: &Ctx| giraffe());
    });
    schema
}

#[test]
fn direct_and_nested_properties_use_the_synchronous_path() {
    let schema = base_schema().build().expect("schema builds");

    for (ty, name) in [
        ("Giraffe", "name"),
        ("Giraffe", "height"),
        ("Giraffe", "keeperName"),
        ("Keeper", "name"),
    ] {
        let field = field(&schema, ty, name);
        assert!(
            field.sync_resolve.is_some(),
            "{ty}.{name} must use the synchronous property resolver"
        );
        assert!(
            field.resolve.is_none(),
            "{ty}.{name} must not install a boxed resolver"
        );
    }

    let response = run(
        &schema,
        "{ giraffe { name height nickname keeperName spots } }",
        Ctx::allowed(),
    );
    assert!(
        response.errors.is_empty(),
        "unexpected errors: {response:?}"
    );
    let giraffe = data(&response)
        .get("giraffe")
        .and_then(gql::Value::as_object)
        .expect("giraffe object");
    assert_eq!(giraffe.get("name"), Some(&gql::Value::from("Melman")));
    assert_eq!(giraffe.get("keeperName"), Some(&gql::Value::from("Bob")));
    assert_eq!(giraffe.get("nickname"), Some(&gql::Value::Null));
    let spots = giraffe.get("spots").and_then(gql::Value::as_list).unwrap();
    assert_eq!(spots.len(), 3);
}

#[test]
fn optional_and_list_properties_resolve_through_the_sync_hook() {
    let mut schema = Schema::<Ctx>::new();
    schema.object::<Giraffe>(|o| {
        o.expose(expose!(Giraffe::nickname));
        o.expose(expose!(Giraffe::spots));
    });
    let mut model = giraffe();
    model.nickname = Some(String::from("Mel"));
    let model = Arc::new(model);
    schema.query(|q| {
        let model = Arc::clone(&model);
        q.field("giraffe", move |_: NoArgs, _: &Ctx| (*model).clone());
    });
    let schema = schema.build().expect("schema builds");

    let response = run(&schema, "{ giraffe { nickname spots } }", Ctx::allowed());
    assert!(
        response.errors.is_empty(),
        "unexpected errors: {response:?}"
    );
    let giraffe = data(&response)
        .get("giraffe")
        .and_then(gql::Value::as_object)
        .expect("giraffe object");
    assert_eq!(giraffe.get("nickname"), Some(&gql::Value::from("Mel")));
    assert_eq!(
        giraffe.get("spots").and_then(gql::Value::as_list).unwrap(),
        &[gql::Value::Int(1), gql::Value::Int(2), gql::Value::Int(3)]
    );
}

#[test]
fn metadata_overrides_and_type_overrides_stay_on_the_synchronous_path() {
    let mut schema = Schema::<Ctx>::new();
    schema.directive(
        gql::DirectiveDefinition::new("tag")
            .location(gql::ast::DirectiveLocation::FieldDefinition)
            .arg(gql::InputValue::new("name", gql::TypeRef::string())),
    );
    schema.object::<Giraffe>(|o| {
        o.expose(expose!(Giraffe::name))
            .description("The name.")
            .deprecated("Use keeperName.")
            .complexity(2)
            .directive(gql::AppliedDirective::new("tag").argument("name", "animal"))
            .returns::<ID>();
        o.expose(expose!(Giraffe::height)).nullable();
        // `Vec<i32>` is `[Int!]!`; loosen the items and keep the field non-null.
        o.expose(expose!(Giraffe::spots)).nullable_items();
        o.expose(expose!(Giraffe::nickname))
            .as_dynamic(gql::TypeRef::string());
    });
    schema.query(|q| {
        q.field("giraffe", |_: NoArgs, _: &Ctx| giraffe());
    });
    let schema = schema.build().expect("schema builds");

    let name = field(&schema, "Giraffe", "name");
    assert!(name.resolve.is_none() && name.sync_resolve.is_some());
    assert_eq!(name.ty.to_string(), "ID!");
    assert_eq!(name.description.as_deref(), Some("The name."));
    assert_eq!(name.deprecation_reason.as_deref(), Some("Use keeperName."));
    assert!(name.complexity.is_some());
    assert_eq!(name.directives.len(), 1);

    let height = field(&schema, "Giraffe", "height");
    assert!(height.resolve.is_none() && height.sync_resolve.is_some());
    assert_eq!(height.ty.to_string(), "Float");

    let spots = field(&schema, "Giraffe", "spots");
    assert!(spots.resolve.is_none() && spots.sync_resolve.is_some());
    assert_eq!(spots.ty.to_string(), "[Int]!");

    let nickname = field(&schema, "Giraffe", "nickname");
    assert!(nickname.resolve.is_none() && nickname.sync_resolve.is_some());
    assert_eq!(nickname.ty.to_string(), "String");

    let response = run(
        &schema,
        "{ giraffe { name height spots nickname } }",
        Ctx::allowed(),
    );
    assert!(
        response.errors.is_empty(),
        "unexpected errors: {response:?}"
    );
    let giraffe = data(&response)
        .get("giraffe")
        .and_then(gql::Value::as_object)
        .expect("giraffe object");
    assert_eq!(giraffe.get("name"), Some(&gql::Value::from("Melman")));
    assert_eq!(
        giraffe
            .get("spots")
            .and_then(gql::Value::as_list)
            .unwrap()
            .len(),
        3
    );
}

#[test]
fn field_auth_converts_to_the_boxed_path() {
    let mut schema = Schema::<Ctx>::new();
    schema.object::<Giraffe>(|o| {
        o.expose(expose!(Giraffe::name))
            .auth(|ctx: &Ctx| ctx.logged_in);
        o.expose(expose!(Giraffe::height));
    });
    schema.query(|q| {
        q.field("giraffe", |_: NoArgs, _: &Ctx| giraffe());
    });
    let schema = schema.build().expect("schema builds");

    let name = field(&schema, "Giraffe", "name");
    assert!(name.resolve.is_some(), "auth must install a boxed resolver");
    assert!(name.sync_resolve.is_none());
    let height = field(&schema, "Giraffe", "height");
    assert!(
        height.resolve.is_none() && height.sync_resolve.is_some(),
        "fields without auth must keep the synchronous path"
    );

    let denied = run(&schema, "{ giraffe { name height } }", Ctx::denied());
    assert_eq!(denied.errors.len(), 1, "unexpected errors: {denied:?}");
    assert_eq!(
        data(&denied).get("giraffe"),
        Some(&gql::Value::Null),
        "a denied non-null field must null the whole object"
    );

    let allowed = run(&schema, "{ giraffe { name } }", Ctx::allowed());
    assert!(allowed.errors.is_empty(), "unexpected errors: {allowed:?}");
}

#[test]
fn async_auth_converts_to_the_boxed_path() {
    let mut schema = Schema::<Ctx>::new();
    schema.object::<Giraffe>(|o| {
        o.expose(expose!(Giraffe::name)).auth_async(|ctx: &Ctx| {
            let allowed = ctx.logged_in;
            Box::pin(async move { allowed })
        });
    });
    schema.query(|q| {
        q.field("giraffe", |_: NoArgs, _: &Ctx| giraffe());
    });
    let schema = schema.build().expect("schema builds");

    let name = field(&schema, "Giraffe", "name");
    assert!(name.resolve.is_some() && name.sync_resolve.is_none());
    let response = run(&schema, "{ giraffe { name } }", Ctx::allowed());
    assert!(
        response.errors.is_empty(),
        "unexpected errors: {response:?}"
    );
}

#[test]
fn validation_converts_to_the_boxed_path() {
    let mut schema = Schema::<Ctx>::new();
    schema.object::<Giraffe>(|o| {
        o.expose(expose!(Giraffe::name)).validate::<()>(|(), ctx| {
            if ctx.allow_validation {
                Ok(())
            } else {
                Err(GraphQLError::new("validation denied"))
            }
        });
    });
    schema.query(|q| {
        q.field("giraffe", |_: NoArgs, _: &Ctx| giraffe());
    });
    let schema = schema.build().expect("schema builds");

    let name = field(&schema, "Giraffe", "name");
    assert!(
        name.resolve.is_some(),
        "validation must install a boxed resolver"
    );
    assert!(name.sync_resolve.is_none());

    let allowed = run(&schema, "{ giraffe { name } }", Ctx::allowed());
    assert!(allowed.errors.is_empty(), "unexpected errors: {allowed:?}");

    let denied = run(&schema, "{ giraffe { name } }", Ctx::denied());
    assert_eq!(denied.errors.len(), 1, "unexpected errors: {denied:?}");
    assert!(
        denied.errors[0].message().contains("validation denied"),
        "unexpected message: {:?}",
        denied.errors[0].message()
    );
}

#[test]
fn metadata_only_plugins_keep_the_synchronous_path() {
    struct Describe;
    impl Plugin<Ctx> for Describe {
        fn field(&self, meta: &FieldMeta<'_>, field: &mut gql::Field) {
            if !meta.is_root {
                field.description = Some("touched".into());
            }
        }
    }

    let mut schema = base_schema();
    schema.plugin(Describe);
    let schema = schema.build().expect("schema builds");

    let name = field(&schema, "Giraffe", "name");
    assert!(
        name.resolve.is_none() && name.sync_resolve.is_some(),
        "a metadata-only plugin must not disable the synchronous path"
    );
    assert_eq!(name.description.as_deref(), Some("touched"));
    let response = run(&schema, "{ giraffe { name } }", Ctx::allowed());
    assert!(
        response.errors.is_empty(),
        "unexpected errors: {response:?}"
    );
}

#[test]
fn wrapping_plugins_convert_to_the_boxed_path() {
    let seen = Arc::new(Mutex::new(Vec::new()));
    let observer = Arc::clone(&seen);
    let mut schema = base_schema();
    schema.plugin(TracingPlugin::new(move |event: &TraceEvent<'_>| {
        if event.phase == TracePhase::Start {
            observer
                .lock()
                .unwrap()
                .push(format!("{}.{}", event.type_name, event.field_name));
        }
    }));
    let schema = schema.build().expect("schema builds");

    let name = field(&schema, "Giraffe", "name");
    assert!(
        name.resolve.is_some() && name.sync_resolve.is_none(),
        "a resolver wrapper must materialize the boxed path"
    );

    let response = run(&schema, "{ giraffe { name } }", Ctx::allowed());
    assert!(
        response.errors.is_empty(),
        "unexpected errors: {response:?}"
    );
    let seen = seen.lock().unwrap();
    assert!(
        seen.iter().any(|entry| entry == "Giraffe.name"),
        "the wrapper did not observe the field: {seen:?}"
    );
}

#[test]
fn interfaces_keep_the_synchronous_path() {
    let mut schema = Schema::<Ctx>::new();
    let node = schema.interface("Node", |i| {
        i.field::<ID>("id");
        i.field::<String>("name");
    });
    schema.object::<Giraffe>(|o| {
        o.implements(&node);
        o.field("id", |g: &Giraffe, _: NoArgs, _: &Ctx| {
            ID::new(g.name.clone())
        });
        o.expose(expose!(Giraffe::name));
        o.expose(expose!(Giraffe::height));
    });
    schema.object::<Keeper>(|o| {
        o.expose(expose!(Keeper::name));
    });
    schema.query(|q| {
        q.field("node", |_: NoArgs, _: &Ctx| giraffe())
            .as_abstract(&node);
    });
    let schema = schema.build().expect("schema builds");

    let name = field(&schema, "Giraffe", "name");
    assert!(name.resolve.is_none() && name.sync_resolve.is_some());
    let response = run(
        &schema,
        "{ node { id name ... on Giraffe { height } } }",
        Ctx::allowed(),
    );
    assert!(
        response.errors.is_empty(),
        "unexpected errors: {response:?}"
    );
    let node = data(&response)
        .get("node")
        .and_then(gql::Value::as_object)
        .expect("node object");
    assert_eq!(node.get("name"), Some(&gql::Value::from("Melman")));
    assert_eq!(node.get("height"), Some(&gql::Value::from(3.4)));
}

#[test]
fn field_ref_and_expose_coexist() {
    let mut schema = Schema::<Ctx>::new();
    let keeper = schema.object_ref::<Keeper>("Keeper");
    schema.implement(&keeper, |o| {
        o.expose(expose!(Keeper::name));
    });
    schema.object::<Giraffe>(|o| {
        o.expose(expose!(Giraffe::name));
        o.field_ref(&keeper, "keeper", |g: &Giraffe, _: NoArgs, _: &Ctx| {
            g.keeper.clone()
        });
    });
    schema.query(|q| {
        q.field("giraffe", |_: NoArgs, _: &Ctx| giraffe());
    });
    let schema = schema.build().expect("schema builds");

    assert!(field(&schema, "Giraffe", "name").sync_resolve.is_some());
    let keeper_field = field(&schema, "Giraffe", "keeper");
    assert!(keeper_field.resolve.is_some() && keeper_field.sync_resolve.is_none());

    let response = run(
        &schema,
        "{ giraffe { name keeper { name } } }",
        Ctx::allowed(),
    );
    assert!(
        response.errors.is_empty(),
        "unexpected errors: {response:?}"
    );
    let giraffe = data(&response)
        .get("giraffe")
        .and_then(gql::Value::as_object)
        .expect("giraffe object");
    assert_eq!(
        giraffe
            .get("keeper")
            .and_then(gql::Value::as_object)
            .and_then(|keeper| keeper.get("name")),
        Some(&gql::Value::from("Bob"))
    );
}

#[test]
fn build_clone_keeps_the_synchronous_path() {
    let schema = base_schema();
    let first = schema.build_clone().expect("first build");
    let second = schema.build_clone().expect("second build");

    for built in [&first, &second] {
        let name = field(built, "Giraffe", "name");
        assert!(name.resolve.is_none() && name.sync_resolve.is_some());
    }

    for built in [&first, &second] {
        let response = run(built, "{ giraffe { name keeperName } }", Ctx::allowed());
        assert!(
            response.errors.is_empty(),
            "unexpected errors: {response:?}"
        );
    }
}

#[test]
fn raw_fields_have_neither_resolver() {
    let mut schema = Schema::<Ctx>::new();
    schema.object::<Keeper>(|o| {
        o.raw_field::<String>("name");
    });
    schema.query(|q| {
        q.field("keeper", |_: NoArgs, _: &Ctx| {
            Raw::<Keeper>::object(gurafi::RawObject::new().field("name", "Bob").build())
        });
    });
    let schema = schema.build().expect("schema builds");

    let name = field(&schema, "Keeper", "name");
    assert!(name.resolve.is_none(), "raw_field must stay resolver-free");
    assert!(name.sync_resolve.is_none());

    let response = run(&schema, "{ keeper { name } }", Ctx::allowed());
    assert!(
        response.errors.is_empty(),
        "unexpected errors: {response:?}"
    );
    assert_eq!(
        data(&response)
            .get("keeper")
            .and_then(gql::Value::as_object)
            .and_then(|keeper| keeper.get("name")),
        Some(&gql::Value::from("Bob"))
    );
}
