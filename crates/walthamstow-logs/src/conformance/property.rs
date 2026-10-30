//! Copyright (c) Scott A Dixon
//!
//! The property test: random sequences of operations, run against an adapter and against the
//! reference model, comparing how every write ends and what every read returns. The sequences
//! include lapsed, released and superseded leases, stale bases and retried writes.
//!
//! Each run is seeded, so a failure names the seed and the steps that led to it, and repeats.

use super::model::{Model, ModelRevision, Outcome};
use super::{author, Fixture, Harness, ENTITY};
use crate::reason_codes::LOG_STALE;
use crate::types::{
    BlobRef, LogCommit, LogImage, NewRecord, Position, Revision, StoredRecord, WriteId,
};
use rand::rngs::SmallRng;
use rand::{Rng, SeedableRng};
use serde_json::{json, Value};
use std::collections::{BTreeMap, HashMap};
use std::fmt::Debug;
use walthamstow_kernel::leases::{Lease, Leases};
use walthamstow_kernel::reason_codes::LEASE_LOST;
use walthamstow_kernel::{DocumentId, KernelError, KernelResult};

/// How many seeded runs the test makes.
const SEEDS: u64 = 48;

/// How many operations each run makes.
const STEPS: usize = 160;

/// How many of the last steps a failure reports.
const REPORTED: usize = 24;

/// The documents a run writes: three of the suite's entity and one of another.
const DOCUMENTS: [(&str, &str); 4] = [(ENTITY, "a"), (ENTITY, "b"), (ENTITY, "c"), ("Other", "a")];

/// The ids a run's writes carry, few enough that writes share them.
const IDS: [&str; 4] = ["w0", "w1", "w2", "w3"];

/// Runs random sequences of operations against the adapter and the reference model, comparing
/// every outcome and every read.
pub fn the_store_agrees_with_the_reference_model(harness: &dyn Harness) {
    for seed in 0..SEEDS {
        Run::new(harness, seed).run();
    }
}

/// Pairs each revision the adapter gives with the one the model gives, one to one.
#[derive(Default)]
struct Revisions {
    to_model: HashMap<Revision, ModelRevision>,
    to_adapter: BTreeMap<ModelRevision, Revision>,
}

impl Revisions {
    /// Pairs a revision a write gave with the model's. False when either is paired with another.
    fn pair(&mut self, adapter: &Revision, model: ModelRevision) -> bool {
        match (self.to_model.get(adapter), self.to_adapter.get(&model)) {
            (None, None) => {
                self.to_model.insert(adapter.clone(), model);
                self.to_adapter.insert(model, adapter.clone());
                true
            }
            (Some(paired), Some(adapted)) => *paired == model && adapted == adapter,
            _ => false,
        }
    }

    /// Whether a revision a read returned is the one the model's is paired with.
    fn same(&self, adapter: &Revision, model: ModelRevision) -> bool {
        self.to_model.get(adapter) == Some(&model)
    }
}

#[derive(Debug, Clone)]
enum Write {
    Create {
        lease: Lease,
        image: LogImage,
    },
    Commit {
        lease: Lease,
        base: ModelRevision,
        commit: LogCommit,
    },
    Rewind {
        lease: Lease,
        base: ModelRevision,
        from: Position,
        commit: LogCommit,
    },
    Replace {
        lease: Lease,
        base: ModelRevision,
        image: LogImage,
    },
    Remove {
        lease: Lease,
        base: ModelRevision,
        id: WriteId,
    },
}

impl Write {
    fn lease(&self) -> &Lease {
        match self {
            Self::Create { lease, .. }
            | Self::Commit { lease, .. }
            | Self::Rewind { lease, .. }
            | Self::Replace { lease, .. }
            | Self::Remove { lease, .. } => lease,
        }
    }

    /// This write under `lease` instead.
    fn under(mut self, under: Lease) -> Self {
        match &mut self {
            Self::Create { lease, .. }
            | Self::Commit { lease, .. }
            | Self::Rewind { lease, .. }
            | Self::Replace { lease, .. }
            | Self::Remove { lease, .. } => *lease = under,
        }
        self
    }
}

struct Run {
    fixture: Fixture,
    seed: u64,
    rng: SmallRng,
    model: Model,
    revisions: Revisions,
    /// Every revision the model has given.
    known: Vec<ModelRevision>,
    /// The lease standing on each document.
    standing: BTreeMap<DocumentId, Lease>,
    /// Every lease granted.
    granted: Vec<Lease>,
    last: Option<Write>,
    /// The step the run is at, counted from 1.
    step: usize,
    trace: Vec<String>,
}

