//! Copyright (c) Scott A Dixon
//!
//! A canonical store kept in memory: each document's version, its operations and its parts,
//! fenced by [`MemoryLeases`].

use super::{
    distinct, lapsed, names_document, rendered, CanonicalStore, Part, Publication, Pulled,
};
use async_trait::async_trait;
use clerkenwell_store::{CollaborationDocumentId, CollaborationVersion};
use std::collections::BTreeMap;
use std::sync::{Arc, Mutex, MutexGuard, PoisonError};
use walthamstow_kernel::leases::{Lease, MemoryLeases};
use walthamstow_kernel::KernelResult;

/// Documents kept in memory, written only while a [`MemoryLeases`] lease holds them.
#[derive(Debug)]
pub struct MemoryCanonicalStore {
    leases: Arc<MemoryLeases>,
    held: Mutex<BTreeMap<(String, String), Pulled>>,
}

fn key(document: &CollaborationDocumentId) -> (String, String) {
    (document.entity.clone(), document.resource_id.clone())
}

impl MemoryCanonicalStore {
    /// A store whose writes `leases` fence.
    pub fn new(leases: Arc<MemoryLeases>) -> Self {
        Self {
            leases,
            held: Mutex::new(BTreeMap::new()),
        }
    }

    fn held(&self) -> MutexGuard<'_, BTreeMap<(String, String), Pulled>> {
        self.held.lock().unwrap_or_else(PoisonError::into_inner)
    }

    /// Writes `parts` as `document` as a writer outside any engine would: the store holds
    /// them with no version and no operations.
    pub fn write_outside(&self, document: &CollaborationDocumentId, parts: Vec<Part>) {
        self.held().insert(
            key(document),
            Pulled {
                version: None,
                operations: None,
                parts,
            },
        );
    }
}

#[async_trait]
impl CanonicalStore for MemoryCanonicalStore {
    async fn publish(
        &self,
        document: &CollaborationDocumentId,
        lease: &Lease,
        publication: &Publication,
    ) -> KernelResult<CollaborationVersion> {
        names_document(document, lease)?;
        rendered(&publication.parts)?;
        self.admit(document, &publication.parts)?;
        self.leases
            .while_held(lease, || {
                self.held().insert(
                    key(document),
                    Pulled {
                        version: Some(publication.version.clone()),
                        operations: Some(publication.operations.clone()),
                        parts: publication.parts.clone(),
                    },
                );
            })
            .ok_or_else(|| lapsed(document))?;
        Ok(publication.version.clone())
    }

    async fn pull(&self, document: &CollaborationDocumentId) -> KernelResult<Option<Pulled>> {
        Ok(self.held().get(&key(document)).cloned())
    }

    async fn list(&self, entity: &str) -> KernelResult<Vec<CollaborationDocumentId>> {
        Ok(self
            .held()
            .keys()
            .filter(|(held, _)| held == entity)
            .map(|(entity, resource_id)| CollaborationDocumentId::new(entity, resource_id))
            .collect())
    }

    fn admit(&self, _document: &CollaborationDocumentId, parts: &[Part]) -> KernelResult<()> {
        distinct(parts)
    }

    async fn remove(&self, document: &CollaborationDocumentId, lease: &Lease) -> KernelResult<()> {
        names_document(document, lease)?;
        self.leases
            .while_held(lease, || {
                self.held().remove(&key(document));
            })
            .ok_or_else(|| lapsed(document))
    }
}

#[cfg(test)]
mod tests {
    use super::super::tests::{document, holder, holds_the_canonical_store_contract, part};
    use super::*;
    use std::time::{Duration, UNIX_EPOCH};
    use walthamstow_kernel::WallClock;

    fn leases() -> Arc<MemoryLeases> {
        Arc::new(MemoryLeases::new(
            WallClock::new(|| UNIX_EPOCH),
            Duration::from_secs(60),
        ))
    }

    #[tokio::test]
    async fn memory_documents_hold_the_canonical_store_contract() {
        let leases = leases();
        let store = MemoryCanonicalStore::new(leases.clone());
        holds_the_canonical_store_contract(&store, leases.as_ref(), &holder("engine")).await;
    }

    #[tokio::test]
    async fn parts_written_outside_an_engine_are_held_with_no_version() {
        let store = MemoryCanonicalStore::new(leases());
        let parts = vec![part("one.yaml", "one\nedited")];
        store.write_outside(&document("one"), parts.clone());
        let pulled = store.pull(&document("one")).await.unwrap().unwrap();
        assert_eq!(pulled.version, None);
        assert_eq!(pulled.operations, None);
        assert_eq!(pulled.parts, parts);
    }
}
