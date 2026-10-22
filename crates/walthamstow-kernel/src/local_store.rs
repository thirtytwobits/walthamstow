//! Copyright (c) Scott A Dixon
//!
//! The engine's local records: its journals and the state it keeps beside them. A record is
//! bytes under a key. A write replaces or removes a record only over the record its writer
//! read, named by its tag, so no write is lost to another.
//!
//! [`FileLocalStore`] keeps each record as a file under the data directory's journals.
//! [`MemoryLocalStore`] keeps them in memory, for an engine with no disk and for tests.

use crate::durability::WriteDurability;
use crate::hashing::sha256_hex;
use crate::journals::Journal;
use crate::{KernelError, KernelResult};
use std::collections::{BTreeMap, HashMap};
use std::fmt;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex, PoisonError, Weak};

/// The extension of the file a write stages before it renames it into place. No record name
/// ends with it.
const STAGED_SUFFIX: &str = ".tmp";

/// Where a record is kept: a journal, and a path of names within it. A key with no names is
/// the journal itself, which lists records but holds none.
#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct JournalKey {
    journal: Journal,
    names: Vec<String>,
}

impl JournalKey {
    /// The journal itself, under which its records are listed.
    pub fn journal(journal: Journal) -> Self {
        Self {
            journal,
            names: Vec::new(),
        }
    }

    /// The key `names` names within `journal`. Each name is one plain path component in ASCII:
    /// not empty, not hidden, holding no separator, not a device name Windows reserves, and not
    /// ending in `.tmp`.
    pub fn new<I>(journal: Journal, names: I) -> KernelResult<Self>
    where
        I: IntoIterator,
        I::Item: Into<String>,
    {
        let mut key = Self::journal(journal);
        for name in names {
            key = key.join(name)?;
        }
        Ok(key)
    }

    /// This key with `name` beneath it.
    pub fn join(&self, name: impl Into<String>) -> KernelResult<Self> {
        let name = name.into();
        if !name.is_ascii()
            || crate::files::safe_id(&name).is_err()
            || crate::files::is_windows_device_name(&name)
            || name.ends_with(STAGED_SUFFIX)
        {
            return Err(KernelError::invalid_request(format!(
                "\"{name}\" cannot name a journal record."
            )));
        }
        let mut names = self.names.clone();
        names.push(name);
        Ok(Self {
            journal: self.journal,
            names,
        })
    }

    /// The journal the key is in.
    pub fn journal_of(&self) -> Journal {
        self.journal
    }

    /// The names within the journal, outermost first.
    pub fn names(&self) -> &[String] {
        &self.names
    }

    /// Whether this key is `prefix` or lies beneath it.
    pub fn starts_with(&self, prefix: &JournalKey) -> bool {
        self.journal == prefix.journal && self.names.starts_with(&prefix.names)
    }

    fn require_record(&self) -> KernelResult<()> {
        if self.names.is_empty() {
            return Err(KernelError::invalid_request(format!(
                "The journal {} is not a record.",
                self.journal.name()
            )));
        }
        Ok(())
    }
}

impl fmt::Display for JournalKey {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str(self.journal.name())?;
        for name in &self.names {
            write!(formatter, "/{name}")?;
        }
        Ok(())
    }
}

/// Names the bytes a record held when it was read. A store chooses its tags; a swap or a
/// removal that names a tag acts only on the record it names.
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub struct Tag(String);

impl Tag {
    /// The tag a store gives a record, as the store spells it.
    pub fn new(tag: impl Into<String>) -> Self {
        Self(tag.into())
    }

    pub fn as_str(&self) -> &str {
        &self.0
    }
}

/// The engine's local records. Each operation is atomic, and a successful write is durable
/// when it returns. A write that fails may still have taken effect, so a writer reads the
/// record again before it writes once more. Reading never creates, repairs or upgrades a
/// record.
pub trait LocalStore: Send + Sync + fmt::Debug {
    /// The record at `key`, with the tag a write names to replace it; `None` when there is
    /// none.
    fn get(&self, key: &JournalKey) -> KernelResult<Option<(Vec<u8>, Tag)>>;

