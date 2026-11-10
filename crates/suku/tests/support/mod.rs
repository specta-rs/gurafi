//! Shared test fixtures.
//!
//! Integration tests are separate crates, so this module is compiled into each of them.

#![allow(dead_code)]

use futures::StreamExt as _;
use gql::{Field, InputValue, ObjectType, Schema, TypeRef, Value};
use suku::{Context, GraphQLRequest, HttpError, Server};

/// A small authenticated user used by the `whoami` field.
#[derive(Clone, Debug, PartialEq)]
pub struct User {
    pub name: String,
}

pub fn schema() -> Schema {
    let hello = Field::new("hello", TypeRef::string().non_null())
        .resolve(|_| Box::pin(async { Ok(Value::from("world")) }));

    let echo = Field::new("echo", TypeRef::string().non_null())
        .arg(InputValue::new("message", TypeRef::string().non_null()))
        .resolve(|context| {
            let message = context
                .args
                .get("message")
                .and_then(Value::as_str)
                .unwrap_or_default()
                .to_owned();
            Box::pin(async move { Ok(Value::from(message)) })
        });

    let whoami = Field::new("whoami", TypeRef::string().non_null()).resolve(|context| {
        let name = context
            .context
            .require_data::<User>()
            .map(|user| user.name.clone());
        Box::pin(async move { name.map(Value::from) })
    });

    let boom = Field::new("boom", TypeRef::string().non_null()).resolve(|_| {
        Box::pin(async { Err::<Value, gql::GraphQLError>(gql::GraphQLError::new("boom")) })
    });

    let coded = Field::new("coded", TypeRef::string().non_null()).resolve(|_| {
        Box::pin(async {
            Err::<Value, gql::GraphQLError>(
                gql::GraphQLError::new("secret database detail").extend("code", "INTERNAL"),
            )
        })
    });

    let located = Field::new("located", TypeRef::string().non_null()).resolve(|_| {
        Box::pin(async {
            Err::<Value, gql::GraphQLError>(
                gql::GraphQLError::new("secret located detail").at(gql::Location::new(3, 5)),
            )
        })
    });

    let query = ObjectType::new("Query")
        .field(hello)
        .field(echo)
        .field(whoami)
        .field(boom)
        .field(coded)
        .field(located);

    let countdown = Field::new("countdown", TypeRef::int().non_null())
        .subscribe(|_| {
            Box::pin(async {
                Ok(futures::stream::iter(vec![
                    Ok(Value::from(3)),
                    Ok(Value::from(2)),
                    Ok(Value::from(1)),
                ])
                .boxed())
            })
        })
        .resolve(|context| Box::pin(async move { Ok(context.parent.clone()) }));

    let failing = Field::new("failing", TypeRef::int().non_null())
        .subscribe(|_| {
            Box::pin(async {
                Ok(futures::stream::iter(vec![Ok(Value::from(1)), Ok(Value::from(2))]).boxed())
            })
        })
        .resolve(|context| {
            Box::pin(async move {
                match context.parent {
                    Value::Int(2) => Err(gql::GraphQLError::new("secret event failure")),
                    value => Ok(value.clone()),
                }
            })
        });

    let subscription = ObjectType::new("Subscription")
        .field(countdown)
        .field(failing);

    let mut builder = Schema::builder();
    builder.add_type(query.into());
    builder.add_type(subscription.into());
    builder = builder.subscription("Subscription");
    builder.build().expect("valid schema")
}

pub fn server() -> Server {
    Server::new(schema())
}

/// Build borrowed request parts for the transport-level tests.
pub fn parts(
    method: ::http::Method,
    uri: &str,
    headers: &[(::http::HeaderName, &str)],
) -> ::http::request::Parts {
    let mut builder = ::http::Request::builder().method(method).uri(uri);
    for (name, value) in headers {
        builder = builder.header(name, *value);
    }
    builder.body(()).expect("valid request").into_parts().0
}

/// An extractor that always returns the empty context.
pub async fn empty_context(_: suku::ContextRequest) -> Result<Context, HttpError> {
    Ok(Context::empty())
}

/// Parse a buffered HTTP response body as JSON.
pub fn json_body(body: &[u8]) -> serde_json::Value {
    serde_json::from_slice(body).expect("JSON body")
}

/// A request used to check the `hello` field.
pub fn hello_request() -> GraphQLRequest {
    GraphQLRequest::new("{ hello }")
}
