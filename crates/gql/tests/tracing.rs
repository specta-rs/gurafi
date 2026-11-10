//! Feature-gated tests for the optional `tracing` instrumentation.
#![cfg(feature = "tracing")]

use std::{
    collections::{BTreeMap, HashMap},
    fmt,
    sync::{
        Arc, Mutex,
        atomic::{AtomicU64, Ordering},
    },
};

use gql::{
    ExecutionOptions, Field, InputValue, Object, ObjectType, Request, Schema, TypeRef, Value,
};
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

    fn subscriber(&self) -> Self {
        self.clone()
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

fn nested_schema() -> Schema {
    let user = ObjectType::new("User").field(
        Field::new("name", TypeRef::string().non_null())
            .resolve(|_| Box::pin(async { Ok(Value::from("Ada")) })),
    );
    let mut builder = Schema::builder();
    builder.add_type(user.into());
    builder.add_type(
        ObjectType::new("Query")
            .field(
                Field::new("user", TypeRef::named("User").non_null()).resolve(|_| {
                    Box::pin(async {
                        let mut user = Object::new();
                        user.insert("name", Value::from("Ada"));
                        Ok(Value::Object(Arc::new(user)))
                    })
                }),
            )
            .into(),
    );
    builder = builder.query("Query");
    builder.build().expect("valid schema")
}

fn find_span<'a>(captured: &'a Captured, name: &str, field: Option<&str>) -> Vec<&'a SpanData> {
    captured
        .spans
        .iter()
        .filter(|span| {
            span.name == name
                && field.is_none_or(|field| {
                    span.fields.get("field").is_some_and(|value| value == field)
                })
        })
        .collect()
}

#[test]
fn execute_emits_root_and_nested_field_spans() {
    let schema = nested_schema();
    let capture = Capture::new();
    let subscriber = capture.subscriber();
    let result = tracing::subscriber::with_default(subscriber, || {
        futures::executor::block_on(gql::execute(
            &schema,
            Request::new("{ user { name } }"),
            &ExecutionOptions::default(),
        ))
    });
    assert!(result.errors.is_empty());
    let captured = capture.snapshot();

    let root = find_span(&captured, "gql.execute", None);
    assert_eq!(root.len(), 1, "one root operation span");
    assert!(root[0].fields.contains_key("document_bytes"));
    assert_eq!(
        root[0].fields.get("kind").map(String::as_str),
        Some("query")
    );
    assert!(
        captured
            .spans
            .iter()
            .any(|span| span.name == "gql.validate"),
        "validation span is emitted"
    );

    let user = find_span(&captured, "gql.field", Some("user"));
    let name = find_span(&captured, "gql.field", Some("name"));
    assert_eq!(user.len(), 1);
    assert_eq!(name.len(), 1);
    assert_eq!(
        user[0].fields.get("parent_type").map(String::as_str),
        Some("Query")
    );
    assert_eq!(
        name[0].fields.get("parent_type").map(String::as_str),
        Some("User")
    );

    let user_index = captured
        .spans
        .iter()
        .position(|span| {
            span.name == "gql.field" && span.fields.get("field").map(String::as_str) == Some("user")
        })
        .expect("user field span");
    assert_eq!(
        name[0].parent,
        Some(user_index),
        "nested field span is parented to the field that selected it"
    );
}

#[test]
fn variable_values_and_resolver_arguments_never_reach_spans() {
    let secret = "SENTINEL_DO_NOT_LEAK";
    let schema = {
        let mut builder = Schema::builder();
        builder.add_type(
            ObjectType::new("Query")
                .field(
                    Field::new("echo", TypeRef::named("String").non_null())
                        .arg(InputValue::new(
                            "token",
                            TypeRef::named("String").non_null(),
                        ))
                        .resolve(|_| Box::pin(async { Ok(Value::from("ok")) })),
                )
                .into(),
        );
        builder = builder.query("Query");
        builder.build().expect("valid schema")
    };
    let mut variables = Object::new();
    variables.insert("token", Value::from(secret));

    let capture = Capture::new();
    tracing::subscriber::with_default(capture.subscriber(), || {
        let _ = futures::executor::block_on(gql::execute(
            &schema,
            Request::new("query Q($token: String!) { echo(token: $token) }")
                .operation_name("Q")
                .variables(&variables),
            &ExecutionOptions::default(),
        ));
    });
    let captured = capture.snapshot();
    let all_field_values: Vec<&str> = captured
        .spans
        .iter()
        .flat_map(|span| span.fields.values().map(String::as_str))
        .chain(
            captured
                .events
                .iter()
                .flat_map(|event| event.fields.values().map(String::as_str)),
        )
        .collect();
    assert!(
        !all_field_values.iter().any(|value| value.contains(secret)),
        "the variable value must never be recorded"
    );
    let root = find_span(&captured, "gql.execute", None);
    assert_eq!(
        root[0].fields.get("operation").map(String::as_str),
        Some("Q")
    );
    assert_eq!(
        root[0].fields.get("variable_count").map(String::as_str),
        Some("1")
    );
}

#[test]
fn field_span_reports_scalar_type_and_fast_path() {
    let mut builder = Schema::builder();
    builder.add_type(
        ObjectType::new("Query")
            .field(
                Field::new("answer", TypeRef::int().non_null())
                    .resolve(|_| Box::pin(async { Ok(Value::from(42_i32)) })),
            )
            .into(),
    );
    builder = builder.query("Query");
    let schema = builder.build().expect("valid schema");

    let capture = Capture::new();
    tracing::subscriber::with_default(capture.subscriber(), || {
        let _ = futures::executor::block_on(gql::execute(
            &schema,
            Request::new("{ answer }"),
            &ExecutionOptions::default(),
        ));
    });
    let captured = capture.snapshot();
    let field = find_span(&captured, "gql.field", Some("answer"));
    assert_eq!(field.len(), 1);
    assert_eq!(
        field[0].fields.get("resolver").map(String::as_str),
        Some("true")
    );
    assert!(
        field[0]
            .fields
            .get("return_type")
            .is_some_and(|ty| ty.contains("Int")),
        "return type is recorded: {:?}",
        field[0].fields
    );
    assert_eq!(
        field[0].fields.get("subfields").map(String::as_str),
        Some("0")
    );
}
