//! A loader in a request context, exercised through real schema execution.
//!
//! These tests document what batching means in this engine: sibling fields are resolved
//! concurrently, so their loads share a batch; list items are completed one at a time, so their
//! loads are separate batches joined by the loader's cache.

use std::sync::{Arc, Mutex};

use futures::executor::block_on;
use gurafi::{GraphQLError, ID, Loader, NoArgs, Object, RequestCache, Schema, expose};

#[derive(Clone, Object)]
struct User {
    id: ID,
    name: String,
}

#[derive(Object)]
struct Post {
    author_id: ID,
}

type Batches = Arc<Mutex<Vec<Vec<String>>>>;

#[derive(Clone)]
struct Ctx {
    users: Loader<ID, User>,
    #[allow(dead_code)]
    cache: RequestCache,
}

fn context(batches: Batches) -> Ctx {
    let cache = RequestCache::new();
    let users = cache.get_or_insert_with(move || {
        Loader::<ID, User>::new(move |ids: Vec<ID>| {
            let batches = Arc::clone(&batches);
            async move {
                batches
                    .lock()
                    .unwrap()
                    .push(ids.iter().map(ID::to_string).collect());
                ids.into_iter()
                    .map(|id| {
                        Ok(User {
                            name: format!("user-{id}"),
                            id,
                        })
                    })
                    .collect::<Vec<Result<User, GraphQLError>>>()
            }
        })
    });
    Ctx {
        users: (*users).clone(),
        cache,
    }
}

fn schema() -> gql::Schema {
    let mut schema = Schema::<Ctx>::new();

    schema.object::<User>(|o| {
        o.expose(expose!(User::id));
        o.expose(expose!(User::name));
    });

    schema.object::<Post>(|o| {
        o.expose(expose!(Post::author_id));
        o.field_result_async("author", |post: &Post, _: NoArgs, ctx: &Ctx| {
            let id = post.author_id.clone();
            Box::pin(async move { ctx.users.load(id).await })
        });
    });

    schema.query(|q| {
        q.field("first", |_: NoArgs, _: &Ctx| Post {
            author_id: ID::new("1"),
        });
        q.field("second", |_: NoArgs, _: &Ctx| Post {
            author_id: ID::new("2"),
        });
        q.field("posts", |_: NoArgs, _: &Ctx| {
            vec![
                Post {
                    author_id: ID::new("1"),
                },
                Post {
                    author_id: ID::new("2"),
                },
                Post {
                    author_id: ID::new("1"),
                },
            ]
        });
    });

    schema.build().expect("valid schema")
}

fn run(schema: &gql::Schema, query: &str, ctx: Ctx) -> gql::Response {
    block_on(gql::execute(
        schema,
        gql::Request::new(query),
        &gql::ExecutionOptions::with_context(ctx),
    ))
}

fn author_names(response: &gql::Response, field: &str) -> Vec<String> {
    response
        .data
        .as_ref()
        .and_then(gql::Value::as_object)
        .and_then(|data| data.get(field))
        .and_then(gql::Value::as_object)
        .and_then(|post| post.get("author"))
        .and_then(gql::Value::as_object)
        .and_then(|author| author.get("name"))
        .and_then(gql::Value::as_str)
        .map(|name| vec![name.to_owned()])
        .unwrap_or_default()
}

#[test]
fn sibling_fields_share_one_batch() {
    let batches: Batches = Arc::default();
    let response = run(
        &schema(),
        "{ first { author { name } } second { author { name } } }",
        context(Arc::clone(&batches)),
    );

    assert!(!response.has_errors(), "unexpected errors: {response:?}");
    assert_eq!(
        batches.lock().unwrap().clone(),
        vec![vec!["1".to_owned(), "2".to_owned()]]
    );
    assert_eq!(author_names(&response, "first"), vec!["user-1"]);
    assert_eq!(author_names(&response, "second"), vec!["user-2"]);
}

#[test]
fn list_items_are_cached_even_though_they_do_not_batch() {
    let batches: Batches = Arc::default();
    let response = run(
        &schema(),
        "{ posts { author { name } } }",
        context(Arc::clone(&batches)),
    );

    assert!(!response.has_errors(), "unexpected errors: {response:?}");

    // `gql` completes list items in order, so the two authors are two batches. The repeated
    // author of the third item is a cache hit and produces no batch at all.
    assert_eq!(
        batches.lock().unwrap().clone(),
        vec![vec!["1".to_owned()], vec!["2".to_owned()]]
    );

    let posts = response
        .data
        .as_ref()
        .and_then(gql::Value::as_object)
        .and_then(|data| data.get("posts"))
        .and_then(gql::Value::as_list)
        .expect("posts is a list");
    let names = posts
        .iter()
        .map(|post| {
            post.as_object()
                .and_then(|post| post.get("author"))
                .and_then(gql::Value::as_object)
                .and_then(|author| author.get("name"))
                .and_then(gql::Value::as_str)
                .expect("every post has an author")
                .to_owned()
        })
        .collect::<Vec<_>>();
    assert_eq!(names, vec!["user-1", "user-2", "user-1"]);
}
