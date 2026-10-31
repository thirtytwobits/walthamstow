//! Copyright (c) Scott A Dixon
//!
//! Log documents kept as files people can read, keep in git, copy and edit. Each document is a
//! directory of the [`DocumentLayout`], holding a head file and a log file the product names for
//! its entity; every other entry there is the product's, and the store neither reads nor changes
//! it. A document exists when its head file does.
//!
//! The files are the record. A commit appends its lines to the log and then replaces the head
//! file, whose next position is the commit point: a line counts once the head's next position is
//! past it, whoever wrote it, and a commit first drops the lines no head counts. The writes that
//! change what is committed publish through the journalled operations of `operations`. A
//! revision is a hash of the files, so a change made outside the store since a writer read a
//! document refuses the writer's write with `log_stale`. Every operation learns a document's
//! revision through the stamp cache of `stamps`.

mod format;
mod operations;
mod stamps;
#[cfg(test)]
mod tests;

pub use operations::LogRecovery;

use crate::fault::LogFault;
use crate::layout::{internal, DocumentDir, DocumentLayout};
use crate::memory::{after, require_limit, stale};
use crate::reason_codes;
use crate::store::LogStore;
use crate::types::{
    Listed, LogCommit, LogImage, Position, Revision, StoredHead, StoredRecord, WriteId,
};
use cap_std::fs::Dir;
use format::{Committed, HeadFile};
use operations::{Change, Operations, Published};
use stamps::{Stamp, Stamps};
use std::collections::{BTreeMap, HashMap, HashSet};
use std::fmt;
use std::io::{Read, Seek, SeekFrom, Write};
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex, MutexGuard, PoisonError, RwLock};
use std::time::Duration;
use walthamstow_kernel::durability::{write_synced_in, WriteDurability};
use walthamstow_kernel::hashing::sha256_hex;
use walthamstow_kernel::leases::{Lease, Leases};
use walthamstow_kernel::local_store::LocalStore;
use walthamstow_kernel::publication::file_hash;
use walthamstow_kernel::{DocumentId, KernelError, KernelResult, WallClock};

/// The files each document of one entity is kept in.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct LogFiles {
    head: String,
    log: String,
}

impl LogFiles {
    /// A document's head file `head` and log file `log`: each a name Windows allows, not one a
    /// durable write stages under, and the two different when compared case-folded.
    pub fn new(head: impl Into<String>, log: impl Into<String>) -> KernelResult<Self> {
        let (head, log) = (head.into(), log.into());
        for name in [&head, &log] {
            if !walthamstow_kernel::files::windows_allows(name)
                || walthamstow_kernel::durability::staged_by_write(name)
            {
                return Err(KernelError::invalid_request(format!(
                    "{name:?} cannot name a log document's file."
                )));
            }
        }
        if head.to_lowercase() == log.to_lowercase() {
            return Err(KernelError::invalid_request(format!(
                "A log document's head file and log file are one file: {head:?} and {log:?}."
            )));
        }
        Ok(Self { head, log })
    }

    pub fn head(&self) -> &str {
        &self.head
    }

    pub fn log(&self) -> &str {
        &self.log
    }
}

/// What a [`FileLogStore`] is opened with.
pub struct FileLogConfig {
    /// Where the documents are laid out.
    pub layout: Arc<DocumentLayout>,
    /// The files each entity's documents are kept in, by entity. The layout registers each
    /// entity with both files.
    pub files: BTreeMap<String, LogFiles>,
    pub durability: WriteDurability,
    /// The leases that fence every write.
    pub leases: Arc<dyn Leases>,
    /// The clock records are recorded on, and a stamp's settled age is judged by.
    pub clock: WallClock,
    /// The engine's local records, which keep the `log-operations` and `log-stamps` journals.
    pub local: Arc<dyn LocalStore>,
    /// The engine's data directory, which holds the `log-operations` staging.
    pub data_dir: PathBuf,
    /// How long after a log file last changed a stamp of it vouches for it. The product sets it
    /// at or above the resolution of the file system's clock.
    pub settled: Duration,
}

