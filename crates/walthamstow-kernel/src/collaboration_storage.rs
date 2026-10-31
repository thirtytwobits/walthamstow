//! Copyright (c) Scott A Dixon
//!
//! The engine's storage for collaboration envelopes, and the peer key its
//! stores derive peer blocks from.
//!
//! Each envelope is one JSON file under its entity's directory. A write goes
//! through a temporary file, a sync and a rename under a per-document lock
//! file, so a reader never sees a partial envelope and two writers never
//! interleave. Each entity's directory names the entity it holds in its
//! `entity` file, so two entities whose names map to one directory are
//! refused rather than mixed. The store's records, its audit stream in
//! `audit.ndjson` and quarantined evidence under `quarantine/`, are kept where
//! the adapter is told to keep them: beside the envelopes unless another place
//! is given.
//!
//! Each operation opens the collaboration root and the records directory as
//! `cap-std` directories, and reaches every file under them through those
//! handles: a path or a link that would leave a root fails. An entity directory and the
//! quarantine directory are opened without following a link, so a link there
//! is refused.

use crate::clock::WallClock;
use crate::durability::WriteDurability;
use crate::hashing::sha256_hex;
use anyhow::{anyhow, Context, Result as AnyhowResult};
use cap_fs_ext::DirExt;
use cap_std::fs::{Dir, Metadata};
use clerkenwell_store::{
    CollaborationAuditQuery, CollaborationAuditRecord, CollaborationDocumentId,
    CollaborationStoragePort, PeerKey, StoreError, StoreResult, StoredEnvelope, ENVELOPE_VERSION,
    PEER_KEY_MIN_BYTES,
};
use fs2::FileExt;
use std::io::{Read, Seek, SeekFrom, Write};
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::{Duration, SystemTime, UNIX_EPOCH};

const AUDIT_FILE: &str = "audit.ndjson";
const AUDIT_LOCK_FILE: &str = "audit.lock";
const QUARANTINE_DIR: &str = "quarantine";
const FORMAT_MARKER_FILE: &str = "envelope-format";
const PEER_KEY_FILE: &str = "peer-key";
const ENTITY_FILE: &str = "entity";
const ENTITY_LOCK_FILE: &str = "entity.lock";

/// How long a file must have gone unchanged before its metadata alone shows
/// any later change: a file timestamp can lag the clock by a timer tick, or by
/// a second on a coarse file system, so a rewrite within that lag can leave
/// the timestamp where it was.
const SETTLED_METADATA_AGE: Duration = Duration::from_secs(2);

/// Envelopes kept as files under one root, with the store's records under
/// another.
#[derive(Debug, Clone)]
pub struct FileCollaborationStorage {
    root: Arc<Root>,
    /// Where the audit stream and quarantined evidence go.
    records: Arc<Root>,
    /// How far each write goes before the store relies on it.
    durability: WriteDurability,
    /// Where evidence names and metadata stamps read the time.
    clock: WallClock,
}

/// A directory the store keeps state under. Each operation opens it as a `cap-std` directory
/// and reaches everything under it through that handle; it is opened afresh each time, since
/// preparing a cache replaces the directory under a store that may outlive it.
#[derive(Debug)]
struct Root {
    path: PathBuf,
}

impl Root {
    fn new(path: &Path) -> Self {
        Self {
            path: path.to_path_buf(),
        }
    }

    /// The root, or `None` while it does not exist.
    fn open(&self) -> StoreResult<Option<Place>> {
        match crate::files::open_dir(&self.path) {
            Ok(dir) => Ok(Some(Place {
                dir,
                path: self.path.clone(),
            })),
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(None),
            Err(error) => Err(inspecting(&self.path, error)),
        }
    }

    /// The root, created with every missing ancestor when it does not exist. It goes through
    /// the kernel's directory creation even when the root exists, which holds out every
    /// creation from its check to its sync, so a root another caller is still settling is
    /// not used before its entry is durable.
    fn create(&self, durability: WriteDurability) -> StoreResult<Place> {
        crate::durability::create_dir_all_synced(durability, &self.path).map_err(|error| {
            StoreError::internal(format!(
                "Could not create collaboration storage directory {}: {error}",
                self.path.display()
            ))
        })?;
        self.open()?.ok_or_else(|| {
            StoreError::internal(format!(
                "The collaboration storage directory {} was removed as it was made.",
                self.path.display()
            ))
        })
    }
}

/// A directory of the store's, open, and where it lies.
struct Place {
    dir: Dir,
    path: PathBuf,
}

fn inspecting(path: &Path, error: impl std::fmt::Display) -> StoreError {
    StoreError::internal(format!(
        "Could not inspect collaboration state {}: {error}",
        path.display()
    ))
}

impl Place {
    fn file(&self, name: &str) -> PathBuf {
        self.path.join(name)
    }

    /// The directory `name` here, or `None` when there is none. A link there is refused
    /// unfollowed.
    fn child(&self, name: &str) -> StoreResult<Option<Place>> {
        let path = self.file(name);
        match self.dir.open_dir_nofollow(name) {
            Ok(dir) => Ok(Some(Place { dir, path })),
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(None),
            Err(error) => Err(StoreError::internal(format!(
                "The collaboration directory {} is not a directory the store opens; a link is \
                 not followed: {error}",
                path.display()
            ))),
        }
    }

    /// The directory `name` here, created with its entry durable when it does not exist. A
    /// link there is refused unfollowed.
    fn create_child(&self, durability: WriteDurability, name: &str) -> StoreResult<Place> {
        crate::durability::create_dir_all_in(durability, &self.dir, Path::new(name)).map_err(
            |error| {
                StoreError::internal(format!(
                    "Could not create collaboration storage directory {}: {error}",
                    self.file(name).display()
                ))
            },
        )?;
        self.child(name)?.ok_or_else(|| {
            StoreError::internal(format!(
                "The collaboration storage directory {} was removed as it was made.",
                self.file(name).display()
            ))
        })
    }

    /// Refuses `name` when something other than a regular file is there: a link, which the
    /// store does not follow, or a special file, which could block the open.
    fn refuse_irregular(&self, name: &str) -> StoreResult<()> {
        match self.dir.symlink_metadata(name) {
            Ok(metadata) if !metadata.is_file() => Err(StoreError::internal(format!(
                "Collaboration state {} is not a regular file; the store does not follow or open it.",
                self.file(name).display()
            ))),
            Ok(_) => Ok(()),
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(()),
            Err(error) => Err(inspecting(&self.file(name), error)),
        }
    }

    /// The bytes of `name`, or `None` when there is no file. Anything but a regular file is
    /// refused.
    fn read_optional(&self, name: &str) -> StoreResult<Option<Vec<u8>>> {
        self.refuse_irregular(name)?;
        match self.dir.read(name) {
            Ok(bytes) => Ok(Some(bytes)),
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(None),
            Err(error) => Err(StoreError::internal(format!(
                "Could not read collaboration state {}: {error}",
                self.file(name).display()
            ))),
        }
    }

    /// Writes `bytes` to `name` durably, by a rename.
    fn write(&self, durability: WriteDurability, name: &str, bytes: &[u8]) -> StoreResult<()> {
        crate::durability::write_synced_in(durability, &self.dir, Path::new(name), bytes).map_err(
            |error| {
                StoreError::internal(format!(
                    "Could not write collaboration state {}: {error}",
                    self.file(name).display()
                ))
            },
        )
    }

    fn remove_file_if_exists(&self, name: &str) -> StoreResult<()> {
        match self.dir.remove_file(name) {
            Ok(()) => Ok(()),
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(()),
            Err(error) => Err(StoreError::internal(format!(
                "Could not remove collaboration state {}: {error}",
                self.file(name).display()
            ))),
        }
    }

    /// Makes this directory's entries durable, recording what `doing` changed there.
    fn sync(&self, durability: WriteDurability, doing: &str) -> StoreResult<()> {
        durability.sync_dir(&self.dir).map_err(|error| {
            StoreError::internal(format!(
                "Could not record {doing} in {}: {error}",
                self.path.display()
            ))
        })
    }

    /// The regular file `name` here, opened to read and write and created when absent.
    fn open_state(&self, name: &str) -> StoreResult<std::fs::File> {
        self.refuse_irregular(name)?;
        let mut options = cap_std::fs::OpenOptions::new();
        options.create(true).truncate(false).read(true).write(true);
        self.dir
            .open_with(name, &options)
            .map(cap_std::fs::File::into_std)
            .map_err(|error| {
                StoreError::internal(format!(
                    "Could not open collaboration state {}: {error}",
                    self.file(name).display()
                ))
            })
    }

    /// Runs `operation` while holding the lock file `name` here, which names `what` it guards.
    fn with_lock<T>(
        &self,
        name: &str,
        what: &str,
        operation: impl FnOnce() -> StoreResult<T>,
    ) -> StoreResult<T> {
        let lock = self.open_state(name)?;
        lock.lock_exclusive()
            .map_err(|error| StoreError::internal(format!("Could not lock {what}: {error}")))?;
        let result = operation();
        let unlocked = FileExt::unlock(&lock)
            .map_err(|error| StoreError::internal(format!("Could not unlock {what}: {error}")));
        match (result, unlocked) {
            (Err(error), _) | (Ok(_), Err(error)) => Err(error),
            (Ok(value), Ok(())) => Ok(value),
        }
    }
}

