//! Review probes for `expose!`'s synchronous property resolver interacting with panic isolation,
//! authorization, validation, plugins and composition.
//!
//! Every probe observes behaviour through the public API; none of them edits library code.

#![allow(dead_code)]

use std::{
    panic::{AssertUnwindSafe, catch_unwind, set_hook, take_hook},
    sync::{
        Arc,
        atomic::{AtomicUsize, Ordering},
    },
};

use gurafi::{Expose, GraphQLError, NoArgs, Object, Schema, expose};

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
struct Thing {
    name: String,
    height: i32,
}

fn thing() -> Thing {
    Thing {
        name: String::from("Melman"),
        height: 3,
    }
}

fn run(schema: &gql::Schema, query: &str, ctx: Ctx) -> gql::Response {
    futures::executor::block_on(gql::execute(
        schema,
        gql::Request::new(query),
        &gql::ExecutionOptions::with_context(ctx),
    ))
}

fn field<'a>(schema: &'a gql::Schema, ty: &str, name: &str) -> &'a gql::Field {
    schema
        .object_type(ty)
        .and_then(|object| object.get_field(name))
        .unwrap_or_else(|| panic!("missing field {ty}.{name}"))
}

fn data(response: &gql::Response) -> &gql::Object {
    response
        .data
        .as_ref()
        .and_then(gql::Value::as_object)
        .unwrap_or_else(|| panic!("missing response data: {response:?}"))
}

fn code(error: &gql::GraphQLError) -> Option<&str> {
    error.extensions()?.get("code").and_then(gql::Value::as_str)
}

#[test]
fn an_expose_accessor_panic_is_a_field_error_not_an_unwind() {
    let mut schema = Schema::<Ctx>::new();
    schema.object::<Thing>(|o| {
        o.expose(expose!(Thing::name));
        o.expose(Expose::new("boom", |_: &Thing| -> Option<String> {
            panic!("expose accessor payload")
        }));
    });
    schema.query(|q| {
        q.field("thing", |_: NoArgs, _: &Ctx| thing());
    });
    let schema = schema.build().expect("schema builds");

    let response = run(&schema, "{ thing { boom name } }", Ctx::allowed());

    let thing = data(&response)
        .get("thing")
        .and_then(gql::Value::as_object)
        .expect("thing object");
    assert_eq!(thing.get("boom"), Some(&gql::Value::Null));
    assert_eq!(thing.get("name"), Some(&gql::Value::from("Melman")));
    assert_eq!(response.errors.len(), 1, "{response:?}");
    assert_eq!(response.errors[0].message(), "Internal server error");
    assert_eq!(code(&response.errors[0]), Some("INTERNAL_SERVER_ERROR"));
    assert_eq!(
        response.errors[0].path().map(<[_]>::to_vec),
        Some(vec![
            gql::PathSegment::field("thing"),
            gql::PathSegment::field("boom")
        ])
    );
    assert!(!response.errors[0].locations().is_empty());
}

#[test]
fn an_expose_accessor_panic_on_the_boxed_path_is_still_isolated() {
    // Authorization materializes the sync hook into a boxed resolver; the panic must be caught
    // there too, not only on the synchronous path.
    let mut schema = Schema::<Ctx>::new();
    schema.object::<Thing>(|o| {
        o.expose(Expose::new("boom", |_: &Thing| -> String {
            panic!("authorized accessor payload")
        }))
        .auth(|ctx: &Ctx| ctx.logged_in);
    });
    schema.query(|q| {
        q.field("thing", |_: NoArgs, _: &Ctx| thing());
    });
    let schema = schema.build().expect("schema builds");

    let boom = field(&schema, "Thing", "boom");
    assert!(boom.resolve.is_some() && boom.sync_resolve.is_none());

    let response = run(&schema, "{ thing { boom } }", Ctx::allowed());

    assert_eq!(response.errors.len(), 1, "{response:?}");
    assert_eq!(response.errors[0].message(), "Internal server error");
    assert_eq!(code(&response.errors[0]), Some("INTERNAL_SERVER_ERROR"));
}