impl Run {
    fn new(harness: &dyn Harness, seed: u64) -> Self {
        Self {
            fixture: Fixture::new(harness),
            seed,
            rng: SmallRng::seed_from_u64(seed),
            model: Model::default(),
            revisions: Revisions::default(),
            known: Vec::new(),
            standing: BTreeMap::new(),
            granted: Vec::new(),
            last: None,
            step: 0,
            trace: Vec::new(),
        }
    }

    fn run(mut self) {
        for step in 1..=STEPS {
            self.step = step;
            self.step();
        }
        for (entity, resource) in DOCUMENTS {
            self.compare_document(&DocumentId::new(entity, resource));
        }
    }

    /// Records what the run does at this step, for a failure to report.
    fn note(&mut self, entry: impl Into<String>) {
        self.trace
            .push(format!("{:>4} {}", self.step, entry.into()));
    }

    fn fail(&self, message: impl std::fmt::Display) -> ! {
        let start = self.trace.len().saturating_sub(REPORTED);
        panic!(
            "seed {}, step {}: {message}\nthe last steps:\n{}",
            self.seed,
            self.step,
            self.trace[start..].join("\n")
        );
    }

    fn step(&mut self) {
        let document = self.document();
        match self.rng.random_range(0..20) {
            0 => {
                self.note(format!("acquire {document}"));
                self.acquire(&document);
            }
            1 => {
                self.note("lapse");
                self.fixture.lapse();
                self.standing.clear();
            }
            2 => {
                self.note(format!("release {document}"));
                if let Some(lease) = self.standing.remove(&document) {
                    self.fixture.leases.release(&lease).unwrap();
                }
            }
            3..=4 => self.create(&document),
            5..=8 => self.revise(&document, |run, lease, base| Write::Commit {
                lease,
                base,
                commit: run.commit(),
            }),
            9 => self.revise(&document, |run, lease, base| {
                let next = run.model.head(lease.document()).map_or(2, |head| head.next);
                Write::Rewind {
                    lease,
                    base,
                    from: Position::new(run.rng.random_range(0..next + 2)),
                    commit: run.commit(),
                }
            }),
            10 => self.revise(&document, |run, lease, base| Write::Replace {
                lease,
                base,
                image: run.image(),
            }),
            11 => self.revise(&document, |run, lease, base| Write::Remove {
                lease,
                base,
                id: run.id(),
            }),
            12 => match self.last.clone() {
                // A retry, mostly under the lease that wrote and sometimes under the one now
                // standing over the document, which may be another.
                Some(write) => {
                    let standing = self.standing.get(write.lease().document()).cloned();
                    match standing {
                        Some(standing) if self.rng.random_bool(0.3) => {
                            self.note("retry under the standing lease");
                            self.write(write.under(standing));
                        }
                        _ => {
                            self.note("retry");
                            self.write(write);
                        }
                    }
                }
                None => self.create(&document),
            },
            13 => match self.last.clone() {
                Some(write) => self.reuse(write),
                None => self.create(&document),
            },
            14..=15 => {
                self.note(format!("head {document}"));
                self.compare_head(&document);
            }
            16..=17 => self.records(&document),
            _ => self.list(),
        }
    }

    fn document(&mut self) -> DocumentId {
        let (entity, resource) = DOCUMENTS[self.rng.random_range(0..DOCUMENTS.len())];
        DocumentId::new(entity, resource)
    }

    fn acquire(&mut self, document: &DocumentId) -> Lease {
        let lease = self.fixture.lease(document);
        self.standing.insert(document.clone(), lease.clone());
        self.granted.push(lease.clone());
        lease
    }

    /// The lease a write uses: mostly the one standing, sometimes one lost.
    fn lease(&mut self, document: &DocumentId) -> Lease {
        let standing = self.standing.get(document).cloned();
        let lost: Vec<Lease> = self
            .granted
            .iter()
            .filter(|lease| {
                lease.document() == document
                    && standing
                        .as_ref()
                        .is_none_or(|standing| standing.epoch() != lease.epoch())
            })
            .cloned()
            .collect();
        match standing {
            Some(lease) if lost.is_empty() || self.rng.random_bool(0.85) => lease,
            _ if !lost.is_empty() => lost[self.rng.random_range(0..lost.len())].clone(),
            _ => self.acquire(document),
        }
    }

    /// The revision a write or read names: mostly the document's own, sometimes any other.
    fn revision(&mut self, document: &DocumentId) -> Option<ModelRevision> {
        let current = self.model.head(document).map(|head| head.revision);
        match current {
            Some(current) if self.known.len() < 2 || self.rng.random_bool(0.8) => Some(current),
            _ if !self.known.is_empty() => {
                Some(self.known[self.rng.random_range(0..self.known.len())])
            }
            _ => None,
        }
    }

    fn id(&mut self) -> WriteId {
        WriteId::new(IDS[self.rng.random_range(0..IDS.len())])
    }

    fn payload(&mut self) -> Value {
        Value::from(self.rng.random_range(0..100))
    }

