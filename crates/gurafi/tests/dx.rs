//! Tests for the developer-experience features: args-only inputs, reopening types, type
//! overrides, resolver info, object/interface authorization, validation hooks, one-of inputs,
//! inherited interface fields, subscription mapping and build options.

#![allow(dead_code)]

use std::sync::Arc;

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

#[derive(Object)]
struct User {
    name: String,
    age: i32,
}

#[derive(Object)]
struct Post {
    title: String,
}

#[derive(Object)]
#[graphql(name = "Keeper")]
struct Keeper {
    name: String,
}

#[derive(Args)]
struct FriendsArgs {
    #[graphql(default = 10)]
    limit: i32,
    after: Option<String>,
}

#[derive(Input)]
#[graphql(name = "FilterInput")]
struct Filter {
    prefix: Option<String>,
    nested: Option<NestedFilter>,
}

#[derive(Input)]
#[graphql(name = "NestedFilterInput")]
struct NestedFilter {
    text: String,
}

#[derive(Input)]
struct IdArgs {
    id: ID,
}

#[derive(Input)]
struct OtherArgs {
    name: String,
}

#[derive(Enum, Clone, Copy)]
enum Color {
    #[graphql(name = "RED")]
    Red,
    #[graphql(name = "BLUE")]
    Blue,
}

