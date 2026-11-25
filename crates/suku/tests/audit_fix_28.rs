//! Regression tests for multipart duplicate-path validation.

#![cfg(feature = "multipart")]

use std::fmt::Write as _;

use suku::{DEFAULT_MAX_MULTIPART_PARTS, multipart};

const BOUNDARY: &str = "----sukuauditfix28";

fn multipart_body(operations: &str, map: &str, files: &[(&str, &[u8])]) -> (String, Vec<u8>) {
    let mut body = Vec::new();
    push_field(&mut body, "operations", None, operations.as_bytes());
    push_field(&mut body, "map", None, map.as_bytes());
    for (name, bytes) in files {
        push_field(&mut body, name, Some("a.txt"), bytes);
    }
    body.extend_from_slice(format!("--{BOUNDARY}--\r\n").as_bytes());
    (format!("multipart/form-data; boundary={BOUNDARY}"), body)
}

fn push_field(body: &mut Vec<u8>, name: &str, filename: Option<&str>, value: &[u8]) {
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
    body.extend_from_slice(b"Content-Type: text/plain\r\n\r\n");
    body.extend_from_slice(value);
    body.extend_from_slice(b"\r\n");
}

#[tokio::test]
async fn rejects_duplicate_paths_within_one_map_entry() {
    let (content_type, body) = multipart_body(
        r#"{"query":"{ hello }","variables":{"file":null}}"#,
        r#"{"0":["variables.file","variables.file"]}"#,
        &[("0", b"a")],
    );

    let error = multipart::parse(&content_type, &body, DEFAULT_MAX_MULTIPART_PARTS)
        .await
        .expect_err("duplicate paths in one map entry must be rejected");
    assert!(
        error.message().contains("more than once"),
        "unexpected error: {error}"
    );
}

#[tokio::test]
async fn rejects_duplicate_paths_across_map_entries() {
    let (content_type, body) = multipart_body(
        r#"{"query":"{ hello }","variables":{"file":null}}"#,
        r#"{"0":["variables.file"],"1":["variables.file"]}"#,
        &[("0", b"a"), ("1", b"b")],
    );

    let error = multipart::parse(&content_type, &body, DEFAULT_MAX_MULTIPART_PARTS)
        .await
        .expect_err("duplicate paths across map entries must be rejected");
    assert!(
        error.message().contains("more than once"),
        "unexpected error: {error}"
    );
}

#[tokio::test]
async fn accepts_many_unique_paths() {
    const PATH_COUNT: usize = 24_000;

    let mut operations = String::from(r#"{"query":"{ hello }","variables":{"files":["#);
    for index in 0..PATH_COUNT {
        if index != 0 {
            operations.push(',');
        }
        operations.push_str("null");
    }
    operations.push_str("]}}");

    let mut map = String::from("{\"0\":[");
    for index in 0..PATH_COUNT {
        if index != 0 {
            map.push(',');
        }
        write!(&mut map, "\"variables.files.{index}\"").expect("write map path");
    }
    map.push_str("]}");

    let (content_type, body) = multipart_body(&operations, &map, &[("0", b"shared upload")]);
    let request = multipart::parse(&content_type, &body, DEFAULT_MAX_MULTIPART_PARTS)
        .await
        .expect("a map with many unique paths must be accepted");

    assert_eq!(request.uploads.len(), PATH_COUNT);
    assert_eq!(request.uploads[0].path, "variables.files.0");
    assert_eq!(
        request.uploads[PATH_COUNT - 1].path,
        format!("variables.files.{}", PATH_COUNT - 1)
    );
}
