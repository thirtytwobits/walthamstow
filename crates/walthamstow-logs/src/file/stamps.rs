//! Copyright (c) Scott A Dixon
//!
//! The stamp cache: what a log file's committed lines give, recorded against a stamp of the file
//! so a read whose file still matches the stamp need not hash it. A stamp is what the platform
//! offers: device, inode, length, modification time and change time on Unix; length and
//! modification time elsewhere.
//!
//! A file changed less than the settled age before its stamp was taken can change again without
//! changing its stamp, so only a stamp taken at least the settled age after the file last changed
//! is recorded. The cache is a `LocalStore` journal, `log-stamps`, with one record per document.
//! Losing it costs one hash per document; a record of another format is passed over.

use super::format::Committed;
use crate::types::Position;
use cap_std::fs::Metadata;
use serde::{Deserialize, Serialize};
use std::sync::Arc;
use std::time::{Duration, SystemTime};
use walthamstow_kernel::journals::Journal;
use walthamstow_kernel::local_store::{JournalKey, LocalStore};
use walthamstow_kernel::DocumentId;

/// The journal the stamp cache is kept in.
pub(super) const JOURNAL: Journal = Journal::new("log-stamps");

/// The format of a stamp record this release writes and reads.
const FORMAT: u32 = 1;

/// A log file's stamp.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub(crate) struct Stamp {
    #[cfg(unix)]
    device: u64,
    #[cfg(unix)]
    inode: u64,
    length: u64,
    /// Nanoseconds since the UNIX epoch.
    modified: i64,
    /// Nanoseconds since the UNIX epoch.
    #[cfg(unix)]
    changed: i64,
}

/// `time` in nanoseconds since the UNIX epoch, negative before it.
fn nanoseconds(time: SystemTime) -> i64 {
    match time.duration_since(SystemTime::UNIX_EPOCH) {
        Ok(after) => i64::try_from(after.as_nanos()).unwrap_or(i64::MAX),
        Err(before) => i64::try_from(before.duration().as_nanos()).map_or(i64::MIN, |n| -n),
    }
}

impl Stamp {
    /// The stamp of the file `metadata` describes.
    pub fn of(metadata: &Metadata) -> std::io::Result<Self> {
        #[cfg(unix)]
        use cap_std::fs::MetadataExt;
        Ok(Self {
            #[cfg(unix)]
            device: metadata.dev(),
            #[cfg(unix)]
            inode: metadata.ino(),
            length: metadata.len(),
            modified: nanoseconds(metadata.modified()?.into_std()),
            #[cfg(unix)]
            changed: metadata
                .ctime()
                .saturating_mul(1_000_000_000)
                .saturating_add(metadata.ctime_nsec()),
        })
    }

    /// When the file last changed, in nanoseconds since the UNIX epoch, by the platform's
    /// account.
    pub fn changed_at(&self) -> i64 {
        #[cfg(unix)]
        {
            self.modified.max(self.changed)
        }
        #[cfg(not(unix))]
        {
            self.modified
        }
    }
}

/// A stamp record: the stamp, the next position the head held, and what the committed lines
/// gave under it.
#[derive(Debug, Serialize, Deserialize)]
struct Record {
    format: u32,
    stamp: Stamp,
    next: Position,
    chain: String,
    length: u64,
    terminated: bool,
}

/// The stamp cache.
#[derive(Debug)]
pub(super) struct Stamps {
    local: Arc<dyn LocalStore>,
    settled: Duration,
}

impl Stamps {
    pub fn new(local: Arc<dyn LocalStore>, settled: Duration) -> Self {
        Self { local, settled }
    }

    fn key(document: &DocumentId) -> Option<JournalKey> {
        JournalKey::new(JOURNAL, [document.entity(), document.resource()]).ok()
    }

    /// What `document`'s committed lines give, when the log file's stamp is `stamp`, the head's
    /// next position is `next`, and a record vouches for both.
    pub fn lookup(
        &self,
        document: &DocumentId,
        stamp: &Stamp,
        next: Position,
    ) -> Option<Committed> {
        let (bytes, _) = self.local.get(&Self::key(document)?).ok()??;
        let record: Record = serde_json::from_slice(&bytes).ok()?;
        (record.format == FORMAT && &record.stamp == stamp && record.next == next).then_some(
            Committed {
                chain: record.chain,
                length: record.length,
                terminated: record.terminated,
            },
        )
    }

    /// Records `committed` for `document` against `stamp` and `next`, taken at `now`, when the
    /// file had settled by then. A record that cannot be written is left as it was, since the
    /// cache only spares a hash.
    pub fn record(
        &self,
        document: &DocumentId,
        stamp: &Stamp,
        next: Position,
        committed: &Committed,
        now: SystemTime,
    ) {
        let settled = i64::try_from(self.settled.as_nanos()).unwrap_or(i64::MAX);
        if nanoseconds(now).saturating_sub(stamp.changed_at()) < settled {
            return;
        }
        let Some(key) = Self::key(document) else {
            return;
        };
        let record = Record {
            format: FORMAT,
            stamp: stamp.clone(),
            next,
            chain: committed.chain.clone(),
            length: committed.length,
            terminated: committed.terminated,
        };
        let bytes = serde_json::to_vec(&record).expect("a stamp record serialises");
        let Ok(current) = self.local.get(&key) else {
            return;
        };
        let _ = self
            .local
            .compare_and_swap(&key, current.as_ref().map(|(_, tag)| tag), &bytes);
    }

    /// Forgets `document`'s record, as a removal does. A record that cannot be removed is left; a
    /// later document's log file matches it only with the same stamp and next position.
    pub fn forget(&self, document: &DocumentId) {
        let Some(key) = Self::key(document) else {
            return;
        };
        if let Ok(Some((_, tag))) = self.local.get(&key) {
            let _ = self.local.remove(&key, &tag);
        }
    }
}
