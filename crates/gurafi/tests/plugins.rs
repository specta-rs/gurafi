//! Plugins: build-time coverage, ordering, metadata, built-ins and interaction with the
//! existing execution paths.

#![allow(dead_code)]

use std::sync::{Arc, Mutex};

use futures::StreamExt;
use gurafi::{
    FieldMeta, GraphQLError, Input, MockPlugin, NoArgs, Object, Plugin, Raw, RawObject,
    ResponseExtensionPlugin, Result as GurafiResult, Schema, TraceEvent, TracePhase, TracingPlugin,
    expose,
};

// =============================================================================================
// Shared helpers
// =============================================================================================

#[derive(Clone, Default)]
struct Recorder {
    fields: Arc<Mutex<Vec<FieldRecord>>>,
    wraps: Arc<Mutex<Vec<String>>>,
    subscriptions: Arc<Mutex<Vec<String>>>,
    entered: Arc<Mutex<Vec<String>>>,
    subscribe_entered: Arc<Mutex<Vec<String>>>,
}

#[derive(Clone, Debug)]
struct FieldRecord {
    type_name: String,
    field_name: String,
    return_type: String,
    args: Vec<(String, String)>,
    is_root: bool,
    is_subscription: bool,
}

impl Recorder {
    fn fields(&self) -> Vec<FieldRecord> {
        self.fields.lock().unwrap().clone()
    }

    fn record(&self, meta: &FieldMeta<'_>) -> FieldRecord {
        FieldRecord {
            type_name: meta.type_name.to_owned(),
            field_name: meta.field_name.to_owned(),
            return_type: meta.return_type.to_string(),
            args: meta
                .args
                .iter()
                .map(|(name, value)| (name.to_string(), value.ty.to_string()))
                .collect(),
            is_root: meta.is_root,
            is_subscription: meta.is_subscription,
        }
    }

    fn find(&self, type_name: &str, field_name: &str) -> FieldRecord {
        self.fields()
            .into_iter()
            .find(|record| record.type_name == type_name && record.field_name == field_name)
            .unwrap_or_else(|| {
                panic!(
                    "missing metadata for {type_name}.{field_name}; saw {:?}",
                    self.fields()
                )
            })
    }
}

impl<Ctx: 'static> Plugin<Ctx> for Recorder {
    fn field(&self, meta: &FieldMeta<'_>, _field: &mut gql::Field) {
        self.fields.lock().unwrap().push(self.record(meta));
    }

    fn wrap_resolve(&self, meta: &FieldMeta<'_>, next: gql::ResolveFn) -> gql::ResolveFn {
        let path = meta.path();
        self.wraps.lock().unwrap().push(path.clone());
        let entered = Arc::clone(&self.entered);
        Arc::new(move |context| {
            entered.lock().unwrap().push(path.clone());
            next(context)
        })
    }

    fn wrap_subscribe(&self, meta: &FieldMeta<'_>, next: gql::SubscribeFn) -> gql::SubscribeFn {
        let path = meta.path();
        self.subscriptions.lock().unwrap().push(path.clone());
        let entered = Arc::clone(&self.subscribe_entered);
        Arc::new(move |context| {
            entered.lock().unwrap().push(path.clone());
            next(context)
        })
    }
}

fn execute(schema: &gql::Schema, query: &str, ctx: Ctx) -> gql::Response {
    futures::executor::block_on(gql::execute(
        schema,
        gql::Request::new(query),
        &gql::ExecutionOptions::with_context(ctx),
    ))
}

fn data(response: &gql::Response) -> &gql::Object {
    response
        .data
        .as_ref()
        .and_then(gql::Value::as_object)
        .unwrap_or_else(|| panic!("missing response data: {response:?}"))
}

#[derive(Clone, Debug)]
struct Ctx {
    logged_in: bool,
}

impl Ctx {
    fn anonymous() -> Self {
        Self { logged_in: false }
    }

    fn logged_in() -> Self {
        Self { logged_in: true }
    }
}

#[derive(Object)]
struct Giraffe {
    name: String,
}

#[derive(Object)]
struct Keeper {
    name: String,
}

#[derive(Object)]
struct RawThing {
    name: String,
}

#[derive(Input)]
struct GreetArgs {
    name: String,
    times: Option<i32>,
}

// =============================================================================================
// Coverage and metadata
// =============================================================================================

