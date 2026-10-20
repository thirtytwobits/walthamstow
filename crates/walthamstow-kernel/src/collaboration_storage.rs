//! Copyright (c) Scott A Dixon
//!
//! The engine's storage for collaboration envelopes, and the peer key its
//! stores derive peer blocks from.
//!
//! Each envelope is one JSON file under its entity's directory. A write goes
//! through a temporary file, a sync and a rename under a per-document lock
//! file, so a reader never sees a partial envelope and two writers never
//! interleave. The store's records, its audit stream in `audit.ndjson` and
//! quarantined evidence under `quarantine/`, are kept where the adapter is
//! told to keep them: beside the envelopes unless another place is given.

use crate::durability::WriteDurability;
use crate::hashing::sha256_hex;
use anyhow::{anyhow, Context, Result as AnyhowResult};
use clerkenwell_store::{
    CollaborationAuditQuery, CollaborationAuditRecord, CollaborationDocumentId,
    CollaborationStoragePort, PeerKey, StoreError, StoreResult, StoredEnvelope, ENVELOPE_VERSION,
    PEER_KEY_MIN_BYTES,
};
use fs2::FileExt;
use std::fs::OpenOptions;
use std::io::Write;
use std::path::{Path, PathBuf};
use std::time::{Duration, SystemTime, UNIX_EPOCH};

const AUDIT_FILE: &str = "audit.ndjson";
const AUDIT_LOCK_FILE: &str = "audit.lock";
const QUARANTINE_DIR: &str = "quarantine";
const FORMAT_MARKER_FILE: &str = "envelope-format";
const PEER_KEY_FILE: &str = "peer-key";

/// How long a file must have gone unchanged before its metadata alone shows
/// any later change: a file timestamp can lag the clock by a timer tick, or by
/// a second on a coarse file system, so a rewrite within that lag can leave
/// the timestamp where it was.
const SETTLED_METADATA_AGE: Duration = Duration::from_secs(2);

/// Envelopes kept as files under one root, with the store's records under
/// another.
#[derive(Debug, Clone)]
pub struct FileCollaborationStorage {
    root: PathBuf,
    /// Where the audit stream and quarantined evidence go.
    records: PathBuf,
    /// How far each write goes before the store relies on it.
    durability: WriteDurability,
}

impl FileCollaborationStorage {
    /// Envelopes under `root`, with the store's records beside them, written as
    /// durably as `durability` asks.
    pub fn new(root: &Path, durability: WriteDurability) -> Self {
        Self {
            root: root.to_path_buf(),
            records: root.to_path_buf(),
            durability,
        }
    }

    /// The same envelopes, with the store's records kept under `records`
    /// instead: a journal directory, when the envelopes sit among authored
    /// content.
    pub fn with_records_at(mut self, records: &Path) -> Self {
        self.records = records.to_path_buf();
        self
    }

    fn entity_dir(&self, entity: &str) -> PathBuf {
        self.root.join(safe_entity_name(entity))
    }

    fn envelope_path(&self, document: &CollaborationDocumentId) -> PathBuf {
        self.entity_dir(&document.entity).join(format!(
            "{}.json",
            sha256_hex(document.resource_id.as_bytes())
        ))
    }

    fn lock_path(&self, document: &CollaborationDocumentId) -> PathBuf {
        self.entity_dir(&document.entity).join(format!(
            "{}.lock",
            sha256_hex(document.resource_id.as_bytes())
        ))
    }

    fn with_document_lock<T>(
        &self,
        document: &CollaborationDocumentId,
        operation: impl FnOnce() -> StoreResult<T>,
    ) -> StoreResult<T> {
        with_lock(
            &self.lock_path(document),
            &format!("{}/{}", document.entity, document.resource_id),
            operation,
        )
    }

    fn with_audit_lock<T>(&self, operation: impl FnOnce() -> StoreResult<T>) -> StoreResult<T> {
        with_lock(
            &self.records.join(AUDIT_LOCK_FILE),
            "the audit stream",
            operation,
        )
    }

