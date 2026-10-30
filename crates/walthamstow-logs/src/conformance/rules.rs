//! Copyright (c) Scott A Dixon
//!
//! Each rule of the log store's contract, as a test an adapter runs through
//! [`log_store_conformance!`](crate::log_store_conformance).

use super::{
    author, commit, document, image, is_invalid, new_record, reason, stored, Breaker, Fixture,
    Harness, ENTITY,
};
use crate::fault::{FaultKind, LogFault};
use crate::reason_codes::LOG_STALE;
use crate::types::{BlobRef, LogCommit, LogImage, Position, Revision, WriteId};
use serde_json::json;
use std::collections::HashSet;
use std::time::Duration;
use walthamstow_kernel::clock::Moment;
use walthamstow_kernel::leases::{Lease, LeaseHolder, Leases, MemoryLeases};
use walthamstow_kernel::reason_codes::LEASE_LOST;
use walthamstow_kernel::{DocumentId, KernelResult};

/// Runs every write but `create` under `lease` over `base`, and returns how each ended.
fn every_write(
    fixture: &Fixture,
    lease: &Lease,
    base: &Revision,
) -> Vec<(&'static str, KernelResult<()>)> {
    let store = &fixture.store;
    vec![
        (
            "commit",
            store.commit(lease, base, &commit("other", &[1])).map(drop),
        ),
        (
            "rewind",
            store
                .rewind(lease, base, Position::FIRST, &commit("other", &[]))
                .map(drop),
        ),
        (
            "replace",
            store
                .replace(lease, base, &image("other", vec![]))
                .map(drop),
        ),
        ("remove", store.remove(lease, base, &WriteId::new("other"))),
    ]
}

fn assert_every_write_refused(
    fixture: &Fixture,
    lease: &Lease,
    base: &Revision,
    expected: &str,
    situation: &str,
) {
    for (write, outcome) in every_write(fixture, lease, base) {
        assert_eq!(
            reason(&outcome),
            Some(expected),
            "{write} {situation}: {outcome:?}"
        );
    }
}

/// A document holding one record, at position 0.
fn with_one_record(fixture: &Fixture, resource: &str) -> (DocumentId, Revision) {
    let document = document(resource);
    let revision = fixture.create(&document, &[stored(0, 0, fixture.now())]);
    (document, revision)
}

fn positions(fixture: &Fixture, document: &DocumentId, revision: &Revision) -> Vec<u64> {
    fixture
        .all(document, revision)
        .iter()
        .map(|record| record.position.get())
        .collect()
}

fn kinds() -> [FaultKind; 3] {
    [
        FaultKind::Missing,
        FaultKind::Invalid,
        FaultKind::Inaccessible,
    ]
}

/// A write under a lease that was released, has lapsed or was granted by another set of leases
/// is refused with `lease_lost`, and changes nothing.
pub fn a_write_under_a_lost_lease_is_refused_and_changes_nothing(harness: &dyn Harness) {
    let fixture = Fixture::new(harness);
    let (document, revision) = with_one_record(&fixture, "a");
    let before = fixture.snapshot(&document);

    let released = fixture.lease(&document);
    fixture.leases.release(&released).unwrap();
    assert_every_write_refused(&fixture, &released, &revision, LEASE_LOST, "released");

    let lapsed = fixture.lease(&document);
    fixture.lapse();
    assert_every_write_refused(&fixture, &lapsed, &revision, LEASE_LOST, "lapsed");

    let elsewhere = MemoryLeases::new(Duration::from_secs(30), || Moment::ORIGIN);
    let foreign = elsewhere
        .acquire(&document, &LeaseHolder::new("conformance"))
        .unwrap()
        .unwrap();
    fixture.lease(&document);
    assert_every_write_refused(&fixture, &foreign, &revision, LEASE_LOST, "foreign");
    assert_eq!(fixture.snapshot(&document), before);

    let absent = super::document("absent");
    let lost = fixture.lease(&absent);
    fixture.leases.release(&lost).unwrap();
    let created = fixture.store.create(&lost, &image("create", vec![]));
    assert_eq!(reason(&created), Some(LEASE_LOST), "{created:?}");
    assert_eq!(fixture.store.head(&absent).unwrap(), None);
}

