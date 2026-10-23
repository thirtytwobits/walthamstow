//! Copyright (c) Scott A Dixon
//!
//! Who may change a document in ways that do not merge: a turn in progress, a compaction, a
//! handoff, or publishing to the canonical store. A lease names its holder and an epoch that
//! rises with each grant, so a store refuses a write under a lease it has moved past. A store
//! checks the lease in the same atomic step as the write it fences.
//!
//! [`FileLeases`] grants one lease over every document of a workspace: the workspace lock.
//! [`MemoryLeases`] grants each document its own lease, which lapses on the engine's clock.

use crate::journals::Journal;
use crate::local_store::{JournalKey, LocalStore};
use crate::workspace_lock::WorkspaceLock;
use crate::{KernelError, KernelResult, WallClock};
use async_trait::async_trait;
use clerkenwell_store::CollaborationDocumentId;
use std::collections::HashMap;
use std::fmt;
use std::sync::{Mutex, MutexGuard, PoisonError};
use std::time::Duration;

/// Who holds a lease: an engine, named the same across its restarts.
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub struct Holder(String);

impl Holder {
    pub fn new(name: impl Into<String>) -> KernelResult<Self> {
        let name = name.into();
        if name.trim().is_empty() {
            return Err(KernelError::invalid_request("A lease holder needs a name."));
        }
        Ok(Self(name))
    }

    pub fn as_str(&self) -> &str {
        &self.0
    }
}

/// The right to change one document in ways that do not merge.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Lease {
    pub document: CollaborationDocumentId,
    pub holder: Holder,
    /// Rises with each grant of the document's lease to a new holding.
    pub epoch: u64,
    /// When the lease lapses, in milliseconds since the Unix epoch on the engine's wall clock;
    /// `None` for a lease held for as long as its holder runs.
    pub expires_at_ms: Option<i64>,
}

/// Grants leases. Expiry reads the engine's clock.
#[async_trait]
pub trait Leases: Send + Sync + fmt::Debug {
    /// A lease on `document` for `holder`; `None` while another holder's lease on it holds.
    /// A holder that already holds the lease gets it renewed.
    async fn acquire(
        &self,
        document: &CollaborationDocumentId,
        holder: &Holder,
    ) -> KernelResult<Option<Lease>>;

    /// `lease`, extended; `None` when it no longer holds.
    async fn renew(&self, lease: &Lease) -> KernelResult<Option<Lease>>;

    /// Gives `lease` up. Releasing a lease that no longer holds changes nothing.
    async fn release(&self, lease: &Lease) -> KernelResult<()>;
}

/// The journal keeping the epoch of each hold of the workspace lock.
pub const LEASES: Journal = Journal::new("leases");

/// One lease over every document of a workspace: the engine's workspace lock, held for as
/// long as the engine runs. Its holder is the engine that took the lock, and its epoch names
/// this hold of it, one past the epoch of the hold before it.
pub struct FileLeases {
    _lock: WorkspaceLock,
    holder: Holder,
    epoch: u64,
}

impl fmt::Debug for FileLeases {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("FileLeases")
            .field("holder", &self.holder)
            .field("epoch", &self.epoch)
            .finish_non_exhaustive()
    }
}

impl FileLeases {
    /// The leases `lock` grants `holder`, under the epoch after the last one `local` records.
    pub fn new(lock: WorkspaceLock, holder: Holder, local: &dyn LocalStore) -> KernelResult<Self> {
        let key = JournalKey::new(LEASES, ["workspace.json"])?;
        let held = local.get(&key)?;
        let last = held
            .as_ref()
            .map(|(bytes, _)| serde_json::from_slice::<serde_json::Value>(bytes))
            .transpose()
            .map_err(|error| KernelError::internal(format!("Lease record {key}: {error}")))?
            .map(|record| {
                record["epoch"].as_u64().ok_or_else(|| {
                    KernelError::internal(format!("Lease record {key} names no epoch."))
                })
            })
            .transpose()?
            .unwrap_or(0);
        let epoch = last
            .checked_add(1)
            .ok_or_else(|| KernelError::internal("The workspace lease epochs are spent."))?;
        let record = serde_json::to_vec(&serde_json::json!({ "epoch": epoch }))
            .expect("a lease record encodes");
        local
            .compare_and_swap(&key, held.as_ref().map(|(_, tag)| tag), &record)?
            .ok_or_else(|| {
                KernelError::internal(format!("Lease record {key} changed while it was written."))
            })?;
        Ok(Self {
            _lock: lock,
            holder,
            epoch,
        })
    }