    fn blobs(&mut self) -> Vec<BlobRef> {
        (0..self.rng.random_range(0..3))
            .map(|_| {
                let digit = ["0", "a", "f"][self.rng.random_range(0..3)];
                BlobRef::new("capture", digit.repeat(64)).unwrap()
            })
            .collect()
    }

    fn author(&mut self) -> clerkenwell_events::Principal {
        author(["writer", "reviewer"][self.rng.random_range(0..2)])
    }

    fn commit(&mut self) -> LogCommit {
        let records = (0..self.rng.random_range(0..3))
            .map(|_| NewRecord {
                author: self.author(),
                blobs: self.blobs(),
                payload: self.payload(),
            })
            .collect();
        LogCommit {
            id: self.id(),
            head: self
                .rng
                .random_bool(0.5)
                .then(|| json!({ "head": self.rng.random_range(0..100) })),
            records,
        }
    }

    /// An image whose positions mostly increase, with gaps.
    fn image(&mut self) -> LogImage {
        let mut position = self.rng.random_range(0..3);
        let mut records: Vec<StoredRecord> = (0..self.rng.random_range(0..4))
            .map(|_| {
                let record = StoredRecord {
                    position: Position::new(position),
                    author: self.author(),
                    recorded: self.fixture.now(),
                    blobs: self.blobs(),
                    payload: self.payload(),
                };
                position += self.rng.random_range(1..4);
                record
            })
            .collect();
        if records.len() > 1 && self.rng.random_bool(0.1) {
            records.swap(0, 1);
        }
        LogImage {
            id: self.id(),
            format: self.rng.random_range(1..4),
            head: json!({ "image": self.rng.random_range(0..100) }),
            records,
        }
    }

    fn create(&mut self, document: &DocumentId) {
        let lease = self.lease(document);
        let image = self.image();
        self.write(Write::Create { lease, image });
    }

    /// Makes a write over a revision of `document`, or creates it when no revision is known.
    fn revise(
        &mut self,
        document: &DocumentId,
        write: impl FnOnce(&mut Self, Lease, ModelRevision) -> Write,
    ) {
        let Some(base) = self.revision(document) else {
            return self.create(document);
        };
        let lease = self.lease(document);
        let write = write(self, lease, base);
        self.write(write);
    }

    /// Makes a write of another kind that repeats the id and base of `last`.
    fn reuse(&mut self, last: Write) {
        self.note("reuse");
        let lease = last.lease().clone();
        let (base, id) = match &last {
            Write::Create { image, .. } => (None, image.id.clone()),
            Write::Commit { base, commit, .. } | Write::Rewind { base, commit, .. } => {
                (Some(*base), commit.id.clone())
            }
            Write::Replace { base, image, .. } => (Some(*base), image.id.clone()),
            Write::Remove { base, id, .. } => (Some(*base), id.clone()),
        };
        let Some(base) = base else {
            // Every other write names a base: one repeats a creation's id over a revision.
            let Some(base) = self.revision(lease.document()) else {
                return;
            };
            let mut commit = self.commit();
            commit.id = id;
            return self.write(Write::Commit {
                lease,
                base,
                commit,
            });
        };
        let write = match (&last, self.rng.random_range(0..3)) {
            (Write::Remove { .. }, 0) | (Write::Replace { .. }, 1) => {
                let mut commit = self.commit();
                commit.id = id;
                Write::Commit {
                    lease,
                    base,
                    commit,
                }
            }
            (Write::Remove { .. }, _) => {
                let mut image = self.image();
                image.id = id;
                Write::Replace { lease, base, image }
            }
            _ => Write::Remove { lease, base, id },
        };
        self.write(write);
    }

    fn adapter(&self, revision: ModelRevision) -> Revision {
        self.revisions.to_adapter[&revision].clone()
    }

    fn write(&mut self, write: Write) {
        self.note(format!("{write:?}"));
        let document = write.lease().document().clone();
        let stands = self.standing.get(&document).map(Lease::epoch) == Some(write.lease().epoch());
        let now = self.fixture.now();
        let store = &self.fixture.store;
        match &write {
            Write::Create { lease, image } => {
                let got = store.create(lease, image);
                let expected = stands.then(|| self.model.create(&document, lease.epoch(), image));
                self.judge_revision(got, expected);
            }
            Write::Commit {
                lease,
                base,
                commit,
            } => {
                let got = store.commit(lease, &self.adapter(*base), commit);
                let expected = stands.then(|| {
                    self.model
                        .commit(&document, lease.epoch(), *base, commit, now)
                });
                self.judge_revision(got, expected);
            }
            Write::Rewind {
                lease,
                base,
                from,
                commit,
            } => {
                let got = store.rewind(lease, &self.adapter(*base), *from, commit);
                let expected = stands.then(|| {
                    self.model
                        .rewind(&document, lease.epoch(), *base, *from, commit, now)
                });
                self.judge_revision(got, expected);
            }
            Write::Replace { lease, base, image } => {
                let got = store.replace(lease, &self.adapter(*base), image);
                let expected =
                    stands.then(|| self.model.replace(&document, lease.epoch(), *base, image));
                self.judge_revision(got, expected);
            }
            Write::Remove { lease, base, id } => {
                let got = store.remove(lease, &self.adapter(*base), id);
                let expected =
                    stands.then(|| self.model.remove(&document, lease.epoch(), *base, id));
                self.judge(got, expected, |_, (), ()| true);
            }
        }
        self.last = Some(write);
    }

