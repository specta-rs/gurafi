//! End-to-end tests: build a schema, execute real requests through `gql`, and assert on the
//! responses.

#![allow(dead_code)]

use std::sync::Arc;

use futures::StreamExt;
use gurafi::{
    Enum, GraphQLError, ID, Input, NoArgs, Object, Result as GurafiResult, Scalar, Schema, Union,
    expose,
};

#[derive(Clone, Debug)]
struct Ctx {
    request_id: String,
    logged_in: bool,
}

impl Ctx {
    fn anonymous() -> Self {
        Self {
            request_id: "req-1".into(),
            logged_in: false,
        }
    }

    fn logged_in() -> Self {
        Self {
            request_id: "req-1".into(),
            logged_in: true,
        }
    }
}

#[derive(Object)]
struct Keeper {
    name: String,
    nickname: String,
}

#[derive(Object)]
#[graphql(description = "A long-necked animal.")]
struct Giraffe {
    name: String,
    height: f32,
    keeper: Keeper,
}

impl Giraffe {
    fn sample(name: &str) -> Self {
        Self {
            name: name.into(),
            height: 3.4,
            keeper: Keeper {
                name: "Bob".into(),
                nickname: "Bobby".into(),
            },
        }
    }
}

#[derive(Object)]
struct User {
    name: String,
}

#[derive(Object)]
struct Post {
    title: String,
}

#[derive(Union)]
enum SearchResult {
    User(User),
    Post(Post),
}

#[derive(Enum, Clone, Copy, Debug, PartialEq)]
enum Color {
    #[graphql(name = "RED")]
    Red,
    #[graphql(name = "BLUE")]
    Blue,
}

#[derive(Input)]
struct FriendsArgs {
    #[graphql(default = 10)]
    limit: i32,
    after: Option<String>,
}

#[derive(Input)]
struct IdArgs {
    id: ID,
}

#[derive(Input)]
struct Filter {
    prefix: Option<String>,
    colors: Option<Vec<Color>>,
}

#[derive(Scalar, Clone, Debug, PartialEq)]
#[graphql(name = "Date", description = "An ISO-8601 date.")]
struct Date(String);

#[derive(Input)]
struct DateArgs {
    at: Date,
}

