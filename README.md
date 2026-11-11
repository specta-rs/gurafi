<div align="center">
    <picture>
        <source media="(prefers-color-scheme: dark)" srcset="assets/logo-inverse.svg">
        <img src="assets/logo.svg" alt="Gurafi logo" height="150">
    </picture>
    <h1>Gurafi</h1>
    <p><b>Type-safe, code-first GraphQL for Rust</b></p>
    <a href="https://crates.io/crates/gurafi"><img src="https://img.shields.io/crates/v/gurafi?style=flat-square" alt="crates.io"></a>
    <a href="https://docs.rs/gurafi"><img src="https://img.shields.io/badge/docs-latest-blue?style=flat-square" alt="docs.rs"></a>
    <a href="LICENSE"><img src="https://img.shields.io/crates/l/gurafi?style=flat-square" alt="License"></a>
    <a href="https://github.com/specta-rs/gurafi/actions/workflows/ci.yml"><img src="https://github.com/specta-rs/gurafi/actions/workflows/ci.yml/badge.svg" alt="CI"></a>
</div>

<br>

## Why Gurafi

GraphQL servers in Rust usually mean maintaining a schema definition alongside the types your
resolvers actually use. Gurafi starts from the other side: a Rust type is the model, derives wire
it into the GraphQL type system, and fields are exposed from that same type. Most schema mistakes
become compile errors, and the result is an ordinary `gql::Schema` — not a generated resolver file
or a second schema to keep in sync.

The approach is inspired by [Pothos GraphQL](https://pothos-graphql.dev)'s
[backing model approach](https://pothos-graphql.dev/docs/guide/schema-builder#backing-models).

## What's inside

| Crate | Role |
| ----- | ---- |
| [`gql`](crates/gql) | A GraphQL reference implementation for Rust: parser, type system, validation, execution, introspection, subscriptions, complexity analysis and schema compatibility checking. Transport- and framework-agnostic. |
| [`gurafi`](crates/gurafi) | An intuitive, type-safe, code-first schema builder built on `gql`. Rust types drive the GraphQL type system; `gurafi` only adds developer experience. |
| [`gurafi-macros`](crates/gurafi-macros) | Procedural macros for `gurafi` (`Object`, `Input`, `Args`, `Enum`, `Scalar`, `Union`, `ErrorUnion`). |
| [`gurafi-relay`](crates/gurafi-relay) | Typed global IDs, `Node`, connections and mutation payloads. |
| [`suku`](crates/suku) | A production GraphQL-over-HTTP server for a `gql::Schema`, with batching, SSE subscriptions, error masking and pluggable authentication. |

The layers are deliberately independent: `gql` knows nothing about HTTP or Rust models, `gurafi`
builds only on the public `gql` API, and `suku` only needs a `gql::Schema`. Use the whole stack, or
take just the layer you need.

## Quick start

```rust
use gurafi::{expose, NoArgs, Object, Schema};

#[derive(Object)]
struct Giraffe {
    name: String,
    height: f64,
}

let mut schema = Schema::<()>::new();

schema.object::<Giraffe>(|o| {
    o.description("Long necks, cool patterns, taller than you.");
    o.expose(expose!(Giraffe::name));
    o.expose(expose!(Giraffe::height));
});

schema.query(|q| {
    q.field("giraffe", |_: NoArgs, _: &()| Giraffe {
        name: "Tony".into(),
        height: 5.2,
    });
});

let schema = schema.build().expect("valid schema");
```

`schema` is a normal `gql::Schema`. Execute it in-process, or serve it over HTTP with `suku` — see
the [`axum` example](examples/axum).

## Documentation

The README is an overview. The full documentation lives on the
[Specta website](https://specta.dev/docs/gurafi), covering installation, the type system, plugins,
schema evolution and serving. API reference is on [docs.rs](https://docs.rs/gurafi), and runnable
code is in [`examples`](examples).

## Contributing

Development and release instructions are in [CONTRIBUTING.md](CONTRIBUTING.md).

Thanks to [Brendonovich](https://github.com/Brendonovich) for shaping some of the original ideas
for the syntax.

## License

Licensed under the [MIT License](LICENSE).
