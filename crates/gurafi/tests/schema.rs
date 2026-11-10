//! Schema-shape tests: everything that can be checked by building a schema and inspecting its
//! type definitions, without executing a request.

#![allow(dead_code)]

use std::sync::Arc;

use gurafi::{
    Enum, FromValue, ID, Input, IntoValue, NoArgs, Object, Scalar, Schema, Union, expose,
};

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

#[derive(Input, Debug)]
#[graphql(description = "Paging and filtering options.")]
struct FriendsArgs {
    #[graphql(description = "Max results", default = 10)]
    limit: i32,
    after: Option<String>,
    #[graphql(deprecated = "Use `limit`.")]
    legacy_limit: Option<i32>,
}

#[derive(Input)]
#[graphql(name = "FilterInput")]
struct Filter {
    color: Option<Color>,
    nested: Option<NestedFilter>,
}

#[derive(Input)]
#[graphql(name = "NestedFilterInput")]
struct NestedFilter {
    text: String,
    #[graphql(default = true)]
    exact: bool,
}

#[derive(Input)]
struct IdArgs {
    id: ID,
}

fn default_limit() -> i32 {
    25
}

#[derive(Input)]
struct ComputedDefaults {
    #[graphql(default_with = "default_limit")]
    limit: i32,
}

/// The status of a ticket.
#[derive(Enum)]
#[graphql(name = "TicketStatus")]
enum Status {
    Open,
    #[graphql(deprecated)]
    Closed,
}

#[derive(Input)]
struct ScalarArgs {
    at: Date,
    colors: Option<Vec<Color>>,
}

#[derive(Enum)]
#[graphql(description = "Giraffe colours.")]
enum Color {
    #[graphql(name = "RED", description = "Hot.")]
    Red,
    #[graphql(name = "BLUE", deprecated = "Giraffes are not blue.")]
    Blue,
}

#[derive(Scalar, Clone, Debug, PartialEq)]
#[graphql(
    name = "Date",
    description = "An ISO-8601 date.",
    specified_by = "https://example.com/date"
)]
struct Date(String);

#[derive(Object)]
#[graphql(description = "A person.")]
struct User {
    name: String,
}

#[derive(Object)]
struct Post {
    title: String,
}

#[derive(Union)]
#[graphql(description = "Anything search can return.")]
enum SearchResult {
    User(User),
    Post(Post),
}

#[derive(Object)]
struct Parent {
    label: String,
}

#[derive(Object)]
struct Child {
    label: String,
}

fn giraffe() -> Giraffe {
    Giraffe {
        name: "Melman".into(),
        birthday: "2001-04-12".into(),
        height: 3.4,
    }
}

