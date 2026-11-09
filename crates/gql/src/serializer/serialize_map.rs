use std::fmt;

use super::{MapImpl, Serializer, serializer::SerializeImpl};

pub struct SerializeMap<'a>(pub(crate) &'a mut dyn MapImpl);

impl<'a> fmt::Debug for SerializeMap<'a> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("SerializeMap").finish()
    }
}

// TODO: Error type
impl<'a> SerializeMap<'a> {
    pub fn serialize_key<T: serde::Serialize>(&mut self, key: &T) -> Result<(), ()> {
        self.0.serialize_key(key)
    }

    pub fn serialize_value<T: serde::Serialize>(&mut self, value: &T) -> Result<(), ()> {
        self.0.serialize_value(value)
    }

    pub fn serialize_entry<K: serde::Serialize, V: serde::Serialize>(
        &mut self,
        key: &K,
        value: &V,
    ) -> Result<(), ()> {
        self.serialize_key(key)?;
        self.serialize_value(value)
    }

    // TODO: Rename this method
    // This is not a serde-thing.
    // It could be achieved with `serde_key` + `serialize_value` but it would require more downstream work.
    pub fn serialize_entry_serializer<K: serde::Serialize>(
        &mut self,
        key: &K,
    ) -> Result<Serializer<'_>, ()> {
        self.0.serialize_key(key)?;
        Ok(Serializer(SerializeImpl::MapValue(&mut *self.0)))
    }

    pub fn end(self) {
        self.0.end();
    }
}
