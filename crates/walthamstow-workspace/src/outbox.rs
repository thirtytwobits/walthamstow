//! The outbox: which accepted workspace documents the canonical store has not yet
//! acknowledged.
//!
//! The engine marks a document before it commits a change to it. Publishing the document
//! renders its accepted state and publishes it to the canonical store under the document's
//! lease, and the mark clears once no commit is under way and the store holds the version
//! accepted now. A change that a failed write or a stop left unpublished is found again,
//! after a restart too. A removal's mark records the version it removed. Each mark is a record
//! in the engine's `collaboration-publications` journal.

use crate::canonical::{document_key, key_document, CanonicalStore};
use clerkenwell_store::{CollaborationDocumentId, CollaborationVersion};
use serde::{Deserialize, Serialize};
use std::collections::HashMap;
use std::sync::{Arc, Mutex, MutexGuard, PoisonError, Weak};
use walthamstow_kernel::journals::Journal;
use walthamstow_kernel::leases::{Holder, Lease, Leases};
use walthamstow_kernel::local_store::{JournalKey, LocalStore};
use walthamstow_kernel::{KernelError, KernelResult};

/// The journal the marks are kept in.
pub const JOURNAL: Journal = Journal::new("collaboration-publications");

/// An outbox open in this process, and the local store it keeps its marks in.
type Open = (Weak<dyn LocalStore>, Weak<Inner>);

/// The outboxes open in this process, by the local store they keep their marks in, so every
/// open over one store shares its marks, the commits under way to them and their turns.
static OPEN: Mutex<Vec<Open>> = Mutex::new(Vec::new());

/// The documents waiting for the canonical store, and how they reach it.
#[derive(Debug, Clone)]
pub struct Outbox {
    inner: Arc<Inner>,
}

#[derive(Debug)]
struct Inner {
    local: Arc<dyn LocalStore>,
    canonical: Arc<dyn CanonicalStore>,
    leases: Arc<dyn Leases>,
    holder: Holder,
    /// Every marked document, with how many commits to it are under way.
    marked: Mutex<HashMap<CollaborationDocumentId, usize>>,
    /// Woken when a commit to a marked document finishes.
    finished: tokio::sync::Notify,
    /// Each document's turn to publish, held from reading its accepted state to the store's
    /// answer, so publications of one document land in the order they read it. Kept while a
    /// publication holds or waits for it.
    turns: Mutex<HashMap<CollaborationDocumentId, Weak<tokio::sync::Mutex<()>>>>,
}

/// What a mark's record holds.
#[derive(Debug, Default, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct Mark {
    /// The version of the document a commit removed, once one does.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    removed: Option<CollaborationVersion>,
}

impl Mark {
    fn decode(key: &JournalKey, bytes: &[u8]) -> KernelResult<Self> {
        serde_json::from_slice(bytes).map_err(|error| {
            KernelError::internal(format!("Collaboration publication mark {key}: {error}"))
        })
    }

    fn encode(&self) -> Vec<u8> {
        serde_json::to_vec(self).expect("a mark encodes")
    }
}

/// A commit under way to a marked document. While one is, the mark stays.
#[derive(Debug)]
pub struct PendingCommit {
    inner: Arc<Inner>,
    document: CollaborationDocumentId,
}

impl PendingCommit {
    /// Records in the mark that this commit removes the document at `version`, so publishing
    /// the removal removes only what this working copy held.
    pub fn removes(&self, version: &CollaborationVersion) -> KernelResult<()> {
        let _marked = self.inner.locked();
        let key = document_key(JOURNAL, &self.document);
        let (bytes, tag) = self.inner.local.get(&key)?.ok_or_else(|| changed(&key))?;
        let mut mark = Mark::decode(&key, &bytes)?;
        if mark.removed.as_ref() == Some(version) {
            return Ok(());
        }
        mark.removed = Some(version.clone());
        self.inner
            .local
            .compare_and_swap(&key, Some(&tag), &mark.encode())?
            .ok_or_else(|| changed(&key))?;
        Ok(())
    }
}