#[test]
fn a_denied_exposed_field_does_not_run_the_accessor() {
    let calls = Arc::new(AtomicUsize::new(0));
    let observer = Arc::clone(&calls);
    let mut schema = Schema::<Ctx>::new();
    schema.object::<Thing>(|o| {
        o.expose(Expose::new("name", move |thing: &Thing| {
            observer.fetch_add(1, Ordering::Relaxed);
            thing.name.clone()
        }))
        .auth(|ctx: &Ctx| ctx.logged_in);
    });
    schema.query(|q| {
        q.field("thing", |_: NoArgs, _: &Ctx| thing());
    });
    let schema = schema.build().expect("schema builds");

    let response = run(&schema, "{ thing { name } }", Ctx::denied());

    assert_eq!(response.errors.len(), 1, "{response:?}");
    assert_eq!(response.errors[0].message(), "Not authorized");
    assert_eq!(
        calls.load(Ordering::Relaxed),
        0,
        "authorization must deny before the accessor runs"
    );
}

#[test]
fn a_failing_validator_does_not_run_the_accessor() {
    let calls = Arc::new(AtomicUsize::new(0));
    let observer = Arc::clone(&calls);
    let mut schema = Schema::<Ctx>::new();
    schema.object::<Thing>(|o| {
        o.expose(Expose::new("name", move |thing: &Thing| {
            observer.fetch_add(1, Ordering::Relaxed);
            thing.name.clone()
        }))
        .validate::<()>(|(), ctx| {
            if ctx.allow_validation {
                Ok(())
            } else {
                Err(GraphQLError::new("validation denied"))
            }
        });
    });
    schema.query(|q| {
        q.field("thing", |_: NoArgs, _: &Ctx| thing());
    });
    let schema = schema.build().expect("schema builds");

    let denied = run(&schema, "{ thing { name } }", Ctx::denied());
    assert_eq!(denied.errors.len(), 1, "{denied:?}");
    assert_eq!(denied.errors[0].message(), "validation denied");
    assert_eq!(
        calls.load(Ordering::Relaxed),
        0,
        "validation must run before the property read"
    );

    let allowed = run(&schema, "{ thing { name } }", Ctx::allowed());
    assert!(allowed.errors.is_empty(), "{allowed:?}");
    assert_eq!(calls.load(Ordering::Relaxed), 1);
}

#[test]
fn object_level_auth_materializes_exposed_fields() {
    let mut schema = Schema::<Ctx>::new();
    schema.object::<Thing>(|o| {
        o.auth(|ctx: &Ctx| ctx.logged_in);
        o.expose(expose!(Thing::name));
    });
    schema.query(|q| {
        q.field("thing", |_: NoArgs, _: &Ctx| thing());
    });
    let schema = schema.build().expect("schema builds");

    let name = field(&schema, "Thing", "name");
    assert!(
        name.resolve.is_some() && name.sync_resolve.is_none(),
        "object-level authorization must convert every affected field"
    );

    let denied = run(&schema, "{ thing { name } }", Ctx::denied());
    assert_eq!(denied.errors.len(), 1, "{denied:?}");
    assert_eq!(denied.errors[0].message(), "Not authorized");

    let allowed = run(&schema, "{ thing { name } }", Ctx::allowed());
    assert!(allowed.errors.is_empty(), "{allowed:?}");
    assert_eq!(
        data(&allowed)
            .get("thing")
            .and_then(gql::Value::as_object)
            .and_then(|thing| thing.get("name")),
        Some(&gql::Value::from("Melman"))
    );
}

#[test]
fn interface_level_auth_materializes_exposed_fields() {
    let mut schema = Schema::<Ctx>::new();
    let node = schema.interface("Node", |i| {
        i.field::<String>("name").auth(|ctx: &Ctx| ctx.logged_in);
    });
    schema.object::<Thing>(|o| {
        o.implements(&node);
        o.expose(expose!(Thing::name));
    });
    schema.query(|q| {
        q.field("thing", |_: NoArgs, _: &Ctx| thing())
            .as_abstract(&node);
    });
    let schema = schema.build().expect("schema builds");

    let name = field(&schema, "Thing", "name");
    assert!(
        name.resolve.is_some() && name.sync_resolve.is_none(),
        "interface authorization must be inherited as a boxed resolver"
    );

    let denied = run(&schema, "{ thing { name } }", Ctx::denied());
    assert_eq!(denied.errors.len(), 1, "{denied:?}");
    assert_eq!(denied.errors[0].message(), "Not authorized");
}

