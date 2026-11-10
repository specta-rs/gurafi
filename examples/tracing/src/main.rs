//! A runnable example of enabling the optional `tracing` feature.
//!
//! ```sh
//! RUST_LOG=gql=debug,gurafi=debug cargo run -p example-tracing --features tracing
//! ```
//!
//! The feature is off by default, so a normal build does not compile `tracing` at all. The
//! `required-features` entry on this binary makes `cargo check --workspace --all-targets` skip it
//! unless the feature is requested.

use std::sync::Arc;

use gql::{ExecutionOptions, InputValue, ObjectType, Schema, TypeRef, Value};
use gurafi::{Loader, RequestCache};
use tracing_subscriber::{EnvFilter, fmt, prelude::*};

#[derive(Clone, Debug)]
struct User {
    name: String,
}

struct Context {
    users: Loader<String, User>,
    _cache: RequestCache,
}

impl Context {
    fn new() -> Self {
        let cache = RequestCache::new();
        let users = cache.get_or_insert_with(|| {
            Loader::<String, User>::new(|ids: Vec<String>| async move {
                ids.into_iter()
                    .map(|id| {
                        Ok(User {
                            name: format!("user-{id}"),
                        })
                    })
                    .collect()
            })
        });
        Self {
            users: (*users).clone(),
            _cache: cache,
        }
    }
}

fn main() {
    // `EnvFilter` is the recommended production setup: enable summaries at INFO and switch to
    // DEBUG only while investigating field-level or loader performance.
    tracing_subscriber::registry()
        .with(
            EnvFilter::try_from_default_env()
                .unwrap_or_else(|_| EnvFilter::new("gql=info,gurafi=info")),
        )
        .with(fmt::layer())
        .init();

    let context = Arc::new(Context::new());
    let mut builder = Schema::builder();
    builder.add_type(
        ObjectType::new("User")
            .field(gql::Field::new("name", TypeRef::string().non_null()))
            .into(),
    );
    builder.add_type(
        ObjectType::new("Query")
            .field(
                gql::Field::new("user", TypeRef::named("User").non_null())
                    .arg(InputValue::new("id", TypeRef::string().non_null()))
                    .resolve(|context| {
                        let id = context
                            .args
                            .get("id")
                            .and_then(Value::as_str)
                            .unwrap_or("1")
                            .to_owned();
                        let loader = context
                            .context
                            .require_data::<Context>()
                            .ok()
                            .map(|context| context.users.clone());
                        Box::pin(async move {
                            let Some(loader) = loader else {
                                return Err(gql::GraphQLError::new("missing example context"));
                            };
                            let user = loader.load(id).await?;
                            let mut object = gql::Object::new();
                            object.insert("name", Value::from(user.name));
                            Ok(Value::Object(Arc::new(object)))
                        })
                    }),
            )
            .into(),
    );
    builder = builder.query("Query");
    let schema = builder.build().expect("valid schema");

    let options = ExecutionOptions::with_context(context);
    let response = futures::executor::block_on(gql::execute(
        &schema,
        gql::Request::new("{ user(id: \"1\") { name } }"),
        &options,
    ));
    println!("{:?}", response.data);
}