/// A write changes the document its lease names, and no other.
pub fn a_write_changes_only_the_document_its_lease_names(harness: &dyn Harness) {
    let fixture = Fixture::new(harness);
    let (first, revision) = with_one_record(&fixture, "a");
    let (second, other) = with_one_record(&fixture, "b");
    let untouched = fixture.snapshot(&second);
    fixture.append(&first, &revision, &[1]);
    assert_eq!(fixture.snapshot(&second), untouched);

    let crossed = fixture
        .store
        .commit(&fixture.lease(&first), &other, &commit("crossed", &[2]));
    assert_eq!(reason(&crossed), Some(LOG_STALE), "{crossed:?}");
    assert_eq!(fixture.snapshot(&second), untouched);
}

/// A write over a revision the document is not at is refused with `log_stale`, and changes
/// nothing.
pub fn a_write_over_another_revision_is_stale_and_changes_nothing(harness: &dyn Harness) {
    let fixture = Fixture::new(harness);
    let (document, first) = with_one_record(&fixture, "a");
    fixture.append(&document, &first, &[1]);
    let (_, elsewhere) = with_one_record(&fixture, "b");
    let before = fixture.snapshot(&document);
    let lease = fixture.lease(&document);
    assert_every_write_refused(&fixture, &lease, &first, LOG_STALE, "over a spent revision");
    assert_every_write_refused(
        &fixture,
        &lease,
        &elsewhere,
        LOG_STALE,
        "over another document's revision",
    );
    assert_eq!(fixture.snapshot(&document), before);
}

/// `create` is refused with `log_stale` while the document exists, and changes nothing.
pub fn create_is_stale_while_the_document_exists(harness: &dyn Harness) {
    let fixture = Fixture::new(harness);
    let (document, revision) = with_one_record(&fixture, "a");
    fixture.append(&document, &revision, &[1]);
    let before = fixture.snapshot(&document);
    let again = fixture
        .store
        .create(&fixture.lease(&document), &image("again", vec![]));
    assert_eq!(reason(&again), Some(LOG_STALE), "{again:?}");
    assert_eq!(fixture.snapshot(&document), before);
}

/// A removed document is at no revision: `head` returns `None`, `list` leaves it out, and every
/// write and every `records` read over it is refused with `log_stale`. So is a read of a
/// document that never existed.
pub fn a_removed_document_has_no_head_and_is_stale_to_writes_and_reads(harness: &dyn Harness) {
    let fixture = Fixture::new(harness);
    let (document, revision) = with_one_record(&fixture, "a");
    let lease = fixture.lease(&document);
    fixture
        .store
        .remove(&lease, &revision, &WriteId::new("remove"))
        .unwrap();
    assert_eq!(fixture.store.head(&document).unwrap(), None);
    assert!(fixture.store.list(ENTITY, None, 10).unwrap().is_empty());
    assert_every_write_refused(&fixture, &lease, &revision, LOG_STALE, "removed");
    let read = fixture
        .store
        .records(&document, &revision, Position::FIRST, 10);
    assert_eq!(reason(&read), Some(LOG_STALE), "{read:?}");
    let never = fixture
        .store
        .records(&super::document("never"), &revision, Position::FIRST, 10);
    assert_eq!(reason(&never), Some(LOG_STALE), "{never:?}");
}

/// The store never gives a revision twice, including across a document's removal and
/// re-creation.
pub fn a_revision_is_never_given_twice_across_removal_and_creation(harness: &dyn Harness) {
    let fixture = Fixture::new(harness);
    let document = document("a");
    let mut given = HashSet::new();
    for round in 0..3 {
        let lease = fixture.lease(&document);
        let mut revision = fixture.create(&document, &[]);
        assert!(given.insert(revision.clone()), "round {round}: {revision}");
        for payload in 0..3 {
            revision = fixture.append(&document, &revision, &[payload]);
            assert!(given.insert(revision.clone()), "round {round}: {revision}");
        }
        fixture
            .store
            .remove(&lease, &revision, &WriteId::new(format!("remove {round}")))
            .unwrap();
    }
}

