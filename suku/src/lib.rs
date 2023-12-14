//! Suku: A universal GraphQL server.

pub(crate) mod introspection;
mod server;
mod value;

pub use server::Server;
pub use value::Value;