    /// Writes `bytes` at `key` only over the record `expected` tags, or only where there is
    /// none when `expected` is `None`, and returns the tag of what it wrote. `None` when the
    /// record is not the one `expected` names.
    fn compare_and_swap(
        &self,
        key: &JournalKey,
        expected: Option<&Tag>,
        bytes: &[u8],
    ) -> KernelResult<Option<Tag>>;

    /// The key of every record at or beneath `prefix`, in key order.
    fn list(&self, prefix: &JournalKey) -> KernelResult<Vec<JournalKey>>;

    /// Removes the record at `key` when `expected` tags it, and returns whether it did.
    fn remove(&self, key: &JournalKey, expected: &Tag) -> KernelResult<bool>;
}

/// The tag both adapters give a record: the SHA-256 of its bytes.
fn digest_tag(bytes: &[u8]) -> Tag {
    Tag(sha256_hex(bytes))
}

/// Records kept in memory.
#[derive(Debug, Default)]
pub struct MemoryLocalStore {
    records: Mutex<BTreeMap<JournalKey, Vec<u8>>>,
}

impl MemoryLocalStore {
    pub fn new() -> Self {
        Self::default()
    }

    fn records(&self) -> std::sync::MutexGuard<'_, BTreeMap<JournalKey, Vec<u8>>> {
        self.records.lock().unwrap_or_else(PoisonError::into_inner)
    }
}

impl LocalStore for MemoryLocalStore {
    fn get(&self, key: &JournalKey) -> KernelResult<Option<(Vec<u8>, Tag)>> {
        key.require_record()?;
        Ok(self
            .records()
            .get(key)
            .map(|bytes| (bytes.clone(), digest_tag(bytes))))
    }

    fn compare_and_swap(
        &self,
        key: &JournalKey,
        expected: Option<&Tag>,
        bytes: &[u8],
    ) -> KernelResult<Option<Tag>> {
        key.require_record()?;
        let mut records = self.records();
        if records.get(key).map(|held| digest_tag(held)).as_ref() != expected {
            return Ok(None);
        }
        records.insert(key.clone(), bytes.to_vec());
        Ok(Some(digest_tag(bytes)))
    }

    fn list(&self, prefix: &JournalKey) -> KernelResult<Vec<JournalKey>> {
        Ok(self
            .records()
            .keys()
            .filter(|key| key.starts_with(prefix))
            .cloned()
            .collect())
    }

    fn remove(&self, key: &JournalKey, expected: &Tag) -> KernelResult<bool> {
        key.require_record()?;
        let mut records = self.records();
        if records.get(key).map(|held| digest_tag(held)).as_ref() != Some(expected) {
            return Ok(false);
        }
        records.remove(key);
        Ok(true)
    }
}

/// The stores open in this process, by their canonical data directory, so every open of one
/// shares its locks.
static OPEN: Mutex<BTreeMap<PathBuf, Weak<FileLocalStore>>> = Mutex::new(BTreeMap::new());

/// Records kept as files under the data directory's journals: the record at
/// `journal/name/…` is the file `journals/journal/name/…`. A write stages a file beside the
/// record and renames it into place under a lock per record, so a reader never sees a part
/// of one and two writers never interleave. Its tag is the SHA-256 of its bytes.
pub struct FileLocalStore {
    data_dir: PathBuf,
    durability: WriteDurability,
    /// One lock per record, held while an operation reads or replaces it, by the key
    /// case-folded, so keys a file system may fold to one file share one lock. Kept while some
    /// operation holds it.
    locks: Mutex<HashMap<String, Weak<Mutex<()>>>>,
}

impl fmt::Debug for FileLocalStore {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("FileLocalStore")
            .field("data_dir", &self.data_dir)
            .field("durability", &self.durability)
            .finish_non_exhaustive()
    }
}

fn internal(path: &Path, error: impl fmt::Display) -> KernelError {
    KernelError::internal(format!("Journal record {}: {error}", path.display()))
}

