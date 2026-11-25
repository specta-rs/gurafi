//! Panic isolation for user resolver code.
//!
//! Resolvers are caller code. A panic inside one must not unwind the request future, abort the
//! process, or end a subscription stream, so every call into a resolver, subscription setup
//! resolver or abstract-type hook goes through a narrow [`std::panic::catch_unwind`] boundary
//! here. A caught panic becomes an ordinary [`GraphQLError`] with a stable code; the caller's
//! existing field-completion code attaches the response path and source location.
//!
//! Only the resolver call is caught. Argument coercion, field collection and the rest of the
//! engine are not wrapped, so an engine bug still surfaces as a panic instead of being disguised
//! as a resolver failure.
//!
//! The success path adds no allocation: the resolver future is already boxed, and the isolation
//! is a wrapper around polling it plus a `match` on the result. The error (and its extension
//! object) is only built when a panic was actually caught. The panic payload is never formatted
//! and is intentionally forgotten rather than dropped: an arbitrary payload can contain multiple
//! panicking destructors, and a double panic during `Drop` aborts the process even inside
//! `catch_unwind`. Isolation therefore takes precedence over reclaiming exceptional payload
//! allocations.
//!
//! `catch_unwind` only works when the binary unwinds. A profile with `panic = "abort"` aborts on
//! the first panic before any of this can run; see the crate documentation.

use std::{
    any::Any,
    panic::{AssertUnwindSafe, catch_unwind},
};

use futures::FutureExt;
use std::future::Future;

use crate::{
    error::{GraphQLError, Result},
    types::{FieldResult, ResolveFn, ResolverContext, SubscribeFn, SubscriptionStream},
};

/// The message reported when a resolver panics.
pub(crate) const INTERNAL_ERROR_MESSAGE: &str = "Internal server error";

/// The extension code attached to a resolver panic.
pub(crate) const INTERNAL_ERROR_CODE: &str = "INTERNAL_SERVER_ERROR";

/// The field error a caught resolver panic becomes.
///
/// The panic payload and backtrace are deliberately not copied into the error.
fn internal_error() -> GraphQLError {
    GraphQLError::new(INTERNAL_ERROR_MESSAGE).extend("code", INTERNAL_ERROR_CODE)
}

/// Turn a caught panic into the internal field error, leaving the value untouched on success.
fn caught<T>(result: Result<T, Box<dyn Any + Send>>) -> Result<T, GraphQLError> {
    match result {
        Ok(value) => Ok(value),
        Err(payload) => {
            discard_payload(payload);
            Err(internal_error())
        }
    }
}

/// Dispose of a caught payload without running application-controlled destructors.
///
/// Dropping an arbitrary payload is not safe inside panic isolation: one panicking destructor can
/// be caught, but a second panic while that drop is already unwinding aborts the process. Panics are
/// exceptional, so the payload is deliberately leaked rather than risking process termination.
fn discard_payload(payload: Box<dyn Any + Send>) {
    std::mem::forget(payload);
}

/// Run synchronous user resolver code, turning a panic into a field error.
///
/// This covers `resolve_type` and `is_type_of`, which the executor calls synchronously.
#[inline]
pub(crate) fn sync<T>(f: impl FnOnce() -> T) -> Result<T, GraphQLError> {
    caught(catch_unwind(AssertUnwindSafe(f)))
}

/// Await a field resolver, turning a panic into a field error.
///
/// The async block covers both construction and polling of the resolver's future, so a resolver
/// that panics before its first suspension point is caught too.
pub(crate) fn resolver<'a>(
    resolve: &'a ResolveFn,
    context: ResolverContext<'a>,
) -> impl Future<Output = FieldResult> + 'a {
    AssertUnwindSafe(async move { resolve(context).await })
        .catch_unwind()
        .map(|result| caught(result).flatten())
}

/// Await a subscription setup resolver, turning a panic into an error response.
pub(crate) fn subscription<'a>(
    subscribe: &'a SubscribeFn,
    context: ResolverContext<'a>,
) -> impl Future<Output = Result<SubscriptionStream, GraphQLError>> + 'a {
    AssertUnwindSafe(async move { subscribe(context).await })
        .catch_unwind()
        .map(|result| caught(result).flatten())
}
