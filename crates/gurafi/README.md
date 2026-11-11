# gurafi

A type-safe, code-first GraphQL schema builder for Rust.

`gurafi` sits on top of [`gql`](../gql), which implements the GraphQL specification. `gql` knows
about the specification; `gurafi` knows about your Rust types. The result is the part you
actually write: Rust types drive GraphQL types, field arguments come from a Rust `Args` struct,
and most mistakes are compile errors instead of runtime schema errors.

```rust
use gurafi::{Enum, ID, Input, NoArgs, Object, Result, Schema, expose};

struct Ctx {
    is_logged_in: bool,
}

#[derive(Object)]
#[graphql(description = "A long-necked animal.")]
struct Giraffe {
    name: String,
    birthday: String,
    height: f32,
}

#[derive(Input)]
struct FriendsArgs {
    #[graphql(description = "Max results", default = 10)]
    limit: i32,
    after: Option<String>,
}

#[derive(Enum)]
enum Color {
    #[graphql(name = "RED")]
    Red,
    Blue,
}

let mut schema = Schema::<Ctx>::new();

let node = schema.interface("Node", |i| {
    i.description("Anything with an id").field::<ID>("id");
});

schema.object::<Giraffe>(|o| {
    o.description("Long necks.");
    o.implements(&node);
    o.expose(expose!(Giraffe::name));
    o.expose(expose!(Giraffe::height)).deprecated("Use heightMeters");
    o.field("id", |g: &Giraffe, _: NoArgs, _: &Ctx| ID::new(g.name.clone()));
    o.field("age", |g: &Giraffe, _: NoArgs, _: &Ctx| {
        g.birthday.parse::<u16>().ok()
    })
    .description("Age in years");
    o.field_result("friends", |_: &Giraffe, args: FriendsArgs, _: &Ctx| {
        Result::Ok(vec![])
    });
    o.field_async("nameAsync", |g: &Giraffe, _: NoArgs, _: &Ctx| {
        Box::pin(async move { g.name.clone() })
    });
    o.field_result_async("slow", |_: &Giraffe, _: NoArgs, _: &Ctx| {
        Box::pin(async move { Result::Ok(1) })
    });
});

schema.query(|q| {
    q.field("giraffe", |_: NoArgs, _: &Ctx| Giraffe {
        name: "Melman".into(),
        birthday: "2001-04-12".into(),
        height: 3.4,
    })
    .auth(|ctx: &Ctx| ctx.is_logged_in)
    .complexity(2);
});

let gql_schema: gql::Schema = schema.build().expect("valid schema");
```

A complete runnable example lives in [`examples/zoo.rs`](examples/zoo.rs).

## Design

- **Rust types are the schema.** `#[derive(Object)]`, `#[derive(Input)]`, `#[derive(Enum)]`,
  `#[derive(Scalar)]` and `#[derive(Union)]` register a Rust type as a GraphQL type. A field's
  output type is inferred from its Rust return type, and its arguments are inferred from its
  Rust arguments type. There is no second list of argument declarations to keep in sync.
  `#[derive(ErrorUnion)]` gives a fallible field a generated GraphQL result union.
- **Resolvers have one shape.** Object fields receive `(&Model, Args, &Ctx)`. Root fields receive
  `(Args, &Ctx)`. Each has a sync, async, infallible and fallible variant.
- **Runtime coercion errors are per-field GraphQL errors.** Bad client input never panics and
  never reaches your model conversion unsafely; it becomes an error in the response.
- **Name-based recursion.** `gql` resolves type references by name when the schema is built, so
  a field may return a type registered later. `build()` reports any type that was referenced but
  never registered, with the type name and what to do about it.
- **Build-time work, not request-time work.** Type registration, argument metadata and resolver
  wrappers are all constructed once. A resolver is an `Arc`ed closure; the only per-field
  allocation at request time is the boxed future `gql` requires.

## Type mapping

The table is implemented by `OutputType` (and mirrored by `InputType`):

| Rust type | GraphQL type |
| --- | --- |
| `String`, `&str`, `Cow<str>` | `String!` |
| `bool` | `Boolean!` |
| `i8`, `i16`, `i32`, `u8`, `u16` | `Int!` |
| `f32`, `f64` | `Float!` |
| `ID` | `ID!` |
| `Option<T>` | `T`, with the outer non-null removed |
| `Vec<T>` | `[T]!`, keeping `T`'s own nullability |
| `Box<T>`, `Arc<T>` | `T` |
| derived object, enum, scalar, union | its GraphQL name (nullable) |

