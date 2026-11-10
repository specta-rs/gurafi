//! Node interface registration, loaders, dispatch and request caching.

#![allow(dead_code)]

use std::sync::{
    Arc,
    atomic::{AtomicUsize, Ordering},
};

use futures::executor::block_on;
use gurafi::{NoArgs, Object, RequestCache, Schema, Union, expose, gql};
use gurafi_relay::{GlobalId, Relay};

#[derive(Clone, Object)]
struct User {
    id: String,
    name: String,
}

#[derive(Clone, Object)]
struct Post {
    id: String,
    title: String,
}

#[derive(Clone, Object)]
struct Legacy {
    id: String,
}

#[derive(Union)]
enum SearchResult {
    User(User),
    Post(Post),
}

struct Ctx {
    cache: RequestCache,
    users: Vec<User>,
    posts: Vec<Post>,
    deny: bool,
    user_load_many: Arc<AtomicUsize>,
    user_ids: Arc<std::sync::Mutex<Vec<Vec<String>>>>,
    post_load_one: Arc<AtomicUsize>,
}

impl Ctx {
    fn new() -> Self {
        Self {
            cache: RequestCache::new(),
            users: vec![
                User {
                    id: "1".into(),
                    name: "Ada".into(),
                },
                User {
                    id: "2".into(),
                    name: "Grace".into(),
                },
            ],
            posts: vec![Post {
                id: "p1".into(),
                title: "Hello".into(),
            }],
            deny: false,
            user_load_many: Arc::new(AtomicUsize::new(0)),
            user_ids: Arc::new(std::sync::Mutex::new(Vec::new())),
            post_load_one: Arc::new(AtomicUsize::new(0)),
        }
    }
}

fn execute(schema: &gql::Schema, query: &str, ctx: Ctx) -> gql::Response {
    execute_with(schema, query, ctx, gql::ExecutionOptions::with_context)
}

fn execute_with(
    schema: &gql::Schema,
    query: &str,
    ctx: Ctx,
    options: impl FnOnce(Ctx) -> gql::ExecutionOptions,
) -> gql::Response {
    block_on(gql::execute(
        schema,
        gql::Request::new(query),
        &options(ctx),
    ))
}

fn data(response: &gql::Response) -> &gql::Object {
    response
        .data
        .as_ref()
        .and_then(gql::Value::as_object)
        .unwrap_or_else(|| panic!("missing data: {:?}", response.errors))
}

fn node_schema() -> gql::Schema {
    node_schema_with_local_ids(false)
}

fn node_schema_with_local_ids(allow_local_ids: bool) -> gql::Schema {
    let mut schema = Schema::<Ctx>::new();
    let mut relay = Relay::<Ctx>::new()
        .with_request_cache(|ctx: &Ctx| ctx.cache.clone())
        .allow_local_ids(allow_local_ids);

    relay
        .node::<User>(&mut schema, |n| {
            n.id(|user: &User| user.id.clone());
            n.load_many(|ids, ctx| {
                Box::pin(async move {
                    ctx.user_load_many.fetch_add(1, Ordering::SeqCst);
                    ctx.user_ids.lock().unwrap().push(ids.clone());
                    ids.into_iter()
                        .map(|id| ctx.users.iter().find(|user| user.id == id).cloned())
                        .collect()
                })
            });
        })
        .unwrap();

    relay
        .node::<Post>(&mut schema, |n| {
            n.id(|post: &Post| post.id.clone());
            n.load_one(|id, ctx| {
                Box::pin(async move {
                    ctx.post_load_one.fetch_add(1, Ordering::SeqCst);
                    ctx.posts.iter().find(|post| post.id == id).cloned()
                })
            });
        })
        .unwrap();

    schema.object::<User>(|o| {
        o.expose(expose!(User::name));
    });
    schema.object::<Post>(|o| {
        o.expose(expose!(Post::title));
    });
    schema.query(|q| {
        q.field("user", |_: NoArgs, _: &Ctx| User {
            id: "1".into(),
            name: "Ada".into(),
        });
    });
    schema.build().expect("valid schema")
}

