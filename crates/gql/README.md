# gql

A GraphQL reference implementation for Rust.

`gql` implements the [GraphQL October 2021 specification](https://spec.graphql.org/October2021/):

- lexer, parser and borrowed AST for executable documents and SDL
- the complete type system: scalars, objects, interfaces, unions, enums, input objects, directives
- full document validation
- execution with variables, arguments, aliases, fragments, directives, lists, null propagation,
  errors and response paths
- queries, mutations and subscriptions
- introspection (`__schema`, `__type`, `__typename`)
- query depth and complexity analysis
- prepared (persisted) queries that skip parsing and validation on every request
- schema printing, SDL import and backwards-compatibility diffing
- pluggable field authorization

It is deliberately transport- and framework-agnostic. `gql` knows nothing about HTTP, databases,
authentication or a particular high-level API. A schema is built with `SchemaBuilder`, or generated
by a higher-level crate such as [`gurafi`](https://docs.rs/gurafi), and served by a transport such as
[`suku`](https://docs.rs/suku).

```rust
use gql::{Field, ObjectType, Schema, TypeRef, Value};

let mut builder = Schema::builder();
builder.add_type(
    ObjectType::new("Query")
        .field(Field::new("hello", TypeRef::string().non_null()).resolve(|_| {
            Box::pin(async { Ok(Value::from("world")) })
        }))
        .into(),
);

let schema = builder.query("Query").build().expect("valid schema");
let response = futures::executor::block_on(gql::execute(
    &schema,
    gql::Request::new("{ hello }"),
    &gql::ExecutionOptions::default(),
));

assert_eq!(response.data.unwrap().as_object().unwrap().get("hello").unwrap().as_str(), Some("world"));
```

## Prepared queries

`gql::execute` parses and validates every document it receives, which is what an endpoint that
accepts arbitrary client documents needs. Once a document is trusted (for example, an operation
loaded from a build-time manifest), prepare it once and execute it many times:

```rust
# use gql::{ExecutionOptions, Schema};
# fn run(schema: &Schema) {
let prepared = gql::prepared::PreparedQuery::new(
    schema,
    "dashboard",
    "query Dashboard { hello }",
    None,
)
.expect("valid query");

let response = futures::executor::block_on(
    prepared.execute(None, &ExecutionOptions::default()),
);
# let _ = response;
# }
```

A prepared query owns its source and AST safely, is `Send + Sync`, and applies the same variables,
complexity limits, introspection policy, authorization, timeouts and null propagation as normal
execution. `PreparedQueryRegistry` builds a named set with duplicate-id detection, and
`gql::execute_prepared` is the free-function form of `PreparedQuery::execute`.

Preparation is done against one schema instance. Cloning a schema shares that instance, but two
separately built schemas do not compare equal: use [`Schema::ptr_eq`] when the distinction matters.
A prepared query stores its schema, so callers that dispatch prepared work next to a different
schema should check the instance first (`suku` does this for its servers). Complexity is checked
with a conservative bound computed at preparation; documents with field-level complexity
estimators fall back to the exact per-request analysis, because estimator results can depend on
request variables.

[`Schema::ptr_eq`]: https://docs.rs/gql/latest/gql/struct.Schema.html#method.ptr_eq

## Features

| Feature | Default | Description |
| --- | --- | --- |
| `serde` | yes | `serde::Serialize` for `Value`, `Object`, `GraphQLError` and `Response` |
| `tracing` | no | tracing spans around parsing and execution |
