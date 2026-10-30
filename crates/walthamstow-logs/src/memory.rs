//! Copyright (c) Scott A Dixon
//!
//! Log documents kept in memory, for an engine with no disk and for tests.

use crate::reason_codes;
use crate::store::LogStore;
use crate::types::{
    Listed, LogCommit, LogImage, Position, Revision, StoredHead, StoredRecord, WriteId,
};
use chrono::{DateTime, Utc};
use serde_json::Value;
use std::collections::BTreeMap;
use std::fmt;
use std::ops::Bound;
use std::sync::{Arc, Mutex, MutexGuard, PoisonError};
use walthamstow_kernel::leases::{Lease, Leases};
use walthamstow_kernel::{DocumentId, KernelError, KernelResult, WallClock};

/// Log documents kept in memory, written under the leases the store is given and recorded on its
/// clock.
pub struct MemoryLogStore {
    leases: Arc<dyn Leases>,
    clock: WallClock,
    slots: Mutex<BTreeMap<DocumentId, Slot>>,
}

impl fmt::Debug for MemoryLogStore {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("MemoryLogStore")
            .field("leases", &self.leases)
            .finish_non_exhaustive()
    }
}

/// One document's place in the store: the document, if it exists, and the last write to it,
/// which outlives the document's removal.
#[derive(Default)]
struct Slot {
    document: Option<Document>,
    last: Option<LastWrite>,
}

struct Document {
    format: u32,
    head: Value,
    records: Vec<StoredRecord>,
    next: Position,
    revision: Revision,
}

/// The last write to a document, which a retry repeats: its id, its base, and the revision it
/// gave, or none for a removal.
struct LastWrite {
    id: WriteId,
    base: Option<Revision>,
    given: Option<Revision>,
}

impl Slot {
    /// What the last write gave, when a write of `id` over `base` repeats it.
    fn retried(&self, id: &WriteId, base: Option<&Revision>) -> Option<&Option<Revision>> {
        self.last
            .as_ref()
            .filter(|last| &last.id == id && last.base.as_ref() == base)
            .map(|last| &last.given)
    }

    /// The document while it is at `base`.
    fn at(&mut self, document: &DocumentId, base: &Revision) -> KernelResult<&mut Document> {
        self.document
            .as_mut()
            .filter(|stored| &stored.revision == base)
            .ok_or_else(|| stale(document, base))
    }
}

fn stale(document: &DocumentId, revision: &Revision) -> KernelError {
    KernelError::conflict(format!(
        "The log document {document} is not at revision {revision}."
    ))
    .with_reason(reason_codes::LOG_STALE)
}

fn locked<T>(mutex: &Mutex<T>) -> MutexGuard<'_, T> {
    mutex.lock().unwrap_or_else(PoisonError::into_inner)
}

fn require_limit(limit: usize) -> KernelResult<()> {
    if limit == 0 {
        return Err(KernelError::invalid_request(
            "A page of log documents or records holds at least one.",
        ));
    }
    Ok(())
}

impl MemoryLogStore {
    /// An empty store whose writes `leases` fence and whose records are recorded on `clock`.
    pub fn new(leases: Arc<dyn Leases>, clock: WallClock) -> Self {
        Self {
            leases,
            clock,
            slots: Mutex::new(BTreeMap::new()),
        }
    }

    /// Runs `write` on the slot of the lease's document while the lease stands.
    fn fenced<T>(
        &self,
        lease: &Lease,
        write: impl FnOnce(&mut Slot) -> KernelResult<T>,
    ) -> KernelResult<T> {
        self.leases.as_ref().fenced(lease, || {
            let mut slots = locked(&self.slots);
            write(slots.entry(lease.document().clone()).or_default())
        })
    }

    /// Runs `change` on the lease's document over `base` and advances its revision. `change`
    /// refuses what the write carries before it changes anything.
    fn revise(
        &self,
        lease: &Lease,
        base: &Revision,
        id: &WriteId,
        change: impl FnOnce(&mut Document) -> KernelResult<()>,
    ) -> KernelResult<Revision> {
        self.fenced(lease, |slot| {
            if let Some(Some(given)) = slot.retried(id, Some(base)) {
                return Ok(given.clone());
            }
            let document = slot.at(lease.document(), base)?;
            let revision = document.revision.successor()?;
            change(document)?;
            document.revision = revision.clone();
            slot.last = Some(LastWrite {
                id: id.clone(),
                base: Some(base.clone()),
                given: Some(revision.clone()),
            });
            Ok(revision)
        })
    }
}

/// The next position after appending `count` records from `next`.
fn after(next: Position, count: usize) -> KernelResult<Position> {
    u64::try_from(count)
        .ok()
        .and_then(|count| next.get().checked_add(count))
        .map(Position::new)
        .ok_or_else(|| KernelError::invalid_request("A log has no position after the last."))
}

