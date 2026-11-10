//! Review probes for the GraphQL spec-gap commit (78d3d8f).
//!
//! These are *review* tests: each one encodes the behaviour the October 2021 specification (or a
//! documented deviation from it) requires. A failure here is a finding, not a regression of the
//! library's own suite.

use std::sync::{
    Arc, Mutex,
    atomic::{AtomicUsize, Ordering},
};

use futures::executor::block_on;
use futures::{FutureExt, StreamExt, future::BoxFuture};
use gql::ast::DirectiveLocation;
use gql::{
    AppliedDirective, AuthorizationContext, Authorizer, DirectiveDefinition, ExecutionOptions,
    Field, GraphQLError, InputObjectType, InputValue, Object, ObjectType, Request, Response,
    ScalarType, Schema, TypeRef, ValidationOptions, Value, ast,
};

fn value(fields: impl IntoIterator<Item = (&'static str, Value)>) -> Value {
    Value::object(Object::from_fields(fields))
}

fn run(schema: &Schema, query: &str) -> Response {
    block_on(gql::execute(
        schema,
        Request::new(query),
        &ExecutionOptions::default(),
    ))
}

fn run_vars(schema: &Schema, query: &str, variables: Object) -> Response {
    block_on(gql::execute(
        schema,
        Request::new(query).variables(&variables),
        &ExecutionOptions::default(),
    ))
}

fn data_field<'a>(response: &'a Response, name: &str) -> &'a Value {
    response
        .data
        .as_ref()
        .and_then(Value::as_object)
        .and_then(|object| object.get(name))
        .unwrap_or_else(|| panic!("response has no field {name}: {response:?}"))
}

fn validation_errors(schema: &Schema, query: &str) -> Vec<GraphQLError> {
    let document = gql::parser::parse_query(query).expect("valid syntax");
    gql::validate(schema, &document, &ValidationOptions::default())
}

fn simple_schema() -> Schema {
    Schema::builder()
        .object(
            ObjectType::new("Query").field(
                Field::new("text", TypeRef::string())
                    .resolve(|_| Box::pin(async { Ok(Value::from("ok")) })),
            ),
        )
        .query("Query")
        .build()
        .expect("valid schema")
}

// ===============================================================================================
// Schema-level applied directives (previously deferred)
// ===============================================================================================

