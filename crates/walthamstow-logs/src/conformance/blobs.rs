//! Copyright (c) Scott A Dixon
//!
//! The blob section of the suite: each rule of the blob store's contract as a named test an
//! adapter runs through [`blob_store_conformance!`](crate::blob_store_conformance).
//!
//! An adapter's tests implement [`BlobHarness`] and invoke
//! `blob_store_conformance!(harness = ...)`. An adapter whose blobs can be broken from outside
//! also implements [`BlobBreaker`] and invokes `blob_store_conformance!(broken = ...)`.

use super::{is_invalid, locked, reason, TERM};
use crate::blobs::{BlobKinds, BlobStore};
use crate::fault::BlobFault;
use crate::types::{BlobKind, BlobRef};
use std::sync::{Arc, Mutex};
use walthamstow_kernel::clock::Moment;
use walthamstow_kernel::hashing::sha256_hex;
use walthamstow_kernel::leases::{Lease, LeaseHolder, Leases, MemoryLeases};
use walthamstow_kernel::reason_codes::LEASE_LOST;
use walthamstow_kernel::DocumentId;

/// Builds the blob store under test.
pub trait BlobHarness {
    /// An empty store of `kinds` whose writes `leases` fence.
    fn store(&self, leases: Arc<dyn Leases>, kinds: BlobKinds) -> Box<dyn BlobStore>;
}

/// Breaks blobs from outside the store under test.
pub trait BlobBreaker: BlobHarness {
    /// Replaces the bytes of `blob` in `document`, in the store this harness built last, with
    /// bytes that do not match its digest.
    fn corrupt(&self, document: &DocumentId, blob: &BlobRef);

    /// Deletes `blob` from `document`, in the store this harness built last.
    fn delete(&self, document: &DocumentId, blob: &BlobRef);
}

/// Runs the blob section of the suite as one test per rule.
///
/// `blob_store_conformance!(harness = expr)` runs every rule over the [`BlobHarness`] `expr`.
/// `blob_store_conformance!(broken = expr)` runs the broken-blob rules over the [`BlobBreaker`]
/// `expr`. Each test evaluates `expr` afresh.
#[macro_export]
macro_rules! blob_store_conformance {
    (harness = $harness:expr) => {
        $crate::blob_store_conformance!(@rules $harness;
            a_put_names_its_bytes_by_kind_and_digest,
            a_put_repeated_changes_nothing,
            the_same_bytes_under_two_kinds_are_two_blobs,
            a_put_or_removal_under_a_lost_lease_is_refused_and_changes_nothing,
            a_write_changes_only_the_leases_document,
            an_unregistered_kind_is_refused_on_every_operation,
            an_absent_blob_is_none_to_get_and_missing_to_check,
            list_pages_by_kind_then_digest,
            an_empty_page_is_invalid,
            a_removal_removes_the_blobs_it_names_and_nothing_else,
            reads_change_nothing,
        );
    };
    (broken = $harness:expr) => {
        $crate::blob_store_conformance!(@rules $harness;
            a_corrupt_blob_is_refused_by_get_and_reported_by_check,
            a_deleted_blob_is_reported_and_never_restored,
        );
    };
    (@rules $harness:expr; $($rule:ident),* $(,)?) => {
        $(
            #[test]
            fn $rule() {
                $crate::conformance::blobs::$rule(&$harness);
            }
        )*
    };
}

/// The kinds the suite registers: `capture`, whose files are `.png`, and `text`, `.txt`.
pub fn kinds() -> BlobKinds {
    BlobKinds::new([
        BlobKind::new("capture", "png").expect("a kind"),
        BlobKind::new("text", "txt").expect("a kind"),
    ])
    .expect("two kinds")
}

/// A store under test, the leases that fence its writes, and the clock the leases read, which
/// moves only when a test moves it.
struct Fixture {
    store: Box<dyn BlobStore>,
    leases: Arc<MemoryLeases>,
    moment: Arc<Mutex<Moment>>,
}

