//! Suku: A universal GraphQL server.

pub(crate) mod introspection;
mod response;
mod server;

// pub use response::Response;
pub use server::Server;

pub use gql::serializer::OwnedSerializer;
