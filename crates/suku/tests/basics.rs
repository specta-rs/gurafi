use std::collections::HashMap;

use gql::{FieldConfig, ObjectType, OutputType, ResolveFn, ScalarType, Schema};

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
                        resolve: Some(ResolveFn::Ptr(|_args, result| {
                            result.serialize_str(&"Hello World")
                        })),
                    },
                );

                fields.into()
            },
            interfaces: None,
            is_type_of: None,
        },
        mutation: None,
        subscription: None,
    }
}

#[test]
fn basic() {
    let schema = basic_schema();

    // TODO: Properly test the server works
}

#[test]
fn alias() {
    // TODO
    // {
    //     hello,
    //     nested {
    //         suku: suku2
    //     }
    // }
}
