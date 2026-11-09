# Defining a schema

`gurafi` is the high-level schema builder. It turns Rust types into a `gql::Schema`
while keeping the specification engine separate.

## Objects

Derive `Object` for a model type. The Rust type name is the GraphQL name unless
`#[graphql(name = "...")]` is used. A doc comment becomes the description.

```rust
use gurafi::{Object, Schema, expose};

#[derive(Object)]
/// Long necks, cool patterns, taller than you.
struct Giraffe {
    name: String,
    height: f32,
}

let mut schema = Schema::<()>::new();

schema.object::<Giraffe>(|o| {
    o.expose(expose!(Giraffe::name));
    o.expose(expose!(Giraffe::height as "heightMeters"));
});
```

Fields are declared explicitly. This is where resolvers, arguments, authorization
and complexity estimates live, so keeping the declaration next to the code beats
deriving field lists automatically.

### Resolvers

Object fields receive the model, their arguments and the context:

```rust
o.field("age", |g: &Giraffe, _: gurafi::NoArgs, _: &Ctx| {
    g.birthday.parse::<u16>().ok()
})
.description("Age in years");
```

Four variants exist:

| Method | Sync/async | Failure |
| --- | --- | --- |
| `field` | sync | infallible |
| `field_result` | sync | `Result` |
| `field_async` | async | infallible |
| `field_result_async` | async | `Result` |

Root fields omit the model argument and receive `(Args, &Ctx)`.

### Field options

`field` and `expose` return `FieldConfig`, which applies its options when the
statement ends:

```rust
o.expose(expose!(Giraffe::height))
    .description("Height in meters.")
    .deprecated("Use heightMeters.")
    .complexity(2)
    .auth(|ctx: &Ctx| ctx.user.is_some())
    .auth_error("Sign in to read heights.");
```

Authorization is per field. Authentication belongs to the server: it validates
the request and puts the user into the context passed to
`gql::ExecutionOptions`, exactly as `suku`'s context extractor does.

## Arguments and input objects

A resolver's second parameter is its arguments type. Derive `Input` and the
GraphQL arguments follow automatically; there is no second declaration.

```rust
use gurafi::Input;

#[derive(Input)]
struct FriendsArgs {
    #[graphql(description = "Max results", default = 10)]
    limit: i32,
    after: Option<String>,
}

o.field_result("friends", |g: &Giraffe, args: FriendsArgs, _: &Ctx| {
    gurafi::Result::Ok(fetch_friends(g, args.limit, args.after))
});
```

Use `NoArgs` when a field takes no arguments.

## Enums, scalars and IDs

```rust
#[derive(gurafi::Enum)]
enum Color {
    #[graphql(name = "RED")]
    Red,
    Blue,
}

#[derive(gurafi::Scalar)]
#[graphql(name = "Slug", specified_by = "https://example.com/slug")]
struct Slug(String);
```

`ID` is a newtype with its own `ID!` mapping. A custom scalar that cannot delegate
to an inner type implements `ScalarValue` and invokes `impl_scalar_traits!`.

## Interfaces and unions

Interfaces are declared once and implemented by objects. Interface fields have no
resolvers; execution uses the concrete object's field resolvers.

```rust
let node = schema.interface("Node", |i| {
    i.description("Anything with an id.").field::<gurafi::ID>("id");
});

schema.object::<User>(|o| {
    o.implements(node);
    o.expose(expose!(User::id));
    // ...
});
```

Unions list their members, and fields can declare an abstract return type:

```rust
let search = schema.union("SearchResult", |u| {
    u.member::<User>().member::<Post>();
});

schema.query(|q| {
    q.field_result("search", |args: SearchArgs, ctx: &Ctx| {
        gurafi::Result::Ok(search_results(args, ctx))
    })
    .as_abstract(search);
});
```

Concrete models travel through the executor as `Value::External`; the object's
generated `is_type_of` check tells the engine which member it is.

## Roots and subscriptions

```rust
schema.query(|q| {
    q.field("giraffe", |_: NoArgs, _: &Ctx| Giraffe { /* ... */ });
});

schema.mutation(|m| {
    m.field_result("createGiraffe", |args: CreateArgs, ctx: &Ctx| {
        gurafi::Result::Ok(create(args, ctx))
    });
});

schema.subscription(|s| {
    s.field("giraffeBorn", |_: NoArgs, _: &Ctx| {
        events().map(|event| gurafi::Result::Ok(event))
    });
});
```

## Building

```rust
let schema: gql::Schema = schema.build().expect("valid schema");
```

`build` validates the schema with `gql`. Duplicate types, missing registrations,
invalid interface implementations and dangling references are reported together.

The result is a plain `gql::Schema`. It can be served by `suku` or any transport
built on `gql`.
