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

The documentation website is in [`docs/`](docs):

```bash
cd docs
pnpm install
pnpm dev
```

## License

Licensed under the [MIT License](LICENSE).
