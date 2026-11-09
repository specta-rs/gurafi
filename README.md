# Gurafi

Rust crates for building GraphQL servers.

This repository is a monorepo containing all first-party Gurafi crates and the
documentation website.

## Crates

| Crate | Description |
| ----- | ----------- |
| [`gql`](crates/gql) | A GraphQL reference implementation for Rust. |
| [`gurafi`](crates/gurafi) | Intuitive GraphQL server for Rust. |
| [`gurafi-macros`](crates/gurafi-macros) | Procedural macros for `gurafi`. |
| [`hidoki`](crates/hidoki) | Serve a `gql::Schema` with [`async-graphql`](https://github.com/async-graphql/async-graphql). |
| [`suku`](crates/suku) | Simple GraphQL server. |
| [`shiri`](crates/shiri) | Rust serialization with Serde compatibility. |

## Examples

Runnable examples live in [`examples/`](examples):

```bash
cargo run -p gurafi-example-axum
cargo run -p gurafi-example-axum2
cargo run -p hidoki-example-axum
```

There are also examples for individual crates under `crates/*/examples`.

## Development

```bash
cargo check --workspace --all-targets
cargo test --workspace
```

Documentation lives in [`docs/`](docs) as plain Markdown, starting at
[`docs/README.md`](docs/README.md).

## Releasing

Publishing uses [crates.io trusted publishing](https://crates.io/docs/trusted-publishing):
the `release` workflow exchanges its GitHub OIDC identity for a short-lived
crates.io token, so **no `CARGO_REGISTRY_TOKEN` is stored** in this repository or
in Actions secrets.

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
own trusted publisher: `gurafi`, `gql`, `gurafi-macros`, `hidoki`, `suku` and
`shiri`. Examples are `publish = false` and are skipped.

A trusted publisher can only be configured once a crate exists on crates.io, and
the initial publish requires an API token. `gql` exists on crates.io but has
never published this implementation, and `gurafi`, `gurafi-macros`, `hidoki`,
`suku` and `shiri` have never been released at all, so publish each of them once
from your machine before adding its trusted publisher.

Adding required reviewers to the `release` GitHub environment is recommended: it
turns publishing into an approved action rather than something any tag push does.

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
