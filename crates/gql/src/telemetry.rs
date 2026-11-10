//! Internal helpers for the optional `tracing` feature.
//!
//! Every helper in this module compiles to nothing when the feature is off. The macros take
//! their field expressions as tokens, so an expression that formats a path or a type is only
//! evaluated inside the `enabled!` branch. That keeps disabled builds free of formatting and
//! allocation even at the call site.

#[cfg(feature = "tracing")]
use std::fmt::Write as _;

/// Format an executor response path without exposing it through a public API.
///
/// Only called from a span field expression, which tracing evaluates only when the span is
/// enabled at the subscriber's filter.
#[cfg(feature = "tracing")]
pub(crate) fn format_path(path: &[crate::PathSegment]) -> String {
    let mut out = String::new();
    for segment in path {
        match segment {
            crate::PathSegment::Field(name) => {
                if !out.is_empty() {
                    out.push('.');
                }
                out.push_str(name);
            }
            crate::PathSegment::Index(index) => {
                // Writing to a String cannot fail.
                let _ = write!(out, "[{index}]");
            }
        }
    }
    out
}

#[cfg(feature = "tracing")]
pub(crate) fn field_type_label(field: Option<&crate::types::Field>) -> String {
    field.map_or_else(
        || String::from("<unresolved>"),
        |field| field.ty.to_string(),
    )
}

#[cfg(feature = "tracing")]
macro_rules! field_span {
    ($parent:expr, $field:expr, $path:expr, $ty:expr, $subfields:expr, $resolver:expr, $fast:expr) => {{
        if ::tracing::enabled!(target: "gql::field", ::tracing::Level::DEBUG) {
            ::tracing::debug_span!(
                target: "gql::field",
                "gql.field",
                parent_type = $parent,
                field = $field,
                path = %$crate::telemetry::format_path($path),
                return_type = %$ty,
                subfields = $subfields,
                resolver = $resolver,
                fast_path = $fast,
                errors = ::tracing::field::Empty,
            )
        } else {
            ::tracing::Span::none()
        }
    }};
}

#[cfg(feature = "tracing")]
pub(crate) use field_span;

#[cfg(not(feature = "tracing"))]
macro_rules! field_span {
    ($parent:expr, $field:expr, $path:expr, $ty:expr, $subfields:expr, $resolver:expr, $fast:expr) => {
        ()
    };
}

#[cfg(not(feature = "tracing"))]
pub(crate) use field_span;

/// Instrument a future only when its span is enabled.
///
/// The no-subscriber path awaits the future directly instead of adding an `Instrumented` poll
/// frame. The executor recurses once per selection-set level, so an unconditional wrapper is a
/// measurable stack cost on deep documents even when nothing is recording.
#[cfg(feature = "tracing")]
macro_rules! instrument_span_if_enabled {
    ($span:expr, $future:expr) => {{
        let span = $span;
        if span.is_disabled() {
            $future.await
        } else {
            ::tracing::Instrument::instrument($future, span).await
        }
    }};
}

#[cfg(feature = "tracing")]
pub(crate) use instrument_span_if_enabled;

#[cfg(not(feature = "tracing"))]
macro_rules! instrument_span_if_enabled {
    ($span:expr, $future:expr) => {
        $future.await
    };
}

#[cfg(not(feature = "tracing"))]
pub(crate) use instrument_span_if_enabled;

/// Record a field outcome on its span.
#[cfg(feature = "tracing")]
macro_rules! record_field_outcome {
    ($span:expr, $outcome:expr) => {{
        let span = &$span;
        if !span.is_disabled() {
            match &$outcome {
                Ok(_) => {
                    span.record("errors", 0usize);
                }
                Err(error) => {
                    span.record("errors", 1usize);
                    if ::tracing::enabled!(target: "gql::field", ::tracing::Level::DEBUG) {
                        ::tracing::event!(
                            target: "gql::field",
                            parent: span,
                            ::tracing::Level::DEBUG,
                            error = %error.message(),
                            "gql.field.error"
                        );
                    }
                }
            }
        }
    }};
}

#[cfg(feature = "tracing")]
pub(crate) use record_field_outcome;

#[cfg(not(feature = "tracing"))]
macro_rules! record_field_outcome {
    ($span:expr, $outcome:expr) => {{}};
}

#[cfg(not(feature = "tracing"))]
pub(crate) use record_field_outcome;
