//! Copyright (c) Scott A Dixon
//!
//! Who may change what does not merge. A lease grants one holder a scope, a document, until it
//! is released or lapses. Each grant carries an epoch no earlier grant in the process shares, and
//! a store writes under a lease only while that grant stands, so a holder whose lease was lost
//! cannot write over the next holder's work.
//!
//! [`FileLeases`] holds the workspace lock for as long as it is open, so no other engine grants
//! leases over the workspace, and its leases never lapse. [`MemoryLeases`] grants leases that
//! lapse on the engine's clock, for an engine with no disk and for tests.

use crate::clock::Moment;
use crate::workspace_lock::WorkspaceLock;
use crate::{reason_codes, KernelError, KernelResult};
use std::collections::HashMap;
use std::fmt;
use std::path::Path;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex, MutexGuard, PoisonError};
use std::time::Duration;

/// What a lease covers: one document, named by its entity and resource.
#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct LeaseScope {
    entity: String,
    resource: String,
}

impl LeaseScope {
    pub fn new(entity: impl Into<String>, resource: impl Into<String>) -> Self {
        Self {
            entity: entity.into(),
            resource: resource.into(),
        }
    }

    pub fn entity(&self) -> &str {
        &self.entity
    }

    pub fn resource(&self) -> &str {
        &self.resource
    }
}

impl fmt::Display for LeaseScope {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(formatter, "{} {:?}", self.entity, self.resource)
    }
}

/// Who holds a lease: an engine, by the name it gives itself.
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub struct LeaseHolder(String);

impl LeaseHolder {
    pub fn new(name: impl Into<String>) -> Self {
        Self(name.into())
    }

    pub fn as_str(&self) -> &str {
        &self.0
    }
}

impl fmt::Display for LeaseHolder {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str(&self.0)
    }
}

/// One grant of a scope to a holder.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Lease {
    scope: LeaseScope,
    holder: LeaseHolder,
    epoch: u64,
    expires: Option<Moment>,
}

impl Lease {
    pub fn scope(&self) -> &LeaseScope {
        &self.scope
    }

    pub fn holder(&self) -> &LeaseHolder {
        &self.holder
    }

    /// The grant's epoch. Every grant in the process has an epoch of its own, later than every
    /// grant before it.
    pub fn epoch(&self) -> u64 {
        self.epoch
    }

    /// When the lease lapses unless it is renewed, or `None` when it never lapses.
    pub fn expires(&self) -> Option<Moment> {
        self.expires
    }
}

/// The leases over a set of documents.
pub trait Leases: Send + Sync + fmt::Debug {
    /// Grants `scope` to `holder`. `None` while another holder's lease on it stands. A holder that
    /// already holds `scope` is given its lease again, renewed.
    fn acquire(&self, scope: &LeaseScope, holder: &LeaseHolder) -> KernelResult<Option<Lease>>;

    /// `lease`, renewed from now. `None` once it has been released or has lapsed.
    fn renew(&self, lease: &Lease) -> KernelResult<Option<Lease>>;

    /// Gives `lease` back. Releasing a lease that was released or has lapsed changes nothing.
    fn release(&self, lease: &Lease) -> KernelResult<()>;

    /// Runs `write` once while `lease` stands, holding its scope against every other grant
    /// until `write` returns. Refused with `lease_lost`, without running `write`, once the lease
    /// has been released or has lapsed.
    fn fence(&self, lease: &Lease, write: &mut dyn FnMut() -> KernelResult<()>)
        -> KernelResult<()>;
}

impl dyn Leases + '_ {
    /// [`Leases::fence`] for a write that returns a value or its own error.
    pub fn fenced<T, E: From<KernelError>>(
        &self,
        lease: &Lease,
        write: impl FnOnce() -> Result<T, E>,
    ) -> Result<T, E> {
        let mut write = Some(write);
        let mut written = None;
        self.fence(lease, &mut || {
            let write = write.take().expect("a fence runs its write once");
            written = Some(write());
            Ok(())
        })?;
        written.expect("a fence that succeeds has run its write")
    }
}

