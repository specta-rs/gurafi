//! Custom directives: definitions, applications on every named kind, validation and the paths
//! that must preserve them (SDL, introspection, merging, cloning and plugins).

#![allow(dead_code)]

use gql::{
    AppliedDirective, DirectiveDefinition, ExecutionOptions, InputValue, Request, Value,
    ast::DirectiveLocation,
};
use gurafi::{Enum, ID, Input, NoArgs, Object, Plugin, Scalar, Schema, Union, expose};

#[derive(Object)]
struct Giraffe {
    name: String,
}

#[derive(Object)]
struct User {
    name: String,
}

#[derive(Object)]
struct Post {
    title: String,
}

#[derive(Enum)]
enum Color {
    Red,
    Blue,
}

#[derive(Scalar, Clone, Debug, PartialEq)]
struct Date(String);

#[derive(Input)]
#[graphql(name = "FilterInput")]
struct Filter {
    name: Option<String>,
    limit: i32,
}

#[derive(Input)]
struct NameArgs {
    limit: i32,
}

#[derive(Object)]
struct NotFoundError {
    message: String,
}

#[derive(gurafi::ErrorUnion)]
enum LookupError {
    NotFound(NotFoundError),
}

fn tag(name: &str) -> AppliedDirective {
    AppliedDirective::new("tag").argument("name", name)
}

/// A repeatable `@tag` usable on every kind these tests attach it to.
fn tag_definition() -> DirectiveDefinition {
    DirectiveDefinition::new("tag")
        .description("A documentation tag.")
        .repeatable()
        .arg(InputValue::new("name", gql::TypeRef::string()))
        .location(DirectiveLocation::Object)
        .location(DirectiveLocation::Interface)
        .location(DirectiveLocation::Union)
        .location(DirectiveLocation::Scalar)
        .location(DirectiveLocation::Enum)
        .location(DirectiveLocation::InputObject)
        .location(DirectiveLocation::FieldDefinition)
        .location(DirectiveLocation::ArgumentDefinition)
        .location(DirectiveLocation::InputFieldDefinition)
}

fn count(haystack: &str, needle: &str) -> usize {
    haystack.matches(needle).count()
}

#[test]
fn sdl_prints_type_and_field_directives_with_arguments() {
    let mut schema = Schema::<()>::new();
    schema.directive(tag_definition());
    schema.type_directive("Giraffe", tag("animal"));
    schema.object::<Giraffe>(|o| {
        o.directive(tag("declared"));
        o.field("name", |g: &Giraffe, _: NoArgs, _: &()| g.name.clone())
            .directive(tag("public"));
    });
    schema.query(|q| {
        q.field("giraffe", |_: NoArgs, _: &()| Giraffe {
            name: "Melman".into(),
        })
        .directive(tag("root"));
    });

    let sdl = schema.build().expect("valid schema").sdl();
    assert!(sdl.contains("directive @tag("), "SDL was:\n{sdl}");
    assert!(
        sdl.contains("desc: \"A documentation tag.\"") || sdl.contains("A documentation tag."),
        "SDL was:\n{sdl}"
    );
    assert!(
        sdl.contains("type Giraffe @tag(name: \"declared\") @tag(name: \"animal\")"),
        "SDL was:\n{sdl}"
    );
    assert!(
        sdl.contains("name: String! @tag(name: \"public\")"),
        "SDL was:\n{sdl}"
    );
    assert!(
        sdl.contains("giraffe: Giraffe @tag(name: \"root\")"),
        "SDL was:\n{sdl}"
    );
}

