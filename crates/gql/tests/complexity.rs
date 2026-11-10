use gql::{
    Field, InputValue, InterfaceType, Object, ObjectType, Schema, TypeRef, UnionType, Value,
    complexity::{Complexity, ComplexityOptions},
};

fn schema() -> Schema {
    let mut builder = Schema::builder();

    builder.add_type(
        InterfaceType::new("Node")
            .field(Field::new("id", TypeRef::id().non_null()))
            .into(),
    );

    builder.add_type(
        ObjectType::new("User")
            .implements("Node")
            .field(Field::new("id", TypeRef::id().non_null()))
            .field(Field::new("name", TypeRef::string()))
            .field(
                Field::new(
                    "friends",
                    TypeRef::named("User").non_null().list().non_null(),
                )
                .arg(InputValue::new("first", TypeRef::int()))
                .complexity(|args| {
                    2 + args
                        .get("first")
                        .and_then(Value::as_i32)
                        .unwrap_or(0)
                        .max(0) as u32
                }),
            )
            .into(),
    );

    builder.add_type(
        ObjectType::new("Post")
            .implements("Node")
            .field(Field::new("id", TypeRef::id().non_null()))
            .field(Field::new("title", TypeRef::string()))
            .into(),
    );

    builder.add_type(
        UnionType::new("SearchResult")
            .member("User")
            .member("Post")
            .into(),
    );

    builder.add_type(
        ObjectType::new("Query")
            .field(Field::new("ping", TypeRef::string()).complexity(|_| 5))
            .field(
                Field::new("user", TypeRef::named("User"))
                    .arg(InputValue::new("id", TypeRef::id().non_null())),
            )
            .field(
                Field::new("users", TypeRef::named("User").non_null().list().non_null())
                    .arg(InputValue::new("first", TypeRef::int())),
            )
            .field(Field::new(
                "search",
                TypeRef::named("SearchResult").non_null().list().non_null(),
            ))
            .field(Field::new(
                "matrix",
                TypeRef::int()
                    .non_null()
                    .list()
                    .non_null()
                    .list()
                    .non_null(),
            ))
            .into(),
    );

    builder.query("Query").build().expect("valid schema")
}

fn analyze(
    query: &str,
    operation: Option<&str>,
    variables: Object,
    options: ComplexityOptions,
) -> Result<Complexity, gql::GraphQLError> {
    let schema = schema();
    let document = gql::parser::parse_query(query).expect("valid query");
    gql::complexity::analyze(&schema, &document, operation, &variables, &options)
}

fn plain(query: &str) -> Complexity {
    analyze(query, None, Object::new(), ComplexityOptions::default()).expect("analysis succeeds")
}

#[test]
fn uses_field_estimators() {
    assert_eq!(
        plain("{ ping }"),
        Complexity {
            cost: 5,
            depth: 1,
            breadth: 1
        }
    );
}

#[test]
fn estimators_receive_coerced_arguments() {
    // `friends` costs 2 + first; the argument is coerced before the estimator is called.
    assert_eq!(
        plain("{ users { friends(first: 5) { name } } }"),
        Complexity {
            cost: 171,
            depth: 3,
            breadth: 1
        }
    );
}

#[test]
fn multiplies_list_fields() {
    assert_eq!(
        plain("{ users { name } }"),
        Complexity {
            cost: 11,
            depth: 2,
            breadth: 1
        }
    );
}

#[test]
fn multiplies_every_list_nesting_level() {
    assert_eq!(
        plain("{ matrix }"),
        Complexity {
            cost: 1,
            depth: 1,
            breadth: 1
        },
        "a leaf list field only costs its base cost"
    );
    assert_eq!(
        plain("{ users { friends { name } } }"),
        Complexity {
            cost: 121,
            depth: 3,
            breadth: 1
        }
    );
}

#[test]
fn merges_duplicate_response_keys() {
    assert_eq!(
        plain("{ ping ping }"),
        Complexity {
            cost: 5,
            depth: 1,
            breadth: 1
        }
    );
    assert_eq!(
        plain("{ a: ping a: ping }"),
        Complexity {
            cost: 5,
            depth: 1,
            breadth: 1
        }
    );
    assert_eq!(
        plain("{ a: ping b: ping }"),
        Complexity {
            cost: 10,
            depth: 1,
            breadth: 2
        }
    );
}

#[test]
fn expands_fragments_without_adding_depth() {
    let fragment = "query { user(id: \"1\") { ...F } } fragment F on User { name }";
    assert_eq!(
        plain(fragment),
        Complexity {
            cost: 2,
            depth: 2,
            breadth: 1
        }
    );
}

#[test]
fn survives_fragment_cycles() {
    let cycle = "{ ...A } fragment A on Query { ping ...B } fragment B on Query { ...A }";
    assert_eq!(
        plain(cycle),
        Complexity {
            cost: 5,
            depth: 1,
            breadth: 1
        }
    );
}

#[test]
fn applies_type_conditions() {
    let union = "{ search { ... on User { name } ... on Post { title } } }";
    assert_eq!(
        plain(union),
        Complexity {
            cost: 21,
            depth: 2,
            breadth: 2
        }
    );

    // `User` and `Post` never overlap, so this fragment cannot contribute a field.
    let impossible = "{ user(id: \"1\") { ... on Post { title } } }";
    assert_eq!(
        plain(impossible),
        Complexity {
            cost: 1,
            depth: 2,
            breadth: 1
        }
    );
}