fn build_full_schema() -> gql::Schema {
    let mut schema = Schema::<Ctx>::new();
    schema.description("A zoo.");

    let node = schema.interface("Node", |i| {
        i.description("Anything with an id.").field::<ID>("id");
    });

    schema.object::<Giraffe>(|o| {
        o.description("Long necks.");
        o.implements(node);
        o.expose(expose!(Giraffe::name));
        o.expose(expose!(Giraffe::height as "heightMeters"));
        o.expose(expose!(Giraffe::height))
            .deprecated("Use heightMeters");
        o.field("id", |g: &Giraffe, _: NoArgs, _: &Ctx| {
            ID::new(g.name.clone())
        });
        o.field("age", |g: &Giraffe, _: NoArgs, _: &Ctx| {
            g.birthday.parse::<u16>().ok()
        })
        .description("Age in years");
        o.field_result("friends", |_: &Giraffe, _: FriendsArgs, _: &Ctx| {
            gurafi::Result::Ok(vec![giraffe()])
        })
        .description("Friends.");
        o.field_async("nameAsync", |g: &Giraffe, _: NoArgs, _: &Ctx| {
            Box::pin(async move { g.name.clone() })
        });
        o.field_result_async("slow", |_: &Giraffe, _: NoArgs, _: &Ctx| {
            Box::pin(async move { gurafi::Result::Ok(1i32) })
        });
        o.field("birthday", |g: &Giraffe, _: NoArgs, _: &Ctx| {
            Date(g.birthday.clone())
        });
        o.field("color", |_: &Giraffe, _: NoArgs, _: &Ctx| Color::Blue);
        o.field("filtered", |g: &Giraffe, _: Filter, _: &Ctx| g.name.clone());
    });

    schema.object::<User>(|o| {
        o.implements(node);
        o.expose(expose!(User::name));
        o.field("id", |u: &User, _: NoArgs, _: &Ctx| ID::new(u.name.clone()));
    });
    schema.object::<Post>(|o| {
        o.expose(expose!(Post::title));
    });

    let search = schema.union("SearchResult", |u| {
        u.description("Anything search can return.")
            .member::<User>()
            .member::<Post>();
    });

    schema.query(|q| {
        q.field("giraffe", |_: NoArgs, _: &Ctx| giraffe())
            .description("Find a giraffe.")
            .auth(|ctx: &Ctx| ctx.is_logged_in)
            .complexity(2);
        q.field("node", |_: NoArgs, _: &Ctx| giraffe())
            .as_abstract(node)
            .description("Resolved through an interface.");
        q.field("search", |_: NoArgs, _: &Ctx| {
            SearchResult::User(User {
                name: "Oscar".into(),
            })
        })
        .as_abstract(search);
        q.field("nodes", |_: NoArgs, _: &Ctx| vec![giraffe()])
            .as_abstract(node);
        q.field("parent", |_: NoArgs, _: &Ctx| Parent {
            label: "parent".into(),
        });
        q.field_result_async("giraffeById", |args: IdArgs, _: &Ctx| {
            Box::pin(async move {
                gurafi::Result::Ok(if args.id.as_str() == "1" {
                    Some(giraffe())
                } else {
                    None
                })
            })
        });
        q.field_result("scalars", |args: ScalarArgs, _: &Ctx| {
            gurafi::Result::Ok(format!(
                "{}:{}",
                args.at.0,
                args.colors.map_or(0, |colors| colors.len())
            ))
        });
    });

    // Registered after the query on purpose: `gql` resolves types by name at build time, so
    // registration order never matters.
    schema.object::<Parent>(|o| {
        o.expose(expose!(Parent::label));
        o.field("child", |_: &Parent, _: NoArgs, _: &Ctx| {
            Some(Child {
                label: "child".into(),
            })
        });
    });
    schema.object::<Child>(|o| {
        o.expose(expose!(Child::label));
        o.field("parent", |_: &Child, _: NoArgs, _: &Ctx| {
            Some(Parent {
                label: "parent".into(),
            })
        });
    });

    schema.mutation(|m| {
        m.field("noop", |_: NoArgs, _: &Ctx| true);
    });

    schema.subscription(|s| {
        s.field("colors", |_: NoArgs, _: &Ctx| {
            futures::stream::iter(vec![gurafi::Result::Ok(Color::Blue)])
        });
    });

    schema.build().expect("schema builds")
}

fn field<'a>(schema: &'a gql::Schema, object: &str, name: &str) -> &'a gql::Field {
    schema
        .object_type(object)
        .expect("object exists")
        .get_field(name)
        .expect("field exists")
}

#[test]
fn object_fields_are_inferred_from_rust_types() {
    let schema = build_full_schema();
    let giraffe = schema.object_type("Giraffe").expect("Giraffe registered");

    assert_eq!(giraffe.description.as_deref(), Some("Long necks."));
    assert_eq!(giraffe.interfaces, vec![Arc::from("Node")]);

    assert_eq!(field(&schema, "Giraffe", "name").ty.to_string(), "String!");
    assert_eq!(
        field(&schema, "Giraffe", "heightMeters").ty.to_string(),
        "Float!"
    );
}

#[test]
fn expose_supports_renaming_and_deprecation() {
    let schema = build_full_schema();
    let deprecated = field(&schema, "Giraffe", "height");
    assert_eq!(deprecated.ty.to_string(), "Float!");
    assert_eq!(
        deprecated.deprecation_reason.as_deref(),
        Some("Use heightMeters")
    );
}