/// Log documents kept as files under a document layout.
pub struct FileLogStore {
    layout: Arc<DocumentLayout>,
    files: BTreeMap<String, LogFiles>,
    durability: WriteDurability,
    leases: Arc<dyn Leases>,
    clock: WallClock,
    stamps: Stamps,
    operations: Operations,
    /// Held by each read of a document, and by each write exclusively.
    locks: Mutex<HashMap<DocumentId, Arc<RwLock<()>>>>,
    /// The last write to each document, which a retry under the same lease repeats.
    last: Mutex<HashMap<DocumentId, LastWrite>>,
    /// The documents a write left a publication authorised for, which refuse writes until the
    /// store is opened again and recovery finishes or discards the publication.
    stopped: Mutex<HashSet<DocumentId>>,
    /// How many times a log file has been read whole.
    #[cfg(test)]
    log_reads: std::sync::atomic::AtomicUsize,
}

impl fmt::Debug for FileLogStore {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("FileLogStore")
            .field("root", &self.layout.root())
            .field("files", &self.files)
            .field("durability", &self.durability)
            .finish_non_exhaustive()
    }
}

/// The last write to a document: its lease's epoch, its id, its base, and the revision it gave,
/// or none for a removal.
struct LastWrite {
    epoch: u64,
    id: WriteId,
    base: Option<Revision>,
    given: Option<Revision>,
}

/// How much of a document a read takes in.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Reading {
    /// Its revision, which a stamp may vouch for.
    Stamped,
    /// Its revision, from a hash of the log file.
    Hashed,
    /// Its revision, records and the log file's bytes.
    Whole,
}

/// A document as its files hold it.
struct Found {
    dir: DocumentDir,
    head: HeadFile,
    head_bytes: Vec<u8>,
    /// The log file's bytes, when read whole.
    log_bytes: Option<Vec<u8>>,
    /// The committed records, when read whole.
    records: Vec<StoredRecord>,
    committed: Committed,
    revision: Revision,
}

/// What a read of a document found.
enum Held {
    Absent,
    Broken(LogFault),
    Found(Box<Found>),
}

fn locked<T>(mutex: &Mutex<T>) -> MutexGuard<'_, T> {
    mutex.lock().unwrap_or_else(PoisonError::into_inner)
}

/// Refuses a write that found `document` changed while it was written.
fn changed(document: &DocumentId) -> KernelError {
    KernelError::conflict(format!(
        "The log document {document} changed while it was written."
    ))
    .with_reason(reason_codes::LOG_STALE)
}

/// Whether `name` in `dir` is a regular file. Anything else there is refused unopened.
fn regular(dir: &Dir, name: &str) -> Result<bool, LogFault> {
    match dir.symlink_metadata(name) {
        Ok(metadata) if metadata.is_file() => Ok(true),
        Ok(_) => Err(LogFault::Inaccessible {
            detail: format!("{name} is not a regular file; it is not opened"),
        }),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(false),
        Err(error) => Err(LogFault::Inaccessible {
            detail: format!("{name}: {error}"),
        }),
    }
}

/// The log file holding `records`, and its chain.
fn log_of<'a>(records: impl IntoIterator<Item = &'a StoredRecord>) -> (Vec<u8>, String) {
    let mut bytes = Vec::new();
    let mut chain = format::empty_chain();
    for record in records {
        let line = format::line(record);
        chain = format::extend_chain(&chain, &line[..line.len() - 1]);
        bytes.extend_from_slice(&line);
    }
    (bytes, chain)
}

/// `commit`'s records as the store keeps them, from `next`, recorded at `recorded`.
fn stored(
    commit: &LogCommit,
    next: Position,
    recorded: chrono::DateTime<chrono::Utc>,
) -> Vec<StoredRecord> {
    commit
        .records
        .iter()
        .zip(next.get()..)
        .map(|(record, position)| StoredRecord {
            position: Position::new(position),
            author: record.author.clone(),
            recorded,
            blobs: record.blobs.clone(),
            payload: record.payload.clone(),
        })
        .collect()
}