#[test]
fn schema_applied_directives_survive_the_sdl_round_trip() {
    let source = r#"
    directive @tag(version: String!) on SCHEMA
    schema @tag(version: "1") { query: Query }
    type Query { f: String }
    "#;
    let schema = Schema::from_sdl(source).expect("valid schema");
    let printed = schema.sdl();
    eprintln!("--- printed ---\n{printed}\n---------------");
    assert!(
        printed.contains(r#"@tag(version: "1")"#),
        "schema-level directive was dropped from the printed SDL:\n{printed}"
    );
}

#[test]
fn schema_applied_directives_are_visible_in_the_model() {
    // There is no accessor at all today; this documents the missing API surface.
    let schema = Schema::from_sdl(
        r#"
        directive @tag on SCHEMA
        schema @tag { query: Query }
        type Query { f: String }
        "#,
    )
    .expect("valid schema");
    eprintln!("schema description = {:?}", schema.description());
    eprintln!("sdl = {}", schema.sdl());
}

// ===============================================================================================
// Directive definitions with @deprecated
// ===============================================================================================

#[test]
fn printed_directive_definition_with_deprecation_reparses() {
    let schema = Schema::builder()
        .directive(
            DirectiveDefinition::new("old")
                .location(DirectiveLocation::FieldDefinition)
                .deprecation_reason("Use @new."),
        )
        .object(ObjectType::new("Query").field(Field::new("f", TypeRef::string())))
        .query("Query")
        .build()
        .expect("valid schema");

    let printed = schema.sdl();
    eprintln!("--- printed ---\n{printed}\n---------------");
    assert!(
        printed.contains("@deprecated(reason: \"Use @new.\")"),
        "{printed}"
    );
    let reparsed = Schema::from_sdl(&printed).expect("printed SDL re-parses");
    let directive = reparsed
        .get_directive("old")
        .expect("@old survives the round trip");
    assert_eq!(directive.deprecation_reason.as_deref(), Some("Use @new."));
    assert_eq!(directive.directives.len(), 1);
}

#[test]
fn sdl_accepts_directives_on_a_directive_definition() {
    let source = r#"
    directive @old @deprecated(reason: "Use @new.") on FIELD_DEFINITION
    type Query { f: String }
    "#;
    let schema = Schema::from_sdl(source).expect("directives on a directive definition");
    let directive = schema.get_directive("old").expect("old");
    assert_eq!(directive.deprecation_reason.as_deref(), Some("Use @new."));
    assert_eq!(directive.directives.len(), 1);
    assert_eq!(
        schema.sdl(),
        Schema::from_sdl(&schema.sdl()).expect("reparse").sdl()
    );
}

#[test]
fn arbitrary_directives_on_a_directive_definition_round_trip() {
    let source = r#"
    directive @tag(name: String) on DIRECTIVE_DEFINITION
    directive @old @deprecated(reason: "Use @new.") @tag(name: "meta") on FIELD_DEFINITION
    type Query { f: String }
    "#;
    let schema = Schema::from_sdl(source).expect("valid schema");
    let directive = schema.get_directive("old").expect("old");
    assert_eq!(directive.directives.len(), 2);
    assert_eq!(directive.deprecation_reason.as_deref(), Some("Use @new."));
    let printed = schema.sdl();
    assert!(
        printed.contains("@deprecated(reason: \"Use @new.\") @tag(name: \"meta\")"),
        "{printed}"
    );
    let reparsed = Schema::from_sdl(&printed).expect("reparse");
    assert_eq!(
        reparsed.get_directive("old").expect("old").directives.len(),
        2
    );
}

// ===============================================================================================
// SDL defaults for custom scalars
// ===============================================================================================

#[test]
fn sdl_default_may_exceed_i32_for_a_custom_scalar() {
    let source = r#"
    scalar BigInt
    type Query { f(v: BigInt = 170141183460469231731687303715884105727): String }
    "#;
    let schema = Schema::from_sdl(source).expect("a custom scalar default is not clamped");
    let default = schema.object_type("Query").expect("Query").fields["f"].args["v"]
        .default_value
        .as_ref()
        .expect("default");
    assert_eq!(default, &Value::integer(i128::MAX));
    assert_eq!(
        schema.sdl(),
        Schema::from_sdl(&schema.sdl()).expect("reparse").sdl()
    );
}

#[test]
fn sdl_custom_scalar_default_goes_through_parse_literal() {
    // The scalar's *literal* representation is a string like "DATE(2020-01-01)" and its runtime
    // representation is the same string, but `parse_literal` is the only coercion function the
    // scalar defines (the usual shape for a scalar parsed out of a document).
    let schema = Schema::from_sdl(
        r#"
        scalar Date
        type Query { f(v: Date = "2020-01-01"): String }
        "#,
    )
    .expect("valid schema");

    // There is no way to attach `parse_literal` after `from_sdl`, so build the same declaration
    // by hand and compare how a literal argument and a default are coerced.
    let schema_with_scalar = Schema::builder()
        .scalar(ScalarType::new("Date").parse_literal(|value| match value {
            ast::Value::String(text) => Ok(Value::from(format!("parsed:{text}"))),
            _ => Err("Date expects a string literal".to_owned()),
        }))
        .object(
            ObjectType::new("Query").field(
                Field::new("f", TypeRef::string())
                    .arg(InputValue::new("v", TypeRef::named("Date")))
                    .resolve(|context| {
                        let value = context
                            .args
                            .get("v")
                            .map(|value| value.as_str().unwrap_or("<non-string>").to_owned())
                            .unwrap_or_else(|| "<missing>".to_owned());
                        Box::pin(async move { Ok(Value::from(value)) })
                    }),
            ),
        )
        .query("Query")
        .build()
        .expect("valid schema");
    let literal = run(&schema_with_scalar, r#"{ f(v: "2020-01-01") }"#);
    assert_eq!(data_field(&literal, "f"), &Value::from("parsed:2020-01-01"));

    // `Schema::from_sdl` cannot express the coercion function, so this only records the shape of
    // the stored default.
    let date = schema
        .object_type("Query")
        .expect("Query")
        .fields
        .get("f")
        .expect("f");
    eprintln!("stored default = {:?}", date.args["v"].default_value);
}

/// NOTE — not a defect: a code-first default is a *runtime* value, and graphql-js's
/// `GraphQLArgument.defaultValue` is coerced with `parseValue` too. Recorded so the difference from
/// the SDL path (which stores an already-parsed literal) is explicit.
#[test]
fn code_first_custom_scalar_default_uses_parse_value() {
    // A code-first default stored as a raw JSON-ish value is re-coerced with `parse_value` only.
    let schema = Schema::builder()
        .scalar(ScalarType::new("Date").parse_literal(|value| match value {
            ast::Value::String(text) => Ok(Value::from(format!("parsed:{text}"))),
            _ => Err("Date expects a string literal".to_owned()),
        }))
        .object(
            ObjectType::new("Query").field(
                Field::new("f", TypeRef::string())
                    .arg(InputValue::new("v", TypeRef::named("Date")).default_value("2020-01-01"))
                    .resolve(|context| {
                        let value = context
                            .args
                            .get("v")
                            .map(|value| value.as_str().unwrap_or("<non-string>").to_owned())
                            .unwrap_or_else(|| "<missing>".to_owned());
                        Box::pin(async move { Ok(Value::from(value)) })
                    }),
            ),
        )
        .query("Query")
        .build()
        .expect("valid schema");
    let response = run(&schema, "{ f }");
    eprintln!("{:?}", response.data);
    assert_eq!(data_field(&response, "f"), &Value::from("2020-01-01"));
}

#[test]
fn non_finite_sdl_float_default_is_rejected_at_build_time() {
    let result = Schema::from_sdl("type Query { f(v: Float = 1e999): String }");
    eprintln!("{result:?}");
    assert!(result.is_err(), "a non-finite default is not a valid Float");
}

// ===============================================================================================
// Subscription event semantics
// ===============================================================================================

fn subscription_schema_with(resolve: bool, ty: TypeRef) -> Schema {
    let mut field = Field::new("tick", ty).subscribe(|_| {
        Box::pin(async {
            Ok(futures::stream::iter(vec![
                Ok::<Value, GraphQLError>(value([
                    ("tick", Value::Int(7)),
                    ("label", Value::from("seven")),
                ])),
                Ok(value([
                    ("tick", Value::Int(8)),
                    ("label", Value::from("eight")),
                ])),
            ])
            .boxed())
        })
    });
    if resolve {
        field = field.resolve(|context| {
            Box::pin(async move {
                Ok(context
                    .parent
                    .as_object()
                    .and_then(|object| object.get("tick"))
                    .cloned()
                    .unwrap_or(Value::Null))
            })
        });
    }
    Schema::builder()
        .object(ObjectType::new("Query").field(Field::new("ping", TypeRef::string())))
        .object(ObjectType::new("Subscription").field(field))
        .query("Query")
        .subscription("Subscription")
        .build()
        .expect("valid schema")
}

fn collect_subscription(schema: &Schema, query: &str) -> Vec<Response> {
    let subscription = block_on(gql::subscribe(
        schema,
        Request::new(query),
        &ExecutionOptions::default(),
    ))
    .expect("subscription starts");
    block_on(subscription.stream.collect::<Vec<_>>())
}

#[test]
fn subscription_default_resolver_reads_the_event_root_value() {
    let schema = subscription_schema_with(false, TypeRef::int().non_null());
    let responses = collect_subscription(&schema, "subscription { tick }");
    eprintln!("{responses:?}");
    assert_eq!(responses[0].data, Some(value([("tick", Value::Int(7))])));
    assert_eq!(responses[1].data, Some(value([("tick", Value::Int(8))])));
}

#[test]
fn subscription_resolve_sees_the_event_root_value_with_an_alias() {
    let schema = subscription_schema_with(true, TypeRef::int().non_null());
    let responses = collect_subscription(&schema, "subscription S { total: tick }");
    eprintln!("{responses:?}");
    assert_eq!(responses[0].data, Some(value([("total", Value::Int(7))])));
}

#[test]
fn subscription_event_missing_non_null_field_nulls_the_event() {
    // The event object has no `tick`, so the default resolver yields null and a non-null field
    // must null the whole event payload with one error at path ["tick"].
    let schema = Schema::builder()
        .object(ObjectType::new("Query").field(Field::new("ping", TypeRef::string())))
        .object(ObjectType::new("Subscription").field(
            Field::new("tick", TypeRef::int().non_null()).subscribe(|_| {
                Box::pin(async {
                    Ok(
                        futures::stream::iter(vec![Ok::<Value, GraphQLError>(value([(
                            "other",
                            Value::Int(1),
                        )]))])
                        .boxed(),
                    )
                })
            }),
        ))
        .query("Query")
        .subscription("Subscription")
        .build()
        .expect("valid schema");

    let responses = collect_subscription(&schema, "subscription { tick }");
    eprintln!("{responses:?}");
    assert_eq!(responses[0].data, Some(Value::Null));
    assert_eq!(responses[0].errors.len(), 1, "{:?}", responses[0].errors);
    let path: Vec<_> = responses[0].errors[0]
        .path()
        .expect("error path")
        .iter()
        .map(|segment| segment.to_string())
        .collect();
    assert_eq!(path, ["tick"]);
}

#[test]
fn subscription_source_error_is_shaped_by_root_nullability() {
    for (ty, expect_data) in [
        (TypeRef::int().non_null(), Value::Null),
        (TypeRef::int(), value([("tick", Value::Null)])),
    ] {
        let field = Field::new("tick", ty).subscribe(|_| {
            Box::pin(async {
                Ok(
                    futures::stream::iter(vec![Err::<Value, GraphQLError>(GraphQLError::new(
                        "source exploded",
                    ))])
                    .boxed(),
                )
            })
        });
        let schema = Schema::builder()
            .object(ObjectType::new("Query").field(Field::new("ping", TypeRef::string())))
            .object(ObjectType::new("Subscription").field(field))
            .query("Query")
            .subscription("Subscription")
            .build()
            .expect("valid schema");

        let responses = collect_subscription(&schema, "subscription { tick }");
        eprintln!("{responses:?}");
        assert_eq!(responses[0].data, Some(expect_data));
        assert_eq!(responses[0].errors.len(), 1);
        assert_eq!(
            responses[0].errors[0].path().map(<[_]>::to_vec),
            Some(vec![gql::PathSegment::field("tick")])
        );
    }
}

#[test]
fn subscription_root_field_in_a_fragment_is_one_root_field() {
    let schema =
        Schema::builder()
            .object(ObjectType::new("Query").field(Field::new("ping", TypeRef::string())))
            .object(
                ObjectType::new("Subscription").field(
                    Field::new("tick", TypeRef::int())
                        .subscribe(|_| {
                            Box::pin(async {
                                Ok(futures::stream::iter(vec![Ok::<Value, GraphQLError>(value([
                                    ("tick", Value::Int(1)),
                                ]))])
                                .boxed())
                            })
                        })
                        .resolve(|context| {
                            Box::pin(async move {
                                Ok(context
                                    .parent
                                    .as_object()
                                    .and_then(|object| object.get("tick"))
                                    .cloned()
                                    .unwrap_or(Value::Null))
                            })
                        }),
                ),
            )
            .query("Query")
            .subscription("Subscription")
            .build()
            .expect("valid schema");

    let responses = collect_subscription(
        &schema,
        "subscription { ...F } fragment F on Subscription { tick }",
    );
    assert_eq!(responses[0].data, Some(value([("tick", Value::Int(1))])));
}

#[test]
fn subscription_skipping_the_only_root_field_is_rejected() {
    let schema = Schema::builder()
        .object(ObjectType::new("Query").field(Field::new("ping", TypeRef::string())))
        .object(
            ObjectType::new("Subscription").field(
                Field::new("tick", TypeRef::int())
                    .subscribe(|_| Box::pin(async { Ok(futures::stream::empty().boxed()) })),
            ),
        )
        .query("Query")
        .subscription("Subscription")
        .build()
        .expect("valid schema");

    let result = block_on(gql::subscribe(
        &schema,
        Request::new("subscription { tick @skip(if: true) }"),
        &ExecutionOptions::default(),
    ));
    eprintln!("{:?}", result.as_ref().err());
    let response = result.expect_err("a subscription whose only root field is skipped");
    assert!(
        response
            .errors
            .iter()
            .any(|error| error.message().contains("selected no root fields")),
        "the error should say no root field was selected: {:?}",
        response.errors
    );
}

#[derive(Clone)]
struct CountingAuthorizer {
    calls: Arc<AtomicUsize>,
    seen: Arc<Mutex<Vec<String>>>,
}

impl Authorizer for CountingAuthorizer {
    fn authorize_async<'a>(
        &'a self,
        context: &'a AuthorizationContext<'a>,
    ) -> BoxFuture<'a, Result<(), GraphQLError>> {
        async move {
            self.calls.fetch_add(1, Ordering::SeqCst);
            self.seen
                .lock()
                .expect("log")
                .push(format!("{}.{}", context.parent_type, context.field_name));
            Ok(())
        }
        .boxed()
    }
}

