//! Feature-gated tests for HTTP request tracing.
#![cfg(feature = "tracing")]

use std::{
    collections::{BTreeMap, HashMap},
    fmt,
    sync::{
        Arc, Mutex,
        atomic::{AtomicU64, Ordering},
    },
};

use gql::{Field, InputValue, ObjectType, Schema, TypeRef, Value};
use suku::{Context, HttpConfig, Server, http::handle};
use tracing::{
    Event, Metadata, Subscriber,
    field::{Field as TracingField, Visit},
    span::{Attributes, Id, Record},
};

#[derive(Clone, Debug, Default)]
struct SpanData {
    name: String,
    fields: BTreeMap<String, String>,
    parent: Option<usize>,
}

#[allow(dead_code)]
#[derive(Clone, Debug, Default)]
struct EventData {
    name: String,
    fields: BTreeMap<String, String>,
    parent: Option<usize>,
}

#[derive(Default)]
struct Captured {
    spans: Vec<SpanData>,
    events: Vec<EventData>,
    by_id: HashMap<Id, usize>,
    metadata: HashMap<Id, &'static Metadata<'static>>,
}

#[derive(Clone)]
struct Capture {
    captured: Arc<Mutex<Captured>>,
    stack: Arc<Mutex<Vec<Id>>>,
    next_id: Arc<AtomicU64>,
}

impl Capture {
    fn new() -> Self {
        Self {
            captured: Arc::new(Mutex::new(Captured::default())),
            stack: Arc::new(Mutex::new(Vec::new())),
            next_id: Arc::new(AtomicU64::new(1)),
        }
    }

    fn snapshot(&self) -> Captured {
        let captured = self.captured.lock().expect("capture lock");
        Captured {
            spans: captured.spans.clone(),
            events: captured.events.clone(),
            by_id: HashMap::new(),
            metadata: HashMap::new(),
        }
    }

    fn span_index(&self, id: &Id) -> Option<usize> {
        self.captured
            .lock()
            .expect("capture lock")
            .by_id
            .get(id)
            .copied()
    }

    fn current(&self) -> Option<Id> {
        self.stack.lock().expect("stack lock").last().cloned()
    }
}

struct Collector<'a>(&'a mut BTreeMap<String, String>);

impl Visit for Collector<'_> {
    fn record_debug(&mut self, field: &TracingField, value: &dyn fmt::Debug) {
        self.0.insert(field.name().to_owned(), format!("{value:?}"));
    }

    fn record_str(&mut self, field: &TracingField, value: &str) {
        self.0.insert(field.name().to_owned(), value.to_owned());
    }

    fn record_i64(&mut self, field: &TracingField, value: i64) {
        self.0.insert(field.name().to_owned(), value.to_string());
    }

    fn record_u64(&mut self, field: &TracingField, value: u64) {
        self.0.insert(field.name().to_owned(), value.to_string());
    }

    fn record_bool(&mut self, field: &TracingField, value: bool) {
        self.0.insert(field.name().to_owned(), value.to_string());
    }

    fn record_f64(&mut self, field: &TracingField, value: f64) {
        self.0.insert(field.name().to_owned(), value.to_string());
    }
}

impl Subscriber for Capture {
    fn enabled(&self, _metadata: &Metadata<'_>) -> bool {
        true
    }

    fn new_span(&self, attributes: &Attributes<'_>) -> Id {
        let mut fields = BTreeMap::new();
        attributes.record(&mut Collector(&mut fields));
        let parent = attributes
            .parent()
            .and_then(|id| self.span_index(id))
            .or_else(|| self.current().and_then(|id| self.span_index(&id)));
        let id = Id::from_u64(self.next_id.fetch_add(1, Ordering::Relaxed));
        let mut captured = self.captured.lock().expect("capture lock");
        let index = captured.spans.len();
        captured.spans.push(SpanData {
            name: attributes.metadata().name().to_owned(),
            fields,
            parent,
        });
        captured.by_id.insert(id.clone(), index);
        captured.metadata.insert(id.clone(), attributes.metadata());
        id
    }

    fn record(&self, span: &Id, values: &Record<'_>) {
        let Some(index) = self.span_index(span) else {
            return;
        };
        let mut fields = BTreeMap::new();
        values.record(&mut Collector(&mut fields));
        let mut captured = self.captured.lock().expect("capture lock");
        if let Some(data) = captured.spans.get_mut(index) {
            data.fields.extend(fields);
        }
    }

    fn record_follows_from(&self, _span: &Id, _follows: &Id) {}

