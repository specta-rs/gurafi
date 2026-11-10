//! Independent review probes for the Pothos-parity DX work (commit 78d3d8f).
//!
//! Each test states behaviour the documentation promises. A failure here is a defect report,
//! not a change request.
//!
//! The D1-D6 probes below started as `#[ignore]`d defect reports and are now regression tests.

#![allow(dead_code)]

use std::sync::{
    Arc,
    atomic::{AtomicUsize, Ordering},
};

use futures::{StreamExt, stream};
use gurafi::{
    Args, BuildOptions, Enum, GraphQLError, ID, Input, NoArgs, Object, Result as GurafiResult,
    Schema, expose,
};

#[derive(Clone, Debug)]
struct Ctx {
    logged_in: bool,
}

impl Ctx {
    fn anonymous() -> Self {
        Self { logged_in: false }
    }

    fn logged_in() -> Self {
        Self { logged_in: true }
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
        .expect("data")
        .as_object()
        .expect("object")
}

fn value(response: &gql::Response, field: &str) -> gql::Value {
    data(response)
        .get(field)
        .cloned()
        .unwrap_or_else(|| panic!("missing `{field}`: {response:?}"))
}

#[derive(Object)]
struct User {
    name: String,
    age: i32,
}

// =============================================================================================
// A6 — authorization
// =============================================================================================

#[test]
fn subscription_field_auth_denies_before_subscribe_runs() {
    let starts = Arc::new(AtomicUsize::new(0));
    let counter = Arc::clone(&starts);

    let mut schema = Schema::<Ctx>::new();
    schema.query(|q| {
        q.field("ok", |_: NoArgs, _: &Ctx| true);
    });
    schema.subscription(|s| {
        s.field("events", move |_: NoArgs, _: &Ctx| {
            counter.fetch_add(1, Ordering::SeqCst);
            stream::iter(vec![GurafiResult::Ok(1i32)])
        })
        .auth(|ctx: &Ctx| ctx.logged_in)
        .auth_error("no events for you");
    });
    let schema = schema.build().expect("schema builds");

    let denied = futures::executor::block_on(gql::subscribe(
        &schema,
        gql::Request::new("subscription { events }"),
        &gql::ExecutionOptions::with_context(Ctx::anonymous()),
    ))
    .expect_err("the subscription must be rejected");
    assert!(
        denied
            .errors
            .iter()
            .any(|error| error.message().contains("no events for you")),
        "{denied:?}"
    );
    assert_eq!(
        starts.load(Ordering::SeqCst),
        0,
        "the subscribe closure must not run when auth denies"
    );

    let allowed = futures::executor::block_on(gql::subscribe(
        &schema,
        gql::Request::new("subscription { events }"),
        &gql::ExecutionOptions::with_context(Ctx::logged_in()),
    ))
    .expect("the subscription starts");
    let events = futures::executor::block_on(allowed.stream.take(1).collect::<Vec<_>>());
    assert_eq!(events.len(), 1);
    assert!(!events[0].has_errors(), "{:?}", events[0]);
}

#[test]
fn interface_auth_is_inherited_through_a_parent_interface() {
    let mut schema = Schema::<Ctx>::new();
    let node = schema.interface("Node", |i| {
        i.auth(|ctx: &Ctx| ctx.logged_in)
            .auth_error("parent interface says no");
        i.field::<ID>("id");
    });
    let resource = schema.interface("Resource", |i| {
        i.implements(&node);
        // An interface that implements another must redeclare its fields (spec 3.6).
        i.field::<ID>("id");
        i.field::<String>("url");
    });
    schema.object::<User>(|o| {
        o.implements(&resource);
        // Transitive interfaces must be declared too (spec validator rule).
        o.implements(&node);
        o.field("id", |u: &User, _: NoArgs, _: &Ctx| ID::new(u.name.clone()));
        o.field("url", |u: &User, _: NoArgs, _: &Ctx| {
            format!("https://example.com/{}", u.name)
        });
    });
    schema.query(|q| {
        q.field("user", |_: NoArgs, _: &Ctx| User {
            name: "ada".into(),
            age: 36,
        });
    });
    let schema = schema.build().expect("schema builds");

    let denied = run(&schema, "{ user { url } }", Ctx::anonymous());
    assert!(
        denied
            .errors
            .iter()
            .any(|error| error.message().contains("parent interface says no")),
        "{denied:?}"
    );
    let allowed = run(&schema, "{ user { id url } }", Ctx::logged_in());
    assert!(!allowed.has_errors(), "{allowed:?}");
}

#[test]
fn auth_declared_on_a_reopened_interface_field_is_inherited() {
    let mut schema = Schema::<Ctx>::new();
    let node = schema.interface("Node", |i| {
        i.field::<ID>("id");
    });
    schema.interface_fields("Node", |i| {
        i.field::<String>("secret").auth(|ctx: &Ctx| ctx.logged_in);
    });
    schema.object::<User>(|o| {
        o.implements(&node);
        o.field("id", |u: &User, _: NoArgs, _: &Ctx| ID::new(u.name.clone()));
        o.field("secret", |_u: &User, _: NoArgs, _: &Ctx| "shh".to_owned());
    });
    schema.query(|q| {
        q.field("user", |_: NoArgs, _: &Ctx| User {
            name: "ada".into(),
            age: 36,
        });
    });
    let schema = schema.build().expect("schema builds");

    let denied = run(&schema, "{ user { secret } }", Ctx::anonymous());
    assert!(denied.has_errors(), "{denied:?}");
    let allowed = run(&schema, "{ user { secret } }", Ctx::logged_in());
    assert!(!allowed.has_errors(), "{allowed:?}");
}

#[test]
fn object_auth_declared_when_reopening_guards_earlier_fields() {
    let mut schema = Schema::<Ctx>::new();
    // Field declared first; the rule arrives with a later declaration.
    schema.object::<User>(|o| {
        o.expose(expose!(User::name));
    });
    schema.object_fields::<User>(|o| {
        o.auth(|ctx: &Ctx| ctx.logged_in);
        o.expose(expose!(User::age));
    });
    schema.query(|q| {
        q.field("user", |_: NoArgs, _: &Ctx| User {
            name: "ada".into(),
            age: 36,
        });
    });
    let schema = schema.build().expect("schema builds");

    let denied = run(&schema, "{ user { name } }", Ctx::anonymous());
    assert!(
        denied.has_errors(),
        "the rule must cover fields declared in the earlier call: {denied:?}"
    );
    let allowed = run(&schema, "{ user { name age } }", Ctx::logged_in());
    assert!(!allowed.has_errors(), "{allowed:?}");
}

#[test]
fn object_auth_is_not_lost_when_the_object_is_reopened_without_a_rule() {
    let mut schema = Schema::<Ctx>::new();
    schema.object::<User>(|o| {
        o.auth(|ctx: &Ctx| ctx.logged_in);
        o.expose(expose!(User::name));
    });
    schema.object_fields::<User>(|o| {
        o.expose(expose!(User::age));
    });
    schema.query(|q| {
        q.field("user", |_: NoArgs, _: &Ctx| User {
            name: "ada".into(),
            age: 36,
        });
    });
    let schema = schema.build().expect("schema builds");

    let denied = run(&schema, "{ user { age } }", Ctx::anonymous());
    assert!(denied.has_errors(), "{denied:?}");
}

#[test]
fn object_auth_guards_inherited_interface_resolvers() {
    let mut schema = Schema::<Ctx>::new();
    let node = schema.interface("Node", |i| {
        i.field::<ID>("id");
    });
    schema.object::<User>(|o| {
        o.auth(|ctx: &Ctx| ctx.logged_in);
        o.implements(&node);
        o.resolve_inherited("id", |u: &User, _: NoArgs, _: &Ctx| ID::new(u.name.clone()));
    });
    schema.query(|q| {
        q.field("user", |_: NoArgs, _: &Ctx| User {
            name: "ada".into(),
            age: 36,
        });
    });
    let schema = schema.build().expect("schema builds");

    let denied = run(&schema, "{ user { id } }", Ctx::anonymous());
    assert!(denied.has_errors(), "{denied:?}");
    let allowed = run(&schema, "{ user { id } }", Ctx::logged_in());
    assert!(!allowed.has_errors(), "{allowed:?}");
}

#[test]
fn async_interface_auth_is_inherited() {
    let mut schema = Schema::<Ctx>::new();
    let node = schema.interface("Node", |i| {
        i.auth_async(|ctx: &Ctx| Box::pin(async move { ctx.logged_in }));
        i.field::<ID>("id");
    });
    schema.object::<User>(|o| {
        o.implements(&node);
        o.field("id", |u: &User, _: NoArgs, _: &Ctx| ID::new(u.name.clone()));
    });
    schema.query(|q| {
        q.field("user", |_: NoArgs, _: &Ctx| User {
            name: "ada".into(),
            age: 36,
        });
    });
    let schema = schema.build().expect("schema builds");

    let denied = run(&schema, "{ user { id } }", Ctx::anonymous());
    assert!(denied.has_errors(), "{denied:?}");
    let allowed = run(&schema, "{ user { id } }", Ctx::logged_in());
    assert!(!allowed.has_errors(), "{allowed:?}");
}

#[test]
fn a_denied_non_null_field_nulls_its_parent() {
    #[derive(Object)]
    struct Boxed {
        label: String,
    }

    let mut schema = Schema::<Ctx>::new();
    schema.object::<Boxed>(|o| {
        o.field("label", |b: &Boxed, _: NoArgs, _: &Ctx| b.label.clone())
            .auth(|ctx: &Ctx| ctx.logged_in);
    });
    schema.query(|q| {
        // `Boxed` is nullable in gurafi, but the field inside it is `String!`.
        q.field("boxed", |_: NoArgs, _: &Ctx| {
            Some(Boxed { label: "x".into() })
        });
    });
    let schema = schema.build().expect("schema builds");

    let denied = run(&schema, "{ boxed { label } }", Ctx::anonymous());
    assert!(denied.has_errors(), "{denied:?}");
    assert_eq!(value(&denied, "boxed"), gql::Value::Null);
}

// =============================================================================================
// A3 — nullability and type overrides
// =============================================================================================

#[derive(gurafi::Scalar, Clone)]
#[graphql(name = "Date")]
struct Date(String);

#[test]
fn as_type_to_the_same_named_type_must_not_hide_a_nullability_change() {
    let mut schema = Schema::<Ctx>::new();
    schema.query(|q| {
        // The resolver can return `None`, but the override claims `String!`.
        q.field("maybe", |_: NoArgs, _: &Ctx| Some("value".to_owned()))
            .as_type::<String>();
    });
    let message = match schema.build() {
        Ok(schema) => panic!(
            "`Option<String>` cannot honestly override itself as `String!`; the field was \
             declared `{}`",
            schema
                .object_type("Query")
                .and_then(|query| query.get_field("maybe"))
                .expect("maybe")
                .ty
        ),
        Err(error) => error.to_string(),
    };
    assert!(
        message.contains("not compatible") || message.contains("optional"),
        "{message}"
    );
}

#[test]
fn nullable_items_only_touches_the_item_wrapper() {
    let mut schema = Schema::<Ctx>::new();
    schema.query(|q| {
        q.field("a", |_: NoArgs, _: &Ctx| vec!["x".to_owned()])
            .nullable_items();
        q.field("b", |_: NoArgs, _: &Ctx| Some(vec!["x".to_owned()]))
            .nullable_items();
        q.field("c", |_: NoArgs, _: &Ctx| vec![Some("x".to_owned())])
            .nullable_items();
    });
    let schema = schema.build().expect("schema builds");
    let query = schema.object_type("Query").expect("Query");
    assert_eq!(query.get_field("a").unwrap().ty.to_string(), "[String]!");
    assert_eq!(query.get_field("b").unwrap().ty.to_string(), "[String]");
    assert_eq!(query.get_field("c").unwrap().ty.to_string(), "[String]!");
}

#[test]
fn nullable_items_is_idempotent_and_composes_with_nullable() {
    let mut schema = Schema::<Ctx>::new();
    schema.query(|q| {
        q.field("a", |_: NoArgs, _: &Ctx| vec!["x".to_owned()])
            .nullable_items()
            .nullable_items()
            .nullable();
    });
    let schema = schema.build().expect("schema builds");
    let field = schema
        .object_type("Query")
        .expect("Query")
        .get_field("a")
        .expect("a");
    assert_eq!(field.ty.to_string(), "[String]");
}

// =============================================================================================
// A7 — validation hooks
// =============================================================================================

static VALIDATIONS: AtomicUsize = AtomicUsize::new(0);

fn count_validation(args: &CountedArgs) -> Result<(), GraphQLError> {
    VALIDATIONS.fetch_add(1, Ordering::SeqCst);
    if args.value > 10 {
        Err(GraphQLError::new("too large"))
    } else {
        Ok(())
    }
}

#[derive(Input)]
#[graphql(validate = "count_validation")]
struct CountedArgs {
    #[graphql(default = 1)]
    value: i32,
}

#[test]
fn input_validation_runs_exactly_once_per_execution() {
    VALIDATIONS.store(0, Ordering::SeqCst);
    let mut schema = Schema::<Ctx>::new();
    schema.query(|q| {
        q.field("counted", |args: CountedArgs, _: &Ctx| args.value);
    });
    let schema = schema.build().expect("schema builds");

    let response = run(&schema, "{ counted }", Ctx::anonymous());
    assert!(!response.has_errors(), "{response:?}");
    assert_eq!(VALIDATIONS.load(Ordering::SeqCst), 1);
}

#[test]
fn field_validate_runs_exactly_once_per_execution() {
    let calls = Arc::new(AtomicUsize::new(0));
    let counter = Arc::clone(&calls);
    let mut schema = Schema::<Ctx>::new();
    schema.query(|q| {
        q.field("counted", |_: NoArgs, _: &Ctx| 1i32)
            .validate::<NoArgs>(move |_, _| {
                counter.fetch_add(1, Ordering::SeqCst);
                Ok(())
            });
    });
    let schema = schema.build().expect("schema builds");
    let response = run(&schema, "{ counted }", Ctx::anonymous());
    assert!(!response.has_errors(), "{response:?}");
    assert_eq!(calls.load(Ordering::SeqCst), 1);
}

#[test]
fn validation_errors_are_field_errors_with_the_right_path() {
    let mut schema = Schema::<Ctx>::new();
    schema.object::<User>(|o| {
        o.field("guarded", |u: &User, args: CountedArgs, _: &Ctx| {
            format!("{}-{}", u.name, args.value)
        });
    });
    schema.query(|q| {
        q.field("user", |_: NoArgs, _: &Ctx| User {
            name: "ada".into(),
            age: 36,
        });
    });
    let schema = schema.build().expect("schema builds");

    let response = run(&schema, "{ user { guarded(value: 11) } }", Ctx::anonymous());
    assert!(response.has_errors(), "{response:?}");
    let path = response.errors[0].path().expect("a path on a field error");
    assert_eq!(path.len(), 2, "{path:?}");
}

#[test]
fn validate_on_a_subscription_field_applies_to_each_subscription() {
    let calls = Arc::new(AtomicUsize::new(0));
    let counter = Arc::clone(&calls);
    let mut schema = Schema::<Ctx>::new();
    schema.query(|q| {
        q.field("ok", |_: NoArgs, _: &Ctx| true);
    });
    schema.subscription(|s| {
        s.field("ticks", |_: NoArgs, _: &Ctx| {
            stream::iter(vec![GurafiResult::Ok(1i32)])
        })
        .validate::<NoArgs>(move |_, _| {
            counter.fetch_add(1, Ordering::SeqCst);
            Ok(())
        });
    });
    let schema = schema.build().expect("schema builds");
    let response = futures::executor::block_on(gql::subscribe(
        &schema,
        gql::Request::new("subscription { ticks }"),
        &gql::ExecutionOptions::with_context(Ctx::anonymous()),
    ))
    .expect("subscription starts");
    let events = futures::executor::block_on(response.stream.take(1).collect::<Vec<_>>());
    assert!(!events[0].has_errors(), "{:?}", events[0]);
    assert_eq!(
        calls.load(Ordering::SeqCst),
        1,
        "a one-event subscription should validate its arguments once"
    );
}

// =============================================================================================
// A8 — resolve_inherited
// =============================================================================================

#[test]
fn resolve_inherited_accepts_a_non_null_override_of_a_nullable_interface_field() {
    let mut schema = Schema::<Ctx>::new();
    let node = schema.interface("Node", |i| {
        i.field::<Option<String>>("label");
    });
    schema.object::<User>(|o| {
        o.implements(&node);
        o.resolve_inherited("label", |u: &User, _: NoArgs, _: &Ctx| u.name.clone());
    });
    schema.query(|q| {
        q.field("user", |_: NoArgs, _: &Ctx| User {
            name: "ada".into(),
            age: 36,
        });
    });
    let schema = schema.build().expect("covariant return type builds");
    let field = schema
        .object_type("User")
        .unwrap()
        .get_field("label")
        .expect("label");
    // The declared type stays the interface's (nullable), so a value is always valid.
    assert_eq!(field.ty.to_string(), "String");
}

#[test]
fn resolve_inherited_walks_parent_interfaces() {
    let mut schema = Schema::<Ctx>::new();
    let node = schema.interface("Node", |i| {
        i.field::<ID>("id");
    });
    let resource = schema.interface("Resource", |i| {
        i.implements(&node);
        i.field::<ID>("id");
        i.field::<String>("url");
    });
    schema.object::<User>(|o| {
        o.implements(&resource);
        o.implements(&node);
        o.resolve_inherited("id", |u: &User, _: NoArgs, _: &Ctx| ID::new(u.name.clone()));
        o.resolve_inherited("url", |u: &User, _: NoArgs, _: &Ctx| {
            format!("https://example.com/{}", u.name)
        });
    });
    schema.query(|q| {
        q.field("user", |_: NoArgs, _: &Ctx| User {
            name: "ada".into(),
            age: 36,
        });
    });
    let schema = schema.build().expect("schema builds");
    let user = schema.object_type("User").expect("User");
    assert!(user.get_field("id").is_some());
    assert!(user.get_field("url").is_some());
}

#[test]
fn resolve_inherited_keeps_the_interface_arguments_and_defaults() {
    #[derive(Input)]
    #[graphql(name = "PageInput")]
    struct Page {
        #[graphql(default = 20)]
        first: i32,
    }

    let mut schema = Schema::<Ctx>::new();
    let node = schema.interface("Node", |i| {
        i.field_with::<String, Page>("page").description("A page.");
    });
    schema.object::<User>(|o| {
        o.implements(&node);
        o.resolve_inherited("page", |u: &User, args: Page, _: &Ctx| {
            format!("{}:{}", u.name, args.first)
        });
    });
    schema.query(|q| {
        q.field("user", |_: NoArgs, _: &Ctx| User {
            name: "ada".into(),
            age: 36,
        });
    });
    let schema = schema.build().expect("schema builds");
    let field = schema
        .object_type("User")
        .unwrap()
        .get_field("page")
        .expect("page");
    let arg = field.args.get("first").expect("inherited argument");
    assert!(arg.default_value.is_some(), "the default must be inherited");

    let response = run(&schema, "{ user { page } }", Ctx::anonymous());
    assert!(!response.has_errors(), "{response:?}");
    assert_eq!(
        value(&response, "user")
            .as_object()
            .and_then(|user| user.get("page"))
            .and_then(gql::Value::as_str),
        Some("ada:20")
    );
}

#[test]
fn resolve_inherited_rejects_a_nullable_resolver_for_a_non_null_interface_field() {
    let mut schema = Schema::<Ctx>::new();
    let node = schema.interface("Node", |i| {
        i.field::<ID>("id");
    });
    schema.object::<User>(|o| {
        o.implements(&node);
        o.resolve_inherited("id", |_u: &User, _: NoArgs, _: &Ctx| Some(ID::new("x")));
    });
    schema.query(|q| {
        q.field("user", |_: NoArgs, _: &Ctx| User {
            name: "ada".into(),
            age: 36,
        });
    });
    let error = schema
        .build()
        .expect_err("a nullable resolver cannot satisfy a non-null interface field");
    assert!(error.to_string().contains("inherits"), "{error}");
}

// =============================================================================================
// A1 — reopening
// =============================================================================================

#[test]
fn reopening_preserves_object_is_type_of_and_interface_resolve_type() {
    #[derive(Object)]
    struct Cat {
        name: String,
    }

    let mut schema = Schema::<Ctx>::new();
    let pet = schema.interface("Pet", |i| {
        i.field::<String>("name");
    });
    schema.interface_fields("Pet", |i| {
        i.description("A pet.");
    });
    schema.object::<Cat>(|o| {
        o.implements(&pet);
        o.expose(expose!(Cat::name));
    });
    schema.object_fields::<Cat>(|o| {
        o.field("lives", |_: &Cat, _: NoArgs, _: &Ctx| 9i32);
    });
    schema.query(|q| {
        q.field("pet", |_: NoArgs, _: &Ctx| Cat { name: "Tom".into() })
            .as_abstract(&pet);
    });
    let schema = schema.build().expect("schema builds");
    assert_eq!(
        schema
            .interface_type("Pet")
            .and_then(|i| i.description.as_deref()),
        Some("A pet.")
    );
    let response = run(&schema, "{ pet { __typename name } }", Ctx::anonymous());
    assert!(!response.has_errors(), "{response:?}");
    assert_eq!(
        value(&response, "pet")
            .as_object()
            .and_then(|pet| pet.get("__typename"))
            .and_then(gql::Value::as_str),
        Some("Cat")
    );
}

#[test]
fn reopening_a_union_preserves_description_members_and_resolve_type() {
    #[derive(Object)]
    struct Dog {
        name: String,
    }

    let mut schema = Schema::<Ctx>::new();
    schema.object::<Dog>(|o| {
        o.expose(expose!(Dog::name));
    });
    schema.object::<User>(|o| {
        o.expose(expose!(User::name));
    });
    let pet = schema.union("Pet", |u| {
        u.description("Any pet.").member::<Dog>();
    });
    schema.union("Pet", |u| {
        u.member::<User>();
    });
    schema.query(|q| {
        q.field("pet", |_: NoArgs, _: &Ctx| Dog { name: "Rex".into() })
            .as_abstract(&pet);
    });
    let schema = schema.build().expect("schema builds");
    let union = schema.union_type("Pet").expect("Pet");
    assert_eq!(union.description.as_deref(), Some("Any pet."));
    assert_eq!(union.types.len(), 2);
    let response = run(&schema, "{ pet { __typename } }", Ctx::anonymous());
    assert!(!response.has_errors(), "{response:?}");
}

#[test]
fn root_fields_keep_their_options_when_reopened() {
    let mut schema = Schema::<Ctx>::new();
    schema.query(|q| {
        q.field("first", |_: NoArgs, _: &Ctx| 1i32)
            .description("First.");
    });
    schema.query_fields(|q| {
        q.field("second", |_: NoArgs, _: &Ctx| 2i32);
    });
    let schema = schema.build().expect("schema builds");
    let query = schema.object_type("Query").expect("Query");
    assert_eq!(
        query.get_field("first").unwrap().description.as_deref(),
        Some("First.")
    );
    assert!(query.get_field("second").is_some());
}

#[test]
fn object_field_then_object_fields_keeps_the_generated_metadata() {
    let mut schema = Schema::<Ctx>::new();
    schema.object_field::<User, String, NoArgs, _>("name", |u: &User, _: NoArgs, _: &Ctx| {
        u.name.clone()
    });
    schema.object_fields::<User>(|o| {
        o.field("age", |u: &User, _: NoArgs, _: &Ctx| u.age);
    });
    schema.query(|q| {
        q.field("user", |_: NoArgs, _: &Ctx| User {
            name: "ada".into(),
            age: 36,
        });
    });
    let schema = schema.build().expect("schema builds");
    assert_eq!(
        schema
            .object_type("User")
            .unwrap()
            .get_field("age")
            .map(|f| f.ty.to_string()),
        Some("Int!".to_owned())
    );
}

// =============================================================================================
// A2 — args-only inputs
// =============================================================================================

#[derive(Args)]
struct ListArgs {
    limit: Option<i32>,
}

#[test]
fn unnamed_input_nested_through_a_list_is_an_actionable_error() {
    #[derive(Input)]
    struct BadNested {
        text: String,
    }

    #[derive(Input)]
    struct BadArgs {
        nested: Option<Vec<BadNested>>,
    }

    let mut schema = Schema::<Ctx>::new();
    schema.query(|q| {
        q.field("bad", |_: BadArgs, _: &Ctx| true);
    });
    let error = schema.build().expect_err("unnamed nested input must fail");
    let message = error.to_string();
    assert!(
        message.contains("used as a nested input type") && message.contains("graphql(name"),
        "{message}"
    );
}

#[test]
fn args_only_inputs_do_not_appear_in_introspection() {
    let mut schema = Schema::<Ctx>::new();
    schema.query(|q| {
        q.field("list", |_: ListArgs, _: &Ctx| 1i32);
    });
    let schema = schema.build().expect("schema builds");
    let response = run(
        &schema,
        r#"{ __type(name: "ListArgs") { name } types: __schema { types { name } } }"#,
        Ctx::anonymous(),
    );
    assert!(!response.has_errors(), "{response:?}");
    assert_eq!(value(&response, "__type"), gql::Value::Null);
    let types_value = value(&response, "types");
    let names = types_value
        .as_object()
        .and_then(|schema| schema.get("types"))
        .and_then(gql::Value::as_list)
        .expect("types")
        .iter()
        .filter_map(|ty| ty.as_object().and_then(|object| object.get("name")))
        .filter_map(gql::Value::as_str)
        .collect::<Vec<_>>();
    assert!(
        !names.iter().any(|name| name.contains("ListArgs")),
        "{names:?}"
    );
    assert!(names.contains(&"Query"), "{names:?}");
}

#[test]
fn unnamed_input_used_as_interface_field_arguments_works() {
    let mut schema = Schema::<Ctx>::new();
    let node = schema.interface("Node", |i| {
        i.field_with::<String, ListArgs>("label");
    });
    schema.object::<User>(|o| {
        o.implements(&node);
        o.field("label", |u: &User, args: ListArgs, _: &Ctx| {
            format!("{}:{:?}", u.name, args.limit)
        });
    });
    schema.query(|q| {
        q.field("user", |_: NoArgs, _: &Ctx| User {
            name: "ada".into(),
            age: 36,
        });
    });
    let schema = schema.build().expect("schema builds");
    let response = run(&schema, "{ user { label(limit: 2) } }", Ctx::anonymous());
    assert!(!response.has_errors(), "{response:?}");
    assert_eq!(
        value(&response, "user")
            .as_object()
            .and_then(|user| user.get("label"))
            .and_then(gql::Value::as_str),
        Some("ada:Some(2)")
    );
}

#[test]
fn a_named_input_used_directly_as_arguments_is_still_a_named_type() {
    #[derive(Input)]
    #[graphql(name = "NamedArgs")]
    struct NamedArgs {
        value: i32,
    }

    let mut schema = Schema::<Ctx>::new();
    schema.query(|q| {
        q.field("named", |args: NamedArgs, _: &Ctx| args.value);
    });
    let schema = schema.build().expect("schema builds");
    let sdl = schema.sdl();
    assert!(sdl.contains("input NamedArgs"), "{sdl}");
    assert!(sdl.contains("named(value: Int!)"), "{sdl}");
}

// =============================================================================================
// A10 — oneOf
// =============================================================================================

#[derive(Input)]
#[graphql(name = "SearchBy", one_of)]
struct SearchBy {
    id: Option<ID>,
    name: Option<String>,
}

#[derive(Input)]
struct SearchArgs {
    by: SearchBy,
}

fn one_of_schema() -> gql::Schema {
    let mut schema = Schema::<Ctx>::new();
    schema.query(|q| {
        q.field("search", |args: SearchArgs, _: &Ctx| {
            let by = args.by;
            format!("id={:?},name={:?}", by.id.map(|id| id.to_string()), by.name)
        });
    });
    schema.build().expect("schema builds")
}

#[test]
fn one_of_is_enforced_for_variables_as_well_as_literals() {
    let schema = one_of_schema();
    let run_with = |variables: gql::Object| -> gql::Response {
        futures::executor::block_on(gql::execute(
            &schema,
            gql::Request::new("query Q($by: SearchBy!) { search(by: $by) }").variables(&variables),
            &gql::ExecutionOptions::with_context(Ctx::anonymous()),
        ))
    };
    let by = |fields: Vec<(&str, gql::Value)>| -> gql::Object {
        let mut variables = gql::Object::new();
        variables.insert("by", gql::Value::object(gql::Object::from_fields(fields)));
        variables
    };

    let ok = run_with(by(vec![("id", gql::Value::String(Arc::from("1")))]));
    assert!(!ok.has_errors(), "{ok:?}");

    let none = run_with(by(vec![]));
    assert!(none.has_errors(), "{none:?}");

    let both = run_with(by(vec![
        ("id", gql::Value::String(Arc::from("1"))),
        ("name", gql::Value::String(Arc::from("ada"))),
    ]));
    assert!(both.has_errors(), "{both:?}");

    let null = run_with(by(vec![("id", gql::Value::Null)]));
    assert!(null.has_errors(), "{null:?}");
}

#[test]
fn one_of_shows_up_in_introspection() {
    let schema = one_of_schema();
    let response = run(
        &schema,
        r#"{ __type(name: "SearchBy") { isOneOf inputFields { name type { kind name } } } }"#,
        Ctx::anonymous(),
    );
    assert!(!response.has_errors(), "{response:?}");
    let ty = value(&response, "__type");
    assert_eq!(
        ty.as_object()
            .and_then(|ty| ty.get("isOneOf"))
            .and_then(gql::Value::as_bool),
        Some(true),
        "{ty:?}"
    );
}

// =============================================================================================
// N2 — subscriptions
// =============================================================================================

#[test]
fn subscription_alias_and_arguments_both_work() {
    #[derive(Input)]
    struct TickArgs {
        limit: i32,
    }

    let mut schema = Schema::<Ctx>::new();
    schema.query(|q| {
        q.field("ok", |_: NoArgs, _: &Ctx| true);
    });
    schema.subscription(|s| {
        s.field("ticks", |args: TickArgs, _: &Ctx| {
            stream::iter((0..args.limit).map(GurafiResult::Ok))
        });
    });
    let schema = schema.build().expect("schema builds");
    let response = futures::executor::block_on(gql::subscribe(
        &schema,
        gql::Request::new("subscription { alias: ticks(limit: 3) }"),
        &gql::ExecutionOptions::with_context(Ctx::anonymous()),
    ))
    .expect("subscription starts");
    let events = futures::executor::block_on(response.stream.collect::<Vec<_>>());
    assert_eq!(events.len(), 3);
    assert_eq!(
        data(&events[0]).get("alias"),
        Some(&gql::Value::Int(0)),
        "{:?}",
        events[0]
    );
}

#[test]
fn subscription_events_can_be_object_models() {
    let mut schema = Schema::<Ctx>::new();
    schema.object::<User>(|o| {
        o.expose(expose!(User::name));
        o.expose(expose!(User::age));
    });
    schema.query(|q| {
        q.field("ok", |_: NoArgs, _: &Ctx| true);
    });
    schema.subscription(|s| {
        s.field("userJoined", |_: NoArgs, _: &Ctx| {
            stream::iter(vec![GurafiResult::Ok(User {
                name: "ada".into(),
                age: 36,
            })])
        });
    });
    let schema = schema.build().expect("schema builds");
    let response = futures::executor::block_on(gql::subscribe(
        &schema,
        gql::Request::new("subscription { userJoined { name age } }"),
        &gql::ExecutionOptions::with_context(Ctx::anonymous()),
    ))
    .expect("subscription starts");
    let events = futures::executor::block_on(response.stream.take(1).collect::<Vec<_>>());
    assert!(!events[0].has_errors(), "{:?}", events[0]);
    let joined = data(&events[0])
        .get("userJoined")
        .and_then(gql::Value::as_object);
    assert_eq!(
        joined
            .and_then(|user| user.get("name"))
            .and_then(gql::Value::as_str),
        Some("ada")
    );
}

#[test]
fn field_map_errors_become_field_errors_not_wrapped_failures() {
    #[derive(Clone)]
    struct Event {
        fail: bool,
    }

    let mut schema = Schema::<Ctx>::new();
    schema.query(|q| {
        q.field("ok", |_: NoArgs, _: &Ctx| true);
    });
    schema.subscription(|s| {
        s.field_map::<NoArgs, Event, String, _, _, _>(
            "mapped",
            |_: NoArgs, _: &Ctx| stream::iter(vec![GurafiResult::Ok(Event { fail: true })]),
            |event: &Event, _: NoArgs, _: &Ctx| {
                if event.fail {
                    Err(GraphQLError::new("mapping failed"))
                } else {
                    Ok("fine".to_owned())
                }
            },
        );
    });
    let schema = schema.build().expect("schema builds");
    let response = futures::executor::block_on(gql::subscribe(
        &schema,
        gql::Request::new("subscription { mapped }"),
        &gql::ExecutionOptions::with_context(Ctx::anonymous()),
    ))
    .expect("subscription starts");
    let events = futures::executor::block_on(response.stream.take(1).collect::<Vec<_>>());
    assert!(events[0].has_errors(), "{:?}", events[0]);
    assert_eq!(events[0].errors[0].message(), "mapping failed");
    let path = events[0].errors[0].path().expect("path");
    assert_eq!(path.len(), 1, "{path:?}");
}

// =============================================================================================
// A11 — clone / build options
// =============================================================================================

#[test]
fn building_from_a_clone_does_not_leak_guards_or_inheritance() {
    let mut schema = Schema::<Ctx>::new();
    let node = schema.interface("Node", |i| {
        i.field::<ID>("id");
    });
    schema.object::<User>(|o| {
        o.auth(|ctx: &Ctx| ctx.logged_in);
        o.implements(&node);
        o.resolve_inherited("id", |u: &User, _: NoArgs, _: &Ctx| ID::new(u.name.clone()));
        o.expose(expose!(User::name));
    });
    schema.query(|q| {
        q.field("user", |_: NoArgs, _: &Ctx| User {
            name: "ada".into(),
            age: 36,
        });
    });

    let first = schema.build_clone().expect("first build");
    let second = schema.build_clone().expect("second build");
    for schema in [&first, &second] {
        // `id` is `ID!`: a double-wrapped guard does not change the answer, but a lost guard or
        // a re-run of inheritance would. Check the allowed path.
        let allowed = run(schema, "{ user { id name } }", Ctx::logged_in());
        assert!(!allowed.has_errors(), "{allowed:?}");
        let denied = run(schema, "{ user { name } }", Ctx::anonymous());
        assert!(denied.has_errors(), "{denied:?}");
    }
}

#[test]
fn build_options_sort_types_and_describe_roots() {
    let mut schema = Schema::<Ctx>::new();
    schema.object::<User>(|o| {
        o.expose(expose!(User::name));
    });
    schema.query(|q| {
        q.field("user", |_: NoArgs, _: &Ctx| User {
            name: "ada".into(),
            age: 36,
        });
    });
    let schema = schema
        .build_with(
            BuildOptions::new()
                .query_description("The root.")
                .sort_types(true),
        )
        .expect("schema builds");
    assert_eq!(
        schema
            .object_type("Query")
            .and_then(|q| q.description.as_deref()),
        Some("The root.")
    );
}

// =============================================================================================
// A4 — resolver info
// =============================================================================================

#[test]
fn info_path_includes_aliases_and_nesting() {
    let mut schema = Schema::<Ctx>::new();
    schema.object::<User>(|o| {
        o.field_with_info("path", |_: &User, _: NoArgs, _: &Ctx, info| {
            info.path
                .iter()
                .map(|segment| format!("{segment}"))
                .collect::<Vec<_>>()
                .join("/")
        });
    });
    schema.query(|q| {
        q.field("user", |_: NoArgs, _: &Ctx| User {
            name: "ada".into(),
            age: 36,
        });
    });
    let schema = schema.build().expect("schema builds");
    let response = run(&schema, "{ u: user { p: path } }", Ctx::anonymous());
    assert!(!response.has_errors(), "{response:?}");
    assert_eq!(
        value(&response, "u")
            .as_object()
            .and_then(|user| user.get("p"))
            .and_then(gql::Value::as_str),
        Some("u/p")
    );
}

#[test]
fn info_reports_the_field_type_and_parent() {
    let mut schema = Schema::<Ctx>::new();
    schema.object::<User>(|o| {
        o.field("name", |u: &User, _: NoArgs, _: &Ctx| u.name.clone());
    });
    schema.query(|q| {
        q.field_result_with_info(
            "describe",
            |_: NoArgs, _: &Ctx, info: &gql::ResolveInfo<'_>| {
                GurafiResult::Ok(format!(
                    "{}|{}|{}",
                    info.parent_type, info.field_name, info.return_type
                ))
            },
        );
    });
    let schema = schema.build().expect("schema builds");
    let response = run(&schema, "{ describe }", Ctx::anonymous());
    assert!(!response.has_errors(), "{response:?}");
    assert_eq!(
        value(&response, "describe").as_str(),
        Some("Query|describe|String!")
    );
}

