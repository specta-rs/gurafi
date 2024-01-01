use std::{collections::HashMap, net::Ipv6Addr, sync::Arc};

use axum::{
    http::{HeaderMap, HeaderValue, Method},
    response::Html,
    routing::{get, post},
    Json, Router,
};
use serde::Deserialize;
use suku::{OwnedSerializer, Server};
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
            post(|Json(req): Json<GQLRequest>| async move {
                let mut body = Vec::new();
                let mut json = serde_json::Serializer::new(&mut body);
                let serializer = OwnedSerializer::new(&mut json);

                match server.execute(serializer, req.query).await {
                    Ok(()) => {}
                    Err(_) => todo!(),
                };

                let mut headers = HeaderMap::new();
                headers.insert("Content-Type", HeaderValue::from_static("application/json"));
                (headers, body)
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
                        resolve: Some(ResolveFn::Ptr(|_, result| {
                            result.serialize_str("Hello World")
                        })),
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
                                            resolve: Some(ResolveFn::Ptr(|_, result| {
                                                result.serialize_str("Hello World Suku")
                                            })),
                                        },
                                    );

                                    fields.into()
                                },
                                interfaces: None,
                                is_type_of: None,
                            }
                            .into(),
                        ),
                        resolve: Some(ResolveFn::Ptr(|_, result| result.serialize_str("Broken"))), // TODO: This is wrong
                    },
                );

                fields.into()
            },
            is_type_of: None,
            interfaces: None,
        },
        mutation: None,
        subscription: None,
    }
}