impl Fixture {
    fn new(harness: &dyn BlobHarness) -> Self {
        let moment = Arc::new(Mutex::new(Moment::ORIGIN));
        let leases = Arc::new(MemoryLeases::new(TERM, {
            let moment = moment.clone();
            move || *locked(&moment)
        }));
        Self {
            store: harness.store(leases.clone(), kinds()),
            leases,
            moment,
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

    fn put(&self, document: &DocumentId, kind: &str, bytes: &[u8]) -> BlobRef {
        self.store.put(&self.lease(document), kind, bytes).unwrap()
    }

    /// Every blob of `document`, read in one page.
    fn all(&self, document: &DocumentId) -> Vec<BlobRef> {
        self.store.list(document, None, usize::MAX).unwrap()
    }
}

fn document(resource: &str) -> DocumentId {
    DocumentId::new("Log", resource)
}

/// The reference the suite expects for `bytes` under `kind`.
fn named(kind: &str, bytes: &[u8]) -> BlobRef {
    BlobRef::new(kind, sha256_hex(bytes)).unwrap()
}

/// A put returns the blob's kind and the digest of its bytes, which `get` returns and `list`
/// names.
pub fn a_put_names_its_bytes_by_kind_and_digest(harness: &dyn BlobHarness) {
    let fixture = Fixture::new(harness);
    let document = document("a");
    let blob = fixture.put(&document, "capture", b"one");
    assert_eq!(blob, named("capture", b"one"));
    assert_eq!(
        fixture.store.get(&document, &blob).unwrap().as_deref(),
        Some(&b"one"[..])
    );
    assert_eq!(fixture.all(&document), [blob]);
}

/// Putting bytes the store already holds changes nothing.
pub fn a_put_repeated_changes_nothing(harness: &dyn BlobHarness) {
    let fixture = Fixture::new(harness);
    let document = document("a");
    let first = fixture.put(&document, "capture", b"one");
    let again = fixture.put(&document, "capture", b"one");
    assert_eq!(again, first);
    assert_eq!(fixture.all(&document), [first.clone()]);
    assert_eq!(
        fixture.store.get(&document, &first).unwrap().as_deref(),
        Some(&b"one"[..])
    );
}

/// A blob is named within its kind: the same bytes under two kinds are two blobs.
pub fn the_same_bytes_under_two_kinds_are_two_blobs(harness: &dyn BlobHarness) {
    let fixture = Fixture::new(harness);
    let document = document("a");
    let capture = fixture.put(&document, "capture", b"same");
    let text = fixture.put(&document, "text", b"same");
    assert_ne!(capture, text);
    assert_eq!(capture.sha256(), text.sha256());
    assert_eq!(fixture.all(&document), [capture, text]);
}

/// A put or a removal under a lease that was released or has lapsed is refused with
/// `lease_lost`, and changes nothing.
pub fn a_put_or_removal_under_a_lost_lease_is_refused_and_changes_nothing(
    harness: &dyn BlobHarness,
) {
    let fixture = Fixture::new(harness);
    let document = document("a");
    let released = fixture.lease(&document);
    let kept = fixture.store.put(&released, "capture", b"kept").unwrap();
    fixture.leases.release(&released).unwrap();
    let refused = fixture.store.put(&released, "capture", b"refused");
    assert_eq!(reason(&refused), Some(LEASE_LOST), "{refused:?}");
    let removed = fixture.store.remove(&released, &[kept.clone()]);
    assert_eq!(reason(&removed), Some(LEASE_LOST), "{removed:?}");

    let lapsed = fixture.lease(&document);
    fixture.lapse();
    let refused = fixture.store.put(&lapsed, "capture", b"refused");
    assert_eq!(reason(&refused), Some(LEASE_LOST), "{refused:?}");
    let removed = fixture.store.remove(&lapsed, &[kept.clone()]);
    assert_eq!(reason(&removed), Some(LEASE_LOST), "{removed:?}");

    assert_eq!(fixture.all(&document), [kept]);
}

/// A write reaches the lease's document alone.
pub fn a_write_changes_only_the_leases_document(harness: &dyn BlobHarness) {
    let fixture = Fixture::new(harness);
    let (a, b) = (document("a"), document("b"));
    let blob = fixture.put(&a, "capture", b"one");
    assert_eq!(fixture.all(&b), []);
    assert_eq!(fixture.store.get(&b, &blob).unwrap(), None);
    fixture
        .store
        .remove(&fixture.lease(&b), &[blob.clone()])
        .unwrap();
    assert_eq!(fixture.all(&a), [blob]);
}

/// A blob of a kind the store does not register is refused as an invalid request, on a read as
/// on a write.
pub fn an_unregistered_kind_is_refused_on_every_operation(harness: &dyn BlobHarness) {
    let fixture = Fixture::new(harness);
    let document = document("a");
    let lease = fixture.lease(&document);
    let other = BlobRef::new("other", sha256_hex(b"one")).unwrap();
    assert!(is_invalid(&fixture.store.put(&lease, "other", b"one")));
    assert!(is_invalid(&fixture.store.get(&document, &other)));
    assert!(is_invalid(
        &fixture.store.check(&document, &[other.clone()])
    ));
    assert!(is_invalid(&fixture.store.remove(&lease, &[other])));
    assert_eq!(fixture.all(&document), []);
}

/// An absent blob is `None` to `get`, and `check` reports it as missing, in the order the blobs
/// were given, passing over those present.
pub fn an_absent_blob_is_none_to_get_and_missing_to_check(harness: &dyn BlobHarness) {
    let fixture = Fixture::new(harness);
    let document = document("a");
    let present = fixture.put(&document, "capture", b"present");
    let absent = named("capture", b"absent");
    let other = named("text", b"absent");
    assert_eq!(fixture.store.get(&document, &absent).unwrap(), None);
    assert_eq!(
        fixture
            .store
            .check(&document, &[other.clone(), present.clone(), absent.clone()])
            .unwrap(),
        [BlobFault::Missing(other), BlobFault::Missing(absent)]
    );
    assert_eq!(fixture.store.check(&document, &[present]).unwrap(), []);
}

/// `list` pages a document's blobs by kind, then digest, after the blob it is given.
pub fn list_pages_by_kind_then_digest(harness: &dyn BlobHarness) {
    let fixture = Fixture::new(harness);
    let document = document("a");
    let mut expected = Vec::new();
    for (kind, bytes) in [
        ("text", &b"one"[..]),
        ("capture", b"two"),
        ("capture", b"three"),
        ("text", b"four"),
        ("capture", b"five"),
    ] {
        expected.push(fixture.put(&document, kind, bytes));
    }
    expected.sort();
    assert_eq!(fixture.all(&document), expected);

    let mut paged = Vec::new();
    let mut after = None;
    loop {
        let page = fixture.store.list(&document, after.as_ref(), 2).unwrap();
        assert!(page.len() <= 2);
        if page.is_empty() {
            break;
        }
        after = page.last().cloned();
        paged.extend(page);
    }
    assert_eq!(paged, expected);
    assert_eq!(
        fixture
            .store
            .list(&document, expected.last(), usize::MAX)
            .unwrap(),
        []
    );
}

/// A page of none is refused as an invalid request.
pub fn an_empty_page_is_invalid(harness: &dyn BlobHarness) {
    let fixture = Fixture::new(harness);
    let document = document("a");
    assert!(is_invalid(&fixture.store.list(&document, None, 0)));
}

/// A removal removes the blobs it names, whether or not each is present, and no other.
pub fn a_removal_removes_the_blobs_it_names_and_nothing_else(harness: &dyn BlobHarness) {
    let fixture = Fixture::new(harness);
    let document = document("a");
    let one = fixture.put(&document, "capture", b"one");
    let two = fixture.put(&document, "text", b"two");
    let three = fixture.put(&document, "capture", b"three");
    let absent = named("text", b"absent");
    fixture
        .store
        .remove(
            &fixture.lease(&document),
            &[one.clone(), absent, three.clone(), one.clone()],
        )
        .unwrap();
    assert_eq!(fixture.all(&document), [two.clone()]);
    assert_eq!(fixture.store.get(&document, &one).unwrap(), None);
    assert_eq!(
        fixture.store.get(&document, &two).unwrap().as_deref(),
        Some(&b"two"[..])
    );
    fixture
        .store
        .remove(&fixture.lease(&document), &[one, three])
        .unwrap();
    assert_eq!(fixture.all(&document), [two]);
}

/// A read leaves the store as it found it.
pub fn reads_change_nothing(harness: &dyn BlobHarness) {
    let fixture = Fixture::new(harness);
    let document = document("a");
    let blob = fixture.put(&document, "capture", b"one");
    let before = fixture.all(&document);
    fixture.store.get(&document, &blob).unwrap();
    fixture.store.get(&document, &named("text", b"x")).unwrap();
    fixture
        .store
        .check(&document, &[blob, named("text", b"x")])
        .unwrap();
    fixture.store.list(&document, None, 1).unwrap();
    assert_eq!(fixture.all(&document), before);
}

/// Bytes that do not match their digest are refused by `get` with `blob_corrupt`, never
/// returned, and reported by `check`. The blob stays listed, and a put of the right bytes
/// restores it.
pub fn a_corrupt_blob_is_refused_by_get_and_reported_by_check(breaker: &dyn BlobBreaker) {
    let fixture = Fixture::new(breaker);
    let document = document("a");
    let blob = fixture.put(&document, "capture", b"one");
    breaker.corrupt(&document, &blob);
    let refused = fixture.store.get(&document, &blob);
    assert_eq!(
        refused.as_ref().err().and_then(BlobFault::of),
        Some(BlobFault::Corrupt(blob.clone())),
        "{refused:?}"
    );
    assert_eq!(
        fixture.store.check(&document, &[blob.clone()]).unwrap(),
        [BlobFault::Corrupt(blob.clone())]
    );
    assert_eq!(fixture.all(&document), [blob.clone()]);
    assert_eq!(fixture.put(&document, "capture", b"one"), blob);
    assert_eq!(
        fixture.store.get(&document, &blob).unwrap().as_deref(),
        Some(&b"one"[..])
    );
}

/// A blob deleted outside the store is `None` to `get` and missing to `check`, and stays so
/// until its bytes are put again: no read restores it.
pub fn a_deleted_blob_is_reported_and_never_restored(breaker: &dyn BlobBreaker) {
    let fixture = Fixture::new(breaker);
    let document = document("a");
    let blob = fixture.put(&document, "capture", b"one");
    breaker.delete(&document, &blob);
    assert_eq!(
        fixture.store.check(&document, &[blob.clone()]).unwrap(),
        [BlobFault::Missing(blob.clone())]
    );
    assert_eq!(fixture.store.get(&document, &blob).unwrap(), None);
    assert_eq!(
        fixture.store.check(&document, &[blob.clone()]).unwrap(),
        [BlobFault::Missing(blob.clone())]
    );
    assert_eq!(fixture.all(&document), []);
    assert_eq!(fixture.put(&document, "capture", b"one"), blob);
    assert_eq!(
        fixture.store.get(&document, &blob).unwrap().as_deref(),
        Some(&b"one"[..])
    );
}

#[cfg(test)]
mod tests {
    //! The broken-blob rules over a memory store whose blobs a test breaks, so the suite's own
    //! rules are tested before an adapter that can be broken runs them.

    use super::*;
    use crate::blobs::MemoryBlobStore;
    use std::collections::BTreeMap;
    use walthamstow_kernel::KernelResult;

    #[derive(Debug, Clone, Copy)]
    enum Broken {
        Corrupt,
        Deleted,
    }

    type Faults = Arc<Mutex<BTreeMap<(DocumentId, BlobRef), Broken>>>;

    #[derive(Debug)]
    struct Breakable {
        inner: MemoryBlobStore,
        faults: Faults,
    }

    impl Breakable {
        fn broken(&self, document: &DocumentId, blob: &BlobRef) -> Option<Broken> {
            locked(&self.faults)
                .get(&(document.clone(), blob.clone()))
                .copied()
        }
    }

    impl BlobStore for Breakable {
        fn put(&self, lease: &Lease, kind: &str, bytes: &[u8]) -> KernelResult<BlobRef> {
            let blob = self.inner.put(lease, kind, bytes)?;
            locked(&self.faults).remove(&(lease.document().clone(), blob.clone()));
            Ok(blob)
        }

        fn get(&self, document: &DocumentId, blob: &BlobRef) -> KernelResult<Option<Vec<u8>>> {
            match self.broken(document, blob) {
                Some(Broken::Corrupt) => Err(BlobFault::Corrupt(blob.clone()).error(document)),
                Some(Broken::Deleted) => {
                    self.inner.get(document, blob)?;
                    Ok(None)
                }
                None => self.inner.get(document, blob),
            }
        }

        fn list(
            &self,
            document: &DocumentId,
            after: Option<&BlobRef>,
            limit: usize,
        ) -> KernelResult<Vec<BlobRef>> {
            Ok(self
                .inner
                .list(document, after, limit)?
                .into_iter()
                .filter(|blob| !matches!(self.broken(document, blob), Some(Broken::Deleted)))
                .collect())
        }

        fn check(&self, document: &DocumentId, blobs: &[BlobRef]) -> KernelResult<Vec<BlobFault>> {
            let mut faults = Vec::new();
            for blob in blobs {
                match self.broken(document, blob) {
                    Some(Broken::Corrupt) => faults.push(BlobFault::Corrupt(blob.clone())),
                    Some(Broken::Deleted) => faults.push(BlobFault::Missing(blob.clone())),
                    None => faults.extend(self.inner.check(document, std::slice::from_ref(blob))?),
                }
            }
            Ok(faults)
        }

        fn remove(&self, lease: &Lease, blobs: &[BlobRef]) -> KernelResult<()> {
            self.inner.remove(lease, blobs)
        }
    }

    #[derive(Default)]
    struct BreakableHarness {
        faults: Faults,
    }

    impl BlobHarness for BreakableHarness {
        fn store(&self, leases: Arc<dyn Leases>, kinds: BlobKinds) -> Box<dyn BlobStore> {
            Box::new(Breakable {
                inner: MemoryBlobStore::new(leases, kinds),
                faults: self.faults.clone(),
            })
        }
    }

    impl BlobBreaker for BreakableHarness {
        fn corrupt(&self, document: &DocumentId, blob: &BlobRef) {
            locked(&self.faults).insert((document.clone(), blob.clone()), Broken::Corrupt);
        }

        fn delete(&self, document: &DocumentId, blob: &BlobRef) {
            locked(&self.faults).insert((document.clone(), blob.clone()), Broken::Deleted);
        }
    }

    crate::blob_store_conformance!(broken = BreakableHarness::default());
}
