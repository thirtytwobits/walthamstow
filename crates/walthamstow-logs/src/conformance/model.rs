//! Copyright (c) Scott A Dixon
//!
//! A reference model of the log store's contract, for the property test. It holds what the
//! contract says a store holds and answers each operation as the contract says. It knows nothing
//! of leases: the property test judges a lease itself, and asks the model only about a write
//! whose lease stands.

use crate::types::{LogCommit, LogImage, Position, StoredRecord, WriteId};
use chrono::{DateTime, Utc};
use serde_json::Value;
use std::collections::BTreeMap;
use walthamstow_kernel::DocumentId;

/// How the contract says an operation ends.
#[derive(Debug, Clone, PartialEq)]
pub(super) enum Outcome<T> {
    Done(T),
    Stale,
    Invalid,
}

/// A revision as the model gives it. The property test pairs each with the adapter's.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub(super) struct ModelRevision {
    incarnation: u64,
    sequence: u64,
}

#[derive(Debug, Clone, PartialEq)]
pub(super) struct ModelHead {
    pub(super) format: u32,
    pub(super) head: Value,
    pub(super) next: u64,
    pub(super) revision: ModelRevision,
}

#[derive(Debug, Clone)]
struct ModelDocument {
    format: u32,
    head: Value,
    records: Vec<StoredRecord>,
    next: u64,
    revision: ModelRevision,
}

/// The last write to a document: its id, its base, and what it gave, which is no revision for
/// a removal.
#[derive(Debug)]
struct LastWrite {
    id: WriteId,
    base: Option<ModelRevision>,
    given: Option<ModelRevision>,
}

#[derive(Debug, Default)]
pub(super) struct Model {
    documents: BTreeMap<DocumentId, ModelDocument>,
    last: BTreeMap<DocumentId, LastWrite>,
    incarnations: u64,
}

/// The next position an image leaves, or `None` when its positions do not increase.
fn image_next(image: &LogImage) -> Option<u64> {
    let mut next = 0;
    for record in &image.records {
        if record.position.get() < next {
            return None;
        }
        next = record.position.get() + 1;
    }
    Some(next)
}

fn append(document: &mut ModelDocument, commit: &LogCommit, now: DateTime<Utc>) {
    for record in &commit.records {
        document.records.push(StoredRecord {
            position: Position::new(document.next),
            author: record.author.clone(),
            recorded: now,
            blobs: record.blobs.clone(),
            payload: record.payload.clone(),
        });
        document.next += 1;
    }
    if let Some(head) = &commit.head {
        document.head = head.clone();
    }
}

impl Model {
    /// What the last write to `document` gave, when a write of `id` over `base` repeats it.
    fn retried(
        &self,
        document: &DocumentId,
        id: &WriteId,
        base: Option<ModelRevision>,
    ) -> Option<Option<ModelRevision>> {
        self.last
            .get(document)
            .filter(|last| &last.id == id && last.base == base)
            .map(|last| last.given)
    }

    pub(super) fn create(
        &mut self,
        document: &DocumentId,
        image: &LogImage,
    ) -> Outcome<ModelRevision> {
        if let Some(Some(given)) = self.retried(document, &image.id, None) {
            return Outcome::Done(given);
        }
        if self.documents.contains_key(document) {
            return Outcome::Stale;
        }
        let Some(next) = image_next(image) else {
            return Outcome::Invalid;
        };
        self.incarnations += 1;
        let revision = ModelRevision {
            incarnation: self.incarnations,
            sequence: 1,
        };
        self.documents.insert(
            document.clone(),
            ModelDocument {
                format: image.format,
                head: image.head.clone(),
                records: image.records.clone(),
                next,
                revision,
            },
        );
        self.last.insert(
            document.clone(),
            LastWrite {
                id: image.id.clone(),
                base: None,
                given: Some(revision),
            },
        );
        Outcome::Done(revision)
    }