`Vec<Option<String>>` is `[String]!` and `Option<Vec<String>>` is `[String!]`. A bare object or
named type is nullable; wrap it in `Option` semantics only when you need `null` to be legal in
GraphQL. Non-empty lists are not enforced.

`Result<T, E>` is deliberately **not** an `OutputType`: a fallible field is declared with
`field_result` / `field_result_async`, which tells the schema builder that failure is possible.

## Typed error result unions

Some failures are part of an API rather than exceptional. Derive `ErrorUnion` on an enum of
payload objects and declare the field with `field_result_union`; the schema gets a generated
union of a success payload and each error payload, the same shape as Pothos's Errors plugin.

```rust
use gurafi::{ErrorUnion, Input, NoArgs, Object, Schema};

#[derive(Object)]
struct User {
    name: String,
}

#[derive(Object)]
struct ConflictError {
    message: String,
    code: String,
}

#[derive(ErrorUnion)]
enum CreateUserError {
    Conflict(ConflictError),
}

#[derive(Input)]
struct CreateUserArgs {
    name: String,
}

let mut schema = Schema::<()>::new();
schema.object::<User>(|o| {
    o.field("name", |u: &User, _: NoArgs, _: &()| u.name.clone());
});
schema.object::<ConflictError>(|o| {
    o.field("message", |e: &ConflictError, _: NoArgs, _: &()| e.message.clone());
    o.field("code", |e: &ConflictError, _: NoArgs, _: &()| e.code.clone());
});
schema.query(|q| {
    q.field("ok", |_: NoArgs, _: &()| true);
});
schema.mutation(|m| {
    m.field_result_union::<User, CreateUserError, CreateUserArgs, _>(
        "createUser",
        |args: CreateUserArgs, _: &()| {
            if args.name.is_empty() {
                Err(CreateUserError::Conflict(ConflictError {
                    message: "Name is required.".into(),
                    code: "NAME_REQUIRED".into(),
                }))
            } else {
                Ok(User { name: args.name })
            }
        },
    );
});
```

That produces:

```graphql
type MutationCreateUserSuccess { data: User! }
union MutationCreateUserResult = MutationCreateUserSuccess | ConflictError
type Mutation { createUser(name: String!): MutationCreateUserResult! }
```

`Ok(T)` becomes the success payload and `Err(E)` becomes the matching error payload; neither is a
GraphQL `errors` entry. The success `data` field uses `T`'s type (`Option<T>` stays nullable,
lists keep their wrappers, and an abstract `T` keeps its abstract type). Error payload objects are
registered like any other object with `schema.object::<M>(...)`; forgetting one is a build error
that names the missing type. Sync, async, root, object and subscription fields are supported.
See [`docs/plugins/errors.md`](../../docs/plugins/errors.md) for names, limitations and how this
relates to error masking.

## Objects

```rust
#[derive(Object)]
#[graphql(name = "Giraffe", description = "A long-necked animal.")]
struct Giraffe {
    name: String,
    height: f32,
}
```

The Rust type name is the GraphQL name unless overridden. A doc comment becomes the description.
Fields are **not** derived: declaring them explicitly keeps the resolver, arguments and access
rules next to each other. The derive implements `ObjectModel` (name and description),
`OutputType`, and `IntoValue` (the model travels through the executor as `Value::External`).

Fields can be added in four flavours (each with a `*_with_info` form), plus `expose`:

```rust
o.field("name", |g: &Giraffe, _: NoArgs, _: &Ctx| g.name.clone());
o.field_result("name", |g, _: NoArgs, _| Result::Ok(g.name.clone()));
o.field_async("name", |g, _: NoArgs, _| Box::pin(async move { g.name.clone() }));
o.field_result_async("name", |g, _: NoArgs, _| {
    Box::pin(async move { Result::Ok(g.name.clone()) })
});
```

Calling `schema.object` again for the same model reopens it and adds fields, which lets a type
span modules. `object_fields` says the same thing explicitly; `object_field` adds one field
without a closure. `interface`/`interface_fields` and the root `*_fields` methods behave the
same way. Declarations merge; duplicate field names are still build errors.

