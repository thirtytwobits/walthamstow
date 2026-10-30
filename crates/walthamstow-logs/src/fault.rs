//! Copyright (c) Scott A Dixon
//!
//! Why a log document cannot be read, and why a blob cannot. A read reports a broken document by
//! a [`LogFault`], never by a message alone: `list` carries the fault, and `head` and `records`
//! refuse with it, from which [`LogFault::of`] reads it back. A blob store reports an absent or
//! corrupt blob by a [`BlobFault`] the same way.

use crate::reason_codes;
use crate::types::BlobRef;
use serde_json::{json, Value};
use walthamstow_kernel::{DocumentId, KernelError};

/// The kinds of [`LogFault`], each named by its reason code.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum FaultKind {
    Missing,
    Invalid,
    Inaccessible,
}

impl FaultKind {
    pub fn reason_code(self) -> &'static str {
        match self {
            Self::Missing => reason_codes::LOG_MISSING,
            Self::Invalid => reason_codes::LOG_INVALID,
            Self::Inaccessible => reason_codes::LOG_INACCESSIBLE,
        }
    }
}

/// Why a log document cannot be read.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum LogFault {
    /// Data the store needs to read the document is absent: each part named.
    Missing { parts: Vec<String> },
    /// What is stored breaks the store's contract: a line that is not a record, positions out of
    /// order, or a position at or beyond the next position.
    Invalid { detail: String },
    /// Storage refused the read.
    Inaccessible { detail: String },
}

impl LogFault {
    pub fn kind(&self) -> FaultKind {
        match self {
            Self::Missing { .. } => FaultKind::Missing,
            Self::Invalid { .. } => FaultKind::Invalid,
            Self::Inaccessible { .. } => FaultKind::Inaccessible,
        }
    }

    /// The refusal of a read of `document`, carrying this fault.
    pub fn error(&self, document: &DocumentId) -> KernelError {
        let (message, data) = match self {
            Self::Missing { parts } => (
                format!(
                    "The log document {document} is missing {}.",
                    parts.join(", ")
                ),
                json!({ "parts": parts }),
            ),
            Self::Invalid { detail } => (
                format!("The log document {document} is invalid: {detail}"),
                json!({ "detail": detail }),
            ),
            Self::Inaccessible { detail } => (
                format!("The log document {document} cannot be read: {detail}"),
                json!({ "detail": detail }),
            ),
        };
        KernelError::internal(message)
            .with_data(data)
            .with_reason(self.kind().reason_code())
    }

    /// The fault a refusal carries, if it carries one.
    pub fn of(error: &KernelError) -> Option<Self> {
        let data = error.data.as_ref()?;
        let text = |key: &str| data.get(key).and_then(Value::as_str).map(str::to_string);
        match error.reason_code()? {
            reason_codes::LOG_MISSING => Some(Self::Missing {
                parts: data
                    .get("parts")?
                    .as_array()?
                    .iter()
                    .map(|part| part.as_str().map(str::to_string))
                    .collect::<Option<_>>()?,
            }),
            reason_codes::LOG_INVALID => Some(Self::Invalid {
                detail: text("detail")?,
            }),
            reason_codes::LOG_INACCESSIBLE => Some(Self::Inaccessible {
                detail: text("detail")?,
            }),
            _ => None,
        }
    }
}

/// Why a blob cannot be read.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum BlobFault {
    /// The blob is absent.
    Missing(BlobRef),
    /// The blob's bytes do not match the digest that names it.
    Corrupt(BlobRef),
}

impl BlobFault {
    /// The blob the fault names.
    pub fn blob(&self) -> &BlobRef {
        match self {
            Self::Missing(blob) | Self::Corrupt(blob) => blob,
        }
    }

    pub fn reason_code(&self) -> &'static str {
        match self {
            Self::Missing(_) => reason_codes::BLOB_MISSING,
            Self::Corrupt(_) => reason_codes::BLOB_CORRUPT,
        }
    }

    /// The refusal of a read of a blob of `document`, carrying this fault.
    pub fn error(&self, document: &DocumentId) -> KernelError {
        let message = match self {
            Self::Missing(blob) => format!("The log document {document} has no blob {blob}."),
            Self::Corrupt(blob) => {
                format!("The blob {blob} of the log document {document} does not match its digest.")
            }
        };
        KernelError::internal(message)
            .with_data(json!({ "blob": self.blob().to_string() }))
            .with_reason(self.reason_code())
    }

    /// The fault a refusal carries, if it carries one.
    pub fn of(error: &KernelError) -> Option<Self> {
        let blob = error
            .data
            .as_ref()?
            .get("blob")?
            .as_str()?
            .parse::<BlobRef>()
            .ok()?;
        match error.reason_code()? {
            reason_codes::BLOB_MISSING => Some(Self::Missing(blob)),
            reason_codes::BLOB_CORRUPT => Some(Self::Corrupt(blob)),
            _ => None,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_blob_fault_reads_back_from_its_refusal() {
        let document = DocumentId::new("Session", "one");
        let blob = BlobRef::new("capture", "a".repeat(64)).unwrap();
        for fault in [
            BlobFault::Missing(blob.clone()),
            BlobFault::Corrupt(blob.clone()),
        ] {
            let error = fault.error(&document);
            assert_eq!(error.reason_code(), Some(fault.reason_code()));
            assert_eq!(BlobFault::of(&error), Some(fault));
        }
        assert_eq!(
            BlobFault::of(&LogFault::Invalid { detail: "x".into() }.error(&document)),
            None
        );
    }

    #[test]
    fn a_fault_reads_back_from_its_refusal() {
        let document = DocumentId::new("Session", "one");
        for fault in [
            LogFault::Missing {
                parts: vec!["head".into(), "log".into()],
            },
            LogFault::Invalid {
                detail: "line 3 is not a record".into(),
            },
            LogFault::Inaccessible {
                detail: "permission denied".into(),
            },
        ] {
            let error = fault.error(&document);
            assert_eq!(error.reason_code(), Some(fault.kind().reason_code()));
            assert_eq!(LogFault::of(&error), Some(fault));
        }
        assert_eq!(LogFault::of(&KernelError::conflict("stale")), None);
    }
}