/// The next epoch. One counter serves every set of leases in the process, so a lease never
/// stands in a set it was not granted by.
fn next_epoch() -> u64 {
    static EPOCH: AtomicU64 = AtomicU64::new(1);
    EPOCH.fetch_add(1, Ordering::Relaxed)
}

fn lost(lease: &Lease) -> KernelError {
    KernelError::conflict(format!(
        "The lease {} held on {} has been released or has lapsed.",
        lease.epoch, lease.scope
    ))
    .with_reason(reason_codes::LEASE_LOST)
}

type Now = Arc<dyn Fn() -> Moment + Send + Sync>;

/// The grant standing on one scope, if any.
type Slot = Arc<Mutex<Option<Lease>>>;

fn locked<T>(mutex: &Mutex<T>) -> MutexGuard<'_, T> {
    mutex.lock().unwrap_or_else(PoisonError::into_inner)
}

/// Grants by scope. Each scope has a lock of its own, so a write under one lease holds back
/// grants of its scope alone.
struct Grants {
    slots: Mutex<HashMap<LeaseScope, Slot>>,
    /// How long a grant stands unless renewed, or `None` when grants never lapse.
    term: Option<Duration>,
    now: Now,
}

impl Grants {
    fn new(term: Option<Duration>, now: Now) -> Self {
        Self {
            slots: Mutex::new(HashMap::new()),
            term,
            now,
        }
    }

    fn slot(&self, scope: &LeaseScope) -> Slot {
        locked(&self.slots)
            .entry(scope.clone())
            .or_default()
            .clone()
    }

    fn expires(&self) -> Option<Moment> {
        self.term.map(|term| (self.now)() + term)
    }

    /// The grant in `slot` while it stands.
    fn standing<'a>(&self, slot: &'a mut Option<Lease>) -> Option<&'a mut Lease> {
        let now = (self.now)();
        slot.as_mut()
            .filter(|lease| lease.expires.is_none_or(|expires| now < expires))
    }

    fn acquire(&self, scope: &LeaseScope, holder: &LeaseHolder) -> Option<Lease> {
        let slot = self.slot(scope);
        let mut granted = locked(&slot);
        let expires = self.expires();
        if let Some(standing) = self.standing(&mut granted) {
            if &standing.holder != holder {
                return None;
            }
            standing.expires = expires;
            return Some(standing.clone());
        }
        let lease = Lease {
            scope: scope.clone(),
            holder: holder.clone(),
            epoch: next_epoch(),
            expires,
        };
        *granted = Some(lease.clone());
        Some(lease)
    }

    fn renew(&self, lease: &Lease) -> Option<Lease> {
        let slot = self.slot(&lease.scope);
        let mut granted = locked(&slot);
        let expires = self.expires();
        let standing = self
            .standing(&mut granted)
            .filter(|standing| standing.epoch == lease.epoch)?;
        standing.expires = expires;
        Some(standing.clone())
    }

    fn release(&self, lease: &Lease) {
        let Some(slot) = locked(&self.slots).get(&lease.scope).cloned() else {
            return;
        };
        {
            let mut granted = locked(&slot);
            if granted
                .as_ref()
                .is_some_and(|standing| standing.epoch == lease.epoch)
            {
                *granted = None;
            }
        }
        // While the map and this call hold the only references, nothing else can reach the
        // slot, so an empty one is dropped.
        let mut slots = locked(&self.slots);
        if slots
            .get(&lease.scope)
            .is_some_and(|held| Arc::ptr_eq(held, &slot))
            && Arc::strong_count(&slot) == 2
            && locked(&slot).is_none()
        {
            slots.remove(&lease.scope);
        }
    }

    fn fence(
        &self,
        lease: &Lease,
        write: &mut dyn FnMut() -> KernelResult<()>,
    ) -> KernelResult<()> {
        let slot = self.slot(&lease.scope);
        let mut granted = locked(&slot);
        if self
            .standing(&mut granted)
            .is_none_or(|standing| standing.epoch != lease.epoch)
        {
            return Err(lost(lease));
        }
        write()
    }
}

/// Leases over a workspace held by its workspace lock. While these are open no other engine
/// opens them, and their leases never lapse.
pub struct FileLeases {
    _lock: WorkspaceLock,
    grants: Grants,
}

