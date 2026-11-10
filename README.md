# Gurafi

Rust crates for building GraphQL servers.

This repository is a monorepo containing all first-party Gurafi crates and their
documentation.

## Crates

| Crate | Description |
| ----- | ----------- |
| [`gql`](crates/gql) | A GraphQL reference implementation for Rust. Implements the October 2021 specification: parser, type system, validation, execution, introspection, subscriptions, complexity analysis and schema compatibility checking. It is transport- and framework-agnostic. |
| [`gurafi`](crates/gurafi) | An intuitive, type-safe, code-first GraphQL schema builder built on `gql`. Rust types drive the GraphQL type system; `gurafi` only adds developer experience. |
| [`gurafi-macros`](crates/gurafi-macros) | Procedural macros for `gurafi` (`Object`, `Input`, `Args`, `Enum`, `Scalar`, `Union`). |
| [`suku`](crates/suku) | A production GraphQL-over-HTTP server for a `gql::Schema`, with batching, SSE subscriptions and pluggable authentication. |
| [`shiri`](crates/shiri) | Serde-compatible serialization experiments. Not part of the GraphQL stack. |

The layers are deliberately decoupled:

```
       gurafi (DX, type safety)  ─┐
                                  ├──►  gql (GraphQL specification)
       suku (HTTP transport)  ────┘
```

`gql` knows nothing about HTTP, frameworks, authentication or database access.
`gurafi` only uses public `gql` APIs, and `suku` only understands `gql::Schema`.
Any other high-level API or transport can be built on the same interface.

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

Serve the schema over HTTP with `suku`; see [`examples/axum`](examples/axum).

## What `gurafi` adds

- **Args, not leaked inputs.** `#[derive(Input)]` (or `#[derive(Args)]`) is
  args-only by default. Add `#[graphql(name = "FilterInput")]` only when the type
  is also nested inside another input.
- **Splittable definitions.** `object`/`interface` reopen an existing type, and
  `object_fields`/`interface_fields`/`query_fields` make that intent explicit.
  Declarations from different modules merge; duplicate field names still fail.
- **Checked type control.** `nullable`, `non_null`, `nullable_items` and
  `as_type`/`returns` are validated at build time, so an override cannot claim a
  non-null value from an `Option` resolver.
- **Authorization at every level.** Object and interface rules are inherited by
  their fields, with sync and async predicates.
- **Validation, one-of inputs and resolver info.** `#[graphql(validate)]`,
  `#[graphql(one_of)]`, and `*_with_info` resolver variants.
- **Reusable builders.** `Schema` is `Clone`; `build_with(BuildOptions)` and
  `build_clone` build without throwing the declaration away.

See [`docs/schema/object.md`](docs/schema/object.md) for the full guide.

## Development

```bash
cargo check --workspace --all-targets
cargo test --workspace
```

Documentation lives in [`docs/`](docs) as plain Markdown, starting at
[`docs/README.md`](docs/README.md).

## Releasing

Publishing uses crates.io trusted publishing: the `release` workflow exchanges
its GitHub OIDC identity for a short-lived crates.io token, so **no
`CARGO_REGISTRY_TOKEN` is stored** in this repository or in Actions secrets.

**One-time setup** — a crate owner adds a trusted publisher in the crates.io UI
(the crate's **Settings → Trusted Publishing**) for every published crate. These
values must match `.github/workflows/release.yml` exactly, or the token exchange
is refused:

| Field | Value |
| --- | --- |
| Owner | `specta-rs` |
| Repository | `gurafi` |
| Workflow filename | `release.yml` |
| Environment | `release` |

The workflow publishes the whole workspace, so each of these crates needs its
own trusted publisher: `gurafi`, `gql`, `gurafi-macros`, `suku` and `shiri`.
Examples are `publish = false` and are skipped.

**Cutting a release:**

1. Bump the `version` of every published workspace crate and merge to `main`.
   The tag names the `gurafi` crate version.
2. Tag the merge commit and push the tag:

   ```bash
   git tag v0.1.0
   git push origin v0.1.0
   ```

The `release` workflow then verifies that the tag matches `gurafi`'s version,
builds and tests the workspace, publishes every crate in dependency order, and
creates the GitHub release.

## License

Licensed under the [MIT License](LICENSE).
