//! Copyright (c) Scott A Dixon
//!
//! How the engine names a document: by its entity and its resource. Leases, log documents and
//! blobs name a document this way, and the name converts to and from Clerkenwell's.

use clerkenwell_store::CollaborationDocumentId;
use std::fmt;

/// One document, named by its entity and resource.
#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct DocumentId {
    entity: String,
    resource: String,
}

impl DocumentId {
    pub fn new(entity: impl Into<String>, resource: impl Into<String>) -> Self {
        Self {
            entity: entity.into(),
            resource: resource.into(),
        }
    }

    pub fn entity(&self) -> &str {
        &self.entity
    }

    pub fn resource(&self) -> &str {
        &self.resource
    }
}

impl fmt::Display for DocumentId {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(formatter, "{} {:?}", self.entity, self.resource)
    }
}

impl From<CollaborationDocumentId> for DocumentId {
    fn from(document: CollaborationDocumentId) -> Self {
        Self::new(document.entity, document.resource_id)
    }
}

impl From<DocumentId> for CollaborationDocumentId {
    fn from(document: DocumentId) -> Self {
        Self::new(document.entity, document.resource)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_document_id_converts_to_clerkenwells_and_back() {
        let document = DocumentId::new("Session", "turn/1");
        let clerkenwell = CollaborationDocumentId::from(document.clone());
        assert_eq!(
            (
                clerkenwell.entity.as_str(),
                clerkenwell.resource_id.as_str()
            ),
            ("Session", "turn/1")
        );
        assert_eq!(DocumentId::from(clerkenwell), document);
    }
}