impl FileLocalStore {
    /// The records under `data_dir`, which must exist. While a store for `data_dir` is open in
    /// this process, opening it again returns that store, which must have been opened with
    /// the same `durability`.
    pub fn open(data_dir: &Path, durability: WriteDurability) -> KernelResult<Arc<Self>> {
        let canonical = data_dir
            .canonicalize()
            .map_err(|error| internal(data_dir, error))?;
        let mut open = OPEN.lock().unwrap_or_else(PoisonError::into_inner);
        open.retain(|_, store| store.strong_count() > 0);
        if let Some(store) = open.get(&canonical).and_then(Weak::upgrade) {
            if store.durability != durability {
                return Err(internal(
                    data_dir,
                    "the store is already open with another durability",
                ));
            }
            return Ok(store);
        }
        let store = Arc::new(Self {
            data_dir: data_dir.to_path_buf(),
            durability,
            locks: Mutex::new(HashMap::new()),
        });
        open.insert(canonical, Arc::downgrade(&store));
        Ok(store)
    }

    /// The directory a key names, each part of it that exists refused when it is not a
    /// directory, so nothing read or written through it leaves the journal.
    fn directory(&self, key: &JournalKey) -> KernelResult<PathBuf> {
        let mut path = key
            .journal
            .unlinked_root(&self.data_dir)
            .map_err(|error| internal(&key.journal.root(&self.data_dir), error))?;
        for name in &key.names {
            match std::fs::symlink_metadata(&path) {
                Ok(metadata) if !metadata.is_dir() => {
                    return Err(internal(
                        &path,
                        "a journal directory is a directory; this is not followed",
                    ))
                }
                Ok(_) => {}
                Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
                Err(error) => return Err(internal(&path, error)),
            }
            path.push(name);
        }
        Ok(path)
    }

    /// The file holding the record at `key`.
    fn path(&self, key: &JournalKey) -> KernelResult<PathBuf> {
        key.require_record()?;
        let path = self.directory(key)?;
        let parent = path.parent().expect("a record lies in its journal");
        match std::fs::symlink_metadata(parent) {
            Ok(metadata) if !metadata.is_dir() => Err(internal(
                parent,
                "a journal directory is a directory; this is not followed",
            )),
            Ok(_) => Ok(path),
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(path),
            Err(error) => Err(internal(parent, error)),
        }
    }

    fn lock(&self, key: &JournalKey) -> Arc<Mutex<()>> {
        let mut locks = self.locks.lock().unwrap_or_else(PoisonError::into_inner);
        locks.retain(|_, lock| lock.strong_count() > 0);
        let folded = key.to_string().to_ascii_lowercase();
        if let Some(lock) = locks.get(&folded).and_then(Weak::upgrade) {
            return lock;
        }
        let lock = Arc::new(Mutex::new(()));
        locks.insert(folded, Arc::downgrade(&lock));
        lock
    }

    /// The bytes of the file at `path`; `None` when there is none. Anything there but a
    /// regular file is refused unopened. What the read finds, a record or none, is settled
    /// first, since a write or a removal that failed may have left it unsynced.
    fn read(&self, path: &Path) -> KernelResult<Option<Vec<u8>>> {
        let bytes = match std::fs::symlink_metadata(path) {
            Ok(metadata) if metadata.file_type().is_file() => {
                Some(std::fs::read(path).map_err(|error| internal(path, error))?)
            }
            Ok(_) => {
                return Err(internal(
                    path,
                    "a record is a regular file; this is not followed or opened",
                ))
            }
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => None,
            Err(error) => return Err(internal(path, error)),
        };
        self.settle(path)?;
        Ok(bytes)
    }

    /// Makes the directory holding `path` durable, when there is one.
    fn settle(&self, path: &Path) -> KernelResult<()> {
        let parent = path.parent().expect("a record lies in its journal");
        match self.durability.sync_directory(parent) {
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(()),
            result => result.map_err(|error| internal(parent, error)),
        }
    }
}

