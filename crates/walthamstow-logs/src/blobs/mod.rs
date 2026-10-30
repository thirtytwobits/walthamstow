//! Copyright (c) Scott A Dixon
//!
//! Blobs: bytes a record or a head names, kept once per document under a kind the product
//! registers and named by their SHA-256. [`BlobStore`] is the port. A put and a removal run
//! under the document's lease; a read checks the bytes against the digest that names them and
//! reports an absent or corrupt blob by a [`BlobFault`], never replacing it. The store does not
//! know what names a blob: a caller collects the blobs that `list` returns and that no head or
//! record names.
//!
//! [`MemoryBlobStore`] keeps blobs in memory. [`FileBlobStore`] keeps each as a file people can
//! open, at `<document directory>/<kind>/<sha256>.<extension>`.

mod file;
mod memory;

pub use file::FileBlobStore;
pub use memory::MemoryBlobStore;

use crate::fault::BlobFault;
use crate::types::{BlobKind, BlobRef};
use std::collections::BTreeMap;
use std::fmt;
use walthamstow_kernel::leases::Lease;
use walthamstow_kernel::{DocumentId, KernelError, KernelResult};

/// Blobs, each kept under a document and written under its lease.
pub trait BlobStore: Send + Sync + fmt::Debug {
    /// Stores `bytes` in the lease's document as a blob of the registered kind named `kind`, and
    /// returns what names it. Putting bytes the store already holds changes nothing. Refused as an
    /// invalid request when `kind` is not registered.
    fn put(&self, lease: &Lease, kind: &str, bytes: &[u8]) -> KernelResult<BlobRef>;

    /// The bytes `blob` names, checked against its digest, or `None` when the blob is absent.
    /// Bytes that do not match are refused with `blob_corrupt` and never returned.
    fn get(&self, document: &DocumentId, blob: &BlobRef) -> KernelResult<Option<Vec<u8>>>;

    /// Up to `limit` of the document's blobs, by kind then digest, after `after`. A page of none
    /// is refused as an invalid request.
    fn list(
        &self,
        document: &DocumentId,
        after: Option<&BlobRef>,
        limit: usize,
    ) -> KernelResult<Vec<BlobRef>>;

    /// Which of `blobs` are absent or do not match their digest, in the order given.
    fn check(&self, document: &DocumentId, blobs: &[BlobRef]) -> KernelResult<Vec<BlobFault>>;

    /// Removes `blobs` from the lease's document. A blob already absent is left so.
    fn remove(&self, lease: &Lease, blobs: &[BlobRef]) -> KernelResult<()>;
}

/// The kinds of blob a store keeps, registered once when the store is built.
#[derive(Debug, Clone, Default)]
pub struct BlobKinds {
    kinds: BTreeMap<String, BlobKind>,
}

impl BlobKinds {
    /// `kinds`, each registered once. A name registered twice is refused as an invalid request.
    pub fn new(kinds: impl IntoIterator<Item = BlobKind>) -> KernelResult<Self> {
        let mut registered = BTreeMap::new();
        for kind in kinds {
            if registered.contains_key(kind.name()) {
                return Err(KernelError::invalid_request(format!(
                    "The blob kind {:?} is registered twice.",
                    kind.name()
                )));
            }
            registered.insert(kind.name().to_owned(), kind);
        }
        Ok(Self { kinds: registered })
    }

    /// The registered kind named `name`, or an invalid request when none is.
    pub fn get(&self, name: &str) -> KernelResult<&BlobKind> {
        self.kinds.get(name).ok_or_else(|| {
            KernelError::invalid_request(format!("The blob kind {name:?} is not registered."))
        })
    }

    /// Every registered kind, by name.
    pub fn iter(&self) -> impl Iterator<Item = &BlobKind> {
        self.kinds.values()
    }
}

/// Refuses a page of none.
fn require_limit(limit: usize) -> KernelResult<()> {
    if limit == 0 {
        return Err(KernelError::invalid_request(
            "A page of blobs holds at least one.",
        ));
    }
    Ok(())
}

/// The reference the blob of `kind` holding `bytes` is named by.
fn name(kind: &BlobKind, bytes: &[u8]) -> BlobRef {
    BlobRef::new(kind.name(), walthamstow_kernel::hashing::sha256_hex(bytes))
        .expect("a registered kind and a digest name a blob")
}
