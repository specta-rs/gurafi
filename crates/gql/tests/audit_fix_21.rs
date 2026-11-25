use std::sync::{
    Arc,
    atomic::{AtomicBool, AtomicUsize, Ordering},
};

use futures::{StreamExt, executor::block_on};
use gql::{
    AuthorizationContext, Authorizer, ExecutionOptions, Field, GraphQLError, Object, ObjectType,
    PreparedQuery, Request, Schema, TypeRef, Value,
};

struct DummyPrincipal {
    id: &'static str,
    allowed: Arc<AtomicBool>,
}

struct PrincipalAuthorizer {
    principal: Arc<DummyPrincipal>,
    calls: Arc<AtomicUsize>,
}

impl Authorizer for PrincipalAuthorizer {
    fn authorize(&self, context: &AuthorizationContext<'_>) -> Result<(), GraphQLError> {
        self.calls.fetch_add(1, Ordering::SeqCst);
        assert_eq!(context.parent_type, "Subscription");
        assert_eq!(context.field_name, "protected");
        assert_eq!(context.path.len(), 1);

        let observed = context
            .context
            .data::<Arc<DummyPrincipal>>()
            .expect("dummy principal in execution context");
        assert!(Arc::ptr_eq(observed, &self.principal));
        assert_eq!(observed.id, "dummy-client");

        if observed.allowed.load(Ordering::SeqCst) {
            Ok(())
        } else {
            Err(GraphQLError::new("dummy permission revoked"))
        }
    }
}

fn fixture() -> (
    Schema,
    Arc<DummyPrincipal>,
    Arc<AtomicUsize>,
    Arc<AtomicUsize>,
) {
    let principal = Arc::new(DummyPrincipal {
        id: "dummy-client",
        allowed: Arc::new(AtomicBool::new(true)),
    });
    let authorizer_calls = Arc::new(AtomicUsize::new(0));
    let resolver_calls = Arc::new(AtomicUsize::new(0));

    let resolve_calls = Arc::clone(&resolver_calls);
    let protected = Field::new("protected", TypeRef::string())
        .subscribe(|_| {
            Box::pin(async {
                Ok(futures::stream::iter(vec![Ok(Value::from("dummy-protected-event"))]).boxed())
            })
        })
        .resolve(move |context| {
            resolve_calls.fetch_add(1, Ordering::SeqCst);
            Box::pin(async move { Ok(context.parent.clone()) })
        });

    let schema = Schema::builder()
        .object(ObjectType::new("Query").field(Field::new("ping", TypeRef::string())))
        .object(ObjectType::new("Subscription").field(protected))
        .query("Query")
        .subscription("Subscription")
        .build()
        .expect("valid schema");

    (schema, principal, authorizer_calls, resolver_calls)
}

fn options(principal: &Arc<DummyPrincipal>, calls: &Arc<AtomicUsize>) -> ExecutionOptions {
    ExecutionOptions::default()
        .context(Arc::clone(principal))
        .authorizer(PrincipalAuthorizer {
            principal: Arc::clone(principal),
            calls: Arc::clone(calls),
        })
}

fn verify_after_revocation(
    path: &str,
    response: gql::Response,
    authorizer_calls: &AtomicUsize,
    resolver_calls: &AtomicUsize,
) {
    let calls = authorizer_calls.load(Ordering::SeqCst);
    let resolves = resolver_calls.load(Ordering::SeqCst);

    assert_eq!(
        calls, 2,
        "{path}: the root authorizer must make a fresh decision before the event resolver"
    );
    assert_eq!(resolves, 0, "{path}: a denied root resolver must not run");
    assert_eq!(
        response.data,
        Some(Value::object(Object::from_fields([(
            "protected",
            Value::Null,
        )])))
    );
    assert_eq!(response.errors.len(), 1, "{path}: {response:?}");
    assert_eq!(response.errors[0].message(), "dummy permission revoked");
    assert_eq!(
        response.errors[0].path(),
        Some(&[gql::PathSegment::field("protected")][..])
    );
}

#[test]
fn normal_subscription_reauthorizes_root_after_revocation() {
    let (schema, principal, authorizer_calls, resolver_calls) = fixture();
    let mut subscription = block_on(gql::subscribe(
        &schema,
        Request::new("subscription { protected }"),
        &options(&principal, &authorizer_calls),
    ))
    .expect("subscription setup is authorized");

    assert_eq!(authorizer_calls.load(Ordering::SeqCst), 1);
    assert_eq!(resolver_calls.load(Ordering::SeqCst), 0);
    principal.allowed.store(false, Ordering::SeqCst);

    let response = block_on(async { subscription.stream.next().await.expect("one event") });
    verify_after_revocation("normal", response, &authorizer_calls, &resolver_calls);
}

#[test]
fn prepared_subscription_reauthorizes_root_after_revocation() {
    let (schema, principal, authorizer_calls, resolver_calls) = fixture();
    let prepared = PreparedQuery::new(&schema, "protected", "subscription { protected }", None)
        .expect("prepared subscription");
    let mut subscription =
        block_on(prepared.subscribe(None, &options(&principal, &authorizer_calls)))
            .expect("subscription setup is authorized");

    assert_eq!(authorizer_calls.load(Ordering::SeqCst), 1);
    assert_eq!(resolver_calls.load(Ordering::SeqCst), 0);
    principal.allowed.store(false, Ordering::SeqCst);

    let response = block_on(async { subscription.stream.next().await.expect("one event") });
    verify_after_revocation("prepared", response, &authorizer_calls, &resolver_calls);
}
