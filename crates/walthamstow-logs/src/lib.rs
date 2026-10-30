//! Copyright (c) Scott A Dixon
//!
//! Log documents: documents that do not merge. A turn of a conversation, an approval or an audit
//! trail is written by one lease holder at a time, and the order of what was written is the
//! record. A log document is a head, replaced whole, a format the product gives, and a log of
//! records appended in order. The store reads neither the head nor a record's payload.
//!
//! [`LogStore`] is the port. [`MemoryLogStore`] keeps log documents in memory, for an engine with
//! no disk and for tests. `conformance`, behind the `conformance` feature, is the test suite every
//! adapter runs.

#[cfg(any(test, feature = "conformance"))]
pub mod conformance;
pub mod fault;
pub mod memory;
pub mod reason_codes;
pub mod store;
pub mod types;

pub use fault::{FaultKind, LogFault};
pub use memory::MemoryLogStore;
pub use store::LogStore;
pub use types::{
    BlobRef, Listed, LogCommit, LogImage, NewRecord, Position, Revision, StoredHead, StoredRecord,
    WriteId,
};
