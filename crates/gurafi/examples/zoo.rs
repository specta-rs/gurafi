//! A complete schema built with `gurafi`, printed as SDL and executed once.
//!
//! Run with:
//!
//! ```sh
//! cargo run -p gurafi --example zoo
//! ```

#![allow(dead_code)]

use gurafi::{BuildOptions, Enum, ID, Input, NoArgs, Object, Result, Schema, Union, expose};

/// Per-request data. A real server puts the authenticated user and database handles here.
struct Ctx {
    user: Option<&'static str>,
}

#[derive(Object)]
#[graphql(description = "A long-necked animal.")]
struct Giraffe {
    name: String,
    height: f32,
}

#[derive(Object)]
struct Zookeeper {
    name: String,
}

#[derive(Object)]
struct Post {
    title: String,
}

#[derive(Union)]
#[graphql(description = "Anything the zoo's search can return.")]
enum SearchResult {
    Giraffe(Giraffe),
    Zookeeper(Zookeeper),
    Post(Post),
}

#[derive(Enum)]
#[graphql(description = "Giraffe colours.")]
enum Color {
    #[graphql(name = "RED")]
    Red,
    #[graphql(name = "BLUE")]
    Blue,
}

#[derive(Input)]
#[graphql(description = "Paging options.")]
struct PageArgs {
    #[graphql(description = "Max results", default = 10)]
    limit: i32,
    after: Option<String>,
}

#[derive(Input)]
struct GiraffeArgs {
    id: ID,
}

fn main() {
    let mut schema = Schema::<Ctx>::new();
    schema.description("A small zoo.");

    let node = schema.interface("Node", |i| {
        i.description("Anything with an id.").field::<ID>("id");
    });

    schema.object::<Giraffe>(|o| {
        o.implements(node);
        // The type and arguments come from `Node`; only the resolver is new.
        o.resolve_inherited("id", |g: &Giraffe, _: NoArgs, _: &Ctx| {
            ID::new(g.name.clone())
        });
        o.expose(expose!(Giraffe::name));
        o.expose(expose!(Giraffe::height as "heightMeters"));
        o.field("color", |_: &Giraffe, _: NoArgs, _: &Ctx| Color::Blue);
        o.field("keeper", |_: &Giraffe, _: NoArgs, _: &Ctx| Some("Bob"));
    });

    schema.object::<Zookeeper>(|o| {
        o.implements(node);
        o.field("id", |z: &Zookeeper, _: NoArgs, _: &Ctx| {
            ID::new(z.name.clone())
        });
        o.expose(expose!(Zookeeper::name));
    });

    schema.object::<Post>(|o| {
        o.expose(expose!(Post::title));
    });

    let search = schema.union("SearchResult", |u| {
        u.member::<Giraffe>().member::<Zookeeper>().member::<Post>();
    });

    schema.query(|q| {
        q.field("giraffes", |args: PageArgs, _: &Ctx| {
            let mut giraffes = vec![
                Giraffe {
                    name: "Melman".into(),
                    height: 3.4,
                },
                Giraffe {
                    name: "Karen".into(),
                    height: 4.1,
                },
            ];
            let limit = usize::try_from(args.limit.max(0)).unwrap_or(usize::MAX);
            giraffes.truncate(limit);
            let _ = args.after;
            giraffes
        })
        .description("Every giraffe, paged.");

        q.field("giraffe", |args: GiraffeArgs, _: &Ctx| {
            (args.id.as_str() == "Melman").then(|| Giraffe {
                name: "Melman".into(),
                height: 3.4,
            })
        });

        q.field_result("search", |_: NoArgs, _: &Ctx| {
            Result::Ok(SearchResult::Zookeeper(Zookeeper { name: "Bob".into() }))
        })
        .as_abstract(search);

        q.field("myName", |_: NoArgs, ctx: &Ctx| ctx.user.map(str::to_owned))
            .auth(|ctx: &Ctx| ctx.user.is_some())
            .auth_error("Sign in to see your name.")
            .complexity(1);

        q.field_with_info("request", |_: NoArgs, _: &Ctx, info| {
            format!("{}:{}", info.parent_type, info.field_name)
        })
        .description("The resolved field's own name.");
    });

    let schema = schema
        .build_with(BuildOptions::new().query_description("The zoo's entry points."))
        .expect("the schema is valid");

    println!("{}", schema.sdl());

    let response = futures::executor::block_on(gql::execute(
        &schema,
        gql::Request::new(
            r#"{
                giraffes(limit: 1) { name heightMeters color }
                search { __typename ... on Zookeeper { name } }
                myName
                request
            }"#,
        ),
        &gql::ExecutionOptions::with_context(Ctx { user: None }),
    ));

    println!("{response:#?}");
}