fn coverage_schema(recorder: &Recorder) -> gql::Schema {
    let mut schema = Schema::<Ctx>::new();

    // Some fields are declared before the plugin, the rest after it. Both must be visited.
    let node = schema.interface("Node", |i| {
        i.field::<gurafi::ID>("id");
    });
    schema.object::<Giraffe>(|o| {
        o.implements(&node);
        o.expose(expose!(Giraffe::name));
        o.field("id", |g: &Giraffe, _: NoArgs, _: &Ctx| {
            gurafi::ID::new(g.name.clone())
        });
        o.field("keeper", |_: &Giraffe, _: NoArgs, _: &Ctx| Keeper {
            name: "Bob".into(),
        });
    });

    schema.plugin(recorder.clone());

    let admin = schema.object_ref::<Giraffe>("AdminGiraffe");
    schema.implement(&admin, |o| {
        o.expose(expose!(Giraffe::name));
    });

    schema.object::<RawThing>(|o| {
        o.raw_field::<String>("name");
    });

    schema.object::<Keeper>(|o| {
        o.expose(expose!(Keeper::name));
    });

    schema.query(|q| {
        q.field("giraffe", |_: NoArgs, _: &Ctx| Giraffe {
            name: "Melman".into(),
        });
        q.field_ref(&admin, "admin", |_: NoArgs, _: &Ctx| Giraffe {
            name: "Ada".into(),
        });
        q.field("raw", |_: NoArgs, _: &Ctx| {
            Raw::<RawThing>::object(RawObject::new().field("name", "raw").build())
        });
        q.field("greet", |args: GreetArgs, _: &Ctx| args.name);
    });

    schema.mutation(|m| {
        m.field("rename", |args: GreetArgs, _: &Ctx| args.name);
    });

    schema.subscription(|s| {
        s.field("tick", |_: NoArgs, _: &Ctx| {
            futures::stream::iter(vec![GurafiResult::Ok(1i32)])
        });
    });

    schema.build().expect("valid schema")
}

#[test]
fn plugins_cover_every_object_interface_root_and_subscription_field() {
    let recorder = Recorder::default();
    let schema = coverage_schema(&recorder);

    let fields = recorder.fields();
    // Every field is visited exactly once, no matter whether it was declared before or after
    // `Schema::plugin`, or whether it is a raw field or belongs to a runtime-named type.
    for (type_name, field_name) in [
        ("Giraffe", "name"),
        ("Giraffe", "id"),
        ("Giraffe", "keeper"),
        ("Node", "id"),
        ("AdminGiraffe", "name"),
        ("RawThing", "name"),
        ("Query", "giraffe"),
        ("Query", "admin"),
        ("Query", "raw"),
        ("Query", "greet"),
        ("Mutation", "rename"),
        ("Subscription", "tick"),
    ] {
        let matches = fields
            .iter()
            .filter(|record| record.type_name == type_name && record.field_name == field_name)
            .count();
        assert_eq!(
            matches, 1,
            "{type_name}.{field_name} was visited {matches} times"
        );
    }

    assert!(!recorder.find("Giraffe", "name").is_root);
    assert!(!recorder.find("Giraffe", "name").is_subscription);
    assert!(recorder.find("Query", "giraffe").is_root);
    assert!(!recorder.find("Query", "giraffe").is_subscription);
    assert!(recorder.find("Mutation", "rename").is_root);
    assert!(recorder.find("Subscription", "tick").is_root);
    assert!(recorder.find("Subscription", "tick").is_subscription);

    // Interface fields have no resolvers, so only the metadata hook runs for them.
    assert!(
        !recorder
            .wraps
            .lock()
            .unwrap()
            .iter()
            .any(|path| path == "Node.id"),
        "interface fields must not be resolver-wrapped"
    );

    // Resolver wrappers were installed once for every executable field, including raw fields.
    let wraps = recorder.wraps.lock().unwrap().clone();
    for path in [
        "Giraffe.name",
        "AdminGiraffe.name",
        "RawThing.name",
        "Query.giraffe",
        "Query.admin",
        "Query.raw",
        "Query.greet",
        "Mutation.rename",
        "Subscription.tick",
    ] {
        assert_eq!(
            wraps.iter().filter(|existing| *existing == path).count(),
            1,
            "{path} must be wrapped exactly once; saw {wraps:?}"
        );
    }
    assert_eq!(
        recorder
            .subscriptions
            .lock()
            .unwrap()
            .iter()
            .filter(|path| *path == "Subscription.tick")
            .count(),
        1
    );

    // Execute through the wrappers. The raw field has no declared resolver, so the plugin
    // installed the property resolver first and wrapped that.
    let response = execute(
        &schema,
        "{ giraffe { name } admin { name } raw { name } greet(name: \"Hi\") }",
        Ctx::anonymous(),
    );
    assert!(!response.has_errors(), "{response:?}");
    let entered = recorder.entered.lock().unwrap().clone();
    for path in [
        "Giraffe.name",
        "AdminGiraffe.name",
        "RawThing.name",
        "Query.giraffe",
        "Query.admin",
        "Query.raw",
        "Query.greet",
    ] {
        assert!(
            entered.iter().any(|entry| entry == path),
            "{path} did not run; saw {entered:?}"
        );
    }
    assert_eq!(
        data(&response)
            .get("raw")
            .and_then(gql::Value::as_object)
            .and_then(|object| object.get("name")),
        Some(&gql::Value::from("raw"))
    );
}

