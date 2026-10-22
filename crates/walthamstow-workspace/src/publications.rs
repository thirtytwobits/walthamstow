//! Which accepted workspace documents still wait for their files.
//!
//! The engine marks a document before it commits a change to it and clears
//! the mark once the document's file renders the accepted state, so a change
//! that a failed write or a stop left unpublished is found again, after a
//! restart too. Each mark is a record in the engine's
//! `collaboration-publications` journal.

use clerkenwell_store::CollaborationDocumentId;
use serde::{Deserialize, Serialize};
use std::collections::HashMap;
use std::sync::{Arc, Mutex, MutexGuard, PoisonError, Weak};
use walthamstow_kernel::journals::Journal;
use walthamstow_kernel::local_store::{JournalKey, LocalStore};
use walthamstow_kernel::{KernelError, KernelResult};

/// The journal the marks are kept in.
pub const JOURNAL: Journal = Journal::new("collaboration-publications");

/// A journal open in this process, and the store it keeps its marks in.
type Open = (Weak<dyn LocalStore>, Weak<Inner>);

/// The journals open in this process, by the store they keep their marks in, so every open
/// over one store shares its marks and the commits under way to them.
static OPEN: Mutex<Vec<Open>> = Mutex::new(Vec::new());

#[derive(Debug, Clone)]
pub struct PublicationJournal {
    inner: Arc<Inner>,
}

#[derive(Debug)]
struct Inner {
    store: Arc<dyn LocalStore>,
    /// Every marked document, with how many commits to it are under way and the record its
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

/// A marked document's commits under way, and the record its mark is kept in: the name it
/// was written under, which a mark from an earlier release keeps.
#[derive(Debug)]
struct Marked {
    under_way: usize,
    key: JournalKey,
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

fn changed(key: &JournalKey) -> KernelError {
    KernelError::internal(format!(
        "Collaboration publication mark {key} changed while it was written."
    ))
}

impl Inner {
    fn locked(&self) -> MutexGuard<'_, HashMap<CollaborationDocumentId, Marked>> {
        self.marked.lock().unwrap_or_else(PoisonError::into_inner)
    }

    /// Where a new mark for `document` is written: named by a hash of the entity and resource
    /// as a JSON pair, which no two documents share.
    fn key(document: &CollaborationDocumentId) -> JournalKey {
        let pair = serde_json::to_vec(&(&document.entity, &document.resource_id))
            .expect("a pair of strings encodes");
        let name = walthamstow_kernel::hashing::sha256_hex(&pair);
        JournalKey::new(JOURNAL, [format!("{name}.json")]).expect("a digest names a record")
    }
}

/// Whether `store` is the store `held` refers to.
fn same_store(held: &Weak<dyn LocalStore>, store: &Arc<dyn LocalStore>) -> bool {
    std::ptr::addr_eq(held.as_ptr(), Arc::as_ptr(store))
}

impl PublicationJournal {
    /// The journal kept in `store`, with every mark a stop left in it. While a journal over
    /// `store` is open in this process, opening it again returns that journal.
    pub fn open(store: Arc<dyn LocalStore>) -> KernelResult<Self> {
        let mut open = OPEN.lock().unwrap_or_else(PoisonError::into_inner);
        open.retain(|(store, inner)| store.strong_count() > 0 && inner.strong_count() > 0);
        if let Some(inner) = open
            .iter()
            .find(|(held, _)| same_store(held, &store))
            .and_then(|(_, inner)| inner.upgrade())
        {
            return Ok(Self { inner });
        }
        let mut marked = HashMap::new();
        for key in store.list(&JournalKey::journal(JOURNAL))? {
            let [name] = key.names() else { continue };
            if !name.ends_with(".json") {
                continue;
            }
            let Some((bytes, _)) = store.get(&key)? else {
                continue;
            };
            let mark: Mark = serde_json::from_slice(&bytes).map_err(|error| {
                KernelError::internal(format!("Collaboration publication mark {key}: {error}"))
            })?;
            marked.insert(
                CollaborationDocumentId::new(mark.entity, mark.resource_id),
                Marked { under_way: 0, key },
            );
        }
        let inner = Arc::new(Inner {
            store: store.clone(),
            marked: Mutex::new(marked),
            finished: tokio::sync::Notify::new(),
        });
        open.push((Arc::downgrade(&store), Arc::downgrade(&inner)));
        Ok(Self { inner })
    }