#[test]
fn a_metadata_only_plugin_after_a_wrapping_plugin_keeps_the_wrapped_resolver() {
    // Regression probe for the plugin restore path: when one plugin wraps and a later one does
    // not, the wrapped resolver must win and the sync hook must not come back.
    struct Wrap;
    impl gurafi::Plugin<Ctx> for Wrap {
        fn wrap_resolve(&self, _: &gurafi::FieldMeta<'_>, next: gql::ResolveFn) -> gql::ResolveFn {
            Arc::new(move |context| {
                let next = Arc::clone(&next);
                Box::pin(async move { next(context).await })
            })
        }
    }
    struct Describe;
    impl gurafi::Plugin<Ctx> for Describe {
        fn field(&self, meta: &gurafi::FieldMeta<'_>, field: &mut gql::Field) {
            if !meta.is_root {
                field.description = Some("described".into());
            }
        }
    }

    let mut schema = Schema::<Ctx>::new();
    schema.plugin(Wrap);
    schema.plugin(Describe);
    schema.object::<Thing>(|o| {
        o.expose(expose!(Thing::name));
    });
    schema.query(|q| {
        q.field("thing", |_: NoArgs, _: &Ctx| thing());
    });
    let schema = schema.build().expect("schema builds");

    let name = field(&schema, "Thing", "name");
    assert!(name.resolve.is_some(), "the wrapper must be installed");
    assert!(name.sync_resolve.is_none(), "the hook must not be restored");
    assert_eq!(name.description.as_deref(), Some("described"));

    let response = run(&schema, "{ thing { name } }", Ctx::allowed());
    assert!(response.errors.is_empty(), "{response:?}");
    assert_eq!(
        data(&response)
            .get("thing")
            .and_then(gql::Value::as_object)
            .and_then(|thing| thing.get("name")),
        Some(&gql::Value::from("Melman"))
    );
}

#[test]
fn extend_sdl_keeps_an_exposed_fields_sync_hook() {
    let mut schema = Schema::<Ctx>::new();
    schema.object::<Thing>(|o| {
        o.expose(expose!(Thing::name));
    });
    schema.query(|q| {
        q.field("thing", |_: NoArgs, _: &Ctx| thing());
    });
    let schema = schema
        .build()
        .expect("schema builds")
        .extend_sdl("extend type Thing { extra: String }")
        .expect("schema extends");

    let name = field(&schema, "Thing", "name");
    assert!(
        name.resolve.is_none() && name.sync_resolve.is_some(),
        "extend_sdl must overlay the synchronous hook"
    );

    let response = run(&schema, "{ thing { name } }", Ctx::allowed());
    assert!(response.errors.is_empty(), "{response:?}");
    assert_eq!(
        data(&response)
            .get("thing")
            .and_then(gql::Value::as_object)
            .and_then(|thing| thing.get("name")),
        Some(&gql::Value::from("Melman"))
    );
}

#[test]
fn transform_keeps_an_exposed_fields_sync_hook() {
    let mut schema = Schema::<Ctx>::new();
    schema.object::<Thing>(|o| {
        o.expose(expose!(Thing::name));
    });
    schema.query(|q| {
        q.field("thing", |_: NoArgs, _: &Ctx| thing());
    });
    let schema = schema.build().expect("schema builds");
    let schema = schema
        .transform(|_, field| {
            if field.name.as_ref() == "name" {
                gql::FieldTransform::Replace(field.clone())
            } else {
                gql::FieldTransform::Keep
            }
        })
        .expect("transform applies");

    let name = field(&schema, "Thing", "name");
    assert!(
        name.resolve.is_none() && name.sync_resolve.is_some(),
        "a field transform clone must keep the synchronous hook"
    );
}

