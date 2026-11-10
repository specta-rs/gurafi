# Plugins

`gurafi` plugins are a small, build-time hook surface for cross-cutting concerns: tracing,
authorization, loaders, mocks and response extensions. A plugin sees every field once while the
schema is assembled and can change field metadata or wrap resolvers. There is no dynamic type
system and no per-request iteration over plugins.

The interface is Pothos-inspired:

```rust
use gurafi::{FieldMeta, Plugin};

pub trait Plugin<Ctx: 'static>: Send + Sync + 'static {
    fn field(&self, meta: &FieldMeta<'_>, field: &mut gql::Field) {}
    fn wrap_resolve(&self, meta: &FieldMeta<'_>, next: gql::ResolveFn) -> gql::ResolveFn { next }
    fn wrap_subscribe(&self, meta: &FieldMeta<'_>, next: gql::SubscribeFn) -> gql::SubscribeFn { next }
    fn configure(&self, builder: &mut gql::SchemaBuilder) {}
}
```

Register one with `Schema::plugin`:

```rust
use gurafi::{FieldMeta, NoArgs, Object, Plugin, Schema};

struct DescribeRoots;

impl Plugin<Ctx> for DescribeRoots {
    fn field(&self, meta: &FieldMeta<'_>, field: &mut gql::Field) {
        if meta.is_root {
            field.description = Some(format!("Root field `{}`", meta.field_name).into());
        }
    }
}

struct Ctx;

#[derive(Object)]
struct Giraffe {
    name: String,
}

let mut schema = Schema::<Ctx>::new();
schema.plugin(DescribeRoots);
schema.object::<Giraffe>(|o| {
    o.expose(gurafi::expose!(Giraffe::name));
});
schema.query(|q| {
    q.field("giraffe", |_: NoArgs, _: &Ctx| Giraffe {
        name: "Melman".into(),
    });
});
let schema = schema.build().expect("valid schema");
```

## Field metadata

`FieldMeta` describes one field at build time:

| Field | Meaning |
| --- | --- |
| `type_name` | Declaring type, or the configured root name (`Query`, `Mutation`, `Subscription`) |
| `field_name` | The GraphQL field name |
| `return_type` | Final return type after interface inheritance and type overrides |
| `args` | Final argument declarations, in order |
| `is_root` | True for query, mutation and subscription root fields |
| `is_subscription` | True only for subscription root fields |

The metadata is a snapshot. Each plugin's `field` hook receives a fresh snapshot, so a later
plugin sees changes made by an earlier one, and `wrap_resolve`/`wrap_subscribe` see the final
field.

`field` can change the description, deprecation reason, complexity estimate, directives,
arguments or type. `configure` runs once, after every type and root object is registered, so it
can add directives, extra types or schema-level directives. Types added directly by `configure`
are not visited by the field hooks because they did not come through the `Schema` builders.

## Coverage and ordering

Plugins are stored on the schema and applied once when it is built with `Schema::build`,
`build_with` or `build_clone`. This means:

- Registering a plugin before or after declaring fields is equivalent.
- Fields on reopened types, runtime-named types (`object_ref`), raw fields, and fields added in a
  later module are all covered.
- `build_clone` applies plugins to the clone, so repeated builds never stack wrappers on the
  original declaration.

Plugins run in registration order. Wrappers compose so that the first plugin registered is the
outermost one:

```text
first:before
  second:before
    resolver
  second:after
first:after
```

Interface fields have no resolvers, so only `field` runs for them. A raw field that has no
resolver is only given the default property resolver when a plugin actually wraps it; a plugin
that only changes metadata leaves the field untouched, so the executor keeps its leaf fast path.
With an empty plugin list nothing is installed and execution is unchanged.

## Authorization

Field-level `.auth(...)` is installed when a field is declared, before plugins are applied. A
plugin that calls `next` preserves it; a plugin that replaces the resolver (such as `MockPlugin`)
bypasses it. Object- and interface-level authorization is installed after plugins, so those rules
always run before the plugin chain.

Argument validation is part of the schema resolver. A wrapping plugin that calls `next` runs it
exactly once; a plugin that replaces the resolver skips it.

## Built-ins

### `TracingPlugin`

`TracingPlugin` is dependency-free: it emits a `Start` and `Finish` event for every field
resolution and every subscription factory call. Bridge the observer to `tracing`, OpenTelemetry or
a metrics registry:

```rust
use gurafi::{Schema, TraceEvent, TracePhase, TracingPlugin};

struct Ctx;

let mut schema = Schema::<Ctx>::new();
schema.plugin(TracingPlugin::new(|event: &TraceEvent<'_>| {
    if event.phase == TracePhase::Start {
        // tracing::info_span!("graphql.field", type = event.type_name, field = event.field_name);
        eprintln!("start {}.{}", event.type_name, event.field_name);
    } else {
        eprintln!(
            "finish {}.{} in {:?} (failed: {})",
            event.type_name,
            event.field_name,
            event.elapsed.unwrap_or_default(),
            event.failed
        );
    }
}));
```

`TracingPlugin::stderr()` is a convenience for local debugging. There is no span guard to leak:
the observer runs synchronously inside the field future, so a dropped future simply emits no
`Finish` event.

### `ResponseExtensionPlugin`

Writes a fixed value into the response's `extensions` from a resolver wrapper:

```rust
use gurafi::{NoArgs, ResponseExtensionPlugin, Schema};

struct Ctx;

let mut schema = Schema::<Ctx>::new();
schema.plugin(ResponseExtensionPlugin::new("servedBy", "gurafi").root_only());
schema.query(|q| {
    q.field("ok", |_: NoArgs, _: &Ctx| true);
});
```

Without `root_only()` the extension is written for every field resolution. The key is the same
on every write, so the last write wins. For a plugin that accumulates per-field data, implement
`Plugin` directly and call `execution.context.set_extension` from the wrapper.

### `MockPlugin`

Replaces exact `Type.field` resolvers with fixed values for tests and local development:

```rust
use gurafi::{gql, MockPlugin, NoArgs, Object, Schema};

struct Ctx;

#[derive(Object)]
struct Giraffe {
    name: String,
}

let mut schema = Schema::<Ctx>::new();
schema.plugin(
    MockPlugin::new()
        .mock("Giraffe", "name", "Mocked")
        .mock_path("Query.giraffe", gql::Value::external(Giraffe {
            name: "Melman".into(),
        })),
);
schema.object::<Giraffe>(|o| {
    o.expose(gurafi::expose!(Giraffe::name));
});
schema.query(|q| {
    q.field("giraffe", |_: NoArgs, _: &Ctx| Giraffe {
        name: "real".into(),
    });
});
```

Only the named paths change; every other resolver runs normally. A mock skips the original
resolver, its argument validator and its field-level auth, but object- and interface-level auth
still runs. A mocked subscription keeps its real stream and returns the mock for every event.

## Writing a custom plugin

A plugin is just a value. Use `Arc` for shared state because `Schema` is `Clone` and plugins may
be shared across builds:

```rust
use std::sync::{Arc, Mutex};

use gurafi::{FieldMeta, Plugin, Schema};

#[derive(Clone)]
struct CountFields {
    paths: Arc<Mutex<Vec<String>>>,
}

impl Plugin<Ctx> for CountFields {
    fn field(&self, meta: &FieldMeta<'_>, _field: &mut gql::Field) {
        self.paths.lock().unwrap().push(meta.path());
    }
}

struct Ctx;

let paths = Arc::new(Mutex::new(Vec::new()));
let mut schema = Schema::<Ctx>::new();
schema.plugin(CountFields {
    paths: Arc::clone(&paths),
});
schema.query(|q| {
    q.field("ok", |_: gurafi::NoArgs, _: &Ctx| true);
});
let _schema = schema.build().expect("valid schema");
assert_eq!(paths.lock().unwrap().as_slice(), ["Query.ok"]);
```

For scopes and auth, a plugin can inspect `meta.type_name` and return a resolver that checks the
request context before calling `next`:

```rust
use std::sync::Arc;

use gurafi::{FieldMeta, GraphQLError, Plugin};

struct RequireScope(&'static str);

impl Plugin<Ctx> for RequireScope {
    fn wrap_resolve(&self, _meta: &FieldMeta<'_>, next: gql::ResolveFn) -> gql::ResolveFn {
        let scope = self.0;
        Arc::new(move |context| {
            let next = Arc::clone(&next);
            Box::pin(async move {
                let ctx = context.context.require_data::<Ctx>()?;
                if !ctx.scopes.iter().any(|granted| granted == scope) {
                    return Err(GraphQLError::new(format!("missing scope `{scope}`")));
                }
                next(context).await
            })
        })
    }
}
```

For loaders, put the loader on the request context as described in the batching section of the
crate documentation. A plugin is useful when the same loader or scope rule must be installed
uniformly across many fields; per-field logic belongs in the field resolver itself.
