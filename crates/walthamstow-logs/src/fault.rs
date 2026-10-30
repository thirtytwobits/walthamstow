//! Copyright (c) Scott A Dixon
//!
//! Why a log document cannot be read. A read reports a broken document by a [`LogFault`], never
//! by a message alone: `list` carries the fault, and `head` and `records` refuse with it, from
//! which [`LogFault::of`] reads it back.

use crate::reason_codes;
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

#[cfg(test)]
mod tests {
    use super::*;

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
