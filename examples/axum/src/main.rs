use std::{collections::HashMap, net::Ipv6Addr, sync::Arc};

use axum::{
    response::Html,
    routing::{get, post},
    Json, Router,
};
use serde::Deserialize;
use suku::Server;
use tokio::net::TcpListener;
use tracing::info;
use tracing_subscriber::{layer::SubscriberExt, util::SubscriberInitExt};

use crate::tracing_timing::CustomLayer;

mod tracing_timing;

// TODO: Move this into Suku
#[derive(Deserialize)]
pub struct Request {
    operation_name: Option<String>,
    query: String,
}

#[tokio::main]
async fn main() {
    tracing_subscriber::registry::Registry::default()
        .with(CustomLayer)
        .with(tracing_subscriber::fmt::layer())
        .init();

    let server = Arc::new(Server::new(basic_schema()));

    let app = Router::new()
        .route(
            "/",
            get(|| async {
                // TODO: Bundle the static assets into a Rust crate
                Html(include_str!("../graphiql.html"))
            }),
        )
        .route(
            "/graphql",
            post(|Json(body): Json<Request>| async move { Json(server.execute(body.query).await) }),
        );

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

use gql::*;
pub fn basic_schema() -> Schema {
    Schema {
        query: ObjectType {
            name: "Query".into(),
            description: None,
            fields: {
                let mut fields = HashMap::new();

                fields.insert(
                    "hello".into(),
                    FieldConfig {
                        description: None,
                        deprecation_reason: None,
                        args: None,
                        ty: OutputType::NonNullable(ScalarType::string().into()),
                        resolve: Some(|| "Hello World".to_string()),
                    },
                );

                fields.into()
            },
        },
        mutation: None,
    }
}
