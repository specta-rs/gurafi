# Schema evolution

A GraphQL schema is an API. Changing it can break every client that already
ships a query, so `gql` can compare schemas before deployment.

## Diffing

```rust
let old = gql::Schema::from_sdl(include_str!("old.graphql"))?;
let new = gql::Schema::from_sdl(include_str!("new.graphql"))?;

let diff = gql::diff(&old, &new);
if !diff.is_compatible() {
    for change in diff.changes() {
        eprintln!("breaking: {change}");
    }
    std::process::exit(1);
}
```

`SchemaDiff` only reports changes that break existing operations. Additive changes
are considered compatible:

| Change | Breaking? |
| --- | --- |
| Add a type | no |
| Add an optional field argument | no |
| Add an enum value | no |
| Add an optional input field | no |
| Add a union member | no |
| Remove a type, field, argument, enum value, union member or directive | yes |
| Change a type's kind | yes |
| Narrow an output type (remove non-null, change list depth) | yes |
| Add a required argument or input field | yes |
| Change an input type so previously valid values become invalid | yes |
| Remove an implemented interface | yes |

Deprecating is the safe way to remove something: mark it `@deprecated`, ship for a
release, then remove it.

## Printing and snapshotting

`Schema::sdl()` prints the schema back to GraphQL SDL. Store it in version
control, review it in pull requests, and feed it to client code generators.

## SDL import

`Schema::from_sdl` builds a schema from SDL without resolvers. It is useful for
compatibility checks, introspection snapshots and tooling. Server schemas are
normally built with `gurafi` or `gql::SchemaBuilder`, which attach resolvers.

## Introspection

All schema metadata is visible through standard introspection: descriptions,
deprecation reasons and text, `specifiedByURL`, interfaces, possible types,
default values and `@oneOf` input objects. Client generators do not need any
Gurafi-specific protocol.
