# gurafi documentation

`gurafi` is a type-safe, code-first GraphQL stack for Rust. Rust types describe the schema,
`#[derive]` macros provide the wiring, and the resulting `gql::Schema` is a normal GraphQL
schema that can be executed directly or served over HTTP.

The documentation follows a progressive learning path: start with a schema and a query, then add
models, fields, arguments and context. Each concept is expressed with Rust types, builder methods
and derives rather than with generated resolver types.

## Start here

- [Getting started](guide/getting-started.md) — install the crates, build a schema, run a query
- [Objects](guide/objects.md) — turn application data into GraphQL object types
- [Fields](guide/fields.md) — resolvers, lists, nullability, exposure and field options
- [Using args](guide/args.md) — typed arguments from Rust structs
- [Using context](guide/context.md) — request state shared by every resolver

## Guide

- [Enums](guide/enums.md)
- [Scalars](guide/scalars.md)
- [Input objects](guide/inputs.md)
- [Queries, mutations and subscriptions](guide/operations.md)
- [Interfaces](guide/interfaces.md)
- [Unions](guide/unions.md)
- [Schema builder](guide/schema-builder.md)
- [Using plugins](guide/using-plugins.md)
- [File layout](guide/app-layout.md)
- [Circular references](guide/circular-references.md)
- [Patterns](guide/patterns.md)
- [Nullability](guide/nullability.md)
- [Type inference](guide/type-inference.md)
- [Using GraphiQL](guide/playground.md)
- [Printing schemas](guide/printing-schemas.md)
- [Generating client types](guide/client-codegen.md)
- [Troubleshooting](guide/troubleshooting.md)
- [Writing plugins](guide/writing-plugins.md)
- [Integrations and non-goals](guide/non-goals.md)

## Plugins and extensions

- [Add GraphQL and schema composition](plugins/add-graphql.md)
- [Complexity](plugins/complexity.md)
- [Dataloaders and request cache](plugins/dataloader.md)
- [Directives](plugins/directives.md)
- [Typed errors](plugins/errors.md)
- [Mocks](plugins/mocks.md)
- [Relay](plugins/relay.md)
- [Scope auth](plugins/scope-auth.md)
- [Simple objects](plugins/simple-objects.md)
- [Tracing and observability](plugins/tracing.md)
- [Validation](plugins/validation.md)

## Server

- [Serving over HTTP with `suku`](server/suku.md) — HTTP, SSE, WebSocket subscriptions, multipart uploads and GraphiQL
- [Error formatting and masking](server/suku.md#error-formatting-and-masking)

## Reference

- [Architecture](architecture.md) — how `gql`, `gurafi`, `gurafi-relay` and `suku` fit together
- [Schema evolution](guide/printing-schemas.md) — compatibility diffing, SDL snapshots, composition

## Crates

| Crate | Role |
| --- | --- |
| [`gql`](../crates/gql) | GraphQL specification: parser, validation, execution, introspection |
| [`gurafi`](../crates/gurafi) | Rust-first schema builder and type-safe DX |
| [`gurafi-macros`](../crates/gurafi-macros) | Derives for `Object`, `Args`, `Input`, `Enum`, `Scalar`, `Union`, `ErrorUnion` |
| [`gurafi-relay`](../crates/gurafi-relay) | Relay global IDs, `Node`, connections and mutation payloads |
| [`suku`](../crates/suku) | GraphQL over HTTP, batching, SSE subscriptions and error masking |

The crates are deliberately independent: `gql` knows nothing about HTTP or Rust models, `gurafi`
builds only on the public `gql` API, and `suku` only understands `gql::Schema`.
