//! Copyright (c) Scott A Dixon
//!
//! A collaborative entity's documents in one store: seeding, reading, the
//! authoring state a writer edits from, importing a writer's update,
//! rewriting, removing and listing what was accepted. A product supplies each
//! entity's spec and validator, and which documents have files to publish.

use crate::publications::{PendingCommit, PublicationJournal};
use crate::reason_codes;
use clerkenwell_doc::CollaborationReplica;
use clerkenwell_events::Principal;
use clerkenwell_schema::{
    AuthoringPeerBlock, AuthoringState, CollaborationExchangeMode, GeneratedCollaborationEntitySpec,
};
use clerkenwell_store::{
    CollaborationAuthoringState, CollaborationDocumentId, CollaborationDocumentSummary,
    CollaborationHeld, CollaborationImportRequest, CollaborationImportResult, CollaborationService,
    CollaborationVersion, ImportFence, PeerBlock,
};
use serde_json::Value;
use walthamstow_kernel::{KernelError, KernelResult};

/// A product's meaning for one generated collaborative entity.
#[derive(Debug)]
pub struct CollaborativeResource {
    pub spec: &'static GeneratedCollaborationEntitySpec,
    /// What every accepted document of the entity must satisfy, given the
    /// identity it is stored under.
    validate: fn(&str, &Value) -> KernelResult<()>,
}

impl CollaborativeResource {
    /// The entity `spec` describes, whose accepted documents satisfy `validate`.
    pub const fn new(
        spec: &'static GeneratedCollaborationEntitySpec,
        validate: fn(&str, &Value) -> KernelResult<()>,
    ) -> Self {
        Self { spec, validate }
    }
}

/// One collaborative update, as an import mutation carries it.
#[derive(Debug, Clone)]
pub struct CollaborationUpdate {
    pub schema_version: u32,
    pub operation_id: String,
    pub exchange_mode: CollaborationExchangeMode,
    pub base_frontier_base64: String,
    pub update_base64: String,
    /// The nonce of every peer block the update's operations are written under.
    pub peer_nonces: Vec<String>,
    /// Why the writer made the update, when it said.
    pub intent: Option<String>,
    /// What the import is judged against when it commits.
    pub fence: ImportFence,
}

/// The peer a writer writes under, as an authoring state delivered it: the
/// first peer of its block.
pub fn delivered_peer(block: &AuthoringPeerBlock) -> KernelResult<u64> {
    block
        .base
        .parse()
        .map_err(|error| KernelError::internal(format!("Invalid peer block base: {error}")))
}

/// A replica of a document's accepted state that writes under a block of
/// peers allocated to its actor, and the state it was read from.
pub struct Writer {
    pub replica: CollaborationReplica,
    pub peers: PeerBlock,
    pub accepted: CollaborationAuthoringState,
}

impl Writer {
    /// The nonce of the block the replica's operations are written under.
    pub fn peer_nonces(&self) -> Vec<String> {
        vec![self.peers.nonce.clone()]
    }

    /// The replica's edits since the accepted state, as an update.
    pub fn incremental_update_base64(&self) -> KernelResult<String> {
        self.replica
            .export_incremental_update_base64(&self.accepted.accepted_frontier_base64)
            .map_err(|error| KernelError::internal(error.to_string()))
    }
}

/// One accepted document in a store, as a listing reads it.
#[derive(Debug)]
pub struct AcceptedCollaborationDocument {
    pub entity: String,
    pub resource_id: String,
    pub value: Option<Value>,
    pub uninspected_reason: Option<String>,
}

/// An accepted document and the version accepted when the read began:
/// `value` holds that version or a later one.
pub struct AcceptedRead {
    pub value: Value,
    pub version: CollaborationVersion,
}

/// One collaborative entity's documents in one store.
#[derive(Debug, Clone)]
pub struct CollaborativeDocuments {
    service: CollaborationService,
    resource: &'static CollaborativeResource,
    /// Marks each document until its workspace files render what was
    /// accepted; `None` for documents that have no files.
    publications: Option<PublicationJournal>,
}