impl LocalStore for FileLocalStore {
    fn get(&self, key: &JournalKey) -> KernelResult<Option<(Vec<u8>, Tag)>> {
        let path = self.path(key)?;
        let lock = self.lock(key);
        let _held = lock.lock().unwrap_or_else(PoisonError::into_inner);
        Ok(self.read(&path)?.map(|bytes| {
            let tag = digest_tag(&bytes);
            (bytes, tag)
        }))
    }

    fn compare_and_swap(
        &self,
        key: &JournalKey,
        expected: Option<&Tag>,
        bytes: &[u8],
    ) -> KernelResult<Option<Tag>> {
        let path = self.path(key)?;
        let lock = self.lock(key);
        let _held = lock.lock().unwrap_or_else(PoisonError::into_inner);
        let held = self.read(&path)?.map(|held| digest_tag(&held));
        if held.as_ref() != expected {
            return Ok(None);
        }
        crate::durability::write_synced(self.durability, &path, bytes)
            .map_err(|error| internal(&path, error))?;
        Ok(Some(digest_tag(bytes)))
    }

    fn list(&self, prefix: &JournalKey) -> KernelResult<Vec<JournalKey>> {
        fn visit(path: &Path, key: &JournalKey, keys: &mut Vec<JournalKey>) -> KernelResult<()> {
            let entries = match std::fs::read_dir(path) {
                Ok(entries) => entries,
                Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(()),
                Err(error) => return Err(internal(path, error)),
            };
            for entry in entries {
                let entry = entry.map_err(|error| internal(path, error))?;
                let entry_path = entry.path();
                let name = entry
                    .file_name()
                    .into_string()
                    .map_err(|_| internal(&entry_path, "a record is named in UTF-8"))?;
                if name.ends_with(STAGED_SUFFIX) {
                    continue;
                }
                let child = key
                    .join(&name)
                    .map_err(|_| internal(&entry_path, "this name cannot name a journal record"))?;
                let file_type = entry
                    .file_type()
                    .map_err(|error| internal(&entry_path, error))?;
                if file_type.is_dir() {
                    visit(&entry_path, &child, keys)?;
                } else if file_type.is_file() {
                    keys.push(child);
                } else {
                    return Err(internal(
                        &entry_path,
                        "a record is a regular file; this is not followed or opened",
                    ));
                }
            }
            Ok(())
        }
        let path = self.directory(prefix)?;
        let mut keys = Vec::new();
        match std::fs::symlink_metadata(&path) {
            Ok(metadata) if metadata.is_dir() => visit(&path, prefix, &mut keys)?,
            Ok(metadata) if metadata.file_type().is_file() && !prefix.names.is_empty() => {
                keys.push(prefix.clone())
            }
            Ok(metadata) if metadata.file_type().is_file() => {
                return Err(internal(
                    &path,
                    "a journal is a directory; this file is not read",
                ))
            }
            Ok(_) => {
                return Err(internal(
                    &path,
                    "a record is a regular file; this is not followed or opened",
                ))
            }
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
            Err(error) => return Err(internal(&path, error)),
        }
        keys.sort();
        Ok(keys)
    }

