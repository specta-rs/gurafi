//! Connections, cursors, page info and runtime type names.

#![allow(dead_code)]

use futures::executor::block_on;
use gurafi::{NoArgs, Object, Schema, expose, gql};
use gurafi_relay::{Connection, ConnectionArgs, Cursor, Relay, RelayObjectExt};

#[derive(Clone, Object)]
struct User {
    id: String,
}

#[derive(Clone, Object)]
struct Post {
    id: String,
    title: String,
}

struct Ctx {
    posts: Vec<Post>,
}

impl Ctx {
    fn posts() -> Self {
        Self {
            posts: vec![
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
            ],
        }
    }

    fn empty() -> Self {
        Self { posts: Vec::new() }
    }
}

fn user() -> User {
    User { id: "u1".into() }
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
        .unwrap_or_else(|| panic!("missing data: {:?}", response.errors))
}

fn posts(response: &gql::Response) -> &gql::Object {
    data(response)
        .get("user")
        .unwrap()
        .as_object()
        .unwrap()
        .get("posts")
        .unwrap()
        .as_object()
        .unwrap()
}

fn edges(response: &gql::Response) -> &[gql::Value] {
    posts(response).get("edges").unwrap().as_list().unwrap()
}

fn edge_titles(response: &gql::Response) -> Vec<&str> {
    edges(response)
        .iter()
        .map(|edge| {
            edge.as_object()
                .unwrap()
                .get("node")
                .unwrap()
                .as_object()
                .unwrap()
                .get("title")
                .unwrap()
                .as_str()
                .unwrap()
        })
        .collect()
}

fn page_info(response: &gql::Response) -> &gql::Object {
    posts(response)
        .get("pageInfo")
        .unwrap()
        .as_object()
        .unwrap()
}

fn boolean(object: &gql::Object, key: &str) -> bool {
    object.get(key).and_then(gql::Value::as_bool).unwrap()
}

fn string(object: &gql::Object, key: &str) -> String {
    object
        .get(key)
        .and_then(gql::Value::as_str)
        .unwrap()
        .to_owned()
}

fn connection_schema() -> gql::Schema {
    let mut schema = Schema::<Ctx>::new();
    let mut relay = Relay::<Ctx>::new();
    schema.object::<User>(|o| {
        o.expose(expose!(User::id));
    });
    schema.object::<Post>(|o| {
        o.expose(expose!(Post::title));
    });
    relay
        .object_connection::<User, Post, _>(
            &mut schema,
            "posts",
            "UserPostsConnection",
            |_user: &User, args: ConnectionArgs, ctx: &Ctx| {
                Connection::from_slice(&ctx.posts, &args, |post: &Post| post.id.clone())
            },
        )
        .unwrap();
    schema.query(|q| {
        q.field("user", |_: NoArgs, _: &Ctx| user());
    });
    schema.build().expect("valid schema")
}

const FIELDS: &str = r#"
    totalCount
    pageInfo { hasNextPage hasPreviousPage startCursor endCursor }
    edges { cursor node { title } }
"#;