#[test]
fn field_metadata_has_final_types_arguments_and_flags() {
    let recorder = Recorder::default();
    let _schema = coverage_schema(&recorder);

    let greet = recorder.find("Query", "greet");
    assert_eq!(
        greet.args,
        vec![
            ("name".to_owned(), "String!".to_owned()),
            ("times".to_owned(), "Int".to_owned()),
        ],
        "arguments must be the final coerced declarations, in order"
    );
    assert_eq!(greet.return_type, "String!");

    assert_eq!(recorder.find("Query", "admin").return_type, "AdminGiraffe");
    assert_eq!(recorder.find("RawThing", "name").return_type, "String!");
    assert_eq!(recorder.find("Subscription", "tick").return_type, "Int!");

    // `path()` is the same spelling the mock plugin accepts.
    let record = recorder.find("Query", "admin");
    assert_eq!(
        format!("{}.{}", record.type_name, record.field_name),
        "Query.admin"
    );
}

// =============================================================================================
// Ordering and field mutation
// =============================================================================================

struct OrderPlugin {
    label: &'static str,
    log: Arc<Mutex<Vec<String>>>,
}

impl Plugin<Ctx> for OrderPlugin {
    fn wrap_resolve(&self, _meta: &FieldMeta<'_>, next: gql::ResolveFn) -> gql::ResolveFn {
        let label = self.label;
        let log = Arc::clone(&self.log);
        Arc::new(move |context| {
            log.lock().unwrap().push(format!("{label}:before"));
            let next = Arc::clone(&next);
            let log = Arc::clone(&log);
            Box::pin(async move {
                let result = next(context).await;
                log.lock().unwrap().push(format!("{label}:after"));
                result
            })
        })
    }
}

#[test]
fn wrappers_compose_in_registration_order_with_the_first_outermost() {
    let log = Arc::new(Mutex::new(Vec::new()));
    let mut schema = Schema::<Ctx>::new();
    schema.plugin(OrderPlugin {
        label: "first",
        log: Arc::clone(&log),
    });
    schema.plugin(OrderPlugin {
        label: "second",
        log: Arc::clone(&log),
    });
    schema.query(|q| {
        let resolver_log = Arc::clone(&log);
        q.field("hello", move |_: NoArgs, _: &Ctx| {
            resolver_log.lock().unwrap().push("resolver".to_owned());
            "world"
        });
    });

    let schema = schema.build().expect("valid schema");
    let response = execute(&schema, "{ hello }", Ctx::anonymous());
    assert!(!response.has_errors(), "{response:?}");
    assert_eq!(
        log.lock().unwrap().clone(),
        vec![
            "first:before".to_owned(),
            "second:before".to_owned(),
            "resolver".to_owned(),
            "second:after".to_owned(),
            "first:after".to_owned(),
        ]
    );
}

struct AddArgument;

impl Plugin<Ctx> for AddArgument {
    fn field(&self, meta: &FieldMeta<'_>, field: &mut gql::Field) {
        if meta.path() == "Query.hello" {
            field.args.insert(
                "fromPlugin".into(),
                gql::InputValue::new("fromPlugin", gql::TypeRef::string()),
            );
        }
    }
}