fn build_schema() -> gql::Schema {
    let mut schema = Schema::<Ctx>::new();

    let node = schema.interface("Node", |i| {
        i.field::<ID>("id");
    });

    schema.object::<Keeper>(|o| {
        o.expose(expose!(Keeper::name));
        o.expose(expose!(Keeper::nickname));
    });

    schema.object::<Giraffe>(|o| {
        o.implements(node);
        o.field("id", |g: &Giraffe, _: NoArgs, _: &Ctx| {
            ID::new(match g.name.as_str() {
                "Melman" => "1",
                _ => "2",
            })
        });
        o.expose(expose!(Giraffe::name));
        o.expose(expose!(Giraffe::height));
        o.expose(expose!(Giraffe::keeper::name as "keeperName"));
        o.expose(expose!(Giraffe::keeper::nickname));
        o.field("secretName", |g: &Giraffe, _: NoArgs, _: &Ctx| {
            Some(g.name.clone())
        })
        .auth(|ctx: &Ctx| ctx.logged_in);
        o.field("customDenied", |_: &Giraffe, _: NoArgs, _: &Ctx| {
            Some("nope")
        })
        .auth(|_: &Ctx| false)
        .auth_error("You shall not pass");
    });

    schema.object::<User>(|o| {
        o.implements(node);
        o.field("id", |u: &User, _: NoArgs, _: &Ctx| ID::new(u.name.clone()));
        o.expose(expose!(User::name));
    });
    schema.object::<Post>(|o| {
        o.expose(expose!(Post::title));
    });

    let search = schema.union("SearchResult", |u| {
        u.member::<User>().member::<Post>();
    });

    schema.query(|q| {
        q.field("hello", |_: NoArgs, ctx: &Ctx| {
            format!("Hello {}", ctx.request_id)
        });

        q.field("giraffe", |_: NoArgs, _: &Ctx| Giraffe::sample("Melman"))
            .complexity(2);

        q.field("giraffes", |_: NoArgs, _: &Ctx| {
            vec![Giraffe::sample("Melman"), Giraffe::sample("Karen")]
        });

        q.field("nullableGiraffe", |pairs: IdArgs, _: &Ctx| {
            (pairs.id.as_str() == "1").then(|| Giraffe::sample("Melman"))
        });

        q.field("colors", |_: NoArgs, _: &Ctx| vec![Color::Red, Color::Blue]);

        q.field_result("friends", |args: FriendsArgs, _: &Ctx| {
            GurafiResult::Ok(format!(
                "limit={},after={}",
                args.limit,
                args.after.unwrap_or_else(|| "none".into())
            ))
        });

        q.field("echoDate", |args: DateArgs, _: &Ctx| args.at.clone());

        q.field("filtered", |args: Filter, _: &Ctx| {
            format!(
                "prefix={:?},colors={}",
                args.prefix,
                args.colors.map_or(0, |colors| colors.len())
            )
        });

        q.field("search", |_: NoArgs, _: &Ctx| {
            SearchResult::User(User {
                name: "Oscar".into(),
            })
        })
        .as_abstract(search);

        q.field("node", |_: NoArgs, _: &Ctx| Giraffe::sample("Melman"))
            .as_abstract(node);

        q.field_result(
            "fail",
            |_: NoArgs, _: &Ctx| -> GurafiResult<Option<String>> {
                GurafiResult::Err(GraphQLError::new("boom"))
            },
        );

        q.field_async("asyncValue", |_: NoArgs, _: &Ctx| {
            Box::pin(async move { 42i32 })
        });

        q.field_result_async("asyncFail", |_: NoArgs, _: &Ctx| {
            Box::pin(async move { Err::<String, GraphQLError>(GraphQLError::new("async boom")) })
        });

        q.field_result_async("asyncGiraffe", |args: IdArgs, _: &Ctx| {
            Box::pin(async move {
                GurafiResult::Ok(Some(Giraffe::sample(if args.id.as_str() == "1" {
                    "Melman"
                } else {
                    "Karen"
                })))
            })
        });

        q.field("secret", |_: NoArgs, _: &Ctx| Some("top secret"))
            .auth(|ctx: &Ctx| ctx.logged_in);

        q.field("requiredSecret", |_: NoArgs, _: &Ctx| "always")
            .auth(|ctx: &Ctx| ctx.logged_in);

        q.field("nullableSecret", |_: NoArgs, _: &Ctx| Some("maybe"))
            .auth(|ctx: &Ctx| ctx.logged_in);
    });

    schema.mutation(|m| {
        m.field("rename", |args: FriendsArgs, _: &Ctx| {
            format!("renamed with limit {}", args.limit)
        });
    });

    schema.subscription(|s| {
        s.field("colors", |_: NoArgs, _: &Ctx| {
            futures::stream::iter(vec![GurafiResult::Ok(Color::Blue)])
        });
        s.field("numbers", |_: NoArgs, _: &Ctx| {
            futures::stream::iter(vec![
                GurafiResult::Ok(1i32),
                GurafiResult::Err(GraphQLError::new("dropped event")),
                GurafiResult::Ok(3i32),
            ])
        });
    });

    schema.build().expect("schema builds")
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
        .expect("response has data")
        .as_object()
        .expect("data is an object")
}

fn value(response: &gql::Response, field: &str) -> gql::Value {
    data(response)
        .get(field)
        .cloned()
        .unwrap_or_else(|| panic!("missing field `{field}` in response: {response:?}"))
}

