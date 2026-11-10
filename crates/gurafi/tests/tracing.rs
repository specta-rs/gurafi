//! Feature-gated tests for loader batching and cache instrumentation.
#![cfg(feature = "tracing")]

use std::{
    collections::BTreeMap,
    fmt,
    sync::{
        Arc, Mutex,
        atomic::{AtomicU64, Ordering},
    },
};

use gurafi::Loader;
use tracing::{
    Event, Metadata, Subscriber,
    field::{Field, Visit},
    span::{Attributes, Id, Record},
};

type CapturedEvent = (String, BTreeMap<String, String>);

#[derive(Clone, Default)]
struct EventCapture {
    events: Arc<Mutex<Vec<CapturedEvent>>>,
    next_id: Arc<AtomicU64>,
}

impl EventCapture {
    fn new() -> Self {
        Self {
            events: Arc::new(Mutex::new(Vec::new())),
            next_id: Arc::new(AtomicU64::new(1)),
        }
    }

    fn events(&self) -> Vec<(String, BTreeMap<String, String>)> {
        self.events.lock().expect("event lock").clone()
    }
}

struct Collector<'a>(&'a mut BTreeMap<String, String>);

impl Visit for Collector<'_> {
    fn record_debug(&mut self, field: &Field, value: &dyn fmt::Debug) {
        self.0.insert(field.name().to_owned(), format!("{value:?}"));
    }

    fn record_str(&mut self, field: &Field, value: &str) {
        self.0.insert(field.name().to_owned(), value.to_owned());
    }

    fn record_i64(&mut self, field: &Field, value: i64) {
        self.0.insert(field.name().to_owned(), value.to_string());
    }

    fn record_u64(&mut self, field: &Field, value: u64) {
        self.0.insert(field.name().to_owned(), value.to_string());
    }

    fn record_bool(&mut self, field: &Field, value: bool) {
        self.0.insert(field.name().to_owned(), value.to_string());
    }

    fn record_f64(&mut self, field: &Field, value: f64) {
        self.0.insert(field.name().to_owned(), value.to_string());
    }
}

impl Subscriber for EventCapture {
    fn enabled(&self, _metadata: &Metadata<'_>) -> bool {
        true
    }

    fn new_span(&self, _attributes: &Attributes<'_>) -> Id {
        Id::from_u64(self.next_id.fetch_add(1, Ordering::Relaxed))
    }

    fn record(&self, _span: &Id, _values: &Record<'_>) {}

    fn record_follows_from(&self, _span: &Id, _follows: &Id) {}

    fn event(&self, event: &Event<'_>) {
        let mut fields = BTreeMap::new();
        event.record(&mut Collector(&mut fields));
        self.events
            .lock()
            .expect("event lock")
            .push((event.metadata().name().to_owned(), fields));
    }

    fn enter(&self, _span: &Id) {}

    fn exit(&self, _span: &Id) {}
}

#[test]
fn loader_batches_emit_queue_and_batch_size() {
    let loader = Loader::<String, String>::new(|keys: Vec<String>| async move {
        keys.into_iter()
            .map(|key| Ok(format!("value-for-{key}")))
            .collect()
    });

    let capture = EventCapture::new();
    let _ = tracing::subscriber::with_default(capture.clone(), || {
        futures::executor::block_on(loader.load_many(vec!["a".to_owned(), "b".to_owned()]))
    });

    let batch = capture
        .events()
        .into_iter()
        .find(|(_, fields)| {
            fields.get("message").map(String::as_str) == Some("gurafi.loader.batch")
        })
        .expect("batch event is emitted");
    assert_eq!(batch.1.get("queue_len").map(String::as_str), Some("2"));
    assert_eq!(batch.1.get("batch_size").map(String::as_str), Some("2"));
    assert_eq!(batch.1.get("deduplicated").map(String::as_str), Some("0"));
    assert_eq!(batch.1.get("errors").map(String::as_str), Some("0"));
    assert!(batch.1.contains_key("duration_us"));
}

#[test]
fn loader_never_records_key_values() {
    let secret = "SENTINEL_LOADER_KEY";
    let loader = Loader::<String, String>::new(|keys: Vec<String>| async move {
        keys.into_iter().map(|_| Ok(String::from("ok"))).collect()
    });

    let capture = EventCapture::new();
    let _ = tracing::subscriber::with_default(capture.clone(), || {
        futures::executor::block_on(loader.load(secret.to_owned()))
    });

    let recorded = capture.events();
    assert!(
        recorded
            .iter()
            .flat_map(|(_, fields)| fields.values())
            .all(|value| !value.contains(secret)),
        "loader keys must never be recorded"
    );
}

#[test]
fn request_cache_records_hit_and_miss_without_values() {
    let cache = gurafi::RequestCache::new();
    let capture = EventCapture::new();
    tracing::subscriber::with_default(capture.clone(), || {
        let _ = cache.get::<String>();
        let _ = cache.get_or_insert_with(|| String::from("SENTINEL_CACHE_VALUE"));
        let _ = cache.get::<String>();
    });
    let events: Vec<_> = capture
        .events()
        .into_iter()
        .filter(|(_, fields)| {
            fields.get("message").map(String::as_str) == Some("gurafi.cache.lookup")
        })
        .collect();
    assert!(
        events
            .iter()
            .any(|(_, fields)| { fields.get("hit").map(String::as_str) == Some("false") })
    );
    assert!(
        events
            .iter()
            .any(|(_, fields)| { fields.get("hit").map(String::as_str) == Some("true") })
    );
}