// =============================================================================================
// Second batch — edge cases found while reading the implementation
// =============================================================================================

#[derive(Object)]
struct Cat {
    name: String,
}

#[test]
fn union_resolve_type_survives_reopening() {
    let mut schema = Schema::<Ctx>::new();
    schema.object::<Cat>(|o| {
        o.expose(expose!(Cat::name));
    });
    schema.object::<User>(|o| {
        o.expose(expose!(User::name));
    });
    let pet = schema.union("Pet", |u| {
        u.member::<Cat>()
            .member::<User>()
            // Force every value to `User`, ignoring `is_type_of`.
            .resolve_type(|_, _| Ok(Arc::from("User")));
    });
    schema.union("Pet", |u| {
        u.description("Reopened.");
    });
    schema.query(|q| {
        q.field("pet", |_: NoArgs, _: &Ctx| Cat { name: "Tom".into() })
            .as_abstract(&pet);
    });
    let schema = schema.build().expect("schema builds");
    let response = run(&schema, "{ pet { __typename } }", Ctx::anonymous());
    assert!(!response.has_errors(), "{response:?}");
    assert_eq!(
        value(&response, "pet")
            .as_object()
            .and_then(|pet| pet.get("__typename"))
            .and_then(gql::Value::as_str),
        Some("User"),
        "a custom resolve_type must survive reopening"
    );
}