#[test]
fn subscription_authorizer_call_count() {
    let schema = subscription_schema_with(true, TypeRef::int().non_null());
    let authorizer = CountingAuthorizer {
        calls: Arc::default(),
        seen: Arc::default(),
    };
    let subscription = block_on(gql::subscribe(
        &schema,
        Request::new("subscription { tick }"),
        &ExecutionOptions::default().authorizer(authorizer.clone()),
    ))
    .expect("starts");
    let responses = block_on(subscription.stream.collect::<Vec<_>>());
    assert!(responses.iter().all(|response| response.errors.is_empty()));
    assert_eq!(
        authorizer.calls.load(Ordering::SeqCst),
        1,
        "setup authorization is enough for the root field; events must not re-authorize it"
    );
    assert_eq!(
        authorizer.seen.lock().unwrap().as_slice(),
        ["Subscription.tick"]
    );
}

// ===============================================================================================
// Built-in scalar pruning
// ===============================================================================================

#[test]
fn unreferenced_builtins_are_absent_from_introspection_types() {
    let schema = simple_schema();
    let response = run(&schema, "{ __schema { types { name } } }");
    let names: Vec<String> = data_field(&response, "__schema")
        .as_object()
        .and_then(|object| object.get("types"))
        .and_then(Value::as_list)
        .expect("types")
        .iter()
        .filter_map(|ty| ty.as_object().and_then(|object| object.get("name")))
        .filter_map(Value::as_str)
        .map(str::to_owned)
        .collect();
    eprintln!("{names:?}");
    assert!(names.iter().any(|name| name == "String"), "{names:?}");
    assert!(names.iter().any(|name| name == "Boolean"), "{names:?}");
    assert!(!names.iter().any(|name| name == "Int"), "{names:?}");
    assert!(!names.iter().any(|name| name == "Float"), "{names:?}");
    assert!(!names.iter().any(|name| name == "ID"), "{names:?}");
}

