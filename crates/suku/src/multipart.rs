//! GraphQL multipart requests (file uploads).
//!
//! This module implements the [GraphQL multipart request specification][spec]: a `POST` with
//! `Content-Type: multipart/form-data` whose fields are, in order:
//!
//! 1. `operations` — the JSON request object (or array for a batch) with each file replaced by
//!    `null`;
//! 2. `map` — a JSON object mapping each file part's field name to the operations paths that
//!    should receive it;
//! 3. one file part per entry in `map`, named by that entry's key.
//!
//! The files are inserted into the request's variables as [`gql::Upload`] values. Nothing else in
//! the request is modified: a path must be rooted at `variables`, must already exist, and must
//! hold `null`. Only the paths explicitly listed in `map` are written.
//!
//! The whole body is buffered by the HTTP transport, so `suku` supports the field structure of the
//! specification but not streaming a file directly from the socket into a resolver. The body
//! limit bounds memory use; see [`crate::HttpConfig`].
//!
//! [spec]: https://github.com/jaydenseric/graphql-multipart-request-spec

use std::{collections::BTreeMap, sync::Arc};

use bytes::Bytes;
use futures::stream;
use gql::Upload;
use multer::Multipart;
use serde_json::Value as JsonValue;

use crate::request::{GraphQLRequest, RequestError, UploadBinding};

/// The default maximum number of multipart fields, including `operations` and `map`.
pub const DEFAULT_MAX_MULTIPART_PARTS: usize = 32;

/// Parse a multipart request body.
///
/// `content_type` is the full request `Content-Type`, including its `boundary` parameter.
/// `max_parts` limits the number of fields; `0` disables the limit. The specification does not
/// define batching, so `operations` must be a single request object.
///
/// # Errors
///
/// Returns a request error for an invalid content type or body, missing or duplicate fields,
/// unknown file fields, a malformed `map`, a path that does not point at `null` inside
/// `variables`, a missing file, or an `operations` value that is not an object.
pub async fn parse(
    content_type: &str,
    body: &[u8],
    max_parts: usize,
) -> Result<GraphQLRequest, RequestError> {
    parse_bytes(content_type, Bytes::copy_from_slice(body), max_parts).await
}

/// [`parse`] for an owned body.
///
/// The Axum adapter already holds the body as [`Bytes`]; taking it here avoids copying the whole
/// request a second time just to hand it to the multipart parser.
pub(crate) async fn parse_bytes(
    content_type: &str,
    body: Bytes,
    max_parts: usize,
) -> Result<GraphQLRequest, RequestError> {
    let boundary = multer::parse_boundary(content_type)
        .map_err(|error| invalid(format!("invalid multipart content type: {error}")))?;

    let stream = stream::once(async move { Ok::<Bytes, std::io::Error>(body) });
    let mut multipart = Multipart::new(stream, boundary);

    let mut operations: Option<JsonValue> = None;
    let mut map: Option<BTreeMap<String, Vec<MappedPath>>> = None;
    let mut files: BTreeMap<String, Upload> = BTreeMap::new();
    let mut parts = 0usize;

    while let Some(field) = multipart
        .next_field()
        .await
        .map_err(|error| invalid(format!("invalid multipart body: {error}")))?
    {
        parts += 1;
        if max_parts != 0 && parts > max_parts {
            return Err(RequestError::new(format!(
                "A multipart request may contain at most {max_parts} parts."
            )));
        }

        // Match the name as a borrow: only a file part needs to own it, and the two control
        // fields (`operations`, `map`) are on every request.
        match field.name() {
            Some("operations") if operations.is_none() && map.is_none() && files.is_empty() => {
                let text = field
                    .text()
                    .await
                    .map_err(|error| invalid(format!("could not read `operations`: {error}")))?;
                let value: JsonValue = serde_json::from_str(&text)
                    .map_err(|error| invalid(format!("invalid `operations` JSON: {error}")))?;
                if !value.is_object() {
                    return Err(invalid(
                        "`operations` must be a single GraphQL request object; batching is not supported",
                    ));
                }
                operations = Some(value);
            }
            Some("map") if map.is_none() && operations.is_some() && files.is_empty() => {
                let Some(operations) = operations.as_ref() else {
                    unreachable!("the match guard requires operations");
                };
                let text = field
                    .text()
                    .await
                    .map_err(|error| invalid(format!("could not read `map`: {error}")))?;
                map = Some(parse_map(&text, operations)?);
            }
            Some(name) => {
                if name == "operations" && operations.is_some() && map.is_none() {
                    return Err(invalid("the `operations` field appears more than once"));
                }
                if name == "map" && operations.is_none() {
                    return Err(invalid("`operations` must appear before `map`"));
                }
                if name == "map"
                    && map.as_ref().is_some_and(|map| !map.contains_key("map"))
                    && files.is_empty()
                {
                    return Err(invalid("the `map` field appears more than once"));
                }
                // `field` is moved by `bytes()`, so the name has to be owned before that.
                let name = name.to_owned();
                let Some(map) = map.as_ref() else {
                    return Err(invalid(
                        "file fields must follow the `operations` and `map` fields",
                    ));
                };
                if !map.contains_key(&name) {
                    return Err(invalid(format!(
                        "file field \"{name}\" is not listed in the multipart `map`"
                    )));
                }
                if files.contains_key(&name) {
                    return Err(invalid(format!(
                        "file field \"{name}\" appears more than once"
                    )));
                }
                // The shared metadata is built straight into an `Arc<str>`; going through
                // `String` first would allocate and copy each of these twice.
                let filename = field
                    .file_name()
                    .map(Arc::<str>::from)
                    .ok_or_else(|| invalid(format!("field \"{name}\" must be a file")))?;
                let content_type = field
                    .content_type()
                    .map(|mime| Arc::<str>::from(mime.as_ref()));
                let bytes = field
                    .bytes()
                    .await
                    .map_err(|error| invalid(format!("could not read file \"{name}\": {error}")))?;
                files.insert(
                    name,
                    Upload::from_parts(Some(filename), content_type, Arc::from(bytes.as_ref())),
                );
            }
            None => {
                return Err(invalid("every multipart part must have a field name"));
            }
        }
    }

    let operations = operations.ok_or_else(|| invalid("missing the `operations` field"))?;
    let map = map.ok_or_else(|| invalid("missing the `map` field"))?;

    for (name, paths) in &map {
        if !files.contains_key(name) {
            return Err(invalid(format!(
                "no file was provided for map entry \"{name}\""
            )));
        }
        if paths.is_empty() {
            return Err(invalid(format!(
                "map entry \"{name}\" must list at least one operations path"
            )));
        }
    }

    let mut request = into_request(operations)?;
    for (name, mut paths) in map {
        let upload = files
            .remove(&name)
            .ok_or_else(|| invalid(format!("no file was provided for map entry \"{name}\"")))?;
        // `parse_map` rejects an entry with no paths, so this is always `Some`.
        let Some(last) = paths.pop() else {
            continue;
        };
        // Clone the shared upload only while another binding still needs it; the final path
        // takes ownership.
        for path in paths {
            bind(&mut request, path, upload.clone());
        }
        bind(&mut request, last, upload);
    }

    Ok(request)
}