impl FileCollaborationStorage {
    /// Envelopes under `root`, with the store's records beside them, written as
    /// durably as `durability` asks, reading the time from `clock`. A `root` that is not
    /// UTF-8 is refused, since each envelope's source names its path as text.
    pub fn new(root: &Path, durability: WriteDurability, clock: WallClock) -> AnyhowResult<Self> {
        if root.to_str().is_none() {
            return Err(anyhow!(
                "The collaboration root {} is not UTF-8, so its envelopes cannot be named.",
                root.display()
            ));
        }
        let root = Arc::new(Root::new(root));
        Ok(Self {
            records: root.clone(),
            root,
            durability,
            clock,
        })
    }

    /// The same envelopes, with the store's records kept under `records`
    /// instead: a journal directory, when the envelopes sit among authored
    /// content. A `records` that is not UTF-8 is refused, since evidence is
    /// reported by its path as text.
    pub fn with_records_at(mut self, records: &Path) -> AnyhowResult<Self> {
        if records.to_str().is_none() {
            return Err(anyhow!(
                "The collaboration records {} are not in a UTF-8 directory, so their evidence cannot be named.",
                records.display()
            ));
        }
        self.records = Arc::new(Root::new(records));
        Ok(self)
    }

    /// The name of `entity`'s directory, refused when the store reserves it or Windows does.
    fn entity_name(entity: &str) -> StoreResult<String> {
        let name = safe_entity_name(entity);
        if [QUARANTINE_DIR, FORMAT_MARKER_FILE].contains(&name.as_str()) {
            return Err(StoreError::internal(format!(
                "The entity \"{entity}\" maps to {name}, which the store reserves for itself."
            )));
        }
        if crate::files::is_windows_device_name(&name) {
            return Err(StoreError::internal(format!(
                "The entity \"{entity}\" maps to {name}, a device name Windows reserves."
            )));
        }
        Ok(name)
    }

    /// Refuses `entity` when its directory names another entity.
    fn verify_entity(directory: &Place, entity: &str) -> StoreResult<()> {
        match directory.read_optional(ENTITY_FILE)? {
            Some(held) if held != entity.as_bytes() => Err(StoreError::internal(format!(
                "The collaboration directory {} holds the entity \"{}\", not \"{entity}\".",
                directory.path.display(),
                String::from_utf8_lossy(&held)
            ))),
            _ => Ok(()),
        }
    }

    /// `entity`'s directory, or `None` when there is none, refused when it is a link or names
    /// another entity.
    fn entity_dir(&self, entity: &str) -> StoreResult<Option<Place>> {
        let name = Self::entity_name(entity)?;
        let Some(root) = self.root.open()? else {
            return Ok(None);
        };
        let Some(directory) = root.child(&name)? else {
            return Ok(None);
        };
        Self::verify_entity(&directory, entity)?;
        Ok(Some(directory))
    }

    /// `entity`'s directory, created when absent, with `entity` named as the one it holds
    /// before anything else is written there; refused when it is a link or names another.
    fn claim_entity(&self, entity: &str) -> StoreResult<Place> {
        let name = Self::entity_name(entity)?;
        let directory = self
            .root
            .create(self.durability)?
            .create_child(self.durability, &name)?;
        if directory.read_optional(ENTITY_FILE)?.is_none() {
            directory.with_lock(ENTITY_LOCK_FILE, "the entity directory", || match directory
                .read_optional(ENTITY_FILE)?
            {
                Some(_) => Ok(()),
                None => directory.write(self.durability, ENTITY_FILE, entity.as_bytes()),
            })?;
        }
        Self::verify_entity(&directory, entity)?;
        Ok(directory)
    }

    fn envelope_name(document: &CollaborationDocumentId) -> String {
        format!("{}.json", sha256_hex(document.resource_id.as_bytes()))
    }

    fn lock_name(document: &CollaborationDocumentId) -> String {
        format!("{}.lock", sha256_hex(document.resource_id.as_bytes()))
    }

    /// The entity directory and envelope file `source` names, resolved under the root:
    /// refused unless it names an envelope file in an entity directory directly under the
    /// root, so nothing outside the root is read. The directory is `None` when it does not
    /// exist, and refused when it is a link.
    fn envelope_source<'a>(&self, source: &'a str) -> StoreResult<(Option<Place>, &'a str)> {
        let names = Path::new(source)
            .strip_prefix(&self.root.path)
            .ok()
            .and_then(|relative| {
                let mut names = relative.components().map(|component| match component {
                    std::path::Component::Normal(name) => name.to_str(),
                    _ => None,
                });
                match (names.next(), names.next(), names.next()) {
                    (Some(Some(directory)), Some(Some(file)), None)
                        if is_entity_directory(directory) && is_envelope_file(file) =>
                    {
                        Some((directory, file))
                    }
                    _ => None,
                }
            });
        let (directory, file) = names.ok_or_else(|| {
            StoreError::internal(format!(
                "{source} is not an envelope under {}.",
                self.root.path.display()
            ))
        })?;
        let Some(root) = self.root.open()? else {
            return Ok((None, file));
        };
        Ok((root.child(directory)?, file))
    }

    fn with_audit_lock<T>(
        &self,
        records: &Place,
        operation: impl FnOnce() -> StoreResult<T>,
    ) -> StoreResult<T> {
        records.with_lock(AUDIT_LOCK_FILE, "the audit stream", operation)
    }

    /// Every audit record the stream in `records` holds. A final line without its newline is
    /// an append that did not finish, and is not a record.
    fn audit_records(records: &Place) -> StoreResult<Vec<CollaborationAuditRecord>> {
        let Some(bytes) = records.read_optional(AUDIT_FILE)? else {
            return Ok(Vec::new());
        };
        let complete = bytes
            .iter()
            .rposition(|byte| *byte == b'\n')
            .map_or(&bytes[..0], |end| &bytes[..=end]);
        std::str::from_utf8(complete)
            .map_err(|error| {
                StoreError::internal(format!("Invalid collaboration audit stream: {error}"))
            })?
            .lines()
            .filter(|line| !line.trim().is_empty())
            .map(|line| {
                serde_json::from_str(line).map_err(|error| {
                    StoreError::internal(format!("Invalid collaboration audit record: {error}"))
                })
            })
            .collect()
    }
}

/// Whether `name` is a directory [`safe_entity_name`] gives an entity the store accepts.
fn is_entity_directory(name: &str) -> bool {
    !name.is_empty()
        && name
            .bytes()
            .all(|byte| matches!(byte, b'a'..=b'z' | b'0'..=b'9' | b'-' | b'_'))
        && ![QUARANTINE_DIR, FORMAT_MARKER_FILE].contains(&name)
        && !crate::files::is_windows_device_name(name)
}

/// Whether `name` is an envelope's file: the SHA-256 of a resource id, in lowercase hex, and
/// `.json`.
fn is_envelope_file(name: &str) -> bool {
    name.strip_suffix(".json").is_some_and(|hash| {
        hash.len() == 64
            && hash
                .bytes()
                .all(|byte| matches!(byte, b'0'..=b'9' | b'a'..=b'f'))
    })
}

fn safe_entity_name(entity: &str) -> String {
    entity
        .chars()
        .map(|character| {
            if character.is_ascii_alphanumeric() || character == '-' || character == '_' {
                character.to_ascii_lowercase()
            } else {
                '-'
            }
        })
        .collect()
}

/// Appends `line` to `audit` after its `complete` bytes, syncing it with `sync`, then calls
/// `settle_entry`, so the stream's entry is synced whether the stream is new, was left empty by
/// a first append that did not finish, or was replaced by a discard whose sync failed. An append
/// that fails is truncated away and the truncation synced, so a record whose append returned an
/// error does not come back after a loss of power.
fn append_record(
    audit: &mut std::fs::File,
    complete: u64,
    line: &[u8],
    sync: impl Fn(&std::fs::File) -> std::io::Result<()>,
    settle_entry: impl FnOnce() -> std::io::Result<()>,
) -> std::io::Result<()> {
    let appended = audit
        .set_len(complete)
        .and_then(|()| audit.seek(SeekFrom::Start(complete)))
        .and_then(|_| audit.write_all(line))
        .and_then(|()| sync(audit))
        .and_then(|()| settle_entry());
    let Err(error) = appended else {
        return Ok(());
    };
    match audit.set_len(complete).and_then(|()| sync(audit)) {
        Ok(()) => Err(error),
        Err(undoing) => Err(std::io::Error::other(format!(
            "{error}; undoing the append also failed: {undoing}"
        ))),
    }
}

/// `text` as one path component: every character but an ASCII letter, digit, `-`
/// or `_` becomes `-`.
fn path_component(text: &str) -> String {
    text.chars()
        .map(|character| {
            if character.is_ascii_alphanumeric() || character == '-' || character == '_' {
                character
            } else {
                '-'
            }
        })
        .collect()
}

/// How many of `file`'s bytes end with its last newline.
fn complete_length(file: &mut std::fs::File) -> std::io::Result<u64> {
    const CHUNK: u64 = 4096;
    let mut end = file.seek(SeekFrom::End(0))?;
    let mut buffer = vec![0u8; CHUNK as usize];
    while end > 0 {
        let start = end.saturating_sub(CHUNK);
        let chunk = &mut buffer[..(end - start) as usize];
        file.seek(SeekFrom::Start(start))?;
        file.read_exact(chunk)?;
        if let Some(newline) = chunk.iter().rposition(|byte| *byte == b'\n') {
            return Ok(start + newline as u64 + 1);
        }
        end = start;
    }
    Ok(0)
}

