//! Independent review probes for the runtime-refs / plugins / loaders / error-unions wave.

#![allow(dead_code)]

use std::sync::{
    Arc, Mutex,
    atomic::{AtomicUsize, Ordering},
};

use futures::{StreamExt, executor::block_on};
use gurafi::{
    ErrorUnion, FieldMeta, GraphQLError, Loader, MockPlugin, NoArgs, Object, Plugin, RequestCache,
    ResponseExtensionPlugin, Schema, Union, expose,
};

#[derive(Clone, Default)]
struct Ctx;

#[derive(Clone, Object)]
struct User {
    name: String,
}

#[derive(Clone, Object)]
struct Post {
    title: String,
}

#[derive(Clone, Object)]
struct NotFound {
    message: String,
}

#[derive(ErrorUnion)]
enum LookupError {
    NotFound(NotFound),
}

fn execute(schema: &gql::Schema, query: &str) -> gql::Response {
    block_on(gql::execute(
        schema,
        gql::Request::new(query),
        &gql::ExecutionOptions::with_context(Ctx),
    ))
}

fn data(response: &gql::Response) -> &gql::Object {
    response
        .data
        .as_ref()
        .and_then(gql::Value::as_object)
        .unwrap_or_else(|| panic!("missing data: {response:?}"))
}

// ---------------------------------------------------------------------------------------------
// Runtime-named refs
// ---------------------------------------------------------------------------------------------

#[test]
fn refs_are_send_sync() {
    fn assert_send_sync<T: Send + Sync>() {}
    assert_send_sync::<gurafi::ObjectRef<User>>();
    assert_send_sync::<gurafi::InterfaceRef>();
    assert_send_sync::<gurafi::UnionRef>();
}

#[test]
fn object_and_interface_sharing_a_name_is_a_build_error() {
    let mut schema = Schema::<Ctx>::new();
    schema.interface("User", |i| {
        i.field::<String>("name");
    });
    let user = schema.object_ref::<User>("User");
    schema.implement(&user, |o| {
        o.expose(expose!(User::name));
    });
    schema.query(|q| {
        q.field("ok", |_: NoArgs, _: &Ctx| true);
    });

    let error = schema
        .build()
        .expect_err("an object and interface with one name must not both exist");
    assert!(
        error.to_string().contains("User"),
        "unexpected error: {error}"
    );
}

#[test]
fn object_and_union_sharing_a_name_is_a_build_error() {
    let mut schema = Schema::<Ctx>::new();
    schema.union("User", |u| {
        u.member::<Post>();
    });
    let user = schema.object_ref::<User>("User");
    schema.implement(&user, |o| {
        o.expose(expose!(User::name));
    });
    schema.query(|q| {
        q.field("ok", |_: NoArgs, _: &Ctx| true);
    });

    let error = schema
        .build()
        .expect_err("an object and union with one name must not both exist");
    assert!(
        error.to_string().contains("User"),
        "unexpected error: {error}"
    );
}

#[derive(Union)]
enum SearchResult {
    User(User),
    Post(Post),
}

#[test]
fn one_model_registered_under_two_names_in_one_union_is_rejected() {
    let mut schema = Schema::<Ctx>::new();
    let first = schema.object_ref::<User>("FirstUser");
    schema.implement(&first, |o| {
        o.expose(expose!(User::name));
    });
    let second = schema.object_ref::<User>("SecondUser");
    schema.implement(&second, |o| {
        o.expose(expose!(User::name));
    });
    let either = schema.union("Either", |u| {
        u.member_ref(&first);
        u.member_ref(&second);
    });
    schema.query(|q| {
        q.field_ref(&first, "either", |_: NoArgs, _: &Ctx| User {
            name: "ada".into(),
        })
        .as_abstract(either.clone());
    });

    // The build itself accepts the schema; the ambiguity only appears when a value is resolved.
    let built = schema.build().expect("schema is structurally valid");
    let response = execute(&built, "{ either { __typename } }");
    assert!(
        response.has_errors(),
        "two runtime names backed by the same `is_type_of` must not silently pick one: {response:?}"
    );
}

#[test]
fn as_abstract_after_as_object_ref_keeps_list_wrappers() {
    let mut schema = Schema::<Ctx>::new();
    let node = schema.interface_ref("Node");
    schema.implement_interface(&node, |i| {
        i.field::<String>("id");
    });
    let users = schema.object_ref::<User>("Users");
    schema.implement(&users, |o| {
        o.implements(&node);
        o.field("id", |u: &User, _: NoArgs, _: &Ctx| u.name.clone());
        o.expose(expose!(User::name));
    });
    schema.query(|q| {
        q.field_ref::<Vec<User>, NoArgs, _, _>(&users, "users", |_: NoArgs, _: &Ctx| {
            vec![User { name: "ada".into() }]
        })
        .non_null_items()
        .as_abstract(node.clone());
    });

    let built = schema.build().expect("valid schema");
    let query = built.query_type();
    let field = query.fields.get("users").expect("users field");
    assert_eq!(
        field.ty.to_string(),
        "[Node!]!",
        "as_abstract must keep the list and non-null wrappers"
    );
    let response = execute(&built, "{ users { __typename ... on Users { name } } }");
    assert!(!response.has_errors(), "{:?}", response.errors);
}

