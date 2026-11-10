//! Persisted-query manifests.
//!
//! [`PreparedQueries`] loads operation text from the JSON manifests produced by Relay,
//! graphql-codegen, Apollo tooling, Hive, Envelop and urql, and builds a
//! [`gql::prepared::PreparedQueryRegistry`] once at startup. Requests then carry only an
//! identifier; the query text is never trusted or parsed per request.
//!
//! # Supported manifest shapes
//!
//! ```json
//! { "queryId": "query { hello }" }
//! ```
//!
//! ```json
//! { "operations": [ { "id": "a", "body": "query { hello }", "name": "Hello", "type": "query" } ] }
//! ```
//!
//! ```json
//! [ { "hash": "a", "query": "query { hello }" } ]
//! ```
//!
//! * Relay's generated manifest is the first form.
//! * Hive, Envelop and GraphQL Codegen's operation manifests are the second form.
//! * graphql-codegen's persisted-document output and urql's document map are the first form.
//! * An object under `"operations"` (or `"documents"`) may itself be a map of id to text.
//!
//! Operation objects accept `id`/`hash`/`sha256Hash`/`queryId` for the identifier,
//! `body`/`query`/`document`/`source` for the text, `name`/`operationName` for the operation
//! name, and `type` for a query/mutation/subscription check.

use std::{collections::HashSet, sync::Arc};

use gql::{
    Schema,
    ast::OperationKind,
    prepared::{PreparedQuery, PreparedQueryRegistry, PreparedQueryRegistryError},
};
use serde::{
    Deserialize,
    de::{self, Deserializer, MapAccess, SeqAccess, Visitor},
};
use serde_json::{Map as JsonMap, Value as JsonValue};

/// One operation loaded from a manifest.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct PreparedOperation {
    /// The persisted identifier.
    pub id: String,
    /// The GraphQL source.
    pub source: String,
    /// The operation to execute when the source contains several.
    pub operation_name: Option<String>,
    /// The operation type declared by the manifest, when it declares one.
    pub kind: Option<OperationKind>,
}

impl PreparedOperation {
    /// Create an operation entry.
    #[must_use]
    pub fn new(id: impl Into<String>, source: impl Into<String>) -> Self {
        Self {
            id: id.into(),
            source: source.into(),
            operation_name: None,
            kind: None,
        }
    }

    /// Select an operation by name.
    #[must_use]
    pub fn operation_name(mut self, name: impl Into<String>) -> Self {
        self.operation_name = Some(name.into());
        self
    }

    /// Record the manifest's operation type.
    #[must_use]
    pub fn kind(mut self, kind: OperationKind) -> Self {
        self.kind = Some(kind);
        self
    }
}

/// An error produced while loading a manifest.
#[derive(Debug)]
pub enum PreparedQueriesError {
    /// The manifest was not valid JSON.
    Json(serde_json::Error),
    /// The JSON was valid but did not have a supported manifest shape.
    Invalid(String),
    /// A query in the manifest was invalid against the schema.
    Registry(PreparedQueryRegistryError),
    /// The manifest's `type` disagreed with the operation it contains.
    KindMismatch {
        /// The operation's identifier.
        id: String,
        /// The type the manifest declared.
        expected: OperationKind,
        /// The type actually parsed from the source.
        actual: OperationKind,
    },
}

impl std::fmt::Display for PreparedQueriesError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Json(error) => write!(f, "invalid persisted-query manifest JSON: {error}"),
            Self::Invalid(message) => write!(f, "invalid persisted-query manifest: {message}"),
            Self::Registry(error) => write!(f, "{error}"),
            Self::KindMismatch {
                id,
                expected,
                actual,
            } => write!(
                f,
                "prepared query \"{id}\" is declared as {expected} but contains a {actual} operation"
            ),
        }
    }
}

impl std::error::Error for PreparedQueriesError {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        match self {
            Self::Json(error) => Some(error),
            Self::Registry(error) => Some(error),
            Self::Invalid(_) | Self::KindMismatch { .. } => None,
        }
    }
}

impl From<serde_json::Error> for PreparedQueriesError {
    fn from(value: serde_json::Error) -> Self {
        Self::Json(value)
    }
}

impl From<PreparedQueryRegistryError> for PreparedQueriesError {
    fn from(value: PreparedQueryRegistryError) -> Self {
        Self::Registry(value)
    }
}

