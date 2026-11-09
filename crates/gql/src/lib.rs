//! Reference GraphQL implementation in Rust

#![cfg_attr(docsrs, feature(doc_cfg))]
#![warn(
    clippy::cargo,
    clippy::pedantic,
    missing_docs,
    missing_debug_implementations,
    clippy::panic,
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::panicking_unwrap,
    clippy::indexing_slicing
)]
#![forbid(unsafe_code)]

mod input_type;
mod interface_type;
mod object_type;
mod output_type;
mod schema;
pub mod serializer;
mod thunk;

pub use input_type::InputType;
pub use interface_type::InterfaceType;
pub use object_type::{
    ArgumentConfig, FieldConfig, GraphQLResolveInfo, ObjectType, ResolveFn, ResolveFnArgs,
};
pub use output_type::{NamedOutputType, OutputType, ScalarType};
pub use schema::Schema;
pub use thunk::Thunk;