#[test]
fn all_resolver_kinds_produce_the_expected_types() {
    let schema = build_full_schema();

    assert_eq!(field(&schema, "Giraffe", "age").ty.to_string(), "Int");
    assert_eq!(
        field(&schema, "Giraffe", "friends").ty.to_string(),
        "[Giraffe]!"
    );
    assert_eq!(
        field(&schema, "Giraffe", "nameAsync").ty.to_string(),
        "String!"
    );
    assert_eq!(field(&schema, "Giraffe", "slow").ty.to_string(), "Int!");
    assert_eq!(field(&schema, "Giraffe", "birthday").ty.to_string(), "Date");
    assert_eq!(field(&schema, "Giraffe", "color").ty.to_string(), "Color");
    assert_eq!(
        field(&schema, "Giraffe", "filtered").ty.to_string(),
        "String!"
    );
    assert_eq!(field(&schema, "Giraffe", "id").ty.to_string(), "ID!");
}

#[test]
fn interface_metadata_is_registered() {
    let schema = build_full_schema();
    let node = schema.interface_type("Node").expect("interface exists");
    assert_eq!(node.description.as_deref(), Some("Anything with an id."));
    assert_eq!(
        node.fields.get("id").expect("id field").ty.to_string(),
        "ID!"
    );
    let giraffe = schema.object_type("Giraffe").expect("Giraffe");
    assert!(schema.object_implements(giraffe, "Node"));
}

#[test]
fn arguments_are_derived_from_the_args_type() {
    let schema = build_full_schema();
    let friends = field(&schema, "Giraffe", "friends");
    assert_eq!(friends.args.len(), 3);
    assert_eq!(friends.args["limit"].ty.to_string(), "Int!");
    assert_eq!(
        friends.args["limit"].default_value,
        Some(gql::Value::Int(10))
    );
    assert_eq!(
        friends.args["limit"].description.as_deref(),
        Some("Max results")
    );
    assert_eq!(friends.args["after"].ty.to_string(), "String");
    assert_eq!(friends.args["after"].default_value, None);
    assert_eq!(
        friends.args["legacy_limit"].deprecation_reason.as_deref(),
        Some("Use `limit`."),
    );
    assert_eq!(friends.args["legacy_limit"].ty.to_string(), "Int");
}

#[test]
fn nested_input_objects_are_registered() {
    let schema = build_full_schema();
    let filter = schema
        .input_object_type("FilterInput")
        .expect("FilterInput registered by the field's args");
    assert_eq!(filter.fields["color"].ty.to_string(), "Color");
    assert_eq!(filter.fields["nested"].ty.to_string(), "NestedFilterInput");

    let nested = schema
        .input_object_type("NestedFilterInput")
        .expect("nested input registered");
    assert_eq!(nested.fields["text"].ty.to_string(), "String!");
    assert_eq!(nested.fields["exact"].ty.to_string(), "Boolean!");
    assert_eq!(
        nested.fields["exact"].default_value,
        Some(gql::Value::Boolean(true))
    );
}

#[test]
fn enum_metadata_is_registered() {
    let schema = build_full_schema();
    let color = schema.enum_type("Color").expect("Color registered");
    assert_eq!(color.description.as_deref(), Some("Giraffe colours."));
    assert_eq!(color.values["RED"].description.as_deref(), Some("Hot."));
    assert_eq!(
        color.values["BLUE"].deprecation_reason.as_deref(),
        Some("Giraffes are not blue.")
    );
}

#[test]
fn custom_scalars_are_registered_with_metadata() {
    let schema = build_full_schema();
    let date = schema.scalar_type("Date").expect("Date registered");
    assert_eq!(date.description.as_deref(), Some("An ISO-8601 date."));
    assert_eq!(
        date.specified_by_url.as_deref(),
        Some("https://example.com/date")
    );
}

#[test]
fn union_metadata_is_registered() {
    let schema = build_full_schema();
    let search = schema.union_type("SearchResult").expect("union registered");
    assert_eq!(
        search.description.as_deref(),
        Some("Anything search can return.")
    );
    assert_eq!(search.types, vec![Arc::from("User"), Arc::from("Post")]);
}

#[test]
fn abstract_output_types_keep_their_wrappers() {
    let schema = build_full_schema();
    assert_eq!(field(&schema, "Query", "node").ty.to_string(), "Node");
    assert_eq!(
        field(&schema, "Query", "search").ty.to_string(),
        "SearchResult"
    );
    assert_eq!(field(&schema, "Query", "nodes").ty.to_string(), "[Node]!");
}