```rust
schema.object_fields::<Giraffe>(|o| {
    o.field("heightMeters", |g: &Giraffe, _: NoArgs, _: &Ctx| g.height);
});
```

`expose!` names a model property once and infers both the GraphQL field name and its type:

```rust
o.expose(expose!(Giraffe::name));                      // field `name`
o.expose(expose!(Giraffe::height as "heightMeters"));   // custom name
o.expose(expose!(Giraffe::keeper::nickname));           // field `nickname`
```

The accessor clones the final value, so the property must be `Clone`. The GraphQL name is the
last Rust path segment verbatim; this crate does not silently change casing.

### Asynchronous resolvers

Async resolvers return a boxed future, written as `Box::pin(async move { ... })`. This is a Rust
language limitation rather than a design choice: an `Fn` bound cannot express a return type that
borrows from the closure's arguments, so the future has to be boxed at the call site. `gql`
boxes resolver futures anyway, so no extra allocation is added.

## Arguments and input objects

Every object and root resolver's second parameter is its arguments type. It must implement
`InputObject`, which `#[derive(Input)]` (or its `#[derive(Args)]` alias) provides. Use `NoArgs`
(an alias for `()`) when there are no arguments. A field's GraphQL arguments are exactly that
type's fields:

```rust
#[derive(Args)]
struct FriendsArgs {
    #[graphql(description = "Max results", default = 10)]
    limit: i32,
    after: Option<String>,
    #[graphql(deprecated = "Use `after`.")]
    before: Option<String>,
}
```

An `Input` type is **args-only by default**: it produces
`friends(limit: Int! = 10, after: String, before: String)`, not a named
`input FriendsArgs`. A type used inside another input must be named:

```rust
#[derive(Input)]
#[graphql(name = "FilterInput")]
struct Filter {
    prefix: Option<String>,
}

#[derive(Args)]
struct SearchArgs {
    filter: Option<Filter>, // FilterInput
}
```

Using an unnamed input nested inside another is a build error that names the fix.

Attribute support:

| Attribute | Meaning |
| --- | --- |
| `#[graphql(name = "...")]` | Make the type a reusable named input object, or rename a field. |
| `#[graphql(description = "...")]` | Set a description (doc comments also work). |
| `#[graphql(default = expr)]` | Default value, typed as the field's Rust type. |
| `#[graphql(default_with = "path")]` | Default computed by calling `path()`. |
| `#[graphql(deprecated)]` / `#[graphql(deprecated = "reason")]` | Deprecate a field. |
| `#[graphql(validate = "path")]` | Validate after coercion; `path` receives `&Self` on the type or `&FieldType` on a field. |
| `#[graphql(one_of)]` | Register a named input as a `@oneOf` input. |

`Option<T>` fields are optional. Every other field without a default is required. Nested input
objects, enums, custom scalars and lists work directly (`Option<Vec<Color>>`,
`Option<FilterInput>`, ...). Input objects can be self-referential through a nullable or list
field; the registration step detects and stops re-entry.

Validation functions return `Result<(), gql::GraphQLError>`; failures become normal field errors.
For rules that need the request context, chain `.validate::<Args>(|args, ctx| ...)` on the field.
A `@oneOf` input requires every field to be `Option<T>`, forbids defaults, and accepts exactly one
non-null field per value.

## Field options

Every `field*` and `expose` call returns a `FieldConfig` that applies its options at the end of
the statement:

```rust
q.field("giraffe", ...)
    .description("Find a giraffe.")
    .deprecated("Use `giraffeById`.")
    .complexity(2)
    .auth(|ctx: &Ctx| ctx.is_logged_in)
    .auth_error("Sign in to read giraffes.");
```

- `description` and `deprecated` are visible through introspection and the SDL.
- `complexity` sets the field's constant complexity estimate; `complexity_with` computes it from
  the coerced arguments.
- `auth` runs before the resolver, including before argument coercion. A denial is a normal
  GraphQL error, so it participates in null propagation: a nullable field becomes `null`, and a
  denial in a non-null field nulls its parent as the specification requires. `auth_error` sets
  the client-visible message (default `"Not authorized"`). `auth_async` takes a boxed future.
- `as_abstract` declares the field's GraphQL type as an interface or union while the resolver
  still returns concrete model values. List and nullability wrappers are preserved, so
  `Vec<Giraffe>` with `as_abstract(node)` becomes `[Node]!`.