struct ObserveArguments(Arc<Mutex<Vec<bool>>>);

impl Plugin<Ctx> for ObserveArguments {
    fn field(&self, meta: &FieldMeta<'_>, _field: &mut gql::Field) {
        if meta.path() == "Query.hello" {
            self.0
                .lock()
                .unwrap()
                .push(meta.args.contains_key("fromPlugin"));
        }
    }
}

#[test]
fn each_field_hook_sees_changes_from_earlier_plugins() {
    let observed = Arc::new(Mutex::new(Vec::new()));
    let mut schema = Schema::<Ctx>::new();
    schema.plugin(AddArgument);
    schema.plugin(ObserveArguments(Arc::clone(&observed)));
    schema.query(|q| {
        q.field("hello", |_: NoArgs, _: &Ctx| "world");
    });

    let schema = schema.build().expect("valid schema");
    assert_eq!(observed.lock().unwrap().clone(), vec![true]);
    let field = schema
        .object_type("Query")
        .and_then(|query| query.get_field("hello"))
        .expect("Query.hello");
    assert!(field.args.contains_key("fromPlugin"));
}

struct MutationPlugin {
    configured: Arc<Mutex<Vec<(bool, bool, bool)>>>,
}

impl Plugin<Ctx> for MutationPlugin {
    fn field(&self, meta: &FieldMeta<'_>, field: &mut gql::Field) {
        if meta.path() == "Query.hello" {
            field.description = Some("Changed by a plugin.".into());
            field.deprecation_reason = Some("Use `hi`.".into());
            field.complexity = Some(Arc::new(|_| 9));
            field
                .directives
                .push(gql::AppliedDirective::new("audit").argument("scope", "read"));
        }
    }

    fn configure(&self, builder: &mut gql::SchemaBuilder) {
        self.configured.lock().unwrap().push((
            builder.has_type("Giraffe"),
            builder.has_type("Node"),
            builder.has_type("Query"),
        ));
        builder.add_directive(
            gql::DirectiveDefinition::new("audit")
                .arg(gql::InputValue::new("scope", gql::TypeRef::string()))
                .location(gql::ast::DirectiveLocation::FieldDefinition),
        );
    }
}

#[test]
fn field_hook_can_change_metadata_and_configure_sees_the_types() {
    let configured = Arc::new(Mutex::new(Vec::new()));
    let mut schema = Schema::<Ctx>::new();
    schema.plugin(MutationPlugin {
        configured: Arc::clone(&configured),
    });
    schema.interface("Node", |i| {
        i.field::<gurafi::ID>("id");
    });
    schema.object::<Giraffe>(|o| {
        o.expose(expose!(Giraffe::name));
    });
    schema.query(|q| {
        q.field("hello", |_: NoArgs, _: &Ctx| "world");
    });

    let schema = schema.build().expect("valid schema");

    assert_eq!(
        configured.lock().unwrap().clone(),
        vec![(true, true, true)],
        "configure runs once, after types and roots are registered"
    );

    let field = schema
        .object_type("Query")
        .and_then(|query| query.get_field("hello"))
        .expect("Query.hello");
    assert_eq!(field.description.as_deref(), Some("Changed by a plugin."));
    assert_eq!(field.deprecation_reason.as_deref(), Some("Use `hi`."));
    assert_eq!(
        field.complexity.as_ref().expect("complexity")(&gql::Object::new()),
        9
    );
    assert_eq!(field.directives.len(), 1);
    assert_eq!(field.directives[0].name.as_ref(), "audit");
    assert_eq!(
        field.directives[0].get_argument("scope"),
        Some(&gql::Value::from("read"))
    );
}

// =============================================================================================
// Authorization and validation
// =============================================================================================