/// A validated set of persisted operations loaded from a manifest.
///
/// Cloning is cheap; the underlying [`PreparedQueryRegistry`] shares one allocation.
#[derive(Clone, Debug)]
pub struct PreparedQueries {
    registry: PreparedQueryRegistry,
}

impl Default for PreparedQueries {
    fn default() -> Self {
        Self::empty()
    }
}

impl PreparedQueries {
    /// An empty set.
    #[must_use]
    pub fn empty() -> Self {
        Self {
            registry: PreparedQueryRegistry::empty(),
        }
    }

    /// Wrap an existing registry.
    #[must_use]
    pub fn from_registry(registry: PreparedQueryRegistry) -> Self {
        Self { registry }
    }

    /// Load a manifest from a JSON string.
    ///
    /// Duplicate object keys are rejected rather than silently keeping the last value, so two
    /// entries with the same identifier cannot hide a mistake in a generated manifest.
    ///
    /// # Errors
    ///
    /// Returns an error for malformed JSON, a duplicate object key, an unrecognised manifest
    /// shape, a duplicate id, or a query that is invalid against the schema.
    pub fn from_json_str(schema: &Schema, json: &str) -> Result<Self, PreparedQueriesError> {
        reject_duplicate_keys_str(json)?;
        let value: JsonValue = serde_json::from_str(json)?;
        Self::from_json_value(schema, &value)
    }

    /// Load a manifest from JSON bytes.
    ///
    /// Duplicate object keys are rejected, exactly as in [`PreparedQueries::from_json_str`].
    ///
    /// # Errors
    ///
    /// Returns an error for malformed JSON, a duplicate object key, or an invalid manifest.
    pub fn from_json_slice(schema: &Schema, json: &[u8]) -> Result<Self, PreparedQueriesError> {
        reject_duplicate_keys_slice(json)?;
        let value: JsonValue = serde_json::from_slice(json)?;
        Self::from_json_value(schema, &value)
    }

    /// Load a manifest from an already parsed JSON value.
    ///
    /// A parsed JSON object cannot retain duplicate keys, so this path cannot detect them; prefer
    /// [`PreparedQueries::from_json_str`] or [`PreparedQueries::from_json_slice`] for untrusted or
    /// generated manifest text.
    ///
    /// # Errors
    ///
    /// Returns an error for an unrecognised manifest shape or an invalid query.
    pub fn from_json_value(
        schema: &Schema,
        value: &JsonValue,
    ) -> Result<Self, PreparedQueriesError> {
        let operations = operations_from_json(value)?;
        Self::from_operations(schema, operations)
    }

    /// Load a manifest from JSON. This is an alias for [`PreparedQueries::from_json_str`].
    ///
    /// # Errors
    ///
    /// Returns an error for malformed JSON or an invalid manifest.
    pub fn from_manifest(schema: &Schema, json: &str) -> Result<Self, PreparedQueriesError> {
        Self::from_json_str(schema, json)
    }

    /// Build a set from `(id, source)` entries.
    ///
    /// # Errors
    ///
    /// Returns an error for a duplicate id or an invalid query.
    pub fn from_map<K, V, I>(schema: &Schema, entries: I) -> Result<Self, PreparedQueriesError>
    where
        K: AsRef<str>,
        V: AsRef<str>,
        I: IntoIterator<Item = (K, V)>,
    {
        let operations = entries
            .into_iter()
            .map(|(id, source)| PreparedOperation::new(id.as_ref(), source.as_ref()));
        Self::from_operations(schema, operations)
    }

    /// Build a set from explicit operation entries.
    ///
    /// # Errors
    ///
    /// Returns an error for a duplicate id, an invalid query, or a `kind` that disagrees with the
    /// operation in the source.
    pub fn from_operations<I>(schema: &Schema, operations: I) -> Result<Self, PreparedQueriesError>
    where
        I: IntoIterator<Item = PreparedOperation>,
    {
        let operations: Vec<PreparedOperation> = operations.into_iter().collect();
        let registry = PreparedQueryRegistry::new(
            schema,
            operations.iter().map(|operation| {
                (
                    operation.id.as_str(),
                    operation.source.as_str(),
                    operation.operation_name.as_deref(),
                )
            }),
        )?;

        for operation in &operations {
            if let Some(expected) = operation.kind {
                let Some(prepared) = registry.get(&operation.id) else {
                    continue;
                };
                let actual = prepared.kind();
                if actual != expected {
                    return Err(PreparedQueriesError::KindMismatch {
                        id: operation.id.clone(),
                        expected,
                        actual,
                    });
                }
            }
        }

        Ok(Self { registry })
    }