- `nullable`, `non_null` and `nullable_items` adjust the declared nullability. `non_null` on an
  `Option`-returning resolver is a build error.
- `as_type::<T>()` (alias `returns::<T>()`) replaces the GraphQL type and is checked at build:
  scalar-to-scalar, enum-to-enum, or a truthful abstract-type relationship. It cannot see runtime
  values, so a dishonest override still fails at execution time.

Object-level rules are declared on the builder with `o.auth(...)` and apply to every field in
every declaration for that model. Interface rules (`i.auth(...)`) and interface-field rules
(`i.field::<T>("f").auth(...)`) are inherited by implementing objects. All levels compose.

Authentication (who the caller is) belongs to the server: it validates credentials and passes
the result to every request as the context. Authorization (whether this caller may see this
field) is the `auth` predicate. The same mechanism works for root and nested fields.

Resolvers can receive `&gql::ResolveInfo<'_>` with the `field_with_info`,
`field_result_with_info`, `field_async_with_info` and `field_result_async_with_info` methods
(and the root equivalents). The info exposes the field name, parent type, return type, response
path and schema; `gql` does not currently expose the selection AST.

## Enums, scalars and IDs

```rust
#[derive(Enum)]
enum Color {
    #[graphql(name = "RED", description = "Hot.")]
    Red,
    #[graphql(name = "BLUE", deprecated = "Giraffes are not blue.")]
    Blue,
}

#[derive(Scalar, Clone)]
#[graphql(name = "Date", description = "An ISO-8601 date.", specified_by = "https://example.com/date")]
struct Date(String);

let id = ID::new("42");
```

GraphQL enum values are conventionally `SCREAMING_CASE`; the derive uses the Rust variant name
verbatim unless renamed, so rename variants (as above) when following the convention matters.

`#[derive(Scalar)]` works on a newtype and delegates conversion to its inner type. For a scalar
whose representation is not its inner type, implement `ScalarValue` by hand and add the
conversions with `impl_scalar_traits!`:

```rust
struct Slug(String);

impl gurafi::ScalarValue for Slug {
    const NAME: &'static str = "Slug";

    fn to_value(&self) -> Result<gql::Value, gql::GraphQLError> {
        Ok(gql::Value::String(self.0.clone().into()))
    }

    fn from_value(value: &gql::Value) -> Result<Self, gql::GraphQLError> {
        value.as_str()
            .map(|value| Self(value.to_owned()))
            .ok_or_else(|| gql::GraphQLError::new("Expected a string."))
    }
}

gurafi::impl_scalar_traits!(Slug);
```

Scalars and enums used by a field or argument are registered automatically; `schema.scalar::<T>()`
and `schema.enum_type::<T>()` register one that appears nowhere in a field signature.

`ID` is the GraphQL `ID` scalar: serialised as a string, accepted as a string or integer, and
implementing `Display`, `FromStr`, `Deref<Target = str>` and the conversion traits.

## Interfaces and unions

Interfaces are declared first and returned as a cloneable `InterfaceRef`:

```rust
let node = schema.interface("Node", |i| {
    i.description("Anything with an id.").field::<ID>("id");
});

schema.object::<Giraffe>(|o| {
    o.implements(&node);
    o.field("id", |g: &Giraffe, _: NoArgs, _: &Ctx| ID::new(g.name.clone()));
    // ...
});
```

Interface fields have no resolvers; execution uses the concrete object's field resolvers. `gql`
validates that every implementing object provides a compatible field and argument set when the
schema is built.

When the object should reuse the interface's type and arguments and only add a resolver, use
`resolve_inherited`. The build finds the matching field (including through interfaces that
implement other interfaces), copies its type and arguments, and checks that the resolver's args
type and return type agree:

```rust
o.resolve_inherited("id", |g: &Giraffe, _: NoArgs, _: &Ctx| ID::new(g.name.clone()));
```

Unions list their members explicitly, or a `#[derive(Union)]` enum provides the output type:

```rust
#[derive(Union)]
enum SearchResult {
    User(User),
    Post(Post),
}

let search = schema.union("SearchResult", |u| {
    u.description("Anything search can return.")
        .member::<User>()
        .member::<Post>();
});

q.field("search", |_: NoArgs, _: &Ctx| SearchResult::User(User { name: "Oscar".into() }))
    .as_abstract(&search);
```

