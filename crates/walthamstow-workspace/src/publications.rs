//! Which accepted workspace documents still wait for their files.
//!
//! The engine marks a document before it commits a change to it and clears
//! the mark once the document's file renders the accepted state, so a change
//! that a failed write or a stop left unpublished is found again, after a
//! restart too. Each mark is a file under the engine's
//! `collaboration-publications` journal.

use clerkenwell_store::CollaborationDocumentId;
use serde::{Deserialize, Serialize};
use std::collections::{BTreeMap, HashMap};
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex, MutexGuard, PoisonError, Weak};
use walthamstow_kernel::journals::Journal;
use walthamstow_kernel::{KernelError, KernelResult};

/// The journal the marks are kept under.
pub const JOURNAL: Journal = Journal::new("collaboration-publications");

/// The journals open in this process, by their canonical data directory, so every open of one
/// shares its marks and the commits under way to them.
static OPEN: Mutex<BTreeMap<PathBuf, Weak<Inner>>> = Mutex::new(BTreeMap::new());

#[derive(Debug, Clone)]
pub struct PublicationJournal {
    inner: Arc<Inner>,
}

#[derive(Debug)]
struct Inner {
    /// The engine data directory the journal lies under.
    data_dir: PathBuf,
    root: PathBuf,
    /// How far each mark is written before a commit relies on it.
    durability: walthamstow_kernel::durability::WriteDurability,
    /// Every marked document, with how many commits to it are under way and the file its
    /// mark is kept in.
    marked: Mutex<HashMap<CollaborationDocumentId, Marked>>,
    /// Woken when a commit to a marked document finishes.
    finished: tokio::sync::Notify,
}

#[derive(PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct Mark {
    entity: String,
    resource_id: String,
}

/// A marked document's commits under way, and the file its mark is kept in: the name it was
/// written under, which a mark from an earlier release keeps.
#[derive(Debug)]
struct Marked {
    under_way: usize,
    path: PathBuf,
}

/// A commit under way to a marked document. While one is, the mark stays.
#[derive(Debug)]
pub struct PendingCommit {
    inner: Arc<Inner>,
    document: CollaborationDocumentId,
}

impl Drop for PendingCommit {
    fn drop(&mut self) {
        if let Some(marked) = self.inner.locked().get_mut(&self.document) {
            marked.under_way = marked.under_way.saturating_sub(1);
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
    fn locked(&self) -> MutexGuard<'_, HashMap<CollaborationDocumentId, Marked>> {
        self.marked.lock().unwrap_or_else(PoisonError::into_inner)
    }

    /// Refuses the journal when a link has replaced its directory since it was opened.
    fn check_root(&self) -> KernelResult<()> {
        JOURNAL
            .unlinked_root(&self.data_dir)
            .map(drop)
            .map_err(|error| internal(&self.root, error))
    }

    /// Where a new mark for `document` is written: named by a hash of the entity and resource
    /// as a JSON pair, which no two documents share.
    fn path(&self, document: &CollaborationDocumentId) -> PathBuf {
        let pair = serde_json::to_vec(&(&document.entity, &document.resource_id))
            .expect("a pair of strings encodes");
        let name = walthamstow_kernel::hashing::sha256_hex(&pair);
        self.root.join(format!("{name}.json"))
    }
}

impl PublicationJournal {
    /// The journal under `data_dir`, with every mark a stop left in it. While a journal for
    /// `data_dir` is open in this process, opening it again returns that journal, which must
    /// have been opened with the same `durability`.
    pub fn open(
        data_dir: &Path,
        durability: walthamstow_kernel::durability::WriteDurability,
    ) -> KernelResult<Self> {
        let root = JOURNAL
            .unlinked_root(data_dir)
            .map_err(|error| internal(&JOURNAL.root(data_dir), error))?;
        let key = data_dir
            .canonicalize()
            .map_err(|error| internal(data_dir, error))?;
        let mut open = OPEN.lock().unwrap_or_else(PoisonError::into_inner);
        open.retain(|_, inner| inner.strong_count() > 0);
        if let Some(inner) = open.get(&key).and_then(Weak::upgrade) {
            if inner.durability != durability {
                return Err(internal(
                    &root,
                    "the journal is already open with another durability",
                ));
            }
            return Ok(Self { inner });
        }
        let mut marked = HashMap::new();
        let entries = match std::fs::read_dir(&root) {
            Ok(entries) => Some(entries),
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => None,
            Err(error) => return Err(internal(&root, error)),
        };
        for entry in entries.into_iter().flatten() {
            let entry = entry.map_err(|error| internal(&root, error))?;
            let path = entry.path();
            if path.extension().and_then(|extension| extension.to_str()) != Some("json") {
                continue;
            }
            if !entry
                .file_type()
                .map_err(|error| internal(&path, error))?
                .is_file()
            {
                return Err(internal(
                    &path,
                    "a mark is a regular file; this is not followed or opened",
                ));
            }
            let bytes = std::fs::read(&path).map_err(|error| internal(&path, error))?;
            let mark: Mark =
                serde_json::from_slice(&bytes).map_err(|error| internal(&path, error))?;
            marked.insert(
                CollaborationDocumentId::new(mark.entity, mark.resource_id),
                Marked { under_way: 0, path },
            );
        }
        let inner = Arc::new(Inner {
            data_dir: data_dir.to_path_buf(),
            root,
            durability,
            marked: Mutex::new(marked),
            finished: tokio::sync::Notify::new(),
        });
        open.insert(key, Arc::downgrade(&inner));
        Ok(Self { inner })
    }