    /// The underlying registry.
    #[must_use]
    pub fn registry(&self) -> &PreparedQueryRegistry {
        &self.registry
    }

    /// Consume this set into its registry.
    #[must_use]
    pub fn into_registry(self) -> PreparedQueryRegistry {
        self.registry
    }

    /// Look up a prepared query by identifier.
    #[must_use]
    pub fn get(&self, id: &str) -> Option<&PreparedQuery> {
        self.registry.get(id)
    }

    /// The number of loaded operations.
    #[must_use]
    pub fn len(&self) -> usize {
        self.registry.len()
    }

    /// Whether no operations were loaded.
    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.registry.is_empty()
    }

    /// Identifiers in manifest order.
    pub fn ids(&self) -> impl ExactSizeIterator<Item = &str> {
        self.registry.ids()
    }
}

impl From<PreparedQueries> for PreparedQueryRegistry {
    fn from(value: PreparedQueries) -> Self {
        value.registry
    }
}

fn operations_from_json(value: &JsonValue) -> Result<Vec<PreparedOperation>, PreparedQueriesError> {
    match value {
        JsonValue::Object(map) => {
            if let Some(operations) = map.get("operations").or_else(|| map.get("documents")) {
                return operations_from_json(operations);
            }
            operations_from_map(map)
        }
        JsonValue::Array(items) => {
            if items.is_empty() {
                return Err(PreparedQueriesError::Invalid(
                    "the manifest contains no operations".to_owned(),
                ));
            }
            items
                .iter()
                .enumerate()
                .map(|(index, item)| {
                    operation_from_object(item, None).map_err(|error| match error {
                        PreparedQueriesError::Invalid(message) => PreparedQueriesError::Invalid(
                            format!("operation at index {index}: {message}"),
                        ),
                        other => other,
                    })
                })
                .collect()
        }
        _ => Err(PreparedQueriesError::Invalid(
            "expected an object map or an array of operations".to_owned(),
        )),
    }
}

/// Read a `{id: source}` map, or a map of id to operation objects.
fn operations_from_map(
    map: &JsonMap<String, JsonValue>,
) -> Result<Vec<PreparedOperation>, PreparedQueriesError> {
    if map.is_empty() {
        return Err(PreparedQueriesError::Invalid(
            "the manifest contains no operations".to_owned(),
        ));
    }
    map.iter()
        .map(|(id, value)| match value {
            JsonValue::String(source) => Ok(PreparedOperation::new(id.as_str(), source.as_str())),
            JsonValue::Object(_) => operation_from_object(value, Some(id.as_str())),
            _ => Err(PreparedQueriesError::Invalid(format!(
                "operation \"{id}\" must be a GraphQL string or an object"
            ))),
        })
        .collect()
}

/// Read one operation object from a Hive/Envelop/codegen-style array.
fn operation_from_object(
    value: &JsonValue,
    fallback_id: Option<&str>,
) -> Result<PreparedOperation, PreparedQueriesError> {
    let JsonValue::Object(map) = value else {
        return Err(PreparedQueriesError::Invalid(
            "each operation must be an object".to_owned(),
        ));
    };

    let id = first_string(map, &["id", "hash", "sha256Hash", "queryId", "documentId"])
        .or(fallback_id)
        .ok_or_else(|| {
            PreparedQueriesError::Invalid("operation is missing an id or hash".to_owned())
        })?;
    let source = first_string(map, &["body", "query", "document", "source"]).ok_or_else(|| {
        PreparedQueriesError::Invalid(format!(
            "operation \"{id}\" is missing its GraphQL source (body/query)"
        ))
    })?;
    let operation_name = first_string(map, &["name", "operationName"]).map(str::to_owned);
    let kind = first_string(map, &["type"])
        .map(|kind| match kind.to_ascii_lowercase().as_str() {
            "query" => Ok(OperationKind::Query),
            "mutation" => Ok(OperationKind::Mutation),
            "subscription" => Ok(OperationKind::Subscription),
            other => Err(PreparedQueriesError::Invalid(format!(
                "operation \"{id}\" has unsupported type \"{other}\""
            ))),
        })
        .transpose()?;

    Ok(PreparedOperation {
        id: id.to_owned(),
        source: source.to_owned(),
        operation_name,
        kind,
    })
}

