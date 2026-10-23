//! Copyright (c) Scott A Dixon
//!
//! Explicit upgrades of a workspace's documents. An upgrade reads documents from the canonical
//! store, and the product transforms them. Its journal, a record in the engine's local store,
//! authorises replacing each document's parts with the transform's, fenced by the parts the
//! upgrade read. Publishing replaces them document by document, and recovery finishes an
//! authorised upgrade that a stop interrupted. One upgrade is unfinished at a time. A finished
//! journal keeps what each document held.

use base64::Engine as _;
use clerkenwell_store::CollaborationDocumentId;
use serde::{Deserialize, Serialize};
use std::path::Path;
use std::sync::Arc;
use walthamstow_kernel::journals::Journal;
use walthamstow_kernel::leases::{Holder, Leases};
use walthamstow_kernel::local_store::{JournalKey, LocalStore, Tag};
use walthamstow_kernel::{KernelError, KernelResult};
use walthamstow_workspace::canonical::{between, CanonicalStore, Part, PartName};

/// The journal every document upgrade works under.
pub const JOURNAL: Journal = Journal::new("document-upgrades");

/// The journal release 0.1.0 upgraded workspace files under.
const EARLIER: Journal = Journal::new("workspace-upgrades");

/// One document an upgrade replaces: the parts it read, and the parts that replace them. No
/// parts read adds the document; no parts replacing them removes it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Change {
    pub document: CollaborationDocumentId,
    pub expected: Vec<Part>,
    pub parts: Vec<Part>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
enum Status {
    Committing,
    Complete,
}

/// One upgrade's journal.
#[derive(Debug, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct Operation {
    operation_id: String,
    status: Status,
    changes: Vec<StoredChange>,
}

#[derive(Debug, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct StoredChange {
    entity: String,
    resource_id: String,
    expected: Vec<StoredPart>,
    parts: Vec<StoredPart>,
}

#[derive(Debug, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct StoredPart {
    name: String,
    bytes_base64: String,
}

fn stored(parts: &[Part]) -> Vec<StoredPart> {
    parts
        .iter()
        .map(|part| StoredPart {
            name: part.name.as_str().to_owned(),
            bytes_base64: base64::engine::general_purpose::STANDARD.encode(&part.bytes),
        })
        .collect()
}

fn parts(stored: &[StoredPart]) -> KernelResult<Vec<Part>> {
    stored
        .iter()
        .map(|part| {
            Ok(Part {
                name: PartName::new(part.name.clone())?,
                bytes: base64::engine::general_purpose::STANDARD
                    .decode(&part.bytes_base64)
                    .map_err(|error| KernelError::internal(error.to_string()))?,
            })
        })
        .collect()
}

impl StoredChange {
    fn of(change: &Change) -> Self {
        Self {
            entity: change.document.entity.clone(),
            resource_id: change.document.resource_id.clone(),
            expected: stored(&change.expected),
            parts: stored(&change.parts),
        }
    }

    fn change(&self) -> KernelResult<Change> {
        Ok(Change {
            document: CollaborationDocumentId::new(&self.entity, &self.resource_id),
            expected: parts(&self.expected)?,
            parts: parts(&self.parts)?,
        })
    }
}

/// The refusal of an upgrade whose document changed after it was read.
fn changed(change: &Change, operation_id: &str) -> KernelError {
    KernelError::conflict(format!(
        "{} \"{}\" changed after the upgrade {operation_id} read it.",
        change.document.entity, change.document.resource_id
    ))
}

/// Upgrades journalled in a local store and published to a canonical store, under the leases
/// granted to one holder.
#[derive(Debug, Clone)]
pub struct Upgrades {
    local: Arc<dyn LocalStore>,
    canonical: Arc<dyn CanonicalStore>,
    leases: Arc<dyn Leases>,
    holder: Holder,
}