impl FileLogStore {
    /// The store `config` describes, after recovery has finished each publication a record
    /// authorises and discarded staging none authorises. An entity whose files the layout does
    /// not register is refused as an invalid request.
    pub fn open(config: FileLogConfig) -> KernelResult<(Self, LogRecovery)> {
        for (entity, files) in &config.files {
            let registered = config.layout.entity_files(entity)?;
            for name in [files.head(), files.log()] {
                if !registered.contains(name) {
                    return Err(KernelError::invalid_request(format!(
                        "The layout does not register the file {name:?} for the entity \
                         {entity:?}."
                    )));
                }
            }
        }
        let store = Self {
            stamps: Stamps::new(config.local.clone(), config.settled),
            operations: Operations::new(config.local, config.data_dir, config.durability),
            layout: config.layout,
            files: config.files,
            durability: config.durability,
            leases: config.leases,
            clock: config.clock,
            locks: Mutex::new(HashMap::new()),
            last: Mutex::new(HashMap::new()),
            stopped: Mutex::new(HashSet::new()),
            #[cfg(test)]
            log_reads: std::sync::atomic::AtomicUsize::new(0),
        };
        let recovery = store.operations.recover(&store.layout)?;
        Ok((store, recovery))
    }

    /// Every document of `entity`, in resource order, each with the revision a hash of its files
    /// gives or its fault. A stamp is recorded for each whose log file has settled.
    pub fn verify(&self, entity: &str) -> KernelResult<Vec<Listed>> {
        if !self.files.contains_key(entity) {
            return Ok(Vec::new());
        }
        Ok(self
            .resources(entity)?
            .into_iter()
            .filter_map(|resource| {
                let document = DocumentId::new(entity, resource);
                self.listed(&document, Reading::Hashed)
                    .map(|state| Listed { document, state })
            })
            .collect())
    }

    fn files_of(&self, entity: &str) -> KernelResult<&LogFiles> {
        self.files.get(entity).ok_or_else(|| {
            KernelError::invalid_request(format!(
                "The entity {entity:?} keeps no log documents in this store."
            ))
        })
    }

    fn lock(&self, document: &DocumentId) -> Arc<RwLock<()>> {
        locked(&self.locks)
            .entry(document.clone())
            .or_default()
            .clone()
    }

    /// What `document`'s files hold, taken in as `reading` asks. An entity this store keeps no
    /// files for has no documents.
    fn read(&self, document: &DocumentId, reading: Reading) -> KernelResult<Held> {
        let Some(files) = self.files.get(document.entity()) else {
            return Ok(Held::Absent);
        };
        // A resource that cannot name a directory is refused; storage that refuses the directory
        // is the document's fault.
        self.layout.document_path(document)?;
        let dir = match self.layout.document_dir(document) {
            Ok(Some(dir)) => dir,
            Ok(None) => return Ok(Held::Absent),
            Err(error) => {
                return Ok(Held::Broken(LogFault::Inaccessible {
                    detail: error.message,
                }))
            }
        };
        let present = regular(&dir.dir, files.head())
            .and_then(|head| Ok((head, regular(&dir.dir, files.log())?)));
        let missing = |name: &str| {
            Ok(Held::Broken(LogFault::Missing {
                parts: vec![name.to_owned()],
            }))
        };
        match present {
            Err(fault) => return Ok(Held::Broken(fault)),
            Ok((false, false)) => return Ok(Held::Absent),
            Ok((false, true)) => return missing(files.head()),
            Ok((true, false)) => return missing(files.log()),
            Ok((true, true)) => {}
        }
        Ok(match self.read_files(document, files, dir, reading) {
            Ok(found) => Held::Found(Box::new(found)),
            Err(fault) => Held::Broken(fault),
        })
    }

