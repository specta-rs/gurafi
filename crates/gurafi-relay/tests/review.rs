//! Independent review probes for the Relay wave.

#![allow(dead_code)]

use std::sync::{
    Arc, Mutex,
    atomic::{AtomicUsize, Ordering},
};

use futures::executor::block_on;
use gurafi::{FieldMeta, NoArgs, Object, Plugin, RequestCache, Schema, expose, gql};
use gurafi_relay::{
    ClientMutationId, Connection, ConnectionArgs, Cursor, GlobalId, MutationConfig, Relay,
};

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

fn execute(schema: &gql::Schema, query: &str, ctx: Ctx) -> gql::Response {
    block_on(gql::execute(
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
        .unwrap_or_else(|| panic!("missing data: {response:?}"))
}

// ---------------------------------------------------------------------------------------------
// Connection pagination / PageInfo conformance (Relay connection spec section 5.1)
// ---------------------------------------------------------------------------------------------

fn items() -> Vec<Post> {
    vec![
        Post {
            id: "p1".into(),
            title: "First".into(),
        },
        Post {
            id: "p2".into(),
            title: "Second".into(),
        },
        Post {
            id: "p3".into(),
            title: "Third".into(),
        },
    ]
}

fn page(args: &ConnectionArgs) -> (Vec<String>, bool, bool) {
    let posts = items();
    let connection = Connection::from_slice(&posts, args, |post: &Post| post.id.clone())
        .expect("valid pagination");
    (
        connection
            .edges
            .iter()
            .map(|edge| edge.node.id.clone())
            .collect(),
        connection.page_info.has_next_page,
        connection.page_info.has_previous_page,
    )
}

fn args(
    first: Option<i32>,
    after: Option<&str>,
    last: Option<i32>,
    before: Option<&str>,
) -> ConnectionArgs {
    ConnectionArgs {
        first,
        after: after.map(Cursor::encode),
        last,
        before: before.map(Cursor::encode),
    }
}

#[test]
fn before_the_last_edge_has_no_next_page() {
    // `before` names the last item, so no element follows the filtered edges. Relay's
    // HasNextPage algorithm permits `true` from the `before` branch only when elements exist
    // after `before`.
    let (edges, has_next, _) = page(&args(None, None, None, Some("p3")));
    assert_eq!(edges, vec!["p1", "p2"]);
    assert!(
        !has_next,
        "hasNextPage must be false when `before` is the final edge"
    );
}

#[test]
fn first_with_before_limits_page_info_to_the_filtered_window() {
    // Filtered edges are [p1, p2]; `first: 2` returns all of them, so no edge follows the
    // returned page. Relay's HasNextPage algorithm uses the filtered edge count for `first`.
    let (edges, has_next, _) = page(&args(Some(2), None, None, Some("p3")));
    assert_eq!(edges, vec!["p1", "p2"]);
    assert!(
        !has_next,
        "hasNextPage must be computed from the edges left after `before`"
    );

    let (edges, has_next, _) = page(&args(Some(99), None, None, Some("p2")));
    assert_eq!(edges, vec!["p1"]);
    assert!(!has_next, "first consumed the whole filtered window");
}

#[test]
fn after_the_first_edge_has_no_previous_page() {
    // `after` is the first edge, so nothing exists prior to the returned page.
    let (edges, _, has_previous) = page(&args(None, Some("p1"), None, None));
    assert_eq!(edges, vec!["p2", "p3"]);
    assert!(
        !has_previous,
        "hasPreviousPage must be false when `after` is the first edge"
    );

    let (edges, _, has_previous) = page(&args(Some(1), Some("p1"), None, None));
    assert_eq!(edges, vec!["p2"]);
    assert!(!has_previous);
}

#[test]
fn empty_filtered_windows_have_no_page_flags() {
    // `before` the first edge filters everything out.
    let (edges, has_next, has_previous) = page(&args(Some(1), None, None, Some("p1")));
    assert!(edges.is_empty());
    assert!(!has_next, "no edges remain, so none can follow");
    assert!(!has_previous);

    // `after` the last edge filters everything out.
    let (edges, has_next, has_previous) = page(&args(None, Some("p3"), Some(1), None));
    assert!(edges.is_empty());
    assert!(!has_next);
    assert!(!has_previous, "an empty window cannot have a previous page");
}

#[test]
fn from_slice_by_index_has_the_same_page_info_rules() {
    let posts = items();
    let connection = Connection::from_slice_by_index(&posts, &args(None, None, None, Some("2")))
        .expect("valid pagination");
    assert_eq!(connection.edges.len(), 2);
    assert!(
        !connection.page_info.has_next_page,
        "before the final edge, no edge can follow"
    );
}

// ---------------------------------------------------------------------------------------------
// GlobalId
// ---------------------------------------------------------------------------------------------

#[test]
fn global_id_round_trips_colons_unicode_and_empty_local_ids() {
    for local in ["42", "a:b:c", "☃", ""] {
        let id = GlobalId::<User>::new(local);
        let decoded = GlobalId::<User>::decode(&id.encode()).expect("round trip");
        assert_eq!(decoded.as_local(), local);
    }
}

#[test]
fn global_id_is_strict_about_base64_and_typename() {
    assert!(GlobalId::<User>::decode("VXNlcjo0Mg").is_err()); // missing padding
    assert!(GlobalId::<User>::decode("VXNlcjo0Mg===").is_err());
    assert!(GlobalId::<User>::decode("!!!").is_err());
    assert!(GlobalId::<User>::decode(&base64(":42")).is_err()); // empty typename
    assert!(GlobalId::<User>::decode(&base64("Post:42")).is_err()); // wrong type
    assert!(GlobalId::<Post>::decode(&base64("Post:42")).is_ok());
}

fn base64(value: &str) -> String {
    // The crate's encoder is private; this small helper keeps the test independent.
    const ALPHABET: &[u8; 64] = b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789+/";
    let bytes = value.as_bytes();
    let mut out = String::new();
    for chunk in bytes.chunks(3) {
        let a = chunk[0];
        let b = chunk.get(1).copied().unwrap_or(0);
        let c = chunk.get(2).copied().unwrap_or(0);
        out.push(ALPHABET[(a >> 2) as usize] as char);
        out.push(ALPHABET[(((a & 3) << 4) | (b >> 4)) as usize] as char);
        out.push(if chunk.len() > 1 {
            ALPHABET[(((b & 15) << 2) | (c >> 6)) as usize] as char
        } else {
            '='
        });
        out.push(if chunk.len() > 2 {
            ALPHABET[(c & 63) as usize] as char
        } else {
            '='
        });
    }
    out
}

// ---------------------------------------------------------------------------------------------
// Node / loaders / cache
// ---------------------------------------------------------------------------------------------

struct Ctx {
    cache: RequestCache,
    users: Arc<Mutex<Vec<User>>>,
    posts: Arc<Mutex<Vec<Post>>>,
    user_calls: Arc<AtomicUsize>,
    post_calls: Arc<AtomicUsize>,
    user_batch_sizes: Arc<Mutex<Vec<usize>>>,
    fail_users: bool,
}

impl Ctx {
    fn new() -> Self {
        Self {
            cache: RequestCache::new(),
            users: Arc::new(Mutex::new(vec![
                User {
                    id: "1".into(),
                    name: "Ada".into(),
                },
                User {
                    id: "2".into(),
                    name: "Grace".into(),
                },
            ])),
            posts: Arc::new(Mutex::new(vec![Post {
                id: "p1".into(),
                title: "Hello".into(),
            }])),
            user_calls: Arc::default(),
            post_calls: Arc::default(),
            user_batch_sizes: Arc::default(),
            fail_users: false,
        }
    }
}

fn node_schema() -> gql::Schema {
    let mut schema = Schema::<Ctx>::new();
    let mut relay = Relay::<Ctx>::new().with_request_cache(|ctx: &Ctx| ctx.cache.clone());
    relay
        .node::<User>(&mut schema, |n| {
            n.id(|user: &User| user.id.clone());
            n.load_many_result(|ids, ctx| {
                Box::pin(async move {
                    ctx.user_calls.fetch_add(1, Ordering::SeqCst);
                    ctx.user_batch_sizes.lock().unwrap().push(ids.len());
                    if ctx.fail_users {
                        return Err(gql::GraphQLError::new("user backend down"));
                    }
                    let users = ctx.users.lock().unwrap();
                    Ok(ids
                        .into_iter()
                        .map(|id| users.iter().find(|user| user.id == id).cloned())
                        .collect())
                })
            });
        })
        .unwrap();
    relay
        .node::<Post>(&mut schema, |n| {
            n.id(|post: &Post| post.id.clone());
            n.load_one(|id, ctx| {
                Box::pin(async move {
                    ctx.post_calls.fetch_add(1, Ordering::SeqCst);
                    ctx.posts
                        .lock()
                        .unwrap()
                        .iter()
                        .find(|post| post.id == id)
                        .cloned()
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
        q.field("ok", |_: NoArgs, _: &Ctx| true);
    });
    schema.build().expect("valid schema")
}

#[test]
fn nodes_dedups_and_batches_one_loader_call_per_type() {
    let schema = node_schema();
    let ctx = Ctx::new();
    let user_calls = Arc::clone(&ctx.user_calls);
    let user_batch_sizes = Arc::clone(&ctx.user_batch_sizes);
    let user_ids = (0..10)
        .map(|_| GlobalId::<User>::new("1").encode())
        .chain(std::iter::once(GlobalId::<User>::new("2").encode()))
        .map(|id| format!("\"{id}\""))
        .collect::<Vec<_>>()
        .join(",");
    let response = execute(
        &schema,
        &format!("{{ nodes(ids: [{user_ids}]) {{ __typename }} }}"),
        ctx,
    );

    assert!(!response.has_errors(), "{:?}", response.errors);
    assert_eq!(
        data(&response)
            .get("nodes")
            .and_then(gql::Value::as_list)
            .map(<[gql::Value]>::len),
        Some(11)
    );
    assert_eq!(
        user_calls.load(Ordering::SeqCst),
        1,
        "one loader call per node type"
    );
    assert_eq!(
        user_batch_sizes.lock().unwrap().clone(),
        vec![2],
        "the eleven ids reduce to two unique local ids"
    );
}

#[test]
fn one_bad_id_in_nodes_only_nulls_that_item() {
    let schema = node_schema();
    let good = GlobalId::<User>::new("1").encode();
    let unknown = base64("Team:1");
    let response = execute(
        &schema,
        &format!(r#"{{ nodes(ids: ["{good}", "{unknown}"]) {{ __typename }} }}"#),
        Ctx::new(),
    );

    assert!(!response.has_errors(), "{response:?}");
    let nodes = data(&response)
        .get("nodes")
        .and_then(gql::Value::as_list)
        .expect("nodes");
    assert_eq!(nodes.len(), 2);
    assert_eq!(
        nodes[0]
            .as_object()
            .and_then(|node| node.get("__typename"))
            .and_then(gql::Value::as_str),
        Some("User")
    );
    assert!(
        nodes[1].is_null(),
        "the unusable item is null: {response:?}"
    );
}

#[test]
fn local_ids_are_rejected_by_default() {
    let schema = node_schema();
    let response = execute(
        &schema,
        r#"{ node(id: "1") { __typename ... on User { name } } }"#,
        Ctx::new(),
    );
    assert!(response.has_errors(), "{response:?}");
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
fn node_loader_errors_are_negative_cached() {
    let schema = node_schema();
    let mut ctx = Ctx::new();
    ctx.fail_users = true;
    let user_calls = Arc::clone(&ctx.user_calls);
    let id = GlobalId::<User>::new("1").encode();
    let response = execute(
        &schema,
        &format!(r#"{{ first: node(id: "{id}") {{ id }} second: node(id: "{id}") {{ id }} }}"#),
        ctx,
    );
    assert!(response.has_errors(), "{response:?}");
    assert_eq!(
        user_calls.load(Ordering::SeqCst),
        1,
        "a failed node load is cached for the request, so sibling fields do not retry it"
    );
}

#[test]
fn prepared_queries_execute_against_relay_schemas() {
    let schema = node_schema();
    let id = GlobalId::<User>::new("1").encode();
    let source = format!(
        r#"query One($skip: Boolean!) {{ node(id: "{id}") @skip(if: $skip) {{ ... on User {{ name }} }} }}"#
    );
    let prepared = gql::prepared::PreparedQuery::new(&schema, "one", &source, None)
        .expect("prepared query validates");

    let response = block_on(prepared.execute(
        Some(&gql::Object::from_fields([(
            "skip",
            gql::Value::Boolean(false),
        )])),
        &gql::ExecutionOptions::with_context(Ctx::new()),
    ));
    assert!(!response.has_errors(), "{:?}", response.errors);
    let name = data(&response)
        .get("node")
        .and_then(gql::Value::as_object)
        .and_then(|node| node.get("name"))
        .and_then(gql::Value::as_str);
    assert_eq!(name, Some("Ada"));
}

// ---------------------------------------------------------------------------------------------
// Plugins on Relay-generated schema
// ---------------------------------------------------------------------------------------------

#[derive(Clone, Default)]
struct Recorder {
    fields: Arc<Mutex<Vec<String>>>,
    wraps: Arc<Mutex<Vec<String>>>,
    entered: Arc<Mutex<Vec<String>>>,
}

impl Plugin<Ctx> for Recorder {
    fn field(&self, meta: &FieldMeta<'_>, _field: &mut gql::Field) {
        self.fields.lock().unwrap().push(meta.path());
    }
    fn wrap_resolve(&self, meta: &FieldMeta<'_>, next: gql::ResolveFn) -> gql::ResolveFn {
        let path = meta.path();
        self.wraps.lock().unwrap().push(path.clone());
        let entered = Arc::clone(&self.entered);
        Arc::new(move |context| {
            entered.lock().unwrap().push(path.clone());
            next(context)
        })
    }
}

#[test]
fn plugins_visit_every_relay_generated_type_and_wrap_root_fields_once() {
    let recorder = Recorder::default();
    let mut schema = Schema::<Ctx>::new();
    schema.plugin(recorder.clone());
    let mut relay = Relay::<Ctx>::new();
    relay
        .node::<User>(&mut schema, |n| {
            n.id(|user: &User| user.id.clone());
            n.load_one(|id, _: &Ctx| {
                Box::pin(async move {
                    Some(User {
                        id,
                        name: "x".into(),
                    })
                })
            });
        })
        .unwrap();
    relay
        .object_connection::<User, Post, _>(
            &mut schema,
            "posts",
            "UserPostsConnection",
            |_user: &User, args: ConnectionArgs, _ctx: &Ctx| {
                Connection::from_slice(&[], &args, |post: &Post| post.id.clone())
            },
        )
        .unwrap();
    schema.object::<User>(|o| {
        o.expose(expose!(User::name));
    });
    schema.object::<Post>(|o| {
        o.expose(expose!(Post::title));
    });
    schema.query(|q| {
        q.field("ok", |_: NoArgs, _: &Ctx| true);
    });
    schema.build().expect("valid schema");

    let fields = recorder.fields.lock().unwrap().clone();
    for expected in [
        "Query.node",
        "Query.nodes",
        "PageInfo.hasNextPage",
        "UserPostsConnection.edges",
        "UserPostsConnectionEdge.cursor",
        "UserPostsConnectionEdge.node",
    ] {
        assert!(
            fields.iter().any(|path| path == expected),
            "plugin did not visit {expected}: {fields:?}"
        );
    }
    assert_eq!(
        recorder
            .wraps
            .lock()
            .unwrap()
            .iter()
            .filter(|path| *path == "Query.node")
            .count(),
        1
    );
}

// ---------------------------------------------------------------------------------------------
// Mutations
// ---------------------------------------------------------------------------------------------

#[derive(gurafi::Input)]
#[graphql(name = "CreatePostInput")]
struct CreatePostInput {
    title: String,
}

fn mutation_schema(config: MutationConfig) -> gql::Schema {
    let mut schema = Schema::<Ctx>::new();
    let mut relay = Relay::<Ctx>::new();
    schema.object::<Post>(|o| {
        o.expose(expose!(Post::title));
    });
    relay
        .mutation_with::<Post, CreatePostInput, _>(
            &mut schema,
            "createPost",
            config,
            |input, _ctx| {
                Ok(Post {
                    id: "new".into(),
                    title: input.title,
                })
            },
        )
        .unwrap();
    schema.query(|q| {
        q.field("ok", |_: NoArgs, _: &Ctx| true);
    });
    schema.build().expect("valid schema")
}

#[test]
fn required_client_mutation_id_rejects_null() {
    let schema =
        mutation_schema(MutationConfig::new().client_mutation_id(ClientMutationId::Required));
    let response = execute(
        &schema,
        r#"mutation { createPost(input: { title: "hi" }, clientMutationId: null) { clientMutationId } }"#,
        Ctx::new(),
    );
    assert!(
        response.has_errors(),
        "a required clientMutationId must reject null: {response:?}"
    );
}

#[test]
fn optional_mutation_result_is_not_declared_non_null() {
    struct OptionalCtx;
    let mut schema = Schema::<OptionalCtx>::new();
    schema.object::<Post>(|o| {
        o.expose(expose!(Post::title));
    });
    let mut relay = Relay::<OptionalCtx>::new();
    relay
        .mutation_with::<Option<Post>, CreatePostInput, _>(
            &mut schema,
            "maybeCreatePost",
            MutationConfig::new().client_mutation_id(ClientMutationId::Omit),
            |_input, _ctx| Ok(None),
        )
        .unwrap();
    schema.query(|q| {
        q.field("ok", |_: NoArgs, _: &OptionalCtx| true);
    });
    let built = schema.build().expect("valid schema");

    let sdl = built.sdl();
    assert!(
        sdl.contains("type MaybeCreatePostPayload") && sdl.contains("result: Post\n"),
        "an optional result must be declared nullable in SDL:\n{sdl}"
    );

    let response = block_on(gql::execute(
        &built,
        gql::Request::new(
            r#"mutation { maybeCreatePost(input: { title: "hi" }) { result { title } } }"#,
        ),
        &gql::ExecutionOptions::with_context(OptionalCtx),
    ));
    assert!(
        !response.has_errors(),
        "a null optional result must not violate a non-null payload field: {response:?}"
    );
}

#[test]
fn mutation_payload_collision_with_a_user_object_is_a_build_error() {
    let mut schema = Schema::<Ctx>::new();
    schema.object::<Post>(|o| {
        o.expose(expose!(Post::title));
    });
    let clash = schema.object_ref::<User>("CreatePostPayload");
    schema.implement(&clash, |o| {
        o.expose(expose!(User::name));
    });
    let mut relay = Relay::<Ctx>::new();
    relay
        .mutation::<Post, CreatePostInput, _>(&mut schema, "createPost", |input, _ctx| {
            Ok(Post {
                id: "new".into(),
                title: input.title,
            })
        })
        .unwrap();
    schema.query(|q| {
        q.field("ok", |_: NoArgs, _: &Ctx| true);
    });

    let error = schema.build().expect_err("payload name collision");
    assert!(
        error.to_string().contains("CreatePostPayload"),
        "unexpected error: {error}"
    );
}

#[test]
fn connection_name_collision_with_a_user_object_is_a_build_error() {
    let mut schema = Schema::<Ctx>::new();
    schema.object::<Post>(|o| {
        o.expose(expose!(Post::title));
    });
    let clash = schema.object_ref::<User>("UserPostsConnection");
    schema.implement(&clash, |o| {
        o.expose(expose!(User::name));
    });
    let mut relay = Relay::<Ctx>::new();
    relay
        .connection::<Post>(&mut schema, "UserPostsConnection")
        .unwrap();
    schema.query(|q| {
        q.field("ok", |_: NoArgs, _: &Ctx| true);
    });

    let error = schema.build().expect_err("connection name collision");
    assert!(
        error.to_string().contains("UserPostsConnection"),
        "unexpected error: {error}"
    );
}

#[test]
fn connection_for_a_runtime_named_node_type_reports_the_model_name() {
    let mut schema = Schema::<Ctx>::new();
    let admin = schema.object_ref::<Post>("AdminPost");
    schema.implement(&admin, |o| {
        o.expose(expose!(Post::title));
    });
    schema.query(|q| {
        q.field("ok", |_: NoArgs, _: &Ctx| true);
    });
    let mut relay = Relay::<Ctx>::new();
    relay
        .connection::<Post>(&mut schema, "AdminPostsConnection")
        .unwrap();

    let error = schema
        .build()
        .expect_err("the edge node type still uses the derived model name");
    assert!(
        error.to_string().contains("Post") || error.to_string().contains("AdminPost"),
        "unexpected error: {error}"
    );
}

#[test]
fn allow_local_ids_after_registering_a_node_still_rejects_bare_local_ids() {
    // The normal registration order is: register nodes, then lock down ID handling. The root
    // field is installed by the first `node` call, so a later configuration call must take
    // effect; otherwise a security setting silently does nothing.
    let mut schema = Schema::<Ctx>::new();
    let mut relay = Relay::<Ctx>::new().allow_local_ids(true);
    relay
        .node::<User>(&mut schema, |n| {
            n.id(|user: &User| user.id.clone());
            n.load_one(|id, _: &Ctx| {
                Box::pin(async move {
                    Some(User {
                        id,
                        name: "x".into(),
                    })
                })
            });
        })
        .unwrap();
    let relay = relay.allow_local_ids(false);
    let _ = &relay;
    schema.object::<User>(|o| {
        o.expose(expose!(User::name));
    });
    schema.query(|q| {
        q.field("ok", |_: NoArgs, _: &Ctx| true);
    });
    let built = schema.build().expect("valid schema");

    let response = execute(&built, r#"{ node(id: "1") { __typename } }"#, Ctx::new());
    assert!(
        response.has_errors(),
        "local IDs must be rejected after `allow_local_ids(false)`: {response:?}"
    );
}

#[test]
fn bare_local_ids_are_rejected() {
    let schema = node_schema();
    // `p1` is Post's local id. A User global ID is `Base64("User:p1")`; a bare value is not a
    // valid global ID and must not be tried against the registered loaders.
    assert!(GlobalId::<User>::decode(&base64("User:p1")).is_ok());
    assert!(
        GlobalId::<User>::decode("p1").is_err(),
        "a bare local id is not a valid User global id"
    );

    let response = execute(
        &schema,
        r#"{ node(id: "p1") { __typename ... on Post { title } } }"#,
        Ctx::new(),
    );
    assert!(response.has_errors(), "{response:?}");
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
fn nodes_with_only_load_one_bounds_fan_out() {
    struct LimitCtx {
        in_flight: AtomicUsize,
        peak: Arc<AtomicUsize>,
    }
    struct YieldOnce(bool);
    impl std::future::Future for YieldOnce {
        type Output = ();
        fn poll(
            mut self: std::pin::Pin<&mut Self>,
            context: &mut std::task::Context<'_>,
        ) -> std::task::Poll<()> {
            if self.0 {
                std::task::Poll::Ready(())
            } else {
                self.0 = true;
                context.waker().wake_by_ref();
                std::task::Poll::Pending
            }
        }
    }

    let mut schema = Schema::<LimitCtx>::new();
    let mut relay = Relay::<LimitCtx>::new();
    relay
        .node::<User>(&mut schema, |n| {
            n.id(|user: &User| user.id.clone());
            n.load_one(|id, ctx: &LimitCtx| {
                Box::pin(async move {
                    let now = ctx.in_flight.fetch_add(1, Ordering::SeqCst) + 1;
                    ctx.peak.fetch_max(now, Ordering::SeqCst);
                    YieldOnce(false).await;
                    ctx.in_flight.fetch_sub(1, Ordering::SeqCst);
                    Some(User {
                        id,
                        name: "x".into(),
                    })
                })
            });
        })
        .unwrap();
    schema.object::<User>(|o| {
        o.expose(expose!(User::name));
    });
    schema.query(|q| {
        q.field("ok", |_: NoArgs, _: &LimitCtx| true);
    });
    let built = schema.build().expect("valid schema");

    let ids = (0..50)
        .map(|index| format!("\"{}\"", GlobalId::<User>::new(index.to_string()).encode()))
        .collect::<Vec<_>>()
        .join(",");
    let peak = Arc::new(AtomicUsize::new(0));
    let ctx = LimitCtx {
        in_flight: AtomicUsize::new(0),
        peak: Arc::clone(&peak),
    };
    let response = block_on(gql::execute(
        &built,
        gql::Request::new(&format!("{{ nodes(ids: [{ids}]) {{ __typename }} }}")),
        &gql::ExecutionOptions::with_context(ctx),
    ));
    assert!(!response.has_errors(), "{:?}", response.errors);

    // The fallback runs bounded `load_one` calls, so a client-sized id list cannot create an
    // unbounded burst of backend work.
    let peak = peak.load(Ordering::SeqCst);
    assert!(
        peak <= 16,
        "the fallback must bound concurrent `load_one` calls, saw {peak}"
    );
    assert!(peak > 1, "loads should still overlap: {peak}");
}