#[test]
fn merging_schemas_keeps_the_single_sync_hook() {
    use std::sync::atomic::AtomicUsize;

    fn build(calls: Arc<AtomicUsize>) -> gql::Schema {
        let item = gql::ObjectType::new("Item").field(
            gql::Field::new("value", gql::TypeRef::string()).sync_resolve(move |_| {
                calls.fetch_add(1, Ordering::Relaxed);
                Ok(gql::Value::from("hook"))
            }),
        );
        gql::Schema::builder()
            .object(item)
            .object(
                gql::ObjectType::new("Query").field(
                    gql::Field::new("item", gql::TypeRef::named("Item")).resolve(|_| {
                        Box::pin(async { Ok(gql::Value::object(gql::Object::new())) })
                    }),
                ),
            )
            .query("Query")
            .build()
            .expect("valid schema")
    }

    // The second schema declares the same field with no resolver, so the merge must keep the
    // only implementation.
    let calls = Arc::new(AtomicUsize::new(0));
    let left = build(Arc::clone(&calls));
    let right = gql::Schema::from_sdl("type Item { value: String } type Query { item: Item }")
        .expect("valid SDL");
    let merged = left.merge(&right).expect("schemas merge");

    assert!(field(&merged, "Item", "value").sync_resolve.is_some());

    let response = futures::executor::block_on(gql::execute(
        &merged,
        gql::Request::new("{ item { value } }"),
        &gql::ExecutionOptions::default(),
    ));
    assert!(response.errors.is_empty(), "{response:?}");
    assert_eq!(calls.load(Ordering::Relaxed), 1);
}

#[test]
fn merging_two_different_sync_hooks_is_rejected() {
    fn build(prefix: &'static str) -> gql::Schema {
        let item = gql::ObjectType::new("Item").field(
            gql::Field::new("value", gql::TypeRef::string())
                .sync_resolve(move |_| Ok(gql::Value::from(prefix))),
        );
        gql::Schema::builder()
            .object(item)
            .object(
                gql::ObjectType::new("Query")
                    .field(gql::Field::new("item", gql::TypeRef::named("Item"))),
            )
            .query("Query")
            .build()
            .expect("valid schema")
    }

    let left = build("left");
    let right = build("right");
    let error = left
        .merge(&right)
        .expect_err("conflicting hooks must be rejected");
    let message = error.to_string();
    assert!(
        message.contains("sync_resolve"),
        "the conflict must name the slot: {message}"
    );
}

#[test]
fn build_clone_keeps_an_expose_hook() {
    let mut schema = Schema::<Ctx>::new();
    schema.object::<Thing>(|o| {
        o.expose(expose!(Thing::name));
    });
    schema.query(|q| {
        q.field("thing", |_: NoArgs, _: &Ctx| thing());
    });
    let schema = schema.build_clone().expect("schema builds");

    let name = field(&schema, "Thing", "name");
    assert!(name.resolve.is_none() && name.sync_resolve.is_some());
    let response = run(&schema, "{ thing { name } }", Ctx::allowed());
    assert!(response.errors.is_empty(), "{response:?}");
}

#[test]
fn an_expose_accessor_panic_does_not_escape_a_thread_with_a_small_stack() {
    // A last-resort probe: the panic must be caught inside the resolver boundary, not rely on a
    // large caller stack or an unwind through `execute`.
    let mut schema = Schema::<Ctx>::new();
    schema.object::<Thing>(|o| {
        o.expose(Expose::new("boom", |_: &Thing| -> String {
            panic!("small stack payload")
        }));
    });
    schema.query(|q| {
        q.field("thing", |_: NoArgs, _: &Ctx| thing());
    });
    let schema = schema.build().expect("schema builds");

    let seen: Arc<std::sync::Mutex<Option<gql::Response>>> = Arc::new(std::sync::Mutex::new(None));
    let writer = Arc::clone(&seen);
    let handle = std::thread::Builder::new()
        .stack_size(256 * 1024)
        .spawn(move || {
            let response = run(&schema, "{ thing { boom } }", Ctx::allowed());
            *writer.lock().unwrap() = Some(response);
        })
        .expect("thread spawns");
    handle
        .join()
        .expect("the request must not unwind the thread");

    let response = seen.lock().unwrap();
    let response = response.as_ref().expect("response");
    assert_eq!(response.errors.len(), 1, "{response:?}");
    assert_eq!(response.errors[0].message(), "Internal server error");
}