#[test]
fn node_and_nodes_dispatch_global_ids() {
    let schema = node_schema();
    let user_id = GlobalId::<User>::new("1").encode();
    let post_id = GlobalId::<Post>::new("p1").encode();
    let query = format!(
        r#"{{
            node(id: "{user_id}") {{ __typename id ... on User {{ name }} }}
            post: node(id: "{post_id}") {{ __typename ... on Post {{ title }} }}
            nodes(ids: ["{user_id}", "{post_id}"]) {{
                __typename
                ... on User {{ name }}
                ... on Post {{ title }}
            }}
        }}"#
    );
    let response = execute(&schema, &query, Ctx::new());
    assert!(!response.has_errors(), "{:?}", response.errors);
    let data = data(&response);

    let node = data.get("node").unwrap().as_object().unwrap();
    assert_eq!(
        node.get("__typename").and_then(gql::Value::as_str),
        Some("User")
    );
    assert_eq!(
        node.get("id").and_then(gql::Value::as_str),
        Some(user_id.as_str())
    );
    assert_eq!(node.get("name").and_then(gql::Value::as_str), Some("Ada"));

    let post = data.get("post").unwrap().as_object().unwrap();
    assert_eq!(
        post.get("__typename").and_then(gql::Value::as_str),
        Some("Post")
    );
    assert_eq!(
        post.get("title").and_then(gql::Value::as_str),
        Some("Hello")
    );

    let nodes = data.get("nodes").unwrap().as_list().unwrap();
    assert_eq!(nodes.len(), 2);
    assert_eq!(
        nodes[0]
            .as_object()
            .and_then(|node| node.get("name"))
            .and_then(gql::Value::as_str),
        Some("Ada")
    );
    assert_eq!(
        nodes[1]
            .as_object()
            .and_then(|node| node.get("title"))
            .and_then(gql::Value::as_str),
        Some("Hello")
    );

    let sdl = schema.sdl();
    assert!(sdl.contains("interface Node"), "SDL was:\n{sdl}");
    assert!(sdl.contains("): Node"), "SDL was:\n{sdl}");
    assert!(sdl.contains("ids: [ID!]!"), "SDL was:\n{sdl}");
    assert!(sdl.contains("): [Node]!"), "SDL was:\n{sdl}");
    assert!(sdl.contains("type User implements Node"), "SDL was:\n{sdl}");
}

#[test]
fn missing_nodes_are_null_and_local_ids_are_accepted() {
    let schema = node_schema_with_local_ids(true);

    let missing = execute(
        &schema,
        r#"{ node(id: "VXNlcjo5OTk=") { id } }"#, // User:999
        Ctx::new(),
    );
    assert!(!missing.has_errors(), "{:?}", missing.errors);
    assert!(data(&missing).get("node").unwrap().is_null());

    let local = execute(
        &schema,
        r#"{ node(id: "2") { __typename ... on User { name } } }"#,
        Ctx::new(),
    );
    assert!(!local.has_errors(), "{:?}", local.errors);
    let node = data(&local).get("node").unwrap().as_object().unwrap();
    assert_eq!(node.get("name").and_then(gql::Value::as_str), Some("Grace"));
}

#[test]
fn unknown_global_type_is_an_error() {
    let schema = node_schema();
    let unknown = "VGVhbTpwMQ=="; // Team:p1
    let response = execute(
        &schema,
        &format!(r#"{{ node(id: "{unknown}") {{ id }} }}"#),
        Ctx::new(),
    );
    assert!(response.has_errors());
    assert!(
        response
            .errors
            .iter()
            .any(|error| error.message().contains("unknown node type")),
        "{:?}",
        response.errors
    );
}

#[test]
fn request_cache_and_multi_dedup_avoid_duplicate_loads() {
    let schema = node_schema();
    let id = GlobalId::<User>::new("1").encode();
    let query = format!(
        r#"{{
            nodes(ids: ["{id}", "{id}"]) {{ ... on User {{ name }} }}
            second: node(id: "{id}") {{ ... on User {{ name }} }}
        }}"#
    );
    let ctx = Ctx::new();
    let user_calls = Arc::clone(&ctx.user_load_many);
    let requested = Arc::clone(&ctx.user_ids);
    let response = execute_with(&schema, &query, ctx, |ctx| {
        gql::ExecutionOptions::with_context(ctx).max_concurrency(1)
    });
    assert!(!response.has_errors(), "{:?}", response.errors);
    assert_eq!(user_calls.load(Ordering::SeqCst), 1);
    assert_eq!(*requested.lock().unwrap(), vec![vec!["1".to_owned()]]);
}

