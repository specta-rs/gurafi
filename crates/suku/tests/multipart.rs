//! GraphQL multipart request tests.
//!
//! These exercise `suku::http::handle` directly with a hand-built multipart body, so no socket or
//! HTTP client is involved.

#![cfg(feature = "multipart")]

mod support;

use ::http::{Method, StatusCode, header};
use gql::{Field, InputObjectType, InputValue, ObjectType, ScalarType, Schema, TypeRef, Value};
use serde_json::json;
use suku::{
    Context, HttpConfig, HttpError, HttpResponse, HttpResult, Server, Upload,
    http::{ContextRequest, handle},
};

const BOUNDARY: &str = "----sukutestboundary";

fn schema() -> Schema {
    let upload = Field::new("upload", TypeRef::string().non_null())
        .arg(InputValue::new("file", TypeRef::named("Upload").non_null()))
        .resolve(|context| {
            let file = context
                .args
                .get("file")
                .and_then(Value::as_external::<Upload>)
                .map(describe);
            Box::pin(async move {
                file.ok_or_else(|| gql::GraphQLError::new("resolver did not receive an Upload"))
                    .map(Value::from)
            })
        });

    let upload_many = Field::new("uploadMany", TypeRef::string().non_null())
        .arg(InputValue::new(
            "files",
            TypeRef::named("Upload").non_null().list().non_null(),
        ))
        .resolve(|context| {
            let files = context
                .args
                .get("files")
                .and_then(Value::as_list)
                .map(|files| {
                    files
                        .iter()
                        .filter_map(Value::as_external::<Upload>)
                        .map(describe)
                        .collect::<Vec<_>>()
                        .join("|")
                });
            Box::pin(async move {
                files
                    .ok_or_else(|| gql::GraphQLError::new("resolver did not receive Upload values"))
                    .map(Value::from)
            })
        });

    let upload_nested = Field::new("uploadNested", TypeRef::string().non_null())
        .arg(InputValue::new(
            "input",
            TypeRef::named("FileInput").non_null(),
        ))
        .resolve(|context| {
            let file = context
                .args
                .get("input")
                .and_then(Value::as_object)
                .and_then(|input| input.get("file"))
                .and_then(Value::as_external::<Upload>)
                .map(describe);
            Box::pin(async move {
                file.ok_or_else(|| gql::GraphQLError::new("resolver did not receive an Upload"))
                    .map(Value::from)
            })
        });

    let query = ObjectType::new("Query").field(
        Field::new("hello", TypeRef::string().non_null())
            .resolve(|_| Box::pin(async { Ok(Value::from("world")) })),
    );
    let mutation = ObjectType::new("Mutation")
        .field(upload)
        .field(upload_many)
        .field(upload_nested);

    let mut builder = Schema::builder();
    builder.add_type(ScalarType::new("Upload").into());
    builder.add_type(
        InputObjectType::new("FileInput")
            .field(InputValue::new("file", TypeRef::named("Upload").non_null()))
            .into(),
    );
    builder.add_type(query.into());
    builder.add_type(mutation.into());
    builder = builder.query("Query").mutation("Mutation");
    builder.build().expect("valid schema")
}

fn describe(upload: &Upload) -> String {
    format!(
        "{}:{}:{}",
        upload.filename().unwrap_or("<none>"),
        upload.content_type().unwrap_or("<none>"),
        String::from_utf8_lossy(upload.bytes())
    )
}

/// Build a complete multipart body from text fields followed by file parts.
fn multipart(
    operations: &str,
    map: &str,
    files: &[(&str, &str, &str, &[u8])],
) -> (String, Vec<u8>) {
    let mut body = Vec::new();
    push_field(&mut body, "operations", None, None, operations.as_bytes());
    push_field(&mut body, "map", None, None, map.as_bytes());
    for (name, filename, content_type, bytes) in files {
        push_field(&mut body, name, Some(filename), Some(content_type), bytes);
    }
    body.extend_from_slice(format!("--{BOUNDARY}--\r\n").as_bytes());
    (format!("multipart/form-data; boundary={BOUNDARY}"), body)
}