    fn read_files(
        &self,
        document: &DocumentId,
        files: &LogFiles,
        dir: DocumentDir,
        reading: Reading,
    ) -> Result<Found, LogFault> {
        let inaccessible = |name: &str, error: std::io::Error| LogFault::Inaccessible {
            detail: format!("{name}: {error}"),
        };
        let head_bytes = dir
            .dir
            .read(files.head())
            .map_err(|error| inaccessible(files.head(), error))?;
        let head = HeadFile::parse(&head_bytes)?;
        let mut opened = dir
            .dir
            .open(files.log())
            .map_err(|error| inaccessible(files.log(), error))?;
        let stamp_now = |opened: &cap_std::fs::File| {
            opened
                .metadata()
                .and_then(|metadata| Stamp::of(&metadata))
                .map_err(|error| inaccessible(files.log(), error))
        };
        let stamp = stamp_now(&opened)?;
        let found = |committed: Committed, log_bytes, records| Found {
            revision: format::revision(head.incarnation, &head_bytes, &committed.chain),
            committed,
            log_bytes,
            records,
            head: head.clone(),
            head_bytes: head_bytes.clone(),
            dir,
        };
        if reading == Reading::Stamped {
            if let Some(committed) = self.stamps.lookup(document, &stamp, head.next_position) {
                return Ok(found(committed, None, Vec::new()));
            }
        }
        let mut bytes = Vec::new();
        opened
            .read_to_end(&mut bytes)
            .map_err(|error| inaccessible(files.log(), error))?;
        #[cfg(test)]
        self.log_reads
            .fetch_add(1, std::sync::atomic::Ordering::Relaxed);
        let log = format::parse_log(&bytes, head.next_position)?;
        // A stamp vouches only for the bytes read under it.
        if stamp_now(&opened)? == stamp {
            self.stamps.record(
                document,
                &stamp,
                head.next_position,
                &log.committed,
                self.clock.now(),
            );
        }
        Ok(if reading == Reading::Whole {
            found(log.committed, Some(bytes), log.records)
        } else {
            found(log.committed, None, Vec::new())
        })
    }

    /// `document` as it stands at `base`, or `log_stale` when it is at another revision or none.
    fn at(&self, document: &DocumentId, base: &Revision, reading: Reading) -> KernelResult<Found> {
        match self.read(document, reading)? {
            Held::Found(found) if &found.revision == base => Ok(*found),
            _ => Err(stale(document, base)),
        }
    }

    /// `document`'s revision or fault, as `list` reports it, or `None` when it is no document.
    fn listed(
        &self,
        document: &DocumentId,
        reading: Reading,
    ) -> Option<Result<Revision, LogFault>> {
        let lock = self.lock(document);
        let _reading = lock.read().unwrap_or_else(PoisonError::into_inner);
        match self.read(document, reading) {
            Ok(Held::Absent) => None,
            Ok(Held::Found(found)) => Some(Ok(found.revision)),
            Ok(Held::Broken(fault)) => Some(Err(fault)),
            Err(error) => Some(Err(LogFault::Inaccessible {
                detail: error.message,
            })),
        }
    }

    /// The resource of each directory in `entity`'s directory whose name the layout writes, in
    /// resource order. A link is resolved through the layout's root, as every other operation
    /// resolves it; one the root cannot resolve is kept, for its read to report.
    fn resources(&self, entity: &str) -> KernelResult<Vec<String>> {
        let Some((dir, relative)) = self.layout.entity_dir(entity)? else {
            return Ok(Vec::new());
        };
        let path = self.layout.root().join(&relative);
        let mut resources = Vec::new();
        for entry in dir.entries().map_err(|error| internal(&path, error))? {
            let entry = entry.map_err(|error| internal(&path, error))?;
            let name = entry.file_name();
            let Some(resource) = name.to_str().and_then(crate::layout::resource_of) else {
                continue;
            };
            let kind = entry.file_type().map_err(|error| internal(&path, error))?;
            let directory = kind.is_dir()
                || (kind.is_symlink()
                    && !matches!(self.layout.open_dir(&relative.join(&name)), Ok(None)));
            if directory {
                resources.push(resource);
            }
        }
        resources.sort();
        Ok(resources)
    }