/// When a file last changed, and a stamp of its metadata. The stamp differs
/// from every earlier stamp of the file once its bytes change, provided the
/// earlier one was taken at least [`SETTLED_METADATA_AGE`] after the file last
/// changed.
#[cfg(unix)]
fn metadata_stamp(metadata: &Metadata) -> Option<(SystemTime, String)> {
    use cap_std::fs::MetadataExt;
    let changed = UNIX_EPOCH.checked_add(Duration::new(
        u64::try_from(metadata.ctime()).ok()?,
        u32::try_from(metadata.ctime_nsec()).ok()?,
    ))?;
    let stamp = format!(
        "{}:{}:{}:{}.{}:{}.{}",
        metadata.dev(),
        metadata.ino(),
        metadata.len(),
        metadata.mtime(),
        metadata.mtime_nsec(),
        metadata.ctime(),
        metadata.ctime_nsec(),
    );
    Some((changed, stamp))
}

#[cfg(not(unix))]
fn metadata_stamp(metadata: &Metadata) -> Option<(SystemTime, String)> {
    let modified = metadata.modified().ok()?.into_std();
    let since_epoch = modified.duration_since(UNIX_EPOCH).ok()?;
    Some((
        modified,
        format!("{}:{}", metadata.len(), since_epoch.as_nanos()),
    ))
}

impl CollaborationStoragePort for FileCollaborationStorage {
    fn source(&self, document: &CollaborationDocumentId) -> String {
        self.root
            .path
            .join(safe_entity_name(&document.entity))
            .join(Self::envelope_name(document))
            .display()
            .to_string()
    }

    fn sources(&self, entity: &str) -> StoreResult<Vec<String>> {
        let Some(directory) = self.entity_dir(entity)? else {
            return Ok(Vec::new());
        };
        let listing = |error: std::io::Error| {
            StoreError::internal(format!(
                "Could not list collaboration state {}: {error}",
                directory.path.display()
            ))
        };
        let mut sources = Vec::new();
        for entry in directory.dir.entries().map_err(listing)? {
            let entry = entry.map_err(listing)?;
            let path = directory.path.join(entry.file_name());
            if path.extension().and_then(|value| value.to_str()) != Some("json") {
                continue;
            }
            if !entry.file_type().map_err(listing)?.is_file() {
                return Err(StoreError::internal(format!(
                    "Collaboration state {} is not a file; a link is not followed.",
                    path.display()
                )));
            }
            sources.push(path.display().to_string());
        }
        sources.sort();
        Ok(sources)
    }

    fn read(&self, source: &str) -> StoreResult<Option<StoredEnvelope>> {
        let (Some(directory), file) = self.envelope_source(source)? else {
            return Ok(None);
        };
        Ok(directory.read_optional(file)?.map(|bytes| StoredEnvelope {
            version: sha256_hex(&bytes),
            bytes,
        }))
    }

    fn stamp(&self, source: &str) -> StoreResult<Option<String>> {
        let (Some(directory), file) = self.envelope_source(source)? else {
            return Ok(None);
        };
        let metadata = match directory.dir.symlink_metadata(file) {
            Ok(metadata) => metadata,
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(None),
            Err(error) => return Err(inspecting(&directory.file(file), error)),
        };
        Ok(metadata_stamp(&metadata).and_then(|(changed, stamp)| {
            self.clock
                .now()
                .duration_since(changed)
                .is_ok_and(|age| age >= SETTLED_METADATA_AGE)
                .then_some(stamp)
        }))
    }

    fn compare_and_swap(
        &self,
        document: &CollaborationDocumentId,
        expected: Option<&str>,
        bytes: &[u8],
    ) -> StoreResult<Option<String>> {
        let directory = self.claim_entity(&document.entity)?;
        let name = Self::envelope_name(document);
        let what = format!("{}/{}", document.entity, document.resource_id);
        directory.with_lock(&Self::lock_name(document), &what, || {
            let current = directory.read_optional(&name)?;
            if current.as_deref().map(sha256_hex).as_deref() != expected {
                // What is there may be a write that returned an error after its rename landed;
                // it is settled before the mismatch is reported.
                directory.sync(self.durability, "the envelope")?;
                return Ok(None);
            }
            directory.write(self.durability, &name, bytes)?;
            Ok(Some(sha256_hex(bytes)))
        })
    }

    fn compare_and_remove(
        &self,
        document: &CollaborationDocumentId,
        expected: &str,
    ) -> StoreResult<bool> {
        let Some(directory) = self.entity_dir(&document.entity)? else {
            return Ok(false);
        };
        let name = Self::envelope_name(document);
        let what = format!("{}/{}", document.entity, document.resource_id);
        directory.with_lock(&Self::lock_name(document), &what, || {
            let current = directory.read_optional(&name)?;
            if current.is_none() {
                // An earlier removal may have unlinked it without syncing its directory.
                directory.sync(self.durability, "the removal")?;
                return Ok(false);
            }
            if current.as_deref().map(sha256_hex).as_deref() != Some(expected) {
                return Ok(false);
            }
            directory.remove_file_if_exists(&name)?;
            directory.sync(self.durability, "the removal")?;
            Ok(true)
        })
    }

    fn preserve(
        &self,
        document: &CollaborationDocumentId,
        label: &str,
        bytes: &[u8],
    ) -> StoreResult<String> {
        let quarantine = self
            .records
            .create(self.durability)?
            .create_child(self.durability, QUARANTINE_DIR)?;
        let name = format!(
            "{}-{}-{}-{}-{}.json",
            safe_entity_name(&document.entity),
            sha256_hex(document.resource_id.as_bytes()),
            self.clock.utc().format("%Y%m%dT%H%M%S%.3fZ"),
            path_component(label),
            uuid::Uuid::new_v4().simple()
        );
        quarantine.write(self.durability, &name, bytes)?;
        Ok(quarantine.file(&name).display().to_string())
    }

    fn append_audit(&self, record: &CollaborationAuditRecord) -> StoreResult<()> {
        let records = self.records.create(self.durability)?;
        self.with_audit_lock(&records, || {
            let mut line = serde_json::to_vec(record)
                .map_err(|error| StoreError::internal(error.to_string()))?;
            line.push(b'\n');
            let mut audit = records.open_state(AUDIT_FILE)?;
            // An append that did not finish was never acknowledged: it goes, and so
            // does this one if it does not finish.
            let complete = complete_length(&mut audit).map_err(|error| {
                StoreError::internal(format!(
                    "Could not read the collaboration audit stream: {error}"
                ))
            })?;
            append_record(
                &mut audit,
                complete,
                &line,
                |file| self.durability.sync(file),
                || self.durability.sync_dir(&records.dir),
            )
            .map_err(|error| {
                StoreError::internal(format!(
                    "Could not append to the collaboration audit stream: {error}"
                ))
            })
        })
    }

    fn audit(&self, query: &CollaborationAuditQuery) -> StoreResult<Vec<CollaborationAuditRecord>> {
        // A read makes nothing: no records directory means no record.
        let Some(records) = self.records.open()? else {
            return Ok(Vec::new());
        };
        // Read under the lock appends hold, so an append that fails is never seen.
        Ok(self
            .with_audit_lock(&records, || Self::audit_records(&records))?
            .into_iter()
            .filter(|record| query.selects(record))
            .collect())
    }

    fn discard_audit_before(&self, unix_ms: u128) -> StoreResult<usize> {
        let records = self.records.create(self.durability)?;
        self.with_audit_lock(&records, || {
            let held = Self::audit_records(&records)?;
            let kept: Vec<&CollaborationAuditRecord> = held
                .iter()
                .filter(|record| record.timestamp_unix_ms >= unix_ms)
                .collect();
            let discarded = held.len() - kept.len();
            if discarded == 0 {
                // An earlier discard may have replaced the stream without syncing it.
                if records.dir.symlink_metadata(AUDIT_FILE).is_ok() {
                    records.sync(self.durability, "the audit stream")?;
                }
                return Ok(0);
            }
            let mut bytes = Vec::new();
            for record in kept {
                serde_json::to_writer(&mut bytes, record)
                    .map_err(|error| StoreError::internal(error.to_string()))?;
                bytes.push(b'\n');
            }
            records.write(self.durability, AUDIT_FILE, &bytes)?;
            Ok(discarded)
        })
    }
}

/// Refuses `path`, which is not a regular file or directory the scan reads.
fn bail_link(path: &Path) -> AnyhowResult<()> {
    Err(anyhow!(
        "{} is not a regular file or directory; the scan does not follow or open it.",
        path.display()
    ))
}

/// The format an envelope's bytes declare, when they declare one.
pub fn declared_envelope_format(bytes: &[u8]) -> Option<u32> {
    #[derive(serde::Deserialize)]
    struct Declared {
        envelope_version: u32,
    }
    serde_json::from_slice::<Declared>(bytes)
        .ok()
        .map(|declared| declared.envelope_version)
}

