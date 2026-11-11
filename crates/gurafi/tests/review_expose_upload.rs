//! Independent review probes for the synchronous `expose!` path and `gurafi::Upload`.
//!
//! These are probes, not part of the library's own suite: each test states the behaviour the
//! public documentation promises, so a failure identifies a divergence.

#![allow(dead_code)]

use std::sync::{
    Arc, Mutex,
    atomic::{AtomicUsize, Ordering},
};

use futures::executor::block_on;
use gurafi::{
    ErrorUnion, Expose, FieldMeta, GraphQLError, ID, Input, NoArgs, Object, Plugin, Schema, Union,
    Upload, expose,
};

#[derive(Clone, Default)]
struct Ctx {
    allow: bool,
    seen: Arc<Mutex<Vec<u8>>>,
}

impl Ctx {
    fn allowed() -> Self {
        Self {
            allow: true,
            seen: Arc::default(),
        }
    }

    fn denied() -> Self {
        Self {
            allow: false,
            seen: Arc::default(),
        }
    }
}

fn run(schema: &gql::Schema, query: &str, ctx: Ctx) -> gql::Response {
    block_on(gql::execute(
        schema,
        gql::Request::new(query),
        &gql::ExecutionOptions::with_context(ctx),
    ))
}

fn run_vars(schema: &gql::Schema, query: &str, variables: &gql::Object, ctx: Ctx) -> gql::Response {
    block_on(gql::execute(
        schema,
        gql::Request::new(query).variables(variables),
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

fn error_code(error: &GraphQLError) -> Option<&str> {
    error
        .extensions()
        .and_then(|extensions| extensions.get("code"))
        .and_then(gql::Value::as_str)
}

fn error_path(error: &GraphQLError) -> Option<String> {
    error.path().map(|path| {
        path.iter()
            .map(ToString::to_string)
            .collect::<Vec<_>>()
            .join(".")
    })
}

// ---------------------------------------------------------------------------------------------
// expose!: selection merging and accessor values
// ---------------------------------------------------------------------------------------------

#[derive(Clone, Object)]
struct Basic {
    name: String,
}

#[test]
fn merged_selections_run_the_accessor_once() {
    let calls = Arc::new(AtomicUsize::new(0));
    let counted = Arc::clone(&calls);

    let mut schema = Schema::<Ctx>::new();
    schema.object::<Basic>(|o| {
        o.expose(Expose::new("name", move |model: &Basic| {
            counted.fetch_add(1, Ordering::SeqCst);
            model.name.clone()
        }));
    });
    schema.query(|q| {
        q.field("basic", |_: NoArgs, _: &Ctx| Basic {
            name: "Melman".into(),
        });
    });
    let schema = schema.build().expect("schema builds");

    let response = run(
        &schema,
        "{ basic { name ...Named } } fragment Named on Basic { name }",
        Ctx::allowed(),
    );
    assert!(
        response.errors.is_empty(),
        "unexpected errors: {response:?}"
    );
    assert_eq!(
        calls.load(Ordering::SeqCst),
        1,
        "a merged selection set must resolve the property once"
    );

    calls.store(0, Ordering::SeqCst);
    let aliased = run(&schema, "{ basic { one: name two: name } }", Ctx::allowed());
    assert!(aliased.errors.is_empty(), "unexpected errors: {aliased:?}");
    assert_eq!(
        calls.load(Ordering::SeqCst),
        2,
        "two response keys are two resolutions"
    );
    let basic = data(&aliased)
        .get("basic")
        .and_then(gql::Value::as_object)
        .expect("basic object");
    assert_eq!(basic.get("one"), Some(&gql::Value::from("Melman")));
    assert_eq!(basic.get("two"), Some(&gql::Value::from("Melman")));
}

#[derive(Clone, Object)]
struct Leaf {
    a: String,
    b: i32,
}

#[derive(Clone, Object)]
struct Shell {
    leaf: Leaf,
}

#[test]
fn merged_object_selections_run_the_accessor_once() {
    let calls = Arc::new(AtomicUsize::new(0));
    let counted = Arc::clone(&calls);

    let mut schema = Schema::<Ctx>::new();
    schema.object::<Leaf>(|o| {
        o.expose(expose!(Leaf::a));
        o.expose(expose!(Leaf::b));
    });
    schema.object::<Shell>(|o| {
        o.expose(Expose::new("leaf", move |shell: &Shell| {
            counted.fetch_add(1, Ordering::SeqCst);
            shell.leaf.clone()
        }));
    });
    schema.query(|q| {
        q.field("shell", |_: NoArgs, _: &Ctx| Shell {
            leaf: Leaf {
                a: "a".into(),
                b: 7,
            },
        });
    });
    let schema = schema.build().expect("schema builds");

    let response = run(
        &schema,
        "{ shell { leaf { a } ...More } } fragment More on Shell { leaf { b } }",
        Ctx::allowed(),
    );
    assert!(
        response.errors.is_empty(),
        "unexpected errors: {response:?}"
    );
    assert_eq!(
        calls.load(Ordering::SeqCst),
        1,
        "an object property merged across fragments must be read once"
    );
    let leaf = data(&response)
        .get("shell")
        .and_then(gql::Value::as_object)
        .and_then(|shell| shell.get("leaf"))
        .and_then(gql::Value::as_object)
        .expect("leaf object");
    assert_eq!(leaf.get("a"), Some(&gql::Value::from("a")));
    assert_eq!(leaf.get("b"), Some(&gql::Value::Int(7)));
}

static LEAF_CLONES: AtomicUsize = AtomicUsize::new(0);

#[derive(Object)]
struct CountedLeaf {
    name: String,
}

impl Clone for CountedLeaf {
    fn clone(&self) -> Self {
        LEAF_CLONES.fetch_add(1, Ordering::SeqCst);
        Self {
            name: self.name.clone(),
        }
    }
}

#[derive(Clone, Object)]
struct MacroParent {
    child: CountedLeaf,
}

#[test]
fn expose_macro_object_property_is_cloned_once() {
    let mut schema = Schema::<Ctx>::new();
    schema.object::<CountedLeaf>(|o| {
        o.expose(expose!(CountedLeaf::name));
        o.field_async("slow", |_: &CountedLeaf, _: NoArgs, _: &Ctx| {
            Box::pin(async move { 1i32 })
        });
    });
    schema.object::<MacroParent>(|o| {
        o.expose(expose!(MacroParent::child));
    });
    schema.query(|q| {
        q.field("parent", |_: NoArgs, _: &Ctx| MacroParent {
            child: CountedLeaf {
                name: "Melman".into(),
            },
        });
    });
    let schema = schema.build().expect("schema builds");

    LEAF_CLONES.store(0, Ordering::SeqCst);
    let response = run(
        &schema,
        "{ parent { child { name slow } } }",
        Ctx::allowed(),
    );
    assert!(
        response.errors.is_empty(),
        "unexpected errors: {response:?}"
    );
    assert_eq!(
        LEAF_CLONES.load(Ordering::SeqCst),
        1,
        "`expose!(MacroParent::child)` cloned the model more than once"
    );
}

#[derive(Clone, Object)]
struct SlowChild {
    name: String,
}

#[derive(Clone, Object)]
struct CountingParent {
    child: SlowChild,
}

#[test]
fn exposed_accessors_are_not_re_executed_when_a_sibling_is_async() {
    let child_reads = Arc::new(AtomicUsize::new(0));
    let name_reads = Arc::new(AtomicUsize::new(0));
    let child_counter = Arc::clone(&child_reads);
    let name_counter = Arc::clone(&name_reads);

    let mut schema = Schema::<Ctx>::new();
    schema.object::<SlowChild>(|o| {
        o.expose(Expose::new("name", move |child: &SlowChild| {
            name_counter.fetch_add(1, Ordering::SeqCst);
            child.name.clone()
        }));
        o.field_async("slow", |_: &SlowChild, _: NoArgs, _: &Ctx| {
            Box::pin(async move { 1i32 })
        });
    });
    schema.object::<CountingParent>(|o| {
        o.expose(Expose::new("child", move |parent: &CountingParent| {
            child_counter.fetch_add(1, Ordering::SeqCst);
            parent.child.clone()
        }));
    });
    schema.query(|q| {
        q.field("parent", |_: NoArgs, _: &Ctx| CountingParent {
            child: SlowChild {
                name: "Melman".into(),
            },
        });
    });
    let schema = schema.build().expect("schema builds");

    let response = run(
        &schema,
        "{ parent { child { name slow } } }",
        Ctx::allowed(),
    );
    assert!(
        response.errors.is_empty(),
        "unexpected errors: {response:?}"
    );
    let child_reads = child_reads.load(Ordering::SeqCst);
    let name_reads = name_reads.load(Ordering::SeqCst);
    assert_eq!(
        (child_reads, name_reads),
        (1, 1),
        "exposed properties must be read once per request"
    );
}

#[derive(Clone, Object)]
struct BoomChild {
    name: String,
}

#[derive(Clone, Object)]
struct BoomParent {
    child: BoomChild,
}

#[derive(Clone, Object)]
struct PanicThenAsync {
    boom: String,
    other: SlowChild,
}

#[test]
fn a_panicking_accessor_on_a_speculative_parent_panics_once() {
    let reads = Arc::new(AtomicUsize::new(0));
    let counted = Arc::clone(&reads);

    let mut schema = Schema::<Ctx>::new();
    schema.object::<SlowChild>(|o| {
        o.expose(expose!(SlowChild::name));
        o.field_async("slow", |_: &SlowChild, _: NoArgs, _: &Ctx| {
            Box::pin(async move { 1i32 })
        });
    });
    schema.object::<PanicThenAsync>(|o| {
        o.expose(Expose::new("boom", move |_: &PanicThenAsync| -> String {
            counted.fetch_add(1, Ordering::SeqCst);
            panic!("accessor boom")
        }))
        .nullable();
        o.expose(expose!(PanicThenAsync::other));
    });
    schema.query(|q| {
        q.field("p", |_: NoArgs, _: &Ctx| PanicThenAsync {
            boom: String::new(),
            other: SlowChild {
                name: "Melman".into(),
            },
        });
    });
    let schema = schema.build().expect("schema builds");

    let response = run(&schema, "{ p { boom other { slow } } }", Ctx::allowed());

    assert_eq!(response.errors.len(), 1, "unexpected errors: {response:?}");
    assert_eq!(
        reads.load(Ordering::SeqCst),
        1,
        "the panicking accessor ran more than once for one request"
    );
}

#[test]
fn a_panicking_accessor_runs_once_per_request() {
    let reads = Arc::new(AtomicUsize::new(0));
    let counted = Arc::clone(&reads);

    let mut schema = Schema::<Ctx>::new();
    schema.object::<BoomChild>(|o| {
        o.expose(expose!(BoomChild::name));
        o.expose(Expose::new("boom", move |_: &BoomChild| -> String {
            counted.fetch_add(1, Ordering::SeqCst);
            panic!("accessor boom")
        }))
        .nullable();
        o.field_async("slow", |_: &BoomChild, _: NoArgs, _: &Ctx| {
            Box::pin(async move { 1i32 })
        });
    });
    schema.object::<BoomParent>(|o| {
        o.expose(expose!(BoomParent::child));
    });
    schema.query(|q| {
        q.field("parent", |_: NoArgs, _: &Ctx| BoomParent {
            child: BoomChild {
                name: "Melman".into(),
            },
        });
    });
    let schema = schema.build().expect("schema builds");

    let response = run(
        &schema,
        "{ parent { child { name boom slow } } }",
        Ctx::allowed(),
    );

    assert_eq!(response.errors.len(), 1, "unexpected errors: {response:?}");
    assert_eq!(
        error_code(&response.errors[0]),
        Some("INTERNAL_SERVER_ERROR")
    );
    assert_eq!(
        reads.load(Ordering::SeqCst),
        1,
        "the panicking accessor ran more than once for one request"
    );
}

#[derive(Clone, Object)]
struct Level3 {
    name: String,
}

#[derive(Clone, Object)]
struct Level2 {
    c: Level3,
}

#[derive(Clone, Object)]
struct Level1 {
    b: Level2,
}

#[test]
fn three_level_nested_rename_reads_the_leaf() {
    let mut schema = Schema::<Ctx>::new();
    schema.object::<Level3>(|o| {
        o.expose(expose!(Level3::name));
    });
    schema.object::<Level2>(|o| {
        o.expose(expose!(Level2::c));
    });
    schema.object::<Level1>(|o| {
        o.expose(expose!(Level1::b::c::name as "deepName"));
    });
    schema.query(|q| {
        q.field("root", |_: NoArgs, _: &Ctx| Level1 {
            b: Level2 {
                c: Level3 {
                    name: "leaf".into(),
                },
            },
        });
    });
    let schema = schema.build().expect("schema builds");

    assert!(field(&schema, "Level1", "deepName").sync_resolve.is_some());
    let response = run(&schema, "{ root { deepName } }", Ctx::allowed());
    assert!(
        response.errors.is_empty(),
        "unexpected errors: {response:?}"
    );
    assert_eq!(
        data(&response)
            .get("root")
            .and_then(gql::Value::as_object)
            .and_then(|root| root.get("deepName")),
        Some(&gql::Value::from("leaf"))
    );
}

#[derive(Clone, Object)]
struct Keyword {
    r#type: String,
}

#[test]
fn raw_identifier_properties_get_a_valid_field_name() {
    let mut schema = Schema::<Ctx>::new();
    schema.object::<Keyword>(|o| {
        o.expose(expose!(Keyword::r#type));
    });
    schema.query(|q| {
        q.field("keyword", |_: NoArgs, _: &Ctx| Keyword {
            r#type: "giraffe".into(),
        });
    });
    let schema = schema.build().expect("schema builds");

    assert_eq!(
        field(&schema, "Keyword", "type").sync_resolve.is_some(),
        true,
        "the `r#` prefix must not leak into the GraphQL name; SDL:\n{}",
        schema.sdl()
    );
    let response = run(&schema, "{ keyword { type } }", Ctx::allowed());
    assert!(
        response.errors.is_empty(),
        "unexpected errors: {response:?}"
    );
}

#[test]
fn duplicate_expose_names_are_a_build_error() {
    let mut schema = Schema::<Ctx>::new();
    schema.object::<Basic>(|o| {
        o.expose(expose!(Basic::name));
        o.expose(expose!(Basic::name as "name"));
    });
    schema.query(|q| {
        q.field("ok", |_: NoArgs, _: &Ctx| true);
    });
    assert!(
        schema.build().is_err(),
        "a duplicate field declaration must not silently replace the first"
    );
}

#[test]
fn an_invalid_custom_expose_name_is_rejected() {
    let mut schema = Schema::<Ctx>::new();
    schema.object::<Basic>(|o| {
        o.expose(expose!(Basic::name as "has space"));
    });
    schema.query(|q| {
        q.field("ok", |_: NoArgs, _: &Ctx| true);
    });
    match schema.build() {
        Ok(schema) => panic!(
            "an invalid GraphQL field name reached the schema; SDL:\n{}",
            schema.sdl()
        ),
        Err(_) => {}
    }
}

// ---------------------------------------------------------------------------------------------
// expose!: panic isolation
// ---------------------------------------------------------------------------------------------

#[derive(Clone, Object)]
struct Panicky {
    name: String,
}

#[test]
fn panicking_accessor_is_an_internal_error_field() {
    let mut schema = Schema::<Ctx>::new();
    schema.object::<Panicky>(|o| {
        o.expose(expose!(Panicky::name));
        o.expose(Expose::new("boom", |_: &Panicky| -> String {
            panic!("the accessor panicked")
        }))
        .nullable();
    });
    schema.query(|q| {
        q.field("panicky", |_: NoArgs, _: &Ctx| Panicky {
            name: "ok".into(),
        });
    });
    let schema = schema.build().expect("schema builds");

    let response = run(&schema, "{ panicky { name boom } }", Ctx::allowed());
    let object = data(&response)
        .get("panicky")
        .and_then(gql::Value::as_object)
        .expect("the nullable field must not null the parent");
    assert_eq!(object.get("name"), Some(&gql::Value::from("ok")));
    assert_eq!(object.get("boom"), Some(&gql::Value::Null));

    assert_eq!(response.errors.len(), 1, "unexpected errors: {response:?}");
    let error = &response.errors[0];
    assert_eq!(error.message(), "Internal server error");
    assert_eq!(error_code(error), Some("INTERNAL_SERVER_ERROR"));
    assert_eq!(error_path(error).as_deref(), Some("panicky.boom"));
}

// ---------------------------------------------------------------------------------------------
// expose!: option chaining
// ---------------------------------------------------------------------------------------------

#[derive(Clone, Object)]
struct OptionalField {
    nickname: Option<String>,
}

#[test]
fn non_null_then_nullable_is_not_a_build_error() {
    // `FieldConfig` documents that options "can be chained in any order". `.non_null()` records
    // a check immediately; `.nullable()` afterwards makes the final type nullable again, so the
    // field can no longer violate the declared type.
    let mut schema = Schema::<Ctx>::new();
    schema.object::<OptionalField>(|o| {
        o.expose(expose!(OptionalField::nickname))
            .non_null()
            .nullable();
    });
    schema.query(|q| {
        q.field("ok", |_: NoArgs, _: &Ctx| true);
    });
    match schema.build() {
        Ok(built) => {
            assert_eq!(
                field(&built, "OptionalField", "nickname").ty.to_string(),
                "String"
            );
        }
        Err(error) => panic!(
            "`.non_null().nullable()` is rejected even though the field ends nullable: {error}"
        ),
    }
}

// ---------------------------------------------------------------------------------------------
// expose!: metadata and type overrides
// ---------------------------------------------------------------------------------------------

#[derive(Clone, Object)]
struct Keeper {
    name: String,
}

#[derive(Clone, Object)]
struct Zoo {
    head: Keeper,
    keepers: Vec<Keeper>,
}

#[derive(Clone, Object)]
struct Other {
    label: String,
}

#[derive(Clone, Union)]
enum Animal {
    Keeper(Keeper),
    Other(Other),
}

#[test]
fn runtime_named_objects_resolve_from_expose() {
    let mut schema = Schema::<Ctx>::new();
    // The plain model must also exist because `expose` registers it (see the probe below).
    schema.object::<Keeper>(|o| {
        o.expose(expose!(Keeper::name));
    });
    let admin = schema.object_ref::<Keeper>("AdminKeeper");
    schema.implement(&admin, |o| {
        o.expose(expose!(Keeper::name));
    });
    schema.object::<Other>(|o| {
        o.expose(expose!(Other::label));
    });
    schema.object::<Zoo>(|o| {
        o.expose(expose!(Zoo::head as "head"))
            .as_dynamic(gql::TypeRef::named("AdminKeeper"));
        o.expose(expose!(Zoo::keepers)).as_object_ref(&admin);
    });
    schema.query(|q| {
        q.field("zoo", |_: NoArgs, _: &Ctx| Zoo {
            head: Keeper {
                name: "head".into(),
            },
            keepers: vec![Keeper { name: "one".into() }, Keeper { name: "two".into() }],
        });
    });
    let schema = schema.build().expect("schema builds");

    assert_eq!(
        field(&schema, "Zoo", "keepers").ty.to_string(),
        "[AdminKeeper]!"
    );
    assert!(field(&schema, "Zoo", "head").sync_resolve.is_some());
    let response = run(
        &schema,
        "{ zoo { head { name } keepers { name } } }",
        Ctx::allowed(),
    );
    assert!(
        response.errors.is_empty(),
        "unexpected errors: {response:?}"
    );
    let zoo = data(&response)
        .get("zoo")
        .and_then(gql::Value::as_object)
        .expect("zoo object");
    assert_eq!(
        zoo.get("head")
            .and_then(gql::Value::as_object)
            .and_then(|head| head.get("name")),
        Some(&gql::Value::from("head"))
    );
    let keepers = zoo
        .get("keepers")
        .and_then(gql::Value::as_list)
        .expect("keeper list");
    assert_eq!(keepers.len(), 2);
}

#[test]
fn as_abstract_union_on_expose_keeps_the_sync_path() {
    let mut schema = Schema::<Ctx>::new();
    schema.object::<Keeper>(|o| {
        o.expose(expose!(Keeper::name));
    });
    schema.object::<Other>(|o| {
        o.expose(expose!(Other::label));
    });
    let animal = schema.union("Animal", |u| {
        u.member::<Keeper>();
        u.member::<Other>();
    });
    schema.object::<Zoo>(|o| {
        o.expose(expose!(Zoo::head as "pet")).as_abstract(&animal);
        o.expose(expose!(Zoo::keepers));
    });
    schema.query(|q| {
        q.field("zoo", |_: NoArgs, _: &Ctx| Zoo {
            head: Keeper { name: "Bob".into() },
            keepers: Vec::new(),
        });
    });
    let schema = schema.build().expect("schema builds");

    let pet = field(&schema, "Zoo", "pet");
    assert!(
        pet.resolve.is_none() && pet.sync_resolve.is_some(),
        "as_abstract only changes the declaration"
    );
    // Models are nullable by default (see docs/guide/nullability.md), so the wrapper is preserved
    // from `Keeper` rather than tightened.
    assert_eq!(pet.ty.to_string(), "Animal");
    let response = run(
        &schema,
        "{ zoo { pet { __typename ... on Keeper { name } } } }",
        Ctx::allowed(),
    );
    assert!(
        response.errors.is_empty(),
        "unexpected errors: {response:?}"
    );
    let pet = data(&response)
        .get("zoo")
        .and_then(gql::Value::as_object)
        .and_then(|zoo| zoo.get("pet"))
        .and_then(gql::Value::as_object)
        .expect("pet object");
    assert_eq!(pet.get("__typename"), Some(&gql::Value::from("Keeper")));
    assert_eq!(pet.get("name"), Some(&gql::Value::from("Bob")));
}

#[test]
fn expose_plus_object_ref_does_not_require_the_plain_model() {
    // `field_ref` deliberately does *not* register the resolver's output type as a reference,
    // because the runtime reference replaces the name. `expose` always does, so a schema whose
    // only object name is the runtime one fails to build with a spurious "never registered".
    let mut schema = Schema::<Ctx>::new();
    let admin = schema.object_ref::<Keeper>("AdminKeeper");
    schema.implement(&admin, |o| {
        o.expose(expose!(Keeper::name));
    });
    schema.object::<Zoo>(|o| {
        o.expose(expose!(Zoo::keepers)).as_object_ref(&admin);
    });
    schema.query(|q| {
        q.field("ok", |_: NoArgs, _: &Ctx| true);
    });
    match schema.build() {
        Ok(_) => {}
        Err(error) => panic!(
            "the plain `Keeper` type is not part of the schema after `as_object_ref`: {error}"
        ),
    }
}

#[test]
fn expose_plus_as_dynamic_does_not_require_the_plain_model() {
    let mut schema = Schema::<Ctx>::new();
    let admin = schema.object_ref::<Keeper>("AdminKeeper");
    schema.implement(&admin, |o| {
        o.expose(expose!(Keeper::name));
    });
    schema.object::<Zoo>(|o| {
        o.expose(expose!(Zoo::head as "head"))
            .as_dynamic(gql::TypeRef::named("AdminKeeper"));
    });
    schema.query(|q| {
        q.field("ok", |_: NoArgs, _: &Ctx| true);
    });
    match schema.build() {
        Ok(_) => {}
        Err(error) => {
            panic!("the plain `Keeper` type is not part of the schema after `as_dynamic`: {error}")
        }
    }
}

#[derive(Clone, Object)]
struct Described {
    name: String,
    height: f64,
    tags: Vec<String>,
}

#[test]
fn metadata_and_type_overrides_print_in_sdl() {
    let mut schema = Schema::<Ctx>::new();
    schema.directive(
        gql::DirectiveDefinition::new("tag")
            .location(gql::ast::DirectiveLocation::FieldDefinition)
            .arg(gql::InputValue::new("name", gql::TypeRef::string())),
    );
    schema.object::<Described>(|o| {
        o.expose(expose!(Described::name))
            .description("The name.")
            .deprecated("Use height.")
            .complexity(2)
            .directive(gql::AppliedDirective::new("tag").argument("name", "animal"))
            .returns::<ID>();
        o.expose(expose!(Described::height)).nullable();
        o.expose(expose!(Described::tags)).nullable_items();
    });
    schema.query(|q| {
        q.field("d", |_: NoArgs, _: &Ctx| Described {
            name: "Melman".into(),
            height: 3.5,
            tags: vec!["tall".into()],
        });
    });
    let schema = schema.build().expect("schema builds");

    let name = field(&schema, "Described", "name");
    assert!(name.resolve.is_none() && name.sync_resolve.is_some());
    assert_eq!(name.ty.to_string(), "ID!");
    assert_eq!(name.description.as_deref(), Some("The name."));
    assert_eq!(name.deprecation_reason.as_deref(), Some("Use height."));
    assert!(name.complexity.is_some());
    assert_eq!(
        field(&schema, "Described", "height").ty.to_string(),
        "Float"
    );
    assert_eq!(
        field(&schema, "Described", "tags").ty.to_string(),
        "[String]!"
    );

    let sdl = schema.sdl();
    assert!(sdl.contains("The name."), "SDL:\n{sdl}");
    assert!(sdl.contains("deprecated"), "SDL:\n{sdl}");
    assert!(sdl.contains("@tag"), "SDL:\n{sdl}");
    assert!(sdl.contains("name: ID!"), "SDL:\n{sdl}");
}

#[test]
fn complexity_with_keeps_the_synchronous_path() {
    let mut schema = Schema::<Ctx>::new();
    schema.object::<Basic>(|o| {
        o.expose(expose!(Basic::name)).complexity_with(|_args| 3);
    });
    schema.query(|q| {
        q.field("basic", |_: NoArgs, _: &Ctx| Basic {
            name: "Melman".into(),
        });
    });
    let schema = schema.build().expect("schema builds");

    let name = field(&schema, "Basic", "name");
    assert!(name.sync_resolve.is_some() && name.resolve.is_none());
    assert!(name.complexity.is_some());
}

// ---------------------------------------------------------------------------------------------
// expose!: auth, validation and interfaces
// ---------------------------------------------------------------------------------------------

#[derive(Clone, Object)]
struct Guarded {
    name: String,
}

#[test]
fn object_auth_boxes_an_exposed_field() {
    let mut schema = Schema::<Ctx>::new();
    schema.object::<Guarded>(|o| {
        o.auth(|ctx: &Ctx| ctx.allow);
        o.expose(expose!(Guarded::name));
    });
    schema.query(|q| {
        q.field("guarded", |_: NoArgs, _: &Ctx| Guarded {
            name: "Melman".into(),
        });
    });
    let schema = schema.build().expect("schema builds");

    let name = field(&schema, "Guarded", "name");
    assert!(
        name.resolve.is_some() && name.sync_resolve.is_none(),
        "object auth must materialize the exposed field"
    );
    let denied = run(&schema, "{ guarded { name } }", Ctx::denied());
    assert_eq!(error_code(&denied.errors[0]), Some("UNAUTHORIZED"));
}

#[test]
fn interface_field_auth_boxes_an_exposed_field() {
    let mut schema = Schema::<Ctx>::new();
    let node = schema.interface("Node", |i| {
        i.field::<String>("name").auth(|ctx: &Ctx| ctx.allow);
    });
    schema.object::<Basic>(|o| {
        o.implements(&node);
        o.expose(expose!(Basic::name));
    });
    schema.query(|q| {
        q.field("node", |_: NoArgs, _: &Ctx| Basic {
            name: "Melman".into(),
        })
        .as_abstract(&node);
    });
    let schema = schema.build().expect("schema builds");

    let name = field(&schema, "Basic", "name");
    assert!(name.resolve.is_some(), "the guard must be installed");
    assert!(name.sync_resolve.is_none());

    let allowed = run(&schema, "{ node { name } }", Ctx::allowed());
    assert!(allowed.errors.is_empty(), "unexpected errors: {allowed:?}");

    let denied = run(&schema, "{ node { name } }", Ctx::denied());
    assert_eq!(denied.errors.len(), 1, "unexpected errors: {denied:?}");
    assert_eq!(error_code(&denied.errors[0]), Some("UNAUTHORIZED"));
}

#[derive(Clone, Object)]
struct Inherits {
    name: String,
}

#[test]
fn resolve_inherited_and_expose_coexist() {
    let mut schema = Schema::<Ctx>::new();
    let node = schema.interface("Node", |i| {
        i.field::<ID>("id");
        i.field::<String>("name");
    });
    schema.object::<Inherits>(|o| {
        o.implements(&node);
        o.resolve_inherited("id", |model: &Inherits, _: NoArgs, _: &Ctx| {
            ID::new(model.name.clone())
        });
        o.expose(expose!(Inherits::name));
    });
    schema.query(|q| {
        q.field("node", |_: NoArgs, _: &Ctx| Inherits { name: "Ada".into() })
            .as_abstract(&node);
    });
    let schema = schema.build().expect("schema builds");

    let response = run(&schema, "{ node { id name } }", Ctx::allowed());
    assert!(
        response.errors.is_empty(),
        "unexpected errors: {response:?}"
    );
    let node = data(&response)
        .get("node")
        .and_then(gql::Value::as_object)
        .expect("node object");
    assert_eq!(node.get("id"), Some(&gql::Value::from("Ada")));
    assert_eq!(node.get("name"), Some(&gql::Value::from("Ada")));
}

#[test]
fn validate_on_expose_runs_once_and_reports_a_field_error() {
    let calls = Arc::new(AtomicUsize::new(0));
    let counted = Arc::clone(&calls);

    let mut schema = Schema::<Ctx>::new();
    schema.object::<Basic>(|o| {
        o.expose(expose!(Basic::name))
            .validate::<()>(move |(), ctx| {
                counted.fetch_add(1, Ordering::SeqCst);
                if ctx.allow {
                    Ok(())
                } else {
                    Err(GraphQLError::new("validation denied"))
                }
            });
    });
    schema.query(|q| {
        q.field("basic", |_: NoArgs, _: &Ctx| Basic {
            name: "Melman".into(),
        });
    });
    let schema = schema.build().expect("schema builds");

    let name = field(&schema, "Basic", "name");
    assert!(name.resolve.is_some() && name.sync_resolve.is_none());

    let response = run(
        &schema,
        "{ basic { name ...Named } } fragment Named on Basic { name }",
        Ctx::allowed(),
    );
    assert!(
        response.errors.is_empty(),
        "unexpected errors: {response:?}"
    );
    assert_eq!(
        calls.load(Ordering::SeqCst),
        1,
        "validation must run once for a merged selection"
    );

    let denied = run(&schema, "{ basic { name } }", Ctx::denied());
    assert_eq!(denied.errors.len(), 1, "unexpected errors: {denied:?}");
    assert_eq!(error_path(&denied.errors[0]).as_deref(), Some("basic.name"));
    assert!(
        denied.errors[0].message().contains("validation denied"),
        "unexpected message: {}",
        denied.errors[0].message()
    );
}

#[test]
fn auth_and_validate_compose_in_either_order() {
    for validate_first in [true, false] {
        let mut schema = Schema::<Ctx>::new();
        schema.object::<Basic>(|o| {
            if validate_first {
                o.expose(expose!(Basic::name))
                    .validate::<()>(|(), _| Ok(()))
                    .auth(|ctx: &Ctx| ctx.allow);
            } else {
                o.expose(expose!(Basic::name))
                    .auth(|ctx: &Ctx| ctx.allow)
                    .validate::<()>(|(), _| Ok(()));
            }
        });
        schema.query(|q| {
            q.field("basic", |_: NoArgs, _: &Ctx| Basic {
                name: "Melman".into(),
            });
        });
        let schema = schema.build().expect("schema builds");

        let allowed = run(&schema, "{ basic { name } }", Ctx::allowed());
        assert!(
            allowed.errors.is_empty(),
            "validate_first={validate_first}: {allowed:?}"
        );
        let denied = run(&schema, "{ basic { name } }", Ctx::denied());
        assert_eq!(
            error_code(&denied.errors[0]),
            Some("UNAUTHORIZED"),
            "validate_first={validate_first}"
        );
    }
}

#[test]
fn auth_error_message_reaches_the_response() {
    let mut schema = Schema::<Ctx>::new();
    schema.object::<Basic>(|o| {
        o.expose(expose!(Basic::name))
            .auth(|ctx: &Ctx| ctx.allow)
            .auth_error("sign in");
    });
    schema.query(|q| {
        q.field("basic", |_: NoArgs, _: &Ctx| Basic {
            name: "Melman".into(),
        });
    });
    let schema = schema.build().expect("schema builds");

    let denied = run(&schema, "{ basic { name } }", Ctx::denied());
    assert_eq!(denied.errors[0].message(), "sign in");
}

// ---------------------------------------------------------------------------------------------
// expose!: plugins and build_clone
// ---------------------------------------------------------------------------------------------

struct Wrapping {
    count: Arc<AtomicUsize>,
}

impl Plugin<Ctx> for Wrapping {
    fn wrap_resolve(&self, meta: &FieldMeta<'_>, next: gql::ResolveFn) -> gql::ResolveFn {
        let count = Arc::clone(&self.count);
        let counted = meta.field_name == "name";
        Arc::new(move |context| {
            if counted {
                count.fetch_add(1, Ordering::SeqCst);
            }
            next(context)
        })
    }
}

struct Passthrough;

impl Plugin<Ctx> for Passthrough {
    fn wrap_resolve(&self, _meta: &FieldMeta<'_>, next: gql::ResolveFn) -> gql::ResolveFn {
        next
    }
}

#[test]
fn a_wrap_resolve_hook_that_returns_next_keeps_the_sync_path() {
    let mut schema = Schema::<Ctx>::new();
    schema.object::<Basic>(|o| {
        o.expose(expose!(Basic::name));
    });
    schema.query(|q| {
        q.field("basic", |_: NoArgs, _: &Ctx| Basic {
            name: "Melman".into(),
        });
    });
    schema.plugin(Passthrough);
    let schema = schema.build().expect("schema builds");

    let name = field(&schema, "Basic", "name");
    assert!(
        name.sync_resolve.is_some() && name.resolve.is_none(),
        "a hook that returns `next` must not materialize the field"
    );
}

#[test]
fn build_clone_does_not_duplicate_plugin_wrapping() {
    let count = Arc::new(AtomicUsize::new(0));
    let mut schema = Schema::<Ctx>::new();
    schema.object::<Basic>(|o| {
        o.expose(expose!(Basic::name));
    });
    schema.query(|q| {
        q.field("basic", |_: NoArgs, _: &Ctx| Basic {
            name: "Melman".into(),
        });
    });
    schema.plugin(Wrapping {
        count: Arc::clone(&count),
    });

    let first = schema.build_clone().expect("first build");
    let second = schema.build_clone().expect("second build");

    let response = run(&first, "{ basic { name } }", Ctx::allowed());
    assert!(
        response.errors.is_empty(),
        "unexpected errors: {response:?}"
    );
    assert_eq!(count.load(Ordering::SeqCst), 1, "one wrapper per execution");

    let response = run(&second, "{ basic { name } }", Ctx::allowed());
    assert!(
        response.errors.is_empty(),
        "unexpected errors: {response:?}"
    );
    assert_eq!(
        count.load(Ordering::SeqCst),
        2,
        "the second clone must not stack a second wrapper"
    );
}

// ---------------------------------------------------------------------------------------------
// expose!: result-union generated fields
// ---------------------------------------------------------------------------------------------

#[derive(Clone, Object)]
struct User {
    name: String,
}

#[derive(Clone, Object)]
struct NotFound {
    message: String,
}

#[derive(ErrorUnion)]
enum LookupError {
    NotFound(NotFound),
}

#[test]
fn expose_works_inside_a_result_union_payload() {
    let mut schema = Schema::<Ctx>::new();
    schema.object::<User>(|o| {
        o.expose(expose!(User::name));
    });
    schema.object::<NotFound>(|o| {
        o.expose(expose!(NotFound::message));
    });
    schema.query(|q| {
        q.field("ok", |_: NoArgs, _: &Ctx| true);
    });
    schema.mutation(|m| {
        m.field_result_union::<User, LookupError, NoArgs, _>("lookup", |_: NoArgs, _: &Ctx| {
            Ok(User { name: "ada".into() })
        });
    });
    let schema = schema.build().expect("schema builds");

    assert!(
        field(&schema, "User", "name").sync_resolve.is_some(),
        "an expose field used as a result-union payload must stay synchronous"
    );
    let response = run(
        &schema,
        "mutation { lookup { __typename ... on MutationLookupSuccess { data { name } } } }",
        Ctx::allowed(),
    );
    assert!(
        response.errors.is_empty(),
        "unexpected errors: {response:?}"
    );
    let lookup = data(&response)
        .get("lookup")
        .and_then(gql::Value::as_object)
        .expect("lookup union value");
    assert_eq!(
        lookup.get("__typename"),
        Some(&gql::Value::from("MutationLookupSuccess"))
    );
    assert_eq!(
        lookup
            .get("data")
            .and_then(gql::Value::as_object)
            .and_then(|data| data.get("name")),
        Some(&gql::Value::from("ada"))
    );
}

// ---------------------------------------------------------------------------------------------
// Upload: registration, SDL and coercion
// ---------------------------------------------------------------------------------------------

#[derive(Input)]
struct UploadArgs {
    file: Upload,
}

fn describe(upload: &Upload) -> String {
    format!(
        "{}:{}:{}",
        upload.filename().unwrap_or("<none>"),
        upload.content_type().unwrap_or("<none>"),
        String::from_utf8_lossy(upload.bytes())
    )
}

fn upload_schema() -> Schema<Ctx> {
    let mut schema = Schema::<Ctx>::new();
    schema.query(|q| {
        q.field("ok", |_: NoArgs, _: &Ctx| true);
    });
    schema.mutation(|m| {
        m.field("uploadFile", |args: UploadArgs, ctx: &Ctx| {
            *ctx.seen.lock().expect("ctx lock") = args.file.bytes().to_vec();
            describe(&args.file)
        });
    });
    schema
}

#[test]
fn upload_registers_a_scalar_and_prints_in_sdl() {
    let schema = upload_schema().build().expect("schema builds");

    let sdl = schema.sdl();
    assert!(sdl.contains("scalar Upload"), "SDL:\n{sdl}");
    assert!(
        sdl.contains("A file uploaded with a GraphQL multipart request."),
        "SDL:\n{sdl}"
    );
    assert!(
        sdl.contains("uploadFile(file: Upload!): String!"),
        "SDL:\n{sdl}"
    );

    let response = run(
        &schema,
        "{ __type(name: \"Upload\") { kind name description } }",
        Ctx::allowed(),
    );
    assert!(
        response.errors.is_empty(),
        "unexpected errors: {response:?}"
    );
    let upload = data(&response)
        .get("__type")
        .and_then(gql::Value::as_object)
        .expect("Upload type");
    assert_eq!(
        upload.get("kind").and_then(gql::Value::as_str),
        Some("SCALAR")
    );
    assert_eq!(
        upload.get("name").and_then(gql::Value::as_str),
        Some("Upload")
    );
}

#[test]
fn upload_external_value_reaches_the_resolver_and_the_context_still_works() {
    let schema = upload_schema().build().expect("schema builds");
    let upload = Upload::new("note.txt", "text/plain", b"hello".to_vec());
    let mut variables = gql::Object::new();
    variables.insert("file", gql::Value::external(upload));
    let ctx = Ctx::allowed();
    let seen = Arc::clone(&ctx.seen);

    let response = run_vars(
        &schema,
        "mutation($file: Upload!) { uploadFile(file: $file) }",
        &variables,
        ctx,
    );
    assert!(
        response.errors.is_empty(),
        "unexpected errors: {response:?}"
    );
    assert_eq!(
        data(&response).get("uploadFile"),
        Some(&gql::Value::from("note.txt:text/plain:hello"))
    );
    assert_eq!(&*seen.lock().expect("ctx lock"), b"hello");
}

#[test]
fn upload_literal_is_rejected_with_an_actionable_error() {
    let schema = upload_schema().build().expect("schema builds");
    let response = run(
        &schema,
        "mutation { uploadFile(file: \"hello\") }",
        Ctx::allowed(),
    );

    assert_eq!(response.errors.len(), 1, "unexpected errors: {response:?}");
    assert!(
        response.errors[0].message().contains("multipart"),
        "unexpected message: {}",
        response.errors[0].message()
    );
    // Literal argument coercion is hoisted into preparation, so this is a request error: no
    // `data` and no response path, but a source location.
    assert!(response.data.is_none(), "unexpected data: {response:?}");
    assert!(response.errors[0].path().is_none());
    assert!(!response.errors[0].locations().is_empty());
}

#[test]
fn missing_required_upload_variable_is_a_request_error() {
    let schema = upload_schema().build().expect("schema builds");
    let response = run_vars(
        &schema,
        "mutation($file: Upload!) { uploadFile(file: $file) }",
        &gql::Object::new(),
        Ctx::allowed(),
    );
    assert!(!response.errors.is_empty(), "expected a coercion error");
    assert_eq!(
        response.errors[0].message(),
        "Variable \"$file\" of required type \"Upload!\" was not provided."
    );
}

#[derive(Input)]
struct OptionalUploadArgs {
    file: Option<Upload>,
}

#[test]
fn a_null_upload_variable_coerces_to_none() {
    let mut schema = Schema::<Ctx>::new();
    schema.query(|q| {
        q.field("ok", |_: NoArgs, _: &Ctx| true);
    });
    schema.mutation(|m| {
        m.field("maybe", |args: OptionalUploadArgs, _: &Ctx| {
            args.file.map_or("none", |file| {
                if file.bytes().is_empty() {
                    "empty"
                } else {
                    "some"
                }
            })
        });
    });
    let schema = schema.build().expect("schema builds");

    let mut variables = gql::Object::new();
    variables.insert("file", gql::Value::Null);
    let response = run_vars(
        &schema,
        "mutation($file: Upload) { maybe(file: $file) }",
        &variables,
        Ctx::allowed(),
    );
    assert!(
        response.errors.is_empty(),
        "unexpected errors: {response:?}"
    );
    assert_eq!(
        data(&response).get("maybe"),
        Some(&gql::Value::from("none"))
    );
}

#[derive(Input)]
struct ManyArgs {
    files: Vec<Upload>,
}

#[test]
fn upload_list_and_reused_clone_reach_the_resolver() {
    let mut schema = Schema::<Ctx>::new();
    schema.query(|q| {
        q.field("ok", |_: NoArgs, _: &Ctx| true);
    });
    schema.mutation(|m| {
        m.field("many", |args: ManyArgs, _: &Ctx| {
            args.files
                .iter()
                .map(describe)
                .collect::<Vec<_>>()
                .join("|")
        });
    });
    let schema = schema.build().expect("schema builds");

    let shared = Upload::new("shared.txt", "text/plain", b"shared".to_vec());
    let mut variables = gql::Object::new();
    variables.insert(
        "files",
        gql::Value::List(vec![
            gql::Value::external(shared.clone()),
            gql::Value::external(shared.clone()),
            gql::Value::external(Upload::new("third.txt", "text/csv", b"3".to_vec())),
        ]),
    );
    let response = run_vars(
        &schema,
        "mutation($files: [Upload!]!) { many(files: $files) }",
        &variables,
        Ctx::allowed(),
    );
    assert!(
        response.errors.is_empty(),
        "unexpected errors: {response:?}"
    );
    assert_eq!(
        data(&response).get("many"),
        Some(&gql::Value::from(
            "shared.txt:text/plain:shared|shared.txt:text/plain:shared|third.txt:text/csv:3"
        ))
    );
}

#[derive(Input)]
#[graphql(name = "MediaChoice", one_of)]
struct MediaChoice {
    file: Option<Upload>,
    url: Option<String>,
}

#[derive(Input)]
struct ChoiceArgs {
    choice: MediaChoice,
}

#[test]
fn upload_nested_in_a_one_of_input_is_coerced() {
    let mut schema = Schema::<Ctx>::new();
    schema.query(|q| {
        q.field("ok", |_: NoArgs, _: &Ctx| true);
    });
    schema.mutation(|m| {
        m.field("choose", |args: ChoiceArgs, _: &Ctx| {
            match (args.choice.file, args.choice.url) {
                (Some(file), None) => describe(&file),
                (None, Some(url)) => url,
                _ => String::from("<invalid>"),
            }
        });
    });
    let schema = schema.build().expect("schema builds");

    let sdl = schema.sdl();
    assert!(sdl.contains("input MediaChoice @oneOf"), "SDL:\n{sdl}");
    assert!(sdl.contains("file: Upload"), "SDL:\n{sdl}");

    let upload = Upload::new("m.txt", "text/plain", b"media".to_vec());
    let mut choice = gql::Object::new();
    choice.insert("file", gql::Value::external(upload));
    let mut variables = gql::Object::new();
    variables.insert("choice", gql::Value::Object(Arc::new(choice)));
    let response = run_vars(
        &schema,
        "mutation($choice: MediaChoice!) { choose(choice: $choice) }",
        &variables,
        Ctx::allowed(),
    );
    assert!(
        response.errors.is_empty(),
        "unexpected errors: {response:?}"
    );
    assert_eq!(
        data(&response).get("choose"),
        Some(&gql::Value::from("m.txt:text/plain:media"))
    );

    // Supplying both sides of a one-of input is still rejected when one side is an Upload.
    let mut choice = gql::Object::new();
    choice.insert(
        "file",
        gql::Value::external(Upload::new("m.txt", "text/plain", b"m".to_vec())),
    );
    choice.insert("url", gql::Value::from("https://example.com"));
    let mut variables = gql::Object::new();
    variables.insert("choice", gql::Value::Object(Arc::new(choice)));
    let response = run_vars(
        &schema,
        "mutation($choice: MediaChoice!) { choose(choice: $choice) }",
        &variables,
        Ctx::allowed(),
    );
    assert_eq!(response.errors.len(), 1, "unexpected errors: {response:?}");
}

fn validate_upload(args: &ValidatedUploadArgs) -> Result<(), GraphQLError> {
    if args.file.len() > args.limit as usize {
        Err(GraphQLError::new("file is too large"))
    } else {
        Ok(())
    }
}

#[derive(Input)]
#[graphql(validate = "validate_upload")]
struct ValidatedUploadArgs {
    file: Upload,
    limit: i32,
}

#[test]
fn input_level_validation_sees_the_upload_bytes() {
    let mut schema = Schema::<Ctx>::new();
    schema.query(|q| {
        q.field("ok", |_: NoArgs, _: &Ctx| true);
    });
    schema.mutation(|m| {
        m.field("validated", |args: ValidatedUploadArgs, _: &Ctx| {
            describe(&args.file)
        });
    });
    let schema = schema.build().expect("schema builds");

    let mut variables = gql::Object::new();
    variables.insert(
        "file",
        gql::Value::external(Upload::new("a.txt", "text/plain", b"0123456789".to_vec())),
    );
    variables.insert("limit", gql::Value::Int(3));
    let response = run_vars(
        &schema,
        "mutation($file: Upload!, $limit: Int!) { validated(file: $file, limit: $limit) }",
        &variables,
        Ctx::allowed(),
    );
    assert_eq!(response.errors.len(), 1, "unexpected errors: {response:?}");
    assert!(
        response.errors[0].message().contains("file is too large"),
        "unexpected message: {}",
        response.errors[0].message()
    );
    assert_eq!(error_code(&response.errors[0]), Some("BAD_USER_INPUT"));
}

/// Build a multipart body exactly as `suku`'s transport expects it.
fn multipart_body(
    operations: &str,
    map: &str,
    files: &[(&str, &str, &str, &[u8])],
) -> (String, Vec<u8>) {
    const BOUNDARY: &str = "----reviewgurafiboundary";
    let mut body = Vec::new();
    let push = |body: &mut Vec<u8>,
                name: &str,
                filename: Option<&str>,
                content_type: Option<&str>,
                value: &[u8]| {
        body.extend_from_slice(format!("--{BOUNDARY}\r\n").as_bytes());
        match filename {
            Some(filename) => body.extend_from_slice(
                format!(
                    "Content-Disposition: form-data; name=\"{name}\"; filename=\"{filename}\"\r\n"
                )
                .as_bytes(),
            ),
            None => body.extend_from_slice(
                format!("Content-Disposition: form-data; name=\"{name}\"\r\n").as_bytes(),
            ),
        }
        if let Some(content_type) = content_type {
            body.extend_from_slice(format!("Content-Type: {content_type}\r\n").as_bytes());
        }
        body.extend_from_slice(b"\r\n");
        body.extend_from_slice(value);
        body.extend_from_slice(b"\r\n");
    };
    push(&mut body, "operations", None, None, operations.as_bytes());
    push(&mut body, "map", None, None, map.as_bytes());
    for (name, filename, content_type, bytes) in files {
        push(&mut body, name, Some(filename), Some(content_type), bytes);
    }
    body.extend_from_slice(format!("--{BOUNDARY}--\r\n").as_bytes());
    (format!("multipart/form-data; boundary={BOUNDARY}"), body)
}

#[test]
fn suku_multipart_body_reaches_a_gurafi_resolver() {
    let schema = upload_schema().build().expect("schema builds");
    let (content_type, body) = multipart_body(
        r#"{"query":"mutation($file: Upload!){ uploadFile(file:$file) }","variables":{"file":null}}"#,
        r#"{"0":["variables.file"]}"#,
        &[("0", "avatar.png", "image/png", b"\x89PNG")],
    );
    let payload = block_on(suku::multipart::parse(&content_type, &body, 32, 16))
        .expect("the multipart body parses");
    let suku::RequestPayload::Single(request) = payload else {
        panic!("expected a single request");
    };
    let variables = request.variables_object().expect("variables convert");
    let response = block_on(gql::execute(
        &schema,
        request.as_gql(variables.as_ref()),
        &gql::ExecutionOptions::with_context(Ctx::allowed()),
    ));
    assert!(
        response.errors.is_empty(),
        "unexpected errors: {response:?}"
    );
    assert_eq!(
        data(&response).get("uploadFile"),
        Some(&gql::Value::from("avatar.png:image/png:\u{fffd}PNG"))
    );
}

#[test]
fn upload_clone_shares_bytes_and_debug_hides_contents() {
    let upload = Upload::new("a.txt", "text/plain", b"top secret".to_vec());
    let cloned = upload.clone();
    assert_eq!(cloned.bytes(), upload.bytes());

    let first = cloned.into_bytes();
    let second = upload.into_bytes();
    assert!(
        Arc::ptr_eq(&first, &second),
        "clones must share the same byte buffer"
    );

    let debug = format!(
        "{:?}",
        Upload::new("a.txt", "text/plain", b"top secret".to_vec())
    );
    assert!(debug.contains("a.txt"), "{debug}");
    assert!(!debug.contains("top secret"), "{debug}");
}

#[test]
fn a_non_optional_upload_input_field_is_non_null() {
    // Every built-in input scalar returns `T!` from `InputType`; `Upload` returns the bare named
    // type, so `file: Upload` (without `Option`) is advertised as optional in SDL and
    // introspection.
    let schema = upload_schema().build().expect("schema builds");
    let argument = schema
        .object_type("Mutation")
        .and_then(|mutation| mutation.get_field("uploadFile"))
        .and_then(|field| field.args.get("file"))
        .expect("the `file` argument");
    assert_eq!(
        argument.ty.to_string(),
        "Upload!",
        "a non-optional `Upload` argument must be non-null"
    );
}

#[test]
fn custom_scalars_share_the_nullable_input_shape() {
    // Supporting evidence for the two probes above: this is not special to `Upload`. Every
    // custom scalar written with `impl_scalar_traits!`/`derive(Scalar)` returns the bare named
    // type from `InputType::input_type`, so a non-`Option` field is declared nullable.
    #[derive(gurafi::Scalar, Clone)]
    #[graphql(name = "Slug")]
    struct Slug(String);

    #[derive(Input)]
    struct SlugArgs {
        slug: Slug,
    }

    let mut schema = Schema::<Ctx>::new();
    schema.query(|q| {
        q.field("ok", |_: NoArgs, _: &Ctx| true);
    });
    schema.mutation(|m| {
        m.field("bySlug", |_: SlugArgs, _: &Ctx| String::new());
    });
    let schema = schema.build().expect("schema builds");
    let argument = schema
        .object_type("Mutation")
        .and_then(|mutation| mutation.get_field("bySlug"))
        .and_then(|field| field.args.get("slug"))
        .expect("the `slug` argument");
    assert_eq!(argument.ty.to_string(), "Slug");
}

#[test]
fn a_vec_of_upload_has_non_null_items() {
    // docs/guide/args.md: "`Vec<T>` produces a list argument. Items are non-null unless the Rust
    // item is `Option`." `Upload`'s nullable `InputType` makes `Vec<Upload>` print `[Upload]!`.
    let mut schema = Schema::<Ctx>::new();
    schema.query(|q| {
        q.field("ok", |_: NoArgs, _: &Ctx| true);
    });
    schema.mutation(|m| {
        m.field("many", |_: ManyArgs, _: &Ctx| String::new());
    });
    let schema = schema.build().expect("schema builds");
    let argument = schema
        .object_type("Mutation")
        .and_then(|mutation| mutation.get_field("many"))
        .and_then(|field| field.args.get("files"))
        .expect("the `files` argument");
    assert_eq!(argument.ty.to_string(), "[Upload!]!");
}

#[test]
fn omitting_upload_argument_is_a_request_error() {
    let schema = upload_schema().build().expect("schema builds");
    let response = run(&schema, "mutation { uploadFile }", Ctx::allowed());
    assert!(
        response.errors[0].message().contains("Upload!"),
        "unexpected errors: {response:?}"
    );
    assert_eq!(error_path(&response.errors[0]), None);
}

#[test]
fn upload_can_be_declared_without_a_signature() {
    let mut schema = Schema::<Ctx>::new();
    schema.scalar::<Upload>();
    schema.query(|q| {
        q.field("ok", |_: NoArgs, _: &Ctx| true);
    });
    let schema = schema.build().expect("schema builds");
    assert!(schema.get_type("Upload").is_some());
}

#[test]
fn upload_as_dynamic_output_is_an_unchecked_escape_hatch() {
    // Direct output use is a compile error because `Upload` has no `OutputType`. `as_dynamic`
    // is the documented unchecked escape hatch for runtime type references; the build cannot
    // know that the named scalar is input-only.
    let mut schema = Schema::<Ctx>::new();
    schema.scalar::<Upload>();
    schema.query(|q| {
        q.field("leak", |_: NoArgs, _: &Ctx| "not a file")
            .as_dynamic(gql::TypeRef::named("Upload"));
    });
    assert!(schema.build().is_ok());
}

#[test]
fn upload_as_dynamic_output_serializes_a_plain_string_if_built() {
    let mut schema = Schema::<Ctx>::new();
    schema.scalar::<Upload>();
    schema.query(|q| {
        q.field("leak", |_: NoArgs, _: &Ctx| "not a file")
            .as_dynamic(gql::TypeRef::named("Upload"));
    });
    let Ok(schema) = schema.build() else {
        // Correct behaviour: the build rejected the field.
        return;
    };
    let response = run(&schema, "{ leak }", Ctx::allowed());
    // Evidence for the report: the registered scalar's serializer clones the value, so
    // `Upload::to_value` (the intended runtime guard) never runs and a plain string leaks out
    // under the Upload type.
    assert_eq!(
        data(&response).get("leak").and_then(gql::Value::as_str),
        Some("not a file"),
        "{response:?}"
    );
}
