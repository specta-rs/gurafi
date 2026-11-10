//! Runtime-named object, interface and union references.
//!
//! These cover the names that only exist at runtime — Relay's `UserPostsConnection` and
//! `CreateUserPayload` are the motivating examples — without giving up model-backed resolvers
//! and `is_type_of` checks.

#![allow(dead_code)]

use futures::executor::block_on;
use gurafi::{NoArgs, Object, Schema, Union, expose};

#[derive(Clone, Default)]
struct Ctx;

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
struct Admin {
    name: String,
}

#[derive(Union)]
enum SearchResult {
    User(User),
    Post(Post),
}

#[derive(Object)]
struct UserPostsConnection;

#[derive(Object)]
struct CreateUserPayload;

fn user(name: &str) -> User {
    User {
        name: name.into(),
        age: 36,
    }
}

fn execute(schema: &gql::Schema, query: &str) -> gql::Response {
    let response = block_on(gql::execute(
        schema,
        gql::Request::new(query),
        &gql::ExecutionOptions::with_context(Ctx),
    ));
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

#[test]
fn a_dynamic_object_name_reaches_sdl_and_introspection() {
    let mut schema = Schema::<Ctx>::new();
    let admin = schema.object_ref::<User>("AdminUser");
    schema.implement(&admin, |o| {
        o.expose(expose!(User::name));
        o.field("id", |u: &User, _: NoArgs, _: &Ctx| u.name.clone());
    });
    schema.query(|q| {
        q.field_ref(&admin, "admin", |_: NoArgs, _: &Ctx| user("ada"));
    });

    let built = schema.build().expect("valid schema");

    assert!(built.object_type("AdminUser").is_some());
    assert!(built.object_type("User").is_none());
    let sdl = built.sdl();
    assert!(sdl.contains("type AdminUser"), "SDL was:\n{sdl}");
    assert!(sdl.contains("admin: AdminUser"), "SDL was:\n{sdl}");

    let response = execute(&built, "{ admin { __typename id name } }");
    let admin = root_field(&response, "admin").as_object().expect("object");
    assert_eq!(
        admin.get("__typename").and_then(gql::Value::as_str),
        Some("AdminUser")
    );
    assert_eq!(admin.get("name").and_then(gql::Value::as_str), Some("ada"));
}

#[test]
fn dynamic_interfaces_and_unions_resolve_abstract_values() {
    let mut schema = Schema::<Ctx>::new();

    let node = schema.interface_ref("Node");
    schema.implement_interface(&node, |i| {
        i.field::<String>("id");
    });

    let user_ref = schema.object_ref::<User>("AdminUser");
    schema.implement(&user_ref, |o| {
        o.implements(&node);
        o.field("id", |u: &User, _: NoArgs, _: &Ctx| u.name.clone());
        o.expose(expose!(User::name));
    });

    let post_ref = schema.object_ref::<Post>("AdminPost");
    schema.implement(&post_ref, |o| {
        o.implements(&node);
        o.field("id", |p: &Post, _: NoArgs, _: &Ctx| p.title.clone());
        o.expose(expose!(Post::title));
    });

    // The runtime union name deliberately differs from the derived `SearchResult` name.
    let search = schema.union_ref("Search");
    schema.implement_union(&search, |u| {
        u.member_ref(&user_ref).member_ref(&post_ref);
    });

    schema.query(|q| {
        q.field_ref(&user_ref, "node", |_: NoArgs, _: &Ctx| user("ada"))
            .as_abstract(&node);
        q.field_ref_type::<SearchResult, NoArgs, _>(
            search.type_ref(),
            "search",
            |_: NoArgs, _: &Ctx| SearchResult::User(user("ada")),
        );
    });

    let built = schema.build().expect("valid schema");

    assert!(user_ref.is_possible_type(&built, "Node"));
    assert!(search.is_possible_type(&built, "AdminUser"));
    assert_eq!(
        node.possible_types(&built)
            .iter()
            .map(ToString::to_string)
            .collect::<Vec<String>>(),
        vec!["AdminUser".to_owned(), "AdminPost".to_owned()]
    );

    let sdl = built.sdl();
    assert!(
        sdl.contains("union Search = AdminUser | AdminPost"),
        "SDL was:\n{sdl}"
    );
    assert!(sdl.contains("node: Node"), "SDL was:\n{sdl}");

    let response = execute(
        &built,
        "{ node { __typename id ... on AdminUser { name } } search { __typename ... on AdminUser { name } } }",
    );
    let node = root_field(&response, "node").as_object().expect("object");
    assert_eq!(
        node.get("__typename").and_then(gql::Value::as_str),
        Some("AdminUser")
    );
    assert_eq!(node.get("name").and_then(gql::Value::as_str), Some("ada"));

    let search = root_field(&response, "search").as_object().expect("object");
    assert_eq!(
        search.get("__typename").and_then(gql::Value::as_str),
        Some("AdminUser")
    );
}

#[test]
fn reopening_a_dynamic_ref_merges_fields() {
    let mut schema = Schema::<Ctx>::new();

    let first = schema.object_ref::<User>("AdminUser");
    schema.implement(&first, |o| {
        o.field("id", |u: &User, _: NoArgs, _: &Ctx| u.name.clone());
    });

    let second = schema.object_ref::<User>("AdminUser");
    schema.implement(&second, |o| {
        o.expose(expose!(User::name));
    });

    schema.query(|q| {
        q.field_ref(&first, "admin", |_: NoArgs, _: &Ctx| user("ada"));
    });

    let built = schema.build().expect("valid schema");
    let admin = built.object_type("AdminUser").expect("AdminUser");
    assert!(admin.fields.contains_key("id"));
    assert!(admin.fields.contains_key("name"));
}

#[test]
fn two_models_claiming_the_same_dynamic_name_is_an_error() {
    let mut schema = Schema::<Ctx>::new();

    let user_ref = schema.object_ref::<User>("Shared");
    schema.implement(&user_ref, |o| {
        o.expose(expose!(User::name));
    });

    // `Admin` is a different Rust model, so it may not claim `Shared`.
    let admin_ref = schema.object_ref::<Admin>("Shared");
    schema.implement(&admin_ref, |o| {
        o.expose(expose!(Admin::name));
    });

    schema.query(|q| {
        q.field_ref(&user_ref, "shared", |_: NoArgs, _: &Ctx| user("ada"));
    });

    let error = schema
        .build()
        .expect_err("the name collision must fail the build");
    assert!(
        error.to_string().contains("more than one Rust type"),
        "error was: {error}"
    );
}

#[test]
fn derived_names_and_refs_still_work() {
    let mut schema = Schema::<Ctx>::new();

    let user_ref = schema.object::<User>(|o| {
        o.expose(expose!(User::name));
    });
    assert_eq!(user_ref.name(), "User");
    assert_eq!(user_ref.model_name(), "User");

    schema.query(|q| {
        q.field("user", |_: NoArgs, _: &Ctx| user("ada"));
    });

    let built = schema.build().expect("valid schema");
    assert!(built.object_type("User").is_some());

    let response = execute(&built, "{ user { name } }");
    let object = root_field(&response, "user").as_object().expect("object");
    assert_eq!(object.get("name").and_then(gql::Value::as_str), Some("ada"));
}

#[test]
fn dynamic_refs_support_lists_and_nullability() {
    let mut schema = Schema::<Ctx>::new();

    let admin = schema.object_ref::<User>("AdminUser");
    schema.implement(&admin, |o| {
        o.expose(expose!(User::name));
    });

    schema.query(|q| {
        q.field_ref::<Vec<User>, NoArgs, _, _>(&admin, "all", |_: NoArgs, _: &Ctx| {
            vec![user("ada")]
        });
        q.field_ref::<Option<Vec<User>>, NoArgs, _, _>(&admin, "maybe", |_: NoArgs, _: &Ctx| {
            Some(vec![user("ada")])
        });
        q.field_ref::<Vec<User>, NoArgs, _, _>(&admin, "nullableItems", |_: NoArgs, _: &Ctx| {
            vec![user("ada")]
        })
        .nullable_items();
        q.field_ref_type::<Vec<User>, NoArgs, _>(
            admin.type_ref().list().non_null(),
            "explicit",
            |_: NoArgs, _: &Ctx| vec![user("ada"), user("grace")],
        );
        q.field_ref::<Vec<User>, NoArgs, _, _>(&admin, "explicitNonNull", |_: NoArgs, _: &Ctx| {
            vec![user("ada"), user("grace")]
        })
        .non_null_items();
        q.field_ref(&admin, "one", |_: NoArgs, _: &Ctx| user("ada"))
            .non_null();
    });

    let built = schema.build().expect("valid schema");
    let query = built.object_type("Query").expect("Query");
    let ty = |name: &str| query.fields.get(name).expect(name).ty.to_string();

    assert_eq!(ty("all"), "[AdminUser]!");
    assert_eq!(ty("maybe"), "[AdminUser]");
    assert_eq!(ty("nullableItems"), "[AdminUser]!");
    assert_eq!(ty("explicit"), "[AdminUser]!");
    assert_eq!(ty("explicitNonNull"), "[AdminUser!]!");
    assert_eq!(ty("one"), "AdminUser!");

    let response = execute(&built, "{ all { name } maybe { name } explicit { name } }");
    assert_eq!(
        root_field(&response, "all")
            .as_list()
            .map(<[gql::Value]>::len),
        Some(1)
    );
    assert_eq!(
        root_field(&response, "explicit")
            .as_list()
            .map(<[gql::Value]>::len),
        Some(2)
    );
}

#[test]
fn dynamic_refs_work_in_subscriptions() {
    let mut schema = Schema::<Ctx>::new();

    let admin = schema.object_ref::<User>("AdminUser");
    schema.implement(&admin, |o| {
        o.expose(expose!(User::name));
    });
    schema.query(|q| {
        q.field("ok", |_: NoArgs, _: &Ctx| true);
    });
    schema.subscription(|s| {
        s.field_ref::<NoArgs, User, _, _, _>(&admin, "adminChanged", |_: NoArgs, _: &Ctx| {
            futures::stream::empty::<gurafi::Result<User>>()
        });
    });

    let built = schema.build().expect("valid schema");
    let subscription = built.object_type("Subscription").expect("Subscription");
    assert_eq!(
        subscription
            .fields
            .get("adminChanged")
            .expect("adminChanged")
            .ty
            .to_string(),
        "AdminUser"
    );
}

#[test]
fn interface_fields_can_use_runtime_named_objects() {
    let mut schema = Schema::<Ctx>::new();

    let user_ref = schema.object_ref::<User>("AdminUser");
    let node = schema.interface_ref("Node");
    schema.implement_interface(&node, |i| {
        i.field::<String>("id");
        i.field_ref::<Vec<User>, _>(&user_ref, "nodes");
    });

    schema.implement(&user_ref, |o| {
        o.implements(&node);
        o.field("id", |u: &User, _: NoArgs, _: &Ctx| u.name.clone());
        o.expose(expose!(User::name));
        o.field_ref::<Vec<User>, NoArgs, _, _>(
            &user_ref,
            "nodes",
            |u: &User, _: NoArgs, _: &Ctx| {
                vec![User {
                    name: u.name.clone(),
                    age: u.age,
                }]
            },
        );
    });

    schema.query(|q| {
        q.field_ref(&user_ref, "node", |_: NoArgs, _: &Ctx| user("ada"))
            .as_abstract(&node);
    });

    let built = schema.build().expect("valid schema");
    let sdl = built.sdl();
    assert!(sdl.contains("nodes: [AdminUser]!"), "SDL was:\n{sdl}");

    let response = execute(&built, "{ node { nodes { name } } }");
    let nodes = root_field(&response, "node")
        .as_object()
        .and_then(|node| node.get("nodes"))
        .and_then(gql::Value::as_list)
        .expect("nodes");
    assert_eq!(nodes.len(), 1);
}

#[test]
fn relay_shaped_names_are_just_runtime_object_names() {
    let mut schema = Schema::<Ctx>::new();

    let user_ref = schema.object_ref::<User>("RelayUser");
    schema.implement(&user_ref, |o| {
        o.expose(expose!(User::name));
    });

    let connection = schema.object_ref::<UserPostsConnection>("UserPostsConnection");
    schema.implement(&connection, |o| {
        o.field(
            "totalCount",
            |_: &UserPostsConnection, _: NoArgs, _: &Ctx| 1i32,
        );
        o.field_ref::<Vec<User>, NoArgs, _, _>(
            &user_ref,
            "nodes",
            |_: &UserPostsConnection, _: NoArgs, _: &Ctx| vec![user("ada")],
        );
    });

    let payload = schema.object_ref::<CreateUserPayload>("CreateUserPayload");
    schema.implement(&payload, |o| {
        o.field_ref(
            &user_ref,
            "user",
            |_: &CreateUserPayload, _: NoArgs, _: &Ctx| user("ada"),
        );
        o.field("ok", |_: &CreateUserPayload, _: NoArgs, _: &Ctx| true);
    });

    schema.query(|q| {
        q.field_ref(&connection, "userPosts", |_: NoArgs, _: &Ctx| {
            UserPostsConnection
        });
        q.field_ref(&payload, "createUser", |_: NoArgs, _: &Ctx| {
            CreateUserPayload
        });
    });

    let built = schema.build().expect("valid schema");
    let sdl = built.sdl();
    assert!(sdl.contains("type UserPostsConnection"), "SDL was:\n{sdl}");
    assert!(sdl.contains("nodes: [RelayUser]!"), "SDL was:\n{sdl}");
    assert!(sdl.contains("type CreateUserPayload"), "SDL was:\n{sdl}");

    let response = execute(
        &built,
        "{ userPosts { totalCount nodes { name } } createUser { ok user { name } } }",
    );
    let payload = root_field(&response, "createUser")
        .as_object()
        .expect("payload");
    assert!(matches!(payload.get("ok"), Some(gql::Value::Boolean(true))));
    let connection = root_field(&response, "userPosts")
        .as_object()
        .expect("connection");
    assert!(matches!(
        connection.get("totalCount"),
        Some(gql::Value::Int(1))
    ));
}