#[test]
fn the_panic_hook_still_sees_an_expose_accessor_panic() {
    static HOOK_LOCK: std::sync::Mutex<()> = std::sync::Mutex::new(());
    let _guard = HOOK_LOCK
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner());

    let seen = Arc::new(AtomicUsize::new(0));
    let observer = Arc::clone(&seen);
    let previous = take_hook();
    set_hook(Box::new(move |info| {
        if info.payload().downcast_ref::<&str>() == Some(&"hooked accessor payload") {
            observer.fetch_add(1, Ordering::Relaxed);
        }
    }));

    let mut schema = Schema::<Ctx>::new();
    schema.object::<Thing>(|o| {
        o.expose(Expose::new("boom", |_: &Thing| -> String {
            panic!("hooked accessor payload")
        }));
    });
    schema.query(|q| {
        q.field("thing", |_: NoArgs, _: &Ctx| thing());
    });
    let schema = schema.build().expect("schema builds");

    let outcome = catch_unwind(AssertUnwindSafe(|| {
        run(&schema, "{ thing { boom } }", Ctx::allowed())
    }));

    let _ = take_hook();
    set_hook(previous);

    let response = outcome.expect("the panic must be caught");
    assert_eq!(response.errors.len(), 1, "{response:?}");
    assert_eq!(seen.load(Ordering::Relaxed), 1);
}

#[test]
fn extend_sdl_keeps_a_materialized_auth_guard() {
    let mut schema = Schema::<Ctx>::new();
    schema.object::<Thing>(|o| {
        o.expose(expose!(Thing::name))
            .auth(|ctx: &Ctx| ctx.logged_in);
    });
    schema.query(|q| {
        q.field("thing", |_: NoArgs, _: &Ctx| thing());
    });
    let schema = schema
        .build()
        .expect("schema builds")
        .extend_sdl("extend type Thing { extra: String }")
        .expect("schema extends");

    let name = field(&schema, "Thing", "name");
    assert!(name.resolve.is_some() && name.sync_resolve.is_none());

    let denied = run(&schema, "{ thing { name } }", Ctx::denied());
    assert_eq!(denied.errors.len(), 1, "{denied:?}");
    assert_eq!(denied.errors[0].message(), "Not authorized");
}

#[test]
fn a_mock_plugin_replaces_an_exposed_field_and_drops_the_hook() {
    let mut schema = Schema::<Ctx>::new();
    schema.plugin(gurafi::MockPlugin::new().mock("Thing", "name", gql::Value::from("mocked")));
    schema.object::<Thing>(|o| {
        o.expose(expose!(Thing::name));
        o.expose(expose!(Thing::height));
    });
    schema.query(|q| {
        q.field("thing", |_: NoArgs, _: &Ctx| thing());
    });
    let schema = schema.build().expect("schema builds");

    let name = field(&schema, "Thing", "name");
    assert!(
        name.resolve.is_some() && name.sync_resolve.is_none(),
        "a mock must install its own resolver and not leave the hook behind"
    );
    // A field the plugin does not mock keeps the synchronous hook.
    assert!(field(&schema, "Thing", "height").sync_resolve.is_some());

    let response = run(&schema, "{ thing { name height } }", Ctx::allowed());
    assert!(response.errors.is_empty(), "{response:?}");
    let thing = data(&response)
        .get("thing")
        .and_then(gql::Value::as_object)
        .expect("thing object");
    assert_eq!(thing.get("name"), Some(&gql::Value::from("mocked")));
    assert_eq!(thing.get("height"), Some(&gql::Value::from(3)));
}

#[test]
fn a_sync_hook_whose_field_is_widened_to_an_interface_still_runs() {
    let mut schema = Schema::<Ctx>::new();
    let node = schema.interface("Node", |i| {
        i.field::<String>("name");
    });
    schema.object::<Thing>(|o| {
        o.implements(&node);
        o.expose(expose!(Thing::name));
    });
    schema.query(|q| {
        // The abstract type forces the asynchronous completion path; the exposed field's hook
        // must still be the one that produces `name`.
        q.field("thing", |_: NoArgs, _: &Ctx| thing())
            .as_abstract(&node);
    });
    let schema = schema.build().expect("schema builds");

    let name = field(&schema, "Thing", "name");
    assert!(
        name.resolve.is_none() && name.sync_resolve.is_some(),
        "widening the type must not box the hook"
    );

    let response = run(&schema, "{ thing { name } }", Ctx::allowed());
    assert!(response.errors.is_empty(), "{response:?}");
    assert_eq!(
        data(&response)
            .get("thing")
            .and_then(gql::Value::as_object)
            .and_then(|thing| thing.get("name")),
        Some(&gql::Value::from("Melman"))
    );
}