impl CollaborativeDocuments {
    /// `resource`'s documents in the store `service` serves. `publications`
    /// marks each document until the files rendering it catch up with what
    /// was accepted; `None` for documents that have no files.
    pub fn new(
        service: CollaborationService,
        resource: &'static CollaborativeResource,
        publications: Option<PublicationJournal>,
    ) -> Self {
        Self {
            service,
            resource,
            publications,
        }
    }

    fn id(&self, resource_id: &str) -> CollaborationDocumentId {
        CollaborationDocumentId::new(self.resource.spec.name, resource_id)
    }

    /// Marks the document for a commit about to be made to it.
    fn pending(&self, resource_id: &str) -> KernelResult<Option<PendingCommit>> {
        self.publications
            .as_ref()
            .map(|journal| journal.mark(&self.id(resource_id)))
            .transpose()
    }

    /// Whether `resource_id` waits for its files to be written from what was
    /// accepted. Documents that have no files never wait.
    pub fn publication_pending(&self, resource_id: &str) -> bool {
        self.publications
            .as_ref()
            .is_some_and(|journal| journal.is_marked(&self.id(resource_id)))
    }

    /// Records that the document's files render `version`, or that no
    /// accepted document remains when it is `None`. The mark stays unless
    /// `version` is the one accepted now.
    pub fn published(
        &self,
        resource_id: &str,
        version: Option<&CollaborationVersion>,
    ) -> KernelResult<()> {
        let Some(journal) = &self.publications else {
            return Ok(());
        };
        let document = self.id(resource_id);
        journal.clear(&document, || {
            let accepted = self
                .service
                .summary(&document)?
                .map(|summary| summary.version);
            Ok(accepted.as_ref() == version)
        })
    }

    fn check_with(
        &self,
        resource_id: &str,
        check: impl Fn(&Value) -> KernelResult<()>,
    ) -> impl Fn(&Value) -> KernelResult<()> {
        let validate = self.resource.validate;
        let resource_id = resource_id.to_string();
        move |value| {
            validate(&resource_id, value)?;
            check(value)
        }
    }

    /// What the stored document is; `None` when there is none.
    pub fn summary(&self, resource_id: &str) -> KernelResult<Option<CollaborationDocumentSummary>> {
        Ok(self.service.summary(&self.id(resource_id))?)
    }

    /// The accepted document, materialised; `None` when there is none.
    pub fn document(&self, resource_id: &str) -> KernelResult<Option<Value>> {
        Ok(self
            .service
            .detail(self.resource.spec, &self.id(resource_id))?)
    }

    /// The accepted document as one read finds it; `None` when there is none.
    pub fn accepted_read(&self, resource_id: &str) -> KernelResult<Option<AcceptedRead>> {
        Ok(self
            .service
            .read_document(self.resource.spec, &self.id(resource_id))?
            .map(|read| AcceptedRead {
                value: read.document,
                version: read.summary.version,
            }))
    }

    /// The accepted document, or a not-found error naming it.
    pub fn require_document(&self, resource_id: &str) -> KernelResult<Value> {
        self.document(resource_id)?
            .ok_or_else(|| self.not_found(resource_id))
    }

    fn not_found(&self, resource_id: &str) -> KernelError {
        KernelError::not_found(format!(
            "No collaborative {} document exists for \"{resource_id}\".",
            self.resource.spec.name
        ))
        .with_data(serde_json::json!({
            "code": reason_codes::COLLABORATION_DOCUMENT_NOT_FOUND,
            "entity": self.resource.spec.name,
            "resource_id": resource_id,
        }))
    }

