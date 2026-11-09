//! Serialization library for usage with the rest of the crate.
//!
//! The primary goal is to allow this crate to remain format agnostic.
//! This requires the serializer to remain object-safe so it can be injected by the server at runtime.
//!
//! This crate wraps the `erased_serde` types:
//!  - So they take `self` not `&mut self` for extra type safety.
//!  - `erased_serde` has not reached 1.0 yet so we don't want it in our public API.
//!

mod owned_serializer;
mod serialize_map;
mod serialize_seq;
mod serialize_struct;
mod serialize_struct_variant;
mod serialize_tuple;
mod serialize_tuple_struct;
mod serialize_tuple_variant;
mod serializer;

// This is hidden as an end-user shouldn't ever need it but it's public in-case they need to refer to it.
#[doc(hidden)]
pub use owned_serializer::AnyOwnedSerializer;

pub use owned_serializer::OwnedSerializer;
pub use serialize_map::SerializeMap;
pub use serializer::Serializer;
