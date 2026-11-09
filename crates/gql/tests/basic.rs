//! A basic example schema.
//!
//! This package is mostly just types so we can't heavily test them but Suku's test will cover this crate.

use std::collections::HashMap;

use gql::{FieldConfig, ObjectType, OutputType, ResolveFn, ScalarType, Schema};

fn basic_schema() -> Schema {
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
                            result.serialize_str("world")
                        })),
                    },
                );

                fields.insert(
                    "hello2".into(),
                    FieldConfig {
                        description: None,
                        deprecation_reason: None,
                        args: None,
                        ty: OutputType::NonNullable(ScalarType::string().into()),
                        resolve: Some(ResolveFn::Ptr(|_args, result| {
                            result.serialize_some(&"world")
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
