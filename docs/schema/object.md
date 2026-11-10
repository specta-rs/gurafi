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

### Splitting a definition across modules

`object` reopens an object that already exists, so a type can be declared in
several places. The merge keeps descriptions, interfaces and the generated
`is_type_of` check. A field name declared twice is still a build error.

```rust
schema.object::<Giraffe>(|o| {
    o.expose(expose!(Giraffe::name));
});

// in another module, or later in the same one
schema.object_fields::<Giraffe>(|o| {
    o.field("height", |g: &Giraffe, _: NoArgs, _: &Ctx| g.height);
});

schema.object_field::<Giraffe, _, NoArgs, _>("age", |g: &Giraffe, _: NoArgs, _: &Ctx| {
    g.age
});
```

`interface`/`interface_fields`, `query`/`query_fields`, `mutation`/`mutation_fields`
and `subscription`/`subscription_fields` behave the same way. Roots already
append; the `*_fields` spellings just say so at the call site.

### Resolvers

Object fields receive the model, their arguments and the context:

```rust
o.field("age", |g: &Giraffe, _: gurafi::NoArgs, _: &Ctx| {
    g.birthday.parse::<u16>().ok()
})
.description("Age in years");
```

Four base variants exist; each has a `*_with_info` form:

| Method | Sync/async | Failure |
| --- | --- | --- |
| `field` | sync | infallible |
| `field_result` | sync | `Result` |
| `field_async` | async | infallible |
| `field_result_async` | async | `Result` |

Each has a `*_with_info` variant that appends `&gql::ResolveInfo<'_>` to the
closure, for field name, parent type, return type, response path and schema:

```rust
o.field_with_info("path", |g: &Giraffe, _: NoArgs, _: &Ctx, info| {
    format!("{}.{}", info.parent_type, info.field_name)
});
```

Root fields omit the model argument and receive `(Args, &Ctx)`; their
`*_with_info` variants work the same way.

When a resolver needs to write response `extensions`, use
`field_with_execution` / `field_result_with_execution` on objects and roots.
They append `&gql::ResolverContext<'_>`, which exposes the execution context,
coerced arguments and schema:

```rust
o.field_with_execution("name", |g: &Giraffe, _: NoArgs, _: &Ctx, execution| {
    execution.context.set_extension("servedBy", "gurafi");
    g.name.clone()
});
```

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

A rule for a whole type is declared once, on the builder:

```rust
schema.object::<Giraffe>(|o| {
    o.auth(|ctx: &Ctx| ctx.user.is_some())
        .auth_error("Sign in to see giraffes");
    // every field on this object, in every object_fields call, is guarded
});

let node = schema.interface("Node", |i| {
    i.auth(|ctx: &Ctx| ctx.user.is_some());
    i.field::<ID>("id").auth(|ctx: &Ctx| ctx.user.is_some());
    // interface authorization is inherited by implementing objects
});
```

`auth_async` takes a boxed future for checks that need I/O. All levels compose:
a field must pass its own rule, the object's rule and every implemented
interface's rule.

### Nullability and type overrides

The Rust return type gives the default GraphQL type. When the API needs a
different one, the builder checks it:

```rust
o.field("name", |g: &Giraffe, _: NoArgs, _: &Ctx| Some(g.name.clone()))
    .non_null();          // build error: the resolver can return None

o.field("name", |g: &Giraffe, _: NoArgs, _: &Ctx| g.name.clone())
    .nullable();          // String! becomes String

o.field("tags", |g: &Giraffe, _: NoArgs, _: &Ctx| g.tags.clone())
    .nullable_items();    // [String!]! becomes [String]!

o.field("date", |g: &Giraffe, _: NoArgs, _: &Ctx| g.date_string.clone())
    .returns::<Date>();   // checked replacement (as_type is an alias)
```

`as_type`/`returns` only accepts scalar-to-scalar, enum-to-enum, or a
relationship to an abstract type that is truthful about possible types. The
override is also checked against the resolver's Rust type: it may loosen
nullability (`String!` to `String`) but it cannot make a nullable position
non-null (`Option<String>` to `String!`, or `Vec<Option<String>>` to
`[String!]!`), and its list shape must match (`Vec<String>` cannot become
`String!`). All of those are build errors, not runtime surprises.