    /// Marks `document` for a commit about to be made to it. The mark stays
    /// at least until the returned commit is dropped.
    pub fn mark(&self, document: &CollaborationDocumentId) -> KernelResult<PendingCommit> {
        let mut marked = self.inner.locked();
        let key = marked
            .get(document)
            .map(|marked| marked.key.clone())
            .unwrap_or_else(|| Inner::key(document));
        let mark = Mark {
            entity: document.entity.clone(),
            resource_id: document.resource_id.clone(),
        };
        // A mark found in place is kept when it names `document`. Anything else there, or
        // nothing, is written over.
        let held = self.inner.store.get(&key)?;
        let names_document = held.as_ref().is_some_and(|(bytes, _)| {
            serde_json::from_slice::<Mark>(bytes).ok().as_ref() == Some(&mark)
        });
        if !names_document {
            let bytes = serde_json::to_vec(&mark).expect("a mark encodes");
            self.inner
                .store
                .compare_and_swap(&key, held.as_ref().map(|(_, tag)| tag), &bytes)?
                .ok_or_else(|| changed(&key))?;
        }
        marked
            .entry(document.clone())
            .or_insert(Marked { under_way: 0, key })
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
        let Some(key) = marked
            .get(document)
            .filter(|marked| marked.under_way == 0)
            .map(|marked| marked.key.clone())
        else {
            return Ok(());
        };
        if !published()? {
            return Ok(());
        }
        // An earlier clear may have removed it already.
        if let Some((_, tag)) = self.inner.store.get(&key)? {
            if !self.inner.store.remove(&key, &tag)? {
                return Err(changed(&key));
            }
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
    use std::path::Path;
    use walthamstow_kernel::durability::WriteDurability;
    use walthamstow_kernel::local_store::{FileLocalStore, MemoryLocalStore};

    fn document(resource_id: &str) -> CollaborationDocumentId {
        CollaborationDocumentId::new("Note", resource_id)
    }

    fn memory() -> Arc<dyn LocalStore> {
        Arc::new(MemoryLocalStore::new())
    }

    fn files(data_dir: &Path, durability: WriteDurability) -> Arc<dyn LocalStore> {
        FileLocalStore::open(data_dir, durability).unwrap()
    }

    /// The one mark `store` holds.
    fn only_mark(store: &Arc<dyn LocalStore>) -> JournalKey {
        let [key] = store
            .list(&JournalKey::journal(JOURNAL))
            .unwrap()
            .try_into()
            .unwrap();
        key
    }

    #[test]
    fn a_mark_is_found_again_after_a_restart() {
        let store = memory();
        let journal = PublicationJournal::open(store.clone()).unwrap();
        drop(journal.mark(&document("pending")).unwrap());
        drop(journal);
        let reopened = PublicationJournal::open(store).unwrap();
        assert_eq!(reopened.marked(), vec![document("pending")]);
    }

    #[test]
    fn a_cleared_mark_is_gone_after_a_restart() {
        let store = memory();
        let journal = PublicationJournal::open(store.clone()).unwrap();
        drop(journal.mark(&document("published")).unwrap());
        journal.clear(&document("published"), || Ok(true)).unwrap();
        drop(journal);
        let reopened = PublicationJournal::open(store).unwrap();
        assert!(reopened.marked().is_empty());
        assert_eq!(reopened.count(), 0);
    }

    #[test]
    fn a_cleared_mark_stays_gone_under_each_durability() {
        for durability in [WriteDurability::Device, WriteDurability::System] {
            let data_dir = tempfile::tempdir().unwrap();
            let journal = PublicationJournal::open(files(data_dir.path(), durability)).unwrap();
            drop(journal.mark(&document("published")).unwrap());
            journal.clear(&document("published"), || Ok(true)).unwrap();
            drop(journal);
            let reopened = PublicationJournal::open(files(data_dir.path(), durability)).unwrap();
            assert!(reopened.marked().is_empty());
        }
    }

    #[test]
    fn a_mark_stays_while_a_commit_to_its_document_is_under_way() {
        let journal = PublicationJournal::open(memory()).unwrap();
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
        let store = memory();
        let journal = PublicationJournal::open(store.clone()).unwrap();
        drop(journal.mark(&document("behind")).unwrap());

        journal.clear(&document("behind"), || Ok(false)).unwrap();

        assert!(journal.is_marked(&document("behind")));
        drop(journal);
        assert_eq!(
            PublicationJournal::open(store).unwrap().marked(),
            vec![document("behind")]
        );
    }

    #[test]
    fn clearing_an_unmarked_document_asks_nothing() {
        let journal = PublicationJournal::open(memory()).unwrap();
        journal
            .clear(&document("never"), || {
                panic!("an unmarked document has nothing to confirm")
            })
            .unwrap();
    }

    #[test]
    fn documents_whose_identities_join_alike_keep_marks_of_their_own() {
        let store = memory();
        let journal = PublicationJournal::open(store.clone()).unwrap();
        let first = CollaborationDocumentId::new("a", "b/c");
        let second = CollaborationDocumentId::new("a/b", "c");
        drop(journal.mark(&first).unwrap());
        drop(journal.mark(&second).unwrap());
        drop(journal);
        let reopened = PublicationJournal::open(store).unwrap();
        assert!(reopened.is_marked(&first) && reopened.is_marked(&second));
        assert_eq!(reopened.count(), 2);
    }

    #[test]
    fn a_mark_under_another_name_is_found_and_cleared() {
        let data_dir = tempfile::tempdir().unwrap();
        let earlier = JOURNAL.root(data_dir.path()).join("an-earlier-name.json");
        walthamstow_kernel::durability::write_synced(
            WriteDurability::System,
            &earlier,
            br#"{"entity":"Note","resource_id":"kept"}"#,
        )
        .unwrap();
        let journal =
            PublicationJournal::open(files(data_dir.path(), WriteDurability::System)).unwrap();
        assert!(journal.is_marked(&document("kept")));
        journal.clear(&document("kept"), || Ok(true)).unwrap();
        assert!(!earlier.exists());
        drop(journal);
        let reopened =
            PublicationJournal::open(files(data_dir.path(), WriteDurability::System)).unwrap();
        assert!(reopened.marked().is_empty());
    }

    #[test]
    fn a_clear_finds_its_mark_already_gone_and_still_clears() {
        let store = memory();
        let journal = PublicationJournal::open(store.clone()).unwrap();
        drop(journal.mark(&document("gone")).unwrap());
        let key = only_mark(&store);
        let (_, tag) = store.get(&key).unwrap().unwrap();
        assert!(store.remove(&key, &tag).unwrap());
        journal.clear(&document("gone"), || Ok(true)).unwrap();
        assert!(!journal.is_marked(&document("gone")));
    }

    #[test]
    fn a_mark_held_in_memory_without_its_record_is_written_again() {
        let store = memory();
        let journal = PublicationJournal::open(store.clone()).unwrap();
        drop(journal.mark(&document("held")).unwrap());
        let key = only_mark(&store);
        let (_, tag) = store.get(&key).unwrap().unwrap();
        assert!(store.remove(&key, &tag).unwrap());
        drop(journal.mark(&document("held")).unwrap());
        drop(journal);
        let reopened = PublicationJournal::open(store).unwrap();
        assert!(reopened.is_marked(&document("held")));
    }

    #[test]
    fn a_mark_found_naming_another_document_or_nothing_is_written_over() {
        for found in [
            &br#"{"entity":"Note","resource_id":"other"}"#[..],
            b"not a mark",
        ] {
            let store = memory();
            let journal = PublicationJournal::open(store.clone()).unwrap();
            drop(journal.mark(&document("held")).unwrap());
            let key = only_mark(&store);
            let (_, tag) = store.get(&key).unwrap().unwrap();
            store
                .compare_and_swap(&key, Some(&tag), found)
                .unwrap()
                .unwrap();
            drop(journal.mark(&document("held")).unwrap());
            drop(journal);
            let reopened = PublicationJournal::open(store).unwrap();
            assert!(reopened.is_marked(&document("held")));
            assert!(!reopened.is_marked(&document("other")));
        }
    }

    #[test]
    fn every_open_over_one_store_shares_the_commits_under_way() {
        let store = memory();
        let first = PublicationJournal::open(store.clone()).unwrap();
        let under_way = first.mark(&document("busy")).unwrap();
        let second = PublicationJournal::open(store).unwrap();
        second.clear(&document("busy"), || Ok(true)).unwrap();
        assert!(second.is_marked(&document("busy")));
        drop(under_way);
        second.clear(&document("busy"), || Ok(true)).unwrap();
        assert!(!first.is_marked(&document("busy")));
    }

    #[test]
    fn every_open_of_one_data_directory_shares_the_commits_under_way() {
        let data_dir = tempfile::tempdir().unwrap();
        let first =
            PublicationJournal::open(files(data_dir.path(), WriteDurability::System)).unwrap();
        let under_way = first.mark(&document("busy")).unwrap();
        let spelled_otherwise = data_dir.path().join(".");
        let second =
            PublicationJournal::open(files(&spelled_otherwise, WriteDurability::System)).unwrap();
        second.clear(&document("busy"), || Ok(true)).unwrap();
        assert!(second.is_marked(&document("busy")));
        drop(under_way);
        second.clear(&document("busy"), || Ok(true)).unwrap();
        assert!(!first.is_marked(&document("busy")));
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
        assert!(PublicationJournal::open(files(data_dir.path(), WriteDurability::System)).is_err());
    }

    #[cfg(unix)]
    #[test]
    fn a_linked_publication_journal_is_refused() {
        let data_dir = tempfile::tempdir().unwrap();
        let outside = tempfile::tempdir().unwrap();
        std::fs::create_dir_all(data_dir.path().join("journals")).unwrap();
        std::os::unix::fs::symlink(outside.path(), JOURNAL.root(data_dir.path())).unwrap();
        assert!(PublicationJournal::open(files(data_dir.path(), WriteDurability::System)).is_err());
    }

    #[cfg(unix)]
    #[test]
    fn a_mark_found_in_place_is_settled_before_it_is_trusted() {
        use std::os::unix::fs::PermissionsExt;
        let data_dir = tempfile::tempdir().unwrap();
        let journal =
            PublicationJournal::open(files(data_dir.path(), WriteDurability::System)).unwrap();
        drop(journal.mark(&document("held")).unwrap());
        let root = JOURNAL.root(data_dir.path());
        // Unreadable, the journal directory can still be searched but not opened to sync.
        std::fs::set_permissions(&root, std::fs::Permissions::from_mode(0o300)).unwrap();
        let retried = journal.mark(&document("held"));
        std::fs::set_permissions(&root, std::fs::Permissions::from_mode(0o700)).unwrap();
        assert!(retried.is_err());
    }

    #[cfg(unix)]
    #[test]
    fn a_mark_found_as_a_link_is_refused_and_its_target_left_alone() {
        let data_dir = tempfile::tempdir().unwrap();
        let outside = tempfile::tempdir().unwrap();
        let journal =
            PublicationJournal::open(files(data_dir.path(), WriteDurability::System)).unwrap();
        drop(journal.mark(&document("held")).unwrap());
        let target = outside.path().join("mark.json");
        for entry in std::fs::read_dir(JOURNAL.root(data_dir.path())).unwrap() {
            let mark = entry.unwrap().path();
            std::fs::rename(&mark, &target).unwrap();
            std::os::unix::fs::symlink(&target, &mark).unwrap();
        }
        let before = std::fs::read(&target).unwrap();
        assert!(journal.mark(&document("held")).is_err());
        assert!(journal.clear(&document("held"), || Ok(true)).is_err());
        assert_eq!(std::fs::read(&target).unwrap(), before);
    }

    #[cfg(unix)]
    #[test]
    fn a_journal_directory_replaced_by_a_link_is_neither_marked_nor_cleared() {
        let data_dir = tempfile::tempdir().unwrap();
        let outside = tempfile::tempdir().unwrap();
        let journal =
            PublicationJournal::open(files(data_dir.path(), WriteDurability::System)).unwrap();
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
