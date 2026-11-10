//! Internal helpers for the optional `tracing` feature.
//!
//! Field expressions are passed as tokens and only expanded inside the `enabled!` branch, so the
//! disabled build never formats or allocates at a call site.

#[cfg(feature = "tracing")]
macro_rules! span_if_enabled {
    ($level:expr, $target:literal, $name:literal, $($fields:tt)*) => {{
        if ::tracing::enabled!(target: $target, $level) {
            ::tracing::span!(target: $target, $level, $name, $($fields)*)
        } else {
            ::tracing::Span::none()
        }
    }};
}

#[cfg(feature = "tracing")]
pub(crate) use span_if_enabled;

#[cfg(not(feature = "tracing"))]
macro_rules! span_if_enabled {
    ($level:expr, $target:literal, $name:literal, $($fields:tt)*) => {
        ()
    };
}

#[cfg(not(feature = "tracing"))]
pub(crate) use span_if_enabled;

#[cfg(feature = "tracing")]
macro_rules! instrument_span {
    ($span:expr, $future:expr) => {
        ::tracing::Instrument::instrument($future, $span)
    };
}

#[cfg(feature = "tracing")]
pub(crate) use instrument_span;

#[cfg(not(feature = "tracing"))]
macro_rules! instrument_span {
    ($span:expr, $future:expr) => {
        $future
    };
}

#[cfg(not(feature = "tracing"))]
pub(crate) use instrument_span;