#[test]
fn reopening_a_runtime_union_keeps_its_resolve_type() {
    let mut schema = Schema::<Ctx>::new();
    let first = schema.object_ref::<User>("First");
    schema.implement(&first, |o| {
        o.expose(expose!(User::name));
    });
    let second = schema.object_ref::<Post>("Second");
    schema.implement(&second, |o| {
        o.expose(expose!(Post::title));
    });
    let union = schema.union("Either", |u| {
        u.member_ref(&first);
        u.resolve_type(|_, _| Ok(gql::Name::from("First")));
    });
    schema.implement_union(&union, |u| {
        u.member_ref(&second);
    });
    schema.query(|q| {
        q.field_ref_type::<User, NoArgs, _>(
            gql::TypeRef::named("Either"),
            "either",
            |_: NoArgs, _: &Ctx| User { name: "ada".into() },
        );
    });

    let built = schema.build().expect("valid schema");
    let union = built.union_type("Either").expect("union");
    assert!(
        union.resolve_type.is_some(),
        "reopening a union must not drop its custom resolve_type"
    );
}

// ---------------------------------------------------------------------------------------------
// Plugins
// ---------------------------------------------------------------------------------------------

#[derive(Clone, Default)]
struct Recorder {
    fields: Arc<Mutex<Vec<String>>>,
    wraps: Arc<Mutex<Vec<String>>>,
    entered: Arc<Mutex<Vec<String>>>,
}

impl<C: 'static> Plugin<C> for Recorder {
    fn field(&self, meta: &FieldMeta<'_>, _field: &mut gql::Field) {
        self.fields.lock().unwrap().push(meta.path());
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
}

#[test]
fn plugin_sees_result_union_generated_success_and_error_payloads() {
    let recorder = Recorder::default();
    let mut schema = Schema::<Ctx>::new();
    schema.plugin(recorder.clone());
    schema.object::<User>(|o| {
        o.expose(expose!(User::name));
    });
    schema.object::<NotFound>(|o| {
        o.expose(expose!(NotFound::message));
    });
    schema.query(|q| {
        q.field_result_union::<User, LookupError, NoArgs, _>("lookup", |_: NoArgs, _: &Ctx| {
            Ok(User { name: "ada".into() })
        });
    });
    let _ = schema.build().expect("valid schema");

    let fields = recorder.fields.lock().unwrap().clone();
    assert!(
        fields.iter().any(|path| path == "QueryLookupSuccess.data"),
        "generated success payload was not visited: {fields:?}"
    );
    assert!(
        fields.iter().any(|path| path == "NotFound.message"),
        "error payload was not visited: {fields:?}"
    );
}

#[test]
fn plugin_wraps_reopened_and_dynamic_fields_once_per_build() {
    let recorder = Recorder::default();
    let mut schema = Schema::<Ctx>::new();
    schema.plugin(recorder.clone());
    let users = schema.object_ref::<User>("Users");
    schema.implement(&users, |o| {
        o.expose(expose!(User::name));
    });
    schema.object_fields::<User>(|o| {
        o.field("extra", |u: &User, _: NoArgs, _: &Ctx| u.name.clone());
    });
    schema.query(|q| {
        q.field_ref(&users, "users", |_: NoArgs, _: &Ctx| User {
            name: "ada".into(),
        });
    });

    let built = schema.build().expect("valid schema");
    let wraps = recorder.wraps.lock().unwrap().clone();
    assert_eq!(
        wraps.iter().filter(|path| *path == "Users.name").count(),
        1,
        "{wraps:?}"
    );
    assert_eq!(
        wraps.iter().filter(|path| *path == "Query.users").count(),
        1,
        "{wraps:?}"
    );

    let response = execute(&built, "{ users { name } }");
    assert!(!response.has_errors(), "{:?}", response.errors);
    let entered = recorder.entered.lock().unwrap().clone();
    assert_eq!(
        entered.iter().filter(|path| *path == "Users.name").count(),
        1,
        "a resolver was wrapped more than once: {entered:?}"
    );
}

