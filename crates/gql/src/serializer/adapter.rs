//! An adapter from a `serde` serializer to the object-safe traits in this module.

use serde::ser::{SerializeMap as _, SerializeSeq as _};

use super::traits::{ErasedValue, MapImpl, SeqImpl, SerializerImpl};

/// Owns a `serde` serializer and drives it through the object-safe traits.
///
/// The state machine exists because `serde`'s serializers are consumed when a map or sequence is
/// started. The serializer is moved into the map/sequence, which is moved back here.
pub(crate) struct SerdeAdapter<S: serde::Serializer> {
    serializer: Option<S>,
    map: Option<S::SerializeMap>,
    seq: Option<S::SerializeSeq>,
}

impl<S: serde::Serializer> SerdeAdapter<S> {
    pub(crate) fn new(serializer: S) -> Self {
        Self {
            serializer: Some(serializer),
            map: None,
            seq: None,
        }
    }

    fn take_serializer(&mut self) -> S {
        self.serializer
            .take()
            .expect("SerdeAdapter: serializer already consumed")
    }
}

macro_rules! impl_scalar {
    ($($method:ident($ty:ty)),* $(,)?) => {
        $(
            fn $method(&mut self, value: $ty) {
                self.take_serializer().$method(value).unwrap();
            }
        )*
    };
}

impl<S: serde::Serializer> SerializerImpl for SerdeAdapter<S> {
    impl_scalar!(
        serialize_bool(bool),
        serialize_i8(i8),
        serialize_i16(i16),
        serialize_i32(i32),
        serialize_i64(i64),
        serialize_u8(u8),
        serialize_u16(u16),
        serialize_u32(u32),
        serialize_u64(u64),
        serialize_f32(f32),
        serialize_f64(f64),
        serialize_i128(i128),
        serialize_u128(u128),
        serialize_char(char),
        serialize_str(&str),
        serialize_bytes(&[u8]),
    );

    fn serialize_none(&mut self) {
        self.take_serializer().serialize_none().unwrap();
    }

    fn serialize_some(&mut self, value: &dyn erased_serde::Serialize) {
        self.take_serializer()
            .serialize_some(&ErasedValue(value))
            .unwrap();
    }

    fn serialize_unit(&mut self) {
        self.take_serializer().serialize_unit().unwrap();
    }

    fn serialize_unit_struct(&mut self, name: &'static str) {
        self.take_serializer().serialize_unit_struct(name).unwrap();
    }

    fn serialize_unit_variant(
        &mut self,
        name: &'static str,
        variant_index: u32,
        variant: &'static str,
    ) {
        self.take_serializer()
            .serialize_unit_variant(name, variant_index, variant)
            .unwrap();
    }

    fn serialize_newtype_struct(
        &mut self,
        name: &'static str,
        value: &dyn erased_serde::Serialize,
    ) {
        self.take_serializer()
            .serialize_newtype_struct(name, &ErasedValue(value))
            .unwrap();
    }

    fn serialize_newtype_variant(
        &mut self,
        name: &'static str,
        variant_index: u32,
        variant: &'static str,
        value: &dyn erased_serde::Serialize,
    ) {
        self.take_serializer()
            .serialize_newtype_variant(name, variant_index, variant, &ErasedValue(value))
            .unwrap();
    }

    fn serialize_seq(&mut self, len: Option<usize>) -> Result<&mut dyn SeqImpl, ()> {
        let serializer = self.take_serializer();
        self.seq = Some(serializer.serialize_seq(len).map_err(|_| ())?);
        Ok(self)
    }

    fn serialize_map(&mut self, len: Option<usize>) -> Result<&mut dyn MapImpl, ()> {
        let serializer = self.take_serializer();
        self.map = Some(serializer.serialize_map(len).map_err(|_| ())?);
        Ok(self)
    }

    fn serialize_serde(&mut self, value: &dyn erased_serde::Serialize) {
        SerializerImpl::serialize_some(self, value);
    }
}

impl<S: serde::Serializer> MapImpl for SerdeAdapter<S> {
    fn serialize_key(&mut self, key: &dyn erased_serde::Serialize) -> Result<(), ()> {
        self.map
            .as_mut()
            .ok_or(())?
            .serialize_key(&ErasedValue(key))
            .map_err(|_| ())
    }

    fn serialize_value(&mut self, value: &dyn erased_serde::Serialize) -> Result<(), ()> {
        self.map
            .as_mut()
            .ok_or(())?
            .serialize_value(&ErasedValue(value))
            .map_err(|_| ())
    }

    fn end(&mut self) {
        if let Some(map) = self.map.take() {
            let _ = map.end();
        }
    }
}

impl<S: serde::Serializer> SeqImpl for SerdeAdapter<S> {
    fn serialize_element(&mut self, value: &dyn erased_serde::Serialize) -> Result<(), ()> {
        self.seq
            .as_mut()
            .ok_or(())?
            .serialize_element(&ErasedValue(value))
            .map_err(|_| ())
    }

    fn end(&mut self) {
        if let Some(seq) = self.seq.take() {
            let _ = seq.end();
        }
    }
}