#[test]
fn builtin_referenced_only_by_a_directive_argument_is_kept() {
    let schema = Schema::builder()
        .directive(
            DirectiveDefinition::new("cost")
                .arg(InputValue::new("weight", TypeRef::float()))
                .location(DirectiveLocation::FieldDefinition),
        )
        .object(ObjectType::new("Query").field(Field::new("hello", TypeRef::string())))
        .query("Query")
        .build()
        .expect("valid schema");
    assert!(schema.get_type("Float").is_some());
}

#[test]
fn builtin_referenced_only_by_an_input_field_is_kept() {
    let schema = Schema::builder()
        .input_object(
            InputObjectType::new("Filter").field(InputValue::new("weight", TypeRef::float())),
        )
        .object(
            ObjectType::new("Query").field(
                Field::new("f", TypeRef::string())
                    .arg(InputValue::new("filter", TypeRef::named("Filter"))),
            ),
        )
        .query("Query")
        .build()
        .expect("valid schema");
    assert!(schema.get_type("Float").is_some());
}

// ===============================================================================================
// Integer / float literal ranges
// ===============================================================================================

fn int_arg_schema() -> Schema {
    Schema::builder()
        .object(
            ObjectType::new("Query").field(
                Field::new("f", TypeRef::string())
                    .arg(InputValue::new("v", TypeRef::int()))
                    .resolve(|context| {
                        let value = context
                            .args
                            .get("v")
                            .and_then(Value::as_i32)
                            .map(|value| value.to_string())
                            .unwrap_or_else(|| "<none>".to_owned());
                        Box::pin(async move { Ok(Value::from(value)) })
                    }),
            ),
        )
        .query("Query")
        .build()
        .expect("valid schema")
}

#[test]
fn int_literal_boundaries() {
    let schema = int_arg_schema();
    for (literal, ok) in [
        ("2147483647", true),
        ("-2147483648", true),
        ("2147483648", false),
        ("-2147483649", false),
    ] {
        let response = run(&schema, &format!("{{ f(v: {literal}) }}"));
        assert_eq!(
            response.errors.is_empty(),
            ok,
            "literal {literal} handled incorrectly: {:?}",
            response.errors
        );
    }
}

#[test]
fn i128_min_and_beyond() {
    assert!(gql::parser::parse_query("{ f(v: -170141183460469231731687303715884105728) }").is_ok());
    assert!(gql::parser::parse_query("{ f(v: 170141183460469231731687303715884105728) }").is_err());
    assert!(
        gql::parser::parse_query("{ f(v: -170141183460469231731687303715884105729) }").is_err()
    );
}

// ===============================================================================================
// Block strings
// ===============================================================================================

#[test]
fn block_string_character_classes() {
    // Allowed: tab, LF, CR, anything from U+0020 up.
    assert!(gql::parser::parse_query("query { f(a: \"\"\"a\tb\"\"\") }").is_ok());
    assert!(gql::parser::parse_query("query { f(a: \"\"\"a\u{007F}b\"\"\") }").is_ok());
    // Rejected: other C0 controls.
    for control in ['\u{0000}', '\u{0001}', '\u{000B}', '\u{001F}'] {
        let source = format!("query {{ f(a: \"\"\"a{control}b\"\"\") }}");
        let result = gql::parser::parse_query(&source);
        assert!(
            result.is_err(),
            "U+{:04X} accepted in a block string",
            control as u32
        );
    }
}

#[test]
fn single_line_strings_still_reject_controls() {
    for control in ['\u{0000}', '\u{0001}', '\u{000B}', '\u{001F}'] {
        let source = format!("query {{ f(a: \"a{control}b\") }}");
        assert!(
            gql::parser::parse_query(&source).is_err(),
            "U+{:04X} accepted in a string",
            control as u32
        );
    }
}