#[test]
fn registration_order_does_not_matter() {
    let schema = build_full_schema();
    assert!(schema.object_type("Parent").is_some());
    assert!(schema.object_type("Child").is_some());
    assert_eq!(field(&schema, "Parent", "child").ty.to_string(), "Child");
    assert_eq!(field(&schema, "Child", "parent").ty.to_string(), "Parent");
}

#[test]
fn complexity_and_descriptions_are_recorded() {
    let schema = build_full_schema();
    let giraffe = field(&schema, "Query", "giraffe");
    assert_eq!(giraffe.description.as_deref(), Some("Find a giraffe."));
    assert!(giraffe.complexity.is_some());
}

#[test]
fn schema_roots_are_only_added_when_used() {
    let schema = build_full_schema();
    assert_eq!(schema.query_name().as_ref(), "Query");
    assert_eq!(schema.mutation_name().map(AsRef::as_ref), Some("Mutation"));
    assert_eq!(
        schema.subscription_name().map(AsRef::as_ref),
        Some("Subscription")
    );

    let mut no_mutation = Schema::<Ctx>::new();
    no_mutation.query(|q| {
        q.field("ok", |_: NoArgs, _: &Ctx| true);
    });
    let no_mutation = no_mutation.build().expect("schema builds");
    assert!(no_mutation.mutation_name().is_none());
    assert!(no_mutation.subscription_name().is_none());
}

#[test]
fn build_reports_unregistered_object_types() {
    #[derive(Object)]
    struct Lonely {
        name: String,
    }

    let mut schema = Schema::<Ctx>::new();
    schema.query(|q| {
        q.field("lonely", |_: NoArgs, _: &Ctx| Lonely {
            name: "alone".into(),
        });
    });

    let error = schema.build().expect_err("build must fail");
    assert!(
        error.errors().iter().any(|error| error
            .message()
            .contains("`Lonely` is used by a field but was never registered")),
        "unexpected errors: {error}"
    );
}

#[test]
fn a_second_object_declaration_merges_fields() {
    let mut schema = Schema::<Ctx>::new();
    schema.object::<User>(|o| {
        o.expose(expose!(User::name));
    });
    schema.object_fields::<User>(|o| {
        o.field("id", |u: &User, _: NoArgs, _: &Ctx| ID::new(u.name.clone()));
    });
    schema.query(|q| {
        q.field("ok", |_: NoArgs, _: &Ctx| true);
    });

    let schema = schema.build().expect("merged object builds");
    let user = schema.object_type("User").expect("User exists");
    assert!(user.get_field("name").is_some());
    assert!(user.get_field("id").is_some());
}

#[test]
fn build_requires_a_query_root() {
    let schema = Schema::<Ctx>::new();
    let error = schema
        .build()
        .expect_err("a schema without a query must fail");
    assert!(
        error
            .errors()
            .iter()
            .any(|error| error.message().contains("query root")),
        "unexpected errors: {error}"
    );
}

#[test]
fn input_args_coerce_with_defaults_and_options() {
    let object = gql::Object::from_fields([
        ("after", gql::Value::String(Arc::from("cursor"))),
        ("unknown", gql::Value::Boolean(true)),
    ]);
    let args = <FriendsArgs as gurafi::InputObject>::from_object(&object).expect("coerces");
    assert_eq!(args.limit, 10);
    assert_eq!(args.after.as_deref(), Some("cursor"));
    assert_eq!(args.legacy_limit, None);
}

#[test]
fn input_args_reject_bad_values() {
    let object = gql::Object::from_fields([("limit", gql::Value::String(Arc::from("nope")))]);
    let error = <FriendsArgs as gurafi::InputObject>::from_object(&object)
        .expect_err("a string is not an Int");
    assert!(
        error.message().contains("FriendsArgs.limit"),
        "unexpected error: {error}"
    );
}

#[test]
fn nested_input_objects_coerce() {
    let object = gql::Object::from_fields([("color", gql::Value::Enum(Arc::from("RED")))]);
    let filter = <Filter as gurafi::InputObject>::from_object(&object).expect("coerces");
    assert!(matches!(filter.color, Some(Color::Red)));
    assert!(filter.nested.is_none());
}