impl Upgrades {
    pub fn new(
        local: Arc<dyn LocalStore>,
        canonical: Arc<dyn CanonicalStore>,
        leases: Arc<dyn Leases>,
        holder: Holder,
    ) -> Self {
        Self {
            local,
            canonical,
            leases,
            holder,
        }
    }

    /// The journal of `operation_id`, which holds only lowercase ASCII letters, digits, `-` and
    /// `_`, so no two ids name one record on a file system that folds case, and is no device
    /// name Windows reserves.
    fn key(operation_id: &str) -> KernelResult<JournalKey> {
        if operation_id.is_empty()
            || walthamstow_kernel::files::is_windows_device_name(operation_id)
            || !operation_id
                .chars()
                .all(|character| matches!(character, 'a'..='z' | '0'..='9' | '-' | '_'))
        {
            return Err(KernelError::invalid_request(
                "An upgrade's operation id holds only lowercase letters, digits, '-' and '_', \
                 and names no device.",
            ));
        }
        JournalKey::new(JOURNAL, [format!("{operation_id}.json")])
    }

    /// The record the unfinished upgrade is journalled in, which no operation id names.
    fn unfinished() -> JournalKey {
        JournalKey::new(JOURNAL, ["unfinished"]).expect("a name")
    }

    fn read(&self, key: &JournalKey) -> KernelResult<Option<(Operation, Tag)>> {
        let Some((bytes, tag)) = self.local.get(key)? else {
            return Ok(None);
        };
        let operation = serde_json::from_slice(&bytes)
            .map_err(|error| KernelError::internal(format!("Upgrade journal {key}: {error}")))?;
        Ok(Some((operation, tag)))
    }

    /// Authorises `changes` under `operation_id`: journals them for publication. An id already
    /// journalled, changes that name one document twice, parts the canonical store cannot keep,
    /// a document holding anything but what its change read or writes, and any upgrade while
    /// another is unfinished are refused.
    pub async fn authorise(&self, operation_id: &str, changes: &[Change]) -> KernelResult<()> {
        let key = Self::key(operation_id)?;
        for (index, change) in changes.iter().enumerate() {
            self.canonical.admit(&change.document, &change.parts)?;
            if changes[..index]
                .iter()
                .any(|earlier| earlier.document == change.document)
            {
                return Err(KernelError::invalid_request(format!(
                    "The upgrade {operation_id} changes {} \"{}\" twice.",
                    change.document.entity, change.document.resource_id
                )));
            }
        }
        let journalled =
            || KernelError::conflict(format!("The upgrade {operation_id} is already journalled."));
        if self.local.get(&key)?.is_some() {
            return Err(journalled());
        }
        if let Some((unfinished, _)) = self.read(&Self::unfinished())? {
            if unfinished.operation_id == operation_id {
                return Err(journalled());
            }
            return Err(KernelError::conflict(format!(
                "The upgrade {} is unfinished; recovery finishes it before {operation_id} is \
                 authorised.",
                unfinished.operation_id
            )));
        }
        self.check(operation_id, changes).await?;
        let operation = Operation {
            operation_id: operation_id.to_owned(),
            status: Status::Committing,
            changes: changes.iter().map(StoredChange::of).collect(),
        };
        let bytes = serde_json::to_vec(&operation).expect("an upgrade journal encodes");
        self.local
            .compare_and_swap(&Self::unfinished(), None, &bytes)?
            .ok_or_else(|| {
                KernelError::conflict(format!(
                    "Another upgrade was authorised while {operation_id} was checked."
                ))
            })?;
        Ok(())
    }

    /// Authorises `changes` under `operation_id` and publishes them.
    pub async fn apply(&self, operation_id: &str, changes: &[Change]) -> KernelResult<()> {
        self.authorise(operation_id, changes).await?;
        self.finish().await
    }

    /// Refuses `changes` unless each document holds only parts its change read or writes, as an
    /// interrupted publication leaves it.
    async fn check(&self, operation_id: &str, changes: &[Change]) -> KernelResult<()> {
        for change in changes {
            let held = self
                .canonical
                .pull(&change.document)
                .await?
                .map(|pulled| pulled.parts)
                .unwrap_or_default();
            if !between(&held, &change.expected, &change.parts) {
                return Err(changed(change, operation_id));
            }
        }
        Ok(())
    }