    fn judge_revision(
        &mut self,
        got: KernelResult<Revision>,
        expected: Option<Outcome<ModelRevision>>,
    ) {
        self.judge(got, expected, |run, adapter, model| {
            if !run.known.contains(&model) {
                run.known.push(model);
            }
            run.revisions.pair(&adapter, model)
        });
    }

    /// Checks that a write or read ended as the model says: refused for its lease when
    /// `expected` is `None`, and otherwise as `expected`, with `agree` judging what it returned.
    fn judge<T: Debug, M: Debug>(
        &mut self,
        got: KernelResult<T>,
        expected: Option<Outcome<M>>,
        agree: impl FnOnce(&mut Self, T, M) -> bool,
    ) {
        let refused = |error: &KernelError, expected: Option<&Outcome<M>>| match expected {
            None => error.reason_code() == Some(LEASE_LOST),
            Some(Outcome::Stale) => error.reason_code() == Some(LOG_STALE),
            Some(Outcome::Invalid) => error.code == "invalid_request",
            Some(Outcome::Done(_)) => false,
        };
        match (expected, got) {
            (Some(Outcome::Done(model)), Ok(value)) => {
                let described = format!("{value:?} against the model's {model:?}");
                if !agree(self, value, model) {
                    self.fail(format!("disagreed: {described}"));
                }
            }
            (expected, Err(error)) if refused(&error, expected.as_ref()) => {}
            (expected, got) => self.fail(format!("expected {expected:?}, got {got:?}")),
        }
    }

    fn compare_head(&mut self, document: &DocumentId) {
        let got = self
            .fixture
            .store
            .head(document)
            .unwrap_or_else(|error| self.fail(format!("head {document}: {error}")));
        let expected = self.model.head(document);
        let agreed = match (&got, &expected) {
            (None, None) => true,
            (Some(got), Some(expected)) => {
                (got.format, &got.head, got.next.get())
                    == (expected.format, &expected.head, expected.next)
                    && self.revisions.same(&got.revision, expected.revision)
            }
            _ => false,
        };
        if !agreed {
            self.fail(format!(
                "head {document}: got {got:?}, expected {expected:?}"
            ));
        }
    }

    fn records(&mut self, document: &DocumentId) {
        let Some(revision) = self.revision(document) else {
            return;
        };
        let next = self.model.head(document).map_or(2, |head| head.next);
        let from = Position::new(self.rng.random_range(0..next + 2));
        let limit = self.rng.random_range(0..4);
        self.note(format!(
            "records {document} at {revision:?} from {from}, {limit}"
        ));
        let got = self
            .fixture
            .store
            .records(document, &self.adapter(revision), from, limit);
        let expected = self.model.records(document, revision, from, limit);
        self.judge(got, Some(expected), |_, got, expected| got == expected);
    }

    fn list(&mut self) {
        let entity = [ENTITY, "Other"][self.rng.random_range(0..2)];
        let after = [None, Some(""), Some("a"), Some("b"), Some("c")][self.rng.random_range(0..5)];
        let limit = self.rng.random_range(0..4);
        self.note(format!("list {entity} after {after:?}, {limit}"));
        let got = self.fixture.store.list(entity, after, limit);
        let expected = self.model.list(entity, after, limit);
        self.judge(got, Some(expected), |run, got, expected| {
            got.len() == expected.len()
                && got
                    .iter()
                    .zip(&expected)
                    .all(|(got, (document, revision))| {
                        &got.document == document
                            && got
                                .state
                                .as_ref()
                                .is_ok_and(|got| run.revisions.same(got, *revision))
                    })
        });
    }

    /// Compares the whole document, head and every record, with the model's.
    fn compare_document(&mut self, document: &DocumentId) {
        self.note(format!("compare {document}"));
        self.compare_head(document);
        if let Some(head) = self.model.head(document) {
            let got = self.fixture.store.records(
                document,
                &self.adapter(head.revision),
                Position::FIRST,
                usize::MAX,
            );
            let expected = self
                .model
                .records(document, head.revision, Position::FIRST, usize::MAX);
            self.judge(got, Some(expected), |_, got, expected| got == expected);
        }
    }
}