#[test]
fn interface_resolve_type_survives_reopening() {
    let mut schema = Schema::<Ctx>::new();
    let pet = schema.interface("Pet", |i| {
        i.field::<String>("name");
    });
    schema.interface_fields("Pet", |i| {
        i.resolve_type(|_, _| Ok(Arc::from("User")));
        i.field::<i32>("age");
    });
    schema.object::<Cat>(|o| {
        o.implements(&pet);
        o.expose(expose!(Cat::name));
        o.field("age", |_: &Cat, _: NoArgs, _: &Ctx| 9i32);
    });
    schema.object::<User>(|o| {
        o.implements(&pet);
        o.expose(expose!(User::name));
        o.field("age", |u: &User, _: NoArgs, _: &Ctx| u.age);
    });
    schema.object_fields::<User>(|o| {
        o.field("extra", |_: &User, _: NoArgs, _: &Ctx| true);
    });
    schema.query(|q| {
        q.field("pet", |_: NoArgs, _: &Ctx| Cat { name: "Tom".into() })
            .as_abstract(&pet);
    });
    let schema = schema.build().expect("schema builds");
    let response = run(&schema, "{ pet { __typename } }", Ctx::anonymous());
    assert!(!response.has_errors(), "{response:?}");
    assert_eq!(
        value(&response, "pet")
            .as_object()
            .and_then(|pet| pet.get("__typename"))
            .and_then(gql::Value::as_str),
        Some("User")
    );
}