#[derive(Enum, Clone, Copy)]
enum Shade {
    Light,
    Dark,
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

// ---------------------------------------------------------------------------------------------
// A2 — args are not named input types unless asked
// ---------------------------------------------------------------------------------------------

fn args_schema() -> gql::Schema {
    let mut schema = Schema::<Ctx>::new();
    schema.object::<User>(|o| {
        o.expose(expose!(User::name));
        o.field("filtered", |u: &User, _: Filter, _: &Ctx| u.name.clone());
    });
    schema.query(|q| {
        q.field_result("friends", |args: FriendsArgs, _: &Ctx| {
            GurafiResult::Ok(format!("limit={}", args.limit))
        });
        q.field("user", |_: NoArgs, _: &Ctx| User {
            name: "ada".into(),
            age: 36,
        });
    });
    schema.build().expect("schema builds")
}

#[test]
fn args_only_inputs_do_not_leak_into_the_schema() {
    let schema = args_schema();
    assert!(
        schema.input_object_type("FriendsArgs").is_none(),
        "an args type must not become a named input object"
    );
    assert!(schema.input_object_type("FilterInput").is_some());

    let sdl = schema.sdl();
    assert!(!sdl.contains("input FriendsArgs"), "{sdl}");
    assert!(!sdl.contains("__GurafiUnnamedInput"), "{sdl}");
    assert!(
        sdl.contains("friends(limit: Int! = 10, after: String): String!"),
        "{sdl}"
    );
    assert!(sdl.contains("input FilterInput"), "{sdl}");
    assert!(sdl.contains("input NestedFilterInput"), "{sdl}");
}

#[test]
fn an_unnamed_input_used_nested_is_an_actionable_build_error() {
    #[derive(Input)]
    struct BadNested {
        text: String,
    }

    #[derive(Input)]
    struct BadArgs {
        nested: Option<BadNested>,
    }

    let mut schema = Schema::<Ctx>::new();
    schema.query(|q| {
        q.field("bad", |_: BadArgs, _: &Ctx| true);
    });
    let error = schema
        .build()
        .expect_err("an unnamed nested input must fail");
    let message = error.to_string();
    assert!(
        message.contains("used as a nested input type") && message.contains("graphql(name"),
        "{message}"
    );
    assert!(!message.contains("__GurafiUnnamedInput"), "{message}");
}

// ---------------------------------------------------------------------------------------------
// A1 — reopening types and split-across-modules roots
// ---------------------------------------------------------------------------------------------

#[test]
fn types_and_roots_can_be_reopened() {
    let mut schema = Schema::<Ctx>::new();

    let node = schema.interface("Node", |i| {
        i.field::<ID>("id");
    });
    schema.interface_fields("Node", |i| {
        i.field::<String>("label").description("A label.");
    });

    schema.object::<User>(|o| {
        o.implements(&node);
        o.expose(expose!(User::name));
        o.field("id", |u: &User, _: NoArgs, _: &Ctx| ID::new(u.name.clone()));
        o.field("label", |u: &User, _: NoArgs, _: &Ctx| u.name.clone());
    });
    schema.object_fields::<User>(|o| {
        o.field("age", |u: &User, _: NoArgs, _: &Ctx| u.age);
    });
    schema.object_field::<User, i32, NoArgs, _>("height", |_: &User, _: NoArgs, _: &Ctx| 340);

    schema.query_fields(|q| {
        q.field("user", |_: NoArgs, _: &Ctx| User {
            name: "ada".into(),
            age: 36,
        });
    });
    schema.query_fields(|q| {
        q.field("ok", |_: NoArgs, _: &Ctx| true);
    });
    schema.mutation_fields(|m| {
        m.field("noop", |_: NoArgs, _: &Ctx| true);
    });
    schema.subscription_fields(|s| {
        s.field("ticks", |_: NoArgs, _: &Ctx| {
            stream::iter(vec![GurafiResult::Ok(1i32)])
        });
    });

    let schema = schema.build().expect("merged schema builds");
    let node = schema.interface_type("Node").expect("Node");
    assert!(node.fields.contains_key("id"));
    assert_eq!(
        node.fields["label"].description.as_deref(),
        Some("A label.")
    );
    let user = schema.object_type("User").expect("User");
    for field in ["name", "id", "age", "height"] {
        assert!(user.get_field(field).is_some(), "missing {field}");
    }
    assert_eq!(
        schema.object_type("User").unwrap().interfaces,
        vec![Arc::from("Node")]
    );
    assert!(schema.get_type("Mutation").is_some());
    assert!(schema.get_type("Subscription").is_some());
}

#[test]
fn two_rust_models_cannot_merge_into_one_object_name() {
    #[derive(Object)]
    #[graphql(name = "Thing")]
    struct First {
        name: String,
    }

    #[derive(Object)]
    #[graphql(name = "Thing")]
    struct Second {
        name: String,
    }

    let mut schema = Schema::<Ctx>::new();
    schema.object::<First>(|o| {
        o.expose(expose!(First::name));
    });
    schema.object::<Second>(|o| {
        o.expose(expose!(Second::name));
    });
    schema.query(|q| {
        q.field("ok", |_: NoArgs, _: &Ctx| true);
    });
    let error = schema.build().expect_err("a name collision must fail");
    assert!(
        error.to_string().contains("more than one Rust type"),
        "{error}"
    );
}

#[test]
fn reopening_keeps_duplicate_field_names_a_build_error() {
    let mut schema = Schema::<Ctx>::new();
    schema.object::<User>(|o| {
        o.expose(expose!(User::name));
    });
    schema.object_fields::<User>(|o| {
        o.expose(expose!(User::name));
    });
    schema.query(|q| {
        q.field("user", |_: NoArgs, _: &Ctx| User {
            name: "ada".into(),
            age: 1,
        });
    });
    let error = schema.build().expect_err("a duplicate field must fail");
    assert!(error.to_string().contains("more than once"), "{error}");
}

// ---------------------------------------------------------------------------------------------
// A3 — nullability and type overrides
// ---------------------------------------------------------------------------------------------

#[derive(gurafi::Scalar, Clone)]
#[graphql(name = "Date")]
struct Date(String);

#[test]
fn nullability_overrides_change_the_declared_type() {
    let mut schema = Schema::<Ctx>::new();
    schema.object::<User>(|o| {
        o.expose(expose!(User::name)).nullable();
        o.field("tags", |_: &User, _: NoArgs, _: &Ctx| vec!["a".to_owned()])
            .nullable_items();
    });
    schema.query(|q| {
        q.field("user", |_: NoArgs, _: &Ctx| User {
            name: "ada".into(),
            age: 36,
        });
    });
    let schema = schema.build().expect("schema builds");
    let user = schema.object_type("User").expect("User");
    assert_eq!(user.get_field("name").unwrap().ty.to_string(), "String");
    assert_eq!(user.get_field("tags").unwrap().ty.to_string(), "[String]!");
}

#[test]
fn non_null_on_an_optional_resolver_is_a_build_error() {
    let mut schema = Schema::<Ctx>::new();
    schema.query(|q| {
        q.field("maybe", |_: NoArgs, _: &Ctx| Some("value"))
            .non_null();
    });
    let error = schema
        .build()
        .expect_err("an optional resolver must not be non-null");
    assert!(
        error.to_string().contains("calls `non_null()`") && error.to_string().contains("optional"),
        "{error}"
    );
}

#[test]
fn nullable_items_on_a_non_list_is_a_build_error() {
    let mut schema = Schema::<Ctx>::new();
    schema.query(|q| {
        q.field("value", |_: NoArgs, _: &Ctx| "text".to_owned())
            .nullable_items();
    });
    let error = schema.build().expect_err("nullable_items needs a list");
    assert!(error.to_string().contains("nullable_items()"), "{error}");
}

#[test]
fn as_type_overrides_between_compatible_kinds() {
    let mut schema = Schema::<Ctx>::new();
    schema.query(|q| {
        // String and a custom scalar are both scalars.
        q.field("date", |_: NoArgs, _: &Ctx| "2024-01-01".to_owned())
            .as_type::<Date>();
        // Alias for the same override.
        q.field("otherDate", |_: NoArgs, _: &Ctx| "2024-01-02".to_owned())
            .returns::<Date>();
        // Enum to enum.
        q.field("color", |_: NoArgs, _: &Ctx| Color::Red)
            .as_type::<Shade>();
    });
    let schema = schema.build().expect("compatible overrides build");
    let query = schema.object_type("Query").expect("Query");
    // Custom scalars and enums are nullable by default in `gurafi`.
    assert_eq!(query.get_field("date").unwrap().ty.to_string(), "Date");
    assert_eq!(query.get_field("color").unwrap().ty.to_string(), "Shade");
}

#[test]
fn as_type_rejects_incompatible_kinds() {
    let mut schema = Schema::<Ctx>::new();
    schema.object::<User>(|o| {
        o.expose(expose!(User::name));
    });
    schema.query(|q| {
        q.field("broken", |_: NoArgs, _: &Ctx| "text".to_owned())
            .as_type::<User>();
    });
    let error = schema
        .build()
        .expect_err("String to object is not compatible");
    assert!(error.to_string().contains("not compatible"), "{error}");
}

#[test]
fn as_type_from_a_model_to_an_abstract_type_checks_possible_types() {
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
        q.field("node", |_: NoArgs, _: &Ctx| User {
            name: "ada".into(),
            age: 36,
        })
        .as_type::<Option<User>>();
    });
    // `Option<User>` is a concrete object, not the interface: this must still be accepted as the
    // same object type (with a nullable wrapper).
    let schema = schema.build().expect("object override builds");
    assert_eq!(
        schema
            .object_type("Query")
            .unwrap()
            .get_field("node")
            .unwrap()
            .ty
            .to_string(),
        "User"
    );
}

