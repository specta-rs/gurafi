//! Exposing model fields without writing a resolver.
//!
//! Most object fields are a plain property of the model. [`expose!`] names the property once and
//! derives the GraphQL field name and type from the Rust path:
//!
//! ```
//! use gurafi::{Object, Schema, expose};
//!
//! #[derive(Object)]
//! struct Giraffe {
//!     name: String,
//!     height: f32,
//! }
//!
//! let mut schema = Schema::<()>::new();
//! schema.object::<Giraffe>(|o| {
//!     o.expose(expose!(Giraffe::name));
//!     // Rename the GraphQL field while keeping the Rust path.
//!     o.expose(expose!(Giraffe::height as "heightMeters"));
//! });
//! schema.query(|q| {
//!     q.field("giraffe", |_: gurafi::NoArgs, _: &()| Giraffe {
//!         name: "Melman".into(),
//!         height: 3.4,
//!     });
//! });
//! ```
//!
//! Nested paths work as `expose!(Type::field.subfield)`, and every segment is cloned, so the
//! model itself is never moved.

use std::sync::Arc;

/// A model property exposed as a GraphQL field.
///
/// Construct it with `expose!`; it is consumed by
/// [`ObjectBuilder::expose`](crate::ObjectBuilder::expose).
pub struct Expose<M, T> {
    name: &'static str,
    get: Arc<dyn Fn(&M) -> T + Send + Sync>,
}

impl<M: 'static, T: 'static> Expose<M, T> {
    /// Create an exposed field from its GraphQL name and an accessor.
    pub fn new(name: &'static str, get: impl Fn(&M) -> T + Send + Sync + 'static) -> Self {
        Self {
            name,
            get: Arc::new(get),
        }
    }

    pub(crate) fn into_parts(self) -> (&'static str, Arc<dyn Fn(&M) -> T + Send + Sync>) {
        (self.name, self.get)
    }
}

impl<M, T> std::fmt::Debug for Expose<M, T> {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Expose").field("name", &self.name).finish()
    }
}

/// The last identifier in a `stringify!`-ed Rust path.
///
/// `stringify!(Giraffe::height)` is `"Giraffe :: height"`, so the GraphQL field name is the final
/// identifier run.
#[doc(hidden)]
#[must_use]
pub fn __expose_name(path: &'static str) -> &'static str {
    fn is_ident_byte(byte: u8) -> bool {
        byte == b'_' || byte.is_ascii_alphanumeric()
    }

    let bytes = path.as_bytes();
    let mut end = bytes.len();
    while end > 0 && !is_ident_byte(bytes[end - 1]) {
        end -= 1;
    }
    let mut start = end;
    while start > 0 && is_ident_byte(bytes[start - 1]) {
        start -= 1;
    }
    &path[start..end]
}

/// Expose a model property as a GraphQL field.
///
/// The GraphQL field name defaults to the last path segment; the type is inferred from the
/// property's Rust type. Forms:
///
/// - `expose!(Giraffe::name)` — field `name`.
/// - `expose!(Giraffe::keeper::name)` — field `name`, reading `model.keeper.name`.
/// - `expose!(Giraffe::height as "heightMeters")` — custom GraphQL name.
///
/// The generated accessor clones the final value, so the property's type must be `Clone`.
#[macro_export]
macro_rules! expose {
    ($first:ident $(:: $rest:ident)+ as $name:literal) => {
        $crate::Expose::new(
            $name,
            |__model: &$first| __model $(. $rest)+ .clone(),
        )
    };
    ($first:ident $(:: $rest:ident)+) => {
        $crate::Expose::new(
            $crate::__expose_name(concat!(
                stringify!($first)
                $(, "::", stringify!($rest))*
            )),
            |__model: &$first| __model $(. $rest)+ .clone(),
        )
    };
}