/// A write that repeats the id and base of the last write to its document is given what that
/// write was given, and writes nothing again.
pub fn a_retried_write_is_given_what_it_was_given_and_writes_nothing_twice(harness: &dyn Harness) {
    let fixture = Fixture::new(harness);
    let document = document("a");
    let lease = fixture.lease(&document);
    let store = &fixture.store;

    let created_image = image("create", vec![stored(0, 0, fixture.now())]);
    let created = store.create(&lease, &created_image).unwrap();
    let before = fixture.snapshot(&document);
    assert_eq!(store.create(&lease, &created_image).unwrap(), created);
    assert_eq!(fixture.snapshot(&document), before);

    let appended = commit("append", &[1, 2]);
    let committed = store.commit(&lease, &created, &appended).unwrap();
    let before = fixture.snapshot(&document);
    assert_eq!(
        store.commit(&lease, &created, &appended).unwrap(),
        committed
    );
    assert_eq!(fixture.snapshot(&document), before);

    let rewound_commit = commit("rewind", &[3]);
    let rewound = store
        .rewind(&lease, &committed, Position::new(1), &rewound_commit)
        .unwrap();
    let before = fixture.snapshot(&document);
    assert_eq!(
        store
            .rewind(&lease, &committed, Position::new(1), &rewound_commit)
            .unwrap(),
        rewound
    );
    assert_eq!(fixture.snapshot(&document), before);

    let replacement = image("replace", vec![stored(9, 9, fixture.now())]);
    let replaced = store.replace(&lease, &rewound, &replacement).unwrap();
    let before = fixture.snapshot(&document);
    assert_eq!(
        store.replace(&lease, &rewound, &replacement).unwrap(),
        replaced
    );
    assert_eq!(fixture.snapshot(&document), before);

    let removal = WriteId::new("remove");
    store.remove(&lease, &replaced, &removal).unwrap();
    store.remove(&lease, &replaced, &removal).unwrap();
    assert_eq!(store.head(&document).unwrap(), None);
}

/// Only the last write to a document is retried: a write that repeats an earlier write is
/// refused with `log_stale`.
pub fn only_the_last_write_is_retried(harness: &dyn Harness) {
    let fixture = Fixture::new(harness);
    let document = document("a");
    let lease = fixture.lease(&document);
    let created_image = image("create", vec![]);
    let created = fixture.store.create(&lease, &created_image).unwrap();
    let first = commit("first", &[1]);
    let after_first = fixture.store.commit(&lease, &created, &first).unwrap();
    fixture
        .store
        .commit(&lease, &after_first, &commit("second", &[2]))
        .unwrap();
    let before = fixture.snapshot(&document);
    let repeated = fixture.store.commit(&lease, &created, &first);
    assert_eq!(reason(&repeated), Some(LOG_STALE), "{repeated:?}");
    let recreated = fixture.store.create(&lease, &created_image);
    assert_eq!(reason(&recreated), Some(LOG_STALE), "{recreated:?}");
    assert_eq!(fixture.snapshot(&document), before);
}

/// A retry is given what the last write gave: a revision to a write that gives one, and nothing
/// more to a removal. A removal that repeats the id and base of another write, or a write that
/// repeats those of a removal, is refused with `log_stale`.
pub fn a_removal_and_another_write_never_retry_each_other(harness: &dyn Harness) {
    let fixture = Fixture::new(harness);
    let (document, revision) = with_one_record(&fixture, "a");
    let lease = fixture.lease(&document);
    let id = WriteId::new("shared");
    fixture
        .store
        .commit(&lease, &revision, &commit("shared", &[1]))
        .unwrap();
    let before = fixture.snapshot(&document);
    let removed = fixture.store.remove(&lease, &revision, &id);
    assert_eq!(reason(&removed), Some(LOG_STALE), "{removed:?}");
    assert_eq!(fixture.snapshot(&document), before);

    let current = before.unwrap().0.revision;
    fixture.store.remove(&lease, &current, &id).unwrap();
    for (write, outcome) in [
        (
            "commit",
            fixture
                .store
                .commit(&lease, &current, &commit("shared", &[2]))
                .map(drop),
        ),
        (
            "replace",
            fixture
                .store
                .replace(&lease, &current, &image("shared", vec![]))
                .map(drop),
        ),
    ] {
        assert_eq!(reason(&outcome), Some(LOG_STALE), "{write}: {outcome:?}");
    }
    assert_eq!(fixture.store.head(&document).unwrap(), None);
}

