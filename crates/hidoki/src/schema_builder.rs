use async_graphql::dynamic;

/// A wrapper around `SchemaBuilder` that allows for `&mut self` instead of `self`.
pub(crate) struct SchemaBuilder(Option<dynamic::SchemaBuilder>);

impl SchemaBuilder {
    pub fn new(builder: dynamic::SchemaBuilder) -> Self {
        Self(Some(builder))
    }

    pub fn apply(&mut self, func: impl FnOnce(dynamic::SchemaBuilder) -> dynamic::SchemaBuilder) {
        self.0 = Some(func(self.0.take().unwrap()));
    }

    pub fn into_inner(self) -> dynamic::SchemaBuilder {
        self.0.unwrap()
    }
}