/// Appends `commit`'s records, recorded at `recorded`, and replaces the head when the commit
/// carries one. Refused, with nothing changed, when the log has no positions left for them.
fn append(
    document: &mut Document,
    commit: &LogCommit,
    recorded: DateTime<Utc>,
) -> KernelResult<()> {
    let next = after(document.next, commit.records.len())?;
    let records = commit.records.iter().zip(document.next.get()..);
    document
        .records
        .extend(records.map(|(record, position)| StoredRecord {
            position: Position::new(position),
            author: record.author.clone(),
            recorded,
            blobs: record.blobs.clone(),
            payload: record.payload.clone(),
        }));
    document.next = next;
    if let Some(head) = &commit.head {
        document.head = head.clone();
    }
    Ok(())
}

impl LogStore for MemoryLogStore {
    fn create(&self, lease: &Lease, image: &LogImage) -> KernelResult<Revision> {
        self.fenced(lease, |slot| {
            if let Some(Some(given)) = slot.retried(&image.id, None) {
                return Ok(given.clone());
            }
            if slot.document.is_some() {
                return Err(KernelError::conflict(format!(
                    "The log document {} exists.",
                    lease.document()
                ))
                .with_reason(reason_codes::LOG_STALE));
            }
            let next = image.next_position()?;
            let revision = Revision::incarnate();
            slot.document = Some(Document {
                format: image.format,
                head: image.head.clone(),
                records: image.records.clone(),
                next,
                revision: revision.clone(),
            });
            slot.last = Some(LastWrite {
                id: image.id.clone(),
                base: None,
                given: Some(revision.clone()),
            });
            Ok(revision)
        })
    }

    fn commit(&self, lease: &Lease, base: &Revision, commit: &LogCommit) -> KernelResult<Revision> {
        self.revise(lease, base, &commit.id, |document| {
            if commit.records.is_empty() && commit.head.is_none() {
                return Err(KernelError::invalid_request(
                    "A commit carries records, a head or both.",
                ));
            }
            append(document, commit, self.clock.utc())
        })
    }

    fn rewind(
        &self,
        lease: &Lease,
        base: &Revision,
        from: Position,
        commit: &LogCommit,
    ) -> KernelResult<Revision> {
        self.revise(lease, base, &commit.id, |document| {
            if from >= document.next {
                return Err(KernelError::invalid_request(format!(
                    "A rewind from {from} starts at or beyond the next position, {}.",
                    document.next
                )));
            }
            after(document.next, commit.records.len())?;
            document.records.retain(|record| record.position < from);
            append(document, commit, self.clock.utc())
        })
    }

    fn replace(&self, lease: &Lease, base: &Revision, image: &LogImage) -> KernelResult<Revision> {
        self.revise(lease, base, &image.id, |document| {
            let next = image.next_position()?.max(document.next);
            document.format = image.format;
            document.head = image.head.clone();
            document.records = image.records.clone();
            document.next = next;
            Ok(())
        })
    }

    fn remove(&self, lease: &Lease, base: &Revision, id: &WriteId) -> KernelResult<()> {
        self.fenced(lease, |slot| {
            if let Some(None) = slot.retried(id, Some(base)) {
                return Ok(());
            }
            slot.at(lease.document(), base)?;
            slot.document = None;
            slot.last = Some(LastWrite {
                id: id.clone(),
                base: Some(base.clone()),
                given: None,
            });
            Ok(())
        })
    }

    fn head(&self, document: &DocumentId) -> KernelResult<Option<StoredHead>> {
        Ok(locked(&self.slots)
            .get(document)
            .and_then(|slot| slot.document.as_ref())
            .map(|stored| StoredHead {
                format: stored.format,
                head: stored.head.clone(),
                next: stored.next,
                revision: stored.revision.clone(),
            }))
    }

    fn records(
        &self,
        document: &DocumentId,
        revision: &Revision,
        from: Position,
        limit: usize,
    ) -> KernelResult<Vec<StoredRecord>> {
        let slots = locked(&self.slots);
        let stored = slots
            .get(document)
            .and_then(|slot| slot.document.as_ref())
            .filter(|stored| &stored.revision == revision)
            .ok_or_else(|| stale(document, revision))?;
        require_limit(limit)?;
        let start = stored
            .records
            .partition_point(|record| record.position < from);
        Ok(stored.records[start..]
            .iter()
            .take(limit)
            .cloned()
            .collect())
    }

    fn list(&self, entity: &str, after: Option<&str>, limit: usize) -> KernelResult<Vec<Listed>> {
        require_limit(limit)?;
        let start = match after {
            Some(after) => Bound::Excluded(DocumentId::new(entity, after)),
            None => Bound::Included(DocumentId::new(entity, "")),
        };
        Ok(locked(&self.slots)
            .range((start, Bound::Unbounded))
            .take_while(|(document, _)| document.entity() == entity)
            .filter_map(|(document, slot)| {
                slot.document.as_ref().map(|stored| Listed {
                    document: document.clone(),
                    state: Ok(stored.revision.clone()),
                })
            })
            .take(limit)
            .collect())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::conformance::Harness;

    struct MemoryHarness;

    impl Harness for MemoryHarness {
        fn store(&self, leases: Arc<dyn Leases>, clock: WallClock) -> Box<dyn LogStore> {
            Box::new(MemoryLogStore::new(leases, clock))
        }
    }

    crate::log_store_conformance!(harness = MemoryHarness);
}
