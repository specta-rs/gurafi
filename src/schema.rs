use std::borrow::Cow;

pub struct Schema {}

impl Schema {
    pub fn new() -> Self {
        Self {}
    }

    // // Can only be called once and defines the whole root query type.
    // pub fn query_type(self, name: impl Into<Cow<'static, str>>) -> Self {
    //     // TODO
    //     self
    // }

    // TODO: Allow async or sync functions
    // TODO: Allow results
    pub fn query_field<T>(
        self,
        name: impl Into<Cow<'static, str>>,
        resolver: impl Fn() -> T,
    ) -> Self {
        // TODO

        self
    }

    // TODO: `mutation_type` & `mutation_field`

    // TODO: Allow merging schema's

    // TODO: Export to GraphQL file

    // TODO: Setup server

    // TODO: Register prepared queries
}