fn first_string<'a>(map: &'a JsonMap<String, JsonValue>, keys: &[&str]) -> Option<&'a str> {
    keys.iter().find_map(|key| {
        map.get(*key)
            .and_then(JsonValue::as_str)
            .filter(|value| !value.is_empty())
    })
}

/// A JSON value walker that rejects duplicate object keys.
///
/// `serde_json` parses an object into a map, so a duplicated key silently overwrites the earlier
/// value before any manifest code can see it. This visitor runs first and reports the duplicate
/// with its key, which is the only place the raw text still distinguishes the two entries.
struct DuplicateKeyCheck;

impl<'de> Deserialize<'de> for DuplicateKeyCheck {
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: Deserializer<'de>,
    {
        deserializer.deserialize_any(DuplicateKeyVisitor)
    }
}

struct DuplicateKeyVisitor;

impl<'de> Visitor<'de> for DuplicateKeyVisitor {
    type Value = DuplicateKeyCheck;

    fn expecting(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter.write_str("any JSON value")
    }

    fn visit_map<A>(self, mut map: A) -> Result<Self::Value, A::Error>
    where
        A: MapAccess<'de>,
    {
        let mut seen = HashSet::new();
        while let Some(key) = map.next_key::<String>()? {
            if !seen.insert(key.clone()) {
                return Err(de::Error::custom(format!("duplicate object key \"{key}\"")));
            }
            map.next_value::<DuplicateKeyCheck>()?;
        }
        Ok(DuplicateKeyCheck)
    }

    fn visit_seq<A>(self, mut seq: A) -> Result<Self::Value, A::Error>
    where
        A: SeqAccess<'de>,
    {
        while seq.next_element::<DuplicateKeyCheck>()?.is_some() {}
        Ok(DuplicateKeyCheck)
    }

    fn visit_bool<E>(self, _value: bool) -> Result<Self::Value, E> {
        Ok(DuplicateKeyCheck)
    }

    fn visit_i64<E>(self, _value: i64) -> Result<Self::Value, E> {
        Ok(DuplicateKeyCheck)
    }

    fn visit_u64<E>(self, _value: u64) -> Result<Self::Value, E> {
        Ok(DuplicateKeyCheck)
    }

    fn visit_f64<E>(self, _value: f64) -> Result<Self::Value, E> {
        Ok(DuplicateKeyCheck)
    }

    fn visit_str<E>(self, _value: &str) -> Result<Self::Value, E> {
        Ok(DuplicateKeyCheck)
    }

    fn visit_string<E>(self, _value: String) -> Result<Self::Value, E> {
        Ok(DuplicateKeyCheck)
    }

    fn visit_unit<E>(self) -> Result<Self::Value, E> {
        Ok(DuplicateKeyCheck)
    }

    fn visit_none<E>(self) -> Result<Self::Value, E> {
        Ok(DuplicateKeyCheck)
    }

    fn visit_some<D>(self, deserializer: D) -> Result<Self::Value, D::Error>
    where
        D: Deserializer<'de>,
    {
        DuplicateKeyCheck::deserialize(deserializer)
    }
}

fn reject_duplicate_keys_str(json: &str) -> Result<(), serde_json::Error> {
    let mut deserializer = serde_json::Deserializer::from_str(json);
    DuplicateKeyCheck::deserialize(&mut deserializer)?;
    deserializer.end()
}

fn reject_duplicate_keys_slice(json: &[u8]) -> Result<(), serde_json::Error> {
    let mut deserializer = serde_json::Deserializer::from_slice(json);
    DuplicateKeyCheck::deserialize(&mut deserializer)?;
    deserializer.end()
}

/// Build the error response for an unknown persisted identifier.
///
/// Apollo clients look for the message `PersistedQueryNotFound`; the machine-readable code lives
/// in `extensions.code`.
#[must_use]
pub fn persisted_query_not_found(id: &str) -> gql::Response {
    gql::Response::error(persisted_query_not_found_error(id))
}

/// Build the GraphQL error for an unknown persisted identifier.
#[must_use]
pub fn persisted_query_not_found_error(id: &str) -> gql::GraphQLError {
    gql::GraphQLError::new("PersistedQueryNotFound")
        .extend("code", "PERSISTED_QUERY_NOT_FOUND")
        .extend("persistedQueryId", Arc::<str>::from(id))
}