    fn audit_records(&self) -> StoreResult<Vec<CollaborationAuditRecord>> {
        match std::fs::read_to_string(self.records.join(AUDIT_FILE)) {
            Ok(contents) => contents
                .lines()
                .filter(|line| !line.trim().is_empty())
                .map(|line| {
                    serde_json::from_str(line).map_err(|error| {
                        StoreError::internal(format!("Invalid collaboration audit record: {error}"))
                    })
                })
                .collect(),
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(Vec::new()),
            Err(error) => Err(StoreError::internal(format!(
                "Could not read the collaboration audit stream: {error}"
            ))),
        }
    }
}

fn with_lock<T>(
    lock_path: &Path,
    what: &str,
    operation: impl FnOnce() -> StoreResult<T>,
) -> StoreResult<T> {
    if let Some(parent) = lock_path.parent() {
        std::fs::create_dir_all(parent).map_err(|error| {
            StoreError::internal(format!(
                "Could not create collaboration storage directory {}: {error}",
                parent.display()
            ))
        })?;
    }
    let lock = OpenOptions::new()
        .create(true)
        .truncate(false)
        .read(true)
        .write(true)
        .open(lock_path)
        .map_err(|error| {
            StoreError::internal(format!(
                "Could not open collaboration lock {}: {error}",
                lock_path.display()
            ))
        })?;
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

fn write_envelope(durability: WriteDurability, path: &Path, bytes: &[u8]) -> StoreResult<()> {
    crate::durability::write_synced(durability, path, bytes).map_err(|error| {
        StoreError::internal(format!(
            "Could not write collaboration state {}: {error}",
            path.display()
        ))
    })
}

/// The bytes at `path`, or `None` when there is no file.
fn read_optional(path: &Path) -> StoreResult<Option<Vec<u8>>> {
    match std::fs::read(path) {
        Ok(bytes) => Ok(Some(bytes)),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(None),
        Err(error) => Err(StoreError::internal(format!(
            "Could not read collaboration state {}: {error}",
            path.display()
        ))),
    }
}

fn remove_file_if_exists(path: &Path) -> StoreResult<()> {
    match std::fs::remove_file(path) {
        Ok(()) => Ok(()),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(()),
        Err(error) => Err(StoreError::internal(format!(
            "Could not remove collaboration state {}: {error}",
            path.display()
        ))),
    }
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

/// When a file last changed, and a stamp of its metadata. The stamp differs
/// from every earlier stamp of the file once its bytes change, provided the
/// earlier one was taken at least [`SETTLED_METADATA_AGE`] after the file last
/// changed.
#[cfg(unix)]
fn metadata_stamp(metadata: &std::fs::Metadata) -> Option<(SystemTime, String)> {
    use std::os::unix::fs::MetadataExt;
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
fn metadata_stamp(metadata: &std::fs::Metadata) -> Option<(SystemTime, String)> {
    let modified = metadata.modified().ok()?;
    let since_epoch = modified.duration_since(UNIX_EPOCH).ok()?;
    Some((
        modified,
        format!("{}:{}", metadata.len(), since_epoch.as_nanos()),
    ))
}

impl CollaborationStoragePort for FileCollaborationStorage {
    fn source(&self, document: &CollaborationDocumentId) -> String {
        self.envelope_path(document).display().to_string()
    }

    fn sources(&self, entity: &str) -> StoreResult<Vec<String>> {
        let directory = self.entity_dir(entity);
        let entries = match std::fs::read_dir(&directory) {
            Ok(entries) => entries,
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(Vec::new()),
            Err(error) => {
                return Err(StoreError::internal(format!(
                    "Could not list collaboration state {}: {error}",
                    directory.display()
                )))
            }
        };
        let mut sources = Vec::new();
        for entry in entries {
            let path = entry
                .map_err(|error| {
                    StoreError::internal(format!(
                        "Could not list collaboration state {}: {error}",
                        directory.display()
                    ))
                })?
                .path();
            if path.extension().and_then(|value| value.to_str()) == Some("json") {
                sources.push(path.display().to_string());
            }
        }
        sources.sort();
        Ok(sources)
    }

    fn read(&self, source: &str) -> StoreResult<Option<StoredEnvelope>> {
        Ok(
            read_optional(Path::new(source))?.map(|bytes| StoredEnvelope {
                version: sha256_hex(&bytes),
                bytes,
            }),
        )
    }

    fn stamp(&self, source: &str) -> StoreResult<Option<String>> {
        let metadata = match std::fs::metadata(source) {
            Ok(metadata) => metadata,
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(None),
            Err(error) => {
                return Err(StoreError::internal(format!(
                    "Could not inspect collaboration state {source}: {error}"
                )))
            }
        };
        Ok(metadata_stamp(&metadata).and_then(|(changed, stamp)| {
            SystemTime::now()
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
        let path = self.envelope_path(document);
        self.with_document_lock(document, || {
            let current = read_optional(&path)?;
            if current.as_deref().map(sha256_hex).as_deref() != expected {
                return Ok(None);
            }
            write_envelope(self.durability, &path, bytes)?;
            Ok(Some(sha256_hex(bytes)))
        })
    }

    fn compare_and_remove(
        &self,
        document: &CollaborationDocumentId,
        expected: &str,
    ) -> StoreResult<bool> {
        let path = self.envelope_path(document);
        self.with_document_lock(document, || {
            let current = read_optional(&path)?;
            if current.as_deref().map(sha256_hex).as_deref() != Some(expected) {
                return Ok(false);
            }
            remove_file_if_exists(&path)?;
            Ok(true)
        })
    }

    fn preserve(
        &self,
        document: &CollaborationDocumentId,
        label: &str,
        bytes: &[u8],
    ) -> StoreResult<String> {
        let destination = self.records.join(QUARANTINE_DIR).join(format!(
            "{}-{}-{}-{}.json",
            safe_entity_name(&document.entity),
            sha256_hex(document.resource_id.as_bytes()),
            chrono::Utc::now().format("%Y%m%dT%H%M%S%.3fZ"),
            label
        ));
        write_envelope(self.durability, &destination, bytes)?;
        Ok(destination.display().to_string())
    }

    fn append_audit(&self, record: &CollaborationAuditRecord) -> StoreResult<()> {
        self.with_audit_lock(|| {
            let mut line = serde_json::to_vec(record)
                .map_err(|error| StoreError::internal(error.to_string()))?;
            line.push(b'\n');
            let mut audit = OpenOptions::new()
                .create(true)
                .append(true)
                .open(self.records.join(AUDIT_FILE))
                .map_err(|error| {
                    StoreError::internal(format!(
                        "Could not open the collaboration audit stream: {error}"
                    ))
                })?;
            audit
                .write_all(&line)
                .and_then(|()| self.durability.sync(&audit))
                .map_err(|error| {
                    StoreError::internal(format!(
                        "Could not append to the collaboration audit stream: {error}"
                    ))
                })
        })
    }

    fn audit(&self, query: &CollaborationAuditQuery) -> StoreResult<Vec<CollaborationAuditRecord>> {
        Ok(self
            .audit_records()?
            .into_iter()
            .filter(|record| query.selects(record))
            .collect())
    }

    fn discard_audit_before(&self, unix_ms: u128) -> StoreResult<usize> {
        self.with_audit_lock(|| {
            let records = self.audit_records()?;
            let kept: Vec<&CollaborationAuditRecord> = records
                .iter()
                .filter(|record| record.timestamp_unix_ms >= unix_ms)
                .collect();
            let discarded = records.len() - kept.len();
            if discarded == 0 {
                return Ok(0);
            }
            let mut bytes = Vec::new();
            for record in kept {
                serde_json::to_writer(&mut bytes, record)
                    .map_err(|error| StoreError::internal(error.to_string()))?;
                bytes.push(b'\n');
            }
            write_envelope(self.durability, &self.records.join(AUDIT_FILE), &bytes)?;
            Ok(discarded)
        })
    }
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
    let entities = match std::fs::read_dir(root) {
        Ok(entries) => entries,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(None),
        Err(error) => return Err(anyhow!("Could not list {}: {error}", root.display())),
    };
    for entity in entities {
        let entity = entity?.path();
        if !entity.is_dir()
            || entity
                .file_name()
                .is_some_and(|name| name == QUARANTINE_DIR)
        {
            continue;
        }
        for envelope in std::fs::read_dir(&entity)? {
            let envelope = envelope?.path();
            if envelope.extension().and_then(|value| value.to_str()) != Some("json") {
                continue;
            }
            let format = declared_envelope_format(&std::fs::read(&envelope)?).unwrap_or(0);
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
    let marker = root.join(FORMAT_MARKER_FILE);
    let current = ENVELOPE_VERSION.to_string();
    if root.try_exists()? {
        let format = std::fs::read_to_string(&marker).ok();
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
        std::fs::remove_dir_all(root)
            .with_context(|| format!("removing the collaboration cache {}", root.display()))?;
    }
    std::fs::create_dir_all(root)?;
    crate::durability::write_synced(durability, &marker, format!("{current}\n").as_bytes())
        .with_context(|| format!("marking the collaboration cache {}", root.display()))
}

/// The peer key kept under `data_dir`, made on first use. The key names which
/// blocks of peers each principal writes under; a new key leaves every
/// recorded binding as it was and makes unsent drafts unverifiable.
pub fn peer_key(data_dir: &Path, durability: WriteDurability) -> AnyhowResult<PeerKey> {
    let path = data_dir.join(PEER_KEY_FILE);
    let secret = match std::fs::read_to_string(&path) {
        Ok(text) => decode_hex(text.trim())
            .filter(|bytes| bytes.len() >= PEER_KEY_MIN_BYTES)
            .ok_or_else(|| {
                anyhow!(
                    "{} does not hold a peer key: expected at least {PEER_KEY_MIN_BYTES} bytes as hex.",
                    path.display()
                )
            })?,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
            let secret = generate_peer_secret();
            std::fs::create_dir_all(data_dir)?;
            write_secret(durability, &path, &secret)
                .with_context(|| format!("writing the peer key {}", path.display()))?;
            secret
        }
        Err(error) => return Err(anyhow!("Could not read {}: {error}", path.display())),
    };
    PeerKey::new(&secret).map_err(|error| anyhow!("{}: {}", path.display(), error.message))
}

fn generate_peer_secret() -> Vec<u8> {
    use rand::RngCore;
    let mut secret = vec![0u8; PEER_KEY_MIN_BYTES];
    rand::rng().fill_bytes(&mut secret);
    secret
}

fn write_secret(durability: WriteDurability, path: &Path, secret: &[u8]) -> std::io::Result<()> {
    let mut options = OpenOptions::new();
    options.write(true).create_new(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        options.mode(0o600);
    }
    let mut file = options.open(path)?;
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

    fn document(resource_id: &str) -> CollaborationDocumentId {
        CollaborationDocumentId::new("Note", resource_id)
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
        let storage = FileCollaborationStorage::new(root.path(), WriteDurability::System);
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
        let storage = FileCollaborationStorage::new(root.path(), WriteDurability::System);
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
        let storage = FileCollaborationStorage::new(root.path(), WriteDurability::System);
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
        let storage = FileCollaborationStorage::new(root.path(), WriteDurability::System);
        let id = document("one");
        let place = storage.preserve(&id, "quarantine", b"evidence").unwrap();
        assert_eq!(std::fs::read(&place).unwrap(), b"evidence");
        assert!(storage.read(&storage.source(&id)).unwrap().is_none());
        assert!(storage.sources("Note").unwrap().is_empty());
    }

    #[test]
    fn audit_records_are_kept_in_order_and_selected_by_query() {
        let root = tempfile::tempdir().unwrap();
        let storage = FileCollaborationStorage::new(root.path(), WriteDurability::System);
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
        let storage = FileCollaborationStorage::new(root.path(), WriteDurability::System);
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
        let storage = FileCollaborationStorage::new(&root, WriteDurability::System);
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
        let storage = FileCollaborationStorage::new(root.path(), WriteDurability::System)
            .with_records_at(records.path());
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
        let storage = FileCollaborationStorage::new(root.path(), WriteDurability::System);
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
}
