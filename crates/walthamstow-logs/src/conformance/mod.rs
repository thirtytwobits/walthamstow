//! Copyright (c) Scott A Dixon
//!
//! The test suite every [`LogStore`] adapter runs: each rule of the store's contract as a named
//! test, and a property test that runs random sequences of operations against the adapter and
//! against a reference model of the contract, comparing every read.
//!
//! An adapter's tests implement [`Harness`] and invoke
//! [`log_store_conformance!`](crate::log_store_conformance)`(harness = ...)`. An adapter whose
//! documents can be broken from outside also implements [`Breaker`] and invokes
//! `log_store_conformance!(broken = ...)`.
//!
//! [`blobs`] is the blob section: [`BlobHarness`](blobs::BlobHarness),
//! [`BlobBreaker`](blobs::BlobBreaker) and
//! [`blob_store_conformance!`](crate::blob_store_conformance).
//!
//! The tests take their clocks from the suite and never sleep.

pub mod blobs;
mod model;
pub mod property;
pub mod rules;

use crate::fault::FaultKind;
use crate::store::LogStore;
use crate::types::{LogCommit, LogImage, NewRecord, Position, Revision, StoredRecord, WriteId};
use chrono::{DateTime, Utc};
use clerkenwell_events::{ActorKind, Principal};
use serde_json::Value;
use std::sync::{Arc, Mutex, MutexGuard, PoisonError};
use std::time::{Duration, SystemTime};
use walthamstow_kernel::clock::Moment;
use walthamstow_kernel::leases::{Lease, LeaseHolder, Leases, MemoryLeases};
use walthamstow_kernel::{DocumentId, KernelError, KernelResult, WallClock};

/// Builds the adapter under test.
pub trait Harness {
    /// An empty store whose writes `leases` fence and whose records are recorded on `clock`.
    fn store(&self, leases: Arc<dyn Leases>, clock: WallClock) -> Box<dyn LogStore>;
}

/// Breaks a document from outside the adapter under test.
pub trait Breaker: Harness {
    /// Breaks `document` in the store this harness built last, so a read of it reports a fault
    /// of `kind`.
    fn break_document(&self, document: &DocumentId, kind: FaultKind);
}

/// Runs the conformance suite as one test per rule.
///
/// `log_store_conformance!(harness = expr)` runs every rule and the property test over the
/// [`Harness`] `expr`. `log_store_conformance!(broken = expr)` runs the broken-document rules
/// over the [`Breaker`] `expr`. Each test evaluates `expr` afresh.
#[macro_export]
macro_rules! log_store_conformance {
    (harness = $harness:expr) => {
        $crate::log_store_conformance!(@rules $harness;
            a_write_under_a_lost_lease_is_refused_and_changes_nothing,
            a_write_changes_only_the_document_its_lease_names,
            a_write_over_another_revision_is_stale_and_changes_nothing,
            create_is_stale_while_the_document_exists,
            a_removed_document_has_no_head_and_is_stale_to_writes_and_reads,
            a_revision_is_never_given_twice_across_removal_and_creation,
            a_retried_write_is_given_what_it_was_given_and_writes_nothing_twice,
            only_the_last_write_is_retried,
            a_retry_is_recognised_only_under_the_lease_that_wrote,
            a_removal_and_another_write_never_retry_each_other,
            a_write_with_a_new_id_over_a_spent_base_is_stale,
            a_write_is_checked_for_its_lease_then_a_retry_then_its_base_then_what_it_carries,
            the_store_gives_positions_and_records_on_its_clock,
            a_commit_replaces_the_head_only_when_it_carries_one,
            a_commit_carrying_nothing_is_invalid,
            an_image_sets_the_next_position_one_past_its_highest,
            an_image_whose_positions_do_not_increase_is_invalid,
            a_rewind_removes_records_from_its_position_and_never_gives_a_position_twice,
            a_rewind_from_the_next_position_or_beyond_is_invalid,
            a_replace_keeps_the_next_position_unless_the_image_reaches_it,
            records_are_stale_once_the_document_moves_on,
            records_page_from_a_position,
            list_pages_an_entitys_documents_in_resource_order,
            an_empty_page_is_invalid,
            reads_change_nothing,
            blob_references_are_returned_as_stored,
        );
        #[test]
        fn the_store_agrees_with_the_reference_model() {
            $crate::conformance::property::the_store_agrees_with_the_reference_model(&$harness);
        }
    };
    (broken = $harness:expr) => {
        $crate::log_store_conformance!(@rules $harness;
            a_broken_document_is_listed_with_its_fault,
            head_and_records_refuse_a_broken_document_with_its_fault,
            a_missing_document_names_each_absent_part,
            a_read_leaves_a_broken_document_broken,
        );
    };
    (@rules $harness:expr; $($rule:ident),* $(,)?) => {
        $(
            #[test]
            fn $rule() {
                $crate::conformance::rules::$rule(&$harness);
            }
        )*
    };
}