#[test]
fn introspection_lists_custom_directive_definitions() {
    let mut schema = Schema::<()>::new();
    schema.directive(tag_definition());
    schema.object::<Giraffe>(|o| {
        o.expose(expose!(Giraffe::name));
    });
    schema.query(|q| {
        q.field("ok", |_: NoArgs, _: &()| true);
    });
    let schema = schema.build().expect("valid schema");

    let query = r#"
        {
            __schema {
                directives {
                    name
                    description
                    isRepeatable
                    locations
                    args { name }
                }
            }
        }
    "#;
    let response = futures::executor::block_on(gql::execute(
        &schema,
        Request::new(query),
        &ExecutionOptions::with_context(()),
    ));
    assert!(response.errors.is_empty(), "errors: {:?}", response.errors);
    let data = response.data.expect("data");
    let directives = data
        .as_object()
        .and_then(|data| data.get("__schema"))
        .and_then(Value::as_object)
        .and_then(|schema| schema.get("directives"))
        .and_then(Value::as_list)
        .expect("__schema.directives");

    let tag = directives
        .iter()
        .filter_map(Value::as_object)
        .find(|directive| directive.get("name") == Some(&Value::String("tag".into())))
        .expect("custom directive is introspectable");
    assert_eq!(
        tag.get("description"),
        Some(&Value::String("A documentation tag.".into()))
    );
    assert_eq!(tag.get("isRepeatable"), Some(&Value::Boolean(true)));
    assert_eq!(
        tag.get("locations"),
        Some(&Value::List(vec![
            Value::Enum("OBJECT".into()),
            Value::Enum("INTERFACE".into()),
            Value::Enum("UNION".into()),
            Value::Enum("SCALAR".into()),
            Value::Enum("ENUM".into()),
            Value::Enum("INPUT_OBJECT".into()),
            Value::Enum("FIELD_DEFINITION".into()),
            Value::Enum("ARGUMENT_DEFINITION".into()),
            Value::Enum("INPUT_FIELD_DEFINITION".into()),
        ]))
    );
}

#[test]
fn type_directive_reaches_every_named_kind() {
    let mut schema = Schema::<()>::new();
    schema.directive(tag_definition());

    // Every application is declared before its type exists.
    schema.type_directive("Giraffe", tag("object"));
    schema.type_directive("Node", tag("interface"));
    schema.type_directive("SearchResult", tag("union"));
    schema.type_directive("Date", tag("scalar"));
    schema.type_directive("Color", tag("enum"));
    schema.type_directive("FilterInput", tag("input"));
    schema.type_directive("Query", tag("root"));

    let node = schema.interface("Node", |i| {
        i.field::<ID>("id");
    });
    schema.object::<Giraffe>(|o| {
        o.implements(&node);
        o.expose(expose!(Giraffe::name));
        o.resolve_inherited("id", |g: &Giraffe, _: NoArgs, _: &()| {
            ID::new(g.name.clone())
        });
    });
    schema.object::<User>(|o| {
        o.implements(&node);
        o.expose(expose!(User::name));
        o.resolve_inherited("id", |u: &User, _: NoArgs, _: &()| ID::new(u.name.clone()));
    });
    schema.object::<Post>(|o| {
        o.expose(expose!(Post::title));
    });
    schema.union("SearchResult", |u| {
        u.member::<User>().member::<Post>();
    });
    schema.scalar::<Date>();
    schema.enum_type::<Color>();
    schema.query(|q| {
        q.field("giraffe", |_: NoArgs, _: &()| Giraffe {
            name: "Melman".into(),
        });
        q.field("filter", |_: Filter, _: &()| true);
    });

    let sdl = schema.build().expect("valid schema").sdl();
    for (kind, name) in [
        ("type", "Giraffe"),
        ("interface", "Node"),
        ("union", "SearchResult"),
        ("scalar", "Date"),
        ("enum", "Color"),
        ("input", "FilterInput"),
        ("type", "Query"),
    ] {
        assert!(
            sdl.lines().any(|line| {
                line.starts_with(&format!("{kind} {name}")) && line.contains("@tag(")
            }),
            "missing @tag on {kind} {name}; SDL was:\n{sdl}"
        );
    }
}

#[test]
fn wrong_location_is_a_build_error() {
    let mut schema = Schema::<()>::new();
    schema.directive(
        DirectiveDefinition::new("objectOnly")
            .location(DirectiveLocation::Object)
            .arg(InputValue::new("name", gql::TypeRef::string())),
    );
    schema.object::<Giraffe>(|o| {
        o.field("name", |g: &Giraffe, _: NoArgs, _: &()| g.name.clone())
            .directive(AppliedDirective::new("objectOnly").argument("name", "x"));
    });
    schema.query(|q| {
        q.field("ok", |_: NoArgs, _: &()| true);
    });

    let error = schema
        .build()
        .expect_err("a field is not an object location");
    assert!(
        error.errors().iter().any(|error| error
            .message()
            .contains("may not be used on FIELD_DEFINITION")),
        "unexpected errors: {error}"
    );
}