    /// Seeds a new document from `seed` unless a document is stored, in
    /// whatever format. Its file is written once it is accepted.
    pub fn bootstrap(
        &self,
        resource_id: &str,
        seed: &Value,
        check: impl Fn(&Value) -> KernelResult<()>,
    ) -> KernelResult<()> {
        if self.summary(resource_id)?.is_some() {
            return Ok(());
        }
        let _pending = self.pending(resource_id)?;
        self.seed(resource_id, seed, check)
    }

    /// Seeds the document from `seed`, its file's content, unless a document
    /// is stored, in whatever format. The file already holds what is accepted.
    pub fn bootstrap_from_file(
        &self,
        resource_id: &str,
        seed: &Value,
        check: impl Fn(&Value) -> KernelResult<()>,
    ) -> KernelResult<()> {
        if self.summary(resource_id)?.is_some() {
            return Ok(());
        }
        self.seed(resource_id, seed, check)
    }

    fn seed(
        &self,
        resource_id: &str,
        seed: &Value,
        check: impl Fn(&Value) -> KernelResult<()>,
    ) -> KernelResult<()> {
        self.service.bootstrap(
            self.resource.spec,
            &self.id(resource_id),
            seed,
            self.check_with(resource_id, check),
        )?;
        Ok(())
    }

    /// Seeds the document from a writer's full update unless a document is
    /// stored, in whatever format. `incarnation` names the history the update
    /// comes from, as another store that holds it reads it; `None` gives the
    /// document a new one.
    pub fn bootstrap_update(
        &self,
        resource_id: &str,
        schema_version: u32,
        update_base64: &str,
        incarnation: Option<&str>,
        check: impl Fn(&Value) -> KernelResult<()>,
    ) -> KernelResult<()> {
        if self.summary(resource_id)?.is_some() {
            return Ok(());
        }
        let _pending = self.pending(resource_id)?;
        self.service.bootstrap_update(
            self.resource.spec,
            &self.id(resource_id),
            schema_version,
            update_base64,
            incarnation,
            self.check_with(resource_id, check),
        )?;
        Ok(())
    }

    /// The accepted state for a peer that holds what `held` names, or nothing
    /// when it is `None`.
    fn accepted_state(
        &self,
        resource_id: &str,
        held: Option<CollaborationHeld<'_>>,
    ) -> KernelResult<CollaborationAuthoringState> {
        Ok(self
            .service
            .authoring_state(self.resource.spec, &self.id(resource_id), held)?)
    }

    /// The accepted state for `actor`, who holds what `held` names, or
    /// nothing when it is `None`, with a new block of peers for the actor to
    /// write under.
    pub fn authoring_state(
        &self,
        resource_id: &str,
        held: Option<CollaborationHeld<'_>>,
        actor: &Principal,
    ) -> KernelResult<AuthoringState> {
        let state = self.accepted_state(resource_id, held)?;
        let peers = self.service.allocate_peers(actor, &self.id(resource_id));
        Ok(state.into_authoring_state(peers))
    }

    /// A replica of the accepted document for `actor` to edit and commit.
    pub fn writer(&self, actor: &Principal, resource_id: &str) -> KernelResult<Writer> {
        let accepted = self.accepted_state(resource_id, None)?;
        let peers = self.service.allocate_peers(actor, &self.id(resource_id));
        let replica = CollaborationReplica::from_versioned_update_base64(
            self.resource.spec,
            accepted.schema_version,
            &accepted.update_base64,
        )
        .map_err(|error| KernelError::internal(error.to_string()))?;
        replica
            .set_peer(peers.base)
            .map_err(|error| KernelError::internal(error.to_string()))?;
        Ok(Writer {
            replica,
            peers,
            accepted,
        })
    }