## Arguments and input objects

A resolver's second parameter is its arguments type. Derive `Input` (or `Args`,
an alias that says "arguments only") and the GraphQL arguments follow
automatically; there is no second declaration.

```rust
use gurafi::{Args, Input};

#[derive(Args)]
struct FriendsArgs {
    #[graphql(description = "Max results", default = 10)]
    limit: i32,
    after: Option<String>,
}

o.field_result("friends", |g: &Giraffe, args: FriendsArgs, _: &Ctx| {
    gurafi::Result::Ok(fetch_friends(g, args.limit, args.after))
});
```

`FriendsArgs` is **not** a named input type: it produces
`friends(limit: Int! = 10, after: String)`, not an extra `input FriendsArgs`.
An input type used inside another input must be named:

```rust
#[derive(Input)]
#[graphql(name = "FilterInput")]
struct Filter {
    color: Option<Color>,
}

#[derive(Args)]
struct SearchArgs {
    filter: Option<Filter>, // refers to FilterInput
}
```

Using an unnamed input nested inside another is a build error that names the
fix, rather than a leaked internal type.

Use `NoArgs` when a field takes no arguments.

### Validation

`#[graphql(validate = "path")]` runs after coercion. On a type it receives
`&Self`; on a field it receives `&FieldType`. Either returns
`Result<(), gql::GraphQLError>` and the failure becomes a field error:

```rust
fn valid_name(name: &str) -> Result<(), gql::GraphQLError> {
    (!name.trim().is_empty())
        .then_some(())
        .ok_or_else(|| gql::GraphQLError::new("name must not be empty"))
}

#[derive(Input)]
struct CreateArgs {
    #[graphql(validate = "valid_name")]
    name: String,
}
```

For rules that need the request context, validate on the field builder instead:

```rust
q.field("create", |args: CreateArgs, _: &Ctx| args.name)
    .validate::<CreateArgs>(|args, ctx| {
        if ctx.user.is_some() { Ok(()) } else {
            Err(gql::GraphQLError::new("sign in first"))
        }
    });
```

The type parameter must be the field's arguments type: `.validate::<OtherArgs>`
on a field that takes `CreateArgs` is a build error, so the validator can never
run with fabricated arguments. The hook runs after input-level validation and
before the resolver, and the arguments are coerced once (subscription events
reuse the arguments validated when the subscription started). Subscription
fields run the field validator once per event.

For a nested input field's default, either write the value inline or use a
derived input type directly; `#[derive(Input)]` implements `IntoValue`, so this
compiles:

```rust
#[derive(Input)]
#[graphql(name = "PageInput")]
struct Page { #[graphql(default = 20)] first: i32 }

#[derive(Input)]
struct ListArgs {
    #[graphql(default = Page { first: 3 })]
    page: Page,
}
```

### One-of inputs

`#[graphql(one_of)]` registers a `@oneOf` input. Every field must be
`Option<T>`, no field may have a default, and a caller must provide exactly one
non-null field:

```rust
#[derive(Input)]
#[graphql(name = "SearchBy", one_of)]
struct SearchBy {
    id: Option<ID>,
    name: Option<String>,
}
```

A one-of input must be an *input object*, so it cannot be used directly as a
field's arguments type: wrap it in an args type (`struct Args { by: SearchBy }`)
or the schema build fails. Derived type names may not shadow the built-in
scalars (`String`, `Int`, `Float`, `Boolean`, `ID`).

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

Interface authorization and authorization on interface fields are inherited by
implementing objects (see above). A field whose type and arguments come straight
from the interface needs only a resolver:

```rust
schema.object::<User>(|o| {
    o.implements(node);
    o.resolve_inherited("id", |u: &User, _: NoArgs, _: &Ctx| ID::new(u.name.clone()));
});
```

At build time the matching interface field (including through interfaces that
implement other interfaces) supplies the type, arguments, description,
deprecation reason and directives when the object did not set one, and the build
checks that the resolver's args type matches the interface declaration.

