//! Copyright (c) Scott A Dixon
//!
//! The log store port.
//!
//! Every write runs inside [`Leases::fence`](walthamstow_kernel::leases::Leases::fence) for the
//! document its lease names, and is refused with `lease_lost` otherwise. A write names the
//! revision it read, and a write over any other revision is refused with `log_stale` and changes
//! nothing. A write carries an id its writer chooses: the store keeps the id and base of its last
//! write to a document, and a write that repeats both is given what that write was given, and
//! writes nothing again. A write is checked in this order: its lease, a retry, its base, then what
//! it carries.
//!
//! Reads are consistent and pure. A read never creates, repairs, truncates or upgrades anything,
//! and reports a broken document by a [`LogFault`](crate::LogFault).

use crate::types::{
    Listed, LogCommit, LogImage, Position, Revision, StoredHead, StoredRecord, WriteId,
};
use std::fmt;
use walthamstow_kernel::leases::Lease;
use walthamstow_kernel::{DocumentId, KernelResult};

/// Log documents, each written under a lease over it.
pub trait LogStore: Send + Sync + fmt::Debug {
    /// Creates the lease's document from `image`. Refused with `log_stale` while the document
    /// exists, and as an invalid request when the image's positions do not increase.
    fn create(&self, lease: &Lease, image: &LogImage) -> KernelResult<Revision>;

    /// Appends `commit`'s records and replaces the head when the commit carries one, over `base`.
    /// A commit with neither records nor a head is refused as an invalid request.
    fn commit(&self, lease: &Lease, base: &Revision, commit: &LogCommit) -> KernelResult<Revision>;

    /// Removes the records from `from` onwards, then applies `commit`, which may be empty.
    /// Refused as an invalid request when `from` is at or beyond the next position.
    fn rewind(
        &self,
        lease: &Lease,
        base: &Revision,
        from: Position,
        commit: &LogCommit,
    ) -> KernelResult<Revision>;

    /// Replaces the whole document: format, head and records with their positions. The next
    /// position is kept unless the image's highest position is at or beyond it, in which case it
    /// is one past that.
    fn replace(&self, lease: &Lease, base: &Revision, image: &LogImage) -> KernelResult<Revision>;

    /// Removes the document. A removed document is at no revision.
    fn remove(&self, lease: &Lease, base: &Revision, id: &WriteId) -> KernelResult<()>;

    /// The document's head, or `None` when there is no document.
    fn head(&self, document: &DocumentId) -> KernelResult<Option<StoredHead>>;

    /// Up to `limit` records at or after `from`, as the document stood at `revision`. Refused with
    /// `log_stale` once the document is at another revision or at none.
    fn records(
        &self,
        document: &DocumentId,
        revision: &Revision,
        from: Position,
        limit: usize,
    ) -> KernelResult<Vec<StoredRecord>>;

    /// Up to `limit` of the entity's documents, in resource order, after the resource `after`. A
    /// broken document is listed with its fault.
    fn list(&self, entity: &str, after: Option<&str>, limit: usize) -> KernelResult<Vec<Listed>>;
}