#[test]
fn a_panicking_plugin_wrapper_is_isolated() {
    struct Explode;
    impl gurafi::Plugin<Ctx> for Explode {
        fn wrap_resolve(
            &self,
            meta: &gurafi::FieldMeta<'_>,
            next: gql::ResolveFn,
        ) -> gql::ResolveFn {
            if meta.field_name != "name" {
                return next;
            }
            Arc::new(|_| Box::pin(async { panic!("plugin wrapper payload") }))
        }
    }

    let mut schema = Schema::<Ctx>::new();
    schema.plugin(Explode);
    schema.object::<Thing>(|o| {
        o.expose(expose!(Thing::name));
    });
    schema.query(|q| {
        q.field("thing", |_: NoArgs, _: &Ctx| thing());
    });
    let schema = schema.build().expect("schema builds");

    let response = run(&schema, "{ thing { name } }", Ctx::allowed());

    assert_eq!(response.errors.len(), 1, "{response:?}");
    assert_eq!(response.errors[0].message(), "Internal server error");
    assert_eq!(code(&response.errors[0]), Some("INTERNAL_SERVER_ERROR"));
    assert_eq!(
        response.errors[0].path().map(<[_]>::to_vec),
        Some(vec![
            gql::PathSegment::field("thing"),
            gql::PathSegment::field("name")
        ])
    );
}

#[test]
fn a_panicking_auth_predicate_is_isolated() {
    let mut schema = Schema::<Ctx>::new();
    schema.object::<Thing>(|o| {
        o.expose(expose!(Thing::name))
            .auth(|_: &Ctx| panic!("auth predicate payload"));
    });
    schema.query(|q| {
        q.field("thing", |_: NoArgs, _: &Ctx| thing());
    });
    let schema = schema.build().expect("schema builds");

    let response = run(&schema, "{ thing { name } }", Ctx::allowed());

    assert_eq!(response.errors.len(), 1, "{response:?}");
    assert_eq!(response.errors[0].message(), "Internal server error");
    assert_eq!(code(&response.errors[0]), Some("INTERNAL_SERVER_ERROR"));
}

#[test]
fn a_panicking_validate_hook_is_isolated() {
    let mut schema = Schema::<Ctx>::new();
    schema.object::<Thing>(|o| {
        o.expose(expose!(Thing::name))
            .validate::<()>(|(), _| panic!("validate hook payload"));
    });
    schema.query(|q| {
        q.field("thing", |_: NoArgs, _: &Ctx| thing());
    });
    let schema = schema.build().expect("schema builds");

    let response = run(&schema, "{ thing { name } }", Ctx::allowed());

    assert_eq!(response.errors.len(), 1, "{response:?}");
    assert_eq!(response.errors[0].message(), "Internal server error");
    assert_eq!(code(&response.errors[0]), Some("INTERNAL_SERVER_ERROR"));
}

#[derive(Clone, Object)]
struct Zoo {
    spots: Vec<i32>,
    name: String,
}

#[test]
fn exposing_a_scalar_list_on_a_type_with_a_resolver_does_not_panic() {
    // `expose!` used to install an async resolver, so a list field was never classified as a
    // leaf. Now it installs a synchronous hook, and `leaf_field` classifies the field by its
    // *named* type, so `Vec<i32>` looks like an `Int` leaf. When the type also has an async
    // resolver the object completes on the asynchronous path, whose all-leaves batch asserts
    // that the inline predicate and the leaf fast path agree.
    let mut schema = Schema::<Ctx>::new();
    schema.object::<Zoo>(|o| {
        o.expose(expose!(Zoo::spots));
        o.expose(expose!(Zoo::name));
        o.field("later", |_: &Zoo, _: NoArgs, _: &Ctx| String::from("async"));
    });
    schema.query(|q| {
        q.field("zoo", |_: NoArgs, _: &Ctx| Zoo {
            spots: vec![1, 2],
            name: String::from("zoo"),
        });
    });
    let schema = schema.build().expect("schema builds");

    let response = run(&schema, "{ zoo { spots name } }", Ctx::allowed());

    assert!(
        response.errors.is_empty(),
        "unexpected errors: {response:?}"
    );
    assert_eq!(
        data(&response)
            .get("zoo")
            .and_then(gql::Value::as_object)
            .and_then(|zoo| zoo.get("spots")),
        Some(&gql::Value::List(vec![
            gql::Value::Int(1),
            gql::Value::Int(2)
        ]))
    );
}
