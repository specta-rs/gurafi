// TODO: Remove this from the tracing example and move into a development playground project instead.

use std::time::Instant;
use tracing::span::{Attributes, Id};
use tracing::Subscriber;
use tracing_subscriber::layer::{Context, Layer};
use tracing_subscriber::registry::LookupSpan;

pub struct Timing {
    started_at: Instant,
}

pub struct CustomLayer;

impl<S> Layer<S> for CustomLayer
where
    S: Subscriber,
    S: for<'lookup> LookupSpan<'lookup>,
{
    fn on_new_span(&self, _attrs: &Attributes<'_>, id: &Id, ctx: Context<'_, S>) {
        let span = ctx.span(id).unwrap();

        span.extensions_mut().insert(Timing {
            started_at: Instant::now(),
        });
    }

    fn on_close(&self, id: Id, ctx: Context<'_, S>) {
        let span = ctx.span(&id).unwrap();

        let started_at = span.extensions().get::<Timing>().unwrap().started_at;

        let name = span.metadata().name();
        println!(
            "span: {}{name} took {:?}",
            span.metadata()
                .module_path()
                .map(|v| format!("{v}::"))
                .unwrap_or_default(),
            started_at.elapsed()
        );
    }
}
