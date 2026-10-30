//! Copyright (c) Scott A Dixon
//!
//! Blobs kept in memory, for an engine with no disk and for tests.

use super::{name, require_limit, BlobKinds, BlobStore};
use crate::fault::BlobFault;
use crate::types::BlobRef;
use std::collections::BTreeMap;
use std::fmt;
use std::ops::Bound;
use std::sync::{Arc, Mutex, MutexGuard, PoisonError};
use walthamstow_kernel::leases::{Lease, Leases};
use walthamstow_kernel::{DocumentId, KernelResult};

/// Blobs kept in memory, written under the leases the store is given.
pub struct MemoryBlobStore {
    leases: Arc<dyn Leases>,
    kinds: BlobKinds,
    blobs: Mutex<BTreeMap<DocumentId, BTreeMap<BlobRef, Vec<u8>>>>,
}

impl fmt::Debug for MemoryBlobStore {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("MemoryBlobStore")
            .field("leases", &self.leases)
            .field("kinds", &self.kinds)
            .finish_non_exhaustive()
    }
}

fn locked<T>(mutex: &Mutex<T>) -> MutexGuard<'_, T> {
    mutex.lock().unwrap_or_else(PoisonError::into_inner)
}

impl MemoryBlobStore {
    /// An empty store of `kinds`, whose writes `leases` fence.
    pub fn new(leases: Arc<dyn Leases>, kinds: BlobKinds) -> Self {
        Self {
            leases,
            kinds,
            blobs: Mutex::new(BTreeMap::new()),
        }
    }
}

impl BlobStore for MemoryBlobStore {
    fn put(&self, lease: &Lease, kind: &str, bytes: &[u8]) -> KernelResult<BlobRef> {
        let blob = name(self.kinds.get(kind)?, bytes);
        self.leases.as_ref().fenced(lease, || {
            locked(&self.blobs)
                .entry(lease.document().clone())
                .or_default()
                .entry(blob.clone())
                .or_insert_with(|| bytes.to_vec());
            Ok::<_, walthamstow_kernel::KernelError>(())
        })?;
        Ok(blob)
    }

    fn get(&self, document: &DocumentId, blob: &BlobRef) -> KernelResult<Option<Vec<u8>>> {
        self.kinds.get(blob.kind())?;
        Ok(locked(&self.blobs)
            .get(document)
            .and_then(|blobs| blobs.get(blob))
            .cloned())
    }

    fn list(
        &self,
        document: &DocumentId,
        after: Option<&BlobRef>,
        limit: usize,
    ) -> KernelResult<Vec<BlobRef>> {
        require_limit(limit)?;
        let start = after.map_or(Bound::Unbounded, |after| Bound::Excluded(after.clone()));
        Ok(locked(&self.blobs)
            .get(document)
            .map(|blobs| {
                blobs
                    .range((start, Bound::Unbounded))
                    .take(limit)
                    .map(|(blob, _)| blob.clone())
                    .collect()
            })
            .unwrap_or_default())
    }

    fn check(&self, document: &DocumentId, blobs: &[BlobRef]) -> KernelResult<Vec<BlobFault>> {
        for blob in blobs {
            self.kinds.get(blob.kind())?;
        }
        let held = locked(&self.blobs);
        Ok(blobs
            .iter()
            .filter(|blob| {
                !held
                    .get(document)
                    .is_some_and(|blobs| blobs.contains_key(*blob))
            })
            .map(|blob| BlobFault::Missing(blob.clone()))
            .collect())
    }

    fn remove(&self, lease: &Lease, blobs: &[BlobRef]) -> KernelResult<()> {
        for blob in blobs {
            self.kinds.get(blob.kind())?;
        }
        self.leases.as_ref().fenced(lease, || {
            let mut held = locked(&self.blobs);
            if let Some(document) = held.get_mut(lease.document()) {
                for blob in blobs {
                    document.remove(blob);
                }
                if document.is_empty() {
                    held.remove(lease.document());
                }
            }
            Ok::<_, walthamstow_kernel::KernelError>(())
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::conformance::blobs::BlobHarness;

    struct MemoryHarness;

    impl BlobHarness for MemoryHarness {
        fn store(&self, leases: Arc<dyn Leases>, kinds: BlobKinds) -> Box<dyn BlobStore> {
            Box::new(MemoryBlobStore::new(leases, kinds))
        }
    }

    crate::blob_store_conformance!(harness = MemoryHarness);
}