Interfaces implemented indirectly are added automatically: if `Resource`
implements `Node`, an object that implements `Resource` also gets `Node`
without repeating `o.implements(node)`. Reopening an interface or object with
another `auth(...)` rule composes with the earlier rule (all must allow); a
later `auth_error(...)` refines the message instead of replacing the rule.

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
generated `is_type_of` check tells the engine which member it is. `member::<M>()`
registers a reference to `M`, so forgetting `schema.object::<M>(...)` is
reported as the usual "never registered" build error.

## Resolving a whole object (raw values)

A field normally asks for one resolver per object field. When the data already exists as a
GraphQL value — cached JSON, a row serialised once, or an upstream response — that per-field
machinery is wasted work. `Raw<T>` is the escape hatch: it declares the GraphQL type from `T`
but hands the executor a prebuilt `gql::Value`.

Declare the raw object's fields with `raw_field`. No resolver is installed, so execution reads
the field from the object map by name:

```rust
use gurafi::{NoArgs, Object, Raw, RawObject, Schema};

struct Ctx;

#[derive(Object)]
struct Giraffe {
    name: String,
    height: f32,
    keeper: Keeper,
}

#[derive(Object)]
struct Keeper {
    name: String,
}

let mut schema = Schema::<Ctx>::new();

schema.object::<Keeper>(|o| {
    o.raw_field::<String>("name");
});

schema.object::<Giraffe>(|o| {
    o.raw_field::<String>("name");
    o.raw_field::<f32>("height");
    o.raw_field::<Keeper>("keeper");
});

schema.query(|q| {
    q.field("giraffe", |_: NoArgs, _: &Ctx| {
        Raw::<Giraffe>::object(
            RawObject::new()
                .field("name", "Melman")
                .field("height", 3.4)
                .field("keeper", RawObject::new().field("name", "Bob"))
                .build(),
        )
    });
});
```

`Raw<T>` implements `OutputType` by forwarding to `T`, so ordinary Rust types still drive the
schema. `Raw<Giraffe>` is a `Giraffe`, `Raw<Option<Giraffe>>` is nullable, and
`Raw<Vec<Giraffe>>` is `[Giraffe]!`. Build a list with `Raw::list` or wrap a `Value::List`
directly:

```rust
q.field("giraffes", |_: NoArgs, _: &Ctx| {
    Raw::<Vec<Giraffe>>::list([giraffe_a, giraffe_b])
});
```

`RawObject` is optional sugar over `gql::Object`. It accepts any `Into<Value>` for a field:
strings, numbers, booleans, `Option<T>`, `Value`, `gql::Object` or another `RawObject`.
`Raw::object` also takes a `gql::Object` directly. For nested lists, use a `Value::List`.

Raw maps are trusted data. Keys beginning with `__gql_` are private to `gql`'s introspection
runtime and are not user fields. `RawObject` ignores them, and `Raw::new` (and therefore
`Raw::object`) removes them at any depth, so decode untrusted JSON with those constructors rather
than building a `gql::Value::Object` by hand. An explicit `__typename` is the supported type
marker; `__gql_typename` is internal and is never trusted on a plain object.

`__typename` is preserved, which also means it is trusted. If a map comes from untrusted JSON and
the data must not choose its own runtime type, overwrite `__typename` from trusted data (for
example with `RawObject::typename`) or let `resolve_type` select the type from data the
application has checked.

`raw_field` has no arguments. To implement an interface field that takes arguments, or to have a
concrete raw field coerce and validate arguments, use `raw_field_with`:

```rust
#[derive(gurafi::Input)]
struct PictureArgs {
    width: i32,
}

schema.object::<Giraffe>(|o| {
    o.raw_field_with::<String, PictureArgs>("picture");
});
```

The executor coerces the arguments and runs any `validate` hook through the field's resolver,
then reads the value from the map by name. A `validate` hook on plain `raw_field` is a schema
build error, because that field has no resolver in which the hook could run.

### Interfaces and unions

An abstract output type needs to know the concrete object type for each value. Either put
`__typename` in the map (with `RawObject::typename`), or install `resolve_type` on the interface
or union. `resolve_type` can read any field of the raw map, including a field that is not part
of the schema.