#[test]
fn enums_round_trip_values() {
    assert_eq!(
        Color::Blue.into_value().unwrap(),
        gql::Value::Enum(Arc::from("BLUE"))
    );
    assert!(matches!(
        Color::from_value(&gql::Value::Enum(Arc::from("RED"))).unwrap(),
        Color::Red
    ));
    assert!(Color::from_value(&gql::Value::Enum(Arc::from("GREEN"))).is_err());
    assert!(matches!(
        Color::from_value(&gql::Value::String(Arc::from("BLUE"))).unwrap(),
        Color::Blue
    ));
}

#[test]
fn scalars_round_trip_values() {
    let date = Date("2024-01-01".into());
    assert_eq!(
        date.clone().into_value().unwrap(),
        gql::Value::String(Arc::from("2024-01-01"))
    );
    assert_eq!(
        Date::from_value(&gql::Value::String(Arc::from("2024-01-01"))).unwrap(),
        date
    );
    assert!(Date::from_value(&gql::Value::Int(1)).is_err());
}

#[test]
fn ids_accept_strings_and_integers() {
    assert_eq!(ID::new("42").to_string(), "42");
    assert_eq!("42".parse::<ID>().unwrap(), ID("42".into()));
    assert_eq!(
        ID::from_value(&gql::Value::Int(42)).unwrap(),
        ID("42".into())
    );
    assert_eq!(
        ID::from_value(&gql::Value::String(Arc::from("42"))).unwrap(),
        ID("42".into())
    );
}

#[test]
fn output_type_inference_matches_the_documented_rules() {
    assert_eq!(gurafi::type_ref::<String>().to_string(), "String!");
    assert_eq!(gurafi::type_ref::<bool>().to_string(), "Boolean!");
    assert_eq!(gurafi::type_ref::<i8>().to_string(), "Int!");
    assert_eq!(gurafi::type_ref::<u16>().to_string(), "Int!");
    assert_eq!(gurafi::type_ref::<f64>().to_string(), "Float!");
    assert_eq!(gurafi::type_ref::<ID>().to_string(), "ID!");
    assert_eq!(
        gurafi::type_ref::<Vec<Option<String>>>().to_string(),
        "[String]!"
    );
    assert_eq!(
        gurafi::type_ref::<Option<Vec<String>>>().to_string(),
        "[String!]"
    );
    assert_eq!(
        gurafi::type_ref::<Option<Option<String>>>().to_string(),
        "String"
    );
    assert_eq!(gurafi::type_ref::<Box<String>>().to_string(), "String!");
}

#[test]
fn type_level_renames_and_doc_comments_work() {
    assert_eq!(<Status as gurafi::EnumModel>::NAME, "TicketStatus");
    assert_eq!(
        <Status as gurafi::EnumModel>::description(),
        Some("The status of a ticket.")
    );
    let values = <Status as gurafi::EnumModel>::values();
    let closed = values
        .iter()
        .find(|value| value.name.as_ref() == "Closed")
        .expect("Closed value");
    assert_eq!(
        closed.deprecation_reason.as_deref(),
        Some("No longer supported")
    );
}

#[test]
fn defaults_can_be_computed_with_a_function() {
    let fields = <ComputedDefaults as gurafi::InputObject>::fields().expect("fields");
    assert_eq!(fields[0].default_value, Some(gql::Value::Int(25)));
}

#[test]
fn interface_and_union_closures_use_statement_style() {
    let mut schema = Schema::<Ctx>::new();
    let node = schema.interface("Node", |i| {
        i.description("id").field::<ID>("id");
    });
    schema.union("Thing", |u| {
        u.member::<User>().member::<Post>();
    });
    schema.object::<User>(|o| {
        o.implements(node);
        o.field("id", |u: &User, _: NoArgs, _: &Ctx| ID::new(u.name.clone()));
        o.expose(expose!(User::name));
    });
    schema.object::<Post>(|o| {
        o.expose(expose!(Post::title));
    });
    schema.query(|q| {
        q.field("user", |_: NoArgs, _: &Ctx| User { name: "x".into() })
            .description("A user.")
            .auth(|_: &Ctx| true)
            .complexity(1);
    });
    let schema = schema.build().expect("schema builds");
    assert_eq!(
        schema.get_type("Thing").map(gql::Type::kind),
        Some(gql::TypeKind::Union)
    );
}