/// How long a lease the suite grants stands.
const TERM: Duration = Duration::from_secs(30);

/// The entity the suite's documents belong to.
const ENTITY: &str = "Log";

fn locked<T>(mutex: &Mutex<T>) -> MutexGuard<'_, T> {
    mutex.lock().unwrap_or_else(PoisonError::into_inner)
}

/// A store under test, the leases that fence its writes, and the clocks both read, which move
/// only when a test moves them.
struct Fixture {
    store: Box<dyn LogStore>,
    leases: Arc<MemoryLeases>,
    moment: Arc<Mutex<Moment>>,
    wall: Arc<Mutex<SystemTime>>,
}

impl Fixture {
    fn new(harness: &dyn Harness) -> Self {
        let moment = Arc::new(Mutex::new(Moment::ORIGIN));
        let wall = Arc::new(Mutex::new(SystemTime::UNIX_EPOCH + Duration::from_secs(1)));
        let leases = Arc::new(MemoryLeases::new(TERM, {
            let moment = moment.clone();
            move || *locked(&moment)
        }));
        let clock = WallClock::new({
            let wall = wall.clone();
            move || *locked(&wall)
        });
        Self {
            store: harness.store(leases.clone(), clock),
            leases,
            moment,
            wall,
        }
    }

    /// The suite's lease over `document`, granted or renewed.
    fn lease(&self, document: &DocumentId) -> Lease {
        self.leases
            .acquire(document, &LeaseHolder::new("conformance"))
            .unwrap()
            .expect("the suite is the one holder")
    }

    /// Moves the leases' clock a term on, so every lease standing lapses.
    fn lapse(&self) {
        let mut moment = locked(&self.moment);
        *moment = *moment + TERM;
    }

    /// Moves the wall clock on by `seconds`, and returns the time it then reads.
    fn advance_wall(&self, seconds: u64) -> DateTime<Utc> {
        let mut wall = locked(&self.wall);
        *wall += Duration::from_secs(seconds);
        DateTime::from(*wall)
    }

    fn now(&self) -> DateTime<Utc> {
        DateTime::from(*locked(&self.wall))
    }

    /// Creates `document` with `records`, no records when empty, and returns its revision.
    fn create(&self, document: &DocumentId, records: &[StoredRecord]) -> Revision {
        self.store
            .create(&self.lease(document), &image("create", records.to_vec()))
            .unwrap()
    }

    /// Commits `payloads` as records to `document` over `base`.
    fn append(&self, document: &DocumentId, base: &Revision, payloads: &[i64]) -> Revision {
        self.store
            .commit(&self.lease(document), base, &commit("append", payloads))
            .unwrap()
    }

    /// Every record of `document` at `revision`.
    fn all(&self, document: &DocumentId, revision: &Revision) -> Vec<StoredRecord> {
        self.store
            .records(document, revision, Position::FIRST, usize::MAX)
            .unwrap()
    }

    /// The document's head and records as they stand, or nothing when there is no document.
    fn snapshot(&self, document: &DocumentId) -> Option<(crate::StoredHead, Vec<StoredRecord>)> {
        let head = self.store.head(document).unwrap()?;
        let records = self.all(document, &head.revision);
        Some((head, records))
    }
}

fn document(resource: &str) -> DocumentId {
    DocumentId::new(ENTITY, resource)
}

fn author(name: &str) -> Principal {
    Principal::new(name, ActorKind::Agent)
}

fn new_record(payload: i64) -> NewRecord {
    NewRecord {
        author: author("writer"),
        blobs: Vec::new(),
        payload: Value::from(payload),
    }
}

fn stored(position: u64, payload: i64, recorded: DateTime<Utc>) -> StoredRecord {
    StoredRecord {
        position: Position::new(position),
        author: author("writer"),
        recorded,
        blobs: Vec::new(),
        payload: Value::from(payload),
    }
}