    /// Runs `write` on the lease's document while the lease stands, holding the document against
    /// every other read and write in this store. A document a write left a publication authorised
    /// for is refused.
    fn fenced<T>(
        &self,
        lease: &Lease,
        write: impl FnOnce(&DocumentId) -> KernelResult<T>,
    ) -> KernelResult<T> {
        self.leases.as_ref().fenced(lease, || {
            let document = lease.document();
            let lock = self.lock(document);
            let _writing = lock.write().unwrap_or_else(PoisonError::into_inner);
            if locked(&self.stopped).contains(document) {
                return Err(KernelError::internal(format!(
                    "A write to the log document {document} left a publication authorised. Open \
                     the store again to recover it."
                )));
            }
            write(document)
        })
    }

    /// What the last write gave, when a write of `id` over `base` under `lease` repeats it.
    fn retried(
        &self,
        lease: &Lease,
        id: &WriteId,
        base: Option<&Revision>,
    ) -> Option<Option<Revision>> {
        locked(&self.last)
            .get(lease.document())
            .filter(|last| {
                last.epoch == lease.epoch() && &last.id == id && last.base.as_ref() == base
            })
            .map(|last| last.given.clone())
    }

    /// What the last write gave, when a write of `id` over `base` under `lease` repeats a write
    /// that gave a revision, once the document's files are durable: a write that landed may have
    /// failed before it made them so.
    fn repeated(
        &self,
        lease: &Lease,
        id: &WriteId,
        base: Option<&Revision>,
    ) -> KernelResult<Option<Revision>> {
        let Some(Some(given)) = self.retried(lease, id, base) else {
            return Ok(None);
        };
        self.make_durable(lease.document())?;
        Ok(Some(given))
    }

    /// Makes `document`'s head and log files and its directory's entries durable. A file that is
    /// absent has nothing to make durable.
    fn make_durable(&self, document: &DocumentId) -> KernelResult<()> {
        let files = self.files_of(document.entity())?;
        let Some(dir) = self.layout.document_dir(document)? else {
            return Ok(());
        };
        let path = self.layout.root().join(&dir.relative);
        let syncing = || -> std::io::Result<()> {
            for name in [files.log(), files.head()] {
                // Windows flushes a file only through a handle with write access.
                let mut options = cap_std::fs::OpenOptions::new();
                options.read(true).write(true);
                match dir.dir.open_with(name, &options) {
                    Ok(file) => self.durability.sync(&file.into_std())?,
                    Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
                    Err(error) => return Err(error),
                }
            }
            self.durability.sync_dir(&dir.dir)
        };
        syncing().map_err(|error| internal(&path, error))
    }

    /// Remembers a write of `id` over `base` under `lease` as giving `given`, and returns the
    /// write it replaces.
    fn remember(
        &self,
        lease: &Lease,
        id: &WriteId,
        base: Option<&Revision>,
        given: Option<Revision>,
    ) -> Option<LastWrite> {
        locked(&self.last).insert(
            lease.document().clone(),
            LastWrite {
                epoch: lease.epoch(),
                id: id.clone(),
                base: base.cloned(),
                given,
            },
        )
    }

    /// Puts back the write `previous` remembered for `document`, for a write that did not land.
    fn restore(&self, document: &DocumentId, previous: Option<LastWrite>) {
        let mut last = locked(&self.last);
        match previous {
            Some(previous) => last.insert(document.clone(), previous),
            None => last.remove(document),
        };
    }