Runtime type resolution uses the `is_type_of` check that the object derive installs: every object
model travels as `Value::External`, and the object type's check is a downcast to the model type.
Override it per abstract type with `resolve_type` on the interface or union builder if you need
custom resolution.

## Runtime-named types

Derived names are static, but some names are only known once the application builds the schema —
Relay's `UserPostsConnection` and `CreateUserPayload` are the common case. A reference separates
the GraphQL name from the Rust model: the model still drives `is_type_of` and resolvers, while
the name is owned runtime data.

```rust
let admin = schema.object_ref::<User>("AdminUser");
schema.implement(&admin, |o| {
    o.expose(expose!(User::name));
    o.field("id", |u: &User, _: NoArgs, _: &Ctx| u.name.clone());
});

schema.query(|q| {
    q.field_ref(&admin, "admin", |_: NoArgs, _: &Ctx| current_admin());
});
```

`object_ref` creates the object and `implement` reopens it, just like `object`/`object_fields`.
The same model and name merge; a different model claiming the name is a build error. References
are `Clone` (not `Copy`) and can be stored and reused. `interface_ref`/`implement_interface` and
`union_ref`/`implement_union` are the interface and union equivalents; a union adds runtime-named
members with `member_ref`.

`field_ref` replaces only the named type, preserving the wrappers of the resolver's Rust type:
`User` becomes `AdminUser`, `Vec<User>` becomes `[AdminUser]!`, and `Option<Vec<User>>` becomes
`[AdminUser]`. Use `field_ref_type` (or `FieldConfig::as_dynamic`) when the name is an interface
or union, or when the wrappers need to be explicit. Widening a runtime-named object to an
interface or union composes with `as_abstract`:

```rust
q.field_ref(&admin, "node", |_: NoArgs, _: &Ctx| current_admin())
    .as_abstract(&node);
```

Derived `object`, `interface` and `union` declarations are unchanged and now also return their
reference.

## Resolving a whole object at once

When an object already exists as a GraphQL value — cached JSON, a row serialised once, an
upstream response — use `Raw<T>` to hand it to the executor in one piece. `T` still determines
the schema type; the value is passed through unchanged.

```rust
use gurafi::{NoArgs, Object, Raw, RawObject};

#[derive(Object)]
struct Giraffe {
    name: String,
    height: f32,
}

schema.object::<Giraffe>(|o| {
    // No resolver: each field is read from the raw object map by name.
    o.raw_field::<String>("name");
    o.raw_field::<f32>("height");
});

q.field("giraffe", |_: NoArgs, _: &Ctx| {
    Raw::<Giraffe>::object(
        RawObject::new()
            .field("name", "Melman")
            .field("height", 3.4)
            .build(),
    )
});
```

`Raw<Vec<Giraffe>>::list([...])` resolves a list, and `Raw<Option<Giraffe>>` (or
`Raw::null()`) makes a nullable value. Nested objects are just another `RawObject` field.

For an interface or union field, add `.as_abstract(&node)` and either include
`RawObject::typename("Giraffe")` in the map or install `resolve_type` on the abstract type.
`gql` cannot infer the concrete type from a raw map, so one of the two is required.

The caller owns the map's correctness. Wrong shapes, missing non-null fields and a `null` in a
non-null position are normal field errors with normal null propagation — no panic — but they are
not schema-build errors. The exception is a custom scalar: its raw value is passed through
unchanged, so `ScalarValue::to_value` does not run. Use a real resolver when a custom scalar must
validate or normalise its output.

Resolving a whole object skips its per-field resolvers, so declare every selected field with
`raw_field`. Mixing a raw field with a resolver-backed field on the same object is a schema-build
error, because the resolver would receive a raw object map instead of the Rust model. An
unguarded raw field stays resolver-free; field, object and inherited interface authorization still
apply, and the builder installs a property resolver only when a guard needs one.

`raw_field` takes no arguments. For an interface field with arguments, or a concrete raw field
that should coerce and validate arguments, use
`o.raw_field_with::<String, Args>("name")`. A `validate` hook on plain `raw_field` is a build
error, since there is no resolver in which it could run.

