//! Typed error result unions (the Pothos Errors pattern).

#![allow(dead_code)]

use futures::StreamExt;
use futures::executor::block_on;
use gurafi::{
    ErrorUnion, GraphQLError, Input, NoArgs, Object, Result as GurafiResult, Schema, Union,
};

#[derive(Clone, Default)]
struct Ctx;

#[derive(Object)]
struct User {
    name: String,
    age: i32,
}

#[derive(Object)]
struct Admin {
    name: String,
}

#[derive(Object)]
struct Account {
    name: String,
}

#[derive(Object)]
struct NotFoundError {
    message: String,
    code: String,
}

#[derive(Object)]
struct ConflictError {
    message: String,
    field: String,
}

#[derive(Object)]
struct NestedDetail {
    reason: String,
}

#[derive(Object)]
struct NestedError {
    code: String,
    detail: NestedDetail,
}

#[derive(ErrorUnion)]
enum CreateUserError {
    NotFound(NotFoundError),
    Conflict(ConflictError),
}

#[derive(ErrorUnion)]
enum NestedErrorUnion {
    Nested(NestedError),
}

#[derive(Union)]
enum SearchResult {
    User(User),
    Admin(Admin),
}

#[derive(Input)]
struct CreateArgs {
    fail: Option<String>,
}

fn user(name: &str) -> User {
    User {
        name: name.into(),
        age: 36,
    }
}

fn execute(schema: &gql::Schema, query: &str) -> gql::Response {
    block_on(gql::execute(
        schema,
        gql::Request::new(query),
        &gql::ExecutionOptions::with_context(Ctx),
    ))
}

fn execute_ok(schema: &gql::Schema, query: &str) -> gql::Response {
    let response = execute(schema, query);
    assert!(!response.has_errors(), "{:?}", response.errors);
    response
}

fn root_field<'a>(response: &'a gql::Response, name: &str) -> &'a gql::Value {
    response
        .data
        .as_ref()
        .and_then(gql::Value::as_object)
        .and_then(|object| object.get(name))
        .unwrap_or_else(|| panic!("missing root field `{name}`: {:?}", response.data))
}

fn register_errors(schema: &mut Schema<Ctx>) {
    schema.object::<NotFoundError>(|o| {
        o.field("message", |e: &NotFoundError, _: NoArgs, _: &Ctx| {
            e.message.clone()
        });
        o.field("code", |e: &NotFoundError, _: NoArgs, _: &Ctx| {
            e.code.clone()
        });
    });
    schema.object::<ConflictError>(|o| {
        o.field("message", |e: &ConflictError, _: NoArgs, _: &Ctx| {
            e.message.clone()
        });
        o.field("field", |e: &ConflictError, _: NoArgs, _: &Ctx| {
            e.field.clone()
        });
    });
}

fn build_create_user_schema() -> gql::Schema {
    let mut schema = Schema::<Ctx>::new();
    schema.object::<User>(|o| {
        o.field("name", |u: &User, _: NoArgs, _: &Ctx| u.name.clone());
        o.field("age", |u: &User, _: NoArgs, _: &Ctx| u.age);
    });
    register_errors(&mut schema);
    schema.query(|q| {
        q.field("ok", |_: NoArgs, _: &Ctx| true);
    });
    schema.mutation(|m| {
        m.field_result_union::<User, CreateUserError, CreateArgs, _>(
            "createUser",
            |args: CreateArgs, _: &Ctx| match args.fail.as_deref() {
                Some("notFound") => Err(CreateUserError::NotFound(NotFoundError {
                    message: "No such user".into(),
                    code: "NOT_FOUND".into(),
                })),
                Some("conflict") => Err(CreateUserError::Conflict(ConflictError {
                    message: "Already exists".into(),
                    field: "email".into(),
                })),
                _ => Ok(user("ada")),
            },
        );
    });
    schema.build().expect("valid schema")
}