    /// Whether `document`'s head file holds `target`, or is absent when `target` is none: whether
    /// a write whose head is `target` landed.
    fn landed(&self, document: &DocumentId, target: Option<&[u8]>) -> bool {
        let Ok(files) = self.files_of(document.entity()) else {
            return false;
        };
        match self.layout.document_dir(document) {
            Ok(Some(dir)) => file_hash(&dir.dir, Path::new(files.head()))
                .is_ok_and(|held| held == target.map(sha256_hex)),
            Ok(None) => target.is_none(),
            Err(_) => false,
        }
    }

    /// The count of writes after `writes`.
    fn next_write(document: &DocumentId, writes: u64) -> KernelResult<u64> {
        writes.checked_add(1).ok_or_else(|| {
            KernelError::internal(format!(
                "The log document {document} has been written more times than are counted."
            ))
        })
    }

    /// Publishes `changes` to `document` through a journalled operation, and once it is published
    /// remembers it as the write of `id` over `base` under `lease` that gave `given`. An operation
    /// the write does not see through stops the document, until recovery sees it through.
    fn journalled(
        &self,
        lease: &Lease,
        id: &WriteId,
        base: Option<&Revision>,
        given: Option<Revision>,
        changes: &[Change],
        remove_directory: bool,
    ) -> KernelResult<()> {
        let document = lease.document();
        let operation = self
            .operations
            .authorise(document, changes, remove_directory)?;
        let stop = |_: &KernelError| {
            locked(&self.stopped).insert(document.clone());
        };
        match self.operations.publish(&self.layout, &operation, true) {
            Ok((_, Published::Done)) => {
                self.remember(lease, id, base, given);
                self.operations.retire(&operation).inspect_err(stop)
            }
            Ok((_, Published::Stale)) => {
                self.operations.retire(&operation).inspect_err(stop)?;
                Err(changed(document))
            }
            Err(error) => {
                stop(&error);
                Err(error)
            }
        }
    }

    /// Appends `appended` to the committed lines of `found`'s log file, dropping the lines no
    /// head counts, and makes the log durable.
    fn append(&self, found: &Found, files: &LogFiles, appended: &[u8]) -> KernelResult<()> {
        let path = self
            .layout
            .root()
            .join(&found.dir.relative)
            .join(files.log());
        let appending = || -> std::io::Result<()> {
            let mut options = cap_std::fs::OpenOptions::new();
            options.read(true).write(true);
            let mut file = found.dir.dir.open_with(files.log(), &options)?.into_std();
            let committed = found.committed.length;
            if file.metadata()?.len() != committed {
                file.set_len(committed)?;
            }
            file.seek(SeekFrom::Start(committed))?;
            if !found.committed.terminated {
                file.write_all(b"\n")?;
            }
            file.write_all(appended)?;
            self.durability.sync(&file)
        };
        appending().map_err(|error| internal(&path, error))
    }

    /// Writes `found`'s document anew as `head` and `records` through a journalled operation, as
    /// the write of `id` over `base` under `lease`, and returns its revision.
    fn rewrite(
        &self,
        lease: &Lease,
        id: &WriteId,
        base: &Revision,
        found: &Found,
        head: &HeadFile,
        records: &[StoredRecord],
    ) -> KernelResult<Revision> {
        let files = self.files_of(lease.document().entity())?;
        let head_bytes = head.to_bytes();
        let (log_bytes, chain) = log_of(records);
        let revision = format::revision(head.incarnation, &head_bytes, &chain);
        let changes = [
            Change {
                name: files.log().to_owned(),
                source: found.log_bytes.as_deref().map(sha256_hex),
                target: Some(log_bytes),
            },
            Change {
                name: files.head().to_owned(),
                source: Some(sha256_hex(&found.head_bytes)),
                target: Some(head_bytes.clone()),
            },
        ];
        self.journalled(
            lease,
            id,
            Some(base),
            Some(revision.clone()),
            &changes,
            false,
        )?;
        Ok(revision)
    }
}

