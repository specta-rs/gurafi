# suku + gurafi Axum example

A giraffe API built with `gurafi`, served over HTTP by `suku` on Axum.

```sh
cargo run -p example-axum
```

Open <http://127.0.0.1:3000/graphql> for GraphiQL.

## Queries

```graphql
{
  giraffes(limit: 2) {
    name
    heightMeters
    species
  }
}

{
  giraffe(name: "Melman") {
    name
  }
}
```

`whoami` is protected by field-level authorization. Send a bearer token from
GraphiQL's request headers or with `curl`:

```sh
curl http://127.0.0.1:3000/graphql \
  -H 'content-type: application/json' \
  -H 'authorization: Bearer secret' \
  -d '{"query":"{ whoami }"}'
```