#[test]
fn a_result_union_exposes_a_success_payload_and_every_error_variant() {
    let schema = build_create_user_schema();

    let sdl = schema.sdl();
    assert!(
        sdl.contains("createUser(fail: String): MutationCreateUserResult!"),
        "SDL was:\n{sdl}"
    );
    assert!(
        sdl.contains("type MutationCreateUserSuccess"),
        "SDL was:\n{sdl}"
    );
    assert!(
        sdl.contains("data: User!"),
        "success data keeps T's non-null type; SDL was:\n{sdl}"
    );
    assert!(
        sdl.contains(
            "union MutationCreateUserResult = MutationCreateUserSuccess | NotFoundError | ConflictError"
        ),
        "SDL was:\n{sdl}"
    );

    let introspection = execute_ok(
        &schema,
        "{ __type(name: \"MutationCreateUserResult\") { kind possibleTypes { name } } }",
    );
    let union = root_field(&introspection, "__type")
        .as_object()
        .expect("__type");
    assert_eq!(
        union.get("kind").and_then(gql::Value::as_str),
        Some("UNION")
    );
    let possible = union
        .get("possibleTypes")
        .and_then(gql::Value::as_list)
        .expect("possibleTypes");
    let names: Vec<&str> = possible
        .iter()
        .filter_map(gql::Value::as_object)
        .filter_map(|member| member.get("name"))
        .filter_map(gql::Value::as_str)
        .collect();
    assert_eq!(
        names,
        vec![
            "MutationCreateUserSuccess",
            "NotFoundError",
            "ConflictError"
        ]
    );

    let response = execute_ok(
        &schema,
        "mutation { createUser { __typename ... on MutationCreateUserSuccess { data { name age } } } }",
    );
    let result = root_field(&response, "createUser")
        .as_object()
        .expect("union object");
    assert_eq!(
        result.get("__typename").and_then(gql::Value::as_str),
        Some("MutationCreateUserSuccess")
    );
    let data = result
        .get("data")
        .and_then(gql::Value::as_object)
        .expect("success data");
    assert_eq!(data.get("name").and_then(gql::Value::as_str), Some("ada"));
    assert_eq!(data.get("age").and_then(gql::Value::as_i32), Some(36));
}

#[test]
fn each_error_variant_is_a_selectable_union_member() {
    let schema = build_create_user_schema();

    let response = execute_ok(
        &schema,
        "mutation { createUser(fail: \"notFound\") { __typename ... on NotFoundError { message code } } }",
    );
    let result = root_field(&response, "createUser")
        .as_object()
        .expect("union object");
    assert_eq!(
        result.get("__typename").and_then(gql::Value::as_str),
        Some("NotFoundError")
    );
    assert_eq!(
        result.get("code").and_then(gql::Value::as_str),
        Some("NOT_FOUND")
    );

    let response = execute_ok(
        &schema,
        "mutation { createUser(fail: \"conflict\") { __typename ... on ConflictError { field message } } }",
    );
    let result = root_field(&response, "createUser")
        .as_object()
        .expect("union object");
    assert_eq!(
        result.get("__typename").and_then(gql::Value::as_str),
        Some("ConflictError")
    );
    assert_eq!(
        result.get("field").and_then(gql::Value::as_str),
        Some("email")
    );

    // A typed error is data, not a GraphQL error.
    let response = execute(
        &schema,
        "mutation { createUser(fail: \"notFound\") { ... on NotFoundError { code } } }",
    );
    assert!(!response.has_errors(), "{:?}", response.errors);
}

#[test]
fn a_nested_error_payload_resolves_its_own_object_fields() {
    let mut schema = Schema::<Ctx>::new();
    schema.object::<NestedDetail>(|o| {
        o.field("reason", |d: &NestedDetail, _: NoArgs, _: &Ctx| {
            d.reason.clone()
        });
    });
    schema.object::<NestedError>(|o| {
        o.field("code", |e: &NestedError, _: NoArgs, _: &Ctx| e.code.clone());
        o.field("detail", |e: &NestedError, _: NoArgs, _: &Ctx| {
            NestedDetail {
                reason: e.detail.reason.clone(),
            }
        });
    });
    schema.query(|q| {
        q.field_result_union::<String, NestedErrorUnion, NoArgs, _>(
            "explode",
            |_: NoArgs, _: &Ctx| {
                Err(NestedErrorUnion::Nested(NestedError {
                    code: "NESTED".into(),
                    detail: NestedDetail {
                        reason: "inner".into(),
                    },
                }))
            },
        );
    });

    let built = schema.build().expect("valid schema");
    let response = execute_ok(
        &built,
        "{ explode { ... on NestedError { code detail { reason } } } }",
    );
    let result = root_field(&response, "explode")
        .as_object()
        .expect("union object");
    assert_eq!(
        result.get("code").and_then(gql::Value::as_str),
        Some("NESTED")
    );
    assert_eq!(
        result
            .get("detail")
            .and_then(gql::Value::as_object)
            .and_then(|detail| detail.get("reason"))
            .and_then(gql::Value::as_str),
        Some("inner")
    );
}