impl LogStore for FileLogStore {
    fn create(&self, lease: &Lease, image: &LogImage) -> KernelResult<Revision> {
        self.fenced(lease, |document| {
            if let Some(given) = self.repeated(lease, &image.id, None)? {
                return Ok(given);
            }
            let files = self.files_of(document.entity())?;
            let dir = self.layout.create_document_dir(self.durability, document)?;
            let held = regular(&dir.dir, files.head())
                .and_then(|head| Ok((head, regular(&dir.dir, files.log())?)))
                .map_err(|fault| fault.error(document))?;
            if held != (false, false) {
                return Err(KernelError::conflict(format!(
                    "The log document {document} exists, or holds a log file and no head file."
                ))
                .with_reason(reason_codes::LOG_STALE));
            }
            let head = HeadFile {
                format: image.format,
                incarnation: uuid::Uuid::new_v4(),
                writes: 1,
                next_position: image.next_position()?,
                head: image.head.clone(),
            };
            let head_bytes = head.to_bytes();
            let (log_bytes, chain) = log_of(&image.records);
            let revision = format::revision(head.incarnation, &head_bytes, &chain);
            let changes = [
                Change {
                    name: files.log().to_owned(),
                    source: None,
                    target: Some(log_bytes),
                },
                Change {
                    name: files.head().to_owned(),
                    source: None,
                    target: Some(head_bytes.clone()),
                },
            ];
            self.journalled(
                lease,
                &image.id,
                None,
                Some(revision.clone()),
                &changes,
                false,
            )?;
            Ok(revision)
        })
    }

    fn commit(&self, lease: &Lease, base: &Revision, commit: &LogCommit) -> KernelResult<Revision> {
        self.fenced(lease, |document| {
            if let Some(given) = self.repeated(lease, &commit.id, Some(base))? {
                return Ok(given);
            }
            let found = self.at(document, base, Reading::Stamped)?;
            if commit.records.is_empty() && commit.head.is_none() {
                return Err(KernelError::invalid_request(
                    "A commit carries records, a head or both.",
                ));
            }
            let files = self.files_of(document.entity())?;
            let next = after(found.head.next_position, commit.records.len())?;
            let mut appended = Vec::new();
            let mut chain = found.committed.chain.clone();
            for record in stored(commit, found.head.next_position, self.clock.utc()) {
                let line = format::line(&record);
                chain = format::extend_chain(&chain, &line[..line.len() - 1]);
                appended.extend_from_slice(&line);
            }
            let head = HeadFile {
                writes: Self::next_write(document, found.head.writes)?,
                next_position: next,
                head: commit
                    .head
                    .clone()
                    .unwrap_or_else(|| found.head.head.clone()),
                ..found.head.clone()
            };
            let head_bytes = head.to_bytes();
            let revision = format::revision(head.incarnation, &head_bytes, &chain);
            let previous = self.remember(lease, &commit.id, Some(base), Some(revision.clone()));
            let written = self.append(&found, files, &appended).and_then(|()| {
                write_synced_in(
                    self.durability,
                    &found.dir.dir,
                    Path::new(files.head()),
                    &head_bytes,
                )
                .map_err(KernelError::from)
            });
            if let Err(error) = written {
                if !self.landed(document, Some(&head_bytes)) {
                    self.restore(document, previous);
                }
                return Err(error);
            }
            Ok(revision)
        })
    }

    fn rewind(
        &self,
        lease: &Lease,
        base: &Revision,
        from: Position,
        commit: &LogCommit,
    ) -> KernelResult<Revision> {
        self.fenced(lease, |document| {
            if let Some(given) = self.repeated(lease, &commit.id, Some(base))? {
                return Ok(given);
            }
            let found = self.at(document, base, Reading::Whole)?;
            let next = found.head.next_position;
            if from >= next {
                return Err(KernelError::invalid_request(format!(
                    "A rewind from {from} starts at or beyond the next position, {next}."
                )));
            }
            let after = after(next, commit.records.len())?;
            let mut records: Vec<StoredRecord> = found
                .records
                .iter()
                .filter(|record| record.position < from)
                .cloned()
                .collect();
            records.extend(stored(commit, next, self.clock.utc()));
            let head = HeadFile {
                writes: Self::next_write(document, found.head.writes)?,
                next_position: after,
                head: commit
                    .head
                    .clone()
                    .unwrap_or_else(|| found.head.head.clone()),
                ..found.head.clone()
            };
            self.rewrite(lease, &commit.id, base, &found, &head, &records)
        })
    }