#[test]
fn executes_context_and_expose_fields() {
    let schema = build_schema();
    let response = run(
        &schema,
        "{ hello giraffe { name height keeperName nickname } }",
        Ctx::anonymous(),
    );
    assert!(!response.has_errors(), "unexpected errors: {response:?}");
    assert_eq!(value(&response, "hello").as_str(), Some("Hello req-1"));

    let giraffe = value(&response, "giraffe");
    let giraffe = giraffe.as_object().expect("giraffe object");
    assert_eq!(
        giraffe.get("name").and_then(gql::Value::as_str),
        Some("Melman")
    );
    let height = giraffe
        .get("height")
        .and_then(gql::Value::as_f64)
        .expect("height is a float");
    assert!((height - 3.4).abs() < 0.001);
    assert_eq!(
        giraffe.get("keeperName").and_then(gql::Value::as_str),
        Some("Bob")
    );
    assert_eq!(
        giraffe.get("nickname").and_then(gql::Value::as_str),
        Some("Bobby")
    );
}

#[test]
fn executes_lists_and_enums() {
    let schema = build_schema();
    let response = run(&schema, "{ giraffes { name } colors }", Ctx::anonymous());
    assert!(!response.has_errors(), "unexpected errors: {response:?}");

    let giraffes = value(&response, "giraffes");
    let giraffes = giraffes.as_list().expect("list");
    assert_eq!(giraffes.len(), 2);
    assert_eq!(
        giraffes[1].as_object().and_then(|o| o.get("name")),
        Some(&gql::Value::String(Arc::from("Karen")))
    );

    let colors = value(&response, "colors");
    assert_eq!(
        colors.as_list(),
        Some(
            &[
                gql::Value::Enum(Arc::from("RED")),
                gql::Value::Enum(Arc::from("BLUE"))
            ][..]
        )
    );
}

#[test]
fn applies_argument_defaults_and_overrides() {
    let schema = build_schema();

    let defaulted = run(&schema, "{ friends }", Ctx::anonymous());
    assert!(!defaulted.has_errors(), "unexpected errors: {defaulted:?}");
    assert_eq!(
        value(&defaulted, "friends").as_str(),
        Some("limit=10,after=none")
    );

    let overridden = run(
        &schema,
        r#"{ friends(limit: 2, after: "cursor") }"#,
        Ctx::anonymous(),
    );
    assert!(
        !overridden.has_errors(),
        "unexpected errors: {overridden:?}"
    );
    assert_eq!(
        value(&overridden, "friends").as_str(),
        Some("limit=2,after=cursor")
    );
}

#[test]
fn coerces_nested_input_objects() {
    let schema = build_schema();
    let response = run(
        &schema,
        r#"{ filtered(prefix: "a", colors: [RED, BLUE]) }"#,
        Ctx::anonymous(),
    );
    assert!(!response.has_errors(), "unexpected errors: {response:?}");
    assert_eq!(
        value(&response, "filtered").as_str(),
        Some("prefix=Some(\"a\"),colors=2")
    );
}

#[test]
fn custom_scalars_coerce_on_input_and_output() {
    let schema = build_schema();

    let ok = run(
        &schema,
        r#"{ echoDate(at: "2024-01-01") }"#,
        Ctx::anonymous(),
    );
    assert!(!ok.has_errors(), "unexpected errors: {ok:?}");
    assert_eq!(value(&ok, "echoDate").as_str(), Some("2024-01-01"));

    let invalid = run(&schema, "{ echoDate(at: 5) }", Ctx::anonymous());
    assert!(
        invalid.has_errors(),
        "an integer is not a Date: {invalid:?}"
    );
}

