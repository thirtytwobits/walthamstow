//! Copyright (c) Scott A Dixon
//!
//! The kernel's error model: a code naming the failure's category, a message,
//! and structured data a caller can branch on. A product's own error converts
//! from it and back.

use anyhow::Error as AnyhowError;
use clerkenwell_store::{StoreError, StoreErrorKind};
use serde_json::Value;

/// A normalised kernel failure, suitable for transport across crate boundaries.
///
/// The optional `data` field carries structured detail a caller can branch
/// on: a conflict can carry the current document and its etag, so the caller
/// can resolve it without another read.
#[derive(Debug, Clone)]
pub struct KernelError {
    pub code: &'static str,
    pub message: String,
    pub data: Option<Value>,
}

impl KernelError {
    /// Builds a not-found error for something the request names that does not exist.
    pub fn not_found(message: impl Into<String>) -> Self {
        Self {
            code: "not_found",
            message: message.into(),
            data: None,
        }
    }

    /// Builds an invalid-request error for bad parameters or illegal state transitions.
    pub fn invalid_request(message: impl Into<String>) -> Self {
        Self {
            code: "invalid_request",
            message: message.into(),
            data: None,
        }
    }

    /// Builds a cancellation error for interrupted long-running work.
    pub fn cancelled(message: impl Into<String>) -> Self {
        Self {
            code: "cancelled",
            message: message.into(),
            data: None,
        }
    }

    /// Builds a generic internal error for unexpected failures.
    pub fn internal(message: impl Into<String>) -> Self {
        Self {
            code: "internal_error",
            message: message.into(),
            data: None,
        }
    }

    /// Builds a conflict error for an optimistic-concurrency mismatch: the
    /// revision the caller expected is no longer current.
    pub fn conflict(message: impl Into<String>) -> Self {
        Self {
            code: "conflict",
            message: message.into(),
            data: None,
        }
    }

    /// Attaches structured data for a caller that needs more than the
    /// message to decide what to do next.
    pub fn with_data(mut self, data: Value) -> Self {
        self.data = Some(data);
        self
    }

    /// Names the failure with a reason code, such as one of
    /// [`crate::reason_codes`], merged into the data object as `code`. Data of
    /// another shape is kept as its `payload`.
    pub fn with_reason(mut self, code: &'static str) -> Self {
        let mut data = match self.data.take() {
            Some(Value::Object(map)) => map,
            None | Some(Value::Null) => serde_json::Map::new(),
            Some(other) => serde_json::Map::from_iter([("payload".to_string(), other)]),
        };
        data.insert("code".to_string(), Value::from(code));
        self.data = Some(Value::Object(data));
        self
    }

    /// The reason code naming this failure, if it carries one.
    pub fn reason_code(&self) -> Option<&str> {
        self.data
            .as_ref()
            .and_then(|data| data.get("code"))
            .and_then(Value::as_str)
    }
}

impl std::fmt::Display for KernelError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{}: {}", self.code, self.message)
    }
}
impl std::error::Error for KernelError {}

impl From<AnyhowError> for KernelError {
    /// Converts arbitrary internal failures into the generic transport-safe internal error shape.
    fn from(value: AnyhowError) -> Self {
        if let Some(error) = value.downcast_ref::<KernelError>() {
            return error.clone();
        }
        Self::internal(value.to_string())
    }
}

impl From<StoreError> for KernelError {
    /// Carries a collaboration store failure across with its message and data.
    fn from(value: StoreError) -> Self {
        let error = match value.kind {
            StoreErrorKind::NotFound => Self::not_found(value.message),
            StoreErrorKind::InvalidRequest => Self::invalid_request(value.message),
            StoreErrorKind::Conflict => Self::conflict(value.message),
            StoreErrorKind::Internal => Self::internal(value.message),
        };
        match value.data {
            Some(data) => error.with_data(data),
            None => error,
        }
    }
}

pub type KernelResult<T> = std::result::Result<T, KernelError>;

#[cfg(test)]
mod tests {
    use super::*;
    use anyhow::anyhow;

    #[test]
    fn constructors_preserve_codes_and_messages() {
        let not_found = KernelError::not_found("Missing document.");
        let invalid = KernelError::invalid_request("Bad payload.");
        let cancelled = KernelError::cancelled("Cancelled by caller.");
        let internal = KernelError::internal("Unexpected failure.");
        let conflict = KernelError::conflict("Etag mismatch.");

        assert_eq!(not_found.code, "not_found");
        assert_eq!(not_found.message, "Missing document.");
        assert_eq!(invalid.code, "invalid_request");
        assert_eq!(invalid.message, "Bad payload.");
        assert_eq!(cancelled.code, "cancelled");
        assert_eq!(cancelled.message, "Cancelled by caller.");
        assert_eq!(internal.code, "internal_error");
        assert_eq!(internal.message, "Unexpected failure.");
        assert_eq!(conflict.code, "conflict");
        assert_eq!(conflict.message, "Etag mismatch.");
        // All constructors default to no structured data; with_data
        // attaches a payload for callers that need to branch on it.
        assert!(not_found.data.is_none());
        let with_payload =
            KernelError::conflict("x").with_data(serde_json::json!({"current_etag": "sha256:abc"}));
        assert_eq!(with_payload.data.unwrap()["current_etag"], "sha256:abc");
    }

    #[test]
    fn a_reason_joins_the_data_and_reads_back() {
        let reason = "a_reason";
        let bare = KernelError::conflict("x").with_reason(reason);
        assert_eq!(bare.reason_code(), Some(reason));

        let with_payload = KernelError::conflict("x")
            .with_data(serde_json::json!({"current_etag": "sha256:abc"}))
            .with_reason(reason);
        assert_eq!(with_payload.reason_code(), Some(reason));
        assert_eq!(
            with_payload.data.as_ref().unwrap()["current_etag"],
            "sha256:abc"
        );

        let scalar = serde_json::json!(["kept"]);
        let wrapped = KernelError::conflict("x")
            .with_data(scalar.clone())
            .with_reason(reason);
        assert_eq!(wrapped.reason_code(), Some(reason));
        assert_eq!(wrapped.data.as_ref().unwrap()["payload"], scalar);

        assert_eq!(KernelError::conflict("x").reason_code(), None);
    }

    #[test]
    fn anyhow_errors_map_to_internal_error_with_display_text() {
        let error = KernelError::from(anyhow!("Disk full."));

        assert_eq!(error.code, "internal_error");
        assert_eq!(error.message, "Disk full.");
    }
}
