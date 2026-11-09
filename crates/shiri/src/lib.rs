//! shit-serialize: The worse serialization library you've ever seen.
//!
//! TODO: Docs but main points are:
//!  - Fully object-safe. Useful for dependency injection.
//!  - Support for nested maps without unnecessary allocations.
//!
//! This is also a replacement for `erased-serde` with better type-safety
//!
//! Maybe support for async serialisation???

mod json;
pub mod serde;

pub use json::Json;

use crate::serde::{SerdeInterop, SerdeInterop2};

// TODO: Error handling + ok variant
// TODO: Wrap in struct that takes `self` for type safety
// TODO: Should this be public, probs so people can implement it but it should be hidden away.
pub trait Serializer {
    // TODO: Other primitive types

    fn serialize_str(&mut self, value: &str) -> Result<(), ()>;

    // TODO: Sequence's

    // TODO: Capacity like Serde
    fn serialize_map(&mut self) -> Result<SerializeMap2, ()>;
}

pub trait SerializeMap {
    // TODO: Allow any `key` type
    fn serialize_entry(&mut self, key: String) -> Result<Serializer2, ()>;

    fn end(&mut self);
}

pub struct Serializer2<'a>(&'a mut dyn Serializer);

impl<'a> Serializer2<'a> {
    pub fn new(inner: &'a mut dyn Serializer) -> Self {
        Serializer2(inner)
    }

    pub fn serialize_str(self, value: &str) -> Result<(), ()> {
        self.0.serialize_str(value)
    }

    // TODO: Serde feature only
    pub fn serialize_with_serde<T: ::serde::Serialize>(self, value: &T) -> Result<(), ()> {
        value.serialize(SerdeInterop2(self)).unwrap();
        Ok(())
    }

    pub fn serialize_map(self) -> Result<SerializeMap2<'a>, ()> {
        self.0.serialize_map() // TODO: Should this replace the need for `SerializeMap2::new` being public???
    }
}

pub struct SerializeMap2<'a> {
    inner: &'a mut dyn SerializeMap,
}

impl<'a> SerializeMap2<'a> {
    pub fn new(inner: &'a mut dyn SerializeMap) -> Self {
        SerializeMap2 { inner }
    }

    pub fn serialize_entry(&mut self, key: impl Into<String>) -> Result<Serializer2, ()> {
        // TODO: Panic on duplicate key or just allow it????

        self.inner.serialize_entry(key.into())
    }

    pub fn end(self) {
        self.inner.end()
    }
}
