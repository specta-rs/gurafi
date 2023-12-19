use std::marker::PhantomData;

use crate::ResolverFn;

pub struct FieldBuilder<M> {
    phantom: PhantomData<M>,
}

impl<M> FieldBuilder<M> {
    // TODO: Allow async or sync functions
    // TODO: Allow results
    // TODO: Context type. How is that gonna be inferred through?
    pub fn new<R>(resolver: R) -> FieldBuilder<M>
    where
        R: ResolverFn<M>, // + Fn(M) -> R::Result,
    {
        // TODO

        Self {
            phantom: PhantomData,
        }
    }

    // TODO: https://pothos-graphql.dev/docs/api/field-builder#fieldoptions-1

    // TODO: Field nullability - Deal with array vs item nullability
}

/// Internal constructor used by `expose!`
pub fn expose_field<M>() -> FieldBuilder<M> {
    todo!();
}