// ---------------------------------------------------------------------------------------------
// A4 — resolver info
// ---------------------------------------------------------------------------------------------

#[test]
fn resolvers_can_receive_resolve_info() {
    let mut schema = Schema::<Ctx>::new();
    schema.object::<User>(|o| {
        o.field_with_info("where", |_: &User, _: NoArgs, _: &Ctx, info| {
            format!("{}:{}", info.parent_type, info.field_name)
        });
    });
    schema.query(|q| {
        q.field_with_info("info", |_: NoArgs, _: &Ctx, info: &gql::ResolveInfo<'_>| {
            format!(
                "{}:{}:{}:{}",
                info.parent_type,
                info.field_name,
                info.return_type,
                info.path.len()
            )
        });
        q.field_result_with_info(
            "fallible",
            |_: NoArgs, _: &Ctx, info: &gql::ResolveInfo<'_>| {
                GurafiResult::Ok(info.field_name.to_owned())
            },
        );
        q.field_async_with_info(
            "asyncInfo",
            |_: NoArgs, _: &Ctx, info: &gql::ResolveInfo<'_>| {
                Box::pin(async move { info.field_name.to_owned() })
            },
        );
        q.field_result_async_with_info(
            "asyncFallibleInfo",
            |_: NoArgs, _: &Ctx, info: &gql::ResolveInfo<'_>| {
                Box::pin(async move { GurafiResult::Ok(info.return_type.to_string()) })
            },
        );
        q.field("user", |_: NoArgs, _: &Ctx| User {
            name: "ada".into(),
            age: 36,
        });
    });
    let schema = schema.build().expect("schema builds");
    let response = run(
        &schema,
        "{ info fallible asyncInfo asyncFallibleInfo user { where } }",
        Ctx::anonymous(),
    );
    assert!(!response.has_errors(), "{response:?}");
    assert_eq!(
        value(&response, "info").as_str(),
        Some("Query:info:String!:1")
    );
    assert_eq!(value(&response, "fallible").as_str(), Some("fallible"));
    assert_eq!(value(&response, "asyncInfo").as_str(), Some("asyncInfo"));
    assert_eq!(
        value(&response, "asyncFallibleInfo").as_str(),
        Some("String!")
    );
    assert_eq!(
        value(&response, "user")
            .as_object()
            .and_then(|user| user.get("where"))
            .and_then(gql::Value::as_str),
        Some("User:where")
    );
}

