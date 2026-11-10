# Gurafi Documentation

Rust crates for building GraphQL servers.

## Contents

- [Architecture](architecture.md) — how `gql`, `gurafi` and `suku` fit together
- [Defining a schema](schema/object.md) — objects, inputs, enums, interfaces, unions, auth, validation, type overrides
- [Plugins](schema/plugins.md) — build-time field hooks, resolver wrapping, tracing, mocks and response extensions
- [Serving over HTTP](server/suku.md) — requests, batches, subscriptions, limits, auth
- [Schema evolution](schema/evolution.md) — compatibility diffing and SDL snapshots

## Define a schema

```rust
use gurafi::{expose, NoArgs, Object, Schema};

#[derive(Object)]
struct Giraffe {
    name: String,
    height_meters: f64,
}

let mut schema = Schema::<()>::new();

schema.object::<Giraffe>(|o| {
    o.description("Long necks, cool patterns, taller than you.");
    o.expose(expose!(Giraffe::name));
    o.expose(expose!(Giraffe::height_meters as "heightMeters"));
});

schema.query(|q| {
    q.field("giraffe", |_: NoArgs, _: &()| Giraffe {
        name: "Melman".into(),
        height_meters: 5.2,
    });
});

let schema = schema.build().unwrap();
```

`gurafi` only adds developer experience. The resulting `gql::Schema` is a normal
GraphQL schema and can be executed directly:

```rust
let response = gql::execute(
    &schema,
    gql::Request::new("{ giraffe { name heightMeters } }"),
    &gql::ExecutionOptions::default(),
).await;
```

## Serve over HTTP

```rust
use suku::{Context, Server};

let server = Server::new(schema)
    .introspection(true)
    .max_depth(15)
    .max_complexity(1_000);

let app = suku::axum::GraphQL::new(server)
    .path("/graphql")
    .graphiql(true)
    .with_context(|request: suku::ContextRequest| async move {
        Ok(Context::empty())
    })
    .into_router();
```

See [`examples/axum`](../examples/axum) for a runnable server with a Bearer-token
authentication extractor and CORS.

## Layers

| Layer | Crate | Knows about |
| --- | --- | --- |
| Specification | `gql` | GraphQL documents, types, validation, execution, introspection |
| DX | `gurafi` | `gql` and Rust types |
| Transport | `suku` | `gql` and HTTP |

The layers are independent: another high-level API can build on `gql`, and another
HTTP framework can serve a `gql::Schema` without `suku`.