impl fmt::Debug for FileLeases {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.debug_struct("FileLeases").finish_non_exhaustive()
    }
}

impl FileLeases {
    /// The leases over the workspace rooted at `root`, holding its workspace lock until they are
    /// dropped. Refused with `workspace_held` while another engine holds it.
    pub fn open(root: &Path) -> anyhow::Result<Self> {
        Ok(Self {
            _lock: WorkspaceLock::acquire(root)?,
            grants: Grants::new(None, Arc::new(|| Moment::ORIGIN)),
        })
    }
}

/// Leases kept in memory, which lapse a term after they were granted or last renewed. Clones
/// share one set of leases.
#[derive(Clone)]
pub struct MemoryLeases {
    grants: Arc<Grants>,
}

impl fmt::Debug for MemoryLeases {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("MemoryLeases")
            .field("term", &self.grants.term)
            .finish_non_exhaustive()
    }
}

impl MemoryLeases {
    /// Leases that stand for `term`, read against the moment `now` gives.
    pub fn new(term: Duration, now: impl Fn() -> Moment + Send + Sync + 'static) -> Self {
        Self {
            grants: Arc::new(Grants::new(Some(term), Arc::new(now))),
        }
    }
}

macro_rules! leases_over_grants {
    ($leases:ty) => {
        impl Leases for $leases {
            fn acquire(
                &self,
                scope: &LeaseScope,
                holder: &LeaseHolder,
            ) -> KernelResult<Option<Lease>> {
                Ok(self.grants.acquire(scope, holder))
            }

            fn renew(&self, lease: &Lease) -> KernelResult<Option<Lease>> {
                Ok(self.grants.renew(lease))
            }

            fn release(&self, lease: &Lease) -> KernelResult<()> {
                self.grants.release(lease);
                Ok(())
            }

            fn fence(
                &self,
                lease: &Lease,
                write: &mut dyn FnMut() -> KernelResult<()>,
            ) -> KernelResult<()> {
                self.grants.fence(lease, write)
            }
        }
    };
}