/// A write with an id of its own over the base the last write spent is refused with `log_stale`.
pub fn a_write_with_a_new_id_over_a_spent_base_is_stale(harness: &dyn Harness) {
    let fixture = Fixture::new(harness);
    let (document, revision) = with_one_record(&fixture, "a");
    let lease = fixture.lease(&document);
    fixture
        .store
        .commit(&lease, &revision, &commit("first", &[1]))
        .unwrap();
    let before = fixture.snapshot(&document);
    let other = fixture
        .store
        .commit(&lease, &revision, &commit("second", &[1]));
    assert_eq!(reason(&other), Some(LOG_STALE), "{other:?}");
    assert_eq!(fixture.snapshot(&document), before);
}

/// A write is checked for its lease, then whether it retries the last write, then its base, then
/// what it carries.
pub fn a_write_is_checked_for_its_lease_then_a_retry_then_its_base_then_what_it_carries(
    harness: &dyn Harness,
) {
    let fixture = Fixture::new(harness);
    let (document, revision) = with_one_record(&fixture, "a");
    let lease = fixture.lease(&document);
    let appended = commit("append", &[1]);
    let current = fixture.store.commit(&lease, &revision, &appended).unwrap();

    // A retry under a lost lease is refused for its lease.
    fixture.leases.release(&lease).unwrap();
    let retried = fixture.store.commit(&lease, &revision, &appended);
    assert_eq!(reason(&retried), Some(LEASE_LOST), "{retried:?}");
    // A write under a lost lease over a stale base is refused for its lease.
    assert_every_write_refused(&fixture, &lease, &revision, LEASE_LOST, "lost and stale");

    // A retry over a base the document has left is given what it was given.
    let lease = fixture.lease(&document);
    assert_eq!(
        fixture.store.commit(&lease, &revision, &appended).unwrap(),
        current
    );

    // A stale write that carries what is invalid is refused as stale.
    let empty = LogCommit {
        id: WriteId::new("empty"),
        head: None,
        records: Vec::new(),
    };
    let checks = [
        fixture.store.commit(&lease, &revision, &empty),
        fixture
            .store
            .rewind(&lease, &revision, Position::new(99), &commit("far", &[])),
        fixture.store.replace(
            &lease,
            &revision,
            &image(
                "disordered",
                vec![stored(2, 0, fixture.now()), stored(1, 0, fixture.now())],
            ),
        ),
        fixture.store.create(
            &lease,
            &image(
                "disordered",
                vec![stored(2, 0, fixture.now()), stored(1, 0, fixture.now())],
            ),
        ),
    ];
    for check in checks {
        assert_eq!(reason(&check), Some(LOG_STALE), "{check:?}");
    }
}

/// The store gives each record its position, one past the last given, and records it on the
/// store's clock. It returns the author, blobs, payload, head and format as the writer gave them.
pub fn the_store_gives_positions_and_records_on_its_clock(harness: &dyn Harness) {
    let fixture = Fixture::new(harness);
    let document = document("a");
    let lease = fixture.lease(&document);
    let head = json!({ "anything": [1, "two", null] });
    let created = fixture
        .store
        .create(
            &lease,
            &LogImage {
                id: WriteId::new("create"),
                format: u32::MAX,
                head: head.clone(),
                records: Vec::new(),
            },
        )
        .unwrap();
    let stored_head = fixture.store.head(&document).unwrap().unwrap();
    assert_eq!(
        (stored_head.format, &stored_head.head, stored_head.next),
        (u32::MAX, &head, Position::FIRST)
    );
    assert_eq!(stored_head.revision, created);

    let first_time = fixture.advance_wall(5);
    let mut records = vec![new_record(10), new_record(11)];
    records[1].author = author("reviewer");
    let first = fixture
        .store
        .commit(
            &lease,
            &created,
            &LogCommit {
                id: WriteId::new("first"),
                head: None,
                records: records.clone(),
            },
        )
        .unwrap();
    let second_time = fixture.advance_wall(7);
    let second = fixture.append(&document, &first, &[12]);

    let read = fixture.all(&document, &second);
    let summary: Vec<_> = read
        .iter()
        .map(|record| {
            (
                record.position.get(),
                record.author.id.as_str(),
                record.recorded,
                record.payload.clone(),
            )
        })
        .collect();
    assert_eq!(
        summary,
        [
            (0, "writer", first_time, json!(10)),
            (1, "reviewer", first_time, json!(11)),
            (2, "writer", second_time, json!(12)),
        ]
    );
    assert_eq!(read[1].author, records[1].author);
    assert_eq!(
        fixture.store.head(&document).unwrap().unwrap().next,
        Position::new(3)
    );
}