#[test]
fn wrong_argument_type_is_a_build_error() {
    let mut schema = Schema::<()>::new();
    schema.directive(
        DirectiveDefinition::new("tag")
            .location(DirectiveLocation::Object)
            .arg(InputValue::new("name", gql::TypeRef::string())),
    );
    schema.type_directive("Giraffe", AppliedDirective::new("tag").argument("name", 5));
    schema.object::<Giraffe>(|o| {
        o.expose(expose!(Giraffe::name));
    });
    schema.query(|q| {
        q.field("ok", |_: NoArgs, _: &()| true);
    });

    let error = schema.build().expect_err("an Int is not a String argument");
    assert!(
        error
            .errors()
            .iter()
            .any(|error| error.message().contains("is invalid")),
        "unexpected errors: {error}"
    );
}

#[test]
fn duplicate_non_repeatable_application_is_a_build_error() {
    let mut schema = Schema::<()>::new();
    schema.directive(
        DirectiveDefinition::new("tag")
            .location(DirectiveLocation::Object)
            .arg(InputValue::new("name", gql::TypeRef::string())),
    );
    schema.object::<Giraffe>(|o| {
        o.directive(tag("one"));
        o.directive(tag("two"));
        o.expose(expose!(Giraffe::name));
    });
    schema.query(|q| {
        q.field("ok", |_: NoArgs, _: &()| true);
    });

    let error = schema.build().expect_err("the directive is not repeatable");
    assert!(
        error
            .errors()
            .iter()
            .any(|error| error.message().contains("may only be used once")),
        "unexpected errors: {error}"
    );
}

#[test]
fn unknown_type_name_is_a_build_error() {
    let mut schema = Schema::<()>::new();
    schema.directive(tag_definition());
    schema.type_directive("NeverRegistered", tag("missing"));
    schema.query(|q| {
        q.field("ok", |_: NoArgs, _: &()| true);
    });

    let error = schema.build().expect_err("the type was never registered");
    assert!(
        error.errors().iter().any(|error| error
            .message()
            .contains("no type with that name was registered")),
        "unexpected errors: {error}"
    );
}

#[test]
fn reopening_and_cloning_do_not_double_apply_directives() {
    let mut schema = Schema::<()>::new();
    schema.directive(tag_definition());
    schema.type_directive("Giraffe", tag("animal"));
    schema.object::<Giraffe>(|o| {
        o.directive(tag("first"));
        o.expose(expose!(Giraffe::name));
    });

    // Reopen and add another repeatable application; it is a second declaration, not a repeat.
    schema.object_fields::<Giraffe>(|o| {
        o.field("id", |g: &Giraffe, _: NoArgs, _: &()| {
            ID::new(g.name.clone())
        });
    });
    schema.type_directive("Giraffe", tag("second"));
    schema.query(|q| {
        q.field("ok", |_: NoArgs, _: &()| true);
    });

    let first = schema.build_clone().expect("first build");
    let second = schema.build_clone().expect("second build");
    assert_eq!(first.sdl(), second.sdl());

    let sdl = first.sdl();
    assert_eq!(count(&sdl, "@tag(name: \"animal\")"), 1, "SDL was:\n{sdl}");
    assert_eq!(count(&sdl, "@tag(name: \"first\")"), 1, "SDL was:\n{sdl}");
    assert_eq!(count(&sdl, "@tag(name: \"second\")"), 1, "SDL was:\n{sdl}");
}

