//! Copyright (c) Scott A Dixon
//!
//! The `walthamstow` contribution: `workspace.status`, `workspace.documents`,
//! `workspace.upgrade`, `workspace.recover` and `journals.list`. Each runs against the workspace
//! under the lease, so in-process it is refused with `workspace_held` while a daemon holds the
//! workspace.

use crate::codes;
use crate::formats::{DocumentFormat, FormatStatus, Upgrade};
use crate::lease::{Holder, Lease};
use crate::workspace::Workspace;
use clapham::{Call, Contribution, Effect, Evidence, Locality, Refusal, TypedOperation};
use schemars::JsonSchema;
use serde::{Deserialize, Serialize};
use std::collections::{BTreeMap, BTreeSet};
use std::fmt;
use std::sync::Arc;
use walthamstow_kernel::hashing::sha256_hex;
use walthamstow_kernel::journals::Journal;
use walthamstow_kernel::local_store::{JournalKey, LocalStore};
use walthamstow_kernel::{KernelError, KernelResult};

/// The namespace Walthamstow's operations are contributed under.
pub const NAMESPACE: &str = "walthamstow";

/// The scope that reads the workspace.
pub const WORKSPACE_READ: &str = "walthamstow:workspace:read";
/// The scope that upgrades and recovers the workspace.
pub const WORKSPACE_ADMIN: &str = "walthamstow:workspace:admin";
/// The scope that reads the engine's journals.
pub const JOURNALS_READ: &str = "walthamstow:journals:read";

/// One kind of typed document the product keeps in the workspace.
pub struct Entity {
    name: String,
    directory: String,
    format: Arc<dyn DocumentFormat>,
}

impl Entity {
    /// The entity `name`, its documents in `directory` as the product configures it, each in
    /// the format `format` tells.
    pub fn new(
        name: impl Into<String>,
        directory: impl Into<String>,
        format: impl DocumentFormat + 'static,
    ) -> Self {
        Self {
            name: name.into(),
            directory: directory.into(),
            format: Arc::new(format),
        }
    }
}

impl fmt::Debug for Entity {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("Entity")
            .field("name", &self.name)
            .field("directory", &self.directory)
            .finish_non_exhaustive()
    }
}

/// Walthamstow's operations over the workspace and local records the product supplies.
pub struct WalthamstowOps {
    lease: Lease,
    workspace: Arc<dyn Workspace>,
    store: Arc<dyn LocalStore>,
    entities: Vec<Entity>,
    upgrades: BTreeMap<String, Arc<dyn Upgrade>>,
    journals: BTreeSet<Journal>,
}

impl fmt::Debug for WalthamstowOps {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("WalthamstowOps")
            .field("lease", &self.lease)
            .field("entities", &self.entities)
            .field("upgrades", &self.upgrades.keys().collect::<Vec<_>>())
            .field("journals", &self.journals)
            .finish_non_exhaustive()
    }
}

/// An operation's empty input.
#[derive(Debug, Clone, Default, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct Nothing {}

/// The workspace, its entities and who holds it.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
pub struct Status {
    /// The workspace's root.
    pub root: String,
    /// Which engine holds the workspace while the operation runs.
    pub holder: Holder,
    /// Each entity, in the order the product registered them.
    pub entities: Vec<EntityStatus>,
}

/// One entity's directory and how many documents it holds.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
pub struct EntityStatus {
    pub name: String,
    /// The entity's directory, relative to the root, as the product configures it.
    pub directory: String,
    pub documents: usize,
}

/// Each typed document and its format.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
pub struct Documents {
    pub documents: Vec<Document>,
}

/// One typed document and its format.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
pub struct Document {
    pub entity: String,
    /// The document's path, relative to the root.
    pub path: String,
    pub format: FormatStatus,
}

/// An upgrade to run.
#[derive(Debug, Clone, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct UpgradeRequest {
    /// The registered upgrade.
    pub upgrade: String,
    /// The revision a preview reported. The upgrade is refused when the documents it would
    /// change no longer hold what that preview read.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub expected_revision: Option<String>,
}

/// What an upgrade changes, or changed.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
pub struct UpgradeResult {
    pub upgrade: String,
    /// Names what the upgrade read of the documents it changes.
    pub revision: String,
    /// The documents the upgrade changes, relative to the root, in path order.
    pub changed: Vec<String>,
    /// The operation the upgrade was journalled as; none for a preview, or when nothing
    /// changes.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub operation_id: Option<String>,
}