// ===============================================================================================
// Enum reserved names
// ===============================================================================================

#[test]
fn sdl_enum_reserved_names_are_rejected() {
    for reserved in ["true", "false", "null"] {
        let source = format!("enum Flag {{ {reserved} OK }} type Query {{ f: Flag }}");
        let result = Schema::from_sdl(&source);
        eprintln!("{reserved}: {result:?}");
        assert!(result.is_err(), "SDL accepted enum value {reserved}");
    }
}

// ===============================================================================================
// List-literal variable defaults
// ===============================================================================================

#[test]
fn list_literal_variable_uses_the_enclosing_argument_default() {
    let schema = Schema::builder()
        .object(
            ObjectType::new("Query").field(
                Field::new("f", TypeRef::string())
                    .arg(
                        InputValue::new("xs", TypeRef::int().list().non_null())
                            .default_value(Value::List(vec![Value::Int(0)])),
                    )
                    .resolve(|_| Box::pin(async { Ok(Value::from("ok")) })),
            ),
        )
        .query("Query")
        .build()
        .expect("valid schema");

    let errors = validation_errors(&schema, "query Q($v: Int = 1) { f(xs: [$v]) }");
    eprintln!("{errors:?}");
    assert!(
        errors.is_empty(),
        "a variable nested in a list literal should inherit the argument default: {errors:?}"
    );
}

// ===============================================================================================
// Breadth limit
// ===============================================================================================

#[test]
fn breadth_counts_expanded_fragment_fields() {
    let schema = simple_schema();
    let options = ExecutionOptions::default().max_breadth(2);
    let response = block_on(gql::execute(
        &schema,
        Request::new("{ ...F a: text b: text } fragment F on Query { c: text } "),
        &options,
    ));
    eprintln!("{response:?}");
    assert!(
        response
            .errors
            .iter()
            .any(|error| error.message().contains("breadth")),
        "fragment expansion must count toward breadth: {:?}",
        response.errors
    );
}

#[test]
fn breadth_is_enforced_for_subscriptions() {
    let schema = Schema::builder()
        .object(ObjectType::new("Query").field(Field::new("ping", TypeRef::string())))
        .object(
            ObjectType::new("Subscription").field(
                Field::new("a", TypeRef::named("Payload"))
                    .subscribe(|_| Box::pin(async { Ok(futures::stream::empty().boxed()) })),
            ),
        )
        .object(
            ObjectType::new("Payload")
                .field(Field::new("x", TypeRef::string()))
                .field(Field::new("y", TypeRef::string()))
                .field(Field::new("z", TypeRef::string())),
        )
        .query("Query")
        .subscription("Subscription")
        .build()
        .expect("valid schema");
    let result = block_on(gql::subscribe(
        &schema,
        Request::new("subscription { a { x y z } }"),
        &ExecutionOptions::default().max_breadth(2),
    ));
    let error = result
        .as_ref()
        .err()
        .map(|response| format!("{:?}", response.errors));
    eprintln!("{error:?}");
    assert!(
        error.as_deref().unwrap_or_default().contains("breadth"),
        "expected a breadth rejection, got {error:?}"
    );
}

// ===============================================================================================
// Response shape across mutually exclusive fragments
// ===============================================================================================

#[test]
fn response_shape_conflicts_include_nested_selections() {
    let schema = Schema::from_sdl(
        r#"
        type Dog { info: DogInfo }
        type Cat { info: CatInfo }
        type DogInfo { value: String }
        type CatInfo { value: Int }
        union Pet = Dog | Cat
        type Query { pet: Pet }
        "#,
    )
    .expect("valid schema");
    let errors = validation_errors(
        &schema,
        "{ pet { ... on Dog { info { value } } ... on Cat { info { value } } } }",
    );
    eprintln!("{errors:?}");
    assert!(
        errors
            .iter()
            .any(|error| error.message().contains("conflicting types")),
        "nested response shapes must agree across mutually exclusive fragments: {errors:?}"
    );
}

#[test]
fn identical_shapes_across_mutually_exclusive_fragments_are_allowed() {
    let schema = Schema::from_sdl(
        r#"
        type Dog { name: String }
        type Cat { name: String }
        union Pet = Dog | Cat
        type Query { pet: Pet }
        "#,
    )
    .expect("valid schema");
    assert!(
        validation_errors(
            &schema,
            "{ pet { ... on Dog { x: name } ... on Cat { x: name } } }"
        )
        .is_empty()
    );
}

// ===============================================================================================
// Runtime-null @skip / @include
// ===============================================================================================

#[test]
fn null_include_and_skip_values() {
    let schema = simple_schema();
    let query = "query Q($v: Boolean = true) { a: text @skip(if: $v) b: text @include(if: $v) }";
    let response = run_vars(&schema, query, Object::from_fields([("v", Value::Null)]));
    eprintln!("{:?}", response.data);
    // October 2021: `@skip` removes the field when `if` is true; `@include` removes it when `if`
    // is *not* true. A null `if` is therefore "not true": skipped stays, included is dropped.
    assert_eq!(response.data, Some(value([("a", Value::from("ok"))])));
}

// ===============================================================================================
// Directive self-reference through a type (no hang / no panic)
// ===============================================================================================

#[test]
fn directive_applied_inside_its_own_argument_type_is_fine() {
    let result = Schema::from_sdl(
        r#"
        directive @tag(meta: TagMeta) on INPUT_FIELD_DEFINITION
        input TagMeta { nested: TagMeta, note: String @tag }
        type Query { f: String }
        "#,
    );
    eprintln!("{result:?}");
}

// ===============================================================================================
// Code-first applied directives are not validated
// ===============================================================================================

