//! Timing probes for the Relay hot paths.
//!
//! These are `#[ignore]`d so a normal `cargo test` run does not spend time on them:
//!
//! ```sh
//! cargo test -p gurafi-relay --test perf_probe -- --ignored --nocapture --test-threads=1
//! ```

#![allow(dead_code)]

use std::{
    sync::Arc,
    time::{Duration, Instant},
};

use futures::executor::block_on;
use gurafi::{Object, RequestCache, Schema, expose, gql};
use gurafi_relay::{Connection, ConnectionArgs, GlobalId, Relay};

macro_rules! node_models {
    ($($name:ident),* $(,)?) => {
        $(
            #[derive(Clone, Object)]
            struct $name {
                id: String,
            }
        )*
    };
}

node_models!(NodeA, NodeB, NodeC, NodeD, NodeE, NodeF, NodeG, NodeH,);

struct Ctx {
    cache: RequestCache,
}

fn node_schema() -> gql::Schema {
    let mut schema = Schema::<Ctx>::new();
    let mut relay = Relay::<Ctx>::new().with_request_cache(|ctx: &Ctx| ctx.cache.clone());

    macro_rules! register {
        ($($name:ident),* $(,)?) => {
            $(
                relay
                    .node::<$name>(&mut schema, |n| {
                        n.id(|node: &$name| node.id.clone());
                        n.load_many(|ids, _ctx| {
                            Box::pin(async move {
                                ids.into_iter()
                                    .map(|id| Some($name { id }))
                                    .collect::<Vec<_>>()
                            })
                        });
                    })
                    .unwrap();
            )*
        };
    }

    register!(NodeA, NodeB, NodeC, NodeD, NodeE, NodeF, NodeG, NodeH);

    schema.query(|q| {
        q.field("ok", |_: gurafi::NoArgs, _: &Ctx| true);
    });
    schema.build().expect("valid schema")
}

fn report(label: &str, iterations: usize, elapsed: Duration, per: usize) {
    let nanos = elapsed.as_nanos() as f64 / iterations as f64;
    let per_item = elapsed.as_nanos() as f64 / (iterations * per.max(1)) as f64;
    println!(
        "{label}: {iterations} iters in {elapsed:?} => {nanos:.0} ns/iter, {per_item:.1} ns/item"
    );
}

fn time<F: FnMut()>(iterations: usize, mut f: F) -> Duration {
    let start = Instant::now();
    for _ in 0..iterations {
        f();
    }
    start.elapsed()
}

#[test]
#[ignore = "timing probe"]
fn probe_nodes_with_many_ids() {
    let schema = node_schema();
    let ids: Vec<String> = (0..500)
        .map(|index| GlobalId::<NodeA>::new(index.to_string()).encode())
        .collect();
    let literal = ids
        .iter()
        .map(|id| format!("\"{id}\""))
        .collect::<Vec<_>>()
        .join(", ");
    let query = format!("{{ nodes(ids: [{literal}]) {{ __typename id }} }}");
    let iterations = 200;

    // Warm up.
    for _ in 0..5 {
        block_on(gql::execute(
            &schema,
            gql::Request::new(query.as_str()),
            &gql::ExecutionOptions::with_context(Ctx {
                cache: RequestCache::new(),
            }),
        ));
    }

    let elapsed = time(iterations, || {
        block_on(gql::execute(
            &schema,
            gql::Request::new(query.as_str()),
            &gql::ExecutionOptions::with_context(Ctx {
                cache: RequestCache::new(),
            }),
        ));
    });
    report("nodes(500 ids)", iterations, elapsed, ids.len());
}

#[test]
#[ignore = "timing probe"]
fn probe_node_single() {
    let schema = node_schema();
    let id = GlobalId::<NodeA>::new("42").encode();
    let query = format!("{{ node(id: \"{id}\") {{ id }} }}");
    let iterations = 20_000;

    for _ in 0..100 {
        block_on(gql::execute(
            &schema,
            gql::Request::new(query.as_str()),
            &gql::ExecutionOptions::with_context(Ctx {
                cache: RequestCache::new(),
            }),
        ));
    }

    let elapsed = time(iterations, || {
        block_on(gql::execute(
            &schema,
            gql::Request::new(query.as_str()),
            &gql::ExecutionOptions::with_context(Ctx {
                cache: RequestCache::new(),
            }),
        ));
    });
    report("node(id)", iterations, elapsed, 1);
}

