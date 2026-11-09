use std::collections::HashMap;

use gql::{FieldConfig, ObjectType, OutputType, ResolveFn, ScalarType};

pub fn schema() -> ObjectType {
    ObjectType {
        name: "Schema".into(),
        description: None, // TODO: allow this to set by user
        fields: {
            let mut fields = HashMap::new();

            // TODO: https://spec.graphql.org/October2021/#sec-Schema-Introspection.Schema-Introspection-Schema

            fields.insert(
                "description".into(),
                FieldConfig {
                    description: None,
                    deprecation_reason: None,
                    args: None,
                    ty: OutputType::Nullable(ScalarType::string().into()),
                    resolve: Some(ResolveFn::Ptr(|_args, result| {
                        result.serialize_str(&"Hello World")
                    })), // TODO: Allow the user to set this
                },
            );

            // fields.insert(
            //     "queryType".into(),
            //     FieldConfig {
            //         description: None,
            //         deprecation_reason: None,
            //         args: None,
            //         ty: OutputType::Nullable(ScalarType::string().into()),
            //         resolve: Some(|ser| ser.erased_serialize_str("Hello World")), // TODO: Allow the user to set this
            //     },
            // );

            fields.into()
        },
        interfaces: None,
        is_type_of: None,
    }
}