    fn replace(&self, lease: &Lease, base: &Revision, image: &LogImage) -> KernelResult<Revision> {
        self.fenced(lease, |document| {
            if let Some(given) = self.repeated(lease, &image.id, Some(base))? {
                return Ok(given);
            }
            let found = self.at(document, base, Reading::Whole)?;
            let head = HeadFile {
                format: image.format,
                incarnation: found.head.incarnation,
                writes: Self::next_write(document, found.head.writes)?,
                next_position: image.next_position()?.max(found.head.next_position),
                head: image.head.clone(),
            };
            self.rewrite(lease, &image.id, base, &found, &head, &image.records)
        })
    }

    fn remove(&self, lease: &Lease, base: &Revision, id: &WriteId) -> KernelResult<()> {
        self.fenced(lease, |document| {
            if let Some(None) = self.retried(lease, id, Some(base)) {
                return Ok(());
            }
            let found = self.at(document, base, Reading::Whole)?;
            let files = self.files_of(document.entity())?;
            let changes = [
                Change {
                    name: files.head().to_owned(),
                    source: Some(sha256_hex(&found.head_bytes)),
                    target: None,
                },
                Change {
                    name: files.log().to_owned(),
                    source: found.log_bytes.as_deref().map(sha256_hex),
                    target: None,
                },
            ];
            drop(found);
            self.journalled(lease, id, Some(base), None, &changes, true)?;
            self.stamps.forget(document);
            Ok(())
        })
    }

    fn head(&self, document: &DocumentId) -> KernelResult<Option<StoredHead>> {
        let lock = self.lock(document);
        let _reading = lock.read().unwrap_or_else(PoisonError::into_inner);
        match self.read(document, Reading::Stamped)? {
            Held::Absent => Ok(None),
            Held::Broken(fault) => Err(fault.error(document)),
            Held::Found(found) => Ok(Some(StoredHead {
                format: found.head.format,
                head: found.head.head,
                next: found.head.next_position,
                revision: found.revision,
            })),
        }
    }

    fn records(
        &self,
        document: &DocumentId,
        revision: &Revision,
        from: Position,
        limit: usize,
    ) -> KernelResult<Vec<StoredRecord>> {
        let lock = self.lock(document);
        let _reading = lock.read().unwrap_or_else(PoisonError::into_inner);
        let found = match self.read(document, Reading::Whole)? {
            Held::Broken(fault) => return Err(fault.error(document)),
            Held::Found(found) if &found.revision == revision => found,
            _ => return Err(stale(document, revision)),
        };
        require_limit(limit)?;
        let start = found
            .records
            .partition_point(|record| record.position < from);
        Ok(found.records[start..].iter().take(limit).cloned().collect())
    }

    fn list(&self, entity: &str, after: Option<&str>, limit: usize) -> KernelResult<Vec<Listed>> {
        require_limit(limit)?;
        if !self.files.contains_key(entity) {
            return Ok(Vec::new());
        }
        let mut listed = Vec::new();
        for resource in self.resources(entity)? {
            if after.is_some_and(|after| resource.as_str() <= after) {
                continue;
            }
            let document = DocumentId::new(entity, resource);
            if let Some(state) = self.listed(&document, Reading::Stamped) {
                listed.push(Listed { document, state });
                if listed.len() == limit {
                    break;
                }
            }
        }
        Ok(listed)
    }
}
