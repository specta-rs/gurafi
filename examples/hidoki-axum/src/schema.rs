use std::collections::HashMap;

use gql::*;

pub fn basic() -> Schema {
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
                        resolve: Some(ResolveFn::Ptr(|_, result| {
                            result.serialize_str("Hello World")
                        })),
                    },
                );

                fields.insert(
                    "nested".into(),
                    FieldConfig {
                        description: None,
                        deprecation_reason: None,
                        args: None,
                        ty: OutputType::NonNullable(
                            ObjectType {
                                name: "Nested".into(),
                                description: None,
                                fields: {
                                    let mut fields = HashMap::new();

                                    fields.insert(
                                        "hidoki".into(),
                                        FieldConfig {
                                            description: None,
                                            deprecation_reason: None,
                                            args: None,
                                            ty: OutputType::NonNullable(
                                                ScalarType::string().into(),
                                            ),
                                            resolve: Some(ResolveFn::Ptr(|_, result| {
                                                result.serialize_str("Hello World Hidoki")
                                            })),
                                        },
                                    );

                                    fields.into()
                                },
                                interfaces: None,
                                is_type_of: None,
                            }
                            .into(),
                        ),
                        resolve: Some(ResolveFn::Ptr(|_, result| result.serialize_str("Broken"))), // TODO: This is wrong
                    },
                );

                fields.into()
            },
            is_type_of: None,
            interfaces: None,
        },
        mutation: None,
        subscription: None,
    }
}
