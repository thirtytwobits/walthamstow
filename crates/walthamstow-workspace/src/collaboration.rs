//! Copyright (c) Scott A Dixon
//!
//! A collaborative entity's documents in one store: seeding, reading, the
//! authoring state a writer edits from, importing a writer's update,
//! rewriting, removing, listing what was accepted, and publishing it to the
//! canonical store. A product supplies each entity's spec and validator, and the
//! codec that renders its documents for the canonical store.

use crate::canonical::{DocumentCodec, Operations, Part, Publication, Pulled};
use crate::outbox::{Outbox, PendingCommit};
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
use std::sync::Arc;
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

/// How a collaborative entity's documents reach the canonical store: the outbox they wait in,
/// and the codec that renders them.
#[derive(Debug, Clone)]
pub struct Publishing {
    pub outbox: Outbox,
    pub codec: Arc<dyn DocumentCodec>,
}

/// What publishing one document did.
#[derive(Debug, Clone)]
pub struct Published {
    /// The version the canonical store holds now; `None` when the document was removed.
    pub version: Option<CollaborationVersion>,
    /// The accepted state published; `None` for a removal.
    pub state: Option<Value>,
    /// The parts the codec rendered it into; none for a removal.
    pub parts: Vec<Part>,
}

/// One collaborative entity's documents in one store.
#[derive(Debug, Clone)]
pub struct CollaborativeDocuments {
    service: CollaborationService,
    resource: &'static CollaborativeResource,
    /// How each document reaches the canonical store; `None` for documents
    /// that are kept nowhere else.
    publishing: Option<Publishing>,
}

impl CollaborativeDocuments {
    /// `resource`'s documents in the store `service` serves. `publishing`
    /// marks each document until the canonical store holds what was accepted,
    /// and renders it there; `None` for documents kept nowhere else.
    pub fn new(
        service: CollaborationService,
        resource: &'static CollaborativeResource,
        publishing: Option<Publishing>,
    ) -> Self {
        Self {
            service,
            resource,
            publishing,
        }
    }

    fn id(&self, resource_id: &str) -> CollaborationDocumentId {
        CollaborationDocumentId::new(self.resource.spec.name, resource_id)
    }

    /// Marks the document for a commit about to be made to it.
    fn pending(&self, resource_id: &str) -> KernelResult<Option<PendingCommit>> {
        self.publishing
            .as_ref()
            .map(|publishing| publishing.outbox.mark(&self.id(resource_id)))
            .transpose()
    }