#[test]
fn response_extension_plugin_writes_only_root_fields() {
    let mut schema = Schema::<Ctx>::new();
    schema.plugin(ResponseExtensionPlugin::new("servedBy", "gurafi").root_only());
    schema.object::<User>(|o| {
        o.expose(expose!(User::name));
    });
    schema.query(|q| {
        q.field("user", |_: NoArgs, _: &Ctx| User { name: "ada".into() });
    });
    let built = schema.build().expect("valid schema");
    let response = execute(&built, "{ user { name } }");
    assert!(!response.has_errors(), "{:?}", response.errors);
    assert_eq!(
        response
            .extensions
            .as_ref()
            .and_then(|extensions| extensions.get("servedBy"))
            .and_then(gql::Value::as_str),
        Some("gurafi")
    );
}

// ---------------------------------------------------------------------------------------------
// Loaders
// ---------------------------------------------------------------------------------------------

#[test]
fn dropping_a_load_mid_batch_does_not_strand_the_key() {
    let loader = Loader::<u32, String>::new(|keys: Vec<u32>| async move {
        // Yield a few times so the batch is genuinely in flight when the waiter is dropped.
        YieldTimes(3).await;
        keys.into_iter().map(|key| Ok(key.to_string())).collect()
    });

    let mut first = Box::pin(loader.load(7));
    assert!(poll_once(first.as_mut()).is_pending());
    // Poll a second time: the batch is now in flight and the load awaits it.
    assert!(poll_once(first.as_mut()).is_pending());
    drop(first);

    let second = loader.load(7);
    let value = block_on(second).expect("a dropped waiter must not strand the key");
    assert_eq!(value, "7");
}

#[test]
fn loader_errors_are_cached_and_fanned_out() {
    let calls = Arc::new(AtomicUsize::new(0));
    let calls_for_fn = Arc::clone(&calls);
    let loader = Loader::<u32, String>::new(move |keys: Vec<u32>| {
        let calls = Arc::clone(&calls_for_fn);
        async move {
            calls.fetch_add(1, Ordering::SeqCst);
            keys.into_iter()
                .map(|_| Err(GraphQLError::new("boom")))
                .collect()
        }
    });

    let results = block_on(loader.load_many(vec![1, 1, 2]));
    assert_eq!(results.len(), 3);
    assert!(results.iter().all(Result::is_err));
    assert_eq!(calls.load(Ordering::SeqCst), 1, "one batch, deduplicated");
    let again = block_on(loader.load(2));
    assert!(again.is_err());
    assert_eq!(calls.load(Ordering::SeqCst), 1, "errors must be cached");
}

#[test]
fn loader_splits_large_batches_and_keeps_order() {
    let seen = Arc::new(Mutex::new(Vec::new()));
    let seen_for_fn = Arc::clone(&seen);
    let loader = Loader::<u32, u32>::with_config(
        move |keys: Vec<u32>| {
            let seen = Arc::clone(&seen_for_fn);
            async move {
                seen.lock().unwrap().push(keys.clone());
                keys.into_iter().map(Ok).collect()
            }
        },
        gurafi::LoaderConfig::new().with_max_batch_size(2),
    );

    let results = block_on(loader.load_many(vec![1, 2, 3, 4, 5]));
    assert_eq!(
        results.into_iter().map(Result::unwrap).collect::<Vec<_>>(),
        vec![1, 2, 3, 4, 5]
    );
    let batches = seen.lock().unwrap().clone();
    assert!(batches.iter().all(|batch| batch.len() <= 2), "{batches:?}");
}

#[test]
fn request_cache_isolated_per_instance_and_single_init() {
    let first = RequestCache::new();
    let second = RequestCache::new();
    let one = first.get_or_insert_with(|| String::from("one"));
    let two = second.get_or_insert_with(|| String::from("two"));
    assert_eq!(&*one, "one");
    assert_eq!(&*two, "two");
    assert_eq!(&*first.get::<String>().unwrap(), "one");
}

// ---------------------------------------------------------------------------------------------
// Typed error unions
// ---------------------------------------------------------------------------------------------

#[test]
fn optional_success_data_stays_nullable() {
    let mut schema = Schema::<Ctx>::new();
    schema.object::<User>(|o| {
        o.expose(expose!(User::name));
    });
    schema.object::<NotFound>(|o| {
        o.expose(expose!(NotFound::message));
    });
    schema.query(|q| {
        q.field_result_union::<Option<User>, LookupError, NoArgs, _>(
            "maybe",
            |_: NoArgs, _: &Ctx| Ok(None),
        );
    });
    let built = schema.build().expect("valid schema");
    let sdl = built.sdl();
    assert!(
        sdl.contains("type QueryMaybeSuccess") && sdl.contains("data: User\n"),
        "optional success data must be nullable in SDL:\n{sdl}"
    );
    let response = execute(
        &built,
        "{ maybe { ... on QueryMaybeSuccess { data { name } } } }",
    );
    assert!(!response.has_errors(), "{:?}", response.errors);
}

