# Architecture

Gurafi is split into layers with one rule: **lower layers never know about higher
layers**.

| Layer | Crate | Knows about |
| --- | --- | --- |
| Specification engine | `gql` | GraphQL documents, types, execution, introspection |
| Developer experience | `gurafi` (+ `gurafi-macros`) | `gql`, Rust types |
| Transport | `suku` | `gql`, HTTP |

This is what makes it possible to build another high-level API or another
transport without forking the engine. Nothing in `gql` names `gurafi`, `suku`,
`axum`, a database or an authentication scheme.

## `gql`: the specification

`gql` is a reference implementation of the [GraphQL October 2021
specification](https://spec.graphql.org/October2021/). Its public surface is
explicit and stable:

| Module | Responsibility |
| --- | --- |
| `ast` | Borrowed abstract syntax tree for executable documents and SDL |
| `parser` | Hand-written lexer and recursive-descent parser for the whole grammar |
| `schema` | Immutable, validated `Schema`; `SchemaBuilder`; SDL import; schema diffing |
| `types` | Scalars, objects, interfaces, unions, enums, input objects, fields, directives |
| `validation` | Every executable-document validation rule |
| `execution` | Variable/argument coercion, field collection, resolvers, subscription events |
| `introspection` | `__schema`, `__type`, `__typename` and the introspection type system |
| `complexity` | Static query cost and depth estimation |
| `sdl` | SDL printing |
| `value` | Runtime input/result values, including opaque `External` model values |

### Design decisions

- **Names, not pointers.** A field's type references another type by name.
  Recursive schemas therefore need no interior mutability. The builder resolves
  and validates every name once.
- **Immutable schemas.** `Schema` is cheap to clone (`Arc` inside) and safe to
  share across threads and requests.
- **Values are explicit.** Execution produces `Value`, not JSON. Transports
  decide how to serialize; tests can assert structure without a JSON round trip.
- **No hidden panics.** Malformed input, invalid schemas and resolver failures
  become `GraphQLError`s. Public APIs do not use `unsafe`.
- **Errors are data.** `GraphQLError` carries source locations, response paths
  and extensions, and implements `serde::Serialize` when the `serde` feature is
  enabled.
- **Opaque model values.** `Value::External` lets a type-safe layer pass its own
  Rust models (`User`, `Post`, …) between resolvers. The engine never tries to
  serialize them.

### Execution pipeline

```
source ──parse──► AST ──validate──► AST ──select operation──►
coerce variables ──complexity/depth checks──► collect fields ──►
resolvers ──complete values (null propagation) ──► Response { data, errors }
```

Every stage can short-circuit with specification-shaped errors. Resolvers are
only reached by a valid operation with valid variables.

### Performance

- The parser borrows names and string literals from the source.
- Names are interned once per schema (`Arc<str>`).
- Object values use an inline `SmallVec`, avoiding a heap allocation for the
  common small-object case.
- Field collection groups by response key once per selection set and deduplicates fragments.
- Sibling fields on queries resolve concurrently with a bounded limit; mutations stay serial.
- Introspection results are descriptors, not copies of the whole schema.
- Complexity analysis runs before execution, so expensive operations are
  rejected before any resolver or database call.

## `gurafi`: developer experience

`gurafi` is the Pothos analogue. It exists to make the common case feel like
writing Rust, not like describing a schema:

- Rust types infer GraphQL types (`String` → `String!`, `Option<T>` → nullable,
  `Vec<T>` → list, `Result<T, E>` in fallible resolvers).
- `#[derive(Object)]`, `#[derive(Input)]`, `#[derive(Enum)]` and
  `#[derive(Scalar)]` connect Rust models to the engine.
- `expose!(Model::field)` creates a field from a Rust field with its name and
  type inferred.
- Field args are derived from a typed input struct; argument declarations are
  not duplicated.
- `.auth(...)`, `.complexity(...)`, `.description(...)` and `.deprecated(...)`
  attach the same metadata Pothos exposes through field options.
- Interfaces, unions, enums, custom scalars and subscriptions are first-class.

Everything `gurafi` produces is a normal `gql::Schema`; `suku` (or any other
transport) cannot tell the difference.

## `suku`: transport

`suku` turns a `gql::Schema` into an HTTP service:

- POST/GET GraphQL over HTTP with correct content negotiation and status codes
- JSON batching
- Server-sent-event subscriptions
- GraphiQL
- Request limits (body, depth, complexity, errors, batch size)
- A context extractor for authentication (Bearer tokens, cookies, headers)
- Field authorization through `gql::Authorizer`

`suku` has no schema-building logic and no knowledge of `gurafi` types.

## Testing philosophy

- Parser tests cover the full grammar and lexical edge cases.
- Validation tests cover every specification rule.
- Execution tests cover aliases, fragments, directives, variables, coercion,
  lists, null propagation, abstract types, mutations and subscriptions.
- Introspection tests compare against the standard introspection query.
- Schema diff tests pin backwards-compatibility behaviour.
- `gurafi` tests exercise the public API end to end, including derive macros.
- `suku` tests exercise HTTP behaviour without opening sockets where possible.