/// The format of the first envelope under `root` this build does not read,
/// or `None` when it reads every envelope there, or there are none. An
/// envelope declaring no format is reported as format 0.
pub fn foreign_envelope_format(root: &Path) -> AnyhowResult<Option<u32>> {
    let dir = match crate::files::open_dir(root) {
        Ok(dir) => dir,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(None),
        Err(error) => return Err(anyhow!("Could not list {}: {error}", root.display())),
    };
    for entity in dir.entries()? {
        let entity = entity?;
        let kind = entity.file_type()?;
        let name = entity.file_name();
        let path = root.join(&name);
        if kind.is_symlink() {
            bail_link(&path)?;
        }
        if !kind.is_dir() || name == QUARANTINE_DIR {
            continue;
        }
        let entity = dir.open_dir_nofollow(&name)?;
        for envelope in entity.entries()? {
            let envelope = envelope?;
            let kind = envelope.file_type()?;
            let file = envelope.file_name();
            if Path::new(&file)
                .extension()
                .and_then(|value| value.to_str())
                != Some("json")
            {
                continue;
            }
            if !kind.is_file() {
                bail_link(&path.join(&file))?;
            }
            let format = declared_envelope_format(&entity.read(&file)?).unwrap_or(0);
            if format != ENVELOPE_VERSION {
                return Ok(Some(format));
            }
        }
    }
    Ok(None)
}

/// Makes `root` a cache of envelopes in this build's format. A root in
/// another format, in none, or holding any envelope in another format is
/// removed whole and made again; what it held is seeded again from the
/// workspace files.
pub fn prepare_cache_root(root: &Path, durability: WriteDurability) -> AnyhowResult<()> {
    if std::fs::symlink_metadata(root).is_ok_and(|metadata| metadata.file_type().is_symlink()) {
        return Err(anyhow!(
            "The collaboration cache {} is a link, which is not followed.",
            root.display()
        ));
    }
    let current = ENVELOPE_VERSION.to_string();
    if root.try_exists()? {
        let dir = crate::files::open_dir(root)?;
        // A marker that is not a regular file marks no format.
        let format = dir
            .symlink_metadata(FORMAT_MARKER_FILE)
            .ok()
            .filter(Metadata::is_file)
            .and_then(|_| dir.read_to_string(FORMAT_MARKER_FILE).ok());
        if format.as_deref().map(str::trim) == Some(current.as_str())
            && foreign_envelope_format(root)?.is_none()
        {
            return Ok(());
        }
        tracing::info!(
            root = %root.display(),
            format = format.as_deref().map(str::trim).unwrap_or("none"),
            "Removing the collaboration cache; this build keeps envelope format {current}."
        );
        drop(dir);
        std::fs::remove_dir_all(root)
            .with_context(|| format!("removing the collaboration cache {}", root.display()))?;
    }
    std::fs::create_dir_all(root)?;
    crate::durability::write_synced_in(
        durability,
        &crate::files::open_dir(root)?,
        Path::new(FORMAT_MARKER_FILE),
        format!("{current}\n").as_bytes(),
    )
    .with_context(|| format!("marking the collaboration cache {}", root.display()))
}

/// The peer key kept under `data_dir`, made on first use. The key names which
/// blocks of peers each principal writes under; a new key leaves every
/// recorded binding as it was and makes unsent drafts unverifiable.
pub fn peer_key(data_dir: &Path, durability: WriteDurability) -> AnyhowResult<PeerKey> {
    let path = data_dir.join(PEER_KEY_FILE);
    crate::durability::create_dir_all_synced(durability, data_dir)?;
    let data = crate::files::open_dir(data_dir)?;
    let secret = match read_secret(&data, &path, durability)? {
        Some(secret) => secret,
        None => {
            let secret = generate_peer_secret();
            match install_secret(durability, &data, &secret) {
                Ok(()) => secret,
                // Another start made the key first; it is the one to read.
                Err(error) if error.kind() == std::io::ErrorKind::AlreadyExists => {
                    read_secret(&data, &path, durability)?.ok_or_else(|| {
                        anyhow!("{} was made and removed while starting.", path.display())
                    })?
                }
                Err(error) => {
                    return Err(error)
                        .with_context(|| format!("writing the peer key {}", path.display()))
                }
            }
        }
    };
    PeerKey::new(&secret).map_err(|error| anyhow!("{}: {}", path.display(), error.message))
}

/// The key kept in the data directory `data`, at `path`, or `None` when there is none. A link
/// or other entry that is not a regular file is refused. A key read back has its directory
/// synced, since the start that installed it may have stopped before syncing its entry.
fn read_secret(
    data: &Dir,
    path: &Path,
    durability: WriteDurability,
) -> AnyhowResult<Option<Vec<u8>>> {
    match data.symlink_metadata(PEER_KEY_FILE) {
        Ok(metadata) if !metadata.is_file() => {
            return Err(anyhow!(
                "{} is not a regular file; the peer key is not read through a link.",
                path.display()
            ))
        }
        Ok(_) => {}
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(None),
        Err(error) => return Err(anyhow!("Could not inspect {}: {error}", path.display())),
    }
    let text = data
        .read_to_string(PEER_KEY_FILE)
        .map_err(|error| anyhow!("Could not read {}: {error}", path.display()))?;
    let secret = decode_hex(text.trim())
        .filter(|bytes| bytes.len() >= PEER_KEY_MIN_BYTES)
        .ok_or_else(|| {
            anyhow!(
                "{} does not hold a peer key: expected at least {PEER_KEY_MIN_BYTES} bytes as hex.",
                path.display()
            )
        })?;
    durability
        .sync_dir(data)
        .with_context(|| format!("syncing the peer key {}", path.display()))?;
    Ok(Some(secret))
}

fn generate_peer_secret() -> Vec<u8> {
    use rand::RngCore;
    let mut secret = vec![0u8; PEER_KEY_MIN_BYTES];
    rand::rng().fill_bytes(&mut secret);
    secret
}

/// Installs `secret` as the peer key in the data directory `data` whole, or not at all: it is
/// written and synced under a temporary name, then linked into place, which fails when a key is
/// already there.
fn install_secret(durability: WriteDurability, data: &Dir, secret: &[u8]) -> std::io::Result<()> {
    let staged = Path::new(PEER_KEY_FILE).with_extension(format!("{}.tmp", uuid::Uuid::new_v4()));
    let installed = write_secret(durability, data, &staged, secret)
        .and_then(|()| data.hard_link(&staged, data, PEER_KEY_FILE));
    let removed = data.remove_file(&staged);
    installed?;
    removed?;
    durability.sync_dir(data)
}

fn write_secret(
    durability: WriteDurability,
    data: &Dir,
    name: &Path,
    secret: &[u8],
) -> std::io::Result<()> {
    let mut options = cap_std::fs::OpenOptions::new();
    options.write(true).create_new(true);
    #[cfg(unix)]
    {
        use cap_std::fs::OpenOptionsExt;
        options.mode(0o600);
    }
    let mut file = data.open_with(name, &options)?.into_std();
    file.write_all(format!("{}\n", encode_hex(secret)).as_bytes())?;
    durability.sync(&file)
}

fn encode_hex(bytes: &[u8]) -> String {
    bytes.iter().map(|byte| format!("{byte:02x}")).collect()
}