    /// Whether `lease` is this hold's lease. A file store checks it as it writes; the
    /// workspace lock holds every write against another engine.
    pub fn holds(&self, lease: &Lease) -> bool {
        lease.holder == self.holder && lease.epoch == self.epoch
    }

    fn lease(&self, document: &CollaborationDocumentId) -> Lease {
        Lease {
            document: document.clone(),
            holder: self.holder.clone(),
            epoch: self.epoch,
            expires_at_ms: None,
        }
    }
}

#[async_trait]
impl Leases for FileLeases {
    async fn acquire(
        &self,
        document: &CollaborationDocumentId,
        holder: &Holder,
    ) -> KernelResult<Option<Lease>> {
        Ok((holder == &self.holder).then(|| self.lease(document)))
    }

    async fn renew(&self, lease: &Lease) -> KernelResult<Option<Lease>> {
        Ok(self.holds(lease).then(|| self.lease(&lease.document)))
    }

    async fn release(&self, _lease: &Lease) -> KernelResult<()> {
        Ok(())
    }
}

/// A document's lease as [`MemoryLeases`] keeps it.
#[derive(Debug)]
struct Granted {
    holder: Holder,
    epoch: u64,
    expires_at_ms: i64,
}

/// Each document's own lease, kept in memory, lapsing a term after it was granted or renewed.
pub struct MemoryLeases {
    clock: WallClock,
    term: Duration,
    granted: Mutex<HashMap<CollaborationDocumentId, Granted>>,
}

impl fmt::Debug for MemoryLeases {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("MemoryLeases")
            .field("term", &self.term)
            .finish_non_exhaustive()
    }
}

impl MemoryLeases {
    /// Leases that lapse `term` after each grant or renewal, on `clock`.
    pub fn new(clock: WallClock, term: Duration) -> Self {
        Self {
            clock,
            term,
            granted: Mutex::new(HashMap::new()),
        }
    }

    fn granted(&self) -> MutexGuard<'_, HashMap<CollaborationDocumentId, Granted>> {
        self.granted.lock().unwrap_or_else(PoisonError::into_inner)
    }

    fn expiry(&self) -> i64 {
        let term = i64::try_from(self.term.as_millis()).unwrap_or(i64::MAX);
        self.clock.epoch_ms().saturating_add(term)
    }

    fn lease(document: &CollaborationDocumentId, granted: &Granted) -> Lease {
        Lease {
            document: document.clone(),
            holder: granted.holder.clone(),
            epoch: granted.epoch,
            expires_at_ms: Some(granted.expires_at_ms),
        }
    }

    /// Whether `granted` is `lease`, unexpired now.
    fn current(&self, granted: Option<&Granted>, lease: &Lease) -> bool {
        granted.is_some_and(|granted| {
            granted.holder == lease.holder
                && granted.epoch == lease.epoch
                && self.clock.epoch_ms() < granted.expires_at_ms
        })
    }

    /// Runs `write` only while `lease` holds, in one step with the check: no grant to another
    /// holder lands between them. `None` when the lease does not hold.
    pub fn while_held<T>(&self, lease: &Lease, write: impl FnOnce() -> T) -> Option<T> {
        let granted = self.granted();
        self.current(granted.get(&lease.document), lease)
            .then(write)
    }
}

#[async_trait]
impl Leases for MemoryLeases {
    async fn acquire(
        &self,
        document: &CollaborationDocumentId,
        holder: &Holder,
    ) -> KernelResult<Option<Lease>> {
        let mut granted = self.granted();
        let now = self.clock.epoch_ms();
        let expires_at_ms = self.expiry();
        let epoch = match granted.get(document) {
            Some(held) if now < held.expires_at_ms && &held.holder != holder => return Ok(None),
            Some(held) if now < held.expires_at_ms => held.epoch,
            Some(held) => held.epoch + 1,
            None => 1,
        };
        let held = Granted {
            holder: holder.clone(),
            epoch,
            expires_at_ms,
        };
        let lease = Self::lease(document, &held);
        granted.insert(document.clone(), held);
        Ok(Some(lease))
    }

    async fn renew(&self, lease: &Lease) -> KernelResult<Option<Lease>> {
        let mut granted = self.granted();
        let expires_at_ms = self.expiry();
        if !self.current(granted.get(&lease.document), lease) {
            return Ok(None);
        }
        let held = granted
            .get_mut(&lease.document)
            .expect("a current lease is granted");
        held.expires_at_ms = expires_at_ms;
        Ok(Some(Self::lease(&lease.document, held)))
    }

