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
    CollaborationImportRequest, CollaborationImportResult, CollaborationService, ImportFence,
    PeerBlock,
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

/// An accepted document and the generation accepted when the read began:
/// `value` is that generation or a later one.
pub struct AcceptedRead {
    pub value: Value,
    pub generation: u64,
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

    /// Records that the document's files render `generation`, or that no
    /// accepted document remains when it is `None`. The mark stays when a
    /// later generation has been accepted since.
    /// Whether `resource_id` waits for its files to be written from what was
    /// accepted.
    pub fn publication_pending(&self, resource_id: &str) -> bool {
        self.publications
            .as_ref()
            .is_none_or(|journal| journal.is_marked(&self.id(resource_id)))
    }

    pub fn published(&self, resource_id: &str, generation: Option<u64>) -> KernelResult<()> {
        let Some(journal) = &self.publications else {
            return Ok(());
        };
        let document = self.id(resource_id);
        journal.clear(&document, || {
            Ok(self
                .service
                .summary(&document)?
                .map(|summary| summary.generation)
                <= generation)
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
                generation: read.summary.generation,
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
    /// stored, in whatever format.
    pub fn bootstrap_update(
        &self,
        resource_id: &str,
        schema_version: u32,
        update_base64: &str,
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
            self.check_with(resource_id, check),
        )?;
        Ok(())
    }

    /// The accepted state for a peer that holds every operation up to
    /// `held_frontier_base64`, or nothing when it is `None`.
    fn accepted_state(
        &self,
        resource_id: &str,
        held_frontier_base64: Option<&str>,
    ) -> KernelResult<CollaborationAuthoringState> {
        Ok(self.service.authoring_state(
            self.resource.spec,
            &self.id(resource_id),
            held_frontier_base64,
        )?)
    }

    /// The accepted state for `actor`, who holds every operation up to
    /// `held_frontier_base64`, or nothing when it is `None`, with a new block
    /// of peers for the actor to write under.
    pub fn authoring_state(
        &self,
        resource_id: &str,
        held_frontier_base64: Option<&str>,
        actor: &Principal,
    ) -> KernelResult<AuthoringState> {
        let state = self.accepted_state(resource_id, held_frontier_base64)?;
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

    /// Removes the document, as `actor` asked.
    pub fn remove(&self, resource_id: &str, actor: &Principal) -> KernelResult<()> {
        self.service.delete(actor, &self.id(resource_id))?;
        self.published(resource_id, None)
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