    /// Commits `actor`'s update, fenced as the writer asked, and checked by
    /// the entity and then by `check`.
    pub fn import(
        &self,
        resource_id: &str,
        actor: &Principal,
        update: CollaborationUpdate,
        check: impl Fn(&Value) -> KernelResult<()>,
    ) -> KernelResult<CollaborationImportResult> {
        let _pending = self.pending(resource_id)?;
        self.service.import(
            self.resource.spec,
            CollaborationImportRequest {
                document: self.id(resource_id),
                schema_version: update.schema_version,
                operation_id: update.operation_id,
                exchange_mode: update.exchange_mode,
                base_frontier_base64: update.base_frontier_base64,
                update_base64: update.update_base64,
                actor: actor.clone(),
                peer_nonces: update.peer_nonces,
                intent: update.intent,
                fence: update.fence,
            },
            self.check_with(resource_id, check),
        )
    }

    /// Removes the document, as `actor` asked, and returns the version it
    /// removed; `None` when there was none to remove. Its mark stays until
    /// [`published`](Self::published) records that its files are gone too.
    pub fn remove(
        &self,
        resource_id: &str,
        actor: &Principal,
    ) -> KernelResult<Option<CollaborationVersion>> {
        let _pending = self.pending(resource_id)?;
        Ok(self.service.delete(actor, &self.id(resource_id))?)
    }

    /// Commits `actor`'s edit to the accepted document, as any writer's edit is
    /// committed, recording `intent` as why. `edit` reports whether it changed
    /// anything; an unchanged document commits nothing and returns `None`.
    pub fn rewrite_as(
        &self,
        actor: &Principal,
        resource_id: &str,
        operation_id: &str,
        intent: &str,
        edit: impl FnOnce(&mut Value) -> bool,
    ) -> KernelResult<Option<CollaborationImportResult>> {
        let mut writer = self.writer(actor, resource_id)?;
        let mut desired = writer
            .replica
            .materialized_document(&writer.accepted.etag)
            .map_err(|error| KernelError::internal(error.to_string()))?;
        if !edit(&mut desired) {
            return Ok(None);
        }
        writer
            .replica
            .replace_document(&desired)
            .map_err(|error| KernelError::internal(error.to_string()))?;
        let update_base64 = writer.incremental_update_base64()?;
        let peer_nonces = writer.peer_nonces();
        let _pending = self.pending(resource_id)?;
        self.service
            .import(
                self.resource.spec,
                CollaborationImportRequest {
                    document: self.id(resource_id),
                    schema_version: self.resource.spec.schema_version,
                    operation_id: operation_id.to_string(),
                    exchange_mode: CollaborationExchangeMode::Incremental,
                    base_frontier_base64: writer.accepted.accepted_frontier_base64,
                    update_base64,
                    actor: actor.clone(),
                    peer_nonces,
                    intent: Some(intent.to_string()),
                    fence: ImportFence::Frontier,
                },
                self.check_with(resource_id, |_| Ok(())),
            )
            .map(Some)
    }

    /// Every accepted document in the store. One whose schema version this
    /// build does not serve is reported with a reason rather than
    /// disappearing, so a reader can tell "nothing there" from "could not be
    /// read".
    pub fn accepted(&self) -> KernelResult<Vec<AcceptedCollaborationDocument>> {
        let spec = self.resource.spec;
        self.service
            .summaries(spec.name)?
            .into_iter()
            .map(|summary| {
                let (value, uninspected_reason) = if summary.schema_version != spec.schema_version {
                    (
                        None,
                        Some(format!(
                            "Unsupported {} collaboration schema version {}; expected {}.",
                            spec.name, summary.schema_version, spec.schema_version
                        )),
                    )
                } else {
                    match self.document(&summary.document.resource_id) {
                        Ok(Some(value)) => (Some(value), None),
                        Ok(None) => (
                            None,
                            Some(
                                "Accepted collaboration document disappeared while being read."
                                    .to_string(),
                            ),
                        ),
                        Err(error) => (None, Some(error.message)),
                    }
                };
                Ok(AcceptedCollaborationDocument {
                    entity: spec.name.to_string(),
                    resource_id: summary.document.resource_id,
                    value,
                    uninspected_reason,
                })
            })
            .collect()
    }