Raw maps are trusted data. Keys beginning with `__gql_` are reserved for introspection and must
not come from user input: `RawObject` ignores them, and `Raw::new`/`Raw::object` strip them
recursively, so build a map from untrusted JSON with those constructors. Use
`RawObject::typename` (not `__gql_typename`) to choose the concrete type of an interface or union
value. `__typename` is preserved and therefore trusted: if untrusted data must not choose its own
type, set it from trusted data or resolve the type from checked data.

The tradeoff is speed versus accountability: per-field resolution gives typed models, argument
coercion, validation and independent field optimization. `Raw` skips those per-field costs, and
in exchange the resolver returning the object is the single place responsible for the whole
value.

## Subscriptions

```rust
schema.subscription(|s| {
    s.field("colors", |_: NoArgs, _: &Ctx| {
        futures::stream::iter(vec![Ok(Color::Blue)])
    });
});
```

The resolver returns a `Stream<Item = Result<T, gql::GraphQLError>> + Send + 'static`. It is
called once when the subscription starts; anything it borrows from the context must be cloned
first because the stream outlives the call (channels and `Arc`s are the usual choice). Each event
is the field's own value: the builder wraps it as the subscription root value and extracts it
again, matching the specification's event model.

When the event type differs from the field value, `field_map` takes a separate resolver:

```rust
s.field_map(
    "giraffeBorn",
    |_: NoArgs, _: &Ctx| events(),                  // Stream<Item = Result<BornEvent, _>>
    |event: &BornEvent, _: NoArgs, _: &Ctx| Ok(event.giraffe.clone()),
);
```

`field_with_info` passes `&ResolveInfo` to the subscribe closure.

## Building and serving

```rust
let gql_schema: gql::Schema = schema.build()?;
```

`build` collects every problem it can find: duplicate type names, fields referencing an object
that was never registered, missing unions, invalid interface implementations, invalid input
cycles, invalid defaults, dishonest type overrides and inherited fields whose resolvers do not
match their interface. `SchemaError::errors()` gives the individual messages.

`Schema` is `Clone`, so the declaration survives building. `build_with(BuildOptions)` sets root
descriptions and stable type ordering; `build_clone()` builds from a clone:

```rust
let schema = schema
    .build_with(BuildOptions::new().query_description("Root fields.").sort_types(true))?;
```

The resulting `gql::Schema` is the same value the rest of the ecosystem uses. A server executes
requests with:

```rust
let options = gql::ExecutionOptions::with_context(Ctx { /* ... */ });
let response = gql::execute(&gql_schema, gql::Request::new(query), &options).await;
```

Root type names default to `Query`, `Mutation` and `Subscription`; a root type is only added when
it has at least one field. Use `query_name`, `mutation_name` and `subscription_name` to override
them. `gql::Schema::query_type()`, `mutation_type()` and `subscription_type()` expose the roots
for server integration.

## Differences from Pothos and other Rust GraphQL crates

- Named types (objects, enums, scalars, unions) are nullable unless wrapped by the caller's
  schema; only the built-in scalar mappings are non-null by default. Wrap a field's return type
  in `Option` when `null` must be legal, or register the type with an explicit non-null
  non-null `OutputType` implementation if you need a different convention.
- GraphQL field names are the Rust names verbatim. Use `#[graphql(name = "...")]` or
  `expose!(Type::field as "camelCase")` to rename.
- Fallible resolvers use `gurafi::Result<T>` (i.e. `gql::GraphQLError`). Map other error types
  with `.map_err(Into::into)` or `?` before returning.
- Async resolvers use `Box::pin(async move { ... })` because of the language limitation described
  above.
- This crate is code-first and does not read SDL; `gql` owns parsing, validation, execution and
  SDL printing.

## Observability

The optional `tracing` feature instruments schema building, plugins, authorization, validation,
resolvers, the `Loader` and `RequestCache`:

```toml
[dependencies]
gurafi = { version = "0.1.2", features = ["tracing"] }
```

`gurafi.schema.build`, `gurafi.loader.load` and `gurafi.loader.batch` are the summary points;
per-resolver, auth, validator and cache events are TRACE. Loader spans record the key *type* and
cache hit/miss, never key values. See [`docs/plugins/tracing.md`](../../docs/plugins/tracing.md) for
the span tree and performance guidance.

## Workspace

- `crates/gql` — the GraphQL specification implementation (`gql` knows nothing about `gurafi`).
- `crates/gurafi` — this crate.
- `crates/gurafi-macros` — the derives.