impl Drop for PendingCommit {
    fn drop(&mut self) {
        if let Some(under_way) = self.inner.locked().get_mut(&self.document) {
            *under_way = under_way.saturating_sub(1);
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
    fn locked(&self) -> MutexGuard<'_, HashMap<CollaborationDocumentId, usize>> {
        self.marked.lock().unwrap_or_else(PoisonError::into_inner)
    }
}

/// Whether `held` and `store` are one store.
fn same<T: ?Sized>(held: &Weak<T>, store: &Arc<T>) -> bool {
    std::ptr::addr_eq(held.as_ptr(), Arc::as_ptr(store))
}

impl Outbox {
    /// The outbox whose marks `local` keeps, with every mark a stop left in it, publishing to
    /// `canonical` under the leases `leases` grants `holder`. While an outbox over `local` is
    /// open in this process, opening it again returns that outbox, which must publish to the
    /// same store under the same leases and holder.
    pub fn open(
        local: Arc<dyn LocalStore>,
        canonical: Arc<dyn CanonicalStore>,
        leases: Arc<dyn Leases>,
        holder: Holder,
    ) -> KernelResult<Self> {
        let mut open = OPEN.lock().unwrap_or_else(PoisonError::into_inner);
        open.retain(|(store, inner)| store.strong_count() > 0 && inner.strong_count() > 0);
        if let Some(inner) = open
            .iter()
            .find(|(held, _)| same(held, &local))
            .and_then(|(_, inner)| inner.upgrade())
        {
            if !Arc::ptr_eq(&inner.canonical, &canonical)
                || !Arc::ptr_eq(&inner.leases, &leases)
                || inner.holder != holder
            {
                return Err(KernelError::internal(
                    "The outbox is already open to another canonical store, leases or holder.",
                ));
            }
            return Ok(Self { inner });
        }
        let mut marked = HashMap::new();
        for key in local.list(&JournalKey::journal(JOURNAL))? {
            marked.insert(key_document(&key)?, 0);
        }
        let inner = Arc::new(Inner {
            local: local.clone(),
            canonical,
            leases,
            holder,
            marked: Mutex::new(marked),
            finished: tokio::sync::Notify::new(),
            turns: Mutex::new(HashMap::new()),
        });
        open.push((Arc::downgrade(&local), Arc::downgrade(&inner)));
        Ok(Self { inner })
    }

    /// The store the outbox publishes to.
    pub fn canonical(&self) -> &Arc<dyn CanonicalStore> {
        &self.inner.canonical
    }

    /// Marks `document` for a commit about to be made to it. The mark stays
    /// at least until the returned commit is dropped.
    pub fn mark(&self, document: &CollaborationDocumentId) -> KernelResult<PendingCommit> {
        let mut marked = self.inner.locked();
        let key = document_key(JOURNAL, document);
        if self.inner.local.get(&key)?.is_none() {
            self.inner
                .local
                .compare_and_swap(&key, None, &Mark::default().encode())?
                .ok_or_else(|| changed(&key))?;
        }
        *marked.entry(document.clone()).or_insert(0) += 1;
        Ok(PendingCommit {
            inner: self.inner.clone(),
            document: document.clone(),
        })
    }

    /// The version of `document` a commit removed, as its mark records it; `None` while no
    /// mark records one.
    pub fn removed(
        &self,
        document: &CollaborationDocumentId,
    ) -> KernelResult<Option<CollaborationVersion>> {
        let key = document_key(JOURNAL, document);
        let Some((bytes, _)) = self.inner.local.get(&key)? else {
            return Ok(None);
        };
        Ok(Mark::decode(&key, &bytes)?.removed)
    }

    /// Clears `document`'s mark when no commit to it is under way and
    /// `published` confirms the store holds the state accepted now.
    pub(crate) fn clear(
        &self,
        document: &CollaborationDocumentId,
        published: impl FnOnce() -> KernelResult<bool>,
    ) -> KernelResult<()> {
        let mut marked = self.inner.locked();
        if marked.get(document) != Some(&0) {
            return Ok(());
        }
        if !published()? {
            return Ok(());
        }
        let key = document_key(JOURNAL, document);
        // An earlier clear may have removed it already.
        if let Some((_, tag)) = self.inner.local.get(&key)? {
            if !self.inner.local.remove(&key, &tag)? {
                return Err(changed(&key));
            }
        }
        marked.remove(document);
        Ok(())
    }

    /// Waits for `document`'s turn to publish, and holds it until the guard drops.
    pub(crate) async fn turn(
        &self,
        document: &CollaborationDocumentId,
    ) -> tokio::sync::OwnedMutexGuard<()> {
        let turn = {
            let mut turns = self
                .inner
                .turns
                .lock()
                .unwrap_or_else(PoisonError::into_inner);
            turns.retain(|_, turn| turn.strong_count() > 0);
            match turns.get(document).and_then(Weak::upgrade) {
                Some(turn) => turn,
                None => {
                    let turn = Arc::new(tokio::sync::Mutex::new(()));
                    turns.insert(document.clone(), Arc::downgrade(&turn));
                    turn
                }
            }
        };
        turn.lock_owned().await
    }

