use async_graphql::http::GraphiQLSource;
use async_graphql_axum::GraphQL;
use axum::{response::Html, routing::get, Router};
use gurafi::{expose, FieldBuilder, Schema};
use tokio::net::TcpListener;

#[derive(Debug)]
pub struct Giraffe {
    name: String,
    birthday: String,
    height_in_meters: f32,
}

// The equivalent of using Pothos classes.
// https://pothos-graphql.dev/docs/guide/objects#using-classes
fn giraffe_schema(s: &mut Schema) {
    s.object_type::<Giraffe>("Giraffe")
        .description("Long necks, cool patterns, taller than you.")
        .field("testing", FieldBuilder::new(|m: &Giraffe| m.name.clone()));
    // .field("name", expose!(Giraffe:name))
    // .field(
    //     "age",
    //     // TODO: Rust can't infer the model type to the resolver closure. Can we trick it?
    //     FieldBuilder::new(|m: &Giraffe| m.birthday.parse::<u16>().unwrap()),
    // )
    // .field("height", expose!(Giraffe:height_in_meters));
}

#[tokio::main]
async fn main() {
    let mut schema = <Schema>::new().with(giraffe_schema);

    // TODO: Get the 'Giraffe' schema working
    // schema.query_field("giraffe", || Giraffe {
    //     name: "Tony".into(),
    //     birthday: 10.to_string(),
    //     height_in_meters: 5.2,
    // });
    schema.query_field("sayit", || "Hello, World");

    // TODO: Export the schema to a GraphQL file

    let schema = hidoki::build_schema(schema.build().unwrap()).unwrap();
    let app = Router::new().route(
        "/",
        get(|| async move { Html(GraphiQLSource::build().endpoint("/").finish()) })
            .post_service(GraphQL::new(schema)),
    );

    println!("GraphiQL IDE: http://localhost:8000");

    axum::serve(TcpListener::bind("[::]:8000").await.unwrap(), app)
        .await
        .unwrap();
}