    /// Publishes the unfinished upgrade, document by document, journals it complete under its
    /// operation id, and lets another be authorised. Every document is checked before any is
    /// replaced: what each holds must lie between what the upgrade read and what it writes, as
    /// an interrupted publication leaves it.
    async fn finish(&self) -> KernelResult<()> {
        let unfinished = Self::unfinished();
        let Some((mut operation, tag)) = self.read(&unfinished)? else {
            return Ok(());
        };
        let key = Self::key(&operation.operation_id)?;
        // A stop after the upgrade was journalled complete leaves only this record to remove.
        if self.local.get(&key)?.is_none() {
            let changes = operation
                .changes
                .iter()
                .map(StoredChange::change)
                .collect::<KernelResult<Vec<_>>>()?;
            self.check(&operation.operation_id, &changes).await?;
            for change in &changes {
                let lease = self
                    .leases
                    .acquire(&change.document, &self.holder)
                    .await?
                    .ok_or_else(|| {
                        KernelError::conflict(format!(
                            "Another engine holds the lease on {} \"{}\".",
                            change.document.entity, change.document.resource_id
                        ))
                    })?;
                let replaced = self
                    .canonical
                    .replace(&change.document, &lease, &change.expected, &change.parts)
                    .await;
                self.leases.release(&lease).await?;
                if !replaced? {
                    return Err(changed(change, &operation.operation_id));
                }
            }
            operation.status = Status::Complete;
            let bytes = serde_json::to_vec(&operation).expect("an upgrade journal encodes");
            self.local
                .compare_and_swap(&key, None, &bytes)?
                .ok_or_else(|| {
                    KernelError::internal(format!(
                        "Upgrade journal {key} was written while it was finished."
                    ))
                })?;
        }
        if !self.local.remove(&unfinished, &tag)? {
            return Err(KernelError::internal(format!(
                "Upgrade journal {unfinished} changed while it was finished."
            )));
        }
        Ok(())
    }

    /// Finishes the authorised upgrade a stop interrupted, if any.
    pub async fn recover(&self) -> KernelResult<()> {
        self.finish().await
    }

    /// The changes `operation_id` authorised, as its journal keeps them; `None` when none is
    /// journalled.
    pub fn changes(&self, operation_id: &str) -> KernelResult<Option<Vec<Change>>> {
        let finished = self.read(&Self::key(operation_id)?)?;
        let unfinished = self
            .read(&Self::unfinished())?
            .filter(|(operation, _)| operation.operation_id == operation_id);
        finished
            .or(unfinished)
            .map(|(operation, _)| operation.changes.iter().map(StoredChange::change).collect())
            .transpose()
    }
}

