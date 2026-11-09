use std::fmt;

use super::{serialize_map::SerializeMapImpl, serialize_seq::SerializeSeq, SerializeMap};

/// TODO
// A wrapper around the object-safe [`Serializer`](super::Serializer) trait.
pub struct Serializer<'a>(pub(crate) SerializeImpl<'a>);

pub(crate) enum SerializeImpl<'a> {
    Regular(&'a mut dyn erased_serde::Serializer),
    // In this state we assume `.serialize_key` was called first.
    MapValue(&'a mut dyn erased_serde::SerializeMap),
}

impl<'a> fmt::Debug for Serializer<'a> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("Serializer").finish()
    }
}

impl<'a> Serializer<'a> {
    pub fn new<S: serde::Serializer>(serializer: S) -> Self {
        // TODO: Can we modify `erased-serde` to support this and drop `super::owned_serializer` completely?
        todo!();
    }
}

macro_rules! dispatch {
    ($self:ident; $value: ident; $method:ident) => {
        match $self.0 {
            SerializeImpl::Regular(serializer) => serializer.$method($value),
            SerializeImpl::MapValue(serializer) => {
                // TODO: Error handling
                serializer.erased_serialize_value(&$value).unwrap()
            }
        }
    };
}

impl<'a> Serializer<'a> {
    pub fn serialize_bool(self, value: bool) {
        dispatch!(self; value; erased_serialize_bool);
    }

    pub fn serialize_i8(self, value: i8) {
        dispatch!(self; value; erased_serialize_i8);
    }

    pub fn serialize_i16(self, value: i16) {
        dispatch!(self; value; erased_serialize_i16);
    }

    pub fn serialize_i32(self, value: i32) {
        dispatch!(self; value; erased_serialize_i32);
    }

    pub fn serialize_i64(self, value: i64) {
        dispatch!(self; value; erased_serialize_i64);
    }

    pub fn serialize_u8(self, value: u8) {
        dispatch!(self; value; erased_serialize_u8);
    }

    pub fn serialize_u16(self, value: u16) {
        dispatch!(self; value; erased_serialize_u16);
    }

    pub fn serialize_u32(self, value: u32) {
        dispatch!(self; value; erased_serialize_u32);
    }

    pub fn serialize_u64(self, value: u64) {
        dispatch!(self; value; erased_serialize_u64);
    }

    pub fn serialize_f32(self, value: f32) {
        dispatch!(self; value; erased_serialize_f32);
    }

    pub fn serialize_f64(self, value: f64) {
        dispatch!(self; value; erased_serialize_f64);
    }

    pub fn serialize_i128(self, value: i128) {
        dispatch!(self; value; erased_serialize_i128);
    }

    pub fn serialize_u128(self, value: u128) {
        dispatch!(self; value; erased_serialize_u128);
    }

    pub fn serialize_char(self, value: char) {
        dispatch!(self; value; erased_serialize_char);
    }

    pub fn serialize_str(self, value: &str) {
        dispatch!(self; value; erased_serialize_str);
    }

    pub fn serialize_bytes(self, value: &[u8]) {
        dispatch!(self; value; erased_serialize_bytes);
    }

    pub fn serialize_none(self) {
        match self.0 {
            SerializeImpl::Regular(serializer) => serializer.erased_serialize_none(),
            SerializeImpl::MapValue(serializer) => serializer
                .erased_serialize_value(&(None as Option<()>))
                .unwrap(),
        }
    }

    pub fn serialize_some<T: serde::Serialize>(self, value: &T) {
        dispatch!(self; value; erased_serialize_some);
    }

    pub fn serialize_unit(self) {
        match self.0 {
            SerializeImpl::Regular(serializer) => serializer.erased_serialize_unit(),
            SerializeImpl::MapValue(serializer) => todo!(),
        }
    }

    pub fn serialize_unit_struct(self, name: &'static str) {
        match self.0 {
            SerializeImpl::Regular(serializer) => serializer.erased_serialize_unit_struct(name),
            SerializeImpl::MapValue(serializer) => todo!(),
        }
    }

    pub fn serialize_unit_variant(
        self,
        name: &'static str,
        variant_index: u32,
        variant: &'static str,
    ) {
        match self.0 {
            SerializeImpl::Regular(serializer) => {
                serializer.erased_serialize_unit_variant(name, variant_index, variant)
            }
            SerializeImpl::MapValue(serializer) => todo!(),
        }
    }

    pub fn serialize_newtype_struct<T: serde::Serialize>(self, name: &'static str, value: &T) {
        match self.0 {
            SerializeImpl::Regular(serializer) => {
                serializer.erased_serialize_newtype_struct(name, value)
            }
            SerializeImpl::MapValue(serializer) => todo!(),
        }
    }

    pub fn serialize_newtype_variant<T: serde::Serialize>(
        self,
        name: &'static str,
        variant_index: u32,
        variant: &'static str,
        value: &T,
    ) {
        match self.0 {
            SerializeImpl::Regular(serializer) => {
                serializer.erased_serialize_newtype_variant(name, variant_index, variant, value)
            }
            SerializeImpl::MapValue(serializer) => todo!(),
        }
    }

    pub fn serialize_seq(self, len: Option<usize>) -> Result<SerializeSeq<'a>, ()> {
        match self.0 {
            SerializeImpl::Regular(serializer) => serializer
                .erased_serialize_seq(len)
                .map(SerializeSeq)
                .map_err(|_| ()),
            SerializeImpl::MapValue(serializer) => todo!(),
        }
    }

    // pub fn serialize_tuple(
    //     self,
    //     len: usize
    // ) -> Result<&mut dyn SerializeTuple, ErrorImpl>

    // pub fn serialize_tuple_struct(
    //     self,
    //     name: &'static str,
    //     len: usize
    // ) -> Result<&mut dyn SerializeTupleStruct, ErrorImpl>

    // pub fn serialize_tuple_variant(
    //     self,
    //     name: &'static str,
    //     variant_index: u32,
    //     variant: &'static str,
    //     len: usize
    // ) -> Result<&mut dyn SerializeTupleVariant, ErrorImpl>

    // TODO: Error variant
    pub fn serialize_map(self, len: Option<usize>) -> Result<SerializeMap<'a>, ()> {
        match self.0 {
            SerializeImpl::Regular(serializer) => serializer
                .erased_serialize_map(len)
                .map(|v| SerializeMap(SerializeMapImpl::Map(v)))
                .map_err(|_| ()),
            SerializeImpl::MapValue(serializer) => Ok(SerializeMap(SerializeMapImpl::SerdeValue {
                value: serde_value::Serializer,
                parent: serializer,
            })),
        }
    }

    // pub fn serialize_struct(
    //     self,
    //     name: &'static str,
    //     len: usize
    // ) -> Result<&mut dyn SerializeStruct, ErrorImpl>

    // pub fn serialize_struct_variant(
    //     self,
    //     name: &'static str,
    //     variant_index: u32,
    //     variant: &'static str,
    //     len: usize
    // ) -> Result<&mut dyn SerializeStructVariant, ErrorImpl>

    pub fn serialize_serde<T: serde::Serialize>(self, value: &T) {
        match self.0 {
            SerializeImpl::Regular(serializer) => serializer.erased_serialize_some(value),
            SerializeImpl::MapValue(serializer) => todo!(),
        }
    }
}
