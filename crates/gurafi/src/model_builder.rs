use std::{borrow::Cow, collections::HashMap};

use crate::FieldBuilder;

pub struct ModelBuilder<M> {
    name: Cow<'static, str>,
    description: Cow<'static, str>,
    fields: HashMap<Cow<'static, str>, FieldBuilder<M>>,
}

impl<M> ModelBuilder<M> {
    pub(crate) fn new(name: Cow<'static, str>) -> Self {
        Self {
            name,
            description: Cow::Borrowed(""),
            fields: HashMap::new(),
        }
    }

    pub fn description(mut self, description: impl Into<Cow<'static, str>>) -> Self {
        self.description = description.into();
        self
    }

    // TODO: Deprecated attributes???

    pub fn field(mut self, name: impl Into<Cow<'static, str>>, field: FieldBuilder<M>) -> Self {
        self.fields.insert(name.into(), field);
        self
    }
}
