//! End-to-end introspection against a rich SDL schema.
//!
//! The fixture pair is also used to compare `gql` with the reference `graphql-js`
//! implementation during development.

use gql::{ExecutionOptions, Request};

#[test]
fn standard_introspection_query_succeeds_for_sdl_schema() {
    let schema = gql::Schema::from_sdl(include_str!("fixtures/schema.graphql"))
        .unwrap_or_else(|error| panic!("{error}"));
    let query = include_str!("fixtures/introspection.graphql");
    let response = futures::executor::block_on(gql::execute(
        &schema,
        Request::new(query),
        &ExecutionOptions::default(),
    ));
    assert!(!response.has_errors(), "{:?}", response.errors);

    let data = response.data.expect("introspection data");
    let schema_value = data.as_object().unwrap().get("__schema").unwrap();
    let schema_object = schema_value.as_object().unwrap();
    let types = schema_object
        .get("types")
        .unwrap()
        .as_list()
        .expect("types list");
    let names: Vec<&str> = types
        .iter()
        .map(|ty| {
            ty.as_object()
                .unwrap()
                .get("name")
                .unwrap()
                .as_str()
                .unwrap()
        })
        .collect();
    for expected in [
        "Query",
        "Mutation",
        "Subscription",
        "User",
        "Post",
        "Node",
        "SearchResult",
        "Role",
        "Filter",
        "Date",
        "__Schema",
        "__Type",
        "__Directive",
        "__DirectiveLocation",
    ] {
        assert!(names.contains(&expected), "missing type {expected}");
    }

    let directives = schema_object.get("directives").unwrap().as_list().unwrap();
    let directive_names: Vec<&str> = directives
        .iter()
        .map(|d| {
            d.as_object()
                .unwrap()
                .get("name")
                .unwrap()
                .as_str()
                .unwrap()
        })
        .collect();
    assert!(directive_names.contains(&"auth"));
    assert!(directive_names.contains(&"skip"));
}