    /// The lease on `document` for the outbox's holder, or a conflict while another holds it.
    pub(crate) async fn lease(&self, document: &CollaborationDocumentId) -> KernelResult<Lease> {
        self.inner
            .leases
            .acquire(document, &self.inner.holder)
            .await?
            .ok_or_else(|| {
                KernelError::conflict(format!(
                    "Another engine holds the lease on {} \"{}\".",
                    document.entity, document.resource_id
                ))
            })
    }

    pub(crate) async fn release(&self, lease: &Lease) -> KernelResult<()> {
        self.inner.leases.release(lease).await
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
    use crate::canonical::MemoryCanonicalStore;
    use std::path::Path;
    use walthamstow_kernel::durability::WriteDurability;
    use walthamstow_kernel::leases::MemoryLeases;
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

    /// The outbox whose marks `local` keeps, publishing to one memory store every test shares.
    fn outbox(local: Arc<dyn LocalStore>) -> KernelResult<Outbox> {
        type Shared = (Arc<dyn CanonicalStore>, Arc<dyn Leases>);
        static SHARED: std::sync::OnceLock<Shared> = std::sync::OnceLock::new();
        let (canonical, leases) = SHARED.get_or_init(|| {
            let leases = Arc::new(MemoryLeases::new(
                walthamstow_kernel::WallClock::new(|| std::time::UNIX_EPOCH),
                std::time::Duration::from_secs(60),
            ));
            (Arc::new(MemoryCanonicalStore::new(leases.clone())), leases)
        });
        Outbox::open(
            local,
            canonical.clone(),
            leases.clone(),
            Holder::new("engine").unwrap(),
        )
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

    #[tokio::test]
    async fn a_turn_no_publication_holds_or_waits_for_is_let_go() {
        let outbox = outbox(memory()).unwrap();
        drop(outbox.turn(&document("published")).await);
        let _held = outbox.turn(&document("publishing")).await;
        assert_eq!(outbox.inner.turns.lock().unwrap().len(), 1);
    }

    #[test]
    fn a_mark_is_found_again_after_a_restart() {
        let store = memory();
        let journal = outbox(store.clone()).unwrap();
        drop(journal.mark(&document("pending")).unwrap());
        drop(journal);
        let reopened = outbox(store).unwrap();
        assert_eq!(reopened.marked(), vec![document("pending")]);
    }

    #[test]
    fn a_document_of_any_identity_is_marked_and_found_again() {
        let store = memory();
        let journal = outbox(store.clone()).unwrap();
        let documents = [
            CollaborationDocumentId::new("Note", ""),
            CollaborationDocumentId::new("", "x"),
            CollaborationDocumentId::new("Note", "a/b\"c"),
        ];
        for document in &documents {
            drop(journal.mark(document).unwrap());
        }
        drop(journal);
        let reopened = outbox(store).unwrap();
        for document in &documents {
            assert!(reopened.is_marked(document), "{document:?}");
            reopened.clear(document, || Ok(true)).unwrap();
            assert!(!reopened.is_marked(document), "{document:?}");
        }
    }

    #[test]
    fn a_removal_its_mark_records_is_found_again_after_a_restart() {
        let store = memory();
        let journal = outbox(store.clone()).unwrap();
        let removed = CollaborationVersion {
            incarnation: "removed".into(),
            peers: [(1, 2)].into(),
        };
        let pending = journal.mark(&document("removed")).unwrap();
        assert_eq!(journal.removed(&document("removed")).unwrap(), None);
        pending.removes(&removed).unwrap();
        pending.removes(&removed).unwrap();
        drop(pending);
        drop(journal);
        let reopened = outbox(store).unwrap();
        assert_eq!(
            reopened.removed(&document("removed")).unwrap(),
            Some(removed)
        );
        reopened.clear(&document("removed"), || Ok(true)).unwrap();
        assert_eq!(reopened.removed(&document("removed")).unwrap(), None);
    }

    #[test]
    fn a_cleared_mark_is_gone_after_a_restart() {
        let store = memory();
        let journal = outbox(store.clone()).unwrap();
        drop(journal.mark(&document("published")).unwrap());
        journal.clear(&document("published"), || Ok(true)).unwrap();
        drop(journal);
        let reopened = outbox(store).unwrap();
        assert!(reopened.marked().is_empty());
        assert_eq!(reopened.count(), 0);
    }

    #[test]
    fn a_cleared_mark_stays_gone_under_each_durability() {
        for durability in [WriteDurability::Device, WriteDurability::System] {
            let data_dir = tempfile::tempdir().unwrap();
            let journal = outbox(files(data_dir.path(), durability)).unwrap();
            drop(journal.mark(&document("published")).unwrap());
            journal.clear(&document("published"), || Ok(true)).unwrap();
            drop(journal);
            let reopened = outbox(files(data_dir.path(), durability)).unwrap();
            assert!(reopened.marked().is_empty());
        }
    }

    #[test]
    fn a_mark_stays_while_a_commit_to_its_document_is_under_way() {
        let journal = outbox(memory()).unwrap();
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
        let journal = outbox(store.clone()).unwrap();
        drop(journal.mark(&document("behind")).unwrap());

        journal.clear(&document("behind"), || Ok(false)).unwrap();

        assert!(journal.is_marked(&document("behind")));
        drop(journal);
        assert_eq!(outbox(store).unwrap().marked(), vec![document("behind")]);
    }

    #[test]
    fn clearing_an_unmarked_document_asks_nothing() {
        let journal = outbox(memory()).unwrap();
        journal
            .clear(&document("never"), || {
                panic!("an unmarked document has nothing to confirm")
            })
            .unwrap();
    }

    #[test]
    fn documents_whose_identities_join_alike_keep_marks_of_their_own() {
        let store = memory();
        let journal = outbox(store.clone()).unwrap();
        let first = CollaborationDocumentId::new("a", "b/c");
        let second = CollaborationDocumentId::new("a/b", "c");
        drop(journal.mark(&first).unwrap());
        drop(journal.mark(&second).unwrap());
        drop(journal);
        let reopened = outbox(store).unwrap();
        assert!(reopened.is_marked(&first) && reopened.is_marked(&second));
        assert_eq!(reopened.count(), 2);
    }

    #[test]
    fn a_mark_an_earlier_release_wrote_stops_the_outbox_opening() {
        let data_dir = tempfile::tempdir().unwrap();
        walthamstow_kernel::durability::write_synced(
            WriteDurability::System,
            &JOURNAL.root(data_dir.path()).join("an-earlier-name.json"),
            br#"{"entity":"Note","resource_id":"kept"}"#,
        )
        .unwrap();
        let refused = outbox(files(data_dir.path(), WriteDurability::System)).unwrap_err();
        assert!(refused.message.contains("earlier release"), "{refused:?}");
    }

    #[test]
    fn a_clear_finds_its_mark_already_gone_and_still_clears() {
        let store = memory();
        let journal = outbox(store.clone()).unwrap();
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
        let journal = outbox(store.clone()).unwrap();
        drop(journal.mark(&document("held")).unwrap());
        let key = only_mark(&store);
        let (_, tag) = store.get(&key).unwrap().unwrap();
        assert!(store.remove(&key, &tag).unwrap());
        drop(journal.mark(&document("held")).unwrap());
        drop(journal);
        let reopened = outbox(store).unwrap();
        assert!(reopened.is_marked(&document("held")));
    }

    #[test]
    fn every_open_over_one_store_shares_the_commits_under_way() {
        let store = memory();
        let first = outbox(store.clone()).unwrap();
        let under_way = first.mark(&document("busy")).unwrap();
        let second = outbox(store).unwrap();
        second.clear(&document("busy"), || Ok(true)).unwrap();
        assert!(second.is_marked(&document("busy")));
        drop(under_way);
        second.clear(&document("busy"), || Ok(true)).unwrap();
        assert!(!first.is_marked(&document("busy")));
    }

    #[test]
    fn every_open_of_one_data_directory_shares_the_commits_under_way() {
        let data_dir = tempfile::tempdir().unwrap();
        let first = outbox(files(data_dir.path(), WriteDurability::System)).unwrap();
        let under_way = first.mark(&document("busy")).unwrap();
        let spelled_otherwise = data_dir.path().join(".");
        let second = outbox(files(&spelled_otherwise, WriteDurability::System)).unwrap();
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
        assert!(outbox(files(data_dir.path(), WriteDurability::System)).is_err());
    }

    #[cfg(unix)]
    #[test]
    fn a_linked_publication_journal_is_refused() {
        let data_dir = tempfile::tempdir().unwrap();
        let outside = tempfile::tempdir().unwrap();
        std::fs::create_dir_all(data_dir.path().join("journals")).unwrap();
        std::os::unix::fs::symlink(outside.path(), JOURNAL.root(data_dir.path())).unwrap();
        assert!(outbox(files(data_dir.path(), WriteDurability::System)).is_err());
    }

    #[cfg(unix)]
    #[test]
    fn a_mark_found_in_place_is_settled_before_it_is_trusted() {
        use std::os::unix::fs::PermissionsExt;
        let data_dir = tempfile::tempdir().unwrap();
        let journal = outbox(files(data_dir.path(), WriteDurability::System)).unwrap();
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
        let journal = outbox(files(data_dir.path(), WriteDurability::System)).unwrap();
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
        let journal = outbox(files(data_dir.path(), WriteDurability::System)).unwrap();
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