fn image(id: &str, records: Vec<StoredRecord>) -> LogImage {
    LogImage {
        id: WriteId::new(id),
        format: 1,
        head: serde_json::json!({ "title": id }),
        records,
    }
}

fn commit(id: &str, payloads: &[i64]) -> LogCommit {
    LogCommit {
        id: WriteId::new(id),
        head: None,
        records: payloads.iter().copied().map(new_record).collect(),
    }
}

/// The reason code a refusal names.
fn reason<T: std::fmt::Debug>(result: &KernelResult<T>) -> Option<&str> {
    result.as_ref().err().and_then(KernelError::reason_code)
}

fn is_invalid<T>(result: &KernelResult<T>) -> bool {
    matches!(result, Err(error) if error.code == "invalid_request")
}

#[cfg(test)]
mod tests {
    //! The broken-document rules over a store whose documents a test breaks, so the suite's own
    //! rules are tested before an adapter that can be broken runs them.

    use super::*;
    use crate::fault::LogFault;
    use crate::types::{Listed, StoredHead};
    use crate::MemoryLogStore;
    use std::collections::BTreeMap;

    type Faults = Arc<Mutex<BTreeMap<DocumentId, LogFault>>>;

    #[derive(Debug)]
    struct Breakable {
        inner: MemoryLogStore,
        faults: Faults,
    }

    impl Breakable {
        fn fault(&self, document: &DocumentId) -> KernelResult<()> {
            match locked(&self.faults).get(document) {
                Some(fault) => Err(fault.error(document)),
                None => Ok(()),
            }
        }
    }

    impl LogStore for Breakable {
        fn create(&self, lease: &Lease, image: &LogImage) -> KernelResult<Revision> {
            self.inner.create(lease, image)
        }

        fn commit(
            &self,
            lease: &Lease,
            base: &Revision,
            commit: &LogCommit,
        ) -> KernelResult<Revision> {
            self.inner.commit(lease, base, commit)
        }

        fn rewind(
            &self,
            lease: &Lease,
            base: &Revision,
            from: Position,
            commit: &LogCommit,
        ) -> KernelResult<Revision> {
            self.inner.rewind(lease, base, from, commit)
        }

        fn replace(
            &self,
            lease: &Lease,
            base: &Revision,
            image: &LogImage,
        ) -> KernelResult<Revision> {
            self.inner.replace(lease, base, image)
        }

        fn remove(&self, lease: &Lease, base: &Revision, id: &WriteId) -> KernelResult<()> {
            self.inner.remove(lease, base, id)
        }

        fn head(&self, document: &DocumentId) -> KernelResult<Option<StoredHead>> {
            self.fault(document)?;
            self.inner.head(document)
        }

        fn records(
            &self,
            document: &DocumentId,
            revision: &Revision,
            from: Position,
            limit: usize,
        ) -> KernelResult<Vec<StoredRecord>> {
            self.fault(document)?;
            self.inner.records(document, revision, from, limit)
        }

        fn list(
            &self,
            entity: &str,
            after: Option<&str>,
            limit: usize,
        ) -> KernelResult<Vec<Listed>> {
            let faults = locked(&self.faults);
            Ok(self
                .inner
                .list(entity, after, limit)?
                .into_iter()
                .map(|listed| match faults.get(&listed.document) {
                    Some(fault) => Listed {
                        state: Err(fault.clone()),
                        ..listed
                    },
                    None => listed,
                })
                .collect())
        }
    }

    #[derive(Default)]
    struct BreakableHarness {
        faults: Faults,
    }

    impl Harness for BreakableHarness {
        fn store(&self, leases: Arc<dyn Leases>, clock: WallClock) -> Box<dyn LogStore> {
            Box::new(Breakable {
                inner: MemoryLogStore::new(leases, clock),
                faults: self.faults.clone(),
            })
        }
    }

    impl Breaker for BreakableHarness {
        fn break_document(&self, document: &DocumentId, kind: FaultKind) {
            let fault = match kind {
                FaultKind::Missing => LogFault::Missing {
                    parts: vec!["log".into()],
                },
                FaultKind::Invalid => LogFault::Invalid {
                    detail: "positions out of order".into(),
                },
                FaultKind::Inaccessible => LogFault::Inaccessible {
                    detail: "permission denied".into(),
                },
            };
            locked(&self.faults).insert(document.clone(), fault);
        }
    }

    crate::log_store_conformance!(broken = BreakableHarness::default());
}