#[test]
fn field_auth_on_a_result_union_field_denies_the_whole_payload() {
    #[derive(Clone)]
    struct AuthCtx {
        allowed: bool,
    }
    let mut schema = Schema::<AuthCtx>::new();
    schema.object::<User>(|o| {
        o.expose(expose!(User::name));
    });
    schema.object::<NotFound>(|o| {
        o.expose(expose!(NotFound::message));
    });
    schema.query(|q| {
        q.field_result_union::<User, LookupError, NoArgs, _>("lookup", |_: NoArgs, _: &AuthCtx| {
            Ok(User { name: "ada".into() })
        })
        .auth(|ctx: &AuthCtx| ctx.allowed);
    });
    let built = schema.build().expect("valid schema");
    let denied = block_on(gql::execute(
        &built,
        gql::Request::new("{ lookup { __typename } }"),
        &gql::ExecutionOptions::with_context(AuthCtx { allowed: false }),
    ));
    assert!(denied.has_errors(), "{denied:?}");
    assert!(
        denied
            .errors
            .iter()
            .any(|error| error.message().contains("Not authorized")),
        "{:?}",
        denied.errors
    );
}

#[test]
fn a_generated_name_colliding_with_a_user_object_is_a_build_error() {
    let mut schema = Schema::<Ctx>::new();
    schema.object::<User>(|o| {
        o.expose(expose!(User::name));
    });
    schema.object::<NotFound>(|o| {
        o.expose(expose!(NotFound::message));
    });
    // Occupy the generated success name with a user-declared type.
    schema.object::<Post>(|o| {
        o.expose(expose!(Post::title));
    });
    let clash = schema.object_ref::<Post>("QueryLookupSuccess");
    schema.implement(&clash, |o| {
        o.expose(expose!(Post::title));
    });
    schema.query(|q| {
        q.field_result_union::<User, LookupError, NoArgs, _>("lookup", |_: NoArgs, _: &Ctx| {
            Ok(User { name: "ada".into() })
        });
    });

    let error = schema.build().expect_err("generated name collision");
    assert!(
        error.to_string().contains("QueryLookupSuccess"),
        "unexpected error: {error}"
    );
}

#[test]
fn result_union_list_and_abstract_success_values_resolve() {
    let mut schema = Schema::<Ctx>::new();
    let node = schema.interface_ref("Node");
    schema.implement_interface(&node, |i| {
        i.field::<String>("id");
    });
    schema.object::<User>(|o| {
        o.implements(&node);
        o.field("id", |u: &User, _: NoArgs, _: &Ctx| u.name.clone());
        o.expose(expose!(User::name));
    });
    schema.object::<Post>(|o| {
        o.implements(&node);
        o.field("id", |p: &Post, _: NoArgs, _: &Ctx| p.title.clone());
        o.expose(expose!(Post::title));
    });
    schema.object::<NotFound>(|o| {
        o.expose(expose!(NotFound::message));
    });
    schema.union("SearchResult", |u| {
        u.member::<User>().member::<Post>();
    });
    schema.query(|q| {
        q.field_result_union::<Vec<User>, LookupError, NoArgs, _>("users", |_: NoArgs, _: &Ctx| {
            Ok(vec![User { name: "ada".into() }])
        });
        q.field_result_union::<SearchResult, LookupError, NoArgs, _>(
            "node",
            |_: NoArgs, _: &Ctx| Ok(SearchResult::User(User { name: "ada".into() })),
        );
    });

    let built = schema.build().expect("valid schema");
    let response = execute(
        &built,
        "{ users { ... on QueryUsersSuccess { data { name } } } node { __typename ... on QueryNodeSuccess { data { ... on User { name } } } } }",
    );
    assert!(!response.has_errors(), "{:?}", response.errors);
    let users = data(&response)
        .get("users")
        .and_then(gql::Value::as_object)
        .expect("users payload");
    assert_eq!(
        users
            .get("__typename")
            .and_then(gql::Value::as_str)
            .or_else(|| {
                users
                    .get("data")
                    .and_then(gql::Value::as_list)
                    .map(|_| "QueryUsersSuccess")
            }),
        Some("QueryUsersSuccess")
    );
}

struct YieldTimes(u8);

impl std::future::Future for YieldTimes {
    type Output = ();

    fn poll(
        mut self: std::pin::Pin<&mut Self>,
        context: &mut std::task::Context<'_>,
    ) -> std::task::Poll<()> {
        if self.0 == 0 {
            std::task::Poll::Ready(())
        } else {
            self.0 -= 1;
            context.waker().wake_by_ref();
            std::task::Poll::Pending
        }
    }
}