    async fn release(&self, lease: &Lease) -> KernelResult<()> {
        let mut granted = self.granted();
        if self.current(granted.get(&lease.document), lease) {
            // The epoch is kept, so the next grant still rises past this one.
            let held = granted
                .get_mut(&lease.document)
                .expect("a current lease is granted");
            held.expires_at_ms = i64::MIN;
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::atomic::{AtomicI64, Ordering};
    use std::sync::Arc;
    use std::time::{Duration, UNIX_EPOCH};

    fn document(resource_id: &str) -> CollaborationDocumentId {
        CollaborationDocumentId::new("Note", resource_id)
    }

    fn holder(name: &str) -> Holder {
        Holder::new(name).unwrap()
    }

    /// Memory leases on a clock the test moves.
    fn memory(term_ms: u64) -> (MemoryLeases, Arc<AtomicI64>) {
        let now = Arc::new(AtomicI64::new(1_000));
        let read = now.clone();
        let clock = WallClock::new(move || {
            UNIX_EPOCH + Duration::from_millis(read.load(Ordering::SeqCst) as u64)
        });
        (
            MemoryLeases::new(clock, Duration::from_millis(term_ms)),
            now,
        )
    }

    #[tokio::test]
    async fn a_lease_is_granted_to_one_holder_until_it_lapses_or_is_released() {
        let (leases, now) = memory(100);
        let first = leases
            .acquire(&document("one"), &holder("first"))
            .await
            .unwrap()
            .expect("an unleased document is granted");
        assert!(leases
            .acquire(&document("one"), &holder("second"))
            .await
            .unwrap()
            .is_none());
        assert!(
            leases
                .acquire(&document("two"), &holder("second"))
                .await
                .unwrap()
                .is_some(),
            "each document has its own lease"
        );

        now.fetch_add(100, Ordering::SeqCst);
        let second = leases
            .acquire(&document("one"), &holder("second"))
            .await
            .unwrap()
            .expect("a lapsed lease is granted again");
        assert!(second.epoch > first.epoch);
        assert!(leases.renew(&first).await.unwrap().is_none());
        assert!(leases.while_held(&first, || ()).is_none());
        assert!(leases.while_held(&second, || ()).is_some());

        leases.release(&second).await.unwrap();
        let third = leases
            .acquire(&document("one"), &holder("first"))
            .await
            .unwrap()
            .expect("a released lease is granted again");
        assert!(third.epoch > second.epoch);
    }

    #[tokio::test]
    async fn a_renewed_lease_holds_for_another_term_under_the_same_epoch() {
        let (leases, now) = memory(100);
        let lease = leases
            .acquire(&document("one"), &holder("first"))
            .await
            .unwrap()
            .unwrap();
        now.fetch_add(60, Ordering::SeqCst);
        let renewed = leases.renew(&lease).await.unwrap().expect("it still holds");
        assert_eq!(renewed.epoch, lease.epoch);
        assert!(renewed.expires_at_ms > lease.expires_at_ms);
        now.fetch_add(60, Ordering::SeqCst);
        assert!(leases.while_held(&renewed, || ()).is_some());
        assert!(
            leases
                .acquire(&document("one"), &holder("second"))
                .await
                .unwrap()
                .is_none(),
            "a renewed lease is not lapsed"
        );
    }

    #[tokio::test]
    async fn the_workspace_lock_leases_every_document_to_its_holder_alone() {
        let root = tempfile::tempdir().unwrap();
        let local = crate::local_store::MemoryLocalStore::new();
        let leases = FileLeases::new(
            WorkspaceLock::acquire(root.path()).unwrap(),
            holder("engine"),
            &local,
        )
        .unwrap();
        let lease = leases
            .acquire(&document("one"), &holder("engine"))
            .await
            .unwrap()
            .expect("the lock's holder holds every document");
        assert!(leases.holds(&lease));
        assert_eq!(lease.expires_at_ms, None);
        assert!(leases
            .acquire(&document("one"), &holder("another"))
            .await
            .unwrap()
            .is_none());
        assert!(leases.renew(&lease).await.unwrap().is_some());
        drop(leases);

        let again = FileLeases::new(
            WorkspaceLock::acquire(root.path()).unwrap(),
            holder("engine"),
            &local,
        )
        .unwrap();
        assert!(
            !again.holds(&lease),
            "a lease from an earlier hold of the lock no longer holds"
        );
        let renewed = again
            .acquire(&document("one"), &holder("engine"))
            .await
            .unwrap()
            .unwrap();
        assert!(
            renewed.epoch > lease.epoch,
            "each hold of the lock leases under a later epoch"
        );
    }
}