#[test]
fn reports_invalid_arguments_as_graphql_errors() {
    let schema = build_schema();

    let invalid = run(&schema, r#"{ friends(limit: "no") }"#, Ctx::anonymous());
    assert!(invalid.has_errors(), "expected an error: {invalid:?}");
    assert!(invalid.data.is_none() || invalid.data.as_ref().unwrap().is_null());

    let missing = run(&schema, "{ nullableGiraffe { name } }", Ctx::anonymous());
    assert!(missing.has_errors(), "expected an error: {missing:?}");

    let unknown = run(&schema, "{ friends(nope: 1) }", Ctx::anonymous());
    assert!(unknown.has_errors(), "expected an error: {unknown:?}");
}

#[test]
fn ids_accept_strings_and_integers() {
    let schema = build_schema();

    for argument in ["\"1\"", "1"] {
        let response = run(
            &schema,
            &format!("{{ nullableGiraffe(id: {argument}) {{ name }} }}"),
            Ctx::anonymous(),
        );
        assert!(!response.has_errors(), "unexpected errors: {response:?}");
        let giraffe = value(&response, "nullableGiraffe");
        assert_eq!(
            giraffe
                .as_object()
                .and_then(|giraffe| giraffe.get("name"))
                .and_then(gql::Value::as_str),
            Some("Melman")
        );
    }
}

#[test]
fn resolves_interfaces_through_is_type_of() {
    let schema = build_schema();
    let response = run(
        &schema,
        "{ node { id ... on Giraffe { name height } } }",
        Ctx::anonymous(),
    );
    assert!(!response.has_errors(), "unexpected errors: {response:?}");
    let node = value(&response, "node");
    let node = node.as_object().expect("node object");
    assert_eq!(node.get("id").and_then(gql::Value::as_str), Some("1"));
    assert_eq!(
        node.get("name").and_then(gql::Value::as_str),
        Some("Melman")
    );
}

#[test]
fn resolves_unions_through_is_type_of() {
    let schema = build_schema();
    let response = run(
        &schema,
        "{ search { __typename ... on User { name } ... on Post { title } } }",
        Ctx::anonymous(),
    );
    assert!(!response.has_errors(), "unexpected errors: {response:?}");
    let search = value(&response, "search");
    let search = search.as_object().expect("search object");
    assert_eq!(
        search.get("__typename").and_then(gql::Value::as_str),
        Some("User")
    );
    assert_eq!(
        search.get("name").and_then(gql::Value::as_str),
        Some("Oscar")
    );
}

#[test]
fn executes_async_and_fallible_resolvers() {
    let schema = build_schema();

    let ok = run(
        &schema,
        "{ asyncValue asyncGiraffe(id: \"2\") { name } }",
        Ctx::anonymous(),
    );
    assert!(!ok.has_errors(), "unexpected errors: {ok:?}");
    assert_eq!(value(&ok, "asyncValue").as_i32(), Some(42));
    assert_eq!(
        value(&ok, "asyncGiraffe")
            .as_object()
            .and_then(|giraffe| giraffe.get("name"))
            .and_then(gql::Value::as_str),
        Some("Karen")
    );

    let sync_error = run(&schema, "{ fail }", Ctx::anonymous());
    assert!(sync_error.has_errors());
    assert!(sync_error.errors[0].message().contains("boom"));
    assert_eq!(value(&sync_error, "fail"), gql::Value::Null);

    let async_error = run(&schema, "{ asyncFail }", Ctx::anonymous());
    assert!(async_error.has_errors());
    assert!(async_error.errors[0].message().contains("async boom"));
}

#[test]
fn authorization_denies_nullable_fields() {
    let schema = build_schema();
    let response = run(
        &schema,
        "{ secret nullableSecret giraffe { name secretName } }",
        Ctx::anonymous(),
    );
    assert!(response.has_errors(), "expected denials: {response:?}");
    assert_eq!(value(&response, "secret"), gql::Value::Null);
    assert_eq!(value(&response, "nullableSecret"), gql::Value::Null);

    let giraffe = value(&response, "giraffe");
    let giraffe = giraffe.as_object().expect("giraffe should stay non-null");
    assert_eq!(
        giraffe.get("name").and_then(gql::Value::as_str),
        Some("Melman")
    );
    assert_eq!(giraffe.get("secretName"), Some(&gql::Value::Null));
    assert!(
        response
            .errors
            .iter()
            .any(|error| error.message().contains("Not authorized"))
    );
}

#[test]
fn authorization_allows_logged_in_callers() {
    let schema = build_schema();
    let response = run(
        &schema,
        "{ secret requiredSecret giraffe { secretName } }",
        Ctx::logged_in(),
    );
    assert!(!response.has_errors(), "unexpected errors: {response:?}");
    assert_eq!(value(&response, "secret").as_str(), Some("top secret"));
    assert_eq!(value(&response, "requiredSecret").as_str(), Some("always"));
}

#[test]
fn authorization_on_a_non_null_field_nulls_the_parent() {
    let schema = build_schema();
    let response = run(&schema, "{ requiredSecret }", Ctx::anonymous());
    assert!(response.has_errors(), "expected a denial: {response:?}");
    assert!(
        response.data.as_ref().is_none_or(gql::Value::is_null),
        "non-null denial should null the data: {response:?}"
    );
}

#[test]
fn authorization_uses_a_custom_message() {
    let schema = build_schema();
    let response = run(&schema, "{ giraffe { customDenied } }", Ctx::anonymous());
    assert!(
        response
            .errors
            .iter()
            .any(|error| error.message().contains("You shall not pass")),
        "unexpected errors: {response:?}"
    );
}

#[test]
fn mutations_execute() {
    let schema = build_schema();
    let response = run(&schema, "mutation { rename(limit: 3) }", Ctx::anonymous());
    assert!(!response.has_errors(), "unexpected errors: {response:?}");
    assert_eq!(
        value(&response, "rename").as_str(),
        Some("renamed with limit 3")
    );
}

#[test]
fn subscriptions_yield_values_and_errors() {
    let schema = build_schema();
    let options = gql::ExecutionOptions::with_context(Ctx::anonymous());

    let subscription = futures::executor::block_on(gql::subscribe(
        &schema,
        gql::Request::new("subscription { colors }"),
        &options,
    ))
    .expect("subscription starts");

    let first = futures::executor::block_on(subscription.stream.take(1).collect::<Vec<_>>());
    assert_eq!(first.len(), 1);
    assert!(!first[0].has_errors(), "unexpected errors: {:?}", first[0]);
    assert_eq!(
        data(&first[0]).get("colors"),
        Some(&gql::Value::Enum(Arc::from("BLUE")))
    );

    let subscription = futures::executor::block_on(gql::subscribe(
        &schema,
        gql::Request::new("subscription { numbers }"),
        &options,
    ))
    .expect("subscription starts");
    let events = futures::executor::block_on(subscription.stream.take(3).collect::<Vec<_>>());
    assert_eq!(events.len(), 3);
    assert_eq!(data(&events[0]).get("numbers"), Some(&gql::Value::Int(1)));
    assert!(events[1].has_errors());
    assert_eq!(data(&events[2]).get("numbers"), Some(&gql::Value::Int(3)));
}

#[test]
fn rejects_invalid_documents() {
    let schema = build_schema();
    let response = run(&schema, "{ doesNotExist }", Ctx::anonymous());
    assert!(response.has_errors());
    assert!(response.data.is_none() || response.data.as_ref().unwrap().is_null());
}

#[test]
fn complexity_estimates_are_enforced_by_the_executor() {
    let schema = build_schema();
    let mut options = gql::ExecutionOptions::with_context(Ctx::anonymous());
    options.max_complexity = 1;
    let response = futures::executor::block_on(gql::execute(
        &schema,
        gql::Request::new("{ giraffe { name } }"),
        &options,
    ));
    assert!(
        response.has_errors(),
        "a complexity of 2 should exceed a limit of 1: {response:?}"
    );
}
