// TODO: This will probs need to move into `gql`???
// TODO: Should this be a visitor trait like Serde so we are format agnostic?

use std::collections::HashMap;

#[derive(Debug)] // TODO: Copy the derive from `serde_json::Value` probs
#[cfg_attr(feature = "serde", derive(serde::Serialize, serde::Deserialize))]
#[cfg_attr(feature = "serde", serde(untagged))]
pub enum Value {
    Null,
    Bool(bool),
    // Number(Number),
    String(String),
    Array(Vec<Value>),
    Object(HashMap<String, Value>),
}

// TODO: `From<T> for Value` impls for common types
