//! This stuff exists because converting `serde` to `erased_serde` requires ownership of the `Serializer`.
//!
//! TODO: Try and fix upstream

use std::fmt;

use super::{serializer::SerializeImpl, Serializer};

pub trait AnyOwnedSerializer: sealed::Sealed {}

mod sealed {
    // This prevents downstream user's from implementing the trait due to the private supertrait.
    pub trait Sealed {
        // This method is hidden from the public API as it has `erased_serde` in it.
        fn get(&mut self) -> &mut dyn erased_serde::Serializer;
    }

    impl<T: erased_serde::Serializer> Sealed for T {
        fn get(&mut self) -> &mut dyn erased_serde::Serializer {
            self
        }
    }

    impl<T: Sealed> super::AnyOwnedSerializer for T {}
}

/// TODO: Replace this with `Serialize::new`. Will likely need upstream work in `erased_serde` or to fork it.
pub struct OwnedSerializer<S>(S);

impl<S> fmt::Debug for OwnedSerializer<S> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("OwnedSerializer").finish()
    }
}

impl<S> OwnedSerializer<S> {
    pub fn new(serializer: S) -> OwnedSerializer<impl AnyOwnedSerializer>
    where
        S: serde::Serializer,
    {
        OwnedSerializer(<dyn erased_serde::Serializer>::erase(serializer))
    }

    pub fn serialize(&mut self) -> Serializer<'_>
    where
        S: AnyOwnedSerializer,
    {
        Serializer(SerializeImpl::Regular(self.0.get()))
    }
}