    /// Whether `resource_id` waits for the canonical store to hold what was
    /// accepted. Documents kept nowhere else never wait.
    pub fn publication_pending(&self, resource_id: &str) -> bool {
        self.publishing
            .as_ref()
            .is_some_and(|publishing| publishing.outbox.is_marked(&self.id(resource_id)))
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

    /// Seeds the document from what the canonical store holds of it unless a
    /// document is stored, in whatever format: from its operations, under the
    /// store's incarnation, when the store keeps them, and otherwise from its
    /// parts. A document seeded from parts holds a version the store does not
    /// name, so it waits to be published.
    pub fn bootstrap_pulled(
        &self,
        resource_id: &str,
        pulled: &Pulled,
        check: impl Fn(&Value) -> KernelResult<()>,
    ) -> KernelResult<()> {
        if self.summary(resource_id)?.is_some() {
            return Ok(());
        }
        let (Some(operations), Some(version)) = (&pulled.operations, &pulled.version) else {
            let seed = self.parse(resource_id, &pulled.parts)?;
            let _pending = self.pending(resource_id)?;
            return self.seed(resource_id, &seed, check);
        };
        self.service.bootstrap_update(
            self.resource.spec,
            &self.id(resource_id),
            operations.schema_version,
            &operations.update_base64,
            Some(&version.incarnation),
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
    /// removed; `None` when there was none to remove. Its mark records the
    /// version before the removal, and again if a commit landed meanwhile,
    /// and stays until [`publish`](Self::publish) removes it from the
    /// canonical store too.
    pub fn remove(
        &self,
        resource_id: &str,
        actor: &Principal,
    ) -> KernelResult<Option<CollaborationVersion>> {
        let pending = self.pending(resource_id)?;
        let removes = |version: &CollaborationVersion| {
            pending
                .as_ref()
                .map_or(Ok(()), |pending| pending.removes(version))
        };
        if let Some(summary) = self.summary(resource_id)? {
            removes(&summary.version)?;
        }
        let removed = self.service.delete(actor, &self.id(resource_id))?;
        if let Some(removed) = &removed {
            removes(removed)?;
        }
        Ok(removed)
    }

    /// The publishing documents of this entity have; an error for documents
    /// kept nowhere else.
    fn publishing(&self) -> KernelResult<&Publishing> {
        self.publishing.as_ref().ok_or_else(|| {
            KernelError::internal(format!(
                "{} documents are kept in no canonical store.",
                self.resource.spec.name
            ))
        })
    }

    /// What the canonical store holds of the document; `None` when it holds
    /// none.
    pub async fn pull(&self, resource_id: &str) -> KernelResult<Option<Pulled>> {
        self.publishing()?
            .outbox
            .canonical()
            .pull(&self.id(resource_id))
            .await
    }

    /// The state `parts` render, as the entity's codec reads them.
    pub fn parse(&self, resource_id: &str, parts: &[Part]) -> KernelResult<Value> {
        self.publishing()?.codec.parse(&self.id(resource_id), parts)
    }

    /// Publishes the document's accepted state to the canonical store, or its
    /// removal when none is accepted, and clears its mark once no commit to it
    /// is under way and the store holds the version accepted now. The lease is
    /// taken for the write alone, once the document is rendered. Publications
    /// of one document land in the order they read it. `None` for documents
    /// kept nowhere else.
    pub async fn publish(&self, resource_id: &str) -> KernelResult<Option<Published>> {
        let Some(publishing) = &self.publishing else {
            return Ok(None);
        };
        let document = self.id(resource_id);
        let outbox = &publishing.outbox;
        let canonical = outbox.canonical();
        let _turn = outbox.turn(&document).await;
        if self.summary(resource_id)?.is_none() {
            // The store loses only a document this working copy removed.
            if let Some(removed) = outbox.removed(&document)? {
                let lease = outbox.lease(&document).await?;
                let result = self
                    .remove_under(&document, &lease, &removed, canonical.as_ref())
                    .await;
                outbox.release(&lease).await?;
                result?;
            }
            outbox.clear(&document, || Ok(self.summary(resource_id)?.is_none()))?;
            return Ok(Some(Published {
                version: None,
                state: None,
                parts: Vec::new(),
            }));
        }
        let accepted = self.accepted_state(resource_id, None)?;
        let state = CollaborationReplica::from_versioned_update_base64(
            self.resource.spec,
            accepted.schema_version,
            &accepted.update_base64,
        )
        .and_then(|replica| replica.materialized_document(&accepted.etag))
        .map_err(|error| KernelError::internal(error.to_string()))?;
        let held = canonical
            .pull(&document)
            .await?
            .map(|pulled| pulled.parts)
            .unwrap_or_default();
        let parts = publishing.codec.render(&document, &state, &held).await?;
        let publication = Publication {
            version: accepted.version,
            operations: Operations {
                schema_version: accepted.schema_version,
                update_base64: accepted.update_base64,
            },
            parts,
        };
        let lease = outbox.lease(&document).await?;
        let published = self
            .publish_under(&document, &lease, &publication, canonical.as_ref())
            .await;
        outbox.release(&lease).await?;
        let version = published?;
        outbox.clear(&document, || {
            Ok(self.summary(resource_id)?.map(|summary| summary.version) == Some(version.clone()))
        })?;
        Ok(Some(Published {
            version: Some(version),
            state: Some(state),
            parts: publication.parts,
        }))
    }

    /// Publishes `publication` while `lease` holds the document, unless the store holds
    /// operations of the document's history that the publication lacks: another holder
    /// published them after this one read its state.
    async fn publish_under(
        &self,
        document: &CollaborationDocumentId,
        lease: &walthamstow_kernel::leases::Lease,
        publication: &Publication,
        canonical: &dyn crate::canonical::CanonicalStore,
    ) -> KernelResult<CollaborationVersion> {
        let held = canonical
            .pull(document)
            .await?
            .and_then(|pulled| pulled.version);
        if held.is_some_and(|held| {
            held.incarnation == publication.version.incarnation
                && !publication.version.covers(&held)
        }) {
            return Err(KernelError::conflict(format!(
                "The canonical store holds {} \"{}\" operations this working copy lacks; pull \
                 them before publishing.",
                document.entity, document.resource_id
            )));
        }
        canonical.publish(document, lease, publication).await
    }

    /// Removes the document from the store while `lease` holds it, unless the store holds
    /// operations of its history that `removed`, the version this working copy removed, lacks:
    /// another holder published them after this one removed it.
    async fn remove_under(
        &self,
        document: &CollaborationDocumentId,
        lease: &walthamstow_kernel::leases::Lease,
        removed: &CollaborationVersion,
        canonical: &dyn crate::canonical::CanonicalStore,
    ) -> KernelResult<()> {
        let held = canonical
            .pull(document)
            .await?
            .and_then(|pulled| pulled.version);
        if held
            .is_some_and(|held| held.incarnation == removed.incarnation && !removed.covers(&held))
        {
            return Err(KernelError::conflict(format!(
                "The canonical store holds {} \"{}\" operations published after this working copy \
                 removed it; pull them before removing it.",
                document.entity, document.resource_id
            )));
        }
        canonical.remove(document, lease).await
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
    use crate::canonical::tests::NoteCodec;
    use crate::canonical::{CanonicalStore, DocumentCodec, MemoryCanonicalStore};
    use clerkenwell_events::ActorKind;
    use clerkenwell_schema::{
        GeneratedCollaborationConflict, GeneratedCollaborationFieldSpec,
        GeneratedCollaborationStorageKind, GeneratedCollaborationValueCodec,
    };
    use clerkenwell_store::{CommitPolicy, PeerKey};
    use std::num::NonZeroU32;
    use std::sync::OnceLock;
    use std::time::Duration;
    use walthamstow_kernel::collaboration_storage::FileCollaborationStorage;
    use walthamstow_kernel::durability::WriteDurability;
    use walthamstow_kernel::leases::{Holder, MemoryLeases};
    use walthamstow_kernel::local_store::{LocalStore, MemoryLocalStore};
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

    fn leases() -> Arc<MemoryLeases> {
        Arc::new(MemoryLeases::new(
            WallClock::new(|| std::time::UNIX_EPOCH),
            Duration::from_secs(60),
        ))
    }

    /// One engine's working copy of notes, publishing to `canonical`.
    struct Engine {
        _store: tempfile::TempDir,
        outbox: Outbox,
        documents: CollaborativeDocuments,
    }

    fn engine(
        canonical: Arc<MemoryCanonicalStore>,
        leases: Arc<MemoryLeases>,
        codec: Arc<dyn DocumentCodec>,
    ) -> Engine {
        let store = tempfile::tempdir().unwrap();
        let local: Arc<dyn LocalStore> = Arc::new(MemoryLocalStore::new());
        let outbox =
            Outbox::open(local, canonical, leases, Holder::new("engine").unwrap()).unwrap();
        let documents = CollaborativeDocuments::new(
            service(store.path()),
            &NOTES,
            Some(Publishing {
                outbox: outbox.clone(),
                codec,
            }),
        );
        Engine {
            _store: store,
            outbox,
            documents,
        }
    }

    fn notes() -> (Arc<MemoryCanonicalStore>, Engine) {
        let leases = leases();
        let canonical = Arc::new(MemoryCanonicalStore::new(leases.clone()));
        let engine = engine(canonical.clone(), leases, Arc::new(NoteCodec));
        (canonical, engine)
    }

    fn id(resource_id: &str) -> CollaborationDocumentId {
        CollaborationDocumentId::new("Note", resource_id)
    }

    #[tokio::test]
    async fn a_published_document_is_held_at_the_version_accepted() {
        let (canonical, engine) = notes();
        let documents = &engine.documents;
        documents
            .bootstrap("one", &note("one"), |_| Ok(()))
            .unwrap();
        assert!(documents.publication_pending("one"));

        let published = documents.publish("one").await.unwrap().unwrap();
        let accepted = documents.summary("one").unwrap().unwrap();
        assert_eq!(published.version.as_ref(), Some(&accepted.version));
        let state = published.state.expect("a published document's state");
        assert_eq!(
            (&state["note_id"], &state["body"]),
            (&note("one")["note_id"], &note("one")["body"])
        );
        assert!(!documents.publication_pending("one"));

        let held = canonical.pull(&id("one")).await.unwrap().unwrap();
        assert_eq!(held.version, Some(accepted.version));
        assert_eq!(held.parts, published.parts);
        assert_eq!(
            documents.parse("one", &held.parts).unwrap()["body"],
            note("one")["body"],
            "the codec reads back what it rendered"
        );
        assert!(held.operations.is_some());
    }

    #[tokio::test]
    async fn a_removed_document_waits_until_its_removal_is_published() {
        let (canonical, engine) = notes();
        let documents = &engine.documents;
        documents
            .bootstrap("one", &note("one"), |_| Ok(()))
            .unwrap();
        documents.publish("one").await.unwrap();

        let removed = documents.remove("one", &writer()).unwrap();
        assert!(
            removed.is_some(),
            "a removal returns the version it removed"
        );
        assert!(documents.publication_pending("one"));
        assert!(canonical.pull(&id("one")).await.unwrap().is_some());

        let published = documents.publish("one").await.unwrap().unwrap();
        assert_eq!(published.version, None);
        assert!(!documents.publication_pending("one"));
        assert_eq!(canonical.pull(&id("one")).await.unwrap(), None);
    }

    #[tokio::test]
    async fn a_commit_under_way_keeps_the_mark_through_a_publication() {
        let (_, engine) = notes();
        let documents = &engine.documents;
        documents
            .bootstrap("one", &note("one"), |_| Ok(()))
            .unwrap();
        let under_way = engine.outbox.mark(&id("one")).unwrap();
        documents.publish("one").await.unwrap();
        assert!(documents.publication_pending("one"));
        drop(under_way);
        documents.publish("one").await.unwrap();
        assert!(!documents.publication_pending("one"));
    }

    #[tokio::test]
    async fn documents_kept_nowhere_else_never_wait_for_publication() {
        let store = tempfile::tempdir().unwrap();
        let documents = CollaborativeDocuments::new(service(store.path()), &NOTES, None);
        documents
            .bootstrap("one", &note("one"), |_| Ok(()))
            .unwrap();
        assert!(!documents.publication_pending("one"));
        assert!(documents.publish("one").await.unwrap().is_none());
        documents.remove("one", &writer()).unwrap();
        assert!(!documents.publication_pending("one"));
    }

    /// Notes rendered as [`NoteCodec`] renders them, after a change to the document has
    /// landed while it renders.
    #[derive(Debug)]
    struct Racing {
        documents: OnceLock<CollaborativeDocuments>,
        removes: bool,
        raced: std::sync::atomic::AtomicBool,
    }

    #[async_trait::async_trait]
    impl DocumentCodec for Racing {
        async fn render(
            &self,
            document: &CollaborationDocumentId,
            state: &Value,
            held: &[Part],
        ) -> KernelResult<Vec<Part>> {
            let documents = self.documents.get().expect("the documents are set");
            if !self.raced.swap(true, std::sync::atomic::Ordering::SeqCst) {
                if self.removes {
                    documents.remove(&document.resource_id, &writer())?;
                } else {
                    documents.rewrite_as(
                        &writer(),
                        &document.resource_id,
                        "racing",
                        "change while rendering",
                        |value| {
                            value["body"] = Value::String("changed".into());
                            true
                        },
                    )?;
                }
            }
            NoteCodec.render(document, state, held).await
        }

        fn parse(&self, document: &CollaborationDocumentId, parts: &[Part]) -> KernelResult<Value> {
            NoteCodec.parse(document, parts)
        }

        fn resource_of(&self, part: &Part) -> KernelResult<Option<String>> {
            NoteCodec.resource_of(part)
        }
    }

    #[tokio::test]
    async fn a_change_landing_while_a_publication_renders_keeps_the_document_waiting() {
        for removes in [false, true] {
            let leases = leases();
            let canonical = Arc::new(MemoryCanonicalStore::new(leases.clone()));
            let codec = Arc::new(Racing {
                documents: OnceLock::new(),
                removes,
                raced: Default::default(),
            });
            let engine = engine(canonical.clone(), leases, codec.clone());
            codec.documents.set(engine.documents.clone()).unwrap();
            let documents = &engine.documents;
            documents
                .bootstrap("one", &note("one"), |_| Ok(()))
                .unwrap();

            let stale = documents.publish("one").await.unwrap().unwrap();
            assert!(
                documents.publication_pending("one"),
                "the store holds a version the change has moved past ({removes})"
            );
            assert_ne!(
                stale.version,
                documents
                    .summary("one")
                    .unwrap()
                    .map(|summary| summary.version)
            );

            documents.publish("one").await.unwrap();
            assert!(!documents.publication_pending("one"), "{removes}");
            assert_eq!(
                canonical
                    .pull(&id("one"))
                    .await
                    .unwrap()
                    .and_then(|held| held.version),
                documents
                    .summary("one")
                    .unwrap()
                    .map(|summary| summary.version)
            );
        }
    }

    /// Notes rendered as [`NoteCodec`] renders them, after the engine's clock has moved past a
    /// lease's term.
    #[derive(Debug)]
    struct Slow {
        now: Arc<std::sync::atomic::AtomicU64>,
    }

    #[async_trait::async_trait]
    impl DocumentCodec for Slow {
        async fn render(
            &self,
            document: &CollaborationDocumentId,
            state: &Value,
            held: &[Part],
        ) -> KernelResult<Vec<Part>> {
            self.now
                .fetch_add(120_000, std::sync::atomic::Ordering::SeqCst);
            NoteCodec.render(document, state, held).await
        }

        fn parse(&self, document: &CollaborationDocumentId, parts: &[Part]) -> KernelResult<Value> {
            NoteCodec.parse(document, parts)
        }

        fn resource_of(&self, part: &Part) -> KernelResult<Option<String>> {
            NoteCodec.resource_of(part)
        }
    }

    #[tokio::test]
    async fn a_render_that_outlasts_a_lease_term_still_publishes() {
        let now = Arc::new(std::sync::atomic::AtomicU64::new(0));
        let read = now.clone();
        let leases = Arc::new(MemoryLeases::new(
            WallClock::new(move || {
                std::time::UNIX_EPOCH
                    + Duration::from_millis(read.load(std::sync::atomic::Ordering::SeqCst))
            }),
            Duration::from_secs(60),
        ));
        let canonical = Arc::new(MemoryCanonicalStore::new(leases.clone()));
        let engine = engine(canonical.clone(), leases, Arc::new(Slow { now }));
        let documents = &engine.documents;
        documents
            .bootstrap("one", &note("one"), |_| Ok(()))
            .unwrap();
        documents.publish("one").await.unwrap();
        assert!(!documents.publication_pending("one"));
        assert!(canonical.pull(&id("one")).await.unwrap().is_some());
    }

    #[tokio::test]
    async fn a_publication_lacking_what_another_holder_published_is_refused() {
        let leases = leases();
        let canonical = Arc::new(MemoryCanonicalStore::new(leases.clone()));
        let first = engine(canonical.clone(), leases.clone(), Arc::new(NoteCodec));
        first
            .documents
            .bootstrap("one", &note("one"), |_| Ok(()))
            .unwrap();
        first.documents.publish("one").await.unwrap();

        let second = engine(canonical.clone(), leases, Arc::new(NoteCodec));
        let pulled = second.documents.pull("one").await.unwrap().unwrap();
        second
            .documents
            .bootstrap_pulled("one", &pulled, |_| Ok(()))
            .unwrap();
        let edit = |documents: &CollaborativeDocuments, body: &str| {
            let body = body.to_owned();
            documents
                .rewrite_as(&writer(), "one", &body.clone(), "edit", move |value| {
                    value["body"] = Value::String(body);
                    true
                })
                .unwrap()
                .unwrap();
        };
        edit(&second.documents, "second");
        second.documents.publish("one").await.unwrap();
        let published = canonical.pull(&id("one")).await.unwrap().unwrap();

        edit(&first.documents, "first");
        assert!(first.documents.publish("one").await.is_err());
        assert!(first.documents.publication_pending("one"));
        assert_eq!(
            canonical.pull(&id("one")).await.unwrap().unwrap(),
            published
        );
    }

    /// Two engines over one canonical store, each holding "one" as the first published it.
    async fn two_engines() -> (Arc<MemoryCanonicalStore>, Engine, Engine) {
        let leases = leases();
        let canonical = Arc::new(MemoryCanonicalStore::new(leases.clone()));
        let first = engine(canonical.clone(), leases.clone(), Arc::new(NoteCodec));
        first
            .documents
            .bootstrap("one", &note("one"), |_| Ok(()))
            .unwrap();
        first.documents.publish("one").await.unwrap();
        let second = engine(canonical.clone(), leases, Arc::new(NoteCodec));
        let pulled = second.documents.pull("one").await.unwrap().unwrap();
        second
            .documents
            .bootstrap_pulled("one", &pulled, |_| Ok(()))
            .unwrap();
        (canonical, first, second)
    }

    #[tokio::test]
    async fn a_removal_lacking_what_another_holder_published_is_refused() {
        let (canonical, first, second) = two_engines().await;
        first.documents.remove("one", &writer()).unwrap().unwrap();
        second
            .documents
            .rewrite_as(&writer(), "one", "edited", "edit", |value| {
                value["body"] = Value::String("edited".into());
                true
            })
            .unwrap()
            .unwrap();
        second.documents.publish("one").await.unwrap();
        let published = canonical.pull(&id("one")).await.unwrap().unwrap();

        assert!(first.documents.publish("one").await.is_err());
        assert!(first.documents.publication_pending("one"));
        assert_eq!(
            canonical.pull(&id("one")).await.unwrap().unwrap(),
            published
        );
    }

    #[tokio::test]
    async fn a_document_this_working_copy_never_removed_stays_in_the_store() {
        let leases = leases();
        let canonical = Arc::new(MemoryCanonicalStore::new(leases.clone()));
        let first = engine(canonical.clone(), leases.clone(), Arc::new(NoteCodec));
        first
            .documents
            .bootstrap("one", &note("one"), |_| Ok(()))
            .unwrap();
        first.documents.publish("one").await.unwrap();
        let published = canonical.pull(&id("one")).await.unwrap();

        // A commit to "one" that never landed, in a working copy that never held it.
        let second = engine(canonical.clone(), leases, Arc::new(NoteCodec));
        drop(second.outbox.mark(&id("one")).unwrap());
        second.documents.publish("one").await.unwrap();
        assert!(!second.documents.publication_pending("one"));
        assert_eq!(canonical.pull(&id("one")).await.unwrap(), published);
    }

    #[tokio::test]
    async fn a_document_seeded_from_the_stores_operations_keeps_its_version() {
        let leases = leases();
        let canonical = Arc::new(MemoryCanonicalStore::new(leases.clone()));
        let first = engine(canonical.clone(), leases.clone(), Arc::new(NoteCodec));
        first
            .documents
            .bootstrap("one", &note("one"), |_| Ok(()))
            .unwrap();
        first.documents.publish("one").await.unwrap();

        let second = engine(canonical, leases, Arc::new(NoteCodec));
        let pulled = second.documents.pull("one").await.unwrap().unwrap();
        second
            .documents
            .bootstrap_pulled("one", &pulled, |_| Ok(()))
            .unwrap();
        assert_eq!(
            second
                .documents
                .summary("one")
                .unwrap()
                .map(|summary| summary.version),
            pulled.version
        );
        assert_eq!(
            second.documents.document("one").unwrap().unwrap()["body"],
            note("one")["body"]
        );
        assert!(!second.documents.publication_pending("one"));
    }

    #[tokio::test]
    async fn a_document_seeded_from_parts_alone_waits_to_be_published() {
        let (canonical, engine) = notes();
        let documents = &engine.documents;
        canonical.write_outside(
            &id("one"),
            vec![crate::canonical::Part {
                name: crate::canonical::PartName::new("one.yaml").unwrap(),
                bytes: b"one\nwritten by hand".to_vec(),
            }],
        );
        let pulled = documents.pull("one").await.unwrap().unwrap();
        assert_eq!(pulled.version, None);
        documents
            .bootstrap_pulled("one", &pulled, |_| Ok(()))
            .unwrap();
        assert_eq!(
            documents.document("one").unwrap().unwrap()["body"],
            "written by hand"
        );
        assert!(documents.publication_pending("one"));
        documents.publish("one").await.unwrap();
        assert!(!documents.publication_pending("one"));
        assert_eq!(
            canonical
                .pull(&id("one"))
                .await
                .unwrap()
                .and_then(|held| held.version),
            documents
                .summary("one")
                .unwrap()
                .map(|summary| summary.version)
        );
    }
}
