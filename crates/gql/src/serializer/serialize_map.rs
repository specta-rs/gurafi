use std::fmt;

use super::{serializer::SerializeImpl, Serializer};

pub struct SerializeMap<'a>(pub(crate) SerializeMapImpl<'a>);

pub enum SerializeMapImpl<'a> {
    Map(&'a mut dyn erased_serde::SerializeMap),
    // TODO: This variant is inherently inefficient because we collect up in memory, then serialize instead of doing both at once.
    // However Serde doesn't provide a better way to do this.
    SerdeValue {
        value: serde_value::Serializer,
        // `value` will be written to `parent` when done
        parent: &'a mut dyn erased_serde::SerializeMap,
    },
}

impl<'a> fmt::Debug for SerializeMap<'a> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("SerializeMap").finish()
    }
}

// TODO: Error type
impl<'a> SerializeMap<'a> {
    pub fn serialize_key<T: serde::Serialize>(&mut self, key: &T) -> Result<(), ()> {
        match &mut self.0 {
            SerializeMapImpl::Map(serializer) => {
                serializer.erased_serialize_key(key).map_err(|_| ())
            }
            SerializeMapImpl::SerdeValue { value, parent } => {
                // value.serialize(key).map_err(|_| ())?;
                // parent.erased_serialize_value(&value).map_err(|_| ())
                todo!();
            }
        }
    }

    pub fn serialize_value<T: serde::Serialize>(&mut self, value: &T) -> Result<(), ()> {
        // self.0.erased_serialize_value(value).map_err(|_| ())
        match &mut self.0 {
            SerializeMapImpl::Map(serializer) => {
                serializer.erased_serialize_value(value).map_err(|_| ())
            }
            SerializeMapImpl::SerdeValue { value, parent } => {
                // value.serialize(value).map_err(|_| ())?;
                // parent.erased_serialize_value(&value).map_err(|_| ())
                todo!();
            }
        }
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
    ) -> Result<Serializer, ()> {
        match &mut self.0 {
            SerializeMapImpl::Map(serializer) => {
                serializer.erased_serialize_key(key).map_err(|_| ())?;
                Ok(Serializer(SerializeImpl::MapValue(&mut **serializer)))
            }
            SerializeMapImpl::SerdeValue { value, parent } => {
                // Ok(Serializer(SerializeImpl::M {
                //     value: serde_value::Serializer,
                //     parent: &mut **serializer,
                // }))
                todo!();

                // serializer.erased_serialize_key(key).map_err(|_| ())?;
                // Ok(Serializer(SerializeImpl::MapValue(&mut **serializer)))
            }
        }
    }

    pub fn end(self) {
        match self.0 {
            SerializeMapImpl::Map(serializer) => serializer.erased_end(),
            SerializeMapImpl::SerdeValue { value, parent } => {
                todo!();
            }
        }
    }
}