#[test]
fn interface_auth_does_not_leak_to_unrelated_objects_with_similar_fields() {
    let mut schema = Schema::<Ctx>::new();
    // The interface has no implementors and a rule nobody should inherit.
    schema.interface("Isolated", |i| {
        i.auth(|_ctx: &Ctx| false);
        i.field::<String>("name");
    });
    schema.object::<User>(|o| {
        o.expose(expose!(User::name));
    });
    schema.query(|q| {
        q.field("user", |_: NoArgs, _: &Ctx| User {
            name: "ada".into(),
            age: 36,
        });
    });
    let schema = schema.build().expect("schema builds");
    let response = run(&schema, "{ user { name } }", Ctx::anonymous());
    assert!(!response.has_errors(), "{response:?}");
}

#[test]
fn field_and_object_auth_compose_but_each_denial_is_a_field_error() {
    let mut schema = Schema::<Ctx>::new();
    schema.object::<User>(|o| {
        o.auth(|ctx: &Ctx| ctx.logged_in);
        o.field("verySecret", |_: &User, _: NoArgs, _: &Ctx| "x")
            .auth(|_ctx: &Ctx| false)
            .auth_error("field rule");
    });
    schema.query(|q| {
        q.field("user", |_: NoArgs, _: &Ctx| User {
            name: "ada".into(),
            age: 36,
        });
    });
    let schema = schema.build().expect("schema builds");

    let denied_object = run(&schema, "{ user { verySecret } }", Ctx::anonymous());
    assert!(
        denied_object
            .errors
            .iter()
            .any(|error| error.message().contains("Not authorized")),
        "{denied_object:?}"
    );
    let denied_field = run(&schema, "{ user { verySecret } }", Ctx::logged_in());
    assert!(
        denied_field
            .errors
            .iter()
            .any(|error| error.message().contains("field rule")),
        "{denied_field:?}"
    );
}