#[test]
fn field_level_authorization_still_runs_inside_the_plugin_chain() {
    let calls = Arc::new(Mutex::new(0usize));
    let recorder = Recorder::default();
    let resolver_calls = Arc::clone(&calls);

    let mut schema = Schema::<Ctx>::new();
    schema.plugin(recorder.clone());
    schema.query(|q| {
        q.field("secret", move |_: NoArgs, _: &Ctx| {
            *resolver_calls.lock().unwrap() += 1;
            "classified"
        })
        .auth(|ctx: &Ctx| ctx.logged_in)
        .auth_error("no entry");
    });
    let schema = schema.build().expect("valid schema");

    let denied = execute(&schema, "{ secret }", Ctx::anonymous());
    assert!(denied.has_errors(), "{denied:?}");
    assert!(
        denied
            .errors
            .iter()
            .any(|error| error.message().contains("no entry")),
        "{denied:?}"
    );
    assert_eq!(*calls.lock().unwrap(), 0);
    // Field-level authorization was installed when the field was declared, so the plugin wraps
    // it. It sees the field start, calls the authorization guard, and the guard stops the chain.
    assert!(
        recorder
            .entered
            .lock()
            .unwrap()
            .iter()
            .any(|path| path == "Query.secret")
    );

    let allowed = execute(&schema, "{ secret }", Ctx::logged_in());
    assert!(!allowed.has_errors(), "{allowed:?}");
    assert_eq!(*calls.lock().unwrap(), 1);
}

#[test]
fn object_level_authorization_wraps_plugins() {
    let recorder = Recorder::default();
    let mut schema = Schema::<Ctx>::new();
    schema.plugin(recorder.clone());
    schema.object::<Giraffe>(|o| {
        o.auth(|ctx: &Ctx| ctx.logged_in)
            .auth_error("object denied");
        o.expose(expose!(Giraffe::name));
    });
    schema.query(|q| {
        q.field("giraffe", |_: NoArgs, _: &Ctx| Giraffe {
            name: "Melman".into(),
        });
    });
    let schema = schema.build().expect("valid schema");

    let denied = execute(&schema, "{ giraffe { name } }", Ctx::anonymous());
    assert!(denied.has_errors(), "{denied:?}");
    assert!(
        denied
            .errors
            .iter()
            .any(|error| error.message().contains("object denied")),
        "{denied:?}"
    );
    // Object authorization is installed after plugins, so the nested field's plugin wrapper is
    // never entered when the object rule denies.
    assert!(
        !recorder
            .entered
            .lock()
            .unwrap()
            .iter()
            .any(|path| path == "Giraffe.name"),
        "object authorization must run before the field's plugin wrapper"
    );
}

#[test]
fn argument_validation_still_runs_once_with_a_plugin() {
    let calls = Arc::new(Mutex::new(0usize));
    let recorder = Recorder::default();
    let resolver_calls = Arc::clone(&calls);

    let mut schema = Schema::<Ctx>::new();
    schema.plugin(recorder.clone());
    schema.query(|q| {
        q.field("greet", move |args: GreetArgs, _: &Ctx| {
            *resolver_calls.lock().unwrap() += 1;
            args.name
        })
        .validate::<GreetArgs>(|args, _| {
            if args.name.is_empty() {
                Err(GraphQLError::new("name must not be empty"))
            } else {
                Ok(())
            }
        });
    });
    let schema = schema.build().expect("valid schema");

    let invalid = execute(&schema, "{ greet(name: \"\") }", Ctx::anonymous());
    assert!(
        invalid
            .errors
            .iter()
            .any(|error| error.message().contains("name must not be empty")),
        "{invalid:?}"
    );
    assert_eq!(*calls.lock().unwrap(), 0);

    let valid = execute(&schema, "{ greet(name: \"Ada\") }", Ctx::anonymous());
    assert!(!valid.has_errors(), "{valid:?}");
    assert_eq!(*calls.lock().unwrap(), 1);
    assert_eq!(
        recorder
            .wraps
            .lock()
            .unwrap()
            .iter()
            .filter(|path| *path == "Query.greet")
            .count(),
        1,
        "the resolver must not be wrapped twice"
    );
}

// =============================================================================================
// Subscriptions
// =============================================================================================

