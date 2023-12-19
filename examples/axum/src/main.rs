use std::{collections::HashMap, net::Ipv6Addr, sync::Arc};

use axum::{
    http::Method,
    response::Html,
    routing::{get, post},
    Json, Router,
};
use serde::Deserialize;
use suku::Server;
use tokio::net::TcpListener;
use tower_http::cors::CorsLayer;
use tracing::info;
use tracing_subscriber::{layer::SubscriberExt, util::SubscriberInitExt};

use crate::tracing_timing::CustomLayer;

mod tracing_timing;

// TODO: Move this into Suku
#[derive(Deserialize)]
pub struct GQLRequest {
    operation_name: Option<String>,
    query: String,
}

#[tokio::main]
async fn main() {
    tracing_subscriber::registry::Registry::default()
        .with(CustomLayer)
        .with(tracing_subscriber::fmt::layer())
        .init();

    let server = Arc::new(Server::new(basic_schema()).introspection(true));

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
            post(|Json(body): Json<GQLRequest>| async move {
                // match server.execute(body.query).await {
                //     Ok(response) => Json(response),
                //     Err(_) => todo!(),
                // }
                todo!();
            }),
        )
        .layer(
            CorsLayer::new()
                .allow_methods([Method::GET, Method::POST])
                .allow_origin(tower_http::cors::Any)
                .allow_headers(tower_http::cors::Any),
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
                        resolve: Some(|_| "Hello World".to_string()),
                    },
                );

                fields.insert(
                    "nested".into(),
                    FieldConfig {
                        description: None,
                        deprecation_reason: None,
                        args: None,
                        ty: OutputType::NonNullable(
                            ObjectType {
                                name: "Nested".into(),
                                description: None,
                                fields: {
                                    let mut fields = HashMap::new();

                                    fields.insert(
                                        "suku".into(),
                                        FieldConfig {
                                            description: None,
                                            deprecation_reason: None,
                                            args: None,
                                            ty: OutputType::NonNullable(
                                                ScalarType::string().into(),
                                            ),
                                            resolve: Some(|_| "Hello World Suku".to_string()),
                                        },
                                    );

                                    fields.into()
                                },
                            }
                            .into(),
                        ),
                        resolve: Some(|_| "Broken".to_string()), // TODO: This is wrong
                    },
                );

                fields.into()
            },
        },
        mutation: None,
    }
}