#[test]
fn as_type_nullability_mismatch_is_a_build_error() {
    let mut schema = Schema::<Ctx>::new();
    schema.query(|q| {
        q.field("maybe", |_: NoArgs, _: &Ctx| None::<String>)
            .as_type::<String>();
    });
    let error = schema
        .build()
        .expect_err("a nullable resolver cannot be declared `String!`");
    assert!(
        error.to_string().contains("not compatible") || error.to_string().contains("optional"),
        "{error}"
    );
}

#[test]
fn validate_with_the_wrong_args_type_is_a_build_error() {
    let called = Arc::new(AtomicUsize::new(0));
    let counter = Arc::clone(&called);
    let mut schema = Schema::<Ctx>::new();
    schema.query(|q| {
        // The field takes no arguments, but the validator names a different args type.
        q.field("noargs", |_: NoArgs, _: &Ctx| 1i32)
            .validate::<CountedArgs>(move |_, _| {
                counter.fetch_add(1, Ordering::SeqCst);
                Ok(())
            });
    });
    let error = schema
        .build()
        .expect_err("a validator must name the field's arguments type");
    assert!(
        error.to_string().contains("validate::<") && error.to_string().contains("arguments type"),
        "{error}"
    );
    assert_eq!(
        called.load(Ordering::SeqCst),
        0,
        "the validator must not run with fabricated arguments"
    );
}

#[test]
fn field_map_validation_count_per_event() {
    #[derive(Clone)]
    struct Event;

    static MAP_VALIDATIONS: AtomicUsize = AtomicUsize::new(0);
    MAP_VALIDATIONS.store(0, Ordering::SeqCst);

    let mut schema = Schema::<Ctx>::new();
    schema.query(|q| {
        q.field("ok", |_: NoArgs, _: &Ctx| true);
    });
    schema.subscription(|s| {
        s.field_map::<NoArgs, Event, i32, _, _, _>(
            "mapped",
            |_: NoArgs, _: &Ctx| stream::iter(vec![GurafiResult::Ok(Event)]),
            |_: &Event, _: NoArgs, _: &Ctx| Ok(1i32),
        )
        .validate::<NoArgs>(|_, _| {
            MAP_VALIDATIONS.fetch_add(1, Ordering::SeqCst);
            Ok(())
        });
    });
    let schema = schema.build().expect("schema builds");
    let response = futures::executor::block_on(gql::subscribe(
        &schema,
        gql::Request::new("subscription { mapped }"),
        &gql::ExecutionOptions::with_context(Ctx::anonymous()),
    ))
    .expect("subscription starts");
    let events = futures::executor::block_on(response.stream.take(1).collect::<Vec<_>>());
    assert!(!events[0].has_errors(), "{:?}", events[0]);
    assert_eq!(
        MAP_VALIDATIONS.load(Ordering::SeqCst),
        1,
        "one event should validate its arguments once"
    );
}

