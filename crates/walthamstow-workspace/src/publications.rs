//! Which accepted workspace documents still wait for their files.
//!
//! The engine marks a document before it commits a change to it and clears
//! the mark once the document's file renders the accepted state, so a change
//! that a failed write or a stop left unpublished is found again, after a
//! restart too. Each mark is a file under the engine's
//! `collaboration-publications` journal.

use clerkenwell_store::CollaborationDocumentId;
use serde::{Deserialize, Serialize};
use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex, MutexGuard, PoisonError};
use walthamstow_kernel::journals::Journal;
use walthamstow_kernel::{KernelError, KernelResult};

/// The journal the marks are kept under.
pub const JOURNAL: Journal = Journal::new("collaboration-publications");

#[derive(Debug, Clone)]
pub struct PublicationJournal {
    inner: Arc<Inner>,
}

#[derive(Debug)]
struct Inner {
    root: PathBuf,
    /// How far each mark is written before a commit relies on it.
    durability: walthamstow_kernel::durability::WriteDurability,
    /// Every marked document, with how many commits to it are under way.
    marked: Mutex<HashMap<CollaborationDocumentId, usize>>,
    /// Woken when a commit to a marked document finishes.
    finished: tokio::sync::Notify,
}

#[derive(Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct Mark {
    entity: String,
    resource_id: String,
}

/// A commit under way to a marked document. While one is, the mark stays.
#[derive(Debug)]
pub struct PendingCommit {
    inner: Arc<Inner>,
    document: CollaborationDocumentId,
}

impl Drop for PendingCommit {
    fn drop(&mut self) {
        if let Some(under_way) = self.inner.locked().get_mut(&self.document) {
            *under_way = under_way.saturating_sub(1);
        }
        self.inner.finished.notify_one();
    }
}

fn internal(path: &Path, error: impl std::fmt::Display) -> KernelError {
    KernelError::internal(format!(
        "Collaboration publication journal {}: {error}",
        path.display()
    ))
}

impl Inner {
    fn locked(&self) -> MutexGuard<'_, HashMap<CollaborationDocumentId, usize>> {
        self.marked.lock().unwrap_or_else(PoisonError::into_inner)
    }

    fn path(&self, document: &CollaborationDocumentId) -> PathBuf {
        let name = walthamstow_kernel::hashing::sha256_hex(
            format!("{}/{}", document.entity, document.resource_id).as_bytes(),
        );
        self.root.join(format!("{name}.json"))
    }
}

impl PublicationJournal {
    /// The journal under `data_dir`, with every mark a stop left in it.
    pub fn open(
        data_dir: &Path,
        durability: walthamstow_kernel::durability::WriteDurability,
    ) -> KernelResult<Self> {
        let root = JOURNAL.root(data_dir);
        let mut marked = HashMap::new();
        let entries = match std::fs::read_dir(&root) {
            Ok(entries) => Some(entries),
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => None,
            Err(error) => return Err(internal(&root, error)),
        };
        for entry in entries.into_iter().flatten() {
            let path = entry.map_err(|error| internal(&root, error))?.path();
            if path.extension().and_then(|extension| extension.to_str()) != Some("json") {
                continue;
            }
            let bytes = std::fs::read(&path).map_err(|error| internal(&path, error))?;
            let mark: Mark =
                serde_json::from_slice(&bytes).map_err(|error| internal(&path, error))?;
            marked.insert(
                CollaborationDocumentId::new(mark.entity, mark.resource_id),
                0,
            );
        }
        Ok(Self {
            inner: Arc::new(Inner {
                root,
                durability,
                marked: Mutex::new(marked),
                finished: tokio::sync::Notify::new(),
            }),
        })
    }

    /// Marks `document` for a commit about to be made to it. The mark stays
    /// at least until the returned commit is dropped.
    pub fn mark(&self, document: &CollaborationDocumentId) -> KernelResult<PendingCommit> {
        let mut marked = self.inner.locked();
        if !marked.contains_key(document) {
            let path = self.inner.path(document);
            let mark = serde_json::to_vec(&Mark {
                entity: document.entity.clone(),
                resource_id: document.resource_id.clone(),
            })
            .map_err(|error| internal(&path, error))?;
            walthamstow_kernel::durability::write_synced(self.inner.durability, &path, &mark)
                .map_err(|error| internal(&path, error))?;
        }
        *marked.entry(document.clone()).or_default() += 1;
        Ok(PendingCommit {
            inner: self.inner.clone(),
            document: document.clone(),
        })
    }

