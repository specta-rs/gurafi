//! Relay mutation payload shape and `clientMutationId` modes.

#![allow(dead_code)]

use futures::executor::block_on;
use gurafi::{Input, NoArgs, Object, Schema, expose, gql};
use gurafi_relay::{ClientMutationId, MutationConfig, Relay};

#[derive(Clone, Object)]
struct User {
    id: String,
    name: String,
}

#[derive(Input)]
#[graphql(name = "CreateUserInput")]
struct CreateUserInput {
    name: String,
}

struct Ctx;

fn execute(schema: &gql::Schema, query: &str) -> gql::Response {
    block_on(gql::execute(
        schema,
        gql::Request::new(query),
        &gql::ExecutionOptions::with_context(Ctx),
    ))
}

fn data(response: &gql::Response) -> &gql::Object {
    response
        .data
        .as_ref()
        .and_then(gql::Value::as_object)
        .unwrap_or_else(|| panic!("missing data: {:?}", response.errors))
}

fn schema(config: MutationConfig) -> gql::Schema {
    let mut schema = Schema::<Ctx>::new();
    let mut relay = Relay::<Ctx>::new();
    schema.object::<User>(|o| {
        o.expose(expose!(User::id));
        o.expose(expose!(User::name));
    });
    schema.query(|q| {
        q.field("ok", |_: NoArgs, _: &Ctx| true);
    });
    relay
        .mutation_with::<User, CreateUserInput, _>(
            &mut schema,
            "createUser",
            config,
            |input, _ctx| {
                Ok(User {
                    id: "1".into(),
                    name: input.name,
                })
            },
        )
        .unwrap();
    schema.build().expect("valid schema")
}

#[test]
fn optional_client_mutation_id_round_trips() {
    let schema = schema(MutationConfig::default());
    let response = execute(
        &schema,
        r#"mutation {
            createUser(input: { name: "Ada" }, clientMutationId: "m1") {
                result { id name }
                clientMutationId
            }
        }"#,
    );
    assert!(!response.has_errors(), "{:?}", response.errors);
    let payload = data(&response)
        .get("createUser")
        .unwrap()
        .as_object()
        .unwrap();
    assert_eq!(
        payload.get("clientMutationId").and_then(gql::Value::as_str),
        Some("m1")
    );
    let result = payload.get("result").unwrap().as_object().unwrap();
    assert_eq!(result.get("name").and_then(gql::Value::as_str), Some("Ada"));

    let omitted = execute(
        &schema,
        r#"mutation { createUser(input: { name: "Grace" }) { clientMutationId } }"#,
    );
    assert!(!omitted.has_errors(), "{:?}", omitted.errors);
    let payload = data(&omitted)
        .get("createUser")
        .unwrap()
        .as_object()
        .unwrap();
    assert!(payload.get("clientMutationId").unwrap().is_null());
}

#[test]
fn required_client_mutation_id_is_validation_error_when_missing() {
    let schema = schema(MutationConfig::new().client_mutation_id(ClientMutationId::Required));

    let missing = execute(
        &schema,
        r#"mutation { createUser(input: { name: "Ada" }) { clientMutationId } }"#,
    );
    assert!(missing.has_errors(), "missing clientMutationId should fail");

    let present = execute(
        &schema,
        r#"mutation { createUser(input: { name: "Ada" }, clientMutationId: "m2") { clientMutationId } }"#,
    );
    assert!(!present.has_errors(), "{:?}", present.errors);
    let payload = data(&present)
        .get("createUser")
        .unwrap()
        .as_object()
        .unwrap();
    assert_eq!(
        payload.get("clientMutationId").and_then(gql::Value::as_str),
        Some("m2")
    );
}

#[test]
fn omit_removes_the_argument_and_payload_field() {
    let schema = schema(MutationConfig::new().client_mutation_id(ClientMutationId::Omit));
    let response = execute(
        &schema,
        r#"mutation { createUser(input: { name: "Ada" }) { result { name } } }"#,
    );
    assert!(!response.has_errors(), "{:?}", response.errors);

    let wants_field = execute(
        &schema,
        r#"mutation { createUser(input: { name: "Ada" }) { clientMutationId } }"#,
    );
    assert!(wants_field.has_errors());

    let sdl = schema.sdl();
    assert!(sdl.contains("type CreateUserPayload"), "SDL was:\n{sdl}");
    assert!(!sdl.contains("clientMutationId"), "SDL was:\n{sdl}");
}

#[test]
fn payload_and_input_names_reach_sdl_and_introspection() {
    let schema = schema(MutationConfig::default());
    let sdl = schema.sdl();
    for expected in [
        "type CreateUserPayload",
        "result: User!",
        "clientMutationId: String",
        "input: CreateUserInput!",
        "createUser(",
    ] {
        assert!(sdl.contains(expected), "missing {expected:?} in:\n{sdl}");
    }

    let response = execute(
        &schema,
        r#"{
            payload: __type(name: "CreateUserPayload") {
                fields { name type { name kind ofType { name } } }
            }
            input: __type(name: "CreateUserInput") { name kind }
        }"#,
    );
    assert!(!response.has_errors(), "{:?}", response.errors);
    assert_eq!(
        data(&response)
            .get("input")
            .and_then(|value| value.as_object())
            .and_then(|object| object.get("name"))
            .and_then(gql::Value::as_str),
        Some("CreateUserInput")
    );
}

#[test]
fn async_mutations_work() {
    let mut schema = Schema::<Ctx>::new();
    let mut relay = Relay::<Ctx>::new();
    schema.object::<User>(|o| {
        o.expose(expose!(User::id));
        o.expose(expose!(User::name));
    });
    schema.query(|q| {
        q.field("ok", |_: NoArgs, _: &Ctx| true);
    });
    relay
        .mutation_async::<User, CreateUserInput, _>(&mut schema, "createUser", |input, _ctx| {
            Box::pin(async move {
                Ok(User {
                    id: "9".into(),
                    name: input.name,
                })
            })
        })
        .unwrap();
    let schema = schema.build().expect("valid schema");

    let response = execute(
        &schema,
        r#"mutation { createUser(input: { name: "Async" }) { result { name } clientMutationId } }"#,
    );
    assert!(!response.has_errors(), "{:?}", response.errors);
    let result = data(&response)
        .get("createUser")
        .unwrap()
        .as_object()
        .unwrap()
        .get("result")
        .unwrap()
        .as_object()
        .unwrap();
    assert_eq!(
        result.get("name").and_then(gql::Value::as_str),
        Some("Async")
    );
}

#[test]
fn an_unnamed_input_is_rejected() {
    #[derive(Input)]
    struct Unnamed {
        name: String,
    }

    let mut schema = Schema::<Ctx>::new();
    let mut relay = Relay::<Ctx>::new();
    schema.object::<User>(|o| {
        o.expose(expose!(User::name));
    });
    schema.query(|q| {
        q.field("ok", |_: NoArgs, _: &Ctx| true);
    });
    let error = relay
        .mutation::<User, Unnamed, _>(&mut schema, "createUser", |input, _ctx| {
            Ok(User {
                id: "1".into(),
                name: input.name,
            })
        })
        .unwrap_err();
    assert!(error.message().contains("named input object"), "{error}");
}