/// A commit replaces the head when it carries one and keeps it otherwise. The format is kept.
pub fn a_commit_replaces_the_head_only_when_it_carries_one(harness: &dyn Harness) {
    let fixture = Fixture::new(harness);
    let (document, revision) = with_one_record(&fixture, "a");
    let original = fixture.store.head(&document).unwrap().unwrap();
    let kept = fixture.append(&document, &revision, &[1]);
    let head = fixture.store.head(&document).unwrap().unwrap();
    assert_eq!((head.format, &head.head), (original.format, &original.head));

    let replaced = fixture
        .store
        .commit(
            &fixture.lease(&document),
            &kept,
            &LogCommit {
                id: WriteId::new("head"),
                head: Some(json!("replaced")),
                records: Vec::new(),
            },
        )
        .unwrap();
    let head = fixture.store.head(&document).unwrap().unwrap();
    assert_eq!(
        (head.format, head.head, head.revision),
        (original.format, json!("replaced"), replaced.clone())
    );
    assert_eq!(positions(&fixture, &document, &replaced), [0, 1]);
}

/// A commit with neither records nor a head is refused as an invalid request, and changes
/// nothing.
pub fn a_commit_carrying_nothing_is_invalid(harness: &dyn Harness) {
    let fixture = Fixture::new(harness);
    let (document, revision) = with_one_record(&fixture, "a");
    let before = fixture.snapshot(&document);
    let empty = fixture.store.commit(
        &fixture.lease(&document),
        &revision,
        &LogCommit {
            id: WriteId::new("empty"),
            head: None,
            records: Vec::new(),
        },
    );
    assert!(is_invalid(&empty), "{empty:?}");
    assert_eq!(fixture.snapshot(&document), before);
}

/// An image's records keep their positions, and the document's next position is one past the
/// highest of them, or the first position when there are none.
pub fn an_image_sets_the_next_position_one_past_its_highest(harness: &dyn Harness) {
    let fixture = Fixture::new(harness);
    let earlier = fixture.now() - chrono::Duration::days(1);
    let records = vec![stored(3, 3, earlier), stored(7, 7, earlier)];
    let gapped = document("gapped");
    let revision = fixture.create(&gapped, &records);
    assert_eq!(fixture.all(&gapped, &revision), records);
    assert_eq!(
        fixture.store.head(&gapped).unwrap().unwrap().next,
        Position::new(8)
    );
    let appended = fixture.append(&gapped, &revision, &[8]);
    assert_eq!(positions(&fixture, &gapped, &appended), [3, 7, 8]);

    let empty = document("empty");
    fixture.create(&empty, &[]);
    assert_eq!(
        fixture.store.head(&empty).unwrap().unwrap().next,
        Position::FIRST
    );
}

/// An image whose positions do not increase along the log is refused as an invalid request, and
/// changes nothing.
pub fn an_image_whose_positions_do_not_increase_is_invalid(harness: &dyn Harness) {
    let fixture = Fixture::new(harness);
    let now = fixture.now();
    let disordered = [
        vec![stored(2, 0, now), stored(2, 1, now)],
        vec![stored(5, 0, now), stored(3, 1, now)],
    ];
    let absent = document("absent");
    for records in &disordered {
        let created = fixture
            .store
            .create(&fixture.lease(&absent), &image("create", records.clone()));
        assert!(is_invalid(&created), "{created:?}");
        assert_eq!(fixture.store.head(&absent).unwrap(), None);
    }

    let (document, revision) = with_one_record(&fixture, "a");
    let before = fixture.snapshot(&document);
    for records in disordered {
        let replaced = fixture.store.replace(
            &fixture.lease(&document),
            &revision,
            &image("replace", records),
        );
        assert!(is_invalid(&replaced), "{replaced:?}");
    }
    assert_eq!(fixture.snapshot(&document), before);
}