#[test]
fn duplicate_root_fields_are_a_build_error() {
    let mut schema = Schema::<Ctx>::new();
    schema.query(|q| {
        q.field("dupe", |_: NoArgs, _: &Ctx| 1i32);
    });
    schema.query_fields(|q| {
        q.field("dupe", |_: NoArgs, _: &Ctx| 2i32);
    });
    let error = schema.build().expect_err("duplicate root fields must fail");
    assert!(error.to_string().contains("more than once"), "{error}");
}

#[test]
fn subscription_option_event_is_wrapped_as_null_not_dropped() {
    let mut schema = Schema::<Ctx>::new();
    schema.query(|q| {
        q.field("ok", |_: NoArgs, _: &Ctx| true);
    });
    schema.subscription(|s| {
        s.field("maybe", |_: NoArgs, _: &Ctx| {
            stream::iter(vec![GurafiResult::Ok(None::<String>)])
        });
    });
    let schema = schema.build().expect("schema builds");
    let response = futures::executor::block_on(gql::subscribe(
        &schema,
        gql::Request::new("subscription { maybe }"),
        &gql::ExecutionOptions::with_context(Ctx::anonymous()),
    ))
    .expect("subscription starts");
    let events = futures::executor::block_on(response.stream.take(1).collect::<Vec<_>>());
    assert!(!events[0].has_errors(), "{:?}", events[0]);
    assert_eq!(data(&events[0]).get("maybe"), Some(&gql::Value::Null));
}

// =============================================================================================
// Third batch — soundness of overrides, reopen collisions, inherited metadata
// =============================================================================================

#[test]
fn as_type_must_not_silently_tighten_list_item_nullability() {
    let mut schema = Schema::<Ctx>::new();
    schema.query(|q| {
        // `Vec<Option<String>>` is `[String]!`; the override claims `[String!]!`.
        q.field("items", |_: NoArgs, _: &Ctx| vec![None::<String>])
            .as_type::<Vec<String>>();
    });
    match schema.build() {
        Ok(schema) => {
            let response = run(&schema, "{ items }", Ctx::anonymous());
            panic!("the override built; runtime response: {response:?}");
        }
        Err(error) => {
            let message = error.to_string();
            assert!(
                message.contains("not compatible") || message.contains("optional"),
                "{message}"
            );
        }
    }
}

#[test]
fn repeated_object_auth_replaces_rather_than_composes() {
    let mut schema = Schema::<Ctx>::new();
    schema.object::<User>(|o| {
        o.auth(|ctx: &Ctx| ctx.logged_in);
        o.expose(expose!(User::name));
    });
    // A second rule that is always true: if rules composed, anonymous would still be denied.
    schema.object_fields::<User>(|o| {
        o.auth(|_ctx: &Ctx| true);
        o.expose(expose!(User::age));
    });
    schema.query(|q| {
        q.field("user", |_: NoArgs, _: &Ctx| User {
            name: "ada".into(),
            age: 36,
        });
    });
    let schema = schema.build().expect("schema builds");
    let response = run(&schema, "{ user { name age } }", Ctx::anonymous());
    assert!(
        response.has_errors(),
        "object auth rules declared in different calls should compose, but the later rule \
         replaced the earlier one: {response:?}"
    );
}

#[test]
fn duplicate_interface_fields_across_reopen_are_a_build_error() {
    let mut schema = Schema::<Ctx>::new();
    schema.interface("Node", |i| {
        i.field::<ID>("id");
    });
    schema.interface_fields("Node", |i| {
        i.field::<ID>("id");
    });
    schema.query(|q| {
        q.field("ok", |_: NoArgs, _: &Ctx| true);
    });
    let error = schema.build().expect_err("duplicate interface field");
    assert!(error.to_string().contains("more than once"), "{error}");
}

#[test]
fn resolve_inherited_accepts_an_abstract_covariant_return() {
    let mut schema = Schema::<Ctx>::new();
    let node = schema.interface("Node", |i| {
        i.field::<ID>("id");
    });
    let resource = schema.interface("Resource", |i| {
        i.implements(&node);
        i.field::<ID>("id");
        i.field::<Option<ID>>("owner");
    });
    schema.object::<User>(|o| {
        o.implements(&resource);
        o.implements(&node);
        o.field("id", |u: &User, _: NoArgs, _: &Ctx| ID::new(u.name.clone()));
        // Interface says `owner: ID`, the resolver returns a concrete object is nonsense, so
        // this checks the simpler case: the interface declares `Option<ID>` and the object's
        // resolver returns `ID!`.
        o.resolve_inherited("owner", |u: &User, _: NoArgs, _: &Ctx| {
            ID::new(u.name.clone())
        });
    });
    schema.query(|q| {
        q.field("user", |_: NoArgs, _: &Ctx| User {
            name: "ada".into(),
            age: 36,
        });
    });
    let schema = schema
        .build()
        .expect("covariant nullable-to-non-null builds");
    let field = schema
        .object_type("User")
        .unwrap()
        .get_field("owner")
        .unwrap();
    assert_eq!(field.ty.to_string(), "ID");
}

#[test]
fn resolve_inherited_copies_the_interface_deprecation() {
    let mut schema = Schema::<Ctx>::new();
    let node = schema.interface("Node", |i| {
        i.field::<String>("oldName").deprecated("Use `name`.");
    });
    schema.object::<User>(|o| {
        o.implements(&node);
        o.resolve_inherited("oldName", |u: &User, _: NoArgs, _: &Ctx| u.name.clone());
    });
    schema.query(|q| {
        q.field("user", |_: NoArgs, _: &Ctx| User {
            name: "ada".into(),
            age: 36,
        });
    });
    let schema = schema.build().expect("schema builds");
    let field = schema
        .object_type("User")
        .unwrap()
        .get_field("oldName")
        .expect("inherited field");
    assert_eq!(
        field.deprecation_reason.as_deref(),
        Some("Use `name`."),
        "inherited fields should keep the interface field's deprecation"
    );
}

#[test]
fn subscription_can_select_its_root_field_through_a_fragment() {
    let mut schema = Schema::<Ctx>::new();
    schema.query(|q| {
        q.field("ok", |_: NoArgs, _: &Ctx| true);
    });
    schema.subscription(|s| {
        s.field("ticks", |_: NoArgs, _: &Ctx| {
            stream::iter(vec![GurafiResult::Ok(1i32)])
        });
    });
    let schema = schema.build().expect("schema builds");
    let response = futures::executor::block_on(gql::subscribe(
        &schema,
        gql::Request::new("subscription { ...Root } fragment Root on Subscription { ticks }"),
        &gql::ExecutionOptions::with_context(Ctx::anonymous()),
    ))
    .expect("subscription starts");
    let events = futures::executor::block_on(response.stream.take(1).collect::<Vec<_>>());
    assert!(!events[0].has_errors(), "{:?}", events[0]);
    assert_eq!(data(&events[0]).get("ticks"), Some(&gql::Value::Int(1)));
}

#[test]
fn unnamed_input_nested_inside_a_named_input_reports_the_inner_type() {
    #[derive(Input)]
    struct Inner {
        text: String,
    }

    #[derive(Input)]
    #[graphql(name = "OuterInput")]
    struct Outer {
        inner: Inner,
    }

    #[derive(Input)]
    struct Args {
        outer: Outer,
    }

    let mut schema = Schema::<Ctx>::new();
    schema.query(|q| {
        q.field("bad", |_: Args, _: &Ctx| true);
    });
    let error = schema.build().expect_err("nested unnamed input");
    let message = error.to_string();
    assert!(
        message.contains("Inner") && message.contains("used as a nested input type"),
        "{message}"
    );
}

// =============================================================================================
// Fourth batch — gql integration changes reachable through gurafi
// =============================================================================================

#[test]
fn an_undefined_variable_in_a_nested_input_literal_drops_only_that_field() {
    #[derive(Input)]
    struct Args {
        by: Option<SearchBy>,
    }

    let mut schema = Schema::<Ctx>::new();
    schema.query(|q| {
        q.field("search", |args: Args, _: &Ctx| {
            args.by.map_or_else(
                || "none".to_owned(),
                |by| format!("{:?}/{:?}", by.id, by.name),
            )
        });
    });
    let schema = schema.build().expect("schema builds");

    // `$id` is not provided. Per CoerceArgumentValues the nullable input field `id` is omitted
    // and the sibling `name` is kept, so `by` is `{ name: "ada" }`.
    let response = futures::executor::block_on(gql::execute(
        &schema,
        gql::Request::new("query Q($id: ID) { search(by: { id: $id, name: \"ada\" }) }"),
        &gql::ExecutionOptions::with_context(Ctx::anonymous()),
    ));
    assert!(!response.has_errors(), "{response:?}");
    assert_eq!(
        value(&response, "search").as_str(),
        Some("None/Some(\"ada\")"),
        "{response:?}"
    );
}

