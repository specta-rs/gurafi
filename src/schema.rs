use std::{borrow::Cow, marker::PhantomData};

use gql::{FieldConfig, NamedOutputType, OutputType, ResolveFn, ScalarType};

use crate::{object_builder::ObjectBuilder, ModelBuilder};

pub struct Schema<Ctx = ()> {
    query: ObjectBuilder,
    mutation: ObjectBuilder,
    subscription: ObjectBuilder,
    phantom: PhantomData<Ctx>,
}

impl<Ctx> Schema<Ctx> {
    pub fn new() -> Self {
        Self {
            query: ObjectBuilder::new("Query".into()),
            mutation: ObjectBuilder::new("Mutation".into()),
            subscription: ObjectBuilder::new("Subscription".into()),
            phantom: PhantomData,
        }
    }

    pub fn with(mut self, mount: impl FnOnce(&mut Self)) -> Self {
        (mount)(&mut self);
        self
    }

    pub fn object_type<M>(&mut self, name: impl Into<Cow<'static, str>>) -> ModelBuilder<M> {
        // TODO: Error on duplicate `name` (or just store to vec and validate in build step)

        ModelBuilder::new(name.into())
    }

    // // Can only be called once and defines the whole root query type.
    // pub fn query_type(self, model: ()) -> Self {
    //     // TODO
    //     self
    // }

    // TODO: Allow async or sync functions
    // TODO: Allow results
    pub fn query_field<T>(
        &mut self,
        name: impl Into<Cow<'static, str>>,
        // TODO: Take in a `FieldBuilder` for extra configuration
        // TODO: Prevent using `expose!` and similar on the `FieldBuilder`.
        resolver: impl Fn() -> T + Send + Sync + 'static,
    ) {
        self.query.field(
            name,
            FieldConfig {
                // TODO: How does the end user set these???
                description: None,
                deprecation_reason: None,
                args: None,

                // TODO: Infer `ty` from `T`
                ty: OutputType::NonNullable(NamedOutputType::Scalar(ScalarType::string())),
                resolve: Some(ResolveFn::Boxed(Box::new(move |_, result| {
                    let actual_result = resolver();
                    // TODO: How to serialize result
                    result.serialize_str(&"todo");
                }))),
            },
        );

        // TODO

        // TODO: Return a builder which allows setting description, deprecation, etc.
    }

    // TODO: `mutation_type` & `mutation_field`

    // TODO: Allow merging schema's

    // TODO: Export to GraphQL file

    pub fn build(self) -> Result<gql::Schema, ()> {
        Ok(gql::Schema {
            query: self.query.build(),
            mutation: (self.mutation.fields_len() != 0).then(|| self.mutation.build()),
            subscription: (self.subscription.fields_len() != 0).then(|| self.subscription.build()),
        })
    }
}