/// Refuses while an upgrade that release 0.1.0 began under `data_dir` is unfinished. That
/// release finishes it at its next start; this one does not read its layout.
pub fn refuse_unfinished_earlier_upgrade(data_dir: &Path) -> KernelResult<()> {
    let internal = |path: &Path, error: &dyn std::fmt::Display| {
        KernelError::internal(format!("{}: {error}", path.display()))
    };
    let root = EARLIER
        .unlinked_root(data_dir)
        .map_err(|error| internal(&EARLIER.root(data_dir), &error))?;
    let entries = match std::fs::read_dir(&root) {
        Ok(entries) => entries,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(()),
        Err(error) => return Err(internal(&root, &error)),
    };
    for entry in entries {
        let entry = entry.map_err(|error| internal(&root, &error))?;
        let kind = entry
            .file_type()
            .map_err(|error| internal(&entry.path(), &error))?;
        if !kind.is_dir() {
            return Err(internal(
                &entry.path(),
                &"an upgrade's directory is a directory; this is not followed",
            ));
        }
        let journal = entry.path().join("operation.json");
        let bytes = match std::fs::symlink_metadata(&journal) {
            Ok(metadata) if metadata.file_type().is_file() => {
                std::fs::read(&journal).map_err(|error| internal(&journal, &error))?
            }
            Ok(_) => return Err(internal(&journal, &"this is not a regular file")),
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => continue,
            Err(error) => return Err(internal(&journal, &error)),
        };
        let operation: serde_json::Value =
            serde_json::from_slice(&bytes).map_err(|error| internal(&journal, &error))?;
        if operation["status"] != "complete" {
            return Err(KernelError::conflict(format!(
                "The upgrade at {} is unfinished. Start the engine release that began it to \
                 finish it, then this one.",
                entry.path().display()
            )));
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::time::{Duration, UNIX_EPOCH};
    use walthamstow_kernel::leases::MemoryLeases;
    use walthamstow_kernel::local_store::MemoryLocalStore;
    use walthamstow_kernel::WallClock;
    use walthamstow_workspace::canonical::MemoryCanonicalStore;

    fn document(resource_id: &str) -> CollaborationDocumentId {
        CollaborationDocumentId::new("Note", resource_id)
    }

    fn part(name: &str, text: &str) -> Part {
        Part {
            name: PartName::new(name).unwrap(),
            bytes: text.as_bytes().to_vec(),
        }
    }

    struct Fixture {
        local: Arc<MemoryLocalStore>,
        canonical: Arc<MemoryCanonicalStore>,
        leases: Arc<MemoryLeases>,
        upgrades: Upgrades,
    }

    fn fixture() -> Fixture {
        let leases = Arc::new(MemoryLeases::new(
            WallClock::new(|| UNIX_EPOCH),
            Duration::from_secs(60),
        ));
        let local = Arc::new(MemoryLocalStore::new());
        let canonical = Arc::new(MemoryCanonicalStore::new(leases.clone()));
        let upgrades = Upgrades::new(
            local.clone(),
            canonical.clone(),
            leases.clone(),
            Holder::new("engine").unwrap(),
        );
        Fixture {
            local,
            canonical,
            leases,
            upgrades,
        }
    }

    /// Two notes in a format the upgrade changes, and the changes that upgrade them.
    async fn held(fixture: &Fixture) -> Vec<Change> {
        let mut changes = Vec::new();
        for resource_id in ["one", "two"] {
            let earlier = vec![part(&format!("{resource_id}.yaml"), "format: 1")];
            fixture
                .canonical
                .write_outside(&document(resource_id), earlier.clone());
            changes.push(Change {
                document: document(resource_id),
                expected: earlier,
                parts: vec![part(&format!("{resource_id}.yaml"), "format: 2")],
            });
        }
        changes
    }

    async fn parts_of(fixture: &Fixture, resource_id: &str) -> Option<Vec<Part>> {
        fixture
            .canonical
            .pull(&document(resource_id))
            .await
            .unwrap()
            .map(|pulled| pulled.parts)
    }

    #[tokio::test]
    async fn an_upgrade_replaces_each_document_and_its_journal_keeps_what_each_held() {
        let fixture = fixture();
        let changes = held(&fixture).await;
        fixture.upgrades.apply("to-two", &changes).await.unwrap();
        for change in &changes {
            let pulled = fixture
                .canonical
                .pull(&change.document)
                .await
                .unwrap()
                .unwrap();
            assert_eq!(pulled.parts, change.parts);
            assert_eq!(pulled.version, None, "no version names upgraded parts");
        }
        assert_eq!(fixture.upgrades.changes("to-two").unwrap(), Some(changes));
        assert!(
            fixture.upgrades.authorise("to-two", &[]).await.is_err(),
            "an operation id names one upgrade"
        );
    }

    #[tokio::test]
    async fn recovery_finishes_an_authorised_upgrade_that_a_stop_interrupted() {
        let fixture = fixture();
        let changes = held(&fixture).await;
        fixture
            .upgrades
            .authorise("interrupted", &changes)
            .await
            .unwrap();
        // The stop came after the first document was replaced.
        fixture
            .canonical
            .write_outside(&changes[0].document, changes[0].parts.clone());

        fixture.upgrades.recover().await.unwrap();
        for change in &changes {
            assert_eq!(
                parts_of(&fixture, &change.document.resource_id).await,
                Some(change.parts.clone())
            );
        }
        fixture.upgrades.recover().await.unwrap();
        assert_eq!(
            parts_of(&fixture, "two").await,
            Some(changes[1].parts.clone()),
            "a finished upgrade is not published again"
        );
    }

    #[tokio::test]
    async fn recovery_finishes_a_document_whose_replacement_stopped_part_way() {
        let fixture = fixture();
        let read = vec![
            part("note.yaml", "format: 1"),
            part("old.yaml", "format: 1"),
        ];
        let written = vec![
            part("new.yaml", "format: 2"),
            part("note.yaml", "format: 2"),
        ];
        fixture
            .canonical
            .write_outside(&document("one"), read.clone());
        let changes = vec![Change {
            document: document("one"),
            expected: read.clone(),
            parts: written.clone(),
        }];
        fixture
            .upgrades
            .authorise("part-way", &changes)
            .await
            .unwrap();
        // The stop came after one part was written.
        fixture
            .canonical
            .write_outside(&document("one"), vec![written[1].clone(), read[1].clone()]);
        fixture.upgrades.recover().await.unwrap();
        assert_eq!(parts_of(&fixture, "one").await, Some(written));
    }

    #[tokio::test]
    async fn recovery_finishes_a_part_stopped_between_its_rename_in_case_and_its_write() {
        let fixture = fixture();
        let read = vec![part("note.yaml", "format: 1")];
        let written = vec![part("NOTE.yaml", "format: 2")];
        fixture
            .canonical
            .write_outside(&document("one"), read.clone());
        let changes = vec![Change {
            document: document("one"),
            expected: read,
            parts: written.clone(),
        }];
        fixture
            .upgrades
            .authorise("renamed", &changes)
            .await
            .unwrap();
        // The stop came after the rename, before the write.
        fixture
            .canonical
            .write_outside(&document("one"), vec![part("NOTE.yaml", "format: 1")]);
        fixture.upgrades.recover().await.unwrap();
        assert_eq!(parts_of(&fixture, "one").await, Some(written));
    }

    #[tokio::test]
    async fn an_upgrade_writing_parts_no_store_can_keep_is_refused_unjournalled() {
        let fixture = fixture();
        let changes = held(&fixture).await;
        for parts in [
            vec![part("one.yaml", "a"), part("ONE.yaml", "b")],
            vec![part("one", "a"), part("one/two.yaml", "b")],
        ] {
            let mut refused = changes.clone();
            refused[1].parts = parts;
            assert!(fixture.upgrades.apply("refused", &refused).await.is_err());
            assert_eq!(fixture.upgrades.changes("refused").unwrap(), None);
            assert_eq!(
                parts_of(&fixture, "one").await,
                Some(changes[0].expected.clone()),
                "no document is replaced"
            );
        }
    }

    #[tokio::test]
    async fn a_document_already_holding_the_target_under_a_version_loses_the_version() {
        let fixture = fixture();
        let changes = held(&fixture).await;
        // The second note was published, as the upgrade would write it, under a version.
        let lease = fixture
            .leases
            .acquire(&document("two"), &Holder::new("engine").unwrap())
            .await
            .unwrap()
            .unwrap();
        fixture
            .canonical
            .publish(
                &document("two"),
                &lease,
                &walthamstow_workspace::canonical::Publication {
                    version: clerkenwell_store::CollaborationVersion {
                        incarnation: "published".into(),
                        peers: Default::default(),
                    },
                    operations: walthamstow_workspace::canonical::Operations {
                        schema_version: 1,
                        update_base64: String::new(),
                    },
                    parts: changes[1].parts.clone(),
                },
            )
            .await
            .unwrap();
        fixture.leases.release(&lease).await.unwrap();
        fixture.upgrades.apply("to-two", &changes).await.unwrap();
        let pulled = fixture
            .canonical
            .pull(&document("two"))
            .await
            .unwrap()
            .unwrap();
        assert_eq!(
            pulled.version, None,
            "no version names what an upgrade wrote"
        );
        assert_eq!(pulled.parts, changes[1].parts);
    }

    #[tokio::test]
    async fn an_upgrade_that_changes_one_document_twice_is_refused_unjournalled() {
        let fixture = fixture();
        let mut changes = held(&fixture).await;
        let mut twice = changes[0].clone();
        twice.parts = vec![part("one.yaml", "format: 3")];
        changes.push(twice);
        assert!(fixture.upgrades.apply("twice", &changes).await.is_err());
        assert_eq!(fixture.upgrades.changes("twice").unwrap(), None);
        assert_eq!(
            parts_of(&fixture, "one").await,
            Some(changes[0].expected.clone())
        );
    }

    #[tokio::test]
    async fn an_upgrade_of_a_document_changed_since_it_was_read_is_refused_unjournalled() {
        let fixture = fixture();
        let mut changes = held(&fixture).await;
        let edited = vec![part("two.yaml", "format: 1\nedited: true")];
        fixture
            .canonical
            .write_outside(&document("two"), edited.clone());

        assert!(fixture.upgrades.apply("stale", &changes).await.is_err());
        assert_eq!(fixture.upgrades.changes("stale").unwrap(), None);
        assert_eq!(
            parts_of(&fixture, "one").await,
            Some(changes[0].expected.clone())
        );
        assert_eq!(parts_of(&fixture, "two").await, Some(edited.clone()));
        fixture.upgrades.recover().await.unwrap();

        changes[1].expected = edited;
        fixture.upgrades.apply("stale", &changes).await.unwrap();
        for change in &changes {
            assert_eq!(
                parts_of(&fixture, &change.document.resource_id).await,
                Some(change.parts.clone()),
                "the id a refused upgrade named authorises it once it reads what is held"
            );
        }
    }

    #[tokio::test]
    async fn a_document_changed_after_its_upgrade_was_authorised_replaces_nothing() {
        let fixture = fixture();
        let changes = held(&fixture).await;
        fixture.upgrades.authorise("raced", &changes).await.unwrap();
        let edited = vec![part("two.yaml", "format: 1\nedited: true")];
        fixture
            .canonical
            .write_outside(&document("two"), edited.clone());

        assert!(fixture.upgrades.recover().await.is_err());
        assert_eq!(
            parts_of(&fixture, "one").await,
            Some(changes[0].expected.clone())
        );
        assert_eq!(parts_of(&fixture, "two").await, Some(edited));
    }

    #[tokio::test]
    async fn one_upgrade_is_unfinished_at_a_time() {
        let fixture = fixture();
        let changes = held(&fixture).await;
        fixture
            .upgrades
            .authorise("first", &changes[..1])
            .await
            .unwrap();
        assert!(fixture
            .upgrades
            .authorise("second", &changes[1..])
            .await
            .is_err());
        assert_eq!(fixture.upgrades.changes("second").unwrap(), None);

        fixture.upgrades.recover().await.unwrap();
        fixture
            .upgrades
            .apply("second", &changes[1..])
            .await
            .unwrap();
        for change in &changes {
            assert_eq!(
                parts_of(&fixture, &change.document.resource_id).await,
                Some(change.parts.clone())
            );
        }
    }

    #[tokio::test]
    async fn of_two_upgrades_authorised_at_once_one_is_journalled() {
        let fixture = fixture();
        let changes = held(&fixture).await;
        let (first, second) = tokio::join!(
            fixture.upgrades.authorise("first", &changes),
            fixture.upgrades.authorise("second", &changes)
        );
        assert_eq!(first.is_ok(), second.is_err());
        fixture.upgrades.recover().await.unwrap();
        for change in &changes {
            assert_eq!(
                parts_of(&fixture, &change.document.resource_id).await,
                Some(change.parts.clone())
            );
        }
    }

    #[tokio::test]
    async fn an_upgrade_journalled_complete_before_a_stop_is_not_published_again() {
        let fixture = fixture();
        let changes = held(&fixture).await;
        fixture
            .upgrades
            .authorise("stopped", &changes)
            .await
            .unwrap();
        // The stop came after the upgrade was journalled complete, before it was let go.
        let (mut operation, _) = fixture
            .upgrades
            .read(&Upgrades::unfinished())
            .unwrap()
            .unwrap();
        operation.status = Status::Complete;
        fixture
            .local
            .compare_and_swap(
                &Upgrades::key("stopped").unwrap(),
                None,
                &serde_json::to_vec(&operation).unwrap(),
            )
            .unwrap()
            .unwrap();

        fixture.upgrades.recover().await.unwrap();
        assert_eq!(
            parts_of(&fixture, "one").await,
            Some(changes[0].expected.clone()),
            "a complete upgrade is not published again"
        );
        assert_eq!(fixture.upgrades.changes("stopped").unwrap(), Some(changes));
        fixture.upgrades.authorise("next", &[]).await.unwrap();
    }

    #[tokio::test]
    async fn an_upgrade_adds_and_removes_documents() {
        let fixture = fixture();
        let earlier = vec![part("one.yaml", "format: 1")];
        fixture
            .canonical
            .write_outside(&document("one"), earlier.clone());
        let changes = vec![
            Change {
                document: document("one"),
                expected: earlier,
                parts: Vec::new(),
            },
            Change {
                document: document("merged"),
                expected: Vec::new(),
                parts: vec![part("merged.yaml", "format: 2")],
            },
        ];
        fixture.upgrades.apply("merge", &changes).await.unwrap();
        assert_eq!(parts_of(&fixture, "one").await, None);
        assert_eq!(
            parts_of(&fixture, "merged").await,
            Some(changes[1].parts.clone())
        );
    }

    #[tokio::test]
    async fn an_operation_id_that_case_or_a_path_could_alias_is_refused() {
        let fixture = fixture();
        for id in ["", "Upper", "a/b", "../escape", ".hidden", "con"] {
            assert!(fixture.upgrades.authorise(id, &[]).await.is_err(), "{id:?}");
        }
    }

    #[cfg(unix)]
    #[test]
    fn a_linked_operation_of_the_earlier_release_is_refused_unread() {
        let data = tempfile::tempdir().unwrap();
        let outside = tempfile::tempdir().unwrap();
        std::fs::write(
            outside.path().join("operation.json"),
            br#"{"operation_id":"linked","status":"complete"}"#,
        )
        .unwrap();
        std::fs::create_dir_all(EARLIER.root(data.path())).unwrap();
        std::os::unix::fs::symlink(outside.path(), EARLIER.root(data.path()).join("linked"))
            .unwrap();
        assert!(refuse_unfinished_earlier_upgrade(data.path()).is_err());
    }

    #[test]
    fn an_unfinished_upgrade_of_the_earlier_release_is_refused() {
        let data = tempfile::tempdir().unwrap();
        refuse_unfinished_earlier_upgrade(data.path()).unwrap();
        let operation = EARLIER.root(data.path()).join("earlier");
        std::fs::create_dir_all(operation.join("original")).unwrap();
        std::fs::write(
            operation.join("operation.json"),
            br#"{"operation_id":"earlier","status":"complete","paths":[],"source_hashes":{},"target_hashes":{}}"#,
        )
        .unwrap();
        refuse_unfinished_earlier_upgrade(data.path()).unwrap();
        std::fs::write(
            operation.join("operation.json"),
            br#"{"operation_id":"earlier","status":"committing","paths":[],"source_hashes":{},"target_hashes":{}}"#,
        )
        .unwrap();
        assert!(refuse_unfinished_earlier_upgrade(data.path()).is_err());
    }
}
