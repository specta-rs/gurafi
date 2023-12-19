use std::{borrow::Cow, marker::PhantomData};

use crate::ModelBuilder;

pub struct Schema<Ctx = ()> {
    phantom: PhantomData<Ctx>,
}

impl<Ctx> Schema<Ctx> {
    pub fn new() -> Self {
        Self {
            phantom: PhantomData,
        }
    }

    pub fn with(self, mount: impl FnOnce(&mut Self)) -> Self {
        todo!();
    }

    pub fn object_type<M>(&mut self, name: impl Into<Cow<'static, str>>) -> ModelBuilder<M> {
        todo!();
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
        resolver: impl Fn() -> T,
    ) {
        // TODO

        // TODO: Return a builder which allows setting description, deprecation, etc.
    }

    // TODO: `mutation_type` & `mutation_field`

    // TODO: Allow merging schema's

    // TODO: Export to GraphQL file

    // TODO: Setup server

    // TODO: Register prepared queries
}