    /// Marks `document` for a commit about to be made to it. The mark stays
    /// at least until the returned commit is dropped.
    pub fn mark(&self, document: &CollaborationDocumentId) -> KernelResult<PendingCommit> {
        let mut marked = self.inner.locked();
        self.inner.check_root()?;
        let path = marked
            .get(document)
            .map(|marked| marked.path.clone())
            .unwrap_or_else(|| self.inner.path(document));
        let mark = Mark {
            entity: document.entity.clone(),
            resource_id: document.resource_id.clone(),
        };
        // A mark found in place is trusted only as a regular file naming `document`, and is
        // settled first, since a failed mark may have left it unsynced. Anything else there,
        // or nothing, is written over.
        let held = match std::fs::symlink_metadata(&path) {
            Ok(metadata) if metadata.file_type().is_file() => {
                let bytes = std::fs::read(&path).map_err(|error| internal(&path, error))?;
                serde_json::from_slice::<Mark>(&bytes).ok()
            }
            Ok(_) => None,
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => None,
            Err(error) => return Err(internal(&path, error)),
        };
        if held.as_ref() == Some(&mark) {
            self.inner
                .durability
                .sync_directory(&self.inner.root)
                .map_err(|error| internal(&path, error))?;
        } else {
            let bytes = serde_json::to_vec(&mark).map_err(|error| internal(&path, error))?;
            walthamstow_kernel::durability::write_synced(self.inner.durability, &path, &bytes)
                .map_err(|error| internal(&path, error))?;
        }
        marked
            .entry(document.clone())
            .or_insert(Marked { under_way: 0, path })
            .under_way += 1;
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
        let Some(path) = marked
            .get(document)
            .filter(|marked| marked.under_way == 0)
            .map(|marked| marked.path.clone())
        else {
            return Ok(());
        };
        if !published()? {
            return Ok(());
        }
        self.inner.check_root()?;
        match std::fs::remove_file(&path) {
            Ok(()) => {}
            // An earlier clear removed it, and may not have synced its removal.
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
            Err(error) => return Err(internal(&path, error)),
        }
        self.inner
            .durability
            .sync_directory(&self.inner.root)
            .map_err(|error| internal(&self.inner.root, error))?;
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
    use walthamstow_kernel::durability::WriteDurability;

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

        drop(journal);

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

        drop(journal);

        let reopened = PublicationJournal::open(
            data_dir.path(),
            walthamstow_kernel::durability::WriteDurability::System,
        )
        .unwrap();

        assert!(reopened.marked().is_empty());
        assert_eq!(reopened.count(), 0);
    }