#[test]
fn object_authorization_is_not_bypassed() {
    let mut schema = Schema::<Ctx>::new();
    let mut relay = Relay::<Ctx>::new();
    relay
        .node::<User>(&mut schema, |n| {
            n.id(|user: &User| user.id.clone());
            n.load_one(|id, _ctx| {
                Box::pin(async move {
                    Some(User {
                        id,
                        name: "Ada".into(),
                    })
                })
            });
        })
        .unwrap();
    schema.object::<User>(|o| {
        o.auth(|ctx: &Ctx| !ctx.deny);
        o.expose(expose!(User::name));
    });
    let schema = schema.build().expect("valid schema");

    let id = GlobalId::<User>::new("1").encode();
    let query = format!(r#"{{ node(id: "{id}") {{ id ... on User {{ name }} }} }}"#);

    let allowed = execute(&schema, &query, Ctx::new());
    assert!(!allowed.has_errors(), "{:?}", allowed.errors);

    let mut denied = Ctx::new();
    denied.deny = true;
    let response = execute(&schema, &query, denied);
    assert!(response.has_errors(), "{:?}", response.data);
}

#[test]
fn duplicate_node_registration_is_rejected() {
    let mut schema = Schema::<Ctx>::new();
    let mut relay = Relay::<Ctx>::new();
    let register = |relay: &mut Relay<Ctx>, schema: &mut Schema<Ctx>| {
        relay.node::<User>(schema, |n| {
            n.id(|user: &User| user.id.clone());
            n.load_one(|id, _ctx| {
                Box::pin(async move {
                    Some(User {
                        id,
                        name: String::new(),
                    })
                })
            });
        })
    };
    register(&mut relay, &mut schema).unwrap();
    let error = register(&mut relay, &mut schema).unwrap_err();
    assert!(error.message().contains("already registered"));
}

#[test]
fn an_existing_id_field_is_preserved() {
    let mut schema = Schema::<Ctx>::new();
    let mut relay = Relay::<Ctx>::new().allow_local_ids(true);
    schema.object::<Legacy>(|o| {
        o.field("id", |legacy: &Legacy, _: NoArgs, _: &Ctx| {
            gurafi::ID::new(legacy.id.clone())
        });
    });
    relay
        .node::<Legacy>(&mut schema, |n| {
            n.id(|legacy: &Legacy| legacy.id.clone());
            n.load_one(|id, _ctx| Box::pin(async move { Some(Legacy { id }) }));
        })
        .unwrap();
    schema.query(|q| {
        q.field("legacy", |_: NoArgs, _: &Ctx| Legacy { id: "7".into() });
    });
    let schema = schema.build().expect("valid schema");

    let response = execute(
        &schema,
        r#"{ legacy { id } node(id: "Nw==") { id } }"#,
        Ctx::new(),
    );
    assert!(!response.has_errors(), "{:?}", response.errors);
    let legacy = data(&response).get("legacy").unwrap().as_object().unwrap();
    assert_eq!(legacy.get("id").and_then(gql::Value::as_str), Some("7"));
}

#[test]
fn local_ids_can_be_disabled() {
    let mut schema = Schema::<Ctx>::new();
    let mut relay = Relay::<Ctx>::new().allow_local_ids(false);
    relay
        .node::<User>(&mut schema, |n| {
            n.id(|user: &User| user.id.clone());
            n.load_one(|id, _ctx| {
                Box::pin(async move {
                    Some(User {
                        id,
                        name: "Ada".into(),
                    })
                })
            });
        })
        .unwrap();
    schema.object::<User>(|o| {
        o.expose(expose!(User::name));
    });
    let schema = schema.build().expect("valid schema");

    let response = execute(&schema, r#"{ node(id: "1") { id } }"#, Ctx::new());
    assert!(response.has_errors());
    assert!(
        response
            .errors
            .iter()
            .any(|error| error.message().contains("Invalid global ID")),
        "{:?}",
        response.errors
    );
}

#[test]
fn node_models_can_also_be_union_members() {
    let mut schema = Schema::<Ctx>::new();
    let mut relay = Relay::<Ctx>::new();
    relay
        .node::<User>(&mut schema, |n| {
            n.id(|user: &User| user.id.clone());
            n.load_one(|id, _ctx| {
                Box::pin(async move {
                    Some(User {
                        id,
                        name: "Ada".into(),
                    })
                })
            });
        })
        .unwrap();
    relay
        .node::<Post>(&mut schema, |n| {
            n.id(|post: &Post| post.id.clone());
            n.load_one(|id, _ctx| {
                Box::pin(async move {
                    Some(Post {
                        id,
                        title: "Hello".into(),
                    })
                })
            });
        })
        .unwrap();
    schema.object::<User>(|o| {
        o.expose(expose!(User::name));
    });
    schema.object::<Post>(|o| {
        o.expose(expose!(Post::title));
    });
    schema.union("SearchResult", |u| {
        u.member::<User>();
        u.member::<Post>();
    });
    schema.query(|q| {
        q.field("search", |_: NoArgs, _: &Ctx| {
            SearchResult::User(User {
                id: "1".into(),
                name: "Ada".into(),
            })
        });
    });
    let schema = schema.build().expect("valid schema");

    let response = execute(
        &schema,
        r#"{ search { __typename ... on User { name } ... on Post { title } } }"#,
        Ctx::new(),
    );
    assert!(!response.has_errors(), "{:?}", response.errors);
    let search = data(&response).get("search").unwrap().as_object().unwrap();
    assert_eq!(
        search.get("__typename").and_then(gql::Value::as_str),
        Some("User")
    );
}