    fn event(&self, event: &Event<'_>) {
        let mut fields = BTreeMap::new();
        event.record(&mut Collector(&mut fields));
        let parent = self.current().and_then(|id| self.span_index(&id));
        let mut captured = self.captured.lock().expect("capture lock");
        captured.events.push(EventData {
            name: event.metadata().name().to_owned(),
            fields,
            parent,
        });
    }

    fn current_span(&self) -> tracing_core::span::Current {
        let current = self.current();
        let Some(id) = current else {
            return tracing_core::span::Current::none();
        };
        let metadata = self
            .captured
            .lock()
            .expect("capture lock")
            .metadata
            .get(&id)
            .copied();
        match metadata {
            Some(metadata) => tracing_core::span::Current::new(id, metadata),
            None => tracing_core::span::Current::none(),
        }
    }

    fn enter(&self, span: &Id) {
        self.stack.lock().expect("stack lock").push(span.clone());
    }

    fn exit(&self, span: &Id) {
        let mut stack = self.stack.lock().expect("stack lock");
        if let Some(position) = stack.iter().rposition(|id| id == span) {
            stack.truncate(position);
        }
    }
}

fn schema() -> Schema {
    let echo = Field::new("echo", TypeRef::string().non_null())
        .arg(InputValue::new("message", TypeRef::string().non_null()))
        .resolve(|context| {
            let message = context
                .args
                .get("message")
                .and_then(Value::as_str)
                .unwrap_or_default()
                .to_owned();
            Box::pin(async move { Ok(Value::from(message)) })
        });
    let mut builder = Schema::builder();
    builder.add_type(ObjectType::new("Query").field(echo).into());
    builder = builder.query("Query");
    builder.build().expect("valid schema")
}

fn post_parts(uri: &str) -> ::http::request::Parts {
    let (parts, ()) = ::http::Request::builder()
        .method(::http::Method::POST)
        .uri(uri)
        .header(::http::header::CONTENT_TYPE, "application/json")
        .body(())
        .expect("request")
        .into_parts();
    parts
}

fn find_span<'a>(captured: &'a Captured, name: &str) -> Vec<&'a SpanData> {
    captured
        .spans
        .iter()
        .filter(|span| span.name == name)
        .collect()
}

#[test]
fn request_span_records_method_operation_and_status() {
    let server = Server::new(schema());
    let parts = post_parts("/graphql");
    let body = br#"{"query":"query Hello { echo(message: \"hi\") }","operationName":"Hello"}"#;
    let capture = Capture::new();

    tracing::subscriber::with_default(capture.clone(), || {
        futures::executor::block_on(handle(
            &server,
            &HttpConfig::default(),
            &parts,
            body,
            |_| async { Ok(Context::new(())) },
        ))
    });

    let captured = capture.snapshot();
    let request = find_span(&captured, "suku.request");
    assert_eq!(request.len(), 1, "one request span");
    assert_eq!(
        request[0].fields.get("method").map(String::as_str),
        Some("POST")
    );
    assert_eq!(
        request[0].fields.get("path").map(String::as_str),
        Some("/graphql")
    );
    assert_eq!(
        request[0].fields.get("status").map(String::as_str),
        Some("200")
    );

    let process = find_span(&captured, "suku.process");
    assert_eq!(process.len(), 1);
    assert_eq!(
        process[0].fields.get("operation").map(String::as_str),
        Some("Hello")
    );
    assert!(find_span(&captured, "suku.execute").len() == 1);
    assert!(find_span(&captured, "suku.parse").len() == 1);
    assert!(find_span(&captured, "suku.serialize").len() == 1);
    assert_eq!(process[0].parent, Some(0));

    // The gql spans are children of the server's execute span, not of the transport root.
    assert!(captured.spans.iter().any(|span| span.name == "gql.execute"));
}

#[test]
fn request_bodies_and_variable_values_never_reach_tracing() {
    let secret = "SENTINEL_HTTP_SECRET";
    let server = Server::new(schema());
    let parts = post_parts("/graphql");
    let body = format!(
        r#"{{"query":"query Q($token: String!) {{ echo(message: $token) }}","variables":{{"token":"{secret}"}}}}"#
    );
    let capture = Capture::new();

    tracing::subscriber::with_default(capture.clone(), || {
        futures::executor::block_on(handle(
            &server,
            &HttpConfig::default(),
            &parts,
            body.as_bytes(),
            |_| async { Ok(Context::new(())) },
        ))
    });

    let captured = capture.snapshot();
    let values = captured
        .spans
        .iter()
        .flat_map(|span| span.fields.values().map(String::as_str))
        .chain(
            captured
                .events
                .iter()
                .flat_map(|event| event.fields.values().map(String::as_str)),
        );
    assert!(
        values.into_iter().all(|value| !value.contains(secret)),
        "HTTP bodies and variable values must never be recorded"
    );
}