    #[test]
    fn a_cleared_mark_stays_gone_under_each_durability() {
        for durability in [WriteDurability::Device, WriteDurability::System] {
            let data_dir = tempfile::tempdir().unwrap();
            let journal = PublicationJournal::open(data_dir.path(), durability).unwrap();
            drop(journal.mark(&document("published")).unwrap());
            journal.clear(&document("published"), || Ok(true)).unwrap();
            drop(journal);
            let reopened = PublicationJournal::open(data_dir.path(), durability).unwrap();
            assert!(reopened.marked().is_empty());
        }
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
        drop(journal);
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

    #[test]
    fn documents_whose_identities_join_alike_keep_marks_of_their_own() {
        let data_dir = tempfile::tempdir().unwrap();
        let journal = PublicationJournal::open(data_dir.path(), WriteDurability::System).unwrap();
        let first = CollaborationDocumentId::new("a", "b/c");
        let second = CollaborationDocumentId::new("a/b", "c");
        drop(journal.mark(&first).unwrap());
        drop(journal.mark(&second).unwrap());
        drop(journal);
        let reopened = PublicationJournal::open(data_dir.path(), WriteDurability::System).unwrap();
        assert!(reopened.is_marked(&first) && reopened.is_marked(&second));
        assert_eq!(reopened.count(), 2);
    }

    #[test]
    fn a_mark_under_another_name_is_found_and_cleared() {
        let data_dir = tempfile::tempdir().unwrap();
        let root = JOURNAL.root(data_dir.path());
        let earlier = root.join("an-earlier-name.json");
        walthamstow_kernel::durability::write_synced(
            WriteDurability::System,
            &earlier,
            br#"{"entity":"Note","resource_id":"kept"}"#,
        )
        .unwrap();
        let journal = PublicationJournal::open(data_dir.path(), WriteDurability::System).unwrap();
        assert!(journal.is_marked(&document("kept")));
        journal.clear(&document("kept"), || Ok(true)).unwrap();
        assert!(!earlier.exists());
        drop(journal);
        let reopened = PublicationJournal::open(data_dir.path(), WriteDurability::System).unwrap();
        assert!(reopened.marked().is_empty());
    }

    #[test]
    fn a_clear_finds_its_mark_already_gone_and_still_settles() {
        for durability in [WriteDurability::Device, WriteDurability::System] {
            let data_dir = tempfile::tempdir().unwrap();
            let journal = PublicationJournal::open(data_dir.path(), durability).unwrap();
            drop(journal.mark(&document("gone")).unwrap());
            for entry in std::fs::read_dir(JOURNAL.root(data_dir.path())).unwrap() {
                std::fs::remove_file(entry.unwrap().path()).unwrap();
            }
            journal.clear(&document("gone"), || Ok(true)).unwrap();
            assert!(!journal.is_marked(&document("gone")));
        }
    }

    #[cfg(unix)]
    #[test]
    fn a_mark_that_is_not_a_regular_file_stops_the_journal_opening() {
        let data_dir = tempfile::tempdir().unwrap();
        let root = JOURNAL.root(data_dir.path());
        std::fs::create_dir_all(&root).unwrap();
        let outside = tempfile::tempdir().unwrap();
        std::fs::write(
            outside.path().join("mark.json"),
            br#"{"entity":"Note","resource_id":"x"}"#,
        )
        .unwrap();
        std::os::unix::fs::symlink(outside.path().join("mark.json"), root.join("linked.json"))
            .unwrap();
        assert!(PublicationJournal::open(data_dir.path(), WriteDurability::System).is_err());
    }

    #[cfg(unix)]
    #[test]
    fn a_linked_publication_journal_is_refused() {
        let data_dir = tempfile::tempdir().unwrap();
        let outside = tempfile::tempdir().unwrap();
        std::fs::create_dir_all(data_dir.path().join("journals")).unwrap();
        std::os::unix::fs::symlink(outside.path(), JOURNAL.root(data_dir.path())).unwrap();
        assert!(PublicationJournal::open(data_dir.path(), WriteDurability::System).is_err());
    }

    #[test]
    fn a_mark_held_in_memory_without_its_file_is_written_again() {
        let data_dir = tempfile::tempdir().unwrap();
        let journal = PublicationJournal::open(data_dir.path(), WriteDurability::System).unwrap();
        drop(journal.mark(&document("held")).unwrap());
        for entry in std::fs::read_dir(JOURNAL.root(data_dir.path())).unwrap() {
            std::fs::remove_file(entry.unwrap().path()).unwrap();
        }
        drop(journal.mark(&document("held")).unwrap());
        drop(journal);
        let reopened = PublicationJournal::open(data_dir.path(), WriteDurability::System).unwrap();
        assert!(reopened.is_marked(&document("held")));
    }

    #[cfg(unix)]
    #[test]
    fn a_mark_found_in_place_is_settled_before_it_is_trusted() {
        use std::os::unix::fs::PermissionsExt;
        let data_dir = tempfile::tempdir().unwrap();
        let journal = PublicationJournal::open(data_dir.path(), WriteDurability::System).unwrap();
        drop(journal.mark(&document("held")).unwrap());
        let root = JOURNAL.root(data_dir.path());
        // Unreadable, the journal directory can still be searched but not opened to sync.
        std::fs::set_permissions(&root, std::fs::Permissions::from_mode(0o300)).unwrap();
        let retried = journal.mark(&document("held"));
        std::fs::set_permissions(&root, std::fs::Permissions::from_mode(0o700)).unwrap();
        assert!(retried.is_err());
    }

    #[test]
    fn a_mark_found_naming_another_document_or_nothing_is_written_over() {
        for found in [
            &br#"{"entity":"Note","resource_id":"other"}"#[..],
            b"not a mark",
        ] {
            let data_dir = tempfile::tempdir().unwrap();
            let journal =
                PublicationJournal::open(data_dir.path(), WriteDurability::System).unwrap();
            drop(journal.mark(&document("held")).unwrap());
            for entry in std::fs::read_dir(JOURNAL.root(data_dir.path())).unwrap() {
                std::fs::write(entry.unwrap().path(), found).unwrap();
            }
            drop(journal.mark(&document("held")).unwrap());
            drop(journal);
            let reopened =
                PublicationJournal::open(data_dir.path(), WriteDurability::System).unwrap();
            assert!(reopened.is_marked(&document("held")));
            assert!(!reopened.is_marked(&document("other")));
        }
    }

    #[cfg(unix)]
    #[test]
    fn a_mark_found_as_a_link_is_written_over_and_its_target_left_alone() {
        let data_dir = tempfile::tempdir().unwrap();
        let outside = tempfile::tempdir().unwrap();
        let journal = PublicationJournal::open(data_dir.path(), WriteDurability::System).unwrap();
        drop(journal.mark(&document("held")).unwrap());
        let target = outside.path().join("mark.json");
        for entry in std::fs::read_dir(JOURNAL.root(data_dir.path())).unwrap() {
            let mark = entry.unwrap().path();
            std::fs::rename(&mark, &target).unwrap();
            std::os::unix::fs::symlink(&target, &mark).unwrap();
        }
        let before = std::fs::read(&target).unwrap();
        drop(journal.mark(&document("held")).unwrap());
        assert_eq!(std::fs::read(&target).unwrap(), before);
        drop(journal);
        let reopened = PublicationJournal::open(data_dir.path(), WriteDurability::System).unwrap();
        assert!(reopened.is_marked(&document("held")));
    }

    #[test]
    fn every_open_of_one_data_directory_shares_the_commits_under_way() {
        let data_dir = tempfile::tempdir().unwrap();
        let first = PublicationJournal::open(data_dir.path(), WriteDurability::System).unwrap();
        let under_way = first.mark(&document("busy")).unwrap();
        let spelled_otherwise = data_dir.path().join(".");
        let second = PublicationJournal::open(&spelled_otherwise, WriteDurability::System).unwrap();
        second.clear(&document("busy"), || Ok(true)).unwrap();
        assert!(second.is_marked(&document("busy")));
        drop(under_way);
        second.clear(&document("busy"), || Ok(true)).unwrap();
        assert!(!first.is_marked(&document("busy")));
        assert!(PublicationJournal::open(data_dir.path(), WriteDurability::Device).is_err());
    }

    #[cfg(unix)]
    #[test]
    fn a_journal_directory_replaced_by_a_link_is_neither_marked_nor_cleared() {
        let data_dir = tempfile::tempdir().unwrap();
        let outside = tempfile::tempdir().unwrap();
        let journal = PublicationJournal::open(data_dir.path(), WriteDurability::System).unwrap();
        drop(journal.mark(&document("held")).unwrap());
        let root = JOURNAL.root(data_dir.path());
        let moved = data_dir.path().join("moved");
        std::fs::rename(&root, &moved).unwrap();
        std::os::unix::fs::symlink(outside.path(), &root).unwrap();
        assert!(journal.mark(&document("other")).is_err());
        assert!(journal.clear(&document("held"), || Ok(true)).is_err());
        assert_eq!(std::fs::read_dir(outside.path()).unwrap().count(), 0);
        assert_eq!(std::fs::read_dir(&moved).unwrap().count(), 1);
    }
}