#[test]
fn code_first_unknown_applied_directive_is_rejected() {
    let result = Schema::builder()
        .object(
            ObjectType::new("Query")
                .directive(AppliedDirective::new("doesNotExist"))
                .field(Field::new("f", TypeRef::string())),
        )
        .query("Query")
        .build();
    eprintln!("{:?}", result.as_ref().err());
    assert!(
        result.is_err(),
        "the builder accepted an applied directive with no definition"
    );
}

#[test]
fn code_first_applied_directive_location_is_validated() {
    let result = Schema::builder()
        .directive(DirectiveDefinition::new("tag").location(DirectiveLocation::Scalar))
        .object(
            ObjectType::new("Query")
                .directive(AppliedDirective::new("tag"))
                .field(Field::new("f", TypeRef::string())),
        )
        .query("Query")
        .build();
    eprintln!("{:?}", result.as_ref().err());
    assert!(
        result.is_err(),
        "the builder accepted @tag on an OBJECT where it is only allowed on SCALAR"
    );
}

// ===============================================================================================
// Misc: enum Eq regression surface
// ===============================================================================================

#[test]
fn enum_value_still_supports_partial_eq() {
    // `EnumValue` was `Eq` before applied directives introduced `Value` (whose `Float` holds an
    // `f64`). It is intentionally `PartialEq` only now; see the changelog note in the report.
    let value = gql::EnumValue {
        name: "RED".into(),
        description: None,
        deprecation_reason: None,
        directives: Vec::new(),
    };
    assert_eq!(value, value.clone());
    fn partial_eq<T: PartialEq>() {}
    partial_eq::<gql::EnumValue>();
}

// ===============================================================================================
// SDL integer constants beyond i32 (affects built-in Float/ID, not just custom scalars)
// ===============================================================================================

#[test]
fn sdl_float_default_above_i32_range_is_accepted() {
    // 10_000_000_000 is an ordinary Float value; an Int literal is a valid Float literal. The
    // SDL default is stored in its literal shape and widened when the argument is coerced.
    let schema = Schema::from_sdl("type Query { f(v: Float = 10000000000): String }")
        .expect("a Float default written as a large integer literal");
    let default = &schema.object_type("Query").expect("Query").fields["f"].args["v"].default_value;
    assert_eq!(default, &Some(Value::integer(10_000_000_000)));
    assert_eq!(
        schema.sdl(),
        Schema::from_sdl(&schema.sdl()).expect("reparse").sdl()
    );
}

#[test]
fn sdl_id_default_above_i32_range_is_accepted() {
    let schema = Schema::from_sdl("type Query { f(v: ID = 10000000000): String }")
        .expect("an ID default written as a large integer literal");
    let default = &schema.object_type("Query").expect("Query").fields["f"].args["v"].default_value;
    assert_eq!(default, &Some(Value::integer(10_000_000_000)));
    assert_eq!(
        schema.sdl(),
        Schema::from_sdl(&schema.sdl()).expect("reparse").sdl()
    );
}

#[test]
fn sdl_directive_argument_float_above_i32_range_is_accepted() {
    let schema = Schema::from_sdl(
        r#"
        directive @cost(weight: Float) on FIELD_DEFINITION
        type Query { f: String @cost(weight: 10000000000) }
        "#,
    )
    .expect("a Float directive argument written as a large integer literal");
    let directive = &schema.object_type("Query").expect("Query").fields["f"].directives[0];
    assert_eq!(
        directive.get_argument("weight"),
        Some(&Value::Float(10_000_000_000.0))
    );
    assert_eq!(
        schema.sdl(),
        Schema::from_sdl(&schema.sdl()).expect("reparse").sdl()
    );
}

#[test]
fn sdl_int_default_outside_i32_is_still_rejected() {
    let result = Schema::from_sdl("type Query { f(v: Int = 10000000000): String }");
    assert!(
        result.is_err(),
        "the built-in Int scalar must reject a value outside 32 bits"
    );
}

#[test]
fn bigint_serialises_as_a_json_number() {
    let value = Value::integer(10_000_000_000);
    let json = serde_json::to_string(&value).expect("serialize BigInt");
    assert_eq!(json, "10000000000");
}

#[test]
fn literal_arguments_accept_bigints_for_float_id_and_custom_scalars() {
    fn echo(name: &'static str, ty: TypeRef) -> Field {
        Field::new(name, ty.clone())
            .arg(InputValue::new("v", ty))
            .resolve(move |context| {
                let value = context.args.get("v").cloned().unwrap_or(Value::Null);
                Box::pin(async move { Ok(value) })
            })
    }

    let schema = Schema::builder()
        .scalar(ScalarType::new("BigInt"))
        .object(
            ObjectType::new("Query")
                .field(echo("float", TypeRef::float()))
                .field(echo("id", TypeRef::id()))
                .field(echo("custom", TypeRef::named("BigInt")))
                .field(echo("int", TypeRef::int())),
        )
        .query("Query")
        .build()
        .expect("valid schema");

    let response = run(
        &schema,
        "{ float(v: 10000000000) id(v: 10000000000) custom(v: 10000000000) }",
    );
    assert!(response.errors.is_empty(), "{:?}", response.errors);
    assert_eq!(
        data_field(&response, "float"),
        &Value::Float(10_000_000_000.0)
    );
    assert_eq!(data_field(&response, "id"), &Value::from("10000000000"));
    assert_eq!(
        data_field(&response, "custom"),
        &Value::integer(10_000_000_000)
    );

    let response = run(&schema, "{ int(v: 10000000000) }");
    assert!(
        response
            .errors
            .iter()
            .any(|error| error.message().contains("32-bit")),
        "the built-in Int scalar must reject the value: {:?}",
        response.errors
    );
}

// ===============================================================================================
// Executable-document directives
// ===============================================================================================