/// What recovery did, each upgrade named by its operation id.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
pub struct RecoveryResult {
    /// The upgrades a journal authorised, which recovery published.
    pub finished: Vec<String>,
    /// The upgrades whose staging no journal authorised, which recovery discarded.
    pub discarded: Vec<String>,
}

/// The engine's local journals and their records.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
pub struct Journals {
    pub journals: Vec<JournalRecords>,
}

/// One journal and the key of each of its records, in key order.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
pub struct JournalRecords {
    pub journal: String,
    /// Each record's names within the journal, outermost first.
    pub records: Vec<Vec<String>>,
}

/// What an upgrade read, and what it makes of it.
struct Plan {
    revision: String,
    sources: BTreeMap<String, Vec<u8>>,
    changes: BTreeMap<String, Vec<u8>>,
}

/// Whether `id` can name an upgrade: lowercase ASCII letters, digits, `-` and `_`.
fn is_upgrade_id(id: &str) -> bool {
    !id.is_empty()
        && id
            .bytes()
            .all(|byte| matches!(byte, b'a'..=b'z' | b'0'..=b'9' | b'-' | b'_'))
}

impl WalthamstowOps {
    /// Operations over `workspace`, run under `lease`, and the local records `store` keeps.
    /// `journals.list` lists the publication journal, and each journal [`Self::journal`]
    /// adds.
    pub fn new(lease: Lease, workspace: Arc<dyn Workspace>, store: Arc<dyn LocalStore>) -> Self {
        Self {
            lease,
            workspace,
            store,
            entities: Vec::new(),
            upgrades: BTreeMap::new(),
            journals: BTreeSet::from([walthamstow_workspace::publications::JOURNAL]),
        }
    }

    /// Reports and upgrades `entity`'s documents.
    pub fn entity(mut self, entity: Entity) -> Self {
        self.entities.push(entity);
        self
    }

    /// Registers `upgrade` as `id`, which holds only lowercase ASCII letters, digits, `-` and
    /// `_`. An id registered twice is refused.
    pub fn upgrade(
        mut self,
        id: impl Into<String>,
        upgrade: impl Upgrade + 'static,
    ) -> KernelResult<Self> {
        let id = id.into();
        if !is_upgrade_id(&id) {
            return Err(KernelError::invalid_request(format!(
                "The upgrade {id:?} is not named by lowercase letters, digits, '-' and '_'."
            )));
        }
        if self
            .upgrades
            .insert(id.clone(), Arc::new(upgrade))
            .is_some()
        {
            return Err(KernelError::invalid_request(format!(
                "The upgrade {id} is registered twice."
            )));
        }
        Ok(self)
    }

    /// Lists `journal`'s records in `journals.list`.
    pub fn journal(mut self, journal: Journal) -> Self {
        self.journals.insert(journal);
        self
    }

    /// The `walthamstow` contribution.
    pub fn contribution(self) -> Contribution {
        let ops = Arc::new(self);
        Contribution::new(NAMESPACE)
            .typed_operation(
                descriptor::<Nothing, Status>(
                    "workspace.status",
                    "Report the workspace's root, which engine holds it, and each entity's \
                     directory and document count.",
                    Effect::Query,
                    WORKSPACE_READ,
                ),
                over(&ops, Evidence::Completed, |ops, _: Nothing, _| ops.status()),
            )
            .typed_operation(
                descriptor::<Nothing, Documents>(
                    "workspace.documents",
                    "List each typed document in the workspace and the format it is in.",
                    Effect::Query,
                    WORKSPACE_READ,
                ),
                over(&ops, Evidence::Completed, |ops, _: Nothing, _| {
                    ops.documents()
                }),
            )
            .typed_operation(
                descriptor::<UpgradeRequest, UpgradeResult>(
                    "workspace.upgrade",
                    "Run a registered upgrade over every document it applies to. The upgrade \
                     is journalled, and each document is replaced only while it holds what \
                     the upgrade read. A preview reports the revision to expect.",
                    Effect::Administrative,
                    WORKSPACE_ADMIN,
                )
                .positional(["upgrade"])
                .preview(true)
                .idempotent(true),
                over(
                    &ops,
                    Evidence::Uncertain,
                    |ops, input: UpgradeRequest, preview| ops.run_upgrade(input, preview),
                ),
            )
            .typed_operation(
                descriptor::<Nothing, RecoveryResult>(
                    "workspace.recover",
                    "Finish every upgrade a journal authorised, and discard staging no journal \
                     authorises.",
                    Effect::Administrative,
                    WORKSPACE_ADMIN,
                )
                .idempotent(true),
                over(&ops, Evidence::Uncertain, |ops, _: Nothing, _| {
                    ops.recover()
                }),
            )
            .typed_operation(
                descriptor::<Nothing, Journals>(
                    "journals.list",
                    "List the engine's local journals and the names of their records.",
                    Effect::Query,
                    JOURNALS_READ,
                ),
                over(&ops, Evidence::Completed, |ops, _: Nothing, _| {
                    ops.list_journals()
                }),
            )
    }