#[test]
fn subscription_factories_and_per_event_resolvers_are_wrapped() {
    let recorder = Recorder::default();
    let mut schema = Schema::<Ctx>::new();
    schema.plugin(recorder.clone());
    schema.query(|q| {
        q.field("ok", |_: NoArgs, _: &Ctx| true);
    });
    schema.subscription(|s| {
        s.field("tick", |_: NoArgs, _: &Ctx| {
            futures::stream::iter(vec![
                GurafiResult::Ok(1i32),
                GurafiResult::Ok(2i32),
                GurafiResult::Ok(3i32),
            ])
        });
    });
    let schema = schema.build().expect("valid schema");

    let subscription = futures::executor::block_on(gql::subscribe(
        &schema,
        gql::Request::new("subscription { tick }"),
        &gql::ExecutionOptions::with_context(Ctx::anonymous()),
    ))
    .expect("subscription starts");

    let events = futures::executor::block_on(subscription.stream.take(2).collect::<Vec<_>>());
    assert_eq!(events.len(), 2);
    for event in &events {
        assert!(!event.has_errors(), "{event:?}");
    }
    assert_eq!(data(&events[0]).get("tick"), Some(&gql::Value::Int(1)));
    assert_eq!(data(&events[1]).get("tick"), Some(&gql::Value::Int(2)));

    assert_eq!(
        recorder
            .subscriptions
            .lock()
            .unwrap()
            .iter()
            .filter(|path| *path == "Subscription.tick")
            .count(),
        1
    );
    assert_eq!(
        recorder
            .subscribe_entered
            .lock()
            .unwrap()
            .iter()
            .filter(|path| *path == "Subscription.tick")
            .count(),
        1
    );
    assert_eq!(
        recorder
            .entered
            .lock()
            .unwrap()
            .iter()
            .filter(|path| *path == "Subscription.tick")
            .count(),
        2,
        "the per-event resolver is wrapped for every event"
    );
}

struct SubscribeOrderPlugin {
    label: &'static str,
    log: Arc<Mutex<Vec<String>>>,
}

impl Plugin<Ctx> for SubscribeOrderPlugin {
    fn wrap_subscribe(&self, _meta: &FieldMeta<'_>, next: gql::SubscribeFn) -> gql::SubscribeFn {
        let label = self.label;
        let log = Arc::clone(&self.log);
        Arc::new(move |context| {
            log.lock().unwrap().push(format!("{label}:before"));
            let next = Arc::clone(&next);
            let log = Arc::clone(&log);
            Box::pin(async move {
                let result = next(context).await;
                log.lock().unwrap().push(format!("{label}:after"));
                result
            })
        })
    }
}

#[test]
fn subscription_wrappers_compose_in_registration_order() {
    let log = Arc::new(Mutex::new(Vec::new()));
    let mut schema = Schema::<Ctx>::new();
    schema.plugin(SubscribeOrderPlugin {
        label: "first",
        log: Arc::clone(&log),
    });
    schema.plugin(SubscribeOrderPlugin {
        label: "second",
        log: Arc::clone(&log),
    });
    schema.query(|q| {
        q.field("ok", |_: NoArgs, _: &Ctx| true);
    });
    schema.subscription(|s| {
        s.field("tick", |_: NoArgs, _: &Ctx| {
            futures::stream::iter(vec![GurafiResult::Ok(1i32)])
        });
    });
    let schema = schema.build().expect("valid schema");

    let subscription = futures::executor::block_on(gql::subscribe(
        &schema,
        gql::Request::new("subscription { tick }"),
        &gql::ExecutionOptions::with_context(Ctx::anonymous()),
    ))
    .expect("subscription starts");
    let _ = futures::executor::block_on(subscription.stream.take(1).collect::<Vec<_>>());
    assert_eq!(
        log.lock().unwrap().clone(),
        vec![
            "first:before".to_owned(),
            "second:before".to_owned(),
            "second:after".to_owned(),
            "first:after".to_owned(),
        ]
    );
}

// =============================================================================================
// Built-ins
// =============================================================================================