#[test]
fn async_object_fields_and_scalar_list_and_optional_success_data_work() {
    let mut schema = Schema::<Ctx>::new();
    schema.object::<User>(|o| {
        o.field("name", |u: &User, _: NoArgs, _: &Ctx| u.name.clone());
    });
    schema.object::<Account>(|o| {
        o.field("name", |a: &Account, _: NoArgs, _: &Ctx| a.name.clone());
        o.field_result_union_async::<User, CreateUserError, NoArgs, _>(
            "owner",
            |_: &Account, _: NoArgs, _: &Ctx| Box::pin(async move { Ok(user("async-ada")) }),
        );
    });
    register_errors(&mut schema);
    schema.query(|q| {
        q.field("account", |_: NoArgs, _: &Ctx| Account {
            name: "acme".into(),
        });
        q.field_result_union::<i32, CreateUserError, NoArgs, _>("count", |_: NoArgs, _: &Ctx| {
            Ok(3)
        });
        q.field_result_union::<Vec<String>, CreateUserError, NoArgs, _>(
            "names",
            |_: NoArgs, _: &Ctx| Ok(vec!["ada".into(), "grace".into()]),
        );
        q.field_result_union::<Option<User>, CreateUserError, NoArgs, _>(
            "maybeUser",
            |_: NoArgs, _: &Ctx| Ok(None),
        );
    });

    let built = schema.build().expect("valid schema");
    let sdl = built.sdl();
    assert!(
        sdl.contains("data: Int!"),
        "scalar success data; SDL was:\n{sdl}"
    );
    assert!(
        sdl.contains("data: [String!]!"),
        "list success data; SDL was:\n{sdl}"
    );
    assert!(
        sdl.contains("data: User\n"),
        "optional success data is nullable; SDL was:\n{sdl}"
    );

    let response = execute_ok(
        &built,
        "{ account { owner { ... on AccountOwnerSuccess { data { name } } } } }",
    );
    let account = root_field(&response, "account")
        .as_object()
        .expect("account");
    let owner = account
        .get("owner")
        .and_then(gql::Value::as_object)
        .expect("owner union");
    assert_eq!(
        owner
            .get("data")
            .and_then(gql::Value::as_object)
            .and_then(|data| data.get("name"))
            .and_then(gql::Value::as_str),
        Some("async-ada")
    );

    let response = execute_ok(
        &built,
        "{ count { ... on QueryCountSuccess { data } } names { ... on QueryNamesSuccess { data } } maybeUser { ... on QueryMaybeUserSuccess { data { name } } } }",
    );
    assert_eq!(
        root_field(&response, "count")
            .as_object()
            .and_then(|value| value.get("data"))
            .and_then(gql::Value::as_i32),
        Some(3)
    );
    assert_eq!(
        root_field(&response, "names")
            .as_object()
            .and_then(|value| value.get("data"))
            .and_then(gql::Value::as_list)
            .map(<[gql::Value]>::len),
        Some(2)
    );
    assert!(
        root_field(&response, "maybeUser")
            .as_object()
            .and_then(|value| value.get("data"))
            .is_some_and(gql::Value::is_null)
    );
}

#[test]
fn abstract_success_data_keeps_the_abstract_type() {
    let mut schema = Schema::<Ctx>::new();
    schema.object::<User>(|o| {
        o.field("name", |u: &User, _: NoArgs, _: &Ctx| u.name.clone());
    });
    schema.object::<Admin>(|o| {
        o.field("name", |a: &Admin, _: NoArgs, _: &Ctx| a.name.clone());
    });
    register_errors(&mut schema);
    schema.union("SearchResult", |u| {
        u.member::<User>().member::<Admin>();
    });
    schema.query(|q| {
        q.field_result_union::<SearchResult, CreateUserError, NoArgs, _>(
            "search",
            |_: NoArgs, _: &Ctx| Ok(SearchResult::User(user("ada"))),
        );
    });

    let built = schema.build().expect("valid schema");
    let sdl = built.sdl();
    assert!(
        sdl.contains("data: SearchResult!"),
        "abstract success data; SDL was:\n{sdl}"
    );

    let response = execute_ok(
        &built,
        "{ search { ... on QuerySearchSuccess { data { __typename ... on User { name } } } } }",
    );
    let data = root_field(&response, "search")
        .as_object()
        .and_then(|value| value.get("data"))
        .and_then(gql::Value::as_object)
        .expect("abstract data");
    assert_eq!(
        data.get("__typename").and_then(gql::Value::as_str),
        Some("User")
    );
    assert_eq!(data.get("name").and_then(gql::Value::as_str), Some("ada"));
}