// ---------------------------------------------------------------------------------------------
// A6 — object and interface authorization
// ---------------------------------------------------------------------------------------------

#[test]
fn object_level_auth_applies_to_every_field() {
    let mut schema = Schema::<Ctx>::new();
    schema.object::<User>(|o| {
        // The message may be set before or after the predicate.
        o.auth_error("Sign in to see users");
        o.auth(|ctx: &Ctx| ctx.logged_in);
        o.expose(expose!(User::name));
        o.field("age", |u: &User, _: NoArgs, _: &Ctx| u.age);
    });
    schema.query(|q| {
        q.field("user", |_: NoArgs, _: &Ctx| {
            Some(User {
                name: "ada".into(),
                age: 36,
            })
        });
    });
    let schema = schema.build().expect("schema builds");

    let denied = run(&schema, "{ user { name age } }", Ctx::anonymous());
    assert!(denied.has_errors(), "{denied:?}");
    assert!(
        denied
            .errors
            .iter()
            .any(|error| error.message().contains("Sign in to see users")),
        "{denied:?}"
    );
    assert_eq!(value(&denied, "user"), gql::Value::Null);

    let allowed = run(&schema, "{ user { name age } }", Ctx::logged_in());
    assert!(!allowed.has_errors(), "{allowed:?}");
    assert_eq!(
        value(&allowed, "user")
            .as_object()
            .and_then(|user| user.get("age"))
            .and_then(gql::Value::as_i32),
        Some(36)
    );
}

#[test]
fn interface_level_auth_is_inherited_by_objects() {
    let mut schema = Schema::<Ctx>::new();
    let node = schema.interface("Node", |i| {
        i.auth(|ctx: &Ctx| ctx.logged_in)
            .auth_error("Interface says no");
        i.field::<ID>("id");
    });
    schema.object::<User>(|o| {
        o.implements(&node);
        o.field("id", |u: &User, _: NoArgs, _: &Ctx| ID::new(u.name.clone()));
        o.expose(expose!(User::name));
    });
    schema.query(|q| {
        q.field("node", |_: NoArgs, _: &Ctx| {
            Some(User {
                name: "ada".into(),
                age: 36,
            })
        })
        .as_abstract(&node);
    });
    let schema = schema.build().expect("schema builds");

    let denied = run(
        &schema,
        "{ node { id ... on User { name } } }",
        Ctx::anonymous(),
    );
    assert!(denied.has_errors(), "{denied:?}");
    assert!(
        denied
            .errors
            .iter()
            .any(|error| error.message().contains("Interface says no")),
        "{denied:?}"
    );

    let allowed = run(
        &schema,
        "{ node { id ... on User { name } } }",
        Ctx::logged_in(),
    );
    assert!(!allowed.has_errors(), "{allowed:?}");
}

