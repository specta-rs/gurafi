//! Regression coverage for field collection.
//!
//! Execution normally keeps its fragment lookup inline because documents spread only a handful of
//! fragments; larger documents switch to a hash index. Both paths must return the same data, and
//! fields selected through several fragments must still merge under one response key.

use futures::executor::block_on;
use gql::{ExecutionOptions, Field, Object, ObjectType, Request, Schema, TypeRef, Value};

fn schema(field_count: usize) -> Schema {
    let mut query = ObjectType::new("Query");
    for index in 0..field_count {
        let name = format!("field{index}");
        query = query.field(
            Field::new(name, TypeRef::string().non_null()).resolve(move |_| {
                let name = format!("field{index}");
                Box::pin(async move { Ok(Value::from(name)) })
            }),
        );
    }
    Schema::builder()
        .object(query)
        .query("Query")
        .build()
        .expect("valid schema")
}

fn run(schema: &Schema, query: &str) -> gql::Response {
    block_on(gql::execute(
        schema,
        Request::new(query),
        &ExecutionOptions::default(),
    ))
}

#[test]
fn many_fragments_are_all_collected() {
    // More than the inline lookup's capacity, so the hashed path is used.
    const FRAGMENTS: usize = 24;
    let schema = schema(FRAGMENTS);

    let mut query = String::from("query ManyFragments {");
    for index in 0..FRAGMENTS {
        query.push_str(&format!(" ...F{index}"));
    }
    query.push_str(" }\n");
    for index in 0..FRAGMENTS {
        query.push_str(&format!("fragment F{index} on Query {{ field{index} }}\n"));
    }

    let response = run(&schema, &query);
    assert!(response.errors.is_empty(), "{:?}", response.errors);
    let data = response
        .data
        .as_ref()
        .and_then(Value::as_object)
        .expect("data");
    assert_eq!(data.len(), FRAGMENTS);
    for index in 0..FRAGMENTS {
        assert_eq!(
            data.get(&format!("field{index}")).and_then(Value::as_str),
            Some(format!("field{index}").as_str()),
        );
    }
}

#[test]
fn fragments_spread_twice_are_expanded_once() {
    let schema = schema(2);
    let query = r#"
        query Twice {
          ...Shared
          ...Shared
          other: field1
        }
        fragment Shared on Query {
          field0
        }
    "#;

    let response = run(&schema, query);
    assert!(response.errors.is_empty(), "{:?}", response.errors);
    let data = response
        .data
        .as_ref()
        .and_then(Value::as_object)
        .expect("data");
    assert_eq!(data.len(), 2);
    assert_eq!(data.get("field0").and_then(Value::as_str), Some("field0"));
    assert_eq!(data.get("other").and_then(Value::as_str), Some("field1"));
}

#[test]
fn duplicate_response_keys_merge_subselections() {
    let user = ObjectType::new("User")
        .field(Field::new("id", TypeRef::id().non_null()))
        .field(Field::new("name", TypeRef::string().non_null()));
    let query = ObjectType::new("Query").field(
        Field::new("user", TypeRef::named("User").non_null()).resolve(|_| {
            Box::pin(async {
                Ok(Value::object(Object::from_fields([
                    ("id", Value::from("1")),
                    ("name", Value::from("Ada")),
                ])))
            })
        }),
    );
    let schema = Schema::builder()
        .object(user)
        .object(query)
        .query("Query")
        .build()
        .expect("valid schema");

    // Both selections share the response key `user`, so its sub-selections are merged.
    let response = run(&schema, "query { user { id } user { name } }");
    assert!(response.errors.is_empty(), "{:?}", response.errors);
    let data = response
        .data
        .as_ref()
        .and_then(Value::as_object)
        .expect("data");
    let user = data
        .get("user")
        .and_then(Value::as_object)
        .expect("user object");
    assert_eq!(user.len(), 2);
    assert_eq!(user.get("id").and_then(Value::as_str), Some("1"));
    assert_eq!(user.get("name").and_then(Value::as_str), Some("Ada"));
}
