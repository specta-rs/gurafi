# gurafi-relay

Pothos-style Relay support for [`gurafi`](../gurafi).

The crate adds the pieces of [Relay](https://relay.dev/graphql/connections.htm) that a code-first
GraphQL server normally writes by hand:

- typed **global IDs** (`Base64("TypeName:localId")`)
- the shared **`Node` interface** plus `Query.node` / `Query.nodes`
- **connections** with explicit runtime type names, cursors and `PageInfo`
- **mutation payloads** with configurable `clientMutationId`

It is a thin layer over the public `gurafi` builder. It does not patch the executor and has no
async-runtime dependency; async resolvers use `Box::pin`, exactly like `gurafi` itself.

## Install

```toml
[dependencies]
gurafi = { path = "../gurafi" }
gurafi-relay = { path = "../gurafi-relay" }
```

## Global IDs

`GlobalId<T>` is an input and output scalar with the GraphQL type `ID!`.

```rust
use gurafi::Object;
use gurafi_relay::GlobalId;

#[derive(Object)]
struct User { id: String }

let id = GlobalId::<User>::new("42");
assert_eq!(id.encode(), "VXNlcjo0Mg==");
assert_eq!(GlobalId::<User>::decode("VXNlcjo0Mg==").unwrap().as_local(), "42");

// The model parameter is checked on input.
assert!(GlobalId::<User>::decode("UG9zdDo0Mg==").is_err()); // Post:42
```

Malformed Base64, missing padding, non-UTF-8 payloads, a missing `TypeName:localId` separator and
a mismatched type name are all rejected with GraphQL errors. The encoder/decoder is a small
standard Base64 implementation local to this crate, so no extra dependency is needed.

Use `gurafi_relay::decode_global_id` when the type is not known up front (the Node dispatcher uses
it).

## Node

```rust
use gurafi::{Object, Schema, expose};
use gurafi_relay::Relay;

#[derive(Clone, Object)]
struct User { id: String, name: String }

struct Ctx;

let mut schema = Schema::<Ctx>::new();
let mut relay = Relay::<Ctx>::new();

relay.node::<User>(&mut schema, |n| {
    n.id(|user: &User| user.id.clone());
    n.load_one(|id, _ctx: &Ctx| {
        Box::pin(async move {
            Some(User { id, name: "Ada".into() })
        })
    });
}).unwrap();

schema.object::<User>(|o| {
    o.expose(expose!(User::name));
});
```

The first `node` call installs:

```graphql
interface Node { id: ID! }
type Query {
  node(id: ID!): Node
  nodes(ids: [ID!]!): [Node]!
}
```

Every later `node` call adds a type to the same dispatcher. The model implements `Node`, and an
`id: ID!` resolver returning the encoded global ID is added unless the object already declares an
`id` field. Existing fields and authorization rules are preserved.

Loaders:

- `load_one` / `load_one_result`: one local ID, returns `Option<M>`.
- `load_many` / `load_many_result`: many local IDs, returns `Vec<Option<M>>` in request order.
- If only one of the two is supplied, the other is derived from it.
- `Box::pin(async move { ... })` is required so the future can borrow the request context.

Request cache:

```rust
use gurafi::RequestCache;
use gurafi_relay::Relay;

struct Ctx { cache: RequestCache }

let relay = Relay::<Ctx>::new().with_request_cache(|ctx: &Ctx| ctx.cache.clone());
```

With an accessor, `node`/`nodes` remember results by type and local ID for the rest of the
request. Without one, `nodes` still deduplicates IDs inside a single call.

### Local IDs

Bare local IDs are **rejected by default**. Global IDs are the only accepted form; a value that
is not valid global-ID Base64 is an `Invalid global ID` error. For a migration period you can opt
in to a fallback that tries the value against every registered node loader in registration order:

```rust
let relay = Relay::<Ctx>::new().allow_local_ids(true);
```

The fallback lets a client resolve a node type it did not name and costs one loader call per
registered type, so enable it only while clients still send local IDs. The setting is shared with
the installed root fields, so it can also be changed after a node is registered.

An ID that *is* valid global-ID Base64 but names an unregistered type is an error; it does not
silently fall back to a local ID.

### Duplicate registration

Register each GraphQL type once per `Relay`. A second registration of the same type returns
`RelayError` rather than creating duplicate root fields or interface entries.

## Connections

Connection types have explicit names. Register the connection first, then attach it to a field.

```rust
use gurafi::{Object, Schema, expose};
use gurafi_relay::{Connection, ConnectionArgs, Relay};

#[derive(Clone, Object)]
struct User { id: String }

#[derive(Clone, Object)]
struct Post { id: String, title: String }

struct Ctx { posts: Vec<Post> }

let mut schema = Schema::<Ctx>::new();
let mut relay = Relay::<Ctx>::new();

schema.object::<User>(|o| { o.expose(expose!(User::id)); });
schema.object::<Post>(|o| { o.expose(expose!(Post::title)); });

relay.object_connection::<User, Post, _>(
    &mut schema,
    "posts",
    "UserPostsConnection",
    |_user: &User, args: ConnectionArgs, ctx: &Ctx| {
        Connection::from_slice(&ctx.posts, &args, |post: &Post| post.id.clone())
    },
).unwrap();
```

This creates:

```graphql
type UserPostsConnection {
  edges: [UserPostsConnectionEdge!]!
  pageInfo: PageInfo!
  totalCount: Int
}

type UserPostsConnectionEdge {
  cursor: String!
  node: Post!
}

type PageInfo {
  hasNextPage: Boolean!
  hasPreviousPage: Boolean!
  startCursor: String
  endCursor: String
}
```

The node type `Post` must be registered as an object (as above); connections do not invent its
fields. A connection does not require its node type to implement `Node`.

Lower-level form when field declaration lives elsewhere:

```rust
use gurafi_relay::RelayObjectExt;

let connection = relay.connection::<Post>(&mut schema, "UserPostsConnection").unwrap();
schema.object::<User>(|o| {
    o.connection(&connection, "posts", |user: &User, args, ctx: &Ctx| {
        // ...
        # Ok(Connection::empty())
    });
});
```

`Relay::query_connection` does the same for a root field. Async variants are
`Relay::object_connection_async` / `ConnectionRef` + `RelayObjectExt::connection_async`.

### Pagination

`ConnectionArgs` contains `first`, `after`, `last`, `before`. `Connection::from_slice` applies
them in Relay order:

1. `after` and `before` restrict the range.
2. `first` takes from the front.
3. `last` takes from the back.

Cursors are opaque Base64 values produced by the cursor function:

```rust
let connection = Connection::from_slice(
    &posts,
    &args,
    |post: &Post| post.id.clone(), // Base64("p1"), ...
)?;
```

A cursor that is malformed, or is well-formed but not present in the list, is a GraphQL error.
`Connection::from_slice_by_index` uses `Base64("index")` instead, for lists whose order is stable.
`Cursor::encode` / `Cursor::decode` are public for custom cursor schemes.

Other helpers:

- `Connection::empty()`
- `Connection::new(edges, page_info, total_count)`
- `Connection::with_total_count(n)` / `without_total_count()`
- `Connection::from_iter(...)`
- `Edge::new(cursor, node)`

`totalCount` is set to the input length by default. Remove it with `without_total_count`.

## Mutations

```rust
use gurafi::{Input, Object, Schema, expose};
use gurafi_relay::{ClientMutationId, MutationConfig, Relay};

#[derive(Clone, Object)]
struct User { id: String, name: String }

#[derive(Input)]
#[graphql(name = "CreateUserInput")]
struct CreateUserInput { name: String }

struct Ctx;

let mut schema = Schema::<Ctx>::new();
let mut relay = Relay::<Ctx>::new();

schema.object::<User>(|o| {
    o.expose(expose!(User::id));
    o.expose(expose!(User::name));
});
schema.query(|q| { q.field("ok", |_: gurafi::NoArgs, _: &Ctx| true); });

relay.mutation::<User, CreateUserInput, _>(
    &mut schema,
    "createUser",
    |input, _ctx| Ok(User { id: "1".into(), name: input.name }),
).unwrap();
```

Generates:

```graphql
createUser(input: CreateUserInput!, clientMutationId: String): CreateUserPayload!
type CreateUserPayload { result: User!, clientMutationId: String }
```

`clientMutationId` modes:

```rust
use gurafi_relay::{ClientMutationId, MutationConfig};

MutationConfig::default(); // optional
MutationConfig::new().client_mutation_id(ClientMutationId::Omit);
MutationConfig::new().client_mutation_id(ClientMutationId::Required);
```

`MutationConfig` is passed to `Relay::mutation_with` /
`Relay::mutation_async_with`. `mutation` and `mutation_async` use the optional default.

## Registration order

1. Create one `Relay` per schema.
2. (Optional) register connection types before the object builder closure that attaches them.
3. Register Node types before or after declaring their other fields; reopening an object merges.
4. Register object types before `Schema::build`; `build` is the validation point.

## Limitations

- The Node interface and root fields are fixed as `Node`, `node` and `nodes`.
- Connection names are explicit, not generated from parent/field names. This keeps schema names
  stable across Rust refactors but differs from Pothos' automatic naming.
- `clientMutationId` is a sibling argument of `input`, not a member of the input object, because
  `gurafi` input object names are compile-time constants. The response shape matches Relay, but a
  Relay client that puts `clientMutationId` inside `variables.input` is not wire-compatible.
- Connections require the node type to be a concrete `ObjectModel` registered in the schema.
- Node and connection node types use their derived `ObjectModel::NAME`; ``Schema::object_ref``
  runtime names are used for the generated connection/payload types, not for node identity.
- `Connection::from_slice` clones items into edges; use cheap/`Clone` node values, or build edges
  directly for large payloads.

## Observability

`gurafi-relay` has an optional `tracing` feature that propagates to `gurafi` and `gql`:

```toml
[dependencies]
gurafi-relay = { version = "0.1.1", features = ["tracing"] }
```

It adds DEBUG spans for `relay.node`, `relay.nodes`, `relay.connection.page` and
`relay.mutation`, with requested/resolved/null/error counts, cache hits, loader calls, page-info
flags and edge counts. Cursor and local-ID values are never recorded. See
[`docs/plugins/tracing.md`](../../docs/plugins/tracing.md).

## Tests

```sh
cargo test -p gurafi-relay
cargo test --workspace
cargo test --workspace --all-features
```