/// A rewind removes the records from its position onwards, then applies its commit. A position
/// it removes is never given again.
pub fn a_rewind_removes_records_from_its_position_and_never_gives_a_position_twice(
    harness: &dyn Harness,
) {
    let fixture = Fixture::new(harness);
    let (document, revision) = with_one_record(&fixture, "a");
    let revision = fixture.append(&document, &revision, &[1, 2, 3]);
    let lease = fixture.lease(&document);

    let revision = fixture
        .store
        .rewind(&lease, &revision, Position::new(2), &commit("rewind", &[9]))
        .unwrap();
    assert_eq!(positions(&fixture, &document, &revision), [0, 1, 4]);
    assert_eq!(fixture.all(&document, &revision)[2].payload, json!(9));

    let revision = fixture
        .store
        .rewind(
            &lease,
            &revision,
            Position::new(1),
            &commit("truncate", &[]),
        )
        .unwrap();
    assert_eq!(positions(&fixture, &document, &revision), [0]);
    assert_eq!(
        fixture.store.head(&document).unwrap().unwrap().next,
        Position::new(5)
    );

    let revision = fixture.append(&document, &revision, &[5]);
    assert_eq!(positions(&fixture, &document, &revision), [0, 5]);
}

/// A rewind from the next position or beyond is refused as an invalid request, and changes
/// nothing.
pub fn a_rewind_from_the_next_position_or_beyond_is_invalid(harness: &dyn Harness) {
    let fixture = Fixture::new(harness);
    let (document, revision) = with_one_record(&fixture, "a");
    let revision = fixture.append(&document, &revision, &[1]);
    let before = fixture.snapshot(&document);
    for from in [2, 3, u64::MAX] {
        let rewound = fixture.store.rewind(
            &fixture.lease(&document),
            &revision,
            Position::new(from),
            &commit("rewind", &[9]),
        );
        assert!(is_invalid(&rewound), "from {from}: {rewound:?}");
    }
    assert_eq!(fixture.snapshot(&document), before);
}

/// A replace sets the format, head and records. It keeps the next position unless the image's
/// highest position is at or beyond it, when the next position is one past that.
pub fn a_replace_keeps_the_next_position_unless_the_image_reaches_it(harness: &dyn Harness) {
    let fixture = Fixture::new(harness);
    let (document, revision) = with_one_record(&fixture, "a");
    let mut revision = fixture.append(&document, &revision, &[1, 2, 3, 4]);
    let lease = fixture.lease(&document);
    let now = fixture.now();
    for (records, next) in [
        (vec![stored(1, 1, now)], 5),
        (vec![stored(5, 5, now)], 6),
        (vec![stored(2, 2, now), stored(9, 9, now)], 10),
        (Vec::new(), 10),
    ] {
        let replacement = LogImage {
            id: WriteId::new(format!("replace to {next}")),
            format: 2,
            head: json!({ "next": next }),
            records: records.clone(),
        };
        revision = fixture
            .store
            .replace(&lease, &revision, &replacement)
            .unwrap();
        let head = fixture.store.head(&document).unwrap().unwrap();
        assert_eq!(
            (head.format, head.head, head.next),
            (2, json!({ "next": next }), Position::new(next))
        );
        assert_eq!(fixture.all(&document, &revision), records);
    }
}

/// `records` is refused with `log_stale` once the document has moved past the revision it
/// names.
pub fn records_are_stale_once_the_document_moves_on(harness: &dyn Harness) {
    let fixture = Fixture::new(harness);
    let (document, revision) = with_one_record(&fixture, "a");
    assert_eq!(positions(&fixture, &document, &revision), [0]);
    fixture.append(&document, &revision, &[1]);
    let read = fixture
        .store
        .records(&document, &revision, Position::FIRST, 10);
    assert_eq!(reason(&read), Some(LOG_STALE), "{read:?}");
}

