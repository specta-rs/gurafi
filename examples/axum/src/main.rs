use std::net::Ipv6Addr;

use axum::{response::Html, routing::get, Router};
use tokio::net::TcpListener;
use tracing::info;

#[tokio::main]
async fn main() {
    tracing_subscriber::fmt::init();

    let app = Router::new()
        .route(
            "/",
            get(|| async {
                // TODO: Bundle the static assets into a Rust crate
                Html(include_str!("../graphiql.html"))
            }),
        )
        .route("/graphql", get(|| async { "Hello, World!" }));

    info!("Listening on http://[::1]:3000");
    axum::serve(
        TcpListener::bind((Ipv6Addr::UNSPECIFIED, 3000))
            .await
            .unwrap(),
        app,
    )
    .await
    .unwrap();
}