    /// Clears `document`'s mark when no commit to it is under way and
    /// `published` confirms its file renders the state accepted now.
    pub fn clear(
        &self,
        document: &CollaborationDocumentId,
        published: impl FnOnce() -> KernelResult<bool>,
    ) -> KernelResult<()> {
        let mut marked = self.inner.locked();
        if marked.get(document) != Some(&0) || !published()? {
            return Ok(());
        }
        let path = self.inner.path(document);
        match std::fs::remove_file(&path) {
            Ok(()) => {}
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
            Err(error) => return Err(internal(&path, error)),
        }
        marked.remove(document);
        Ok(())
    }

    /// Every marked document, in entity and resource order.
    pub fn marked(&self) -> Vec<CollaborationDocumentId> {
        let mut marked = self.inner.locked().keys().cloned().collect::<Vec<_>>();
        marked.sort_by(|left, right| {
            (&left.entity, &left.resource_id).cmp(&(&right.entity, &right.resource_id))
        });
        marked
    }

    pub fn is_marked(&self, document: &CollaborationDocumentId) -> bool {
        self.inner.locked().contains_key(document)
    }

    pub fn count(&self) -> usize {
        self.inner.locked().len()
    }

    /// Resolves once a commit to a marked document has finished since the
    /// last time this resolved.
    pub async fn commit_finished(&self) {
        self.inner.finished.notified().await
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn document(resource_id: &str) -> CollaborationDocumentId {
        CollaborationDocumentId::new("Note", resource_id)
    }

    #[test]
    fn a_mark_is_found_again_after_a_restart() {
        let data_dir = tempfile::tempdir().unwrap();
        let journal = PublicationJournal::open(
            data_dir.path(),
            walthamstow_kernel::durability::WriteDurability::System,
        )
        .unwrap();
        drop(journal.mark(&document("pending")).unwrap());

        let reopened = PublicationJournal::open(
            data_dir.path(),
            walthamstow_kernel::durability::WriteDurability::System,
        )
        .unwrap();

        assert_eq!(reopened.marked(), vec![document("pending")]);
    }

    #[test]
    fn a_cleared_mark_is_gone_after_a_restart() {
        let data_dir = tempfile::tempdir().unwrap();
        let journal = PublicationJournal::open(
            data_dir.path(),
            walthamstow_kernel::durability::WriteDurability::System,
        )
        .unwrap();
        drop(journal.mark(&document("published")).unwrap());
        journal.clear(&document("published"), || Ok(true)).unwrap();

        let reopened = PublicationJournal::open(
            data_dir.path(),
            walthamstow_kernel::durability::WriteDurability::System,
        )
        .unwrap();

        assert!(reopened.marked().is_empty());
        assert_eq!(reopened.count(), 0);
    }

    #[test]
    fn a_mark_stays_while_a_commit_to_its_document_is_under_way() {
        let data_dir = tempfile::tempdir().unwrap();
        let journal = PublicationJournal::open(
            data_dir.path(),
            walthamstow_kernel::durability::WriteDurability::System,
        )
        .unwrap();
        let earlier = journal.mark(&document("busy")).unwrap();
        drop(earlier);
        let under_way = journal.mark(&document("busy")).unwrap();

        journal.clear(&document("busy"), || Ok(true)).unwrap();
        assert!(journal.is_marked(&document("busy")));

        drop(under_way);
        journal.clear(&document("busy"), || Ok(true)).unwrap();
        assert!(!journal.is_marked(&document("busy")));
    }

    #[test]
    fn a_mark_stays_until_its_file_renders_the_accepted_state() {
        let data_dir = tempfile::tempdir().unwrap();
        let journal = PublicationJournal::open(
            data_dir.path(),
            walthamstow_kernel::durability::WriteDurability::System,
        )
        .unwrap();
        drop(journal.mark(&document("behind")).unwrap());

        journal.clear(&document("behind"), || Ok(false)).unwrap();

        assert!(journal.is_marked(&document("behind")));
        assert_eq!(
            PublicationJournal::open(
                data_dir.path(),
                walthamstow_kernel::durability::WriteDurability::System
            )
            .unwrap()
            .marked(),
            vec![document("behind")]
        );
    }

    #[test]
    fn clearing_an_unmarked_document_asks_nothing() {
        let data_dir = tempfile::tempdir().unwrap();
        let journal = PublicationJournal::open(
            data_dir.path(),
            walthamstow_kernel::durability::WriteDurability::System,
        )
        .unwrap();

        journal
            .clear(&document("never"), || {
                panic!("an unmarked document has nothing to confirm")
            })
            .unwrap();
    }
}
