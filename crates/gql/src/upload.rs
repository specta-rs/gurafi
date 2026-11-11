//! File uploads carried through the GraphQL value model.
//!
//! The GraphQL multipart request specification replaces each file in the operation's variables
//! with `null` and sends the bytes in a separate part. A transport parses those parts and inserts
//! an [`Upload`] into the already-converted variable value:
//!
//! ```no_run
//! use gql::{Value, Upload};
//!
//! # fn attach(variables: &mut gql::Object, bytes: Vec<u8>) {
//! let upload = Upload::new("avatar.png", "image/png", bytes);
//! variables.insert("file", Value::from(upload));
//! # }
//! ```
//!
//! Execution never understands the bytes. A custom scalar owns them: its `parse_value` function
//! receives the [`Value::External`] and downcasts it with
//! [`Value::as_external::<Upload>`](Value::as_external), after which a resolver can read
//! [`Upload::bytes`], [`Upload::filename`] and [`Upload::content_type`].
//!
//! Keeping the type in `gql` lets a transport crate depend on `gql` without making the execution
//! engine depend on that transport, or on a high-level schema builder.

use std::{fmt, sync::Arc};

use crate::Value;

/// A file supplied by a multipart request.
///
/// The bytes are immutable and shared through an [`Arc`], so cloning an upload into several
/// variable paths (the specification allows one file to be mapped to multiple places) does not
/// copy the contents.
///
/// `Debug` deliberately prints the name, content type and length but never the bytes; uploads are
/// commonly logged alongside request metadata.
#[derive(Clone, PartialEq, Eq)]
pub struct Upload {
    filename: Option<Arc<str>>,
    content_type: Option<Arc<str>>,
    bytes: Arc<[u8]>,
}

impl Upload {
    /// Create an upload with a filename and content type.
    #[must_use]
    pub fn new(
        filename: impl Into<Arc<str>>,
        content_type: impl Into<Arc<str>>,
        bytes: impl Into<Arc<[u8]>>,
    ) -> Self {
        Self {
            filename: Some(filename.into()),
            content_type: Some(content_type.into()),
            bytes: bytes.into(),
        }
    }

    /// Create an upload from optional metadata.
    ///
    /// A multipart part is not required to carry a `filename` or `Content-Type`; transports use
    /// this constructor so the missing values stay missing rather than becoming empty strings.
    #[must_use]
    pub fn from_parts(
        filename: Option<Arc<str>>,
        content_type: Option<Arc<str>>,
        bytes: Arc<[u8]>,
    ) -> Self {
        Self {
            filename,
            content_type,
            bytes,
        }
    }

    /// The multipart part's filename, if it declared one.
    #[must_use]
    pub fn filename(&self) -> Option<&str> {
        self.filename.as_deref()
    }

    /// The multipart part's content type, if it declared one.
    ///
    /// This value is supplied by the client and is not verified against the bytes.
    #[must_use]
    pub fn content_type(&self) -> Option<&str> {
        self.content_type.as_deref()
    }

    /// The file contents.
    #[must_use]
    pub fn bytes(&self) -> &[u8] {
        &self.bytes
    }

    /// Consume the upload, returning its shared contents.
    #[must_use]
    pub fn into_bytes(self) -> Arc<[u8]> {
        self.bytes
    }

    /// The number of bytes.
    #[must_use]
    pub fn len(&self) -> usize {
        self.bytes.len()
    }

    /// Whether the file is empty.
    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.bytes.is_empty()
    }
}

impl fmt::Debug for Upload {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("Upload")
            .field("filename", &self.filename)
            .field("content_type", &self.content_type)
            .field("len", &self.bytes.len())
            .finish()
    }
}

impl From<Upload> for Value {
    fn from(value: Upload) -> Self {
        Self::external(value)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn debug_does_not_print_contents() {
        let upload = Upload::new("a.txt", "text/plain", b"top secret".to_vec());
        let debug = format!("{upload:?}");
        assert!(debug.contains("a.txt"));
        assert!(debug.contains("text/plain"));
        assert!(!debug.contains("top secret"));
    }

    #[test]
    fn value_round_trips_through_external() {
        let upload = Upload::new("a.txt", "text/plain", b"hello".to_vec());
        let value = Value::from(upload);
        let borrowed = value.as_external::<Upload>().expect("upload");
        assert_eq!(borrowed.filename(), Some("a.txt"));
        assert_eq!(borrowed.bytes(), b"hello");
    }
}