#[test]
fn root_names_can_be_overridden() {
    let mut schema = Schema::<Ctx>::new();
    schema.query_name("QueryRoot");
    schema.mutation_name("MutationRoot");
    schema.subscription_name("SubscriptionRoot");
    schema.query(|q| {
        q.field("ok", |_: NoArgs, _: &Ctx| true);
    });
    let schema = schema.build().expect("schema builds");
    assert_eq!(schema.query_name().as_ref(), "QueryRoot");

    let mut with_all_roots = Schema::<Ctx>::new();
    with_all_roots.query_name("QueryRoot");
    with_all_roots.mutation_name("MutationRoot");
    with_all_roots.subscription_name("SubscriptionRoot");
    with_all_roots.query(|q| {
        q.field("ok", |_: NoArgs, _: &Ctx| true);
    });
    with_all_roots.mutation(|m| {
        m.field("ok", |_: NoArgs, _: &Ctx| true);
    });
    with_all_roots.subscription(|s| {
        s.field("ticks", |_: NoArgs, _: &Ctx| {
            futures::stream::iter(vec![gurafi::Result::Ok(1i32)])
        });
    });
    let schema = with_all_roots.build().expect("schema builds");
    assert_eq!(
        schema.mutation_name().map(AsRef::as_ref),
        Some("MutationRoot")
    );
    assert_eq!(
        schema.subscription_name().map(AsRef::as_ref),
        Some("SubscriptionRoot")
    );
}

#[test]
fn schema_renders_sdl() {
    let schema = build_full_schema();
    let sdl = schema.sdl();
    assert!(sdl.contains("type Giraffe"), "SDL was:\n{sdl}");
    assert!(sdl.contains("interface Node"), "SDL was:\n{sdl}");
    assert!(sdl.contains("enum Color"), "SDL was:\n{sdl}");
    assert!(sdl.contains("union SearchResult"), "SDL was:\n{sdl}");
    assert!(sdl.contains("scalar Date"), "SDL was:\n{sdl}");
}

#[test]
fn duplicate_field_names_are_reported() {
    #[derive(Object)]
    struct Thing {
        name: String,
    }

    let mut schema = Schema::<()>::new();
    schema.object::<Thing>(|o| {
        o.expose(expose!(Thing::name));
        o.expose(expose!(Thing::name));
    });
    schema.query(|q| {
        q.field("thing", |_: NoArgs, _: &()| Thing { name: "x".into() });
    });
    let error = schema.build().expect_err("duplicate fields must fail");
    assert!(error.to_string().contains("more than once"), "{error}");
}

#[test]
fn different_rust_types_cannot_share_a_graphql_name() {
    #[derive(Enum)]
    #[graphql(name = "Colour")]
    enum First {
        Red,
    }

    #[derive(Enum)]
    #[graphql(name = "Colour")]
    enum Second {
        Blue,
    }

    #[derive(Object)]
    struct Thing;

    let mut schema = Schema::<()>::new();
    schema.object::<Thing>(|o| {
        o.field("first", |_: &Thing, _: NoArgs, _: &()| First::Red);
        o.field("second", |_: &Thing, _: NoArgs, _: &()| Second::Blue);
    });
    schema.query(|q| {
        q.field("thing", |_: NoArgs, _: &()| Thing);
    });
    let error = schema.build().expect_err("name collision must fail");
    assert!(
        error.to_string().contains("more than one Rust type"),
        "{error}"
    );
}

#[test]
fn as_abstract_must_match_the_resolvers_rust_type() {
    #[derive(Object)]
    struct Thing {
        name: String,
    }
    #[derive(Object)]
    struct Other {
        name: String,
    }

    let mut schema = Schema::<()>::new();
    let thing = schema.interface("Node", |i| {
        i.field::<String>("name");
    });
    schema.object::<Thing>(|o| {
        o.implements(thing);
        o.expose(expose!(Thing::name));
    });
    schema.object::<Other>(|o| {
        o.implements(thing);
        o.expose(expose!(Other::name));
    });
    let search = schema.union("Search", |u| {
        u.member::<Thing>().member::<Other>();
    });
    schema.query(|q| {
        // A String can never be a possible type of the `Search` union.
        q.field("broken", |_: NoArgs, _: &()| "not a model".to_owned())
            .as_abstract(search);
    });
    let error = schema
        .build()
        .expect_err("mismatched abstract type must fail");
    assert!(error.to_string().contains("not a possible type"), "{error}");
}