fn decode_hex(text: &str) -> Option<Vec<u8>> {
    if !text.len().is_multiple_of(2) {
        return None;
    }
    (0..text.len())
        .step_by(2)
        .map(|index| u8::from_str_radix(text.get(index..index + 2)?, 16).ok())
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;
    use clerkenwell_events::{ActorKind, Principal};
    use clerkenwell_store::{CollaborationAuditEvent, CollaborationRecoveryAction};
    use std::fs::OpenOptions;

    fn document(resource_id: &str) -> CollaborationDocumentId {
        CollaborationDocumentId::new("Note", resource_id)
    }

    /// A clock stopped at one moment.
    fn clock_at(moment: SystemTime) -> WallClock {
        WallClock::new(move || moment)
    }

    fn clock() -> WallClock {
        clock_at(UNIX_EPOCH + Duration::from_secs(1_800_000_000))
    }

    fn record(actor: &str, resource_id: &str, at: u128) -> CollaborationAuditRecord {
        CollaborationAuditRecord {
            actor: Principal::new(actor, ActorKind::Human),
            entity: "Note".into(),
            resource_id: resource_id.into(),
            timestamp_unix_ms: at,
            event: CollaborationAuditEvent::Recovery {
                action: CollaborationRecoveryAction::Export,
                reason_sha256: None,
                destructive: false,
            },
        }
    }

    #[test]
    fn a_swap_replaces_only_the_version_the_writer_read() {
        let root = tempfile::tempdir().unwrap();
        let storage =
            FileCollaborationStorage::new(root.path(), WriteDurability::System, clock()).unwrap();
        let id = document("one");
        let first = storage.compare_and_swap(&id, None, b"first").unwrap();
        assert!(first.is_some(), "a swap against nothing stored writes");
        assert!(
            storage
                .compare_and_swap(&id, None, b"again")
                .unwrap()
                .is_none(),
            "a swap against nothing stored refuses once something is"
        );
        let stale = storage
            .compare_and_swap(&id, Some("not the version"), b"stale")
            .unwrap();
        assert!(stale.is_none());
        let stored = storage.read(&storage.source(&id)).unwrap().unwrap();
        assert_eq!(stored.bytes, b"first");
        assert_eq!(Some(stored.version.as_str()), first.as_deref());
        let second = storage
            .compare_and_swap(&id, first.as_deref(), b"second")
            .unwrap();
        assert!(second.is_some());
        assert_ne!(second, first, "a changed envelope has a new version");
    }

    #[test]
    fn a_remove_takes_only_the_version_the_writer_read() {
        let root = tempfile::tempdir().unwrap();
        let storage =
            FileCollaborationStorage::new(root.path(), WriteDurability::System, clock()).unwrap();
        let id = document("one");
        let version = storage
            .compare_and_swap(&id, None, b"kept")
            .unwrap()
            .unwrap();
        assert!(!storage.compare_and_remove(&id, "moved").unwrap());
        assert!(storage.read(&storage.source(&id)).unwrap().is_some());
        assert!(storage.compare_and_remove(&id, &version).unwrap());
        assert!(storage.read(&storage.source(&id)).unwrap().is_none());
        assert!(storage.sources("Note").unwrap().is_empty());
    }

    #[test]
    fn sources_name_every_stored_envelope_of_an_entity() {
        let root = tempfile::tempdir().unwrap();
        let storage =
            FileCollaborationStorage::new(root.path(), WriteDurability::System, clock()).unwrap();
        for resource_id in ["b", "a", "c"] {
            storage
                .compare_and_swap(&document(resource_id), None, resource_id.as_bytes())
                .unwrap();
        }
        storage
            .compare_and_swap(&CollaborationDocumentId::new("Folder", "w"), None, b"w")
            .unwrap();
        let sources = storage.sources("Note").unwrap();
        assert_eq!(sources.len(), 3);
        let mut sorted = sources.clone();
        sorted.sort();
        assert_eq!(sources, sorted, "sources come in a stable order");
        assert!(sources
            .iter()
            .all(|source| storage.read(source).unwrap().is_some()));
    }

    #[test]
    fn preserved_evidence_is_kept_apart_from_the_envelope() {
        let root = tempfile::tempdir().unwrap();
        let storage =
            FileCollaborationStorage::new(root.path(), WriteDurability::System, clock()).unwrap();
        let id = document("one");
        let place = storage.preserve(&id, "quarantine", b"evidence").unwrap();
        assert_eq!(std::fs::read(&place).unwrap(), b"evidence");
        assert!(storage.read(&storage.source(&id)).unwrap().is_none());
        assert!(storage.sources("Note").unwrap().is_empty());
    }

    #[test]
    fn audit_records_are_kept_in_order_and_selected_by_query() {
        let root = tempfile::tempdir().unwrap();
        let storage =
            FileCollaborationStorage::new(root.path(), WriteDurability::System, clock()).unwrap();
        assert!(storage
            .audit(&CollaborationAuditQuery::default())
            .unwrap()
            .is_empty());
        storage.append_audit(&record("ada", "one", 10)).unwrap();
        storage.append_audit(&record("bob", "two", 20)).unwrap();
        storage.append_audit(&record("ada", "two", 30)).unwrap();
        let all = storage.audit(&CollaborationAuditQuery::default()).unwrap();
        assert_eq!(
            all.iter().map(|r| r.timestamp_unix_ms).collect::<Vec<_>>(),
            vec![10, 20, 30],
            "records are answered oldest first"
        );
        let by_actor = storage
            .audit(&CollaborationAuditQuery {
                actor_id: Some("ada".into()),
                ..Default::default()
            })
            .unwrap();
        assert!(by_actor.iter().all(|r| r.actor.id == "ada") && by_actor.len() == 2);
        let by_document = storage
            .audit(&CollaborationAuditQuery {
                document: Some(document("two")),
                ..Default::default()
            })
            .unwrap();
        assert!(by_document.iter().all(|r| r.resource_id == "two") && by_document.len() == 2);
        let since = storage
            .audit(&CollaborationAuditQuery {
                from_unix_ms: Some(20),
                ..Default::default()
            })
            .unwrap();
        assert!(since.iter().all(|r| r.timestamp_unix_ms >= 20) && since.len() == 2);
    }

    #[test]
    fn discarding_audit_records_keeps_those_from_the_cutoff_on() {
        let root = tempfile::tempdir().unwrap();
        let storage =
            FileCollaborationStorage::new(root.path(), WriteDurability::System, clock()).unwrap();
        storage.append_audit(&record("ada", "one", 10)).unwrap();
        storage.append_audit(&record("ada", "one", 20)).unwrap();
        storage.append_audit(&record("ada", "one", 30)).unwrap();
        assert_eq!(storage.discard_audit_before(20).unwrap(), 1);
        let kept = storage.audit(&CollaborationAuditQuery::default()).unwrap();
        assert!(kept.iter().all(|r| r.timestamp_unix_ms >= 20) && kept.len() == 2);
        assert_eq!(storage.discard_audit_before(20).unwrap(), 0);
        storage.append_audit(&record("ada", "one", 40)).unwrap();
        assert_eq!(
            storage
                .audit(&CollaborationAuditQuery::default())
                .unwrap()
                .len(),
            3,
            "the stream takes records after a discard"
        );
    }

    #[test]
    fn a_cache_root_in_another_format_is_removed_and_one_in_this_format_kept() {
        let data = tempfile::tempdir().unwrap();
        let root = data.path().join("collaboration");
        std::fs::create_dir_all(root.join("note")).unwrap();
        std::fs::write(root.join("note/old.json"), b"{}").unwrap();
        prepare_cache_root(&root, WriteDurability::System).unwrap();
        assert!(
            !root.join("note/old.json").exists(),
            "a root without this build's marker is removed whole"
        );
        let storage =
            FileCollaborationStorage::new(&root, WriteDurability::System, clock()).unwrap();
        let current = format!("{{\"envelope_version\":{ENVELOPE_VERSION}}}");
        storage
            .compare_and_swap(&document("kept"), None, current.as_bytes())
            .unwrap();
        prepare_cache_root(&root, WriteDurability::System).unwrap();
        assert!(
            storage
                .read(&storage.source(&document("kept")))
                .unwrap()
                .is_some(),
            "a root in this build's format keeps its envelopes"
        );
        storage
            .compare_and_swap(&document("older"), None, b"{\"envelope_version\":3}")
            .unwrap();
        prepare_cache_root(&root, WriteDurability::System).unwrap();
        assert!(
            storage
                .read(&storage.source(&document("kept")))
                .unwrap()
                .is_none(),
            "a root holding an envelope in another format is removed whole"
        );
        storage
            .compare_and_swap(&document("kept"), None, current.as_bytes())
            .unwrap();
        std::fs::write(root.join(FORMAT_MARKER_FILE), b"1\n").unwrap();
        prepare_cache_root(&root, WriteDurability::System).unwrap();
        assert!(storage
            .read(&storage.source(&document("kept")))
            .unwrap()
            .is_none());
    }

    #[test]
    fn records_go_where_the_adapter_is_told_and_leave_the_envelopes_alone() {
        let root = tempfile::tempdir().unwrap();
        let records = tempfile::tempdir().unwrap();
        let storage = FileCollaborationStorage::new(root.path(), WriteDurability::System, clock())
            .unwrap()
            .with_records_at(records.path())
            .unwrap();
        storage
            .compare_and_swap(&document("one"), None, b"kept")
            .unwrap();
        let listed = |dir: &Path| -> Vec<String> {
            let mut names: Vec<String> = std::fs::read_dir(dir)
                .unwrap()
                .map(|entry| entry.unwrap().file_name().to_string_lossy().into_owned())
                .collect();
            names.sort();
            names
        };
        let envelopes_before = listed(root.path());
        storage.append_audit(&record("ada", "one", 1)).unwrap();
        storage
            .preserve(&document("one"), "evidence", b"kept")
            .unwrap();
        assert_eq!(
            listed(root.path()),
            envelopes_before,
            "records do not land among the envelopes"
        );
        let kept = listed(records.path());
        assert!(kept.iter().any(|name| name == AUDIT_FILE), "{kept:?}");
        assert!(kept.iter().any(|name| name == QUARANTINE_DIR), "{kept:?}");
        assert_eq!(
            storage
                .audit(&CollaborationAuditQuery::default())
                .unwrap()
                .len(),
            1
        );
    }

    #[test]
    fn a_root_is_foreign_when_any_envelope_is_in_another_format() {
        let root = tempfile::tempdir().unwrap();
        assert_eq!(
            foreign_envelope_format(&root.path().join("absent")).unwrap(),
            None
        );
        let storage =
            FileCollaborationStorage::new(root.path(), WriteDurability::System, clock()).unwrap();
        let current = format!("{{\"envelope_version\":{ENVELOPE_VERSION}}}");
        storage
            .compare_and_swap(&document("current"), None, current.as_bytes())
            .unwrap();
        storage
            .preserve(&document("evidence"), "old", b"{\"envelope_version\":1}")
            .unwrap();
        assert_eq!(
            foreign_envelope_format(root.path()).unwrap(),
            None,
            "evidence under quarantine is not an envelope the store reads"
        );
        storage
            .compare_and_swap(&document("older"), None, b"{\"envelope_version\":3}")
            .unwrap();
        assert_eq!(foreign_envelope_format(root.path()).unwrap(), Some(3));
        storage
            .compare_and_swap(&document("bare"), None, b"{}")
            .unwrap();
        assert!(foreign_envelope_format(root.path()).unwrap().is_some());
    }

    #[test]
    fn the_peer_key_is_made_once_and_read_back_the_same() {
        let data = tempfile::tempdir().unwrap();
        let first = peer_key(data.path(), WriteDurability::System).unwrap();
        let text = std::fs::read_to_string(data.path().join(PEER_KEY_FILE)).unwrap();
        let secret = decode_hex(text.trim()).expect("the key is kept as hex");
        assert!(secret.len() >= PEER_KEY_MIN_BYTES);
        peer_key(data.path(), WriteDurability::System).unwrap();
        assert_eq!(
            std::fs::read_to_string(data.path().join(PEER_KEY_FILE)).unwrap(),
            text,
            "a later start reads the key it finds"
        );
        let elsewhere = tempfile::tempdir().unwrap();
        peer_key(elsewhere.path(), WriteDurability::System).unwrap();
        assert_ne!(
            std::fs::read_to_string(elsewhere.path().join(PEER_KEY_FILE)).unwrap(),
            text,
            "each data directory makes its own key"
        );
        drop(first);
    }

    #[cfg(unix)]
    #[test]
    fn the_peer_key_file_is_readable_by_its_owner_alone() {
        use std::os::unix::fs::PermissionsExt;
        let data = tempfile::tempdir().unwrap();
        peer_key(data.path(), WriteDurability::System).unwrap();
        let mode = std::fs::metadata(data.path().join(PEER_KEY_FILE))
            .unwrap()
            .permissions()
            .mode();
        assert_eq!(mode & 0o077, 0, "mode {mode:o} lets others read the key");
    }

    #[test]
    fn a_malformed_peer_key_file_is_refused() {
        let data = tempfile::tempdir().unwrap();
        std::fs::write(data.path().join(PEER_KEY_FILE), b"not hex\n").unwrap();
        assert!(peer_key(data.path(), WriteDurability::System).is_err());
        std::fs::write(data.path().join(PEER_KEY_FILE), b"abcd\n").unwrap();
        assert!(
            peer_key(data.path(), WriteDurability::System).is_err(),
            "a short key is refused"
        );
    }
    #[test]
    fn a_removal_holds_under_each_durability() {
        for durability in [WriteDurability::Device, WriteDurability::System] {
            let root = tempfile::tempdir().unwrap();
            let storage = FileCollaborationStorage::new(root.path(), durability, clock()).unwrap();
            let id = document("one");
            let version = storage
                .compare_and_swap(&id, None, b"kept")
                .unwrap()
                .unwrap();
            assert!(storage.compare_and_remove(&id, &version).unwrap());
            assert!(storage.read(&storage.source(&id)).unwrap().is_none());
            assert!(storage.sources("Note").unwrap().is_empty());
        }
    }

    #[test]
    fn evidence_preserved_twice_at_one_moment_under_one_label_is_kept_twice() {
        let root = tempfile::tempdir().unwrap();
        let storage =
            FileCollaborationStorage::new(root.path(), WriteDurability::System, clock()).unwrap();
        let id = document("one");
        let first = storage.preserve(&id, "corrupt", b"first").unwrap();
        let second = storage.preserve(&id, "corrupt", b"second").unwrap();
        assert_ne!(first, second);
        assert_eq!(std::fs::read(&first).unwrap(), b"first");
        assert_eq!(std::fs::read(&second).unwrap(), b"second");
    }

    #[test]
    fn a_label_cannot_lead_evidence_out_of_quarantine() {
        let root = tempfile::tempdir().unwrap();
        let storage =
            FileCollaborationStorage::new(root.path(), WriteDurability::System, clock()).unwrap();
        let quarantine = root.path().join(QUARANTINE_DIR);
        for label in ["x/../../../outside", "..", "a\\b", "/absolute"] {
            let place = PathBuf::from(storage.preserve(&document("one"), label, b"kept").unwrap());
            assert_eq!(place.parent(), Some(quarantine.as_path()), "{label:?}");
            assert_eq!(std::fs::read(&place).unwrap(), b"kept");
        }
        let held = std::fs::read_dir(root.path())
            .unwrap()
            .map(|entry| entry.unwrap().file_name())
            .collect::<Vec<_>>();
        assert_eq!(held, [std::ffi::OsString::from(QUARANTINE_DIR)]);
    }

    #[test]
    fn a_torn_audit_line_hides_no_record_and_the_next_append_drops_it() {
        let root = tempfile::tempdir().unwrap();
        let storage =
            FileCollaborationStorage::new(root.path(), WriteDurability::System, clock()).unwrap();
        storage.append_audit(&record("ada", "one", 10)).unwrap();
        storage.append_audit(&record("bob", "two", 20)).unwrap();
        let stream = root.path().join(AUDIT_FILE);
        let mut torn = OpenOptions::new().append(true).open(&stream).unwrap();
        torn.write_all(br#"{"actor":{"id":"cy"#).unwrap();
        drop(torn);

        let read = |storage: &FileCollaborationStorage| {
            storage
                .audit(&CollaborationAuditQuery::default())
                .unwrap()
                .iter()
                .map(|record| record.timestamp_unix_ms)
                .collect::<Vec<_>>()
        };
        assert_eq!(read(&storage), vec![10, 20]);
        storage.append_audit(&record("cy", "three", 30)).unwrap();
        assert_eq!(read(&storage), vec![10, 20, 30]);
        let text = std::fs::read_to_string(&stream).unwrap();
        assert!(text.ends_with('\n'));
        assert!(text
            .lines()
            .all(|line| serde_json::from_str::<CollaborationAuditRecord>(line).is_ok()));
    }

    #[test]
    fn a_new_peer_key_in_a_missing_directory_is_kept_under_each_durability() {
        for durability in [WriteDurability::Device, WriteDurability::System] {
            let data = tempfile::tempdir().unwrap();
            let data_dir = data.path().join("engine/data");
            peer_key(&data_dir, durability).unwrap();
            let text = std::fs::read_to_string(data_dir.join(PEER_KEY_FILE)).unwrap();
            peer_key(&data_dir, durability).unwrap();
            assert_eq!(
                std::fs::read_to_string(data_dir.join(PEER_KEY_FILE)).unwrap(),
                text
            );
        }
    }

    #[test]
    fn two_entities_whose_names_share_a_directory_are_refused() {
        let root = tempfile::tempdir().unwrap();
        let storage =
            FileCollaborationStorage::new(root.path(), WriteDurability::System, clock()).unwrap();
        let kept = storage
            .compare_and_swap(&document("one"), None, b"kept")
            .unwrap()
            .unwrap();
        let other = CollaborationDocumentId::new("note", "one");
        assert!(storage.compare_and_swap(&other, None, b"other").is_err());
        assert!(storage.compare_and_remove(&other, &kept).is_err());
        assert!(storage.sources("note").is_err());
        let stored = storage.read(&storage.source(&document("one"))).unwrap();
        assert_eq!(stored.unwrap().bytes, b"kept");
        assert_eq!(storage.sources("Note").unwrap().len(), 1);
    }

    #[test]
    fn a_directory_that_names_no_entity_is_claimed_by_its_first_writer() {
        let root = tempfile::tempdir().unwrap();
        let storage =
            FileCollaborationStorage::new(root.path(), WriteDurability::System, clock()).unwrap();
        let directory = root.path().join(safe_entity_name("Note"));
        std::fs::create_dir_all(&directory).unwrap();
        assert!(storage.sources("Note").unwrap().is_empty());
        assert!(
            !directory.join(ENTITY_FILE).exists(),
            "a read writes nothing"
        );
        storage
            .compare_and_swap(&document("one"), None, b"kept")
            .unwrap()
            .unwrap();
        assert!(storage
            .compare_and_swap(&CollaborationDocumentId::new("note", "two"), None, b"x")
            .is_err());
    }

    #[test]
    fn a_stamp_is_given_only_once_the_storage_clock_says_the_file_has_settled() {
        let root = tempfile::tempdir().unwrap();
        let id = document("one");
        FileCollaborationStorage::new(root.path(), WriteDurability::System, clock())
            .unwrap()
            .compare_and_swap(&id, None, b"kept")
            .unwrap();
        let before = FileCollaborationStorage::new(
            root.path(),
            WriteDurability::System,
            clock_at(UNIX_EPOCH),
        )
        .unwrap();
        assert!(before.stamp(&before.source(&id)).unwrap().is_none());
        let long_after = FileCollaborationStorage::new(
            root.path(),
            WriteDurability::System,
            clock_at(UNIX_EPOCH + Duration::from_secs(u32::MAX as u64 * 4)),
        )
        .unwrap();
        assert!(long_after.stamp(&long_after.source(&id)).unwrap().is_some());
    }

    #[test]
    fn the_first_audit_record_in_a_missing_directory_is_kept_under_each_durability() {
        for durability in [WriteDurability::Device, WriteDurability::System] {
            let root = tempfile::tempdir().unwrap();
            let records = root.path().join("journals/collaboration");
            let storage = FileCollaborationStorage::new(root.path(), durability, clock())
                .unwrap()
                .with_records_at(&records)
                .unwrap();
            storage.append_audit(&record("ada", "one", 10)).unwrap();
            let kept = storage.audit(&CollaborationAuditQuery::default()).unwrap();
            assert_eq!(kept.len(), 1);
        }
    }

    #[cfg(unix)]
    #[test]
    fn a_linked_entity_directory_is_refused_and_nothing_is_written_through_it() {
        let root = tempfile::tempdir().unwrap();
        let outside = tempfile::tempdir().unwrap();
        std::os::unix::fs::symlink(outside.path(), root.path().join(safe_entity_name("Note")))
            .unwrap();
        let storage =
            FileCollaborationStorage::new(root.path(), WriteDurability::System, clock()).unwrap();
        assert!(storage
            .compare_and_swap(&document("one"), None, b"kept")
            .is_err());
        assert!(storage.compare_and_remove(&document("one"), "any").is_err());
        assert!(storage.sources("Note").is_err());
        assert_eq!(std::fs::read_dir(outside.path()).unwrap().count(), 0);
    }

    #[test]
    fn an_entity_whose_directory_the_store_reserves_is_refused() {
        let root = tempfile::tempdir().unwrap();
        let storage =
            FileCollaborationStorage::new(root.path(), WriteDurability::System, clock()).unwrap();
        storage
            .preserve(&document("one"), "corrupt", b"evidence")
            .unwrap();
        for entity in ["Quarantine", "Envelope-Format", "envelope.format"] {
            let id = CollaborationDocumentId::new(entity, "one");
            assert!(
                storage.compare_and_swap(&id, None, b"x").is_err(),
                "{entity}"
            );
            assert!(storage.sources(entity).is_err(), "{entity}");
        }
        assert_eq!(
            std::fs::read_dir(root.path().join(QUARANTINE_DIR))
                .unwrap()
                .count(),
            1,
            "the evidence is all quarantine holds"
        );
    }

    #[cfg(unix)]
    #[test]
    fn a_linked_envelope_is_neither_listed_nor_read() {
        let root = tempfile::tempdir().unwrap();
        let outside = tempfile::tempdir().unwrap();
        let storage =
            FileCollaborationStorage::new(root.path(), WriteDurability::System, clock()).unwrap();
        storage
            .compare_and_swap(&document("kept"), None, b"kept")
            .unwrap();
        std::fs::write(outside.path().join("foreign.json"), b"{}").unwrap();
        let linked = root
            .path()
            .join(safe_entity_name("Note"))
            .join("linked.json");
        std::os::unix::fs::symlink(outside.path().join("foreign.json"), &linked).unwrap();
        assert!(storage.sources("Note").is_err());
        assert!(storage.read(&linked.display().to_string()).is_err());
    }

    #[test]
    fn a_new_peer_key_appears_whole_and_leaves_nothing_staged() {
        let data = tempfile::tempdir().unwrap();
        let stray = data.path().join("peer-key.interrupted.tmp");
        std::fs::write(&stray, b"ab").unwrap();
        peer_key(data.path(), WriteDurability::System).unwrap();
        let text = std::fs::read_to_string(data.path().join(PEER_KEY_FILE)).unwrap();
        assert!(decode_hex(text.trim()).is_some_and(|key| key.len() >= PEER_KEY_MIN_BYTES));
        let mut held = std::fs::read_dir(data.path())
            .unwrap()
            .map(|entry| entry.unwrap().file_name().to_string_lossy().into_owned())
            .collect::<Vec<_>>();
        held.sort();
        assert_eq!(held, ["peer-key", "peer-key.interrupted.tmp"]);
    }

    #[cfg(unix)]
    #[test]
    fn a_linked_quarantine_directory_receives_no_evidence() {
        let root = tempfile::tempdir().unwrap();
        let outside = tempfile::tempdir().unwrap();
        std::os::unix::fs::symlink(outside.path(), root.path().join(QUARANTINE_DIR)).unwrap();
        let storage =
            FileCollaborationStorage::new(root.path(), WriteDurability::System, clock()).unwrap();
        assert!(storage.preserve(&document("one"), "corrupt", b"x").is_err());
        assert_eq!(std::fs::read_dir(outside.path()).unwrap().count(), 0);
    }

    #[cfg(unix)]
    #[test]
    fn a_linked_audit_stream_or_lock_is_neither_read_nor_written() {
        let root = tempfile::tempdir().unwrap();
        let outside = tempfile::tempdir().unwrap();
        let target = outside.path().join("stream");
        std::fs::write(&target, b"kept\n").unwrap();
        std::os::unix::fs::symlink(&target, root.path().join(AUDIT_FILE)).unwrap();
        let storage =
            FileCollaborationStorage::new(root.path(), WriteDurability::System, clock()).unwrap();
        assert!(storage.append_audit(&record("ada", "one", 10)).is_err());
        assert!(storage.audit(&CollaborationAuditQuery::default()).is_err());
        assert_eq!(std::fs::read(&target).unwrap(), b"kept\n");

        let locked = tempfile::tempdir().unwrap();
        let lock_target = outside.path().join("lock");
        std::os::unix::fs::symlink(&lock_target, locked.path().join(AUDIT_LOCK_FILE)).unwrap();
        let storage =
            FileCollaborationStorage::new(locked.path(), WriteDurability::System, clock()).unwrap();
        assert!(storage.append_audit(&record("ada", "one", 10)).is_err());
        assert!(!lock_target.exists(), "nothing is made through the link");
    }

    #[test]
    fn an_existing_peer_key_is_read_back_under_each_durability() {
        for durability in [WriteDurability::Device, WriteDurability::System] {
            let data = tempfile::tempdir().unwrap();
            peer_key(data.path(), WriteDurability::System).unwrap();
            let text = std::fs::read_to_string(data.path().join(PEER_KEY_FILE)).unwrap();
            peer_key(data.path(), durability).unwrap();
            assert_eq!(
                std::fs::read_to_string(data.path().join(PEER_KEY_FILE)).unwrap(),
                text
            );
        }
    }

    #[cfg(unix)]
    #[test]
    fn a_special_file_where_state_is_kept_is_refused_unopened() {
        let root = tempfile::tempdir().unwrap();
        let storage =
            FileCollaborationStorage::new(root.path(), WriteDurability::System, clock()).unwrap();
        let id = document("one");
        let source = storage.source(&id);
        std::fs::create_dir_all(Path::new(&source).parent().unwrap()).unwrap();
        // A socket's path is limited in length; one bound at a short name is moved into place.
        let short = root.path().join("s");
        let _special = std::os::unix::net::UnixListener::bind(&short).unwrap();
        std::fs::rename(&short, &source).unwrap();
        for error in [
            storage.read(&source).unwrap_err(),
            storage.compare_and_swap(&id, None, b"x").unwrap_err(),
        ] {
            assert!(
                error.message.contains("not a regular file"),
                "{}",
                error.message
            );
        }
    }

    #[test]
    fn a_removal_retried_after_the_envelope_is_gone_settles_under_each_durability() {
        for durability in [WriteDurability::Device, WriteDurability::System] {
            let root = tempfile::tempdir().unwrap();
            let storage = FileCollaborationStorage::new(root.path(), durability, clock()).unwrap();
            let id = document("one");
            let version = storage
                .compare_and_swap(&id, None, b"kept")
                .unwrap()
                .unwrap();
            assert!(storage.compare_and_remove(&id, &version).unwrap());
            assert!(!storage.compare_and_remove(&id, &version).unwrap());
            assert!(storage.read(&storage.source(&id)).unwrap().is_none());
        }
    }

    #[test]
    fn a_stream_left_empty_takes_its_first_record_under_each_durability() {
        for durability in [WriteDurability::Device, WriteDurability::System] {
            let root = tempfile::tempdir().unwrap();
            std::fs::write(root.path().join(AUDIT_FILE), b"").unwrap();
            let storage = FileCollaborationStorage::new(root.path(), durability, clock()).unwrap();
            storage.append_audit(&record("ada", "one", 10)).unwrap();
            let kept = storage.audit(&CollaborationAuditQuery::default()).unwrap();
            assert_eq!(kept.len(), 1);
        }
    }

    #[cfg(unix)]
    #[test]
    fn a_peer_key_behind_a_link_is_refused() {
        let outside = tempfile::tempdir().unwrap();
        let elsewhere = tempfile::tempdir().unwrap();
        peer_key(elsewhere.path(), WriteDurability::System).unwrap();
        for target in [
            outside.path().join("missing"),
            elsewhere.path().join(PEER_KEY_FILE),
        ] {
            let data = tempfile::tempdir().unwrap();
            std::os::unix::fs::symlink(&target, data.path().join(PEER_KEY_FILE)).unwrap();
            assert!(
                peer_key(data.path(), WriteDurability::System).is_err(),
                "{}",
                target.display()
            );
        }
    }

    #[cfg(unix)]
    #[test]
    fn a_format_scan_refuses_what_is_not_a_regular_envelope() {
        let root = tempfile::tempdir().unwrap();
        let outside = tempfile::tempdir().unwrap();
        std::fs::create_dir_all(root.path().join("note")).unwrap();
        std::os::unix::fs::symlink(outside.path(), root.path().join("linked")).unwrap();
        assert!(foreign_envelope_format(root.path()).is_err());
        std::fs::remove_file(root.path().join("linked")).unwrap();
        let short = root.path().join("s");
        let _special = std::os::unix::net::UnixListener::bind(&short).unwrap();
        std::fs::rename(&short, root.path().join("note/special.json")).unwrap();
        assert!(foreign_envelope_format(root.path()).is_err());
    }

    #[test]
    fn a_failed_append_is_truncated_away_and_the_truncation_synced() {
        let root = tempfile::tempdir().unwrap();
        let path = root.path().join(AUDIT_FILE);
        let mut audit = OpenOptions::new()
            .create(true)
            .truncate(false)
            .read(true)
            .write(true)
            .open(&path)
            .unwrap();
        let syncs = std::cell::Cell::new(0);
        let failed = append_record(
            &mut audit,
            0,
            b"{\"record\":1}\n",
            |_| {
                syncs.set(syncs.get() + 1);
                Ok(())
            },
            || Err(std::io::Error::other("the directory sync failed")),
        );
        assert!(failed.is_err());
        assert_eq!(std::fs::metadata(&path).unwrap().len(), 0);
        assert_eq!(
            syncs.get(),
            2,
            "the append and then its truncation are synced"
        );
    }

    #[cfg(unix)]
    #[test]
    fn a_linked_cache_root_is_refused() {
        let data = tempfile::tempdir().unwrap();
        let outside = tempfile::tempdir().unwrap();
        prepare_cache_root(outside.path(), WriteDurability::System).unwrap();
        let root = data.path().join("collaboration");
        std::os::unix::fs::symlink(outside.path(), &root).unwrap();
        assert!(prepare_cache_root(&root, WriteDurability::System).is_err());
    }

    #[test]
    fn an_entity_whose_directory_windows_reserves_is_refused() {
        let root = tempfile::tempdir().unwrap();
        let storage =
            FileCollaborationStorage::new(root.path(), WriteDurability::System, clock()).unwrap();
        for entity in ["Con", "AUX", "Com1", "lpt9"] {
            let id = CollaborationDocumentId::new(entity, "one");
            assert!(
                storage.compare_and_swap(&id, None, b"x").is_err(),
                "{entity}"
            );
            assert!(storage.sources(entity).is_err(), "{entity}");
        }
    }

    #[test]
    fn an_audit_read_of_records_never_kept_makes_nothing() {
        let root = tempfile::tempdir().unwrap();
        let records = root.path().join("records");
        let storage = FileCollaborationStorage::new(root.path(), WriteDurability::System, clock())
            .unwrap()
            .with_records_at(&records)
            .unwrap();
        assert!(storage
            .audit(&CollaborationAuditQuery::default())
            .unwrap()
            .is_empty());
        assert!(!records.exists());
        storage.append_audit(&record("ada", "one", 10)).unwrap();
        assert_eq!(
            storage
                .audit(&CollaborationAuditQuery::default())
                .unwrap()
                .len(),
            1
        );
    }

    #[test]
    fn a_stale_swap_is_refused_even_when_its_bytes_are_in_place() {
        for durability in [WriteDurability::Device, WriteDurability::System] {
            let root = tempfile::tempdir().unwrap();
            let storage = FileCollaborationStorage::new(root.path(), durability, clock()).unwrap();
            let id = document("one");
            let written = storage.compare_and_swap(&id, None, b"landed").unwrap();
            assert!(written.is_some());
            for stale in [None, Some("stale")] {
                assert!(storage
                    .compare_and_swap(&id, stale, b"landed")
                    .unwrap()
                    .is_none());
            }
            assert!(storage
                .compare_and_swap(&id, written.as_deref(), b"next")
                .unwrap()
                .is_some());
        }
    }

    #[test]
    fn a_discard_retried_with_nothing_left_settles_under_each_durability() {
        for durability in [WriteDurability::Device, WriteDurability::System] {
            let root = tempfile::tempdir().unwrap();
            let storage = FileCollaborationStorage::new(root.path(), durability, clock()).unwrap();
            storage.append_audit(&record("ada", "one", 10)).unwrap();
            storage.append_audit(&record("ada", "one", 20)).unwrap();
            assert_eq!(storage.discard_audit_before(20).unwrap(), 1);
            assert_eq!(storage.discard_audit_before(20).unwrap(), 0);
            assert_eq!(
                storage
                    .audit(&CollaborationAuditQuery::default())
                    .unwrap()
                    .len(),
                1
            );
        }
    }

    #[test]
    fn every_append_settles_the_streams_entry() {
        let root = tempfile::tempdir().unwrap();
        let path = root.path().join(AUDIT_FILE);
        let mut audit = OpenOptions::new()
            .create(true)
            .truncate(false)
            .read(true)
            .write(true)
            .open(&path)
            .unwrap();
        let settled = std::cell::Cell::new(0);
        let mut complete = 0;
        for line in [&b"{\"record\":1}\n"[..], b"{\"record\":2}\n"] {
            append_record(
                &mut audit,
                complete,
                line,
                |_| Ok(()),
                || {
                    settled.set(settled.get() + 1);
                    Ok(())
                },
            )
            .unwrap();
            complete += line.len() as u64;
        }
        assert_eq!(
            settled.get(),
            2,
            "a discard replacing the stream may not be settled"
        );
    }

    #[cfg(unix)]
    #[test]
    fn an_envelope_is_not_read_through_a_linked_entity_directory() {
        let root = tempfile::tempdir().unwrap();
        let outside = tempfile::tempdir().unwrap();
        std::fs::write(outside.path().join("envelope.json"), b"outside").unwrap();
        std::os::unix::fs::symlink(outside.path(), root.path().join("note")).unwrap();
        let storage =
            FileCollaborationStorage::new(root.path(), WriteDurability::System, clock()).unwrap();
        let linked = root.path().join("note/envelope.json");
        assert!(storage.read(&linked.display().to_string()).is_err());
        assert!(storage.stamp(&linked.display().to_string()).is_err());
        let elsewhere = outside.path().join("envelope.json");
        assert!(storage.read(&elsewhere.display().to_string()).is_err());
    }

    #[cfg(unix)]
    #[test]
    fn a_root_that_is_not_utf8_is_refused() {
        use std::os::unix::ffi::OsStrExt;
        let root = Path::new(std::ffi::OsStr::from_bytes(b"/collaboration-\xff"));
        assert!(FileCollaborationStorage::new(root, WriteDurability::System, clock()).is_err());
    }

    #[test]
    fn a_source_that_climbs_out_of_the_root_is_refused() {
        let parent = tempfile::tempdir().unwrap();
        let root = parent.path().join("collaboration");
        std::fs::create_dir_all(&root).unwrap();
        std::fs::write(parent.path().join("secret.json"), b"outside").unwrap();
        let storage =
            FileCollaborationStorage::new(&root, WriteDurability::System, clock()).unwrap();
        for source in [
            root.join("../secret.json"),
            root.join("note/../../secret.json"),
        ] {
            let source = source.display().to_string();
            assert!(storage.read(&source).is_err(), "{source}");
            assert!(storage.stamp(&source).is_err(), "{source}");
        }
    }

    #[test]
    fn a_source_that_names_an_envelope_outside_the_root_is_refused() {
        let root = tempfile::tempdir().unwrap();
        let outside = tempfile::tempdir().unwrap();
        let storage =
            FileCollaborationStorage::new(root.path(), WriteDurability::System, clock()).unwrap();
        storage
            .compare_and_swap(&document("inside"), None, b"inside")
            .unwrap();
        let elsewhere =
            FileCollaborationStorage::new(outside.path(), WriteDurability::System, clock())
                .unwrap();
        elsewhere
            .compare_and_swap(&document("outside"), None, b"outside")
            .unwrap();
        let source = elsewhere.source(&document("outside"));
        assert!(std::path::Path::new(&source).is_file());
        assert!(storage.read(&source).is_err());
        assert!(storage.stamp(&source).is_err());
        assert_eq!(
            storage
                .read(&storage.source(&document("inside")))
                .unwrap()
                .unwrap()
                .bytes,
            b"inside"
        );
    }

    #[test]
    fn only_a_source_the_layout_generates_is_read() {
        let root = tempfile::tempdir().unwrap();
        let storage =
            FileCollaborationStorage::new(root.path(), WriteDurability::System, clock()).unwrap();
        let id = document("one");
        storage.compare_and_swap(&id, None, b"envelope").unwrap();
        let source = storage.source(&id);
        assert!(storage.read(&source).unwrap().is_some());
        let note = root.path().join("note");
        std::fs::create_dir_all(root.path().join(QUARANTINE_DIR)).unwrap();
        let hash = sha256_hex(b"one");
        std::fs::write(
            root.path()
                .join(QUARANTINE_DIR)
                .join(format!("{hash}.json")),
            b"evidence",
        )
        .unwrap();
        for crafted in [
            root.path()
                .join(QUARANTINE_DIR)
                .join(format!("{hash}.json")),
            note.join(ENTITY_FILE),
            note.join(format!("{hash}.lock")),
            note.join("not-a-hash.json"),
            note.join(format!("{}.json", hash.to_uppercase())),
        ] {
            let crafted = crafted.display().to_string();
            assert!(storage.read(&crafted).is_err(), "{crafted}");
            assert!(storage.stamp(&crafted).is_err(), "{crafted}");
        }
    }

    #[cfg(unix)]
    #[test]
    fn records_that_are_not_utf8_are_refused() {
        use std::os::unix::ffi::OsStrExt;
        let root = tempfile::tempdir().unwrap();
        let records = Path::new(std::ffi::OsStr::from_bytes(b"/records-\xff"));
        let storage =
            FileCollaborationStorage::new(root.path(), WriteDurability::System, clock()).unwrap();
        assert!(storage.with_records_at(records).is_err());
    }
}
