//! Pothos-style Relay support for [`gurafi`].
//!
//! This crate adds the three pieces of Relay that a code-first GraphQL server normally
//! hand-writes: global IDs, the `Node` interface, and connection/mutation types with runtime
//! names. It is deliberately a thin layer over the public `gurafi` builder; it does not patch
//! the executor or require a particular async runtime.
//!
//! # Quick start
//!
//! ```
//! use gurafi::{Object, Schema, expose};
//! use gurafi_relay::{Connection, ConnectionArgs, GlobalId, Relay};
//!
//! #[derive(Clone, Object)]
//! struct User {
//!     id: String,
//!     name: String,
//! }
//!
//! #[derive(Clone, Object)]
//! struct Post {
//!     id: String,
//!     title: String,
//! }
//!
//! struct Ctx;
//!
//! let mut schema = Schema::<Ctx>::new();
//! let mut relay = Relay::<Ctx>::new();
//!
//! relay.node::<User>(&mut schema, |n| {
//!     n.id(|user: &User| user.id.clone());
//!     n.load_one(|id, _: &Ctx| {
//!         Box::pin(async move {
//!             Some(User { id, name: "Ada".into() })
//!         })
//!     });
//! }).unwrap();
//!
//! schema.object::<User>(|o| {
//!     o.expose(expose!(User::name));
//! });
//!
//! schema.object::<Post>(|o| {
//!     o.expose(expose!(Post::title));
//! });
//!
//! relay.object_connection::<User, Post, _>(
//!     &mut schema,
//!     "posts",
//!     "UserPostsConnection",
//!     |_user: &User, args: ConnectionArgs, _ctx: &Ctx| {
//!         Connection::from_slice(&[], &args, |post: &Post| post.id.clone())
//!     },
//! ).unwrap();
//!
//! let built = schema.build().expect("valid schema");
//! assert!(built.object_type("UserPostsConnection").is_some());
//! assert!(built.interface_type("Node").is_some());
//!
//! // `GlobalId<T>` is a normal input/output scalar with an `ID` representation.
//! let id = GlobalId::<User>::new("42");
//! assert_eq!(GlobalId::<User>::decode(&id.encode()).unwrap().as_local(), "42");
//! ```
//!
//! # Registration order
//!
//! Keep one [`Relay`] per schema. It owns the shared `Node` interface, `PageInfo` object and root
//! fields, so repeated helper calls are idempotent. Register a connection before attaching its
//! field with [`RelayObjectExt::connection`]. The convenience methods
//! ([`Relay::object_connection`], [`Relay::query_connection`], [`Relay::mutation`]) do both in
//! order.
//!
//! `Schema::build` is the validation point: it reports duplicate types, missing loaders, and
//! fields whose runtime name is not actually in the schema. The Relay helpers return
//! [`RelayError`] for mistakes they can detect earlier.
//!
//! # Limitations
//!
//! - The `Node` interface and its root fields are named exactly `Node`, `node` and `nodes`.
//! - Connection type names are explicit (`UserPostsConnection`), not generated from the field or
//!   parent type. This is deliberate: it makes the schema stable when refactoring Rust modules.
//! - Bare local IDs are **rejected by default**. Opt into the migration fallback with
//!   [`Relay::allow_local_ids(true)`](Relay::allow_local_ids); local IDs are then tried against
//!   every registered node loader in registration order.
//! - Relay's `clientMutationId` is a sibling argument of `input`, not a field inside the input
//!   object, because `gurafi` input object names are compile-time constants. This is not
//!   wire-compatible with clients that send `clientMutationId` inside `variables.input`.
//! - The helpers live on [`Relay`] (and the object/root extension traits) rather than directly on
//!   `Schema`. Generated shared types and root fields need schema ownership, and an explicit
//!   registry keeps repeated registration idempotent without global state.

#![forbid(unsafe_code)]
#![warn(missing_docs)]

mod base64;
mod connection;
mod error;
mod global_id;
mod mutation;
mod node;

pub use connection::{
    Connection, ConnectionArgs, ConnectionRef, Cursor, Edge, PageInfo, RelayObjectExt, RelayRootExt,
};
pub use error::RelayError;
pub use global_id::{DecodedGlobalId, GlobalId, GlobalIdType, decode_global_id};
pub use mutation::{ClientMutationId, MutationConfig};
pub use node::{NodeArgs, NodeBuilder, NodesArgs, Relay};

/// Re-exported so callers can use the same `gql` value and error types without adding a direct
/// dependency for simple integrations.
pub use gurafi::gql;

/// Commonly used Relay items.
///
/// ```
/// use gurafi_relay::prelude::*;
/// ```
pub mod prelude {
    pub use crate::{
        ClientMutationId, Connection, ConnectionArgs, ConnectionRef, Cursor, Edge, GlobalId,
        MutationConfig, PageInfo, Relay, RelayError, RelayObjectExt, RelayRootExt,
    };
}