#[test]
fn honours_skip_and_include_literals() {
    assert_eq!(
        plain("{ ping @skip(if: true) users { name } }"),
        Complexity {
            cost: 11,
            depth: 2,
            breadth: 1
        }
    );
    assert_eq!(
        plain("{ ping @include(if: false) }"),
        Complexity {
            cost: 0,
            depth: 1,
            breadth: 0
        }
    );
    assert_eq!(
        plain("{ ping @include(if: true) }"),
        Complexity {
            cost: 5,
            depth: 1,
            breadth: 1
        }
    );
}

#[test]
fn honours_skip_and_include_variables() {
    let options = ComplexityOptions::default();
    let skipped = Object::from_fields([("v", Value::from(true))]);
    assert_eq!(
        analyze(
            "query Q($v: Boolean!) { ping @skip(if: $v) }",
            None,
            skipped,
            options
        )
        .unwrap(),
        Complexity {
            cost: 0,
            depth: 1,
            breadth: 0
        }
    );

    let included = Object::from_fields([("v", Value::from(false))]);
    assert_eq!(
        analyze(
            "query Q($v: Boolean!) { ping @skip(if: $v) }",
            None,
            included,
            options
        )
        .unwrap(),
        Complexity {
            cost: 5,
            depth: 1,
            breadth: 1
        }
    );

    // A missing variable is treated as `false`.
    assert_eq!(
        analyze(
            "query Q($v: Boolean!) { ping @skip(if: $v) }",
            None,
            Object::new(),
            options
        )
        .unwrap(),
        Complexity {
            cost: 5,
            depth: 1,
            breadth: 1
        }
    );
    assert_eq!(
        analyze(
            "query Q($v: Boolean!) { ping @include(if: $v) }",
            None,
            Object::new(),
            options
        )
        .unwrap(),
        Complexity {
            cost: 0,
            depth: 1,
            breadth: 0
        }
    );
}

#[test]
fn enforces_depth_limits() {
    let options = ComplexityOptions {
        max_depth: 2,
        ..ComplexityOptions::default()
    };
    let error = analyze(
        "{ users { friends { name } } }",
        None,
        Object::new(),
        options,
    )
    .unwrap_err();
    assert!(error.message().contains('3'), "{}", error.message());
    assert!(error.message().contains('2'), "{}", error.message());
}

#[test]
fn enforces_complexity_limits() {
    let options = ComplexityOptions {
        max_complexity: 4,
        ..ComplexityOptions::default()
    };
    let error = analyze("{ ping }", None, Object::new(), options).unwrap_err();
    assert!(error.message().contains('5'), "{}", error.message());
    assert!(error.message().contains('4'), "{}", error.message());
}

#[test]
fn enforces_breadth_limits() {
    let options = ComplexityOptions {
        max_breadth: 2,
        ..ComplexityOptions::default()
    };
    // Three distinct response keys in one selection set exceed a breadth of two.
    let error = analyze("{ a: ping b: ping c: ping }", None, Object::new(), options).unwrap_err();
    assert!(error.message().contains("breadth 3"), "{}", error.message());
    assert!(
        error.message().contains("maximum allowed breadth of 2"),
        "{}",
        error.message()
    );

    // Duplicate response keys merge and do not count towards breadth.
    assert_eq!(
        analyze("{ a: ping a: ping b: ping }", None, Object::new(), options)
            .unwrap()
            .breadth,
        2
    );

    // Breadth is the widest selection set, not just the root.
    assert_eq!(
        analyze(
            "{ users { name friends { name } } }",
            None,
            Object::new(),
            options
        )
        .unwrap()
        .breadth,
        2
    );
}

#[test]
fn rejects_unknown_and_ambiguous_operations() {
    let options = ComplexityOptions::default();

    let none = analyze("fragment F on Query { ping }", None, Object::new(), options).unwrap_err();
    assert!(none.message().contains("Must provide an operation"));

    let ambiguous = analyze(
        "query A { ping } query B { ping }",
        None,
        Object::new(),
        options,
    )
    .unwrap_err();
    assert!(ambiguous.message().contains("multiple operations"));

    let unknown = analyze(
        "query A { ping } query B { ping }",
        Some("C"),
        Object::new(),
        options,
    )
    .unwrap_err();
    assert!(unknown.message().contains("Unknown operation named \"C\""));

    let selected = analyze(
        "query A { ping } query B { users { name } }",
        Some("B"),
        Object::new(),
        options,
    )
    .unwrap();
    assert_eq!(
        selected,
        Complexity {
            cost: 11,
            depth: 2,
            breadth: 1
        }
    );
}

#[test]
fn rejects_operations_the_schema_cannot_run() {
    let error = analyze(
        "mutation { ping }",
        None,
        Object::new(),
        ComplexityOptions::default(),
    )
    .unwrap_err();
    assert!(error.message().contains("mutations"), "{}", error.message());
}

#[test]
fn saturates_instead_of_overflowing() {
    let options = ComplexityOptions {
        list_factor: u32::MAX,
        ..ComplexityOptions::default()
    };
    let complexity = analyze(
        "{ users { friends { name } } }",
        None,
        Object::new(),
        options,
    )
    .unwrap();
    assert_eq!(complexity.cost, u32::MAX);
}
