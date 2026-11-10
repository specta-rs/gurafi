//! Differential tests for the precompiled field-collection plan.
//!
//! Every document is executed twice against the same schema: once with `gql::execute` (parse,
//! validate, dynamic `CollectFields`) and once with a `PreparedQuery` (which uses the plan built
//! at preparation). The responses must be identical, including errors and null propagation. The
//! cases deliberately cover the plan's edge cases: aliases, merged selections, variable
//! directives, conditional fragments (which fall back to dynamic collection), abstract types,
//! nested lists, and every shape of field argument.

use std::sync::Arc;

use futures::executor::block_on;
use gql::{
    ExecutionOptions, Field, InputValue, InterfaceType, Object, ObjectType, Schema, TypeRef, Value,
    prepared::PreparedQuery,
};

fn object(fields: impl IntoIterator<Item = (&'static str, Value)>) -> Value {
    Value::object(Object::from_fields(fields))
}

fn schema() -> Schema {
    let node = InterfaceType::new("Node")
        .field(Field::new("id", TypeRef::id().non_null()))
        .resolve_type(|value, _context| {
            let name = value
                .as_object()
                .and_then(|object| object.get("__typename"))
                .and_then(Value::as_str)
                .unwrap_or("User");
            Ok(gql::Name::from(name))
        });

    let user = ObjectType::new("User")
        .implements("Node")
        .field(Field::new("id", TypeRef::id().non_null()))
        .field(Field::new("name", TypeRef::string().non_null()))
        .field(Field::new("age", TypeRef::int()))
        .field(Field::new("admin", TypeRef::boolean().non_null()))
        .field(Field::new(
            "friends",
            TypeRef::named("User").non_null().list().non_null(),
        ))
        .field(Field::new(
            "posts",
            TypeRef::named("Post").non_null().list(),
        ))
        .field(Field::new("best", TypeRef::named("Rank")));

    let post = ObjectType::new("Post")
        .implements("Node")
        .field(Field::new("id", TypeRef::id().non_null()))
        .field(Field::new("title", TypeRef::string().non_null()))
        .field(Field::new(
            "tags",
            TypeRef::string().non_null().list().non_null(),
        ))
        .field(Field::new("score", TypeRef::int()));

    let search = gql::UnionType::new("Search").member("User").member("Post");

    let query = ObjectType::new("Query")
        .field(
            Field::new("user", TypeRef::named("User"))
                .arg(InputValue::new("id", TypeRef::id()))
                .arg(InputValue::new("limit", TypeRef::int()).default_value(Value::Int(2)))
                .arg(InputValue::new("filter", TypeRef::named("Filter")))
                .resolve(|context| {
                    let id = context
                        .args
                        .get("id")
                        .and_then(Value::as_str)
                        .unwrap_or("1")
                        .to_owned();
                    Box::pin(async move { Ok(user_value(&id)) })
                }),
        )
        .field(
            Field::new("node", TypeRef::named("Node"))
                .resolve(|context| Box::pin(async move { Ok(context.parent.clone()) })),
        )
        .field(
            Field::new(
                "search",
                TypeRef::named("Search").non_null().list().non_null(),
            )
            .resolve(|context| Box::pin(async move { Ok(context.parent.clone()) })),
        )
        .field(
            Field::new("matrix", TypeRef::int().list().list()).resolve(|_| {
                Box::pin(async {
                    Ok(Value::List(vec![
                        Value::List(vec![Value::Int(1), Value::Int(2)]),
                        Value::List(vec![Value::Int(3)]),
                    ]))
                })
            }),
        )
        .field(
            Field::new("maybe", TypeRef::string()).resolve(|_| Box::pin(async { Ok(Value::Null) })),
        )
        .field(
            Field::new("broken", TypeRef::string())
                .resolve(|_| Box::pin(async { Err(gql::GraphQLError::new("resolver exploded")) })),
        );

    let filter = gql::InputObjectType::new("Filter")
        .field(InputValue::new("name", TypeRef::string()))
        .field(InputValue::new("tags", TypeRef::string().non_null().list()));

    let rank = gql::EnumType::new("Rank").value("LOW").value("HIGH");

    let mut builder = Schema::builder();
    builder.add_type(node.into());
    builder.add_type(user.into());
    builder.add_type(post.into());
    builder.add_type(search.into());
    builder.add_type(filter.into());
    builder.add_type(rank.into());
    builder.add_type(query.into());
    builder.query("Query").build().expect("valid schema")
}

fn user_value(id: &str) -> Value {
    let friend = object([
        ("id", Value::from("2")),
        ("name", Value::from("Bob")),
        ("age", Value::Int(31)),
        ("admin", Value::Boolean(false)),
        ("friends", Value::List(Vec::new())),
        ("posts", Value::List(Vec::new())),
        ("best", Value::Enum("HIGH".into())),
        ("__typename", Value::from("User")),
    ]);
    let post = object([
        ("id", Value::from("p1")),
        ("title", Value::from("Hello")),
        (
            "tags",
            Value::List(vec![Value::from("a"), Value::from("b")]),
        ),
        ("score", Value::Int(7)),
        ("__typename", Value::from("Post")),
    ]);
    object([
        ("id", Value::from(id)),
        ("name", Value::from("Alice")),
        ("age", Value::Int(30)),
        ("admin", Value::Boolean(true)),
        ("friends", Value::List(vec![friend.clone()])),
        ("posts", Value::List(vec![post.clone(), Value::Null])),
        ("best", Value::Enum("LOW".into())),
        ("__typename", Value::from("User")),
    ])
}

fn root_value() -> Value {
    let user = user_value("1");
    object([
        ("user", user.clone()),
        ("node", user.clone()),
        (
            "search",
            Value::List(vec![
                user,
                object([
                    ("id", Value::from("p1")),
                    ("title", Value::from("Hello")),
                    ("tags", Value::List(vec![Value::from("a")])),
                    ("score", Value::Int(7)),
                    ("__typename", Value::from("Post")),
                ]),
            ]),
        ),
    ])
}

fn options() -> ExecutionOptions {
    let mut options = ExecutionOptions::default()
        .root_value(root_value())
        .max_depth(32)
        .max_complexity(100_000);
    options.timeout = Some(std::time::Duration::from_secs(5));
    options
}

/// Execute a document normally and as a prepared query, asserting identical responses.
fn assert_same(source: &str, variables: Option<&Object>) {
    let schema = schema();
    let execution_options = options();
    let request = match variables {
        Some(variables) => gql::Request::new(source).variables(variables),
        None => gql::Request::new(source),
    };
    let normal = block_on(gql::execute(&schema, request, &execution_options));
    let prepared = PreparedQuery::new(&schema, "plan", source, None).expect("prepared");
    let shared = block_on(prepared.execute(variables, &execution_options));

    assert_eq!(
        normal.data, shared.data,
        "data differs for:\n{source}\nnormal: {normal:?}\nprepared: {shared:?}"
    );
    assert_eq!(
        normal.errors, shared.errors,
        "errors differ for:\n{source}\nnormal: {normal:?}\nprepared: {shared:?}"
    );
}

fn vars(fields: impl IntoIterator<Item = (&'static str, Value)>) -> Object {
    Object::from_fields(fields)
}

#[test]
fn aliases_and_merged_selections() {
    assert_same(
        r#"
        query {
          viewer: user(id: "1") {
            id
            called: name
            name
            ...UserFields
            ... on User { age admin }
            posts { id title }
            posts { title tags score }
          }
          other: user(id: "1") { id name }
        }

        fragment UserFields on User {
          id
          age
          friends { id name }
        }
        "#,
        None,
    );
}

#[test]
fn variable_directives_all_combinations() {
    let source = r#"
        query Dashboard($withAge: Boolean!, $withoutName: Boolean!) {
          user(id: "1") {
            id
            name @skip(if: $withoutName)
            age @include(if: $withAge)
            posts @include(if: $withAge) { title }
          }
        }
        "#;

    for with_age in [true, false] {
        for without_name in [true, false] {
            let variables = vars([
                ("withAge", Value::Boolean(with_age)),
                ("withoutName", Value::Boolean(without_name)),
            ]);
            assert_same(source, Some(&variables));
        }
    }

    // Missing variables: `@include` behaves as false and `@skip` as false.
    assert_same(source, Some(&vars([("withAge", Value::Boolean(false))])));
}

#[test]
fn static_and_nested_directives() {
    assert_same(
        r#"
        query ($on: Boolean = true) {
          user(id: "1") {
            id @skip(if: false)
            name @include(if: true)
            age @include(if: false)
            ... on User @include(if: $on) {
              admin
              ...More @include(if: $on)
            }
            ... on User @skip(if: $on) {
              posts { title }
            }
          }
        }
        fragment More on User { age best }
        "#,
        None,
    );
}

#[test]
fn conditional_fragment_spread_falls_back_dynamically() {
    // A conditional spread has dynamic `visitedFragments` semantics; the plan must leave it to the
    // dynamic collector without changing the result.
    for on in [true, false] {
        let variables = vars([("on", Value::Boolean(on))]);
        assert_same(
            r#"
            query ($on: Boolean!) {
              user(id: "1") {
                id
                ...Fields @include(if: $on)
                ...Fields
              }
            }
            fragment Fields on User { name age }
            "#,
            Some(&variables),
        );
    }

    // The same interaction through a conditional inline fragment.
    for on in [true, false] {
        let variables = vars([("on", Value::Boolean(on))]);
        assert_same(
            r#"
            query ($on: Boolean!) {
              user(id: "1") {
                ... on User @include(if: $on) { ...Fields }
                ...Fields
              }
            }
            fragment Fields on User { name }
            "#,
            Some(&variables),
        );
    }
}

#[test]
fn repeated_fragments_are_deduplicated() {
    assert_same(
        r#"
        query {
          user(id: "1") {
            ...A
            ...A
            ...B
            friends { ...A ...A }
          }
        }
        fragment A on User { id name }
        fragment B on User { age ...A }
        "#,
        None,
    );
}

#[test]
fn abstract_types_select_runtime_plans() {
    assert_same(
        r#"
        query {
          node {
            __typename
            id
            ... on User { name admin friends { id name } }
            ... on Post { title tags }
          }
          search {
            __typename
            ... on User { id name best }
            ... on Post { id title score }
          }
        }
        "#,
        None,
    );
}

#[test]
fn nested_lists_and_null_items() {
    assert_same(
        r#"
        query {
          matrix
          user(id: "1") {
            posts {
              title
              tags
              score
            }
            friends { name }
          }
          maybe
          broken
        }
        "#,
        None,
    );
}

#[test]
fn argument_templates_match_dynamic_coercion() {
    // Literal arguments, argument defaults, an explicitly provided nullable variable that is absent,
    // and a nested variable inside an input object (which keeps the general coercion path).
    for (source, variables) in [
        (
            r#"query { user { id } }"#,
            vars([("unused", Value::Int(1))]),
        ),
        (
            r#"query ($id: ID) { user(id: $id, limit: 5) { id } }"#,
            vars([("id", Value::from("1"))]),
        ),
        (r#"query ($id: ID) { user(id: $id) { id } }"#, vars([])),
        (
            r#"query ($filter: Filter) {
                 user(id: "1", filter: $filter) { id }
               }"#,
            vars([(
                "filter",
                Value::object(Object::from_fields([("name", Value::from("Alice"))])),
            )]),
        ),
        (
            r#"query ($name: String) {
                 user(filter: { name: $name, tags: ["a"] }) { id }
               }"#,
            vars([("name", Value::from("Alice"))]),
        ),
    ] {
        assert_same(source, Some(&variables));
    }
}

#[test]
fn default_variable_values_drive_conditions() {
    assert_same(
        r#"
        query ($show: Boolean = false, $hide: Boolean = true) {
          user(id: "1") {
            id
            name @include(if: $show)
            age @skip(if: $hide)
          }
        }
        "#,
        None,
    );
}

#[test]
fn introspection_fields_fall_back_safely() {
    assert_same(
        r#"
        {
          __schema {
            queryType { name }
            types { name kind }
          }
          __type(name: "User") {
            name
            fields { name type { name kind ofType { name } } }
          }
        }
        "#,
        None,
    );
}

#[test]
fn fragments_behind_type_conditions_merge_in_document_order() {
    assert_same(
        r#"
        query {
          user(id: "1") {
            ... on Node { id }
            ... on User { name ... on Node { id } }
            ... on User { age }
          }
        }
        "#,
        None,
    );
}

#[test]
fn nullable_and_non_null_propagation_match() {
    assert_same(
        r#"
        query {
          user(id: "1") {
            posts {
              title
              score
            }
            friends { id name age }
          }
          node { id }
        }
        "#,
        None,
    );
}

#[test]
fn prepared_subscription_event_matches_normal() {
    use futures::StreamExt as _;

    let schema = {
        let subscription = ObjectType::new("Subscription").field(
            Field::new("ticks", TypeRef::int().non_null())
                .subscribe(|_| {
                    Box::pin(async {
                        Ok(
                            futures::stream::iter(vec![Ok(Value::Int(1)), Ok(Value::Int(2))])
                                .boxed(),
                        )
                    })
                })
                .resolve(|context| Box::pin(async move { Ok(context.parent.clone()) })),
        );
        let query = ObjectType::new("Query").field(
            Field::new("health", TypeRef::string().non_null())
                .resolve(|_| Box::pin(async { Ok(Value::from("ok")) })),
        );
        let mut builder = Schema::builder();
        builder.add_type(query.into());
        builder.add_type(subscription.into());
        builder
            .query("Query")
            .subscription("Subscription")
            .build()
            .expect("valid schema")
    };

    let source = "subscription { ticks }";
    let options = ExecutionOptions::default();
    let prepared = PreparedQuery::new(&schema, "ticks", source, None).expect("prepared");

    let normal = block_on(gql::subscribe(&schema, gql::Request::new(source), &options))
        .expect("normal subscription");
    let shared = block_on(prepared.subscribe(None, &options)).expect("prepared subscription");

    let normal: Vec<_> = block_on(normal.stream.collect::<Vec<_>>());
    let shared: Vec<_> = block_on(shared.stream.collect::<Vec<_>>());
    assert_eq!(normal.len(), shared.len());
    for (normal, shared) in normal.iter().zip(&shared) {
        assert_eq!(normal.data, shared.data);
        assert_eq!(normal.errors, shared.errors);
    }
}

#[test]
fn prepared_query_is_send_and_shareable_across_threads() {
    let schema = schema();
    let prepared = PreparedQuery::new(&schema, "shared", r#"{ user(id: "1") { id name } }"#, None)
        .expect("prepared");
    let handles: Vec<_> = (0..4)
        .map(|_| {
            let prepared = prepared.clone();
            let options = options();
            std::thread::spawn(move || block_on(prepared.execute(None, &options)).data)
        })
        .collect();
    let expected = handles
        .into_iter()
        .map(|handle| handle.join().expect("thread"))
        .collect::<Vec<_>>();
    assert!(expected.windows(2).all(|pair| pair[0] == pair[1]));
}

#[test]
fn request_variables_override_prepared_defaults() {
    assert_same(
        r#"
        query ($show: Boolean = false) {
          user(id: "1") { id name @include(if: $show) }
        }
        "#,
        Some(&vars([("show", Value::Boolean(true))])),
    );
}

#[test]
fn error_on_non_null_field_matches() {
    let schema = schema();
    let source = r#"{ user(id: "1") { posts { title score } } }"#;
    let options = options();
    let normal = block_on(gql::execute(&schema, gql::Request::new(source), &options));
    let prepared = PreparedQuery::new(&schema, "err", source, None).expect("prepared");
    let shared = block_on(prepared.execute(None, &options));
    assert_eq!(normal.data, shared.data);
    assert_eq!(normal.errors, shared.errors);
}

#[test]
fn empty_lists_and_null_lists() {
    assert_same(
        r#"
        {
          user(id: "1") {
            posts { title }
            friends { id }
          }
        }
        "#,
        None,
    );
}

#[test]
fn prepared_reuses_plan_across_repeated_executions() {
    let schema = schema();
    let prepared = PreparedQuery::new(
        &schema,
        "repeat",
        r#"{ user(id: "1") { id name posts { title tags } } }"#,
        None,
    )
    .expect("prepared");
    let options = options();
    let first = block_on(prepared.execute(None, &options));
    for _ in 0..10 {
        let again = block_on(prepared.execute(None, &options));
        assert_eq!(first.data, again.data);
        assert_eq!(first.errors, again.errors);
    }
}

#[test]
fn large_selection_plan_builds() {
    // More than the executor's inline index limit and more than eight fields per object.
    let fields = (0..24)
        .map(|index| format!("a{index}: id"))
        .chain((0..24).map(|index| format!("n{index}: name @include(if: true)")))
        .collect::<Vec<_>>()
        .join("\n");
    let source = format!("query {{ user(id: \"1\") {{ {fields} }} }}");
    assert_same(&source, None);
}

#[test]
fn final_response_value_is_stable() {
    // Guard against the plan borrowing anything that would outlive the prepared query's source.
    let schema = Arc::new(schema());
    let prepared = PreparedQuery::new(&schema, "owned", r#"{ user(id: "1") { id name } }"#, None)
        .expect("prepared");
    let options = options();
    let response = block_on(prepared.execute(None, &options));
    let rendered = format!("{response:?}");
    assert!(rendered.contains("Alice"));
}
