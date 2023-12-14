use std::collections::HashMap;

use gql::{FieldConfig, ObjectType, OutputType, ScalarType, Schema};

pub fn basic_schema() -> Schema {
    Schema {
        query: ObjectType {
            name: "Query".into(),
            description: None,
            fields: {
                let mut fields = HashMap::new();

                fields.insert(
                    "hello".into(),
                    FieldConfig {
                        description: None,
                        deprecation_reason: None,
                        args: None,
                        ty: OutputType::NonNullable(ScalarType::string().into()),
                        resolve: Some(|| "Hello World".to_string()),
                    },
                );

                fields.into()
            },
        },
        mutation: None,
    }
}

#[test]
fn basic() {
    let schema = basic_schema();

    // TODO: Properly test the server works
}