fn poll_once<F: std::future::Future>(future: std::pin::Pin<&mut F>) -> std::task::Poll<F::Output> {
    let waker = futures::task::noop_waker();
    let mut context = std::task::Context::from_waker(&waker);
    future.poll(&mut context)
}

// ---------------------------------------------------------------------------------------------
// Authorization / plugin composition
// ---------------------------------------------------------------------------------------------

#[derive(Clone, Debug)]
struct GateCtx {
    object_allowed: bool,
    field_allowed: bool,
}

#[derive(Clone, Default)]
struct Entered(Arc<Mutex<Vec<String>>>);

impl Plugin<GateCtx> for Entered {
    fn wrap_resolve(&self, meta: &FieldMeta<'_>, next: gql::ResolveFn) -> gql::ResolveFn {
        let path = meta.path();
        let entered = Arc::clone(&self.0);
        Arc::new(move |context| {
            entered.lock().unwrap().push(path.clone());
            next(context)
        })
    }
}

fn gate_schema(plugin: impl Plugin<GateCtx> + 'static) -> (gql::Schema, Entered) {
    let entered = Entered::default();
    let mut schema = Schema::<GateCtx>::new();
    schema.plugin(entered.clone());
    schema.plugin(plugin);
    schema.object::<User>(|o| {
        o.auth(|ctx: &GateCtx| ctx.object_allowed);
        o.field("name", |u: &User, _: NoArgs, _: &GateCtx| u.name.clone());
    });
    schema.query(|q| {
        q.field("user", |_: NoArgs, _: &GateCtx| User { name: "ada".into() });
        q.field("secret", |_: NoArgs, _: &GateCtx| "classified".to_owned())
            .auth(|ctx: &GateCtx| ctx.field_allowed);
    });
    (schema.build().expect("valid schema"), entered)
}

fn run_gate(schema: &gql::Schema, query: &str, ctx: GateCtx) -> gql::Response {
    block_on(gql::execute(
        schema,
        gql::Request::new(query),
        &gql::ExecutionOptions::with_context(ctx),
    ))
}

#[test]
fn object_auth_runs_before_plugins_and_field_auth_runs_inside() {
    let (schema, entered) = gate_schema(MockPlugin::new());
    let denied = run_gate(
        &schema,
        "{ user { name } secret }",
        GateCtx {
            object_allowed: false,
            field_allowed: false,
        },
    );
    assert!(denied.has_errors());
    let entered = entered.0.lock().unwrap().clone();
    assert!(
        !entered.iter().any(|path| path == "User.name"),
        "object auth must run before the plugin wrapper: {entered:?}"
    );
    assert!(
        entered.iter().any(|path| path == "Query.secret"),
        "field auth sits inside the plugin chain, so the wrapper still runs: {entered:?}"
    );
}

#[test]
fn mock_bypasses_field_auth_but_not_object_auth() {
    let (schema, _) = gate_schema(
        MockPlugin::new()
            .mock("Query", "secret", "mocked")
            .mock("User", "name", "mocked"),
    );

    // Field-level auth is bypassed by the replacement resolver.
    let response = run_gate(
        &schema,
        "{ secret }",
        GateCtx {
            object_allowed: false,
            field_allowed: false,
        },
    );
    assert!(!response.has_errors(), "{:?}", response.errors);
    assert_eq!(
        data(&response).get("secret").and_then(gql::Value::as_str),
        Some("mocked")
    );

    // Object-level auth is installed after plugins and cannot be bypassed.
    let response = run_gate(
        &schema,
        "{ user { name } }",
        GateCtx {
            object_allowed: false,
            field_allowed: true,
        },
    );
    assert!(response.has_errors(), "{response:?}");
    assert!(
        response
            .errors
            .iter()
            .any(|error| error.message().contains("Not authorized")),
        "{:?}",
        response.errors
    );
}

