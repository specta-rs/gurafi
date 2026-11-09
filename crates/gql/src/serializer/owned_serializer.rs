//! This stuff exists because converting `serde` to the object-safe traits in this module requires
//! ownership of the `Serializer`.

use std::fmt;

use super::{SerdeAdapter, Serializer, serializer::SerializeImpl};

pub struct OwnedSerializer<S: serde::Serializer>(SerdeAdapter<S>);

impl<S: serde::Serializer> fmt::Debug for OwnedSerializer<S> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("OwnedSerializer").finish()
    }
}

impl<S: serde::Serializer> OwnedSerializer<S> {
    pub fn new(serializer: S) -> Self {
        OwnedSerializer(SerdeAdapter::new(serializer))
    }

    pub fn serialize(&mut self) -> Serializer<'_> {
        Serializer(SerializeImpl::Regular(&mut self.0))
    }
}