#[test]
fn variable_definition_directives_are_parsed_and_validated() {
    let schema = simple_schema();
    assert!(
        gql::parser::parse_query("query Q($v: Int @foo) { text }").is_ok(),
        "VARIABLE_DEFINITION is a directive location"
    );
    let errors = validation_errors(&schema, "query Q($v: Int @foo) { text }");
    eprintln!("{errors:?}");
    assert!(
        errors
            .iter()
            .any(|error| error.message().contains("Unknown directive")),
        "an unknown variable-definition directive must be reported"
    );
}

#[test]
fn operation_directive_location_is_validated() {
    let schema = simple_schema();
    let errors = validation_errors(&schema, "query @deprecated { text }");
    eprintln!("{errors:?}");
    assert!(
        errors
            .iter()
            .any(|error| error.message().contains("may not be used on QUERY")),
        "@deprecated is not valid on an operation: {errors:?}"
    );
}

// ===============================================================================================
// Introspection agreement for a referenced built-in
// ===============================================================================================

#[test]
fn referenced_builtin_is_described_by_introspection() {
    let schema = Schema::builder()
        .object(
            ObjectType::new("Query")
                .field(Field::new("id", TypeRef::id()))
                .field(Field::new("i", TypeRef::int())),
        )
        .query("Query")
        .build()
        .expect("valid schema");
    let response = run(
        &schema,
        r#"{ id: __type(name: "ID") { name } int: __type(name: "Int") { name } float: __type(name: "Float") { name } }"#,
    );
    assert!(response.errors.is_empty(), "{:?}", response.errors);
    assert_eq!(
        data_field(&response, "id"),
        &value([("name", Value::from("ID"))])
    );
    assert_eq!(
        data_field(&response, "int"),
        &value([("name", Value::from("Int"))])
    );
    assert_eq!(data_field(&response, "float"), &Value::Null);
}

// ===============================================================================================
// Subscription edge cases
// ===============================================================================================

#[test]
fn subscription_excluding_the_only_root_field_is_rejected() {
    let schema = Schema::builder()
        .object(ObjectType::new("Query").field(Field::new("ping", TypeRef::string())))
        .object(
            ObjectType::new("Subscription").field(
                Field::new("tick", TypeRef::int())
                    .subscribe(|_| Box::pin(async { Ok(futures::stream::empty().boxed()) })),
            ),
        )
        .query("Query")
        .subscription("Subscription")
        .build()
        .expect("valid schema");
    let result = block_on(gql::subscribe(
        &schema,
        Request::new("subscription { tick @include(if: false) }"),
        &ExecutionOptions::default(),
    ));
    eprintln!("{:?}", result.as_ref().err());
    assert!(result.is_err());
}

#[test]
fn subscription_nested_error_path() {
    let schema = Schema::builder()
        .object(ObjectType::new("Query").field(Field::new("ping", TypeRef::string())))
        .object(ObjectType::new("Subscription").field(
            Field::new("tick", TypeRef::named("Payload")).subscribe(|_| {
                Box::pin(async {
                    Ok(
                        futures::stream::iter(vec![Ok::<Value, GraphQLError>(value([(
                            "tick",
                            value([("v", Value::Null)]),
                        )]))])
                        .boxed(),
                    )
                })
            }),
        ))
        .object(
            ObjectType::new("Payload")
                .field(Field::new("v", TypeRef::int().non_null()))
                .field(Field::new("other", TypeRef::string())),
        )
        .query("Query")
        .subscription("Subscription")
        .build()
        .expect("valid schema");

    let responses = collect_subscription(&schema, "subscription { tick { v } }");
    eprintln!("{responses:?}");
    assert_eq!(responses[0].data, Some(value([("tick", Value::Null)])));
    let path: Vec<_> = responses[0].errors[0]
        .path()
        .expect("path")
        .iter()
        .map(|segment| segment.to_string())
        .collect();
    assert_eq!(path, ["tick", "v"]);
}

// ===============================================================================================
// Extension merging of repeatable directives
// ===============================================================================================