/// Attach one upload to the request's variables path.
fn bind(request: &mut GraphQLRequest, path: MappedPath, upload: Upload) {
    request
        .uploads
        .push(UploadBinding::new(path.relative, upload));
}

/// A file's destination inside the request's variables.
struct MappedPath {
    /// The operations path, which always starts with `variables`.
    relative: String,
}

/// Parse and validate `map`, checking every destination exists as `null` in `operations`.
fn parse_map(
    text: &str,
    operations: &JsonValue,
) -> Result<BTreeMap<String, Vec<MappedPath>>, RequestError> {
    let raw: JsonValue = serde_json::from_str(text)
        .map_err(|error| invalid(format!("invalid `map` JSON: {error}")))?;
    let raw = raw
        .as_object()
        .ok_or_else(|| invalid("`map` must be a JSON object"))?;

    // A multipart request normally maps one file to one path, so a linear scan is cheaper than
    // a tree or a hash set; duplicates are rare and still rejected.
    let mut seen: Vec<String> = Vec::new();
    let mut map = BTreeMap::new();

    for (name, paths) in raw {
        let paths = paths
            .as_array()
            .ok_or_else(|| invalid(format!("map entry \"{name}\" must be an array of paths")))?;
        let mut mapped = Vec::with_capacity(paths.len());
        for path in paths {
            let path = path.as_str().ok_or_else(|| {
                invalid(format!(
                    "map entry \"{name}\" contains a path that is not a string"
                ))
            })?;
            let mapped_path = parse_path(path, operations)?;
            if seen
                .iter()
                .any(|relative| relative == &mapped_path.relative)
            {
                return Err(invalid(format!(
                    "map paths contain \"{path}\" more than once"
                )));
            }
            seen.push(mapped_path.relative.clone());
            mapped.push(mapped_path);
        }
        map.insert(name.clone(), mapped);
    }

    Ok(map)
}

/// Validate one operations path and confirm its destination is `null`.
fn parse_path(path: &str, operations: &JsonValue) -> Result<MappedPath, RequestError> {
    if path.is_empty() || path.starts_with('.') || path.ends_with('.') || path.contains("..") {
        return Err(invalid(format!(
            "map path \"{path}\" contains an empty segment"
        )));
    }

    let mut segments = path.split('.');
    if segments.next() != Some("variables") {
        return Err(invalid(format!(
            "map path \"{path}\" must address `variables`; files may only be placed in request variables"
        )));
    }
    if segments.next().is_none() {
        return Err(invalid(format!(
            "map path \"{path}\" does not name a variable"
        )));
    }

    let mut value = operations;
    for segment in path.split('.') {
        value = match value {
            JsonValue::Object(object) => object.get(segment),
            JsonValue::Array(list) => segment
                .parse::<usize>()
                .ok()
                .and_then(|index| list.get(index)),
            _ => None,
        }
        .ok_or_else(|| {
            invalid(format!(
                "map path \"{path}\" does not exist in `operations`"
            ))
        })?;
    }
    if !value.is_null() {
        return Err(invalid(format!(
            "map path \"{path}\" points at a value that is not `null`; the multipart specification requires files to be replaced with `null`"
        )));
    }

    Ok(MappedPath {
        relative: path.to_owned(),
    })
}

/// Convert the `operations` JSON into a request, reporting the payload shape.
fn into_request(operations: JsonValue) -> Result<GraphQLRequest, RequestError> {
    if !operations.is_object() {
        return Err(invalid(
            "`operations` must be a single GraphQL request object; batching is not supported",
        ));
    }
    serde_json::from_value::<GraphQLRequest>(operations)
        .map_err(|error| invalid(format!("invalid `operations` JSON: {error}")))
}

fn invalid(message: impl Into<String>) -> RequestError {
    RequestError::new(format!("Invalid multipart request: {}", message.into()))
}
