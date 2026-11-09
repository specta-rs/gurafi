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

## Features

| Feature | Default | Description |
| --- | --- | --- |
| `serde` | yes | `serde::Serialize` for `Value`, `Object`, `GraphQLError` and `Response` |
| `tracing` | no | tracing spans around parsing and execution |
