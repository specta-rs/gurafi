// TODO: Rust lints

use std::borrow::Cow;

use async_graphql::{
    dynamic::{self, Field, FieldFuture, Object, SchemaError, TypeRef},
    Value,
};
use gql::{
    serializer::OwnedSerializer, GraphQLResolveInfo, NamedOutputType, OutputType, ResolveFn,
    ResolveFnArgs,
};
use schema_builder::SchemaBuilder;

mod schema_builder;

pub fn build_schema(schema: gql::Schema) -> Result<dynamic::Schema, SchemaError> {
    let mut builder = SchemaBuilder::new(dynamic::Schema::build(
        &schema.query.name,
        schema.mutation.as_ref().map(|t| &*t.name),
        schema.subscription.as_ref().map(|t| &*t.name),
    ));

    build_object(&mut builder, schema.query);
    if let Some(mutation) = schema.mutation {
        build_object(&mut builder, mutation);
    }
    if let Some(subscription) = schema.subscription {
        build_object(&mut builder, subscription);
    }

    builder.into_inner().finish()
}

fn build_object(schema: &mut SchemaBuilder, object_ty: gql::ObjectType) {
    debug_assert!(object_ty.interfaces.is_none()); // TODO: Implement interfaces
    debug_assert!(object_ty.is_type_of.is_none()); // TODO: Implement is_type_of

    let mut object = Object::new(object_ty.name);

    if let Some(description) = object_ty.description {
        object = object.description(description);
    }

    for (field_name, field_cfg) in object_ty.fields.get() {
        debug_assert!(field_cfg.args.is_none()); // TODO: Implement args

        let mut field = Field::new(
            field_name.clone(),
            resolve_output_ty(schema, field_cfg.ty),
            {
                let resolve = field_cfg.resolve;

                move |_| {
                    // TODO: Serializing to a buf then deserializing is super inefficient
                    let mut buf = Vec::new();
                    let mut serializer = serde_json::Serializer::new(&mut buf);
                    let mut serializer = OwnedSerializer::new(&mut serializer);

                    let resolve = resolve.as_ref().unwrap(); // TODO: Error handling
                    let args = ResolveFnArgs {
                        context: &(), // TODO: Properly hook up context
                        info: GraphQLResolveInfo {
                            field_name: field_name.as_ref(),
                        },
                    };
                    let result = serializer.serialize();
                    match resolve {
                        ResolveFn::Ptr(v) => v(args, result),
                        ResolveFn::Boxed(v) => v(args, result),
                        // TODO: Async resolvers working
                        // ResolveFn::PtrAsync(v) => v(args, result).await,
                        // ResolveFn::BoxedAsync(v) => v(args, result).await,
                        _ => todo!(),
                    }

                    drop(serializer);
                    let output: Value = serde_json::from_slice(&buf).unwrap(); // TODO: Remove this cause it's slow and error prone

                    FieldFuture::new(async { Ok(Some(output)) })
                }
            },
        );

        if let Some(description) = field_cfg.description {
            field = field.description(description);
        }

        if let Some(deprecation_reason) = field_cfg.deprecation_reason {
            field = field.deprecation(Some(&deprecation_reason));
        }

        object = object.field(field);
    }

    schema.apply(|s| s.register(object));
}

fn resolve_output_ty(schema: &mut SchemaBuilder, ty: gql::OutputType) -> TypeRef {
    match ty {
        OutputType::Nullable(ty) => TypeRef::named(resolve_named_output_ty(schema, ty)),
        OutputType::NonNullable(ty) => TypeRef::named_nn(resolve_named_output_ty(schema, ty)),
        // TODO: Dealing with list vs item nullability
        OutputType::List(ty) => TypeRef::named_list(resolve_named_output_ty(schema, ty)),
    }
}

fn resolve_named_output_ty(
    schema: &mut SchemaBuilder,
    ty: gql::NamedOutputType,
) -> Cow<'static, str> {
    match ty {
        NamedOutputType::Scalar(ty) => {
            // TODO: Make this a bit more typesafe
            match &*ty.name {
                "String" => TypeRef::STRING.into(),
                // "Int" => "Int".into(),
                // "Float" => "Float".into(),
                // "Boolean" => "Boolean".into(),
                // "ID" => "ID".into(),
                _ => todo!(),
            }
        }
        NamedOutputType::Object(ty) => {
            let name = ty.name.clone();
            build_object(schema, ty);
            name
        }
    }
}