leases_over_grants!(FileLeases);
leases_over_grants!(MemoryLeases);

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::mpsc;

    const TERM: Duration = Duration::from_secs(30);

    /// A clock that moves only when a test moves it.
    #[derive(Clone)]
    struct Hands(Arc<Mutex<Moment>>);

    impl Hands {
        fn new() -> Self {
            Self(Arc::new(Mutex::new(Moment::ORIGIN)))
        }

        fn advance(&self, by: Duration) {
            let mut now = locked(&self.0);
            *now = *now + by;
        }

        fn leases(&self) -> MemoryLeases {
            let hands = self.clone();
            MemoryLeases::new(TERM, move || *locked(&hands.0))
        }
    }

    fn document(resource: &str) -> LeaseScope {
        LeaseScope::new("Session", resource)
    }

    fn engine(name: &str) -> LeaseHolder {
        LeaseHolder::new(name)
    }

    /// Writes nothing under `lease`.
    fn write(leases: &dyn Leases, lease: &Lease) -> KernelResult<()> {
        leases.fenced(lease, || Ok(()))
    }

    fn is_lost(error: &KernelError) -> bool {
        error.reason_code() == Some(reason_codes::LEASE_LOST)
    }

    /// Every adapter, each over a scratch workspace it holds while the test runs.
    fn adapters() -> Vec<(Arc<dyn Leases>, Option<tempfile::TempDir>)> {
        let root = tempfile::tempdir().unwrap();
        vec![
            (Arc::new(Hands::new().leases()), None),
            (Arc::new(FileLeases::open(root.path()).unwrap()), Some(root)),
        ]
    }

    #[test]
    fn a_scope_held_by_one_holder_is_refused_to_another_until_released() {
        for (leases, _root) in adapters() {
            let first = leases
                .acquire(&document("held"), &engine("a"))
                .unwrap()
                .unwrap();
            assert!(leases
                .acquire(&document("held"), &engine("b"))
                .unwrap()
                .is_none());
            leases.release(&first).unwrap();
            let second = leases
                .acquire(&document("held"), &engine("b"))
                .unwrap()
                .unwrap();
            assert_eq!(second.holder(), &engine("b"));
            assert!(second.epoch() > first.epoch());
        }
    }

    #[test]
    fn scopes_are_granted_independently() {
        for (leases, _root) in adapters() {
            let one = leases.acquire(&document("one"), &engine("a")).unwrap();
            let two = leases.acquire(&document("two"), &engine("b")).unwrap();
            assert!(one.is_some() && two.is_some());
        }
    }

    #[test]
    fn a_holder_acquiring_its_own_scope_again_keeps_its_grant() {
        for (leases, _root) in adapters() {
            let first = leases
                .acquire(&document("mine"), &engine("a"))
                .unwrap()
                .unwrap();
            let again = leases
                .acquire(&document("mine"), &engine("a"))
                .unwrap()
                .unwrap();
            assert_eq!(again.epoch(), first.epoch());
            write(&*leases, &first).unwrap();
        }
    }

    #[test]
    fn a_fence_runs_its_write_once_and_returns_what_it_wrote() {
        for (leases, _root) in adapters() {
            let lease = leases
                .acquire(&document("written"), &engine("a"))
                .unwrap()
                .unwrap();
            let mut runs = 0;
            let written = leases
                .fenced(&lease, || {
                    runs += 1;
                    Ok::<_, KernelError>(runs)
                })
                .unwrap();
            assert_eq!((runs, written), (1, 1));
        }
    }

    #[test]
    fn a_fence_returns_its_writes_refusal() {
        for (leases, _root) in adapters() {
            let lease = leases
                .acquire(&document("refused"), &engine("a"))
                .unwrap()
                .unwrap();
            let refusal = leases
                .fenced(&lease, || -> KernelResult<()> {
                    Err(KernelError::invalid_request("refused by the store"))
                })
                .unwrap_err();
            assert!(!is_lost(&refusal), "{refusal:?}");
        }
    }

    #[test]
    fn a_released_lease_fences_no_write() {
        for (leases, _root) in adapters() {
            let lease = leases
                .acquire(&document("released"), &engine("a"))
                .unwrap()
                .unwrap();
            leases.release(&lease).unwrap();
            let refusal = leases
                .fenced(&lease, || -> KernelResult<()> {
                    panic!("a released lease runs no write")
                })
                .unwrap_err();
            assert!(is_lost(&refusal), "{refusal:?}");
            assert!(leases.renew(&lease).unwrap().is_none());
        }
    }

    #[test]
    fn a_lease_lost_to_a_later_grant_fences_no_write() {
        for (leases, _root) in adapters() {
            let stale = leases
                .acquire(&document("taken"), &engine("a"))
                .unwrap()
                .unwrap();
            leases.release(&stale).unwrap();
            let later = leases
                .acquire(&document("taken"), &engine("a"))
                .unwrap()
                .unwrap();
            assert!(is_lost(&write(&*leases, &stale).unwrap_err()));
            write(&*leases, &later).unwrap();
            leases.release(&stale).unwrap();
            write(&*leases, &later).unwrap();
        }
    }

    #[test]
    fn a_lease_stands_only_in_the_set_that_granted_it() {
        let elsewhere = Hands::new().leases();
        let granted = elsewhere
            .acquire(&document("one"), &engine("a"))
            .unwrap()
            .unwrap();
        for (leases, _root) in adapters() {
            leases
                .acquire(&document("one"), &engine("a"))
                .unwrap()
                .unwrap();
            assert!(is_lost(&write(&*leases, &granted).unwrap_err()));
        }
    }

    #[test]
    fn a_memory_lease_lapses_a_term_after_its_grant() {
        let hands = Hands::new();
        let leases: Arc<dyn Leases> = Arc::new(hands.leases());
        let lease = leases
            .acquire(&document("lapsing"), &engine("a"))
            .unwrap()
            .unwrap();
        assert_eq!(lease.expires(), Some(Moment::ORIGIN + TERM));
        hands.advance(TERM - Duration::from_millis(1));
        write(&*leases, &lease).unwrap();
        hands.advance(Duration::from_millis(1));
        assert!(is_lost(&write(&*leases, &lease).unwrap_err()));
        assert!(leases.renew(&lease).unwrap().is_none());
        let next = leases
            .acquire(&document("lapsing"), &engine("b"))
            .unwrap()
            .unwrap();
        assert!(next.epoch() > lease.epoch());
    }

    #[test]
    fn a_renewed_memory_lease_stands_a_term_from_its_renewal() {
        let hands = Hands::new();
        let leases: Arc<dyn Leases> = Arc::new(hands.leases());
        let lease = leases
            .acquire(&document("renewed"), &engine("a"))
            .unwrap()
            .unwrap();
        hands.advance(TERM / 2);
        let renewed = leases.renew(&lease).unwrap().unwrap();
        assert_eq!(renewed.epoch(), lease.epoch());
        hands.advance(TERM / 2 + TERM / 4);
        write(&*leases, &lease).unwrap();
        assert!(leases
            .acquire(&document("renewed"), &engine("b"))
            .unwrap()
            .is_none());
    }

    #[test]
    fn a_holder_acquiring_its_memory_lease_again_renews_it() {
        let hands = Hands::new();
        let leases: Arc<dyn Leases> = Arc::new(hands.leases());
        leases
            .acquire(&document("again"), &engine("a"))
            .unwrap()
            .unwrap();
        hands.advance(TERM / 2);
        let again = leases
            .acquire(&document("again"), &engine("a"))
            .unwrap()
            .unwrap();
        assert_eq!(again.expires(), Some(Moment::ORIGIN + TERM / 2 + TERM));
    }

    #[test]
    fn a_file_lease_never_lapses() {
        let root = tempfile::tempdir().unwrap();
        let leases = FileLeases::open(root.path()).unwrap();
        let lease = leases
            .acquire(&document("kept"), &engine("a"))
            .unwrap()
            .unwrap();
        assert_eq!(lease.expires(), None);
    }

    #[test]
    fn a_workspace_whose_leases_are_open_refuses_a_second_open_until_they_are_dropped() {
        let root = tempfile::tempdir().unwrap();
        let first = FileLeases::open(root.path()).unwrap();
        let refused = FileLeases::open(root.path()).unwrap_err();
        assert_eq!(
            refused
                .downcast_ref::<KernelError>()
                .and_then(KernelError::reason_code),
            Some(reason_codes::WORKSPACE_HELD)
        );
        drop(first);
        FileLeases::open(root.path()).unwrap();
    }

    #[test]
    fn a_lease_from_leases_since_dropped_fences_no_write_in_those_opened_after() {
        let root = tempfile::tempdir().unwrap();
        let first = FileLeases::open(root.path()).unwrap();
        let stale = first
            .acquire(&document("reopened"), &engine("a"))
            .unwrap()
            .unwrap();
        drop(first);
        let reopened: Arc<dyn Leases> = Arc::new(FileLeases::open(root.path()).unwrap());
        reopened
            .acquire(&document("reopened"), &engine("a"))
            .unwrap()
            .unwrap();
        assert!(is_lost(&write(&*reopened, &stale).unwrap_err()));
    }

    #[test]
    fn a_lease_lapsing_while_its_write_runs_is_granted_again_only_after_the_write() {
        let hands = Hands::new();
        let leases = hands.leases();
        let lease = leases
            .acquire(&document("busy"), &engine("a"))
            .unwrap()
            .unwrap();
        let order = Arc::new(Mutex::new(Vec::new()));
        let (entered, in_write) = mpsc::channel();
        let (finish, finished) = mpsc::channel::<()>();
        let writer = {
            let leases: Arc<dyn Leases> = Arc::new(leases.clone());
            let order = order.clone();
            std::thread::spawn(move || {
                leases
                    .fenced(&lease, || {
                        entered.send(()).unwrap();
                        finished.recv().unwrap();
                        locked(&order).push("written");
                        Ok::<_, KernelError>(())
                    })
                    .unwrap();
            })
        };
        in_write.recv().unwrap();
        hands.advance(TERM);
        let taker = {
            let order = order.clone();
            std::thread::spawn(move || {
                leases
                    .acquire(&document("busy"), &engine("b"))
                    .unwrap()
                    .unwrap();
                locked(&order).push("granted");
            })
        };
        finish.send(()).unwrap();
        writer.join().unwrap();
        taker.join().unwrap();
        assert_eq!(*locked(&order), ["written", "granted"]);
    }
}