```rust
let node = schema.interface("Node", |i| {
    i.field::<gurafi::ID>("id");
    i.field::<String>("name");
});

// The object implements Node, so it declares the same fields (as raw fields here).
schema.object::<Giraffe>(|o| {
    o.implements(node);
    o.raw_field::<gurafi::ID>("id");
    o.raw_field::<String>("name");
    o.raw_field::<f32>("height");
});

schema.query(|q| {
    q.field("node", |_: NoArgs, _: &Ctx| {
        Raw::<Giraffe>::object(
            RawObject::new()
                .typename("Giraffe")
                .field("id", "1")
                .field("name", "Melman")
                .field("height", 3.4)
                .build(),
        )
    })
    .as_abstract(node);
});
```

For a union, declare the union and add `.as_abstract(union)` to the field. If the union has a
`resolve_type` resolver it wins; otherwise `gql` reads `__typename` from the object map. The
fallback `is_type_of` checks cannot see a raw map's Rust type, so a raw abstract value without
either `__typename` or `resolve_type` is an execution error.

`__typename` on a concrete object field does not need to be in the map: `gql` derives it from
the type the executor completed the value as. An internal `__gql_typename` key cannot change
that, and cannot choose the type of an abstract value; only `__typename` or `resolve_type` can.

### Nullability and correctness

The caller owns the value. `gql` catches most mismatches at execution time, never by panicking:

- A non-object value for an object field is a field error.
- A missing non-null field propagates `null` according to the schema, exactly as it would from a
  resolver.
- A missing nullable field resolves to `null`.
- `Raw::<Giraffe>::null()` or a `Value::Null` in a non-null field is a normal non-null violation.

There is no runtime check that a `Value::String` really is what `Raw::<String>` claims, and no
per-field validation of the map. Custom scalars are the notable gap: a raw value for a custom
scalar is passed through unchanged, so `ScalarValue::to_value` does not run and a value the scalar
would reject can still reach the response. Use a normal resolver when a custom scalar must
validate or normalise its output, and keep the value and `T` in sync.

A raw object bypasses the object's resolvers, so every selected field must be declared with
`raw_field` or `raw_field_with`. Mixing a raw field with a resolver-backed field on the same
object is a schema build error: the resolver would receive the raw map where it expects the Rust
model.

### Tradeoff

Per-field resolution gives the executor typed model values, per-field authorization, argument
coercion and validation, and lets a field resolver be optimized independently. A raw object
trades all of that for one value construction: the executor still completes the selected
subtree, but no unguarded field runs a per-field resolver, model downcast or `IntoValue` call.
An unguarded raw object can even use the executor's synchronous property fast path, skipping the
per-field completion futures. Authorization is not lost: field-level `.auth(...)` on a
`raw_field`, object-level `auth` and inherited interface rules still apply, because the builder
installs a property resolver only when a guard needs one. A `raw_field_with` always has a
resolver (it coerces arguments), and a guarded raw field is wrapped by its guard. Keep `Raw` for
read paths where the object is already materialised; do not use it to skip a domain model that a
resolver needs.

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

A `field` stream yields the field's own values; the builder wraps each event as
the subscription root value and extracts it again. When the event carries more
than the field value, use `field_map` and supply the resolver:

```rust
s.field_map(
    "giraffeBorn",
    |_: NoArgs, _: &Ctx| events(),
    |event: &BornEvent, _: NoArgs, _: &Ctx| Ok(event.giraffe.clone()),
);
```

Subscription fields also accept `field_with_info`, which passes `&ResolveInfo`
to the `subscribe` closure.

## Building

```rust
let schema: gql::Schema = schema.build().expect("valid schema");
```

`build` validates the schema with `gql`. Duplicate types, missing registrations,
invalid interface implementations and dangling references are reported together.

`Schema` is `Clone`, so a builder can be kept after building. `build_with` takes
[`BuildOptions`] for root descriptions and stable type ordering, and
`build_clone` builds from a clone:

```rust
use gurafi::BuildOptions;

let options = BuildOptions::new()
    .query_description("Everything readable.")
    .sort_types(true);

let first = schema.clone().build_with(options)?;
let second = schema.build_clone()?;
```

The result is a plain `gql::Schema`. It can be served by `suku` or any transport
built on `gql`.