#[test]
fn repeatable_directives_accumulate_across_extensions_in_order() {
    let schema = Schema::from_sdl(
        r#"
        directive @tag(name: String!) repeatable on OBJECT
        type Query @tag(name: "a") { f: String }
        extend type Query @tag(name: "b")
        extend type Query @tag(name: "c")
        "#,
    )
    .expect("valid schema");
    let directives = &schema.object_type("Query").expect("Query").directives;
    assert_eq!(directives.len(), 3);
    let printed = schema.sdl();
    eprintln!("{printed}");
    assert!(printed.contains(r#"@tag(name: "a") @tag(name: "b") @tag(name: "c")"#));
    assert_eq!(printed, Schema::from_sdl(&printed).expect("reparse").sdl());
}

// ===============================================================================================
// Applied built-ins in the model are mapped to authoritative fields
// ===============================================================================================

#[test]
fn code_first_applied_deprecated_directive_is_not_silently_dropped() {
    let schema = Schema::builder()
        .object(ObjectType::new("Query").field(
            Field::new("f", TypeRef::string()).directive(AppliedDirective::new("deprecated")),
        ))
        .query("Query")
        .build()
        .expect("valid schema");

    let printed = schema.sdl();
    let response = run(
        &schema,
        "{ __type(name: \"Query\") { fields(includeDeprecated: true) { name isDeprecated } } }",
    );
    eprintln!("sdl = {printed}\nintrospection = {:?}", response.data);
    let field = &schema.object_type("Query").expect("Query").fields["f"];
    assert_eq!(
        field.deprecation_reason.as_deref(),
        Some("No longer supported"),
        "the applied @deprecated must populate the authoritative field"
    );
    assert!(printed.contains("@deprecated"), "{printed}");
    let fields = data_field(&response, "__type")
        .as_object()
        .and_then(|object| object.get("fields"))
        .and_then(Value::as_list)
        .expect("fields");
    let field = fields[0].as_object().expect("field");
    assert_eq!(field.get("name"), Some(&Value::from("f")));
    assert_eq!(field.get("isDeprecated"), Some(&Value::Boolean(true)));
}

#[test]
fn code_first_applied_specified_by_directive_is_not_silently_dropped() {
    let schema = Schema::builder()
        .scalar(
            ScalarType::new("Date")
                .directive(AppliedDirective::new("specifiedBy").argument("url", "https://x")),
        )
        .object(ObjectType::new("Query").field(Field::new("d", TypeRef::named("Date"))))
        .query("Query")
        .build()
        .expect("valid schema");
    let printed = schema.sdl();
    eprintln!("{printed}");
    assert_eq!(
        schema
            .scalar_type("Date")
            .expect("Date")
            .specified_by_url
            .as_deref(),
        Some("https://x")
    );
    assert!(
        printed.contains("specifiedBy"),
        "an applied @specifiedBy in the code-first model is dropped from the SDL"
    );
}

#[test]
fn code_first_applied_one_of_directive_sets_the_flag() {
    let schema = Schema::builder()
        .input_object(
            InputObjectType::new("Choice")
                .field(InputValue::new("a", TypeRef::int()))
                .field(InputValue::new("b", TypeRef::string()))
                .directive(AppliedDirective::new("oneOf")),
        )
        .object(ObjectType::new("Query").field(
            Field::new("f", TypeRef::string()).arg(InputValue::new("c", TypeRef::named("Choice"))),
        ))
        .query("Query")
        .build()
        .expect("valid schema");
    assert!(schema.input_object_type("Choice").expect("Choice").one_of);
    assert!(schema.sdl().contains("@oneOf"), "{}", schema.sdl());
}

// ===============================================================================================
// Applied directive arguments are coerced to the declared input type
// ===============================================================================================

#[test]
fn applied_directive_arguments_are_coerced_to_the_declared_type() {
    // When the schema is built, directive arguments follow the same coercion rules as variable
    // values: an integer given to `ID` becomes its decimal string, and an integer given to
    // `Float` widens to a double.
    let schema = Schema::builder()
        .directive(
            DirectiveDefinition::new("tag")
                .arg(InputValue::new("id", TypeRef::id()))
                .arg(InputValue::new("weight", TypeRef::float()))
                .location(DirectiveLocation::FieldDefinition),
        )
        .object(
            ObjectType::new("Query").field(
                Field::new("f", TypeRef::string()).directive(
                    AppliedDirective::new("tag")
                        .argument("id", 4)
                        .argument("weight", 1),
                ),
            ),
        )
        .query("Query")
        .build()
        .expect("valid schema");

    let directive = &schema.object_type("Query").expect("Query").fields["f"].directives[0];
    eprintln!("arguments = {:?}", directive.arguments);
    assert_eq!(directive.get_argument("id"), Some(&Value::from("4")));
    assert_eq!(directive.get_argument("weight"), Some(&Value::Float(1.0)));
}

#[test]
fn custom_scalar_directive_argument_uses_parse_value() {
    let schema = Schema::builder()
        .scalar(ScalarType::new("Upper").parse_value(|value| match value {
            Value::String(value) => Ok(Value::from(value.to_uppercase())),
            _ => Err("Upper expects a string".to_owned()),
        }))
        .directive(
            DirectiveDefinition::new("tag")
                .arg(InputValue::new("value", TypeRef::named("Upper")))
                .location(DirectiveLocation::FieldDefinition),
        )
        .object(
            ObjectType::new("Query").field(
                Field::new("f", TypeRef::string())
                    .directive(AppliedDirective::new("tag").argument("value", "abc")),
            ),
        )
        .query("Query")
        .build()
        .expect("valid schema");
    let directive = &schema.object_type("Query").expect("Query").fields["f"].directives[0];
    assert_eq!(directive.get_argument("value"), Some(&Value::from("ABC")));
}

#[test]
fn sdl_custom_scalar_directive_argument_keeps_the_big_integer() {
    // An SDL-declared scalar has no coercion functions to run, so the exact integer is kept.
    let schema = Schema::from_sdl(
        r#"
        scalar BigInt
        directive @tag(value: BigInt) on FIELD_DEFINITION
        type Query { f: String @tag(value: 10000000000) }
        "#,
    )
    .expect("valid schema");
    let directive = &schema.object_type("Query").expect("Query").fields["f"].directives[0];
    assert_eq!(
        directive.get_argument("value"),
        Some(&Value::integer(10_000_000_000))
    );
    assert_eq!(
        schema.sdl(),
        Schema::from_sdl(&schema.sdl()).expect("reparse").sdl()
    );
}

#[test]
fn sdl_applied_directive_arguments_are_coerced_to_the_declared_type() {
    let schema = Schema::from_sdl(
        r#"
        directive @tag(id: ID, weight: Float) on FIELD_DEFINITION
        type Query { f: String @tag(id: 4, weight: 1) }
        "#,
    )
    .expect("valid schema");
    let directive = &schema.object_type("Query").expect("Query").fields["f"].directives[0];
    eprintln!("arguments = {:?}", directive.arguments);
    assert_eq!(directive.get_argument("id"), Some(&Value::from("4")));
    assert_eq!(directive.get_argument("weight"), Some(&Value::Float(1.0)));
}

// ===============================================================================================
// Non-null directive argument omissions on extensions
// ===============================================================================================

#[test]
fn extension_cannot_add_a_field_without_a_required_directive_argument() {
    let result = Schema::from_sdl(
        r#"
        directive @tag(name: String!) on FIELD_DEFINITION
        type Query { f: String }
        extend type Query { g: String @tag }
        "#,
    );
    eprintln!("{:?}", result.as_ref().err());
    assert!(
        result.is_err(),
        "a missing required directive argument must be rejected"
    );
}