#[test]
fn a_missing_error_payload_is_a_clear_build_error() {
    let mut schema = Schema::<Ctx>::new();
    schema.object::<User>(|o| {
        o.field("name", |u: &User, _: NoArgs, _: &Ctx| u.name.clone());
    });
    // Deliberately not registering `NotFoundError` or `ConflictError`.
    schema.query(|q| {
        q.field_result_union::<User, CreateUserError, NoArgs, _>(
            "createUser",
            |_: NoArgs, _: &Ctx| Ok(user("ada")),
        );
    });

    let error = schema.build().expect_err("missing payloads must fail");
    let message = error.to_string();
    assert!(
        message.contains("Error union member")
            && message.contains("Query.createUser")
            && message.contains("NotFoundError")
            && message.contains("never registered"),
        "unexpected error:\n{message}"
    );
    assert!(
        message.contains("ConflictError"),
        "every missing payload should be reported:\n{message}"
    );
}

#[test]
fn reopening_and_rebuilding_does_not_duplicate_generated_types() {
    let mut schema = Schema::<Ctx>::new();
    schema.object::<User>(|o| {
        o.field("name", |u: &User, _: NoArgs, _: &Ctx| u.name.clone());
    });
    register_errors(&mut schema);
    schema.object::<Account>(|o| {
        o.field("name", |a: &Account, _: NoArgs, _: &Ctx| a.name.clone());
        o.field_result_union::<User, CreateUserError, NoArgs, _>(
            "owner",
            |_: &Account, _: NoArgs, _: &Ctx| Ok(user("ada")),
        );
    });
    schema.query(|q| {
        q.field("account", |_: NoArgs, _: &Ctx| Account {
            name: "acme".into(),
        });
    });

    let first = schema.build_clone().expect("first build");
    assert!(first.object_type("AccountOwnerSuccess").is_some());
    assert!(first.union_type("AccountOwnerResult").is_some());

    // Reopening the object with another result-union field must not re-register the first
    // generated pair, and building from a clone must remain repeatable.
    schema.object_fields::<Account>(|o| {
        o.field_result_union::<User, CreateUserError, NoArgs, _>(
            "secondOwner",
            |_: &Account, _: NoArgs, _: &Ctx| Ok(user("grace")),
        );
    });
    let second = schema.build_clone().expect("second build");
    let third = schema.build_clone().expect("third build");

    let sdl = third.sdl();
    assert_eq!(
        sdl.matches("type AccountOwnerSuccess").count(),
        1,
        "SDL was:\n{sdl}"
    );
    assert_eq!(
        sdl.matches("union AccountOwnerResult").count(),
        1,
        "SDL was:\n{sdl}"
    );
    assert!(
        sdl.contains("type AccountSecondOwnerSuccess"),
        "SDL was:\n{sdl}"
    );
    assert!(second.union_type("AccountSecondOwnerResult").is_some());
}

#[test]
fn resolver_errors_still_behave_like_resolver_errors() {
    let mut schema = Schema::<Ctx>::new();
    register_errors(&mut schema);
    schema.query(|q| {
        q.field_result_union::<String, CreateUserError, NoArgs, _>(
            "typed",
            |_: NoArgs, _: &Ctx| {
                Err(CreateUserError::NotFound(NotFoundError {
                    message: "typed".into(),
                    code: "TYPED".into(),
                }))
            },
        );
        q.field_result::<String, NoArgs, _>("boom", |_: NoArgs, _: &Ctx| {
            GurafiResult::Err(GraphQLError::new("ordinary resolver failure"))
        });
    });

    let built = schema.build().expect("valid schema");

    let typed = execute_ok(&built, "{ typed { ... on NotFoundError { code } } }");
    assert_eq!(
        root_field(&typed, "typed")
            .as_object()
            .and_then(|value| value.get("code"))
            .and_then(gql::Value::as_str),
        Some("TYPED")
    );

    let boom = execute(&built, "{ boom }");
    assert!(boom.has_errors());
    assert!(
        boom.errors
            .iter()
            .any(|error| error.message().contains("ordinary resolver failure")),
        "{:?}",
        boom.errors
    );
}