/// `records` returns up to its limit of records, from the first at or after its position.
pub fn records_page_from_a_position(harness: &dyn Harness) {
    let fixture = Fixture::new(harness);
    let (document, revision) = with_one_record(&fixture, "a");
    let revision = fixture.append(&document, &revision, &[1, 2, 3, 4, 5]);
    let revision = fixture
        .store
        .rewind(
            &fixture.lease(&document),
            &revision,
            Position::new(3),
            &commit("gap", &[6, 7]),
        )
        .unwrap();
    // The log holds positions 0, 1, 2, 6 and 7.
    let page = |from: u64, limit: usize| -> Vec<u64> {
        fixture
            .store
            .records(&document, &revision, Position::new(from), limit)
            .unwrap()
            .iter()
            .map(|record| record.position.get())
            .collect()
    };
    assert_eq!(page(0, 2), [0, 1]);
    assert_eq!(page(2, 2), [2, 6]);
    assert_eq!(page(3, 10), [6, 7]);
    assert_eq!(page(7, 1), [7]);
    assert!(page(8, 10).is_empty());
}

/// `list` returns up to its limit of the entity's documents, in resource order, after the
/// resource it names.
pub fn list_pages_an_entitys_documents_in_resource_order(harness: &dyn Harness) {
    let fixture = Fixture::new(harness);
    let mut revisions = Vec::new();
    for resource in ["c", "a", "b", "b/1"] {
        let (document, revision) = with_one_record(&fixture, resource);
        revisions.push((document, revision));
    }
    let other = DocumentId::new("Other", "a");
    let other_revision = fixture.create(&other, &[]);
    let (removed, removed_revision) = with_one_record(&fixture, "ab");
    fixture
        .store
        .remove(
            &fixture.lease(&removed),
            &removed_revision,
            &WriteId::new("remove"),
        )
        .unwrap();
    revisions.sort_by(|left, right| left.0.cmp(&right.0));

    let page = |entity: &str, after: Option<&str>, limit: usize| {
        fixture
            .store
            .list(entity, after, limit)
            .unwrap()
            .into_iter()
            .map(|listed| (listed.document, listed.state.unwrap()))
            .collect::<Vec<_>>()
    };
    assert_eq!(page(ENTITY, None, 10), revisions);
    assert_eq!(page(ENTITY, None, 2), revisions[..2]);
    assert_eq!(page(ENTITY, Some("a"), 2), revisions[1..3]);
    assert_eq!(page(ENTITY, Some("ab"), 10), revisions[1..]);
    assert_eq!(page(ENTITY, Some("c"), 10), []);
    assert_eq!(page("Other", None, 10), [(other, other_revision)]);
    assert_eq!(page("Lo", None, 10), []);
}

/// A page of no records or no documents is refused as an invalid request.
pub fn an_empty_page_is_invalid(harness: &dyn Harness) {
    let fixture = Fixture::new(harness);
    let (document, revision) = with_one_record(&fixture, "a");
    let records = fixture
        .store
        .records(&document, &revision, Position::FIRST, 0);
    assert!(is_invalid(&records), "{records:?}");
    let listed = fixture.store.list(ENTITY, None, 0);
    assert!(is_invalid(&listed), "{listed:?}");
}

/// A read changes nothing: the document stays at the revision a write names.
pub fn reads_change_nothing(harness: &dyn Harness) {
    let fixture = Fixture::new(harness);
    let (document, revision) = with_one_record(&fixture, "a");
    let revision = fixture.append(&document, &revision, &[1, 2]);
    let before = fixture.snapshot(&document);
    for _ in 0..2 {
        fixture.store.head(&document).unwrap();
        fixture
            .store
            .records(&document, &revision, Position::new(1), 1)
            .unwrap();
        fixture.store.list(ENTITY, None, 10).unwrap();
        fixture.store.head(&super::document("absent")).unwrap();
    }
    assert_eq!(fixture.snapshot(&document), before);
    fixture.append(&document, &revision, &[3]);
}

