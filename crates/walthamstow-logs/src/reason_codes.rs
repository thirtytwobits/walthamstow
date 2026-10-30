//! Copyright (c) Scott A Dixon
//!
//! The reason codes log documents name their refusals with. A product's catalogue of reason codes
//! includes them.

/// A write or a read named a revision the document is not at, or a creation named a document
/// that exists.
pub const LOG_STALE: &str = "log_stale";

/// Data the store needs to read a log document is absent.
pub const LOG_MISSING: &str = "log_missing";

/// What is stored for a log document breaks the store's contract.
pub const LOG_INVALID: &str = "log_invalid";

/// Storage refused a read of a log document.
pub const LOG_INACCESSIBLE: &str = "log_inaccessible";
