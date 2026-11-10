//! Run the giraffe API at <http://127.0.0.1:3000/graphql>.
//!
//! Try the GraphiQL page in a browser. Public fields work without a token; `whoami` requires:
//!
//! ```text
//! Authorization: Bearer secret
//! ```

mod schema;

use std::{net::SocketAddr, time::Duration};

use axum::{
    Router,
    http::{Method, StatusCode, header},
};
use suku::{
    Context, Server,
    axum::GraphQL,
    http::{ContextRequest, HttpError},
};
use tokio::net::TcpListener;
use tower_http::{
    cors::{Any, CorsLayer},
    trace::TraceLayer,
};
use tracing_subscriber::{layer::SubscriberExt as _, util::SubscriberInitExt as _};

use crate::schema::AppContext;

#[tokio::main]
async fn main() -> Result<(), Box<dyn std::error::Error>> {
    tracing_subscriber::registry()
        .with(
            tracing_subscriber::EnvFilter::try_from_default_env()
                .unwrap_or_else(|_| "info,tower_http=debug".into()),
        )
        .with(tracing_subscriber::fmt::layer())
        .init();

    let server = Server::new(schema::schema())
        .introspection(true)
        .max_depth(15)
        .max_complexity(500)
        .max_errors(50)
        .max_body_bytes(1024 * 1024)
        .max_batch_size(10)
        .batch_concurrency(8)
        .timeout(Duration::from_secs(15));

    let graphql = GraphQL::new(server)
        .path("/graphql")
        .graphiql(true)
        .with_context(|request: ContextRequest| async move {
            let Some(value) = request.header(header::AUTHORIZATION.as_str()) else {
                // Public fields still work without a token. Field-level auth rejects `whoami`.
                return Ok(Context::new(AppContext::anonymous()));
            };

            let value = value.to_str().map_err(|_| {
                HttpError::message(StatusCode::UNAUTHORIZED, "Invalid Authorization header")
            })?;

            let Some(token) = value.strip_prefix("Bearer ") else {
                return Err(HttpError::message(
                    StatusCode::UNAUTHORIZED,
                    "Expected `Authorization: Bearer <token>`",
                ));
            };

            match token.trim() {
                "secret" => Ok(Context::new(AppContext::authenticated("Ada"))),
                _ => Err(HttpError::message(
                    StatusCode::UNAUTHORIZED,
                    "Unknown bearer token",
                )),
            }
        });

    let app = Router::new()
        .merge(graphql.into_router())
        .layer(
            CorsLayer::new()
                .allow_origin(Any)
                .allow_methods([Method::GET, Method::POST])
                .allow_headers([header::AUTHORIZATION, header::CONTENT_TYPE]),
        )
        .layer(TraceLayer::new_for_http());

    let address = SocketAddr::from(([127, 0, 0, 1], 3000));
    let listener = TcpListener::bind(address).await?;
    tracing::info!("GraphQL endpoint: http://{address}/graphql");

    axum::serve(listener, app).await?;
    Ok(())
}