#[test]
fn mock_bypasses_argument_validation() {
    #[derive(gurafi::Input)]
    struct Args {
        value: String,
    }
    let mut schema = Schema::<Ctx>::new();
    schema.plugin(MockPlugin::new().mock("Query", "checked", "mocked"));
    schema.query(|q| {
        q.field("checked", |args: Args, _: &Ctx| args.value)
            .validate::<Args>(|_args, _ctx| Err(GraphQLError::new("must not be empty")));
    });
    let built = schema.build().expect("valid schema");
    let response = execute(&built, r#"{ checked(value: "x") }"#);
    assert!(!response.has_errors(), "{:?}", response.errors);
    assert_eq!(
        data(&response).get("checked").and_then(gql::Value::as_str),
        Some("mocked")
    );
}

// ---------------------------------------------------------------------------------------------
// Schema composition with generated and runtime-named types
// ---------------------------------------------------------------------------------------------

fn union_schema() -> gql::Schema {
    let mut schema = Schema::<Ctx>::new();
    let users = schema.object_ref::<User>("Users");
    schema.implement(&users, |o| {
        o.expose(expose!(User::name));
    });
    schema.object::<Post>(|o| {
        o.expose(expose!(Post::title));
    });
    schema.union("Either", |u| {
        u.member_ref(&users);
        u.member::<Post>();
    });
    schema.object::<NotFound>(|o| {
        o.expose(expose!(NotFound::message));
    });
    schema.query(|q| {
        q.field_ref(&users, "users", |_: NoArgs, _: &Ctx| User {
            name: "ada".into(),
        });
        q.field_result_union::<Post, LookupError, NoArgs, _>("post", |_: NoArgs, _: &Ctx| {
            Ok(Post {
                title: "hello".into(),
            })
        });
        q.field_ref_type::<User, NoArgs, _>(
            gql::TypeRef::named("Either"),
            "either",
            |_: NoArgs, _: &Ctx| User { name: "ada".into() },
        );
    });
    schema.build().expect("valid schema")
}

#[test]
fn merging_a_generated_schema_with_itself_keeps_resolvers() {
    let schema = union_schema();
    let merged = schema.merge(&schema).expect("a schema merges with itself");
    assert!(merged.object_type("QueryPostSuccess").is_some());
    assert!(merged.union_type("QueryPostResult").is_some());

    let response = execute(
        &merged,
        "{ users { name } post { ... on QueryPostSuccess { data { title } } } either { __typename } }",
    );
    assert!(!response.has_errors(), "{:?}", response.errors);
}

#[test]
fn extending_generated_sdl_keeps_generated_and_dynamic_resolvers() {
    let schema = union_schema();
    let extended = schema
        .extend_sdl("extend type Query { extra: String }")
        .expect("schema extends");
    let response = execute(
        &extended,
        "{ extra post { ... on QueryPostSuccess { data { title } } } either { __typename } }",
    );
    assert!(!response.has_errors(), "{:?}", response.errors);
    assert!(
        data(&response)
            .get("extra")
            .is_some_and(gql::Value::is_null),
        "an SDL-only field has no resolver and reads as null"
    );
    assert_eq!(
        data(&response)
            .get("post")
            .and_then(gql::Value::as_object)
            .and_then(|payload| payload.get("data"))
            .and_then(gql::Value::as_object)
            .and_then(|post| post.get("title"))
            .and_then(gql::Value::as_str),
        Some("hello"),
        "the generated result-union resolver must survive extend_sdl"
    );
    assert_eq!(
        data(&response)
            .get("either")
            .and_then(gql::Value::as_object)
            .and_then(|either| either.get("__typename"))
            .and_then(gql::Value::as_str),
        Some("Users")
    );
}

#[test]
fn transforming_away_a_result_union_field_rebuilds() {
    let schema = union_schema();
    let transformed = schema
        .transform(|_, field| {
            if field.name.as_ref() == "post" {
                gql::FieldTransform::Remove
            } else {
                gql::FieldTransform::Keep
            }
        })
        .expect("removing a field leaves a valid schema");
    let response = execute(&transformed, "{ users { name } }");
    assert!(!response.has_errors(), "{:?}", response.errors);
}

// ---------------------------------------------------------------------------------------------
// Typed error unions: subscription + auth + plugins
// ---------------------------------------------------------------------------------------------

#[test]
fn typed_result_subscription_composes_with_auth_and_plugins() {
    let recorder = Recorder::default();
    let mut schema = Schema::<GateCtx>::new();
    schema.plugin(recorder.clone());
    schema.object::<User>(|o| {
        o.expose(expose!(User::name));
    });
    schema.object::<NotFound>(|o| {
        o.expose(expose!(NotFound::message));
    });
    schema.query(|q| {
        q.field("ok", |_: NoArgs, _: &GateCtx| true);
    });
    schema.subscription(|s| {
        s.field_result_union::<User, LookupError, NoArgs, _, _>(
            "created",
            |_: NoArgs, _: &GateCtx| {
                futures::stream::iter(vec![
                    Ok(User { name: "ada".into() }),
                    Err(LookupError::NotFound(NotFound {
                        message: "gone".into(),
                    })),
                ])
            },
        )
        .auth(|ctx: &GateCtx| ctx.field_allowed);
    });
    let built = schema.build().expect("valid schema");

    let denied = block_on(gql::subscribe(
        &built,
        gql::Request::new("subscription { created { __typename } }"),
        &gql::ExecutionOptions::with_context(GateCtx {
            object_allowed: true,
            field_allowed: false,
        }),
    ));
    assert!(
        denied.is_err(),
        "field auth must be checked when the subscription starts: {denied:?}"
    );

    let subscription = block_on(gql::subscribe(
        &built,
        gql::Request::new(
            "subscription { created { __typename ... on SubscriptionCreatedSuccess { data { name } } ... on NotFound { message } } }",
        ),
        &gql::ExecutionOptions::with_context(GateCtx {
            object_allowed: true,
            field_allowed: true,
        }),
    ))
    .expect("subscription starts");
    let events = block_on(subscription.stream.take(2).collect::<Vec<_>>());
    assert_eq!(events.len(), 2);
    assert!(!events[0].has_errors(), "{:?}", events[0].errors);
    assert!(!events[1].has_errors(), "{:?}", events[1].errors);
    assert!(
        recorder
            .fields
            .lock()
            .unwrap()
            .iter()
            .any(|path| path == "SubscriptionCreatedSuccess.data"),
        "plugin did not visit the generated subscription payload"
    );
}

// ---------------------------------------------------------------------------------------------
// Validation gaps
// ---------------------------------------------------------------------------------------------

#[test]
fn runtime_type_names_are_validated() {
    for name in ["Bad Name", "1Bad", "Bad-Name", ""] {
        let mut schema = Schema::<Ctx>::new();
        let users = schema.object_ref::<User>(name.to_owned());
        schema.implement(&users, |o| {
            o.expose(expose!(User::name));
        });
        schema.query(|q| {
            q.field_ref(&users, "users", |_: NoArgs, _: &Ctx| User {
                name: "ada".into(),
            });
        });
        let result = schema.build();
        assert!(
            result.is_err(),
            "`{name}` is not a valid GraphQL type name but the schema built successfully"
        );
    }
}

#[test]
fn as_dynamic_checks_the_declared_name_at_runtime() {
    let mut schema = Schema::<Ctx>::new();
    schema.object::<User>(|o| {
        o.expose(expose!(User::name));
    });
    schema.query(|q| {
        // Declares `String` while the resolver returns a `User` model. The builder can check the
        // wrapper shape and that the name exists, but not that the runtime value has that type;
        // the mismatch is reported when the field is completed.
        q.field_ref_type::<User, NoArgs, _>(
            gql::TypeRef::string(),
            "user",
            |_: NoArgs, _: &Ctx| User { name: "ada".into() },
        );
    });
    let schema = schema.build().expect("declaration is structurally valid");
    let response = execute(&schema, "{ user }");
    assert!(
        response
            .errors
            .iter()
            .any(|error| error.message().contains("could not be serialized")),
        "{:?}",
        response.errors
    );
}

#[test]
fn as_dynamic_rejects_an_interface_the_value_does_not_implement() {
    let mut schema = Schema::<Ctx>::new();
    let node = schema.interface_ref("Node");
    schema.implement_interface(&node, |i| {
        i.field::<String>("id");
    });
    schema.object::<Post>(|o| {
        o.expose(expose!(Post::title));
    });
    schema.query(|q| {
        q.field_ref_type::<Post, NoArgs, _>(
            gql::TypeRef::named("Node"),
            "post",
            |_: NoArgs, _: &Ctx| Post {
                title: "hello".into(),
            },
        );
    });
    let built = schema.build();
    if let Ok(built) = &built {
        let response = execute(built, "{ post { __typename } }");
        assert!(
            response.has_errors(),
            "Post is not a possible type of Node; the build or execution must reject it"
        );
    }
}

#[derive(Clone, Object)]
#[graphql(name = "Shared")]
struct ModelA {
    name: String,
}

#[derive(Clone, Object)]
#[graphql(name = "Shared")]
struct ModelB {
    name: String,
}

#[test]
fn merging_schemas_with_the_same_runtime_type_but_different_models_is_rejected() {
    let mut left = Schema::<Ctx>::new();
    left.object::<ModelA>(|o| {
        o.expose(expose!(ModelA::name));
    });
    left.query(|q| {
        q.field("shared", |_: NoArgs, _: &Ctx| ModelA {
            name: "left".into(),
        });
    });
    let left = left.build().expect("left schema is valid");

    let mut right = Schema::<Ctx>::new();
    right.object::<ModelB>(|o| {
        o.expose(expose!(ModelB::name));
    });
    right.query(|q| {
        q.field("shared", |_: NoArgs, _: &Ctx| ModelB {
            name: "right".into(),
        });
    });
    let right = right.build().expect("right schema is valid");

    // The definitions are structurally identical (both name a `Shared` object with `name`), but
    // each side carries a different `is_type_of` and resolver. Merge must not pick one by order.
    for error in [
        left.merge(&right).expect_err("left conflict"),
        right.merge(&left).expect_err("right conflict"),
    ] {
        let message = error.to_string();
        assert!(
            message.contains("Shared") || message.contains("Query.shared"),
            "{message}"
        );
    }
}

#[test]
fn interface_field_auth_is_not_bypassed_by_mock_even_though_object_field_auth_is() {
    #[derive(Clone, Debug)]
    struct Gate {
        allow: bool,
    }
    let mut schema = Schema::<Gate>::new();
    schema.plugin(MockPlugin::new().mock("User", "name", "mocked"));
    let named = schema.interface("Named", |i| {
        i.field::<String>("name").auth(|ctx: &Gate| ctx.allow);
    });
    schema.object::<User>(|o| {
        o.implements(&named);
        o.field("name", |u: &User, _: NoArgs, _: &Gate| u.name.clone());
    });
    schema.query(|q| {
        q.field("user", |_: NoArgs, _: &Gate| User { name: "ada".into() });
    });
    let built = schema.build().expect("valid schema");
    let response = block_on(gql::execute(
        &built,
        gql::Request::new("{ user { name } }"),
        &gql::ExecutionOptions::with_context(Gate { allow: false }),
    ));
    // The mock is installed first; interface field authorization is installed afterwards in
    // `apply_guards`, so it is *outside* the plugin chain and still denies.
    assert!(
        response.has_errors(),
        "interface field auth unexpectedly ran inside the mock: {response:?}"
    );
}

#[test]
fn object_auth_on_a_runtime_named_object_is_enforced() {
    #[derive(Clone, Debug)]
    struct Gate {
        allow: bool,
    }
    let mut schema = Schema::<Gate>::new();
    let admin = schema.object_ref::<User>("AdminUser");
    schema.implement(&admin, |o| {
        o.auth(|ctx: &Gate| ctx.allow);
        o.expose(expose!(User::name));
    });
    schema.query(|q| {
        q.field_ref(&admin, "admin", |_: NoArgs, _: &Gate| User {
            name: "ada".into(),
        });
    });
    let built = schema.build().expect("valid schema");
    let response = block_on(gql::execute(
        &built,
        gql::Request::new("{ admin { name } }"),
        &gql::ExecutionOptions::with_context(Gate { allow: false }),
    ));
    assert!(response.has_errors(), "{response:?}");
    assert!(
        response
            .errors
            .iter()
            .any(|error| error.message().contains("Not authorized")),
        "{:?}",
        response.errors
    );
}

#[test]
fn interface_field_auth_survives_reopening_the_interface() {
    #[derive(Clone, Debug)]
    struct Gate {
        allow: bool,
    }
    let mut schema = Schema::<Gate>::new();
    let named = schema.interface("Named", |i| {
        i.field::<String>("name").auth(|ctx: &Gate| ctx.allow);
    });
    schema.interface_fields("Named", |i| {
        i.field::<String>("nickname");
    });
    schema.object::<User>(|o| {
        o.implements(&named);
        o.field("name", |u: &User, _: NoArgs, _: &Gate| u.name.clone());
        o.field("nickname", |u: &User, _: NoArgs, _: &Gate| u.name.clone());
    });
    schema.query(|q| {
        q.field("user", |_: NoArgs, _: &Gate| User { name: "ada".into() });
    });
    let built = schema.build().expect("valid schema");
    let response = block_on(gql::execute(
        &built,
        gql::Request::new("{ user { name nickname } }"),
        &gql::ExecutionOptions::with_context(Gate { allow: false }),
    ));
    assert!(
        response.has_errors(),
        "reopening an interface must not drop inherited field auth: {response:?}"
    );
}

#[test]
fn prepared_queries_execute_generated_unions_and_runtime_refs() {
    let schema = union_schema();
    let source = "{ post { __typename ... on QueryPostSuccess { data { title } } } either { __typename ... on Users { name } } }";
    let prepared = gql::prepared::PreparedQuery::new(&schema, "wave", source, None)
        .expect("prepared query validates");
    let response = block_on(prepared.execute(None, &gql::ExecutionOptions::with_context(Ctx)));
    assert!(!response.has_errors(), "{:?}", response.errors);

    assert_eq!(
        data(&response)
            .get("either")
            .and_then(gql::Value::as_object)
            .and_then(|either| either.get("__typename"))
            .and_then(gql::Value::as_str),
        Some("Users")
    );
    assert_eq!(
        data(&response)
            .get("post")
            .and_then(gql::Value::as_object)
            .and_then(|post| post.get("__typename"))
            .and_then(gql::Value::as_str),
        Some("QueryPostSuccess")
    );
}