    /// Validates one stored document the build serves.
    pub fn validate_accepted(&self, summary: &CollaborationDocumentSummary) -> KernelResult<()> {
        if summary.schema_version != self.resource.spec.schema_version {
            return Ok(());
        }
        let resource_id = summary.document.resource_id.as_str();
        let value = self.document(resource_id)?.ok_or_else(|| {
            KernelError::internal(format!(
                "Collaboration state for {} \"{resource_id}\" disappeared during load.",
                self.resource.spec.name
            ))
        })?;
        (self.resource.validate)(resource_id, &value)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use clerkenwell_events::ActorKind;
    use clerkenwell_schema::{
        GeneratedCollaborationConflict, GeneratedCollaborationFieldSpec,
        GeneratedCollaborationStorageKind, GeneratedCollaborationValueCodec,
    };
    use clerkenwell_store::{CommitPolicy, PeerKey};
    use std::num::NonZeroU32;
    use std::time::Duration;
    use walthamstow_kernel::collaboration_storage::FileCollaborationStorage;
    use walthamstow_kernel::durability::WriteDurability;
    use walthamstow_kernel::local_store::FileLocalStore;
    use walthamstow_kernel::WallClock;

    const fn field(
        path: &'static str,
        storage_kind: GeneratedCollaborationStorageKind,
        container: Option<&'static str>,
        key: Option<&'static str>,
        conflict: GeneratedCollaborationConflict,
    ) -> GeneratedCollaborationFieldSpec {
        GeneratedCollaborationFieldSpec {
            path,
            storage_kind,
            container,
            container_template: None,
            key,
            identity_path: None,
            identity_variable: None,
            order_container: None,
            item_container_template: None,
            metadata_container: None,
            metadata_container_template: None,
            metadata_key: None,
            codec: GeneratedCollaborationValueCodec::String,
            value_schema: None,
            required: true,
            required_in_parent: true,
            conflict,
            writers: &[],
        }
    }

    static NOTE_FIELDS: &[GeneratedCollaborationFieldSpec] = &[
        field(
            "note_id",
            GeneratedCollaborationStorageKind::Scalar,
            Some("note"),
            Some("note_id"),
            GeneratedCollaborationConflict::Immutable,
        ),
        field(
            "body",
            GeneratedCollaborationStorageKind::Text,
            Some("body"),
            None,
            GeneratedCollaborationConflict::Merge,
        ),
        field(
            "etag",
            GeneratedCollaborationStorageKind::DerivedRevision,
            None,
            None,
            GeneratedCollaborationConflict::Immutable,
        ),
    ];

    static NOTE_PLANS: &[GeneratedCollaborationEntitySpec] = &[GeneratedCollaborationEntitySpec {
        name: "Note",
        id_field: "note_id",
        schema_version: 1,
        authoring_projection: "notes.authoringState",
        authoring_document: None,
        authoring_store_params: &[],
        import_mutation: "note.importUpdate",
        root_container: "note",
        fields: NOTE_FIELDS,
    }];

    fn accept(_: &str, _: &Value) -> KernelResult<()> {
        Ok(())
    }

    static NOTES: CollaborativeResource = CollaborativeResource::new(&NOTE_PLANS[0], accept);

    const DURABILITY: WriteDurability = WriteDurability::System;

    fn service(root: &std::path::Path) -> CollaborationService {
        CollaborationService::new(
            FileCollaborationStorage::new(
                root,
                DURABILITY,
                WallClock::new(|| std::time::UNIX_EPOCH),
            )
            .unwrap(),
            NOTE_PLANS,
            CommitPolicy {
                retained_operations: 8,
                attempts: NonZeroU32::MIN,
                backoff: Duration::ZERO,
            },
            PeerKey::new([7; 32]).unwrap(),
            "walthamstow-test",
        )
    }

    fn writer() -> Principal {
        Principal::new("writer", ActorKind::Human)
    }

    fn note(id: &str) -> Value {
        serde_json::json!({ "note_id": id, "body": "text" })
    }

    #[test]
    fn a_removed_document_waits_for_its_files_until_they_are_published_gone() {
        let store = tempfile::tempdir().unwrap();
        let data = tempfile::tempdir().unwrap();
        let journal = || {
            PublicationJournal::open(FileLocalStore::open(data.path(), DURABILITY).unwrap())
                .unwrap()
        };
        let documents = CollaborativeDocuments::new(service(store.path()), &NOTES, Some(journal()));
        documents
            .bootstrap("one", &note("one"), |_| Ok(()))
            .unwrap();
        let version = documents.summary("one").unwrap().unwrap().version;
        documents.published("one", Some(&version)).unwrap();
        assert!(!documents.publication_pending("one"));

        documents.remove("one", &writer()).unwrap();
        assert!(documents.summary("one").unwrap().is_none());
        assert!(
            documents.publication_pending("one"),
            "the removed document's files are not yet known to be gone"
        );
        assert!(
            journal().is_marked(&CollaborationDocumentId::new("Note", "one")),
            "the mark is found again after a restart"
        );

        documents.published("one", None).unwrap();
        assert!(!documents.publication_pending("one"));
        assert!(!journal().is_marked(&CollaborationDocumentId::new("Note", "one")));
    }

    #[test]
    fn a_stale_publication_finishing_after_a_removal_keeps_the_removal_pending() {
        let store = tempfile::tempdir().unwrap();
        let data = tempfile::tempdir().unwrap();
        let journal =
            PublicationJournal::open(FileLocalStore::open(data.path(), DURABILITY).unwrap())
                .unwrap();
        let documents = CollaborativeDocuments::new(service(store.path()), &NOTES, Some(journal));
        documents
            .bootstrap("one", &note("one"), |_| Ok(()))
            .unwrap();
        let stale = documents.summary("one").unwrap().unwrap().version;
        assert_eq!(
            documents.remove("one", &writer()).unwrap().as_ref(),
            Some(&stale),
            "a removal returns the version it removed"
        );

        documents.published("one", Some(&stale)).unwrap();
        assert!(
            documents.publication_pending("one"),
            "files written from the removed document do not settle its removal"
        );
        documents.published("one", None).unwrap();
        assert!(!documents.publication_pending("one"));
    }

    #[test]
    fn documents_that_have_no_files_never_wait_for_publication() {
        let store = tempfile::tempdir().unwrap();
        let documents = CollaborativeDocuments::new(service(store.path()), &NOTES, None);
        assert!(!documents.publication_pending("one"));
        documents
            .bootstrap("one", &note("one"), |_| Ok(()))
            .unwrap();
        assert!(!documents.publication_pending("one"));
        documents.remove("one", &writer()).unwrap();
        assert!(!documents.publication_pending("one"));
    }

    #[test]
    fn a_publication_clears_the_mark_only_at_the_version_accepted() {
        let store = tempfile::tempdir().unwrap();
        let data = tempfile::tempdir().unwrap();
        let journal =
            PublicationJournal::open(FileLocalStore::open(data.path(), DURABILITY).unwrap())
                .unwrap();
        let documents = CollaborativeDocuments::new(service(store.path()), &NOTES, Some(journal));
        documents
            .bootstrap("one", &note("one"), |_| Ok(()))
            .unwrap();
        let earlier = documents.summary("one").unwrap().unwrap().version;
        documents.remove("one", &writer()).unwrap();
        documents
            .bootstrap("one", &note("one"), |_| Ok(()))
            .unwrap();
        let accepted = documents.summary("one").unwrap().unwrap().version;
        assert_ne!(
            accepted, earlier,
            "a document created again has a new history"
        );

        documents.published("one", Some(&earlier)).unwrap();
        assert!(
            documents.publication_pending("one"),
            "files rendering the removed document's history do not render this one"
        );
        documents.published("one", Some(&accepted)).unwrap();
        assert!(!documents.publication_pending("one"));
    }
}