fn push_field(
    body: &mut Vec<u8>,
    name: &str,
    filename: Option<&str>,
    content_type: Option<&str>,
    value: &[u8],
) {
    body.extend_from_slice(format!("--{BOUNDARY}\r\n").as_bytes());
    match filename {
        Some(filename) => body.extend_from_slice(
            format!("Content-Disposition: form-data; name=\"{name}\"; filename=\"{filename}\"\r\n")
                .as_bytes(),
        ),
        None => body.extend_from_slice(
            format!("Content-Disposition: form-data; name=\"{name}\"\r\n").as_bytes(),
        ),
    }
    if let Some(content_type) = content_type {
        body.extend_from_slice(format!("Content-Type: {content_type}\r\n").as_bytes());
    }
    body.extend_from_slice(b"\r\n");
    body.extend_from_slice(value);
    body.extend_from_slice(b"\r\n");
}

fn buffered(result: HttpResult) -> HttpResponse {
    match result {
        HttpResult::Response(response) => response,
        HttpResult::Stream(_) => panic!("expected a buffered response"),
    }
}

async fn post(
    server: &Server,
    config: &HttpConfig,
    content_type: &str,
    body: &[u8],
) -> HttpResponse {
    let parts = support::parts(
        Method::POST,
        "/graphql",
        &[
            (header::CONTENT_TYPE, content_type),
            (header::ACCEPT, "application/json"),
        ],
    );
    buffered(handle(server, config, &parts, body, support::empty_context).await)
}

fn body_json(response: &HttpResponse) -> serde_json::Value {
    serde_json::from_slice(&response.body).expect("JSON body")
}

#[tokio::test]
async fn single_file_reaches_the_resolver() {
    let server = Server::new(schema());
    let (content_type, body) = multipart(
        r#"{"query":"mutation($file: Upload!){ upload(file:$file) }","variables":{"file":null}}"#,
        r#"{"0":["variables.file"]}"#,
        &[("0", "a.txt", "text/plain", b"Alpha file content.")],
    );

    let response = post(&server, &HttpConfig::default(), &content_type, &body).await;
    assert_eq!(response.status, StatusCode::OK);
    assert_eq!(
        body_json(&response)["data"]["upload"],
        "a.txt:text/plain:Alpha file content."
    );
}

#[tokio::test]
async fn multiple_files_and_list_variables_are_supported() {
    let server = Server::new(schema());
    let (content_type, body) = multipart(
        r#"{"query":"mutation($files: [Upload!]!){ uploadMany(files:$files) }","variables":{"files":[null,null]}}"#,
        r#"{"0":["variables.files.0"],"1":["variables.files.1"]}"#,
        &[
            ("0", "b.txt", "text/plain", b"Bravo"),
            ("1", "c.txt", "text/csv", b"Charlie"),
        ],
    );

    let response = post(&server, &HttpConfig::default(), &content_type, &body).await;
    assert_eq!(response.status, StatusCode::OK);
    assert_eq!(
        body_json(&response)["data"]["uploadMany"],
        "b.txt:text/plain:Bravo|c.txt:text/csv:Charlie"
    );
}

#[tokio::test]
async fn nested_variables_are_supported() {
    let server = Server::new(schema());
    let (content_type, body) = multipart(
        r#"{"query":"mutation($input: FileInput!){ uploadNested(input:$input) }","variables":{"input":{"file":null}}}"#,
        r#"{"0":["variables.input.file"]}"#,
        &[("0", "nested.txt", "text/plain", b"nested")],
    );

    let response = post(&server, &HttpConfig::default(), &content_type, &body).await;
    assert_eq!(response.status, StatusCode::OK);
    assert_eq!(
        body_json(&response)["data"]["uploadNested"],
        "nested.txt:text/plain:nested"
    );
}

#[tokio::test]
async fn batch_operations_with_files_are_supported() {
    let server = Server::new(schema());
    let (content_type, body) = multipart(
        r#"[{"query":"mutation($file: Upload!){ upload(file:$file) }","variables":{"file":null}},{"query":"mutation($file: Upload!){ upload(file:$file) }","variables":{"file":null}}]"#,
        r#"{"0":["0.variables.file"],"1":["1.variables.file"]}"#,
        &[
            ("0", "first.txt", "text/plain", b"first"),
            ("1", "second.txt", "text/plain", b"second"),
        ],
    );

    let response = post(&server, &HttpConfig::default(), &content_type, &body).await;
    assert_eq!(response.status, StatusCode::OK);
    let body = body_json(&response);
    assert_eq!(body[0]["data"]["upload"], "first.txt:text/plain:first");
    assert_eq!(body[1]["data"]["upload"], "second.txt:text/plain:second");
}