#[test]
fn first_pages_in_order_with_cursors_and_page_info() {
    let schema = connection_schema();
    let response = execute(
        &schema,
        &format!(r#"{{ user {{ posts(first: 2) {{ {FIELDS} }} }} }}"#),
        Ctx::posts(),
    );
    assert!(!response.has_errors(), "{:?}", response.errors);
    assert_eq!(edge_titles(&response), vec!["First", "Second"]);

    let page = page_info(&response);
    assert!(boolean(page, "hasNextPage"));
    assert!(!boolean(page, "hasPreviousPage"));
    assert_eq!(string(page, "startCursor"), Cursor::encode("p1"));
    assert_eq!(string(page, "endCursor"), Cursor::encode("p2"));
    assert_eq!(
        posts(&response)
            .get("totalCount")
            .and_then(gql::Value::as_i32),
        Some(3)
    );

    // Cursors are opaque Base64, not the raw local ID.
    let first = edges(&response)[0].as_object().unwrap();
    assert_eq!(string(first, "cursor"), "cDE=");
}

#[test]
fn after_and_before_slice_the_list() {
    let schema = connection_schema();
    let after = Cursor::encode("p1");
    let response = execute(
        &schema,
        &format!(r#"{{ user {{ posts(first: 2, after: "{after}") {{ {FIELDS} }} }} }}"#),
        Ctx::posts(),
    );
    assert!(!response.has_errors(), "{:?}", response.errors);
    assert_eq!(edge_titles(&response), vec!["Second", "Third"]);
    let page = page_info(&response);
    assert!(!boolean(page, "hasNextPage"));
    assert!(boolean(page, "hasPreviousPage"));

    let before = Cursor::encode("p3");
    let response = execute(
        &schema,
        &format!(r#"{{ user {{ posts(before: "{before}") {{ {FIELDS} }} }} }}"#),
        Ctx::posts(),
    );
    assert!(!response.has_errors(), "{:?}", response.errors);
    assert_eq!(edge_titles(&response), vec!["First", "Second"]);
    assert!(boolean(page_info(&response), "hasNextPage"));
}

#[test]
fn last_takes_from_the_end() {
    let schema = connection_schema();
    let response = execute(
        &schema,
        &format!(r#"{{ user {{ posts(last: 2) {{ {FIELDS} }} }} }}"#),
        Ctx::posts(),
    );
    assert!(!response.has_errors(), "{:?}", response.errors);
    assert_eq!(edge_titles(&response), vec!["Second", "Third"]);
    let page = page_info(&response);
    assert!(boolean(page, "hasPreviousPage"));
    assert!(!boolean(page, "hasNextPage"));
}

#[test]
fn empty_boundaries_and_zero_page_sizes() {
    let schema = connection_schema();

    let empty = execute(
        &schema,
        &format!(r#"{{ user {{ posts {{ {FIELDS} }} }} }}"#),
        Ctx::empty(),
    );
    assert!(!empty.has_errors(), "{:?}", empty.errors);
    assert!(edges(&empty).is_empty());
    let page = page_info(&empty);
    assert!(!boolean(page, "hasNextPage"));
    assert!(!boolean(page, "hasPreviousPage"));
    assert!(page.get("startCursor").unwrap().is_null());
    assert!(page.get("endCursor").unwrap().is_null());
    assert_eq!(
        posts(&empty).get("totalCount").and_then(gql::Value::as_i32),
        Some(0)
    );

    let zero = execute(
        &schema,
        &format!(r#"{{ user {{ posts(first: 0) {{ {FIELDS} }} }} }}"#),
        Ctx::posts(),
    );
    assert!(!zero.has_errors(), "{:?}", zero.errors);
    assert!(edges(&zero).is_empty());
    assert!(boolean(page_info(&zero), "hasNextPage"));
    assert!(!boolean(page_info(&zero), "hasPreviousPage"));

    let oversized = execute(
        &schema,
        &format!(r#"{{ user {{ posts(first: 99) {{ {FIELDS} }} }} }}"#),
        Ctx::posts(),
    );
    assert!(!oversized.has_errors(), "{:?}", oversized.errors);
    assert_eq!(edge_titles(&oversized), vec!["First", "Second", "Third"]);
    assert!(!boolean(page_info(&oversized), "hasNextPage"));
}

#[test]
fn invalid_cursors_are_errors() {
    let schema = connection_schema();
    for cursor in ["not-base64", Cursor::encode("missing").as_str()] {
        let response = execute(
            &schema,
            &format!(r#"{{ user {{ posts(after: "{cursor}") {{ edges {{ cursor }} }} }} }}"#),
            Ctx::posts(),
        );
        assert!(response.has_errors(), "{cursor:?} should fail");
        assert!(
            response
                .errors
                .iter()
                .any(|error| error.message().contains("cursor")),
            "{:?}",
            response.errors
        );
    }

    let negative = execute(
        &schema,
        r#"{ user { posts(first: -1) { edges { cursor } } } }"#,
        Ctx::posts(),
    );
    assert!(negative.has_errors());
}

#[test]
fn names_reach_sdl_and_no_node_interface_is_required() {
    let schema = connection_schema();
    let sdl = schema.sdl();
    assert!(sdl.contains("type UserPostsConnection"), "SDL was:\n{sdl}");
    assert!(
        sdl.contains("type UserPostsConnectionEdge"),
        "SDL was:\n{sdl}"
    );
    assert!(
        sdl.contains("edges: [UserPostsConnectionEdge!]!"),
        "SDL was:\n{sdl}"
    );
    assert!(sdl.contains("pageInfo: PageInfo!"), "SDL was:\n{sdl}");
    assert!(sdl.contains("node: Post!"), "SDL was:\n{sdl}");
    assert!(!sdl.contains("interface Node"), "SDL was:\n{sdl}");
}

#[test]
fn explicit_connection_registration_and_object_extension() {
    let mut schema = Schema::<Ctx>::new();
    let mut relay = Relay::<Ctx>::new();
    schema.object::<Post>(|o| {
        o.expose(expose!(Post::title));
    });
    let connection = relay
        .connection::<Post>(&mut schema, "UserPostsConnection")
        .unwrap();
    schema.object::<User>(|object| {
        object.connection(
            &connection,
            "posts",
            |_user: &User, args: ConnectionArgs, ctx: &Ctx| {
                Connection::from_slice(&ctx.posts, &args, |post: &Post| post.id.clone())
            },
        );
    });
    schema.query(|q| {
        q.field("user", |_: NoArgs, _: &Ctx| user());
    });
    let schema = schema.build().expect("valid schema");

    let response = execute(
        &schema,
        r#"{ user { posts(first: 1) { edges { node { title } } } } }"#,
        Ctx::posts(),
    );
    assert!(!response.has_errors(), "{:?}", response.errors);
    assert_eq!(edge_titles(&response), vec!["First"]);
}

#[test]
fn root_connections_and_async_connections_work() {
    let mut schema = Schema::<Ctx>::new();
    let mut relay = Relay::<Ctx>::new();
    schema.object::<Post>(|o| {
        o.expose(expose!(Post::title));
    });
    relay
        .query_connection(
            &mut schema,
            "allPosts",
            "PostConnection",
            |args: ConnectionArgs, ctx: &Ctx| {
                Connection::from_slice(&ctx.posts, &args, |post: &Post| post.id.clone())
            },
        )
        .unwrap();
    relay
        .object_connection_async::<User, Post, _>(
            &mut schema,
            "posts",
            "UserPostsConnection",
            |_user: &User, args: ConnectionArgs, ctx: &Ctx| {
                Box::pin(async move {
                    Connection::from_slice(&ctx.posts, &args, |post: &Post| post.id.clone())
                })
            },
        )
        .unwrap();
    schema.query(|q| {
        q.field("user", |_: NoArgs, _: &Ctx| user());
    });
    let schema = schema.build().expect("valid schema");

    let response = execute(
        &schema,
        r#"{ allPosts(first: 1) { edges { node { title } } } user { posts(last: 1) { edges { node { title } } } } }"#,
        Ctx::posts(),
    );
    assert!(!response.has_errors(), "{:?}", response.errors);
    let data = data(&response);
    assert_eq!(
        data.get("allPosts")
            .unwrap()
            .as_object()
            .unwrap()
            .get("edges")
            .unwrap()
            .as_list()
            .unwrap()
            .len(),
        1
    );
    assert_eq!(edge_titles(&response), vec!["Third"]);
}

#[test]
fn from_slice_by_index_cursors_are_deterministic() {
    let items = vec!["a".to_owned(), "b".to_owned(), "c".to_owned()];
    let empty = ConnectionArgs::default();
    let page = Connection::from_slice_by_index(&items, &empty).unwrap();
    assert_eq!(page.edges.len(), 3);
    assert_eq!(page.edges[0].cursor, Cursor::encode("0"));

    let args = ConnectionArgs {
        first: Some(1),
        after: Some(Cursor::encode("0")),
        ..ConnectionArgs::default()
    };
    let page = Connection::from_slice_by_index(&items, &args).unwrap();
    assert_eq!(page.edges[0].node, "b");
    assert!(page.page_info.has_next_page);
    assert!(page.page_info.has_previous_page);
}

#[test]
fn exposes_metadata_for_static_derives() {
    // `Connection`/`Edge` are ordinary typed data structures; this test only exercises the
    // public constructors and cursor helper used by resolvers.
    let edge = gurafi_relay::Edge::new(
        Cursor::encode("x"),
        Post {
            id: "x".into(),
            title: "X".into(),
        },
    );
    let connection = Connection::new(vec![edge], gurafi_relay::PageInfo::default(), Some(1));
    assert_eq!(connection.total_count, Some(1));
    assert_eq!(Cursor::decode(&connection.edges[0].cursor).unwrap(), "x");
}

#[test]
fn duplicate_connection_registration_is_rejected() {
    let mut schema = Schema::<Ctx>::new();
    let mut relay = Relay::<Ctx>::new();
    schema.object::<Post>(|o| {
        o.expose(expose!(Post::title));
    });
    relay
        .connection::<Post>(&mut schema, "UserPostsConnection")
        .unwrap();
    let error = relay
        .connection::<Post>(&mut schema, "UserPostsConnection")
        .unwrap_err();
    assert!(error.message().contains("already registered"));
}

#[test]
fn field_ref_type_accepts_a_connection_ref() {
    let mut schema = Schema::<Ctx>::new();
    let mut relay = Relay::<Ctx>::new();
    schema.object::<Post>(|o| {
        o.expose(expose!(Post::title));
    });
    let connection = relay
        .connection::<Post>(&mut schema, "UserPostsConnection")
        .unwrap();
    schema.object::<User>(|object| {
        object.field_ref_type::<Connection<Post>, ConnectionArgs, _>(
            connection.type_ref(),
            "posts",
            |_user: &User, _args: ConnectionArgs, _ctx: &Ctx| Connection::empty(),
        );
    });
    schema.query(|q| {
        q.field("user", |_: NoArgs, _: &Ctx| user());
    });
    let schema = schema.build().expect("valid schema");

    let response = execute(
        &schema,
        r#"{ user { posts { edges { node { title } } pageInfo { hasNextPage } } } }"#,
        Ctx::posts(),
    );
    assert!(!response.has_errors(), "{:?}", response.errors);
    assert!(edges(&response).is_empty());
}