#[test]
fn tracing_plugin_emits_start_and_finish_events() {
    let events = Arc::new(Mutex::new(Vec::new()));
    let observed = Arc::clone(&events);
    let mut schema = Schema::<Ctx>::new();
    schema.plugin(TracingPlugin::new(move |event: &TraceEvent<'_>| {
        observed.lock().unwrap().push((
            event.phase,
            event.type_name.to_owned(),
            event.field_name.to_owned(),
            event.return_type.to_string(),
            event.elapsed.is_some(),
            event.failed,
        ));
    }));
    schema.object::<Giraffe>(|o| {
        o.expose(expose!(Giraffe::name));
    });
    schema.query(|q| {
        q.field("giraffe", |_: NoArgs, _: &Ctx| Giraffe {
            name: "Melman".into(),
        });
        q.field_result("fail", |_: NoArgs, _: &Ctx| {
            GurafiResult::<String>::Err(GraphQLError::new("boom"))
        });
    });
    let schema = schema.build().expect("valid schema");

    let response = execute(&schema, "{ giraffe { name } fail }", Ctx::anonymous());
    assert!(response.has_errors(), "{response:?}");

    let events = events.lock().unwrap().clone();
    let starts = events
        .iter()
        .filter(|event| event.0 == TracePhase::Start)
        .count();
    let finishes = events
        .iter()
        .filter(|event| event.0 == TracePhase::Finish)
        .count();
    assert_eq!(starts, 3, "{events:?}");
    assert_eq!(finishes, 3, "{events:?}");
    assert!(
        events
            .iter()
            .filter(|event| event.0 == TracePhase::Finish)
            .all(|event| event.4),
        "every finish event carries a duration: {events:?}"
    );
    assert!(
        events
            .iter()
            .any(|event| event.1 == "Query" && event.2 == "fail" && event.5),
        "the failing field must be marked as failed: {events:?}"
    );
    assert!(
        events
            .iter()
            .any(|event| event.1 == "Giraffe" && event.2 == "name" && event.3 == "String!"),
        "{events:?}"
    );
}

#[test]
fn response_extension_plugin_writes_extensions() {
    let mut schema = Schema::<Ctx>::new();
    schema.plugin(ResponseExtensionPlugin::new("servedBy", "gurafi").root_only());
    schema.object::<Giraffe>(|o| {
        o.expose(expose!(Giraffe::name));
    });
    schema.query(|q| {
        q.field("giraffe", |_: NoArgs, _: &Ctx| Giraffe {
            name: "Melman".into(),
        });
    });
    let schema = schema.build().expect("valid schema");

    let response = execute(&schema, "{ giraffe { name } }", Ctx::anonymous());
    assert!(!response.has_errors(), "{response:?}");
    let extensions = response.extensions.expect("extensions");
    assert_eq!(
        extensions.get("servedBy"),
        Some(&gql::Value::from("gurafi"))
    );
}

#[test]
fn mock_plugin_replaces_only_the_named_paths() {
    let mut schema = Schema::<Ctx>::new();
    schema.plugin(
        MockPlugin::new()
            .mock(
                "Query",
                "giraffe",
                gql::Value::external(Giraffe {
                    name: "mocked".into(),
                }),
            )
            .mock("Giraffe", "name", "patched"),
    );
    schema.object::<Giraffe>(|o| {
        o.expose(expose!(Giraffe::name));
    });
    schema.query(|q| {
        q.field("giraffe", |_: NoArgs, _: &Ctx| Giraffe {
            name: "real".into(),
        });
        q.field("other", |_: NoArgs, _: &Ctx| Giraffe {
            name: "other".into(),
        });
    });
    let schema = schema.build().expect("valid schema");

    let response = execute(
        &schema,
        "{ giraffe { name } other { name } }",
        Ctx::anonymous(),
    );
    assert!(!response.has_errors(), "{response:?}");
    let giraffe = data(&response)
        .get("giraffe")
        .and_then(gql::Value::as_object)
        .expect("giraffe object");
    assert_eq!(giraffe.get("name"), Some(&gql::Value::from("patched")));

    // `other` is not mocked, but its nested field is.
    let other = data(&response)
        .get("other")
        .and_then(gql::Value::as_object)
        .expect("other object");
    assert_eq!(other.get("name"), Some(&gql::Value::from("patched")));
}

#[test]
fn mock_plugin_only_replaces_the_named_field() {
    let mut schema = Schema::<Ctx>::new();
    schema.plugin(MockPlugin::new().mock_path("Query.hello", "mocked"));
    schema.query(|q| {
        q.field("hello", |_: NoArgs, _: &Ctx| "real");
        q.field("other", |_: NoArgs, _: &Ctx| "other");
    });
    let schema = schema.build().expect("valid schema");

    let response = execute(&schema, "{ hello other }", Ctx::anonymous());
    assert!(!response.has_errors(), "{response:?}");
    assert_eq!(
        data(&response).get("hello"),
        Some(&gql::Value::from("mocked"))
    );
    assert_eq!(
        data(&response).get("other"),
        Some(&gql::Value::from("other"))
    );
}