#[test]
fn directives_survive_extend_sdl_and_merge() {
    let mut left = Schema::<()>::new();
    left.directive(tag_definition());
    left.type_directive("Giraffe", tag("left"));
    left.object::<Giraffe>(|o| {
        o.expose(expose!(Giraffe::name));
    });
    left.query(|q| {
        q.field("ok", |_: NoArgs, _: &()| true);
    });
    let left = left.build().expect("left builds");

    let extended = left
        .extend_sdl("type Extra { value: String }")
        .expect("extension builds");
    let sdl = extended.sdl();
    assert!(sdl.contains("@tag(name: \"left\")"), "SDL was:\n{sdl}");
    assert!(sdl.contains("type Extra"), "SDL was:\n{sdl}");

    // The right-hand schema comes from SDL so the two `Query` objects can be identical without
    // sharing resolver allocations; merging keeps the resolvers that only the left side has.
    let right = gql::Schema::from_sdl(
        r#"
        """A documentation tag."""
        directive @tag(name: String) repeatable on OBJECT | INTERFACE | UNION | SCALAR | ENUM | INPUT_OBJECT | FIELD_DEFINITION | ARGUMENT_DEFINITION | INPUT_FIELD_DEFINITION

        type Query { ok: Boolean! }

        type User @tag(name: "right") {
            name: String!
        }
        "#,
    )
    .expect("right SDL builds");

    let merged = left.merge(&right).expect("compatible schemas merge");
    let sdl = merged.sdl();
    assert!(sdl.contains("@tag(name: \"left\")"), "SDL was:\n{sdl}");
    assert!(sdl.contains("@tag(name: \"right\")"), "SDL was:\n{sdl}");
}

/// A plugin that sees a field directive and adds another application.
struct AddTrace;

impl Plugin<()> for AddTrace {
    fn field(&self, _meta: &gurafi::FieldMeta<'_>, field: &mut gql::Field) {
        if field.name.as_ref() == "name" {
            field
                .directives
                .push(AppliedDirective::new("tag").argument("name", "plugin"));
        }
    }
}

#[test]
fn directives_survive_plugin_processing() {
    let mut schema = Schema::<()>::new();
    schema.directive(tag_definition());
    schema.plugin(AddTrace);
    schema.object::<Giraffe>(|o| {
        o.directive(tag("object"));
        o.field("name", |g: &Giraffe, _: NoArgs, _: &()| g.name.clone())
            .directive(tag("field"));
    });
    schema.query(|q| {
        q.field("ok", |_: NoArgs, _: &()| true);
    });

    let sdl = schema.build().expect("valid schema").sdl();
    assert!(sdl.contains("@tag(name: \"object\")"), "SDL was:\n{sdl}");
    assert!(sdl.contains("@tag(name: \"field\")"), "SDL was:\n{sdl}");
    assert!(sdl.contains("@tag(name: \"plugin\")"), "SDL was:\n{sdl}");
}

#[test]
fn runtime_named_objects_root_fields_and_arguments_accept_directives() {
    let mut schema = Schema::<()>::new();
    schema.directive(tag_definition());

    let admin = schema.object_ref::<User>("AdminUser");
    schema.type_directive("AdminUser", tag("runtime"));
    schema.implement(&admin, |o| {
        o.expose(expose!(User::name));
    });

    schema.object::<User>(|o| {
        o.expose(expose!(User::name));
        o.field("nameWithLimit", |u: &User, _: NameArgs, _: &()| {
            u.name.clone()
        })
        .directive(tag("object-field"));
    });
    schema.argument_directive("User", "nameWithLimit", "limit", tag("argument"));
    schema.input_field_directive("FilterInput", "limit", tag("input-field"));
    schema.query(|q| {
        q.field_ref(&admin, "admin", |_: NoArgs, _: &()| User {
            name: "ada".into(),
        })
        .directive(tag("root-field"));
    });
    schema.query(|q| {
        q.field("filter", |_: Filter, _: &()| true);
        q.field("adminUserField", |_: Filter, _: &()| true)
            .description("registers FilterInput");
    });
    schema.argument_directive("Query", "filter", "limit", tag("root-argument"));
    schema.type_directive("Query", tag("root-type"));

    let sdl = schema.build().expect("valid schema").sdl();
    assert!(sdl.contains("type AdminUser @tag("), "SDL was:\n{sdl}");
    assert!(
        sdl.contains("admin: AdminUser @tag(name: \"root-field\")"),
        "SDL was:\n{sdl}"
    );
    assert!(
        sdl.contains("limit: Int! @tag(name: \"argument\")"),
        "SDL was:\n{sdl}"
    );
    assert!(
        sdl.contains("limit: Int! @tag(name: \"input-field\")"),
        "SDL was:\n{sdl}"
    );
    assert!(
        sdl.contains("limit: Int! @tag(name: \"root-argument\")"),
        "SDL was:\n{sdl}"
    );
    assert!(sdl.contains("type Query @tag("), "SDL was:\n{sdl}");
}