    fn status(&self) -> Result<Status, Refusal> {
        let entities = self
            .entities
            .iter()
            .map(|entity| {
                Ok(EntityStatus {
                    name: entity.name.clone(),
                    directory: entity.directory.clone(),
                    documents: self.read(entity, Evidence::Completed)?.len(),
                })
            })
            .collect::<Result<_, Refusal>>()?;
        Ok(Status {
            root: self.workspace.root(),
            holder: self.lease.holder(),
            entities,
        })
    }

    fn documents(&self) -> Result<Documents, Refusal> {
        let mut documents = Vec::new();
        for entity in &self.entities {
            for (path, bytes) in self.read(entity, Evidence::Completed)? {
                documents.push(Document {
                    entity: entity.name.clone(),
                    format: entity.format.status(&path, &bytes),
                    path,
                });
            }
        }
        Ok(Documents { documents })
    }

    fn read(&self, entity: &Entity, evidence: Evidence) -> Result<Vec<(String, Vec<u8>)>, Refusal> {
        self.workspace
            .documents(&entity.name)
            .map_err(|error| codes::from_error(error, evidence))
    }

    /// What the upgrade `id` reads and makes of it. A document in a newer format than the
    /// product knows refuses the upgrade, whichever entity it belongs to.
    fn plan(&self, id: &str) -> Result<Plan, Refusal> {
        let upgrade = self.upgrades.get(id).ok_or_else(|| {
            codes::refusal(
                codes::UPGRADE_UNKNOWN,
                format!("No upgrade is registered as {id}."),
            )
        })?;
        let mut newer = Vec::new();
        let mut sources = BTreeMap::new();
        let mut changes = BTreeMap::new();
        for entity in &self.entities {
            for (path, bytes) in self.read(entity, Evidence::Completed)? {
                match entity.format.status(&path, &bytes) {
                    FormatStatus::Newer { format } => {
                        newer.push(serde_json::json!({ "path": path, "format": format }));
                    }
                    FormatStatus::Upgradable { upgrade: named } if named == id => {
                        let upgraded =
                            upgrade
                                .apply(&entity.name, &path, &bytes)
                                .map_err(|message| {
                                    let mut refusal = codes::refusal(
                                        codes::UPGRADE_FAILED,
                                        format!(
                                            "The upgrade {id} could not upgrade {path}: {message}"
                                        ),
                                    );
                                    refusal.details = Some(serde_json::json!({ "path": path }));
                                    refusal
                                })?;
                        if upgraded != bytes {
                            changes.insert(path.clone(), upgraded);
                            sources.insert(path, bytes);
                        }
                    }
                    _ => {}
                }
            }
        }
        if !newer.is_empty() {
            let mut refusal = codes::refusal(
                codes::DOCUMENT_NEWER,
                "The workspace holds documents in a format newer than this engine knows. An \
                 engine that knows it upgrades them.",
            );
            refusal.details = Some(serde_json::json!({ "documents": newer }));
            return Err(refusal);
        }
        let read = sources
            .iter()
            .map(|(path, bytes)| (path.as_str(), sha256_hex(bytes)))
            .collect::<Vec<_>>();
        let revision = sha256_hex(&serde_json::to_vec(&(id, read)).expect("a plan encodes"));
        Ok(Plan {
            revision,
            sources,
            changes,
        })
    }