#[test]
#[ignore = "timing probe"]
fn probe_nodes_with_duplicate_ids() {
    let schema = node_schema();
    let ids: Vec<String> = (0..500)
        .map(|index| GlobalId::<NodeA>::new(index.to_string()).encode())
        .collect();
    // Every ID appears twice: the loader must see each one once, and the per-request cache
    // answers the second occurrence.
    let literal = ids
        .iter()
        .chain(ids.iter())
        .map(|id| format!("\"{id}\""))
        .collect::<Vec<_>>()
        .join(", ");
    let query = format!("{{ nodes(ids: [{literal}]) {{ __typename id }} }}");
    let iterations = 200;

    for _ in 0..5 {
        block_on(gql::execute(
            &schema,
            gql::Request::new(query.as_str()),
            &gql::ExecutionOptions::with_context(Ctx {
                cache: RequestCache::new(),
            }),
        ));
    }

    let elapsed = time(iterations, || {
        block_on(gql::execute(
            &schema,
            gql::Request::new(query.as_str()),
            &gql::ExecutionOptions::with_context(Ctx {
                cache: RequestCache::new(),
            }),
        ));
    });
    report(
        &format!("nodes({} ids, half duplicated)", ids.len() * 2),
        iterations,
        elapsed,
        ids.len() * 2,
    );
}

#[derive(Clone, Object)]
struct Post {
    id: String,
}

#[test]
#[ignore = "timing probe"]
fn probe_connection_page() {
    let posts: Arc<Vec<Post>> = Arc::new(
        (0..2_000)
            .map(|index| Post {
                id: format!("post-{index}"),
            })
            .collect(),
    );
    let args = ConnectionArgs {
        first: Some(10),
        ..ConnectionArgs::default()
    };
    let iterations = 5_000;
    let cursor = |post: &Post| post.id.clone();

    for _ in 0..10 {
        let connection = Connection::from_slice(&posts, &args, cursor).unwrap();
        assert_eq!(connection.edges.len(), 10);
    }

    let elapsed = time(iterations, || {
        let connection = Connection::from_slice(&posts, &args, cursor).unwrap();
        assert_eq!(connection.edges.len(), 10);
    });
    report("connection first:10 of 2000", iterations, elapsed, 10);
}

/// Build a schema with a Relay node, connection and error union, then build it twice.
#[test]
#[ignore = "timing probe"]
fn probe_build_clone() {
    use gurafi::ErrorUnion;

    #[derive(Clone, Object)]
    struct User {
        id: String,
        name: String,
    }

    #[derive(Clone, Object)]
    struct Post {
        id: String,
    }

    #[derive(Object)]
    struct NotFound {
        message: String,
    }

    #[derive(ErrorUnion)]
    enum FindUserError {
        NotFound(NotFound),
    }

    fn builder() -> Schema<Ctx> {
        let mut schema = Schema::<Ctx>::new();
        let mut relay = Relay::<Ctx>::new();
        relay
            .node::<User>(&mut schema, |n| {
                n.id(|user: &User| user.id.clone());
                n.load_one(|id, _: &Ctx| {
                    Box::pin(async move {
                        Some(User {
                            id,
                            name: String::new(),
                        })
                    })
                });
            })
            .unwrap();
        schema.object::<User>(|o| {
            o.expose(expose!(User::name));
        });
        schema.object::<Post>(|o| {
            o.expose(expose!(Post::id));
        });
        schema.object::<NotFound>(|o| {
            o.expose(expose!(NotFound::message));
        });
        relay
            .object_connection::<User, Post, _>(
                &mut schema,
                "posts",
                "UserPostsConnection",
                |_: &User, args: ConnectionArgs, _: &Ctx| {
                    Connection::from_slice(&[], &args, |post: &Post| post.id.clone())
                },
            )
            .unwrap();
        schema.query(|q| {
            q.field_result_union::<Option<User>, FindUserError, gurafi::NoArgs, _>(
                "findUser",
                |_: gurafi::NoArgs, _: &Ctx| Ok(None),
            );
        });
        schema
    }

    let builder = builder();
    let iterations = 2_000;

    let _ = builder.build_clone().unwrap();

    for _ in 0..20 {
        let _ = builder.build_clone().unwrap();
    }

    let elapsed = time(iterations, || {
        let _ = builder.build_clone().unwrap();
    });
    report("build_clone", iterations, elapsed, 1);

    let elapsed = time(iterations, || {
        let clone = builder.clone();
        std::hint::black_box(clone);
    });
    report("clone only", iterations, elapsed, 1);

    let elapsed = time(iterations, || {
        let _ = builder.clone().build().unwrap();
    });
    report("clone + build", iterations, elapsed, 1);
}