#[test]
fn generated_names_follow_runtime_object_and_configured_root_names() {
    let mut schema = Schema::<Ctx>::new();
    schema.query_name("RootQuery");
    schema.object::<User>(|o| {
        o.field("name", |u: &User, _: NoArgs, _: &Ctx| u.name.clone());
    });
    register_errors(&mut schema);

    let admin = schema.object_ref::<Account>("AdminAccount");
    schema.implement(&admin, |o| {
        o.field("name", |a: &Account, _: NoArgs, _: &Ctx| a.name.clone());
        o.field_result_union::<User, CreateUserError, NoArgs, _>(
            "owner",
            |_: &Account, _: NoArgs, _: &Ctx| Ok(user("ada")),
        );
    });
    schema.query(|q| {
        q.field_ref(&admin, "admin", |_: NoArgs, _: &Ctx| Account {
            name: "acme".into(),
        });
        q.field_result_union::<String, CreateUserError, NoArgs, _>("ping", |_: NoArgs, _: &Ctx| {
            Ok("pong".to_owned())
        });
    });

    let built = schema.build().expect("valid schema");
    let sdl = built.sdl();
    assert!(
        sdl.contains("type AdminAccountOwnerSuccess"),
        "SDL was:\n{sdl}"
    );
    assert!(
        sdl.contains("union AdminAccountOwnerResult"),
        "SDL was:\n{sdl}"
    );
    assert!(sdl.contains("union RootQueryPingResult"), "SDL was:\n{sdl}");
}

#[test]
fn typed_result_subscriptions_emit_success_and_error_payloads() {
    let mut schema = Schema::<Ctx>::new();
    schema.object::<User>(|o| {
        o.field("name", |u: &User, _: NoArgs, _: &Ctx| u.name.clone());
    });
    register_errors(&mut schema);
    schema.query(|q| {
        q.field("ok", |_: NoArgs, _: &Ctx| true);
    });
    schema.subscription(|s| {
        s.field_result_union::<User, CreateUserError, NoArgs, _, _>(
            "created",
            |_: NoArgs, _: &Ctx| {
                futures::stream::iter(vec![
                    Ok(user("ada")),
                    Err(CreateUserError::Conflict(ConflictError {
                        message: "taken".into(),
                        field: "email".into(),
                    })),
                ])
            },
        );
    });

    let built = schema.build().expect("valid schema");
    let subscription = block_on(gql::subscribe(
        &built,
        gql::Request::new(
            "subscription { created { __typename ... on SubscriptionCreatedSuccess { data { name } } ... on ConflictError { field } } }",
        ),
        &gql::ExecutionOptions::with_context(Ctx),
    ))
    .expect("subscription starts");
    let events = block_on(subscription.stream.take(2).collect::<Vec<_>>());
    assert_eq!(events.len(), 2);
    assert!(!events[0].has_errors(), "{:?}", events[0].errors);
    assert!(!events[1].has_errors(), "{:?}", events[1].errors);

    let first = events[0]
        .data
        .as_ref()
        .and_then(gql::Value::as_object)
        .and_then(|object| object.get("created"))
        .and_then(gql::Value::as_object)
        .expect("first event");
    assert_eq!(
        first.get("__typename").and_then(gql::Value::as_str),
        Some("SubscriptionCreatedSuccess")
    );

    let second = events[1]
        .data
        .as_ref()
        .and_then(gql::Value::as_object)
        .and_then(|object| object.get("created"))
        .and_then(gql::Value::as_object)
        .expect("second event");
    assert_eq!(
        second.get("__typename").and_then(gql::Value::as_str),
        Some("ConflictError")
    );
    assert_eq!(
        second.get("field").and_then(gql::Value::as_str),
        Some("email")
    );
}