    /// Applies `change` to `document` over `base`. `change` returns false, having changed a copy
    /// that is then dropped, when what the write carries is invalid.
    fn change(
        &mut self,
        document: &DocumentId,
        base: ModelRevision,
        id: &WriteId,
        change: impl FnOnce(&mut ModelDocument) -> bool,
    ) -> Outcome<ModelRevision> {
        if let Some(Some(given)) = self.retried(document, id, Some(base)) {
            return Outcome::Done(given);
        }
        let Some(current) = self
            .documents
            .get_mut(document)
            .filter(|current| current.revision == base)
        else {
            return Outcome::Stale;
        };
        let mut changed = current.clone();
        if !change(&mut changed) {
            return Outcome::Invalid;
        }
        changed.revision.sequence += 1;
        let revision = changed.revision;
        *current = changed;
        self.last.insert(
            document.clone(),
            LastWrite {
                id: id.clone(),
                base: Some(base),
                given: Some(revision),
            },
        );
        Outcome::Done(revision)
    }

    pub(super) fn commit(
        &mut self,
        document: &DocumentId,
        base: ModelRevision,
        commit: &LogCommit,
        now: DateTime<Utc>,
    ) -> Outcome<ModelRevision> {
        self.change(document, base, &commit.id, |stored| {
            if commit.records.is_empty() && commit.head.is_none() {
                return false;
            }
            append(stored, commit, now);
            true
        })
    }

    pub(super) fn rewind(
        &mut self,
        document: &DocumentId,
        base: ModelRevision,
        from: Position,
        commit: &LogCommit,
        now: DateTime<Utc>,
    ) -> Outcome<ModelRevision> {
        self.change(document, base, &commit.id, |stored| {
            if from.get() >= stored.next {
                return false;
            }
            stored.records.retain(|record| record.position < from);
            append(stored, commit, now);
            true
        })
    }

    pub(super) fn replace(
        &mut self,
        document: &DocumentId,
        base: ModelRevision,
        image: &LogImage,
    ) -> Outcome<ModelRevision> {
        self.change(document, base, &image.id, |stored| {
            let Some(next) = image_next(image) else {
                return false;
            };
            stored.format = image.format;
            stored.head = image.head.clone();
            stored.records = image.records.clone();
            stored.next = stored.next.max(next);
            true
        })
    }

    pub(super) fn remove(
        &mut self,
        document: &DocumentId,
        base: ModelRevision,
        id: &WriteId,
    ) -> Outcome<()> {
        if let Some(None) = self.retried(document, id, Some(base)) {
            return Outcome::Done(());
        }
        if self.documents.get(document).map(|stored| stored.revision) != Some(base) {
            return Outcome::Stale;
        }
        self.documents.remove(document);
        self.last.insert(
            document.clone(),
            LastWrite {
                id: id.clone(),
                base: Some(base),
                given: None,
            },
        );
        Outcome::Done(())
    }

    pub(super) fn head(&self, document: &DocumentId) -> Option<ModelHead> {
        self.documents.get(document).map(|stored| ModelHead {
            format: stored.format,
            head: stored.head.clone(),
            next: stored.next,
            revision: stored.revision,
        })
    }

    pub(super) fn records(
        &self,
        document: &DocumentId,
        revision: ModelRevision,
        from: Position,
        limit: usize,
    ) -> Outcome<Vec<StoredRecord>> {
        let Some(stored) = self
            .documents
            .get(document)
            .filter(|stored| stored.revision == revision)
        else {
            return Outcome::Stale;
        };
        if limit == 0 {
            return Outcome::Invalid;
        }
        Outcome::Done(
            stored
                .records
                .iter()
                .filter(|record| record.position >= from)
                .take(limit)
                .cloned()
                .collect(),
        )
    }

    pub(super) fn list(
        &self,
        entity: &str,
        after: Option<&str>,
        limit: usize,
    ) -> Outcome<Vec<(DocumentId, ModelRevision)>> {
        if limit == 0 {
            return Outcome::Invalid;
        }
        Outcome::Done(
            self.documents
                .iter()
                .filter(|(document, _)| document.entity() == entity)
                .filter(|(document, _)| after.is_none_or(|after| document.resource() > after))
                .take(limit)
                .map(|(document, stored)| (document.clone(), stored.revision))
                .collect(),
        )
    }
}