#[test]
fn unknown_argument_and_input_field_targets_are_build_errors() {
    let mut schema = Schema::<()>::new();
    schema.directive(tag_definition());
    schema.argument_directive("Giraffe", "missing", "name", tag("x"));
    schema.input_field_directive("FilterInput", "missing", tag("x"));
    schema.object::<Giraffe>(|o| {
        o.expose(expose!(Giraffe::name));
    });
    schema.query(|q| {
        q.field("filter", |_: Filter, _: &()| true);
    });

    let error = schema.build().expect_err("both targets are missing");
    let messages: Vec<&str> = error.errors().iter().map(|error| error.message()).collect();
    assert!(
        messages
            .iter()
            .any(|message| message.contains("that field does not exist")),
        "unexpected errors: {error}"
    );
    assert!(
        messages
            .iter()
            .any(|message| message.contains("has no field named `missing`")),
        "unexpected errors: {error}"
    );
}

#[test]
fn interface_field_directives_are_shown_and_inherited() {
    let mut schema = Schema::<()>::new();
    schema.directive(tag_definition());
    let node = schema.interface("Node", |i| {
        i.directive(tag("interface"));
        i.field::<ID>("id").directive(tag("interface-field"));
    });
    schema.object::<Giraffe>(|o| {
        o.implements(&node);
        o.expose(expose!(Giraffe::name));
        o.resolve_inherited("id", |g: &Giraffe, _: NoArgs, _: &()| {
            ID::new(g.name.clone())
        });
    });
    schema.query(|q| {
        q.field("ok", |_: NoArgs, _: &()| true);
    });

    let sdl = schema.build().expect("valid schema").sdl();
    assert!(
        sdl.contains("interface Node @tag(name: \"interface\")"),
        "SDL was:\n{sdl}"
    );
    assert!(
        sdl.contains("id: ID! @tag(name: \"interface-field\")"),
        "SDL was:\n{sdl}"
    );
    // The object field inherits the directive because it carries no directive of its own.
    assert_eq!(
        count(&sdl, "@tag(name: \"interface-field\")"),
        2,
        "SDL was:\n{sdl}"
    );
}

#[test]
fn generated_result_union_types_accept_directives() {
    let mut schema = Schema::<()>::new();
    schema.directive(tag_definition());
    schema.object::<NotFoundError>(|o| {
        o.expose(expose!(NotFoundError::message));
    });
    schema.object::<User>(|o| {
        o.expose(expose!(User::name));
    });
    schema.type_directive("QueryLookupResult", tag("generated-union"));
    schema.type_directive("QueryLookupSuccess", tag("generated-object"));
    schema.query(|q| {
        q.field_result_union::<User, LookupError, NoArgs, _>("lookup", |_: NoArgs, _: &()| {
            Ok(User { name: "ada".into() })
        });
    });

    let sdl = schema.build().expect("valid schema").sdl();
    assert!(
        sdl.lines()
            .any(|line| line.starts_with("type QueryLookupSuccess")
                && line.contains("@tag(name: \"generated-object\")")),
        "SDL was:\n{sdl}"
    );
    assert!(
        sdl.lines()
            .any(|line| line.starts_with("union QueryLookupResult")
                && line.contains("@tag(name: \"generated-union\")")),
        "SDL was:\n{sdl}"
    );
}

#[test]
fn union_model_keeps_its_declared_directive() {
    #[derive(Union)]
    enum Search {
        User(User),
    }

    let mut schema = Schema::<()>::new();
    schema.directive(tag_definition());
    schema.object::<User>(|o| {
        o.expose(expose!(User::name));
    });
    schema.union("Search", |u| {
        u.directive(tag("union"));
        u.member::<User>();
    });
    schema.query(|q| {
        q.field("search", |_: NoArgs, _: &()| User { name: "ada".into() })
            .returns::<Search>();
    });

    let sdl = schema.build().expect("valid schema").sdl();
    assert!(
        sdl.contains("union Search @tag(name: \"union\")"),
        "SDL was:\n{sdl}"
    );
}
