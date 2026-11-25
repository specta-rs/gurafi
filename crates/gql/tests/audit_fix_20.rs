use std::sync::{Arc, Mutex};

use gql::{
    AuthorizationContext, Authorizer, ExecutionOptions, Field, GraphQLError, Object, ObjectType,
    PreparedQuery, Request, Schema, TypeRef, Value,
};

#[derive(Clone, Default)]
struct TypeNamePolicy {
    seen: Arc<Mutex<Vec<String>>>,
}

impl Authorizer for TypeNamePolicy {
    fn authorize(&self, context: &AuthorizationContext<'_>) -> Result<(), GraphQLError> {
        if context.field_name == "__type" {
            let name = context
                .args
                .get("name")
                .and_then(Value::as_str)
                .unwrap_or("<missing>")
                .to_owned();
            self.seen.lock().expect("policy lock").push(name.clone());
            if name == "Secret" {
                return Err(GraphQLError::new("denied Secret type introspection"));
            }
        }
        Ok(())
    }
}

fn schema() -> Schema {
    let secret = ObjectType::new("Secret").field(Field::new("token", TypeRef::string()));
    let query = ObjectType::new("Query").field(Field::new("hello", TypeRef::string()));
    let mut builder = Schema::builder();
    builder.add_type(secret.into());
    builder.add_type(query.into());
    builder.query("Query").build().expect("valid schema")
}

fn assert_denied(label: &str, policy: &TypeNamePolicy, response: gql::Response) {
    let seen = policy.seen.lock().expect("policy lock").clone();
    assert_eq!(seen, ["Secret"], "{label}: authorizer arguments");
    assert_eq!(response.errors.len(), 1, "{label}: {response:?}");
    assert_eq!(
        response.errors[0].message(),
        "denied Secret type introspection"
    );
    let root = response
        .data
        .as_ref()
        .and_then(Value::as_object)
        .unwrap_or_else(|| panic!("{label}: response has no object data: {response:?}"));
    assert!(
        root.get("__type").is_some_and(Value::is_null),
        "{label}: denied descriptor must be null: {response:?}"
    );
}

#[test]
fn meta_field_authorizer_receives_coerced_arguments_on_dynamic_and_prepared_paths() {
    let schema = schema();
    let cases = [
        (
            "literal",
            "{ __type(name: \"Secret\") { name fields { name } } }",
            None,
        ),
        (
            "variable",
            "query Q($name: String!) { __type(name: $name) { name fields { name } } }",
            Some(Object::from_fields([("name", Value::from("Secret"))])),
        ),
    ];

    for (label, source, variables) in cases {
        let policy = TypeNamePolicy::default();
        let options = ExecutionOptions::default().authorizer(policy.clone());
        let dynamic = futures::executor::block_on(gql::execute(
            &schema,
            Request::new(source).variables(variables.as_ref().unwrap_or(&Object::new())),
            &options,
        ));
        assert_denied(&format!("dynamic-{label}"), &policy, dynamic);

        let policy = TypeNamePolicy::default();
        let options = ExecutionOptions::default().authorizer(policy.clone());
        let prepared = PreparedQuery::new(&schema, label, source, None).expect("prepared");
        let planned = futures::executor::block_on(prepared.execute(variables.as_ref(), &options));
        assert_denied(&format!("prepared-{label}"), &policy, planned);
    }
}