#[tokio::test]
async fn one_file_may_be_mapped_to_several_paths() {
    let server = Server::new(schema());
    let (content_type, body) = multipart(
        r#"{"query":"mutation($a: Upload!, $b: Upload!){ one: upload(file:$a) two: upload(file:$b) }","variables":{"a":null,"b":null}}"#,
        r#"{"0":["variables.a","variables.b"]}"#,
        &[("0", "shared.txt", "text/plain", b"shared")],
    );

    let response = post(&server, &HttpConfig::default(), &content_type, &body).await;
    assert_eq!(response.status, StatusCode::OK);
    let body = body_json(&response);
    assert_eq!(body["data"]["one"], "shared.txt:text/plain:shared");
    assert_eq!(body["data"]["two"], "shared.txt:text/plain:shared");
}

#[tokio::test]
async fn malformed_map_is_rejected() {
    let server = Server::new(schema());
    let (content_type, body) =
        multipart(r#"{"query":"{ hello }","variables":{}}"#, "{not json", &[]);

    let response = post(&server, &HttpConfig::default(), &content_type, &body).await;
    assert_eq!(response.status, StatusCode::BAD_REQUEST);
    assert!(
        body_json(&response)["errors"][0]["message"]
            .as_str()
            .unwrap_or_default()
            .contains("map")
    );
}

#[tokio::test]
async fn missing_file_is_rejected() {
    let server = Server::new(schema());
    let (content_type, body) = multipart(
        r#"{"query":"mutation($file: Upload!){ upload(file:$file) }","variables":{"file":null}}"#,
        r#"{"0":["variables.file"]}"#,
        &[],
    );

    let response = post(&server, &HttpConfig::default(), &content_type, &body).await;
    assert_eq!(response.status, StatusCode::BAD_REQUEST);
    assert!(
        body_json(&response)["errors"][0]["message"]
            .as_str()
            .unwrap_or_default()
            .contains("no file")
    );
}

#[tokio::test]
async fn file_not_listed_in_map_is_rejected() {
    let server = Server::new(schema());
    let (content_type, body) = multipart(
        r#"{"query":"{ hello }","variables":{}}"#,
        r#"{}"#,
        &[("0", "extra.txt", "text/plain", b"extra")],
    );

    let response = post(&server, &HttpConfig::default(), &content_type, &body).await;
    assert_eq!(response.status, StatusCode::BAD_REQUEST);
    assert!(
        body_json(&response)["errors"][0]["message"]
            .as_str()
            .unwrap_or_default()
            .contains("not listed")
    );
}

#[tokio::test]
async fn paths_outside_variables_are_rejected() {
    let server = Server::new(schema());
    let (content_type, body) = multipart(
        r#"{"query":"{ hello }","variables":{}}"#,
        r#"{"0":["query"]}"#,
        &[("0", "a.txt", "text/plain", b"a")],
    );

    let response = post(&server, &HttpConfig::default(), &content_type, &body).await;
    assert_eq!(response.status, StatusCode::BAD_REQUEST);
    assert!(
        body_json(&response)["errors"][0]["message"]
            .as_str()
            .unwrap_or_default()
            .contains("variables")
    );
}

#[tokio::test]
async fn non_null_destinations_are_rejected() {
    let server = Server::new(schema());
    let (content_type, body) = multipart(
        r#"{"query":"mutation($file: Upload!){ upload(file:$file) }","variables":{"file":"already"}}"#,
        r#"{"0":["variables.file"]}"#,
        &[("0", "a.txt", "text/plain", b"a")],
    );

    let response = post(&server, &HttpConfig::default(), &content_type, &body).await;
    assert_eq!(response.status, StatusCode::BAD_REQUEST);
    assert!(
        body_json(&response)["errors"][0]["message"]
            .as_str()
            .unwrap_or_default()
            .contains("null")
    );
}

#[tokio::test]
async fn duplicate_map_paths_are_rejected() {
    let server = Server::new(schema());
    let (content_type, body) = multipart(
        r#"{"query":"mutation($file: Upload!){ upload(file:$file) }","variables":{"file":null}}"#,
        r#"{"0":["variables.file"],"1":["variables.file"]}"#,
        &[
            ("0", "a.txt", "text/plain", b"a"),
            ("1", "b.txt", "text/plain", b"b"),
        ],
    );

    let response = post(&server, &HttpConfig::default(), &content_type, &body).await;
    assert_eq!(response.status, StatusCode::BAD_REQUEST);
    assert!(
        body_json(&response)["errors"][0]["message"]
            .as_str()
            .unwrap_or_default()
            .contains("more than once")
    );
}

#[tokio::test]
async fn too_many_parts_is_rejected() {
    let server = Server::new(schema());
    let config = HttpConfig::default().max_multipart_parts(2);
    let (content_type, body) = multipart(
        r#"{"query":"mutation($file: Upload!){ upload(file:$file) }","variables":{"file":null}}"#,
        r#"{"0":["variables.file"]}"#,
        &[("0", "a.txt", "text/plain", b"a")],
    );

    let response = post(&server, &config, &content_type, &body).await;
    assert_eq!(response.status, StatusCode::BAD_REQUEST);
    assert!(
        body_json(&response)["errors"][0]["message"]
            .as_str()
            .unwrap_or_default()
            .contains("at most 2")
    );
}

#[tokio::test]
async fn oversized_bodies_are_rejected_before_parsing() {
    let server = Server::new(schema()).max_body_bytes(32);
    let (content_type, body) = multipart(
        r#"{"query":"mutation($file: Upload!){ upload(file:$file) }","variables":{"file":null}}"#,
        r#"{"0":["variables.file"]}"#,
        &[("0", "a.txt", "text/plain", b"a")],
    );

    let response = post(&server, &HttpConfig::default(), &content_type, &body).await;
    assert_eq!(response.status, StatusCode::PAYLOAD_TOO_LARGE);
}

#[tokio::test]
async fn authentication_runs_before_the_body_is_parsed() {
    let server = Server::new(schema());
    let (content_type, body) = multipart(
        r#"{"query":"mutation($file: Upload!){ upload(file:$file) }","variables":{"file":null}}"#,
        r#"{"0":["variables.file"]}"#,
        &[("0", "a.txt", "text/plain", b"a")],
    );
    let parts = support::parts(
        Method::POST,
        "/graphql",
        &[
            (header::CONTENT_TYPE, content_type.as_str()),
            (header::ACCEPT, "application/json"),
        ],
    );

    let response = buffered(
        handle(
            &server,
            &HttpConfig::default(),
            &parts,
            &body,
            |_: ContextRequest| async {
                Err(HttpError::message(
                    StatusCode::UNAUTHORIZED,
                    "Missing bearer token",
                ))
            },
        )
        .await,
    );

    assert_eq!(response.status, StatusCode::UNAUTHORIZED);
    assert_eq!(
        body_json(&response)["errors"][0]["message"],
        "Missing bearer token"
    );
}

#[tokio::test]
async fn json_posts_are_unchanged() {
    let server = Server::new(schema());
    let response = post(
        &server,
        &HttpConfig::default(),
        "application/json",
        br#"{"query":"{ hello }"}"#,
    )
    .await;
    assert_eq!(response.status, StatusCode::OK);
    assert_eq!(body_json(&response)["data"]["hello"], "world");
}

#[tokio::test]
async fn uploads_can_be_attached_without_http() {
    use suku::GraphQLRequest;

    let server = Server::new(schema());
    let variables = json!({ "file": null })
        .as_object()
        .cloned()
        .expect("object");
    let request = GraphQLRequest::new("mutation($file: Upload!){ upload(file:$file) }")
        .variables(variables)
        .with_upload(
            "variables.file",
            Upload::new("direct.txt", "text/plain", b"direct".to_vec()),
        );

    let response = server.execute(&request, Context::empty()).await;
    assert_eq!(
        suku::to_json(&response)["data"]["upload"],
        json!("direct.txt:text/plain:direct")
    );
}