#[test]
fn root_metadata_uses_the_configured_root_names() {
    let recorder = Recorder::default();
    let mut schema = Schema::<Ctx>::new();
    schema.plugin(recorder.clone());
    schema.query_name("RootQuery");
    schema.mutation_name("RootMutation");
    schema.subscription_name("RootSubscription");
    schema.query(|q| {
        q.field("ok", |_: NoArgs, _: &Ctx| true);
    });
    schema.mutation(|m| {
        m.field("change", |_: NoArgs, _: &Ctx| true);
    });
    schema.subscription(|s| {
        s.field("events", |_: NoArgs, _: &Ctx| {
            futures::stream::iter(vec![GurafiResult::Ok(true)])
        });
    });
    let _schema = schema.build().expect("valid schema");

    assert!(recorder.find("RootQuery", "ok").is_root);
    assert!(recorder.find("RootMutation", "change").is_root);
    let events = recorder.find("RootSubscription", "events");
    assert!(events.is_root);
    assert!(events.is_subscription);
}

// =============================================================================================
// Raw fields and the leaf fast path
// =============================================================================================

struct MetadataOnly;

impl Plugin<Ctx> for MetadataOnly {
    fn field(&self, _meta: &FieldMeta<'_>, _field: &mut gql::Field) {}
}

#[test]
fn metadata_only_plugins_leave_raw_fields_on_the_fast_path() {
    let mut schema = Schema::<Ctx>::new();
    schema.plugin(MetadataOnly);
    schema.object::<RawThing>(|o| {
        o.raw_field::<String>("name");
    });
    schema.query(|q| {
        q.field("raw", |_: NoArgs, _: &Ctx| {
            Raw::<RawThing>::object(RawObject::new().field("name", "raw").build())
        });
    });
    let schema = schema.build().expect("valid schema");

    let field = schema
        .object_type("RawThing")
        .and_then(|object| object.get_field("name"))
        .expect("RawThing.name");
    assert!(
        field.resolve.is_none(),
        "a plugin that only reads metadata must not install a resolver on a raw field"
    );

    let response = execute(&schema, "{ raw { name } }", Ctx::anonymous());
    assert!(!response.has_errors(), "{response:?}");
    assert_eq!(
        data(&response)
            .get("raw")
            .and_then(gql::Value::as_object)
            .and_then(|object| object.get("name")),
        Some(&gql::Value::from("raw"))
    );
}

#[test]
fn wrapping_plugins_install_a_property_resolver_on_raw_fields() {
    let recorder = Recorder::default();
    let mut schema = Schema::<Ctx>::new();
    schema.plugin(recorder.clone());
    schema.object::<RawThing>(|o| {
        o.raw_field::<String>("name");
    });
    schema.query(|q| {
        q.field("raw", |_: NoArgs, _: &Ctx| {
            Raw::<RawThing>::object(RawObject::new().field("name", "raw").build())
        });
    });
    let schema = schema.build().expect("valid schema");

    let field = schema
        .object_type("RawThing")
        .and_then(|object| object.get_field("name"))
        .expect("RawThing.name");
    assert!(
        field.resolve.is_some(),
        "a wrapping plugin needs a resolver to wrap on a raw field"
    );

    let response = execute(&schema, "{ raw { name } }", Ctx::anonymous());
    assert!(!response.has_errors(), "{response:?}");
    assert!(
        recorder
            .entered
            .lock()
            .unwrap()
            .iter()
            .any(|path| path == "RawThing.name")
    );
}

// =============================================================================================
// Build reuse
// =============================================================================================

#[test]
fn building_a_clone_does_not_double_wrap_the_original() {
    let recorder = Recorder::default();
    let mut schema = Schema::<Ctx>::new();
    schema.plugin(recorder.clone());
    schema.query(|q| {
        q.field("hello", |_: NoArgs, _: &Ctx| "world");
    });

    let first = schema.build_clone().expect("first build");
    let second = schema.build_clone().expect("second build");
    assert!(!execute(&first, "{ hello }", Ctx::anonymous()).has_errors());
    assert!(!execute(&second, "{ hello }", Ctx::anonymous()).has_errors());

    // Each build sees the pristine declaration, so each installs one wrapper.
    assert_eq!(
        recorder
            .wraps
            .lock()
            .unwrap()
            .iter()
            .filter(|path| *path == "Query.hello")
            .count(),
        2,
        "one wrap per build, never a re-wrap of an already-built schema"
    );
}