/// The store returns a record's blob references as they were written, and never consults a
/// blob store.
pub fn blob_references_are_returned_as_stored(harness: &dyn Harness) {
    let fixture = Fixture::new(harness);
    let blobs = vec![
        BlobRef::new("ff".repeat(32)).unwrap(),
        BlobRef::new("00".repeat(32)).unwrap(),
        BlobRef::new("ff".repeat(32)).unwrap(),
    ];
    let document = document("a");
    let mut imaged = stored(0, 0, fixture.now());
    imaged.blobs = blobs.clone();
    let revision = fixture.create(&document, &[imaged]);
    let mut record = new_record(1);
    record.blobs = blobs.iter().rev().cloned().collect();
    let revision = fixture
        .store
        .commit(
            &fixture.lease(&document),
            &revision,
            &LogCommit {
                id: WriteId::new("blobs"),
                head: None,
                records: vec![record.clone()],
            },
        )
        .unwrap();
    let read = fixture.all(&document, &revision);
    assert_eq!(read[0].blobs, blobs);
    assert_eq!(read[1].blobs, record.blobs);
}

/// Creates a document for each fault kind and breaks it, beside one left whole. Returns each
/// broken document with its kind and revision.
fn broken_documents(
    fixture: &Fixture,
    breaker: &dyn Breaker,
) -> Vec<(DocumentId, FaultKind, Revision)> {
    with_one_record(fixture, "whole");
    kinds()
        .into_iter()
        .enumerate()
        .map(|(index, kind)| {
            let (document, revision) = with_one_record(fixture, &format!("broken {index}"));
            breaker.break_document(&document, kind);
            (document, kind, revision)
        })
        .collect()
}

fn fault_kind<T: std::fmt::Debug>(result: &KernelResult<T>) -> Option<FaultKind> {
    result
        .as_ref()
        .err()
        .and_then(LogFault::of)
        .map(|fault| fault.kind())
}

/// A broken document is listed with its fault, in its place among the entity's documents.
pub fn a_broken_document_is_listed_with_its_fault(breaker: &dyn Breaker) {
    let fixture = Fixture::new(breaker);
    let broken = broken_documents(&fixture, breaker);
    let listed = fixture.store.list(ENTITY, None, 10).unwrap();
    let mut expected: Vec<_> = broken
        .iter()
        .map(|(document, kind, _)| (document.clone(), Some(*kind)))
        .collect();
    expected.push((document("whole"), None));
    expected.sort_by(|left, right| left.0.cmp(&right.0));
    let found: Vec<_> = listed
        .iter()
        .map(|listed| {
            (
                listed.document.clone(),
                listed.state.as_ref().err().map(LogFault::kind),
            )
        })
        .collect();
    assert_eq!(found, expected);
}

/// `head` and `records` refuse a broken document with its fault.
pub fn head_and_records_refuse_a_broken_document_with_its_fault(breaker: &dyn Breaker) {
    let fixture = Fixture::new(breaker);
    for (document, kind, revision) in broken_documents(&fixture, breaker) {
        let head = fixture.store.head(&document);
        assert_eq!(fault_kind(&head), Some(kind), "{head:?}");
        let records = fixture
            .store
            .records(&document, &revision, Position::FIRST, 10);
        assert_eq!(fault_kind(&records), Some(kind), "{records:?}");
    }
}

/// A document missing data names each part it is missing.
pub fn a_missing_document_names_each_absent_part(breaker: &dyn Breaker) {
    let fixture = Fixture::new(breaker);
    let (document, _) = with_one_record(&fixture, "a");
    breaker.break_document(&document, FaultKind::Missing);
    let refusal = fixture.store.head(&document).unwrap_err();
    match LogFault::of(&refusal) {
        Some(LogFault::Missing { parts }) => assert!(!parts.is_empty(), "{refusal:?}"),
        other => panic!("{other:?} from {refusal:?}"),
    }
}

/// A read never repairs a broken document: it stays broken however often it is read.
pub fn a_read_leaves_a_broken_document_broken(breaker: &dyn Breaker) {
    let fixture = Fixture::new(breaker);
    let broken = broken_documents(&fixture, breaker);
    let listing = || -> Vec<_> {
        fixture
            .store
            .list(ENTITY, None, 10)
            .unwrap()
            .into_iter()
            .map(|listed| (listed.document, listed.state.err()))
            .collect()
    };
    let first = listing();
    for _ in 0..2 {
        for (document, kind, revision) in &broken {
            assert_eq!(fault_kind(&fixture.store.head(document)), Some(*kind));
            assert_eq!(
                fault_kind(
                    &fixture
                        .store
                        .records(document, revision, Position::FIRST, 10)
                ),
                Some(*kind)
            );
        }
        assert_eq!(listing(), first);
    }
}
