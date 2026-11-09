//! Adversarial review probes for `gurafi`.

use futures::executor::block_on;
use gurafi::{Enum, ID, Input, NoArgs, Object, Schema, Union, expose};

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

#[derive(Union)]
enum SearchResult {
    User(User),
    Post(Post),
}

#[derive(Enum, Clone, Copy)]
enum Role {
    Admin,
    Guest,
}

#[derive(Input)]
struct Filter {
    #[graphql(default = 10)]
    limit: i32,
    role: Option<Role>,
}

fn schema() -> gql::Schema {
    let mut schema = Schema::<Ctx>::new();
    let node = schema.interface("Node", |i| {
        i.field::<ID>("id");
        i.field::<String>("description");
    });
    schema.object::<User>(|o| {
        o.implements(node);
        o.field("id", |u: &User, _: NoArgs, _: &Ctx| ID::new(u.name.clone()))
            .description("The user's id.");
        o.expose(expose!(User::name));
        o.field("description", |u: &User, _: NoArgs, _: &Ctx| {
            format!("{} ({})", u.name, u.age)
        });
    });
    schema.object::<Post>(|o| {
        o.implements(node);
        o.field("id", |p: &Post, _: NoArgs, _: &Ctx| {
            ID::new(p.title.clone())
        });
        o.expose(expose!(Post::title));
        o.field("description", |p: &Post, _: NoArgs, _: &Ctx| {
            p.title.clone()
        });
    });
    let search_result = schema.union("SearchResult", |u| {
        u.member::<User>();
        u.member::<Post>();
    });
    schema.query(|q| {
        q.field("node", |_: NoArgs, _: &Ctx| User {
            name: "ada".into(),
            age: 36,
        })
        .as_abstract(node);
        q.field("users", |f: Filter, _: &Ctx| {
            vec![User {
                name: format!("ada{}", f.limit),
                age: 36,
            }]
        });
        q.field("search", |_: NoArgs, _: &Ctx| {
            SearchResult::User(User {
                name: "ada".into(),
                age: 36,
            })
        })
        .as_abstract(search_result);
    });
    schema.build().expect("valid schema")
}

/// PROBE G1: the standard GraphiQL introspection query against a `gurafi` schema.
#[test]
fn standard_introspection_query_on_gurafi_schema() {
    let schema = schema();
    let query = include_str!("../../gql/tests/fixtures/introspection.graphql");
    let response = block_on(gql::execute(
        &schema,
        gql::Request::new(query),
        &gql::ExecutionOptions::default(),
    ));
    println!("errors: {:?}", response.errors);
    assert!(!response.has_errors(), "{:?}", response.errors);
}

/// PROBE G2: `__typename` inside a union list resolved through `is_type_of`, plus abstract
/// interface resolution, in one request.
#[test]
fn typename_and_abstract_resolution() {
    let response = block_on(gql::execute(
        &schema(),
        gql::Request::new("{ node { __typename id description } search { __typename } }"),
        &gql::ExecutionOptions::with_context(Ctx),
    ));
    println!("data={:?} errors={:?}", response.data, response.errors);
}
