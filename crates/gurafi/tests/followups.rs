//! Regression tests for the follow-up review findings (items 7-13).

use std::sync::Arc;

use gurafi::{
    GraphQLError, ID, Input, NoArgs, Object, ScalarValue, Schema, Value, impl_scalar_traits,
};

#[derive(Clone, Debug)]
struct Ctx {
    logged_in: bool,
}

impl Ctx {
    fn anonymous() -> Self {
        Self { logged_in: false }
    }

    fn logged_in() -> Self {
        Self { logged_in: true }
    }
}

fn run(schema: &gql::Schema, query: &str, ctx: Ctx) -> gql::Response {
    futures::executor::block_on(gql::execute(
        schema,
        gql::Request::new(query),
        &gql::ExecutionOptions::with_context(ctx),
    ))
}

#[derive(Object)]
struct User {
    name: String,
}

fn user() -> User {
    User { name: "ada".into() }
}

#[test]
fn a_transitively_implemented_interface_is_added_for_the_object() {
    let mut schema = Schema::<Ctx>::new();
    let node = schema.interface("Node", |i| {
        i.field::<ID>("id");
    });
    let resource = schema.interface("Resource", |i| {
        i.implements(&node);
        // The specification requires the child interface to redeclare the parent's fields.
        i.field::<ID>("id");
        i.field::<String>("url");
    });
    schema.object::<User>(|o| {
        // `Node` is intentionally not declared again: the builder must add it.
        o.implements(&resource);
        o.field("id", |u: &User, _: NoArgs, _: &Ctx| ID::new(u.name.clone()));
        o.field("url", |u: &User, _: NoArgs, _: &Ctx| {
            format!("https://example.com/{}", u.name)
        });
    });
    schema.query(|q| {
        q.field("user", |_: NoArgs, _: &Ctx| user());
    });
    let schema = schema.build().expect("transitive interfaces build");

    let user_type = schema.object_type("User").expect("User");
    let names: Vec<_> = user_type
        .interfaces
        .iter()
        .map(ToString::to_string)
        .collect();
    assert!(names.contains(&"Node".to_owned()), "{names:?}");
    assert!(names.contains(&"Resource".to_owned()), "{names:?}");

    let response = run(&schema, "{ user { id url } }", Ctx::anonymous());
    assert!(!response.has_errors(), "{response:?}");
}

#[test]
fn a_named_input_can_be_used_as_an_input_field_default() {
    #[derive(Input)]
    #[graphql(name = "PageInput")]
    struct Page {
        #[graphql(default = 20)]
        first: i32,
    }

    #[derive(Input)]
    struct ListArgs {
        #[graphql(default = Page { first: 3 })]
        page: Page,
    }

    let mut schema = Schema::<Ctx>::new();
    schema.query(|q| {
        q.field("list", |args: ListArgs, _: &Ctx| args.page.first);
    });
    let schema = schema.build().expect("a derived input is a valid default");
    let field = schema
        .object_type("Query")
        .expect("Query")
        .get_field("list")
        .expect("list");
    let default = field
        .args
        .get("page")
        .and_then(|arg| arg.default_value.as_ref())
        .expect("the nested default is serialized");
    assert_eq!(
        default
            .as_object()
            .and_then(|object| object.get("first"))
            .and_then(Value::as_i64),
        Some(3)
    );

    let response = run(&schema, "{ list }", Ctx::anonymous());
    assert!(!response.has_errors(), "{response:?}");
    assert_eq!(
        response
            .data
            .as_ref()
            .and_then(Value::as_object)
            .and_then(|data| data.get("list")),
        Some(&Value::Int(3))
    );
}

#[test]
fn a_one_of_input_used_directly_as_arguments_is_rejected() {
    #[derive(Input)]
    #[graphql(name = "Choice", one_of)]
    struct Choice {
        id: Option<ID>,
        name: Option<String>,
    }

    let mut schema = Schema::<Ctx>::new();
    schema.query(|q| {
        q.field("pick", |_: Choice, _: &Ctx| true);
    });
    let error = schema
        .build()
        .expect_err("a one-of input needs its own argument wrapper");
    assert!(error.to_string().contains("oneOf"), "{error}");
    assert!(error.to_string().contains("Wrap"), "{error}");
}

#[test]
fn an_unregistered_union_member_reports_the_friendly_error() {
    #[derive(Object)]
    #[allow(dead_code)]
    struct Ghost {
        name: String,
    }
    let _ = Ghost {
        name: String::new(),
    };

    let mut schema = Schema::<Ctx>::new();
    schema.union("Thing", |u| {
        u.member::<Ghost>();
    });
    schema.query(|q| {
        q.field("ok", |_: NoArgs, _: &Ctx| true);
    });
    let error = schema
        .build()
        .expect_err("the union member was never registered");
    assert!(error.to_string().contains("never registered"), "{error}");
}

#[test]
fn resolvers_can_set_response_extensions() {
    let mut schema = Schema::<Ctx>::new();
    schema.query(|q| {
        q.field_with_execution("ping", |_: NoArgs, _: &Ctx, execution| {
            execution
                .context
                .set_extension("servedBy", Value::String(Arc::from("gurafi")));
            true
        });
    });
    let schema = schema.build().expect("schema builds");
    let response = run(&schema, "{ ping }", Ctx::anonymous());
    assert!(!response.has_errors(), "{response:?}");
    let extensions = response.extensions.expect("extensions");
    assert_eq!(
        extensions.get("servedBy").and_then(Value::as_str),
        Some("gurafi")
    );
}

