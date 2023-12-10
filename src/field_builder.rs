use std::marker::PhantomData;

use crate::ResolverFn;

pub struct FieldBuilder<M> {
    phantom: PhantomData<M>,
}

impl<M> FieldBuilder<M> {
    // TODO: Allow async or sync functions
    // TODO: Allow results
    // TODO: Context type. How is that gonna be inferred through?
    pub fn new(resolver: impl ResolverFn<M>) -> FieldBuilder<M> {
        // TODO

        Self {
            phantom: PhantomData,
        }
    }

    // TODO: https://pothos-graphql.dev/docs/api/field-builder#fieldoptions-1
}

/// Internal constructor used by `expose!`
pub fn expose_field<M>() -> FieldBuilder<M> {
    todo!();
}