#[test]
fn max_breadth_rejects_a_wide_query() {
    let mut schema = Schema::<Ctx>::new();
    schema.query(|q| {
        q.field("a", |_: NoArgs, _: &Ctx| 1i32);
        q.field("b", |_: NoArgs, _: &Ctx| 2i32);
        q.field("c", |_: NoArgs, _: &Ctx| 3i32);
    });
    let schema = schema.build().expect("schema builds");
    let options = gql::ExecutionOptions::with_context(Ctx::anonymous()).max_breadth(2);
    let response = futures::executor::block_on(gql::execute(
        &schema,
        gql::Request::new("{ a b c }"),
        &options,
    ));
    assert!(response.has_errors(), "{response:?}");
}

struct DenyAll;

impl gql::Authorizer for DenyAll {
    fn authorize(&self, _: &gql::AuthorizationContext<'_>) -> Result<(), GraphQLError> {
        Err(GraphQLError::new("denied by authorizer"))
    }
}

struct DenyAllAsync;

impl gql::Authorizer for DenyAllAsync {
    fn authorize_async<'a>(
        &'a self,
        _: &'a gql::AuthorizationContext<'a>,
    ) -> futures::future::BoxFuture<'a, Result<(), GraphQLError>> {
        Box::pin(async { Err(GraphQLError::new("denied asynchronously")) })
    }
}

#[test]
fn async_authorizers_work_alongside_gurafi_auth() {
    let mut schema = Schema::<Ctx>::new();
    schema.query(|q| {
        q.field("secret", |_: NoArgs, _: &Ctx| "x")
            .auth(|ctx: &Ctx| ctx.logged_in);
    });
    let schema = schema.build().expect("schema builds");

    let options = gql::ExecutionOptions::with_context(Ctx::logged_in()).authorizer(DenyAllAsync);
    let response = futures::executor::block_on(gql::execute(
        &schema,
        gql::Request::new("{ secret }"),
        &options,
    ));
    assert!(
        response
            .errors
            .iter()
            .any(|error| error.message().contains("denied asynchronously")),
        "{response:?}"
    );

    let options = gql::ExecutionOptions::with_context(Ctx::logged_in()).authorizer(DenyAll);
    let response = futures::executor::block_on(gql::execute(
        &schema,
        gql::Request::new("{ secret }"),
        &options,
    ));
    assert!(
        response
            .errors
            .iter()
            .any(|error| error.message().contains("denied by authorizer")),
        "{response:?}"
    );
}

#[derive(Input)]
struct IntArgs {
    value: i32,
}

#[test]
fn oversized_integer_literals_are_an_error_not_a_panic() {
    let mut schema = Schema::<Ctx>::new();
    schema.query(|q| {
        q.field("int", |args: IntArgs, _: &Ctx| args.value);
    });
    let schema = schema.build().expect("schema builds");
    let response = run(
        &schema,
        "{ int(value: 99999999999999999999999999) }",
        Ctx::anonymous(),
    );
    assert!(response.has_errors(), "{response:?}");
}

#[test]
fn unused_built_in_scalars_are_pruned_from_introspection_and_sdl() {
    #[derive(Enum, Clone, Copy)]
    enum Only {
        A,
    }

    let mut schema = Schema::<Ctx>::new();
    schema.query(|q| {
        q.field("only", |_: NoArgs, _: &Ctx| Only::A);
    });
    let schema = schema.build().expect("schema builds");
    let sdl = schema.sdl();
    assert!(
        !sdl.contains("Float"),
        "unused built-ins should not be printed: {sdl}"
    );

    let response = run(&schema, "{ __schema { types { name } } }", Ctx::anonymous());
    let types = value(&response, "__schema");
    let names = types
        .as_object()
        .and_then(|schema| schema.get("types"))
        .and_then(gql::Value::as_list)
        .expect("types")
        .iter()
        .filter_map(|ty| ty.as_object().and_then(|object| object.get("name")))
        .filter_map(gql::Value::as_str)
        .map(str::to_owned)
        .collect::<Vec<_>>();
    assert!(!names.iter().any(|name| name == "Float"), "{names:?}");
    // `String` stays because the introspection types themselves use it.
    assert!(names.iter().any(|name| name == "String"), "{names:?}");
    assert!(names.iter().any(|name| name == "Boolean"));
}

// =============================================================================================
// Fifth batch — info paths, auth coverage, subscription validation counts
// =============================================================================================

#[test]
fn info_path_includes_list_indices() {
    let mut schema = Schema::<Ctx>::new();
    schema.object::<User>(|o| {
        o.field_with_info("where", |_: &User, _: NoArgs, _: &Ctx, info| {
            info.path
                .iter()
                .map(|segment| format!("{segment}"))
                .collect::<Vec<_>>()
                .join("/")
        });
    });
    schema.query(|q| {
        q.field("users", |_: NoArgs, _: &Ctx| {
            vec![
                User {
                    name: "a".into(),
                    age: 1,
                },
                User {
                    name: "b".into(),
                    age: 2,
                },
            ]
        });
    });
    let schema = schema.build().expect("schema builds");
    let response = run(&schema, "{ users { where } }", Ctx::anonymous());
    assert!(!response.has_errors(), "{response:?}");
    let users = value(&response, "users");
    let first = users
        .as_list()
        .and_then(|users| users.first())
        .and_then(gql::Value::as_object)
        .and_then(|user| user.get("where"))
        .and_then(gql::Value::as_str)
        .expect("first user");
    assert_eq!(first, "users/0/where");
}

#[test]
fn object_auth_applies_when_the_object_is_reached_through_a_union() {
    #[derive(Object)]
    struct Secret {
        value: String,
    }

    let mut schema = Schema::<Ctx>::new();
    schema.object::<Secret>(|o| {
        o.auth(|ctx: &Ctx| ctx.logged_in);
        o.expose(expose!(Secret::value));
    });
    schema.object::<User>(|o| {
        o.expose(expose!(User::name));
    });
    let thing = schema.union("Thing", |u| {
        u.member::<Secret>().member::<User>();
    });
    schema.query(|q| {
        q.field("thing", |_: NoArgs, _: &Ctx| Secret {
            value: "shh".into(),
        })
        .as_abstract(&thing);
    });
    let schema = schema.build().expect("schema builds");

    let denied = run(
        &schema,
        "{ thing { ... on Secret { value } } }",
        Ctx::anonymous(),
    );
    assert!(denied.has_errors(), "{denied:?}");
    let allowed = run(
        &schema,
        "{ thing { ... on Secret { value } } }",
        Ctx::logged_in(),
    );
    assert!(!allowed.has_errors(), "{allowed:?}");
}

#[test]
fn subscription_validation_counts_for_several_events() {
    static COUNT: AtomicUsize = AtomicUsize::new(0);
    COUNT.store(0, Ordering::SeqCst);

    let mut schema = Schema::<Ctx>::new();
    schema.query(|q| {
        q.field("ok", |_: NoArgs, _: &Ctx| true);
    });
    schema.subscription(|s| {
        s.field("ticks", |_: NoArgs, _: &Ctx| {
            stream::iter((0..3).map(GurafiResult::Ok))
        })
        .validate::<NoArgs>(|_, _| {
            COUNT.fetch_add(1, Ordering::SeqCst);
            Ok(())
        });
    });
    let schema = schema.build().expect("schema builds");
    let response = futures::executor::block_on(gql::subscribe(
        &schema,
        gql::Request::new("subscription { ticks }"),
        &gql::ExecutionOptions::with_context(Ctx::anonymous()),
    ))
    .expect("subscription starts");
    let events = futures::executor::block_on(response.stream.collect::<Vec<_>>());
    assert_eq!(events.len(), 3);
    assert_eq!(
        COUNT.load(Ordering::SeqCst),
        3,
        "3 events should validate 3 times, not once per subscribe plus once per event"
    );
}

#[test]
fn subscription_event_object_fields_can_receive_info() {
    let mut schema = Schema::<Ctx>::new();
    schema.object::<User>(|o| {
        o.field_with_info("label", |_u: &User, _: NoArgs, _: &Ctx, info| {
            format!(
                "{}:{}:{}",
                info.parent_type,
                info.field_name,
                info.path.len()
            )
        });
    });
    schema.query(|q| {
        q.field("ok", |_: NoArgs, _: &Ctx| true);
    });
    schema.subscription(|s| {
        s.field("user", |_: NoArgs, _: &Ctx| {
            stream::iter(vec![GurafiResult::Ok(User {
                name: "ada".into(),
                age: 36,
            })])
        });
    });
    let schema = schema.build().expect("schema builds");
    let response = futures::executor::block_on(gql::subscribe(
        &schema,
        gql::Request::new("subscription { user { label } }"),
        &gql::ExecutionOptions::with_context(Ctx::anonymous()),
    ))
    .expect("subscription starts");
    let events = futures::executor::block_on(response.stream.take(1).collect::<Vec<_>>());
    assert!(!events[0].has_errors(), "{:?}", events[0]);
    assert_eq!(
        data(&events[0])
            .get("user")
            .and_then(gql::Value::as_object)
            .and_then(|user| user.get("label"))
            .and_then(gql::Value::as_str),
        Some("User:label:2")
    );
}