#[test]
fn interface_field_auth_is_inherited_by_objects() {
    let mut schema = Schema::<Ctx>::new();
    let node = schema.interface("Node", |i| {
        i.field::<ID>("id").auth(|ctx: &Ctx| ctx.logged_in);
    });
    schema.object::<User>(|o| {
        o.implements(&node);
        o.field("id", |u: &User, _: NoArgs, _: &Ctx| ID::new(u.name.clone()));
        o.expose(expose!(User::name));
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
fn field_and_object_async_auth_work() {
    let mut schema = Schema::<Ctx>::new();
    schema.object::<User>(|o| {
        o.auth_async(|ctx: &Ctx| Box::pin(async move { ctx.logged_in }));
        o.expose(expose!(User::name));
    });
    schema.query(|q| {
        q.field("asyncSecret", |_: NoArgs, _: &Ctx| "secret")
            .auth_async(|ctx: &Ctx| Box::pin(async move { ctx.logged_in }));
    });
    let schema = schema.build().expect("schema builds");

    let denied = run(&schema, "{ asyncSecret }", Ctx::anonymous());
    assert!(denied.has_errors(), "{denied:?}");

    let allowed = run(&schema, "{ asyncSecret }", Ctx::logged_in());
    assert!(!allowed.has_errors(), "{allowed:?}");
    assert_eq!(value(&allowed, "asyncSecret").as_str(), Some("secret"));
}

// ---------------------------------------------------------------------------------------------
// A7 — validation hooks
// ---------------------------------------------------------------------------------------------

fn valid_name(name: &str) -> Result<(), GraphQLError> {
    if name.trim().is_empty() {
        Err(GraphQLError::new("name must not be empty"))
    } else {
        Ok(())
    }
}

fn valid_age(age: &i32) -> Result<(), GraphQLError> {
    if *age > 150 {
        Err(GraphQLError::new("age must be at most 150"))
    } else {
        Ok(())
    }
}

fn valid_create(args: &CreateArgs) -> Result<(), GraphQLError> {
    if args.name == "admin" {
        Err(GraphQLError::new("that name is reserved"))
    } else {
        Ok(())
    }
}

#[derive(Input)]
#[graphql(validate = "valid_create")]
struct CreateArgs {
    #[graphql(validate = "valid_name")]
    name: String,
    #[graphql(validate = "valid_age", default = 20)]
    age: i32,
}

#[test]
fn input_validation_runs_after_coercion() {
    let mut schema = Schema::<Ctx>::new();
    schema.query(|q| {
        q.field("create", |args: CreateArgs, _: &Ctx| args.name);
    });
    let schema = schema.build().expect("schema builds");

    let ok = run(&schema, r#"{ create(name: "ada") }"#, Ctx::anonymous());
    assert!(!ok.has_errors(), "{ok:?}");
    assert_eq!(value(&ok, "create").as_str(), Some("ada"));

    let empty = run(&schema, r#"{ create(name: "  ") }"#, Ctx::anonymous());
    assert!(empty.has_errors(), "{empty:?}");
    assert!(
        empty.errors.iter().any(|error| error
            .message()
            .contains("Invalid value for input field `CreateArgs.name`")),
        "{empty:?}"
    );

    let too_old = run(
        &schema,
        r#"{ create(name: "ada", age: 200) }"#,
        Ctx::anonymous(),
    );
    assert!(too_old.has_errors(), "{too_old:?}");
    assert!(
        too_old
            .errors
            .iter()
            .any(|error| error.message().contains("CreateArgs.age")),
        "{too_old:?}"
    );

    let reserved = run(&schema, r#"{ create(name: "admin") }"#, Ctx::anonymous());
    assert!(reserved.has_errors(), "{reserved:?}");
    assert!(
        reserved.errors.iter().any(|error| error
            .message()
            .contains("Invalid value for input type `CreateArgs`")),
        "{reserved:?}"
    );
}

fn valid_inner_text(text: &String) -> Result<(), GraphQLError> {
    if text == "bad" {
        Err(GraphQLError::new("inner text is rejected"))
    } else {
        Ok(())
    }
}

#[derive(Input)]
#[graphql(name = "ValidatedInner")]
struct ValidatedInner {
    #[graphql(validate = "valid_inner_text")]
    text: String,
}

#[derive(Input)]
struct ValidatedOuter {
    inner: Option<ValidatedInner>,
}

#[test]
fn nested_input_validation_runs_too() {
    let mut schema = Schema::<Ctx>::new();
    schema.query(|q| {
        q.field("check", |args: ValidatedOuter, _: &Ctx| {
            args.inner
                .map_or_else(|| "none".to_owned(), |inner| inner.text)
        });
    });
    let schema = schema.build().expect("schema builds");

    let ok = run(
        &schema,
        r#"{ check(inner: { text: "fine" }) }"#,
        Ctx::anonymous(),
    );
    assert!(!ok.has_errors(), "{ok:?}");

    let bad = run(
        &schema,
        r#"{ check(inner: { text: "bad" }) }"#,
        Ctx::anonymous(),
    );
    assert!(bad.has_errors(), "{bad:?}");
    assert!(
        bad.errors
            .iter()
            .any(|error| error.message().contains("inner text is rejected")),
        "{bad:?}"
    );
}

#[test]
fn builder_validation_can_use_the_context() {
    let mut schema = Schema::<Ctx>::new();
    schema.query(|q| {
        q.field("greet", |args: FriendsArgs, _: &Ctx| {
            format!("hi {}", args.limit)
        })
        .validate::<FriendsArgs>(|args, ctx| {
            if args.limit < 0 {
                Err(GraphQLError::new("limit must be positive"))
            } else if !ctx.logged_in {
                Err(GraphQLError::new("sign in first"))
            } else {
                Ok(())
            }
        });
    });
    let schema = schema.build().expect("schema builds");

    let denied = run(&schema, "{ greet }", Ctx::anonymous());
    assert!(denied.has_errors(), "{denied:?}");
    assert!(
        denied
            .errors
            .iter()
            .any(|error| error.message().contains("sign in first")),
        "{denied:?}"
    );

    let allowed = run(&schema, "{ greet(limit: 1) }", Ctx::logged_in());
    assert!(!allowed.has_errors(), "{allowed:?}");
}

// ---------------------------------------------------------------------------------------------
// A10 — oneOf inputs
// ---------------------------------------------------------------------------------------------

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

#[test]
fn one_of_inputs_are_registered_and_coerced() {
    let mut schema = Schema::<Ctx>::new();
    schema.query(|q| {
        q.field("search", |args: SearchArgs, _: &Ctx| {
            let by = args.by;
            format!("id={:?},name={:?}", by.id.map(|id| id.to_string()), by.name)
        });
    });
    let schema = schema.build().expect("schema builds");
    let input = schema.input_object_type("SearchBy").expect("oneOf input");
    assert!(input.one_of);
    assert!(schema.sdl().contains("@oneOf"), "{}", schema.sdl());

    let by_id = run(&schema, r#"{ search(by: { id: "1" }) }"#, Ctx::anonymous());
    assert!(!by_id.has_errors(), "{by_id:?}");
    assert_eq!(
        value(&by_id, "search").as_str(),
        Some("id=Some(\"1\"),name=None")
    );

    let by_name = run(
        &schema,
        r#"{ search(by: { name: "ada" }) }"#,
        Ctx::anonymous(),
    );
    assert!(!by_name.has_errors(), "{by_name:?}");

    let none = run(&schema, "{ search(by: {}) }", Ctx::anonymous());
    assert!(none.has_errors(), "{none:?}");

    let both = run(
        &schema,
        r#"{ search(by: { id: "1", name: "ada" }) }"#,
        Ctx::anonymous(),
    );
    assert!(both.has_errors(), "{both:?}");

    let null = run(&schema, r#"{ search(by: { id: null }) }"#, Ctx::anonymous());
    assert!(null.has_errors(), "{null:?}");
}

#[test]
fn one_of_requires_option_fields_and_a_name() {
    // Compile-time checks are exercised through the derive's generated code; this test documents
    // the runtime half: the trait reports the flag only for a named input.
    assert!(<SearchBy as gurafi::InputObject>::one_of());
    assert!(<SearchBy as gurafi::InputObject>::NAME.is_some());
    assert!(!<FriendsArgs as gurafi::InputObject>::one_of());
    assert!(<FriendsArgs as gurafi::InputObject>::NAME.is_none());
}

// ---------------------------------------------------------------------------------------------
// A8 — interface resolver-only fields
// ---------------------------------------------------------------------------------------------

#[test]
fn resolve_inherited_copies_type_and_arguments_from_the_interface() {
    let mut schema = Schema::<Ctx>::new();
    let node = schema.interface("Node", |i| {
        i.field_with::<String, IdArgs>("label")
            .description("Inherited label.");
    });
    schema.object::<User>(|o| {
        o.implements(&node);
        o.resolve_inherited("label", |u: &User, _: IdArgs, _: &Ctx| {
            format!("user:{}", u.name)
        });
        o.expose(expose!(User::name));
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
        .get_field("label")
        .expect("inherited field");
    assert_eq!(field.ty.to_string(), "String!");
    assert!(field.args.contains_key("id"));
    assert_eq!(field.description.as_deref(), Some("Inherited label."));

    let response = run(&schema, r#"{ user { label(id: "1") } }"#, Ctx::anonymous());
    assert!(!response.has_errors(), "{response:?}");
    assert_eq!(
        value(&response, "user")
            .as_object()
            .and_then(|user| user.get("label"))
            .and_then(gql::Value::as_str),
        Some("user:ada")
    );
}

#[test]
fn resolve_inherited_reports_missing_fields_and_argument_mismatches() {
    let mut schema = Schema::<Ctx>::new();
    let node = schema.interface("Node", |i| {
        i.field_with::<String, OtherArgs>("label");
    });
    schema.object::<User>(|o| {
        o.implements(&node);
        o.resolve_inherited("label", |u: &User, _: IdArgs, _: &Ctx| u.name.clone());
    });
    schema.query(|q| {
        q.field("user", |_: NoArgs, _: &Ctx| User {
            name: "ada".into(),
            age: 36,
        });
    });
    let error = schema
        .build()
        .expect_err("arguments must match the interface");
    assert!(
        error.to_string().contains("arguments do not match"),
        "{error}"
    );

    let mut schema = Schema::<Ctx>::new();
    let node = schema.interface("Node", |i| {
        i.field::<ID>("id");
    });
    schema.object::<User>(|o| {
        o.implements(&node);
        o.resolve_inherited("missing", |u: &User, _: NoArgs, _: &Ctx| u.name.clone());
    });
    schema.query(|q| {
        q.field("user", |_: NoArgs, _: &Ctx| User {
            name: "ada".into(),
            age: 36,
        });
    });
    let error = schema.build().expect_err("the interface has no such field");
    assert!(
        error.to_string().contains("no implemented interface"),
        "{error}"
    );

    let mut schema = Schema::<Ctx>::new();
    let node = schema.interface("Node", |i| {
        i.field::<String>("label");
    });
    schema.object::<User>(|o| {
        o.implements(&node);
        // The interface declares String, but the resolver returns an integer.
        o.resolve_inherited("label", |u: &User, _: NoArgs, _: &Ctx| u.age);
    });
    schema.query(|q| {
        q.field("user", |_: NoArgs, _: &Ctx| User {
            name: "ada".into(),
            age: 36,
        });
    });
    let error = schema.build().expect_err("the inherited type must match");
    assert!(error.to_string().contains("inherits `String!`"), "{error}");
}

// ---------------------------------------------------------------------------------------------
// N2 — subscription mapping
// ---------------------------------------------------------------------------------------------

#[derive(Clone, Debug)]
struct BornEvent {
    name: String,
}

#[test]
fn subscriptions_wrap_field_values_under_the_field_name() {
    let mut schema = Schema::<Ctx>::new();
    schema.query(|q| {
        q.field("ok", |_: NoArgs, _: &Ctx| true);
    });
    schema.subscription(|s| {
        s.field("born", |_: NoArgs, _: &Ctx| {
            stream::iter(vec![
                GurafiResult::Ok("Melman".to_owned()),
                GurafiResult::Err(GraphQLError::new("dropped")),
                GurafiResult::Ok("Karen".to_owned()),
            ])
        });
    });
    let schema = schema.build().expect("schema builds");
    let response = futures::executor::block_on(gql::subscribe(
        &schema,
        gql::Request::new("subscription { born }"),
        &gql::ExecutionOptions::with_context(Ctx::anonymous()),
    ))
    .expect("subscription starts");
    let events = futures::executor::block_on(response.stream.take(3).collect::<Vec<_>>());
    assert_eq!(events.len(), 3);
    assert_eq!(
        data(&events[0]).get("born"),
        Some(&gql::Value::String(Arc::from("Melman")))
    );
    assert!(events[1].has_errors());
    assert_eq!(
        data(&events[2]).get("born"),
        Some(&gql::Value::String(Arc::from("Karen")))
    );
}

#[test]
fn subscription_fields_can_receive_resolve_info() {
    let mut schema = Schema::<Ctx>::new();
    schema.query(|q| {
        q.field("ok", |_: NoArgs, _: &Ctx| true);
    });
    schema.subscription(|s| {
        s.field_with_info(
            "events",
            |_: NoArgs, _: &Ctx, info: &gql::ResolveInfo<'_>| {
                stream::iter(vec![GurafiResult::Ok(info.field_name.to_owned())])
            },
        );
    });
    let schema = schema.build().expect("schema builds");
    let response = futures::executor::block_on(gql::subscribe(
        &schema,
        gql::Request::new("subscription { events }"),
        &gql::ExecutionOptions::with_context(Ctx::anonymous()),
    ))
    .expect("subscription starts");
    let events = futures::executor::block_on(response.stream.take(1).collect::<Vec<_>>());
    assert_eq!(
        data(&events[0]).get("events"),
        Some(&gql::Value::String(Arc::from("events")))
    );
}

#[test]
fn field_map_maps_a_separate_event_type_to_the_field_value() {
    let mut schema = Schema::<Ctx>::new();
    schema.query(|q| {
        q.field("ok", |_: NoArgs, _: &Ctx| true);
    });
    schema.subscription(|s| {
        s.field_map::<NoArgs, BornEvent, String, _, _, _>(
            "born",
            |_: NoArgs, _: &Ctx| {
                stream::iter(vec![GurafiResult::Ok(BornEvent {
                    name: "Melman".into(),
                })])
            },
            |event: &BornEvent, _: NoArgs, _: &Ctx| Ok(event.name.clone()),
        );
    });
    let schema = schema.build().expect("schema builds");
    let response = futures::executor::block_on(gql::subscribe(
        &schema,
        gql::Request::new("subscription { born }"),
        &gql::ExecutionOptions::with_context(Ctx::anonymous()),
    ))
    .expect("subscription starts");
    let events = futures::executor::block_on(response.stream.take(1).collect::<Vec<_>>());
    assert_eq!(
        data(&events[0]).get("born"),
        Some(&gql::Value::String(Arc::from("Melman")))
    );
}

// ---------------------------------------------------------------------------------------------
// A11 — Clone, build_with and build_clone
// ---------------------------------------------------------------------------------------------

#[test]
fn schemas_are_cloneable_and_build_with_options() {
    let mut schema = Schema::<Ctx>::new();
    schema.object::<User>(|o| {
        o.expose(expose!(User::name));
    });
    schema.object::<Post>(|o| {
        o.expose(expose!(Post::title));
    });
    schema.query(|q| {
        q.field("ok", |_: NoArgs, _: &Ctx| true);
    });

    let options = BuildOptions::new()
        .query_description("The query root.")
        .sort_types(true);
    let first = schema
        .clone()
        .build_with(options)
        .expect("first build succeeds");
    let second = schema.build_clone().expect("the clone still builds");
    assert_eq!(
        first
            .object_type("Query")
            .and_then(|query| query.description.as_deref()),
        Some("The query root.")
    );
    assert!(second.object_type("Query").is_some());
    assert!(schema.build().is_ok(), "the original still builds");
}