    fn remove(&self, key: &JournalKey, expected: &Tag) -> KernelResult<bool> {
        let path = self.path(key)?;
        let lock = self.lock(key);
        let _held = lock.lock().unwrap_or_else(PoisonError::into_inner);
        if self.read(&path)?.map(|held| digest_tag(&held)).as_ref() != Some(expected) {
            return Ok(false);
        }
        std::fs::remove_file(&path).map_err(|error| internal(&path, error))?;
        self.settle(&path)?;
        Ok(true)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const JOURNAL: Journal = Journal::new("records");
    const OTHER: Journal = Journal::new("other-records");

    fn key(names: &[&str]) -> JournalKey {
        JournalKey::new(JOURNAL, names.iter().copied()).unwrap()
    }

    /// What every store does, whatever keeps its records.
    fn holds_the_local_store_contract(store: &dyn LocalStore) {
        let record = key(&["operation", "journal.json"]);
        assert_eq!(store.get(&record).unwrap(), None);

        let first = store
            .compare_and_swap(&record, None, b"first")
            .unwrap()
            .expect("a record is written where there is none");
        assert_eq!(
            store.get(&record).unwrap(),
            Some((b"first".to_vec(), first.clone()))
        );
        assert_eq!(
            store.compare_and_swap(&record, None, b"again").unwrap(),
            None,
            "a write expecting no record leaves one in place"
        );

        let stale = Tag::new("not the record's tag");
        assert_eq!(
            store
                .compare_and_swap(&record, Some(&stale), b"lost")
                .unwrap(),
            None
        );
        assert!(!store.remove(&record, &stale).unwrap());
        assert_eq!(store.get(&record).unwrap().unwrap().0, b"first");

        let second = store
            .compare_and_swap(&record, Some(&first), b"second")
            .unwrap()
            .expect("a write naming the record's tag replaces it");
        assert_eq!(
            store
                .compare_and_swap(&record, Some(&first), b"lost")
                .unwrap(),
            None,
            "a tag read before a replacement no longer names the record"
        );

        let sibling = key(&["operation", "staged.json"]);
        let elsewhere = key(&["another"]);
        let other_journal = JournalKey::new(OTHER, ["operation"]).unwrap();
        for written in [&sibling, &elsewhere, &other_journal] {
            store
                .compare_and_swap(written, None, b"held")
                .unwrap()
                .unwrap();
        }
        assert_eq!(
            store.list(&JournalKey::journal(JOURNAL)).unwrap(),
            vec![elsewhere.clone(), record.clone(), sibling.clone()]
        );
        assert_eq!(
            store.list(&key(&["operation"])).unwrap(),
            vec![record.clone(), sibling.clone()]
        );
        assert_eq!(store.list(&key(&["missing"])).unwrap(), Vec::new());

        assert!(!store.remove(&record, &first).unwrap());
        assert!(store.remove(&record, &second).unwrap());
        assert_eq!(store.get(&record).unwrap(), None);
        assert!(!store.remove(&record, &second).unwrap());
        assert_eq!(
            store.list(&key(&["operation"])).unwrap(),
            vec![sibling.clone()]
        );

        let journal = JournalKey::journal(JOURNAL);
        assert!(store.get(&journal).is_err(), "a journal is not a record");
        assert!(store.compare_and_swap(&journal, None, b"no").is_err());
    }

    #[test]
    fn memory_records_hold_the_local_store_contract() {
        holds_the_local_store_contract(&MemoryLocalStore::new());
    }

    #[test]
    fn file_records_hold_the_local_store_contract() {
        let data = tempfile::tempdir().unwrap();
        holds_the_local_store_contract(
            FileLocalStore::open(data.path(), WriteDurability::System)
                .unwrap()
                .as_ref(),
        );
    }

    #[test]
    fn keys_a_file_system_may_fold_to_one_file_share_one_lock_while_it_is_held() {
        let data = tempfile::tempdir().unwrap();
        let store = FileLocalStore::open(data.path(), WriteDurability::System).unwrap();
        let upper = store.lock(&key(&["Record.json"]));
        let lower = store.lock(&key(&["record.json"]));
        assert!(Arc::ptr_eq(&upper, &lower));
        drop((upper, lower));
        let _other = store.lock(&key(&["other.json"]));
        assert_eq!(
            store.locks.lock().unwrap().len(),
            1,
            "a lock no operation holds is let go"
        );
    }

    #[test]
    fn a_file_where_a_journal_lies_is_refused() {
        let data = tempfile::tempdir().unwrap();
        let store = FileLocalStore::open(data.path(), WriteDurability::System).unwrap();
        std::fs::create_dir_all(data.path().join("journals")).unwrap();
        std::fs::write(JOURNAL.root(data.path()), b"not a journal").unwrap();
        assert!(store.list(&JournalKey::journal(JOURNAL)).is_err());
    }

    #[cfg(unix)]
    #[test]
    fn a_record_not_named_in_utf8_is_refused() {
        use std::os::unix::ffi::OsStrExt;
        let data = tempfile::tempdir().unwrap();
        let store = FileLocalStore::open(data.path(), WriteDurability::System).unwrap();
        let root = JOURNAL.root(data.path());
        std::fs::create_dir_all(&root).unwrap();
        let name = std::ffi::OsStr::from_bytes(b"record-\xff.json");
        if std::fs::write(root.join(name), b"held").is_err() {
            // This file system names files in UTF-8 alone.
            return;
        }
        assert!(store.list(&JournalKey::journal(JOURNAL)).is_err());
    }

    #[test]
    fn a_name_that_is_not_one_plain_component_cannot_name_a_record() {
        for name in [
            "",
            "ünïcode.json",
            ".hidden",
            "a/b",
            "a\\b",
            "..",
            "con",
            "record.tmp",
            "record.0b2e.tmp",
        ] {
            assert!(JournalKey::new(JOURNAL, [name]).is_err(), "{name:?}");
        }
        assert!(JournalKey::new(JOURNAL, ["0a1b.json", "operation.json"]).is_ok());
    }

    #[test]
    fn a_file_record_is_the_file_its_key_names_under_the_journals() {
        let data = tempfile::tempdir().unwrap();
        let store = FileLocalStore::open(data.path(), WriteDurability::System).unwrap();
        let record = key(&["operation", "journal.json"]);
        store.compare_and_swap(&record, None, b"held").unwrap();
        let file = JOURNAL.root(data.path()).join("operation/journal.json");
        assert_eq!(std::fs::read(&file).unwrap(), b"held");

        std::fs::write(
            JOURNAL.root(data.path()).join("written.json"),
            b"by an earlier release",
        )
        .unwrap();
        let earlier = key(&["written.json"]);
        assert_eq!(
            store.get(&earlier).unwrap().unwrap().0,
            b"by an earlier release"
        );
    }

    #[test]
    fn a_staged_file_a_write_left_is_not_a_record() {
        let data = tempfile::tempdir().unwrap();
        let store = FileLocalStore::open(data.path(), WriteDurability::System).unwrap();
        let record = key(&["held.json"]);
        store.compare_and_swap(&record, None, b"held").unwrap();
        std::fs::write(
            JOURNAL.root(data.path()).join("held.6f1e.tmp"),
            b"interrupted",
        )
        .unwrap();
        assert_eq!(
            store.list(&JournalKey::journal(JOURNAL)).unwrap(),
            vec![record]
        );
    }

    #[test]
    fn every_open_of_a_data_directory_shares_one_store() {
        let data = tempfile::tempdir().unwrap();
        let first = FileLocalStore::open(data.path(), WriteDurability::System).unwrap();
        let second = FileLocalStore::open(data.path(), WriteDurability::System).unwrap();
        assert!(Arc::ptr_eq(&first, &second));
        assert!(FileLocalStore::open(data.path(), WriteDurability::Device).is_err());
    }

    #[cfg(unix)]
    #[test]
    fn a_link_where_a_record_or_its_directory_lies_is_refused() {
        let outside = tempfile::tempdir().unwrap();
        std::fs::write(outside.path().join("journal.json"), b"outside").unwrap();
        for linked in [
            "journals/records",
            "journals/records/operation",
            "journals/records/operation/journal.json",
        ] {
            let data = tempfile::tempdir().unwrap();
            let store = FileLocalStore::open(data.path(), WriteDurability::System).unwrap();
            let link = data.path().join(linked);
            std::fs::create_dir_all(link.parent().unwrap()).unwrap();
            let target = if linked.ends_with(".json") {
                outside.path().join("journal.json")
            } else {
                outside.path().to_path_buf()
            };
            std::os::unix::fs::symlink(&target, &link).unwrap();
            let record = key(&["operation", "journal.json"]);
            assert!(store.get(&record).is_err(), "{linked}");
            assert!(
                store.compare_and_swap(&record, None, b"written").is_err(),
                "{linked}"
            );
            assert!(
                store.list(&JournalKey::journal(JOURNAL)).is_err(),
                "{linked}"
            );
            assert_eq!(
                std::fs::read(outside.path().join("journal.json")).unwrap(),
                b"outside"
            );
        }
    }
}
