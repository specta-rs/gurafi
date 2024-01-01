// TODO: Should this be moved back into the `gql` crate???

use std::{borrow::Cow, collections::HashMap};

use gql::{FieldConfig, ObjectType, Thunk};

pub(crate) struct ObjectBuilder {
    name: Cow<'static, str>,
    description: Option<Cow<'static, str>>,
    fields: HashMap<Cow<'static, str>, FieldConfig>,
}

impl ObjectBuilder {
    pub fn new(name: Cow<'static, str>) -> Self {
        Self {
            name,
            description: None,
            fields: HashMap::new(),
        }
    }

    pub fn description(&mut self, description: impl Into<Cow<'static, str>>) {
        self.description = Some(description.into());
    }

    pub fn field(&mut self, name: impl Into<Cow<'static, str>>, config: FieldConfig) {
        self.fields.insert(name.into(), config);
    }

    pub fn fields_len(&self) -> usize {
        self.fields.len()
    }

    pub fn build(self) -> ObjectType {
        ObjectType {
            name: self.name,
            description: self.description,
            interfaces: None,
            is_type_of: None,
            fields: Thunk::Eager(self.fields),
        }
    }
}