#[test]
fn object_extension_resolvers_also_work() {
    let mut schema = Schema::<Ctx>::new();
    schema.object::<User>(|o| {
        o.field_result_with_execution("name", |u: &User, _: NoArgs, _: &Ctx, execution| {
            execution.context.set_extension("source", "object");
            Ok(u.name.clone())
        });
    });
    schema.query(|q| {
        q.field("user", |_: NoArgs, _: &Ctx| user());
    });
    let schema = schema.build().expect("schema builds");
    let response = run(&schema, "{ user { name } }", Ctx::anonymous());
    assert!(!response.has_errors(), "{response:?}");
    assert_eq!(
        response
            .extensions
            .as_ref()
            .and_then(|extensions| extensions.get("source"))
            .and_then(Value::as_str),
        Some("object")
    );
}

struct Big(i128);

impl ScalarValue for Big {
    const NAME: &'static str = "Big";

    fn to_value(&self) -> Result<Value, GraphQLError> {
        Ok(Value::String(Arc::from(self.0.to_string())))
    }

    fn from_value(value: &Value) -> Result<Self, GraphQLError> {
        match value {
            Value::Int(number) => Ok(Self(i128::from(*number))),
            Value::BigInt(number) => Ok(Self(**number)),
            other => Err(GraphQLError::new(format!(
                "`Big` cannot represent a value of kind {}.",
                other.kind()
            ))),
        }
    }
}

impl_scalar_traits!(Big);

#[derive(Input)]
struct BigArgs {
    value: Big,
}

#[test]
fn a_literal_beyond_i32_reaches_a_custom_scalar_unchanged() {
    const BIG: i128 = 99_999_999_999_999_999_999_999_999;

    let mut schema = Schema::<Ctx>::new();
    schema.query(|q| {
        q.field("big", |args: BigArgs, _: &Ctx| args.value.0.to_string());
    });
    let schema = schema.build().expect("schema builds");
    let response = run(
        &schema,
        &format!("{{ big(value: {BIG}) }}"),
        Ctx::anonymous(),
    );
    assert!(!response.has_errors(), "{response:?}");
    assert_eq!(
        response
            .data
            .as_ref()
            .and_then(Value::as_object)
            .and_then(|data| data.get("big"))
            .and_then(Value::as_str),
        Some(BIG.to_string().as_str())
    );
}

#[derive(Input)]
struct IdArgs {
    id: ID,
}

#[test]
fn a_big_integer_argument_can_be_used_as_an_id() {
    const BIG: i128 = 99_999_999_999_999_999_999_999_999;

    let mut schema = Schema::<Ctx>::new();
    schema.query(|q| {
        q.field("id", |args: IdArgs, _: &Ctx| args.id.to_string());
    });
    let schema = schema.build().expect("schema builds");
    let response = run(&schema, &format!("{{ id(id: {BIG}) }}"), Ctx::anonymous());
    assert!(!response.has_errors(), "{response:?}");
    assert_eq!(
        response
            .data
            .as_ref()
            .and_then(Value::as_object)
            .and_then(|data| data.get("id"))
            .and_then(Value::as_str),
        Some(BIG.to_string().as_str())
    );
}

#[test]
fn repeated_auth_in_one_object_declaration_composes() {
    let mut schema = Schema::<Ctx>::new();
    schema.object::<User>(|o| {
        o.auth(|ctx: &Ctx| ctx.logged_in);
        o.auth(|_: &Ctx| true);
        o.field("name", |u: &User, _: NoArgs, _: &Ctx| u.name.clone());
    });
    schema.query(|q| {
        q.field("user", |_: NoArgs, _: &Ctx| user());
    });
    let schema = schema.build().expect("schema builds");

    let denied = run(&schema, "{ user { name } }", Ctx::anonymous());
    assert!(denied.has_errors(), "{denied:?}");
    let allowed = run(&schema, "{ user { name } }", Ctx::logged_in());
    assert!(!allowed.has_errors(), "{allowed:?}");
}

#[test]
fn repeated_interface_auth_composes_across_reopens() {
    let mut schema = Schema::<Ctx>::new();
    let node = schema.interface("Node", |i| {
        i.auth(|ctx: &Ctx| ctx.logged_in);
        i.field::<ID>("id");
    });
    schema.interface_fields("Node", |i| {
        // Always true: if this replaced the earlier rule, anonymous access would succeed.
        i.auth(|_: &Ctx| true);
    });
    schema.object::<User>(|o| {
        o.implements(&node);
        o.field("id", |u: &User, _: NoArgs, _: &Ctx| ID::new(u.name.clone()));
    });
    schema.query(|q| {
        q.field("user", |_: NoArgs, _: &Ctx| user());
    });
    let schema = schema.build().expect("schema builds");

    let denied = run(&schema, "{ user { id } }", Ctx::anonymous());
    assert!(denied.has_errors(), "{denied:?}");
    let allowed = run(&schema, "{ user { id } }", Ctx::logged_in());
    assert!(!allowed.has_errors(), "{allowed:?}");
}

#[test]
fn a_later_interface_auth_error_refines_the_existing_rule() {
    let mut schema = Schema::<Ctx>::new();
    let node = schema.interface("Node", |i| {
        i.auth(|ctx: &Ctx| ctx.logged_in);
        i.field::<ID>("id");
    });
    schema.interface_fields("Node", |i| {
        i.auth_error("Sign in to read nodes");
    });
    schema.object::<User>(|o| {
        o.implements(&node);
        o.field("id", |u: &User, _: NoArgs, _: &Ctx| ID::new(u.name.clone()));
    });
    schema.query(|q| {
        q.field("user", |_: NoArgs, _: &Ctx| user());
    });
    let schema = schema.build().expect("schema builds");

    let denied = run(&schema, "{ user { id } }", Ctx::anonymous());
    assert!(
        denied
            .errors
            .iter()
            .any(|error| error.message().contains("Sign in to read nodes")),
        "{denied:?}"
    );
}