#[test]
fn field_map_with_an_alias_and_no_double_wrapping() {
    #[derive(Clone)]
    struct Event {
        value: i32,
    }

    let mut schema = Schema::<Ctx>::new();
    schema.query(|q| {
        q.field("ok", |_: NoArgs, _: &Ctx| true);
    });
    schema.subscription(|s| {
        s.field_map::<NoArgs, Event, i32, _, _, _>(
            "counted",
            |_: NoArgs, _: &Ctx| stream::iter(vec![GurafiResult::Ok(Event { value: 7 })]),
            |event: &Event, _: NoArgs, _: &Ctx| Ok(event.value),
        );
    });
    let schema = schema.build().expect("schema builds");
    let response = futures::executor::block_on(gql::subscribe(
        &schema,
        gql::Request::new("subscription { n: counted }"),
        &gql::ExecutionOptions::with_context(Ctx::anonymous()),
    ))
    .expect("subscription starts");
    let events = futures::executor::block_on(response.stream.take(1).collect::<Vec<_>>());
    assert!(!events[0].has_errors(), "{:?}", events[0]);
    assert_eq!(data(&events[0]).get("n"), Some(&gql::Value::Int(7)));
    assert_eq!(
        data(&events[0]).len(),
        1,
        "no extra wrapping keys: {:?}",
        events[0]
    );
}

// =============================================================================================
// Sixth batch — validation coerce duplication, auth ordering, abstract lists
// =============================================================================================

static NESTED_VALIDATIONS: AtomicUsize = AtomicUsize::new(0);

fn count_nested(_: &ValidatedCreate) -> Result<(), GraphQLError> {
    NESTED_VALIDATIONS.fetch_add(1, Ordering::SeqCst);
    Ok(())
}

#[derive(Input)]
#[graphql(validate = "count_nested")]
struct ValidatedCreate {
    #[graphql(default = 1)]
    value: i32,
}

#[test]
fn field_validate_does_not_re_run_input_validation() {
    NESTED_VALIDATIONS.store(0, Ordering::SeqCst);
    let mut schema = Schema::<Ctx>::new();
    schema.query(|q| {
        q.field("create", |args: ValidatedCreate, _: &Ctx| args.value)
            .validate::<ValidatedCreate>(|_, _| Ok(()));
    });
    let schema = schema.build().expect("schema builds");
    let response = run(&schema, "{ create }", Ctx::anonymous());
    assert!(!response.has_errors(), "{response:?}");
    assert_eq!(
        NESTED_VALIDATIONS.load(Ordering::SeqCst),
        1,
        "input-level validation must not run again just because a field validator is installed"
    );
}

#[test]
fn auth_runs_before_validation_regardless_of_chain_order() {
    static RAN: AtomicUsize = AtomicUsize::new(0);
    RAN.store(0, Ordering::SeqCst);

    let mut schema = Schema::<Ctx>::new();
    schema.query(|q| {
        q.field("a", |_: NoArgs, _: &Ctx| 1i32)
            .auth(|ctx: &Ctx| ctx.logged_in)
            .validate::<NoArgs>(|_, _| {
                RAN.fetch_add(1, Ordering::SeqCst);
                Ok(())
            });
        q.field("b", |_: NoArgs, _: &Ctx| 2i32)
            .validate::<NoArgs>(|_, _| {
                RAN.fetch_add(1, Ordering::SeqCst);
                Ok(())
            })
            .auth(|ctx: &Ctx| ctx.logged_in);
    });
    let schema = schema.build().expect("schema builds");
    let response = run(&schema, "{ a b }", Ctx::anonymous());
    assert!(response.has_errors(), "{response:?}");
    assert_eq!(
        RAN.load(Ordering::SeqCst),
        0,
        "a denied field must not run its validation hook"
    );
}

#[test]
fn resolve_inherited_finds_interface_fields_added_after_the_object() {
    let mut schema = Schema::<Ctx>::new();
    let node = schema.interface("Node", |i| {
        i.field::<ID>("id");
    });
    schema.object::<User>(|o| {
        o.implements(&node);
        o.field("id", |u: &User, _: NoArgs, _: &Ctx| ID::new(u.name.clone()));
        o.resolve_inherited("label", |u: &User, _: NoArgs, _: &Ctx| u.name.clone());
    });
    // The field arrives after the object used it.
    schema.interface_fields("Node", |i| {
        i.field::<String>("label");
    });
    schema.query(|q| {
        q.field("user", |_: NoArgs, _: &Ctx| User {
            name: "ada".into(),
            age: 36,
        });
    });
    let schema = schema.build().expect("schema builds");
    assert!(
        schema
            .object_type("User")
            .unwrap()
            .get_field("label")
            .is_some()
    );
}

#[test]
fn as_abstract_preserves_list_wrappers_and_resolves_each_item() {
    let mut schema = Schema::<Ctx>::new();
    let node = schema.interface("Node", |i| {
        i.field::<ID>("id");
    });
    schema.object::<User>(|o| {
        o.implements(&node);
        o.field("id", |u: &User, _: NoArgs, _: &Ctx| ID::new(u.name.clone()));
        o.expose(expose!(User::name));
    });
    schema.query(|q| {
        q.field("nodes", |_: NoArgs, _: &Ctx| {
            vec![
                User {
                    name: "a".into(),
                    age: 1,
                },
                User {
                    name: "b".into(),
                    age: 2,
                },
            ]
        })
        .as_abstract(&node);
        q.field("maybeNodes", |_: NoArgs, _: &Ctx| {
            Some(vec![User {
                name: "c".into(),
                age: 3,
            }])
        })
        .as_abstract(&node);
    });
    let schema = schema.build().expect("schema builds");
    let query_type = schema.object_type("Query").expect("Query");
    assert_eq!(
        query_type.get_field("nodes").unwrap().ty.to_string(),
        "[Node]!"
    );
    assert_eq!(
        query_type.get_field("maybeNodes").unwrap().ty.to_string(),
        "[Node]"
    );

    let response = run(
        &schema,
        "{ nodes { __typename ... on User { name } } }",
        Ctx::anonymous(),
    );
    assert!(!response.has_errors(), "{response:?}");
    let nodes = value(&response, "nodes");
    assert_eq!(nodes.as_list().map(<[gql::Value]>::len), Some(2));
}

// =============================================================================================
// Seventh batch — remaining resolver-info variants
// =============================================================================================

#[test]
fn async_and_fallible_info_variants_on_objects() {
    let mut schema = Schema::<Ctx>::new();
    schema.object::<User>(|o| {
        o.field_async_with_info("a", |u: &User, _: NoArgs, _: &Ctx, info| {
            let name = u.name.clone();
            Box::pin(async move { format!("{name}:{}", info.field_name) })
        });
        o.field_result_with_info("b", |_: &User, _: NoArgs, _: &Ctx, info| {
            GurafiResult::Ok(info.parent_type.to_owned())
        });
        o.field_result_async_with_info("c", |_: &User, _: NoArgs, _: &Ctx, info| {
            Box::pin(async move { GurafiResult::Ok(info.return_type.to_string()) })
        });
    });
    schema.query(|q| {
        q.field("user", |_: NoArgs, _: &Ctx| User {
            name: "ada".into(),
            age: 36,
        });
    });
    let schema = schema.build().expect("schema builds");
    let response = run(&schema, "{ user { a b c } }", Ctx::anonymous());
    assert!(!response.has_errors(), "{response:?}");
    let user = value(&response, "user");
    let user = user.as_object().expect("user");
    assert_eq!(user.get("a").and_then(gql::Value::as_str), Some("ada:a"));
    assert_eq!(user.get("b").and_then(gql::Value::as_str), Some("User"));
    assert_eq!(user.get("c").and_then(gql::Value::as_str), Some("String!"));
}

#[test]
fn object_auth_error_declared_on_a_later_reopen_is_applied() {
    let mut schema = Schema::<Ctx>::new();
    schema.object::<User>(|o| {
        o.auth(|ctx: &Ctx| ctx.logged_in);
        o.expose(expose!(User::name));
    });
    // Only the message is refined later; the predicate is already installed.
    schema.object_fields::<User>(|o| {
        o.auth_error("Sign in to read users");
        o.expose(expose!(User::age));
    });
    schema.query(|q| {
        q.field("user", |_: NoArgs, _: &Ctx| User {
            name: "ada".into(),
            age: 36,
        });
    });
    let schema = schema.build().expect("schema builds");
    let denied = run(&schema, "{ user { name } }", Ctx::anonymous());
    assert!(
        denied
            .errors
            .iter()
            .any(|error| error.message().contains("Sign in to read users")),
        "auth_error from a later declaration should update the existing rule: {denied:?}"
    );
}
