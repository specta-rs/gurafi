//! Object-safe traits used by [`Serializer`](super::Serializer).
//!
//! These traits wrap the format-specific `serde` serializer types so the rest of the crate can stay
//! format agnostic.

/// The object-safe equivalent of [`serde::Serializer`].
pub(crate) trait SerializerImpl {
    fn serialize_bool(&mut self, value: bool);
    fn serialize_i8(&mut self, value: i8);
    fn serialize_i16(&mut self, value: i16);
    fn serialize_i32(&mut self, value: i32);
    fn serialize_i64(&mut self, value: i64);
    fn serialize_u8(&mut self, value: u8);
    fn serialize_u16(&mut self, value: u16);
    fn serialize_u32(&mut self, value: u32);
    fn serialize_u64(&mut self, value: u64);
    fn serialize_f32(&mut self, value: f32);
    fn serialize_f64(&mut self, value: f64);
    fn serialize_i128(&mut self, value: i128);
    fn serialize_u128(&mut self, value: u128);
    fn serialize_char(&mut self, value: char);
    fn serialize_str(&mut self, value: &str);
    fn serialize_bytes(&mut self, value: &[u8]);
    fn serialize_none(&mut self);
    fn serialize_some(&mut self, value: &dyn erased_serde::Serialize);
    fn serialize_unit(&mut self);
    fn serialize_unit_struct(&mut self, name: &'static str);
    fn serialize_unit_variant(
        &mut self,
        name: &'static str,
        variant_index: u32,
        variant: &'static str,
    );
    fn serialize_newtype_struct(&mut self, name: &'static str, value: &dyn erased_serde::Serialize);
    fn serialize_newtype_variant(
        &mut self,
        name: &'static str,
        variant_index: u32,
        variant: &'static str,
        value: &dyn erased_serde::Serialize,
    );
    fn serialize_seq(&mut self, len: Option<usize>) -> Result<&mut dyn SeqImpl, ()>;
    fn serialize_map(&mut self, len: Option<usize>) -> Result<&mut dyn MapImpl, ()>;
    fn serialize_serde(&mut self, value: &dyn erased_serde::Serialize);
}

/// The object-safe equivalent of [`serde::ser::SerializeMap`].
pub(crate) trait MapImpl {
    fn serialize_key(&mut self, key: &dyn erased_serde::Serialize) -> Result<(), ()>;
    fn serialize_value(&mut self, value: &dyn erased_serde::Serialize) -> Result<(), ()>;
    fn end(&mut self);

    // The remaining methods write a scalar into the map's current value slot.
    // They exist so `SerializeImpl::MapValue` can reuse the `dispatch!` macro.

    fn serialize_bool(&mut self, value: bool) {
        self.serialize_value(&value).unwrap();
    }

    fn serialize_i8(&mut self, value: i8) {
        self.serialize_value(&value).unwrap();
    }

    fn serialize_i16(&mut self, value: i16) {
        self.serialize_value(&value).unwrap();
    }

    fn serialize_i32(&mut self, value: i32) {
        self.serialize_value(&value).unwrap();
    }

    fn serialize_i64(&mut self, value: i64) {
        self.serialize_value(&value).unwrap();
    }

    fn serialize_u8(&mut self, value: u8) {
        self.serialize_value(&value).unwrap();
    }

    fn serialize_u16(&mut self, value: u16) {
        self.serialize_value(&value).unwrap();
    }

    fn serialize_u32(&mut self, value: u32) {
        self.serialize_value(&value).unwrap();
    }

    fn serialize_u64(&mut self, value: u64) {
        self.serialize_value(&value).unwrap();
    }

    fn serialize_f32(&mut self, value: f32) {
        self.serialize_value(&value).unwrap();
    }

    fn serialize_f64(&mut self, value: f64) {
        self.serialize_value(&value).unwrap();
    }

    fn serialize_i128(&mut self, value: i128) {
        self.serialize_value(&value).unwrap();
    }

    fn serialize_u128(&mut self, value: u128) {
        self.serialize_value(&value).unwrap();
    }

    fn serialize_char(&mut self, value: char) {
        self.serialize_value(&value).unwrap();
    }

    fn serialize_str(&mut self, value: &str) {
        self.serialize_value(&value).unwrap();
    }

    fn serialize_bytes(&mut self, value: &[u8]) {
        self.serialize_value(&value).unwrap();
    }

    fn serialize_none(&mut self) {
        let none: Option<()> = None;
        self.serialize_value(&none).unwrap();
    }

    fn serialize_some(&mut self, value: &dyn erased_serde::Serialize) {
        self.serialize_value(value).unwrap();
    }
}

/// The object-safe equivalent of [`serde::ser::SerializeSeq`].
pub(crate) trait SeqImpl {
    fn serialize_element(&mut self, value: &dyn erased_serde::Serialize) -> Result<(), ()>;
    fn end(&mut self);
}

/// Bridges an erased value back into `serde`'s generic API.
pub(crate) struct ErasedValue<'a>(pub(crate) &'a dyn erased_serde::Serialize);

impl serde::Serialize for ErasedValue<'_> {
    fn serialize<S: serde::Serializer>(&self, serializer: S) -> Result<S::Ok, S::Error> {
        erased_serde::serialize(self.0, serializer)
    }
}