    /// Runs, or previews, the upgrade `input` names. While an upgrade a journal authorises is
    /// still to finish, every upgrade is refused until recovery finishes it, since a document
    /// that upgrade published may not yet be durable.
    fn run_upgrade(&self, input: UpgradeRequest, preview: bool) -> Result<UpgradeResult, Refusal> {
        let pending = self
            .workspace
            .pending()
            .map_err(|error| codes::from_error(error, Evidence::Completed))?;
        if !pending.is_empty() {
            let mut refusal = codes::refusal(
                codes::UPGRADE_PENDING,
                "Upgrades are journalled and not finished. Recover the workspace first.",
            );
            refusal.details = Some(serde_json::json!({ "operation_ids": pending }));
            return Err(refusal);
        }
        let plan = self.plan(&input.upgrade)?;
        if input
            .expected_revision
            .as_ref()
            .is_some_and(|expected| *expected != plan.revision)
        {
            let mut refusal = codes::refusal(
                codes::UPGRADE_STALE,
                "The documents the upgrade changes no longer hold what the preview read.",
            );
            refusal.details = Some(serde_json::json!({ "revision": plan.revision }));
            refusal.retryable = true;
            return Err(refusal);
        }
        let mut result = UpgradeResult {
            upgrade: input.upgrade,
            changed: plan.changes.keys().cloned().collect(),
            operation_id: None,
            revision: plan.revision,
        };
        if preview || plan.changes.is_empty() {
            return Ok(result);
        }
        let operation_id = format!("{}-{}", result.upgrade, &result.revision[..16]);
        self.workspace
            .publish(&operation_id, &plan.sources, &plan.changes)
            .map_err(|error| codes::from_error(error, Evidence::Uncertain))?;
        result.operation_id = Some(operation_id);
        Ok(result)
    }

    fn recover(&self) -> Result<RecoveryResult, Refusal> {
        let recovery = self
            .workspace
            .recover()
            .map_err(|error| codes::from_error(error, Evidence::Uncertain))?;
        Ok(RecoveryResult {
            finished: recovery.finished,
            discarded: recovery.discarded,
        })
    }

    fn list_journals(&self) -> Result<Journals, Refusal> {
        let journals = self
            .journals
            .iter()
            .map(|journal| {
                let records = self
                    .store
                    .list(&JournalKey::journal(*journal))
                    .map_err(|error| codes::from_error(error.into(), Evidence::Completed))?
                    .into_iter()
                    .map(|key| key.names().to_vec())
                    .collect();
                Ok(JournalRecords {
                    journal: journal.name().to_owned(),
                    records,
                })
            })
            .collect::<Result<_, Refusal>>()?;
        Ok(Journals { journals })
    }
}

/// The descriptor of the operation `local` in the `walthamstow` namespace, requiring `scope`,
/// declaring every Walthamstow refusal.
fn descriptor<I: JsonSchema, O: JsonSchema>(
    local: &str,
    description: &str,
    effect: Effect,
    scope: &str,
) -> TypedOperation<I, O> {
    let mut operation = TypedOperation::new(format!("{NAMESPACE}.{local}"), description, effect)
        .expect("Walthamstow's operation names are valid")
        .locality(Locality::Workspace)
        .require_scope(scope);
    for code in codes::ALL {
        operation = operation.refusal(*code, codes::category(code).expect("a declared code"));
    }
    operation
}

type Handled<O> = std::pin::Pin<Box<dyn std::future::Future<Output = Result<O, Refusal>> + Send>>;

/// A typed handler that runs `work` under the lease, off the runtime's threads, with the
/// call's input and whether it is a preview. A handler that panics is reported with
/// `evidence`.
fn over<I, O, F>(
    ops: &Arc<WalthamstowOps>,
    evidence: Evidence,
    work: F,
) -> impl Fn(Call, I) -> Handled<O> + Send + Sync + 'static
where
    I: Send + 'static,
    O: Send + 'static,
    F: Fn(&WalthamstowOps, I, bool) -> Result<O, Refusal> + Send + Sync + 'static,
{
    let ops = ops.clone();
    let work = Arc::new(work);
    move |call, input| {
        let ops = ops.clone();
        let work = work.clone();
        Box::pin(async move {
            let hold = ops
                .lease
                .hold()
                .await
                .map_err(|error| codes::from_error(error, Evidence::NotDispatched))?;
            tokio::task::spawn_blocking(move || {
                let _hold = hold;
                work(&ops, input, call.preview)
            })
            .await
            .map_err(|error| {
                codes::refusal(codes::WORKSPACE_FAILURE, error.to_string()).with_evidence(evidence)
            })?
        })
    }
}
