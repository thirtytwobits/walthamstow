//! Copyright (c) Scott A Dixon
//!
//! The engine's local records: its journals and the state it keeps beside them. A record is
//! bytes under a key. A write replaces or removes a record only over the record its writer
//! read, named by its tag, so no write is lost to another.
//!
//! [`FileLocalStore`] keeps each journal's records as files in one directory it owns, each
//! named by a digest of its key, so a key never becomes a path. [`MemoryLocalStore`] keeps
//! them in memory, for an engine with no disk and for tests.

use crate::durability::WriteDurability;
use crate::hashing::sha256_hex;
use crate::journals::Journal;
use crate::{KernelError, KernelResult};
use std::collections::{BTreeMap, HashMap};
use std::fmt;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex, PoisonError, RwLock, Weak};

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

    /// The key `names` names within `journal`. A name is any text but the empty text.
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
        if name.is_empty() {
            return Err(KernelError::invalid_request(
                "A journal record is not named by empty text.",
            ));
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

/// The extension of a record's file.
const RECORD_SUFFIX: &str = ".record";

/// Records kept as files: each journal's in its own directory, `journals/<journal>/`, which the
/// store owns. A record's file is named by the SHA-256 of its key's names and holds those names
/// as its first line, then the record's bytes. A write stages a file beside the record and
/// renames it into place, so a reader never sees a part of one. A record's tag is the SHA-256
/// of its bytes.
pub struct FileLocalStore {
    /// The canonical data directory.
    data_dir: PathBuf,
    durability: WriteDurability,
    /// One lock per record, by its file's name, held while an operation reads or replaces it.
    /// Kept while some operation holds it.
    locks: Mutex<HashMap<String, Weak<Mutex<()>>>>,
    /// Held shared by every write and removal, and alone by a listing, so a listing sees the
    /// records as they were at one moment.
    listing: RwLock<()>,
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

/// Whether `text` is lowercase hexadecimal of `length` digits.
fn hex(text: &str, length: usize) -> bool {
    text.len() == length
        && text
            .bytes()
            .all(|byte| matches!(byte, b'0'..=b'9' | b'a'..=b'f'))
}

/// Whether `name` is the name of a record's file: a digest and the record suffix.
fn is_record_file(name: &str) -> bool {
    name.strip_suffix(RECORD_SUFFIX)
        .is_some_and(|digest| hex(digest, 64))
}

/// Whether `name` is the name of a file a write staged beside a record.
fn is_staged_file(name: &str) -> bool {
    crate::durability::staged_by_write(name)
        && name
            .split_once('.')
            .is_some_and(|(digest, _)| hex(digest, 64))
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
            data_dir: canonical.clone(),
            durability,
            locks: Mutex::new(HashMap::new()),
            listing: RwLock::new(()),
        });
        open.insert(canonical, Arc::downgrade(&store));
        Ok(store)
    }

    /// The journal's directory, refused when it or `journals/` is a link.
    fn directory(&self, journal: Journal) -> KernelResult<PathBuf> {
        journal
            .unlinked_root(&self.data_dir)
            .map_err(|error| internal(&journal.root(&self.data_dir), error))
    }

    /// The name of the file holding the record at `key`.
    fn file_name(key: &JournalKey) -> String {
        let names = serde_json::to_vec(&key.names).expect("names encode");
        format!("{}{RECORD_SUFFIX}", sha256_hex(&names))
    }

    /// The file holding the record at `key`, and its name.
    fn path(&self, key: &JournalKey) -> KernelResult<(PathBuf, String)> {
        key.require_record()?;
        let name = Self::file_name(key);
        Ok((self.directory(key.journal)?.join(&name), name))
    }

    fn lock(&self, name: &str) -> Arc<Mutex<()>> {
        let mut locks = self.locks.lock().unwrap_or_else(PoisonError::into_inner);
        locks.retain(|_, lock| lock.strong_count() > 0);
        if let Some(lock) = locks.get(name).and_then(Weak::upgrade) {
            return lock;
        }
        let lock = Arc::new(Mutex::new(()));
        locks.insert(name.to_owned(), Arc::downgrade(&lock));
        lock
    }

    fn encode(key: &JournalKey, bytes: &[u8]) -> Vec<u8> {
        let mut file = serde_json::to_vec(&key.names).expect("names encode");
        file.push(b'\n');
        file.extend_from_slice(bytes);
        file
    }

    /// The names and bytes a record's file holds, refused unless its name is the digest of
    /// those names.
    fn decode(path: &Path, name: &str, file: &[u8]) -> KernelResult<(Vec<String>, Vec<u8>)> {
        let line = file
            .iter()
            .position(|byte| *byte == b'\n')
            .ok_or_else(|| internal(path, "a record's file names its key on its first line"))?;
        let names: Vec<String> =
            serde_json::from_slice(&file[..line]).map_err(|error| internal(path, error))?;
        if names.is_empty() || names.iter().any(String::is_empty) {
            return Err(internal(path, "a record's key names it by non-empty names"));
        }
        let named = serde_json::to_vec(&names).expect("names encode");
        if format!("{}{RECORD_SUFFIX}", sha256_hex(&named)) != name {
            return Err(internal(path, "this file holds a record of another key"));
        }
        Ok((names, file[line + 1..].to_vec()))
    }

    /// The bytes of the record at `key`, in the file at `path` named `name`; `None` when there
    /// is none. Anything there but a regular file is refused unopened. What the read finds, a
    /// record or none, is settled first, since a write or a removal that failed may have left
    /// it unsynced.
    fn read(&self, key: &JournalKey, path: &Path, name: &str) -> KernelResult<Option<Vec<u8>>> {
        let file = match std::fs::symlink_metadata(path) {
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
        let Some(file) = file else {
            return Ok(None);
        };
        let (names, bytes) = Self::decode(path, name, &file)?;
        if names != key.names {
            return Err(internal(path, "this file holds a record of another key"));
        }
        Ok(Some(bytes))
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
        let (path, name) = self.path(key)?;
        let lock = self.lock(&name);
        let _held = lock.lock().unwrap_or_else(PoisonError::into_inner);
        Ok(self.read(key, &path, &name)?.map(|bytes| {
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
        let (path, name) = self.path(key)?;
        let _writing = self.listing.read().unwrap_or_else(PoisonError::into_inner);
        let lock = self.lock(&name);
        let _held = lock.lock().unwrap_or_else(PoisonError::into_inner);
        let held = self.read(key, &path, &name)?.map(|held| digest_tag(&held));
        if held.as_ref() != expected {
            return Ok(None);
        }
        crate::durability::write_synced(self.durability, &path, &Self::encode(key, bytes))
            .map_err(|error| internal(&path, error))?;
        Ok(Some(digest_tag(bytes)))
    }

    fn list(&self, prefix: &JournalKey) -> KernelResult<Vec<JournalKey>> {
        let directory = self.directory(prefix.journal)?;
        let _listing = self.listing.write().unwrap_or_else(PoisonError::into_inner);
        let entries = match std::fs::symlink_metadata(&directory) {
            Ok(metadata) if metadata.is_dir() => {
                std::fs::read_dir(&directory).map_err(|error| internal(&directory, error))?
            }
            Ok(_) => {
                return Err(internal(
                    &directory,
                    "a journal is a directory; this is not read",
                ))
            }
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(Vec::new()),
            Err(error) => return Err(internal(&directory, error)),
        };
        let mut keys = Vec::new();
        for entry in entries {
            let entry = entry.map_err(|error| internal(&directory, error))?;
            let path = entry.path();
            let regular = entry
                .file_type()
                .map_err(|error| internal(&path, error))?
                .is_file();
            let name = entry.file_name().into_string().ok().filter(|_| regular);
            let Some(name) = name.filter(|name| is_record_file(name) || is_staged_file(name))
            else {
                return Err(internal(
                    &path,
                    "this store did not write this entry; an earlier release finishes what \
                     it left in its journals",
                ));
            };
            if is_staged_file(&name) {
                continue;
            }
            let file = std::fs::read(&path).map_err(|error| internal(&path, error))?;
            let (names, _) = Self::decode(&path, &name, &file)?;
            let key = JournalKey {
                journal: prefix.journal,
                names,
            };
            if key.starts_with(prefix) {
                keys.push(key);
            }
        }
        // A write or removal that failed may have left what this listing found unsynced.
        self.durability
            .sync_directory(&directory)
            .map_err(|error| internal(&directory, error))?;
        keys.sort();
        Ok(keys)
    }

    fn remove(&self, key: &JournalKey, expected: &Tag) -> KernelResult<bool> {
        let (path, name) = self.path(key)?;
        let _writing = self.listing.read().unwrap_or_else(PoisonError::into_inner);
        let lock = self.lock(&name);
        let _held = lock.lock().unwrap_or_else(PoisonError::into_inner);
        if self
            .read(key, &path, &name)?
            .map(|held| digest_tag(&held))
            .as_ref()
            != Some(expected)
        {
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

    /// The suffix of a file a write stages.
    const STAGED_SUFFIX: &str = ".tmp";

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
    fn any_name_but_the_empty_names_a_record_whatever_a_file_system_allows() {
        assert!(JournalKey::new(JOURNAL, [""]).is_err());
        let data = tempfile::tempdir().unwrap();
        let store = FileLocalStore::open(data.path(), WriteDurability::System).unwrap();
        let long = "n".repeat(300);
        let names = [
            "con.json",
            "FOOBAR~1.JSO",
            "Ünïcode",
            "a/b",
            "../escape",
            ".hidden",
            "record.tmp",
            "trailing. ",
            "what?:*",
            long.as_str(),
        ];
        for name in names {
            store
                .compare_and_swap(&key(&[name]), None, name.as_bytes())
                .unwrap()
                .unwrap();
        }
        for name in names {
            assert_eq!(
                store.get(&key(&[name])).unwrap().unwrap().0,
                name.as_bytes(),
                "{name:?}"
            );
        }
        assert_eq!(
            store.list(&JournalKey::journal(JOURNAL)).unwrap().len(),
            names.len()
        );
        assert!(
            std::fs::read_dir(data.path().join("journals"))
                .unwrap()
                .count()
                == 1,
            "no name escapes the journal's directory"
        );
    }

    #[test]
    fn keys_that_differ_only_in_case_are_different_records() {
        let data = tempfile::tempdir().unwrap();
        let store = FileLocalStore::open(data.path(), WriteDurability::System).unwrap();
        store
            .compare_and_swap(&key(&["Record.json"]), None, b"upper")
            .unwrap()
            .unwrap();
        store
            .compare_and_swap(&key(&["record.json"]), None, b"lower")
            .unwrap()
            .unwrap();
        assert_eq!(
            store.get(&key(&["Record.json"])).unwrap().unwrap().0,
            b"upper"
        );
        assert_eq!(
            store.get(&key(&["record.json"])).unwrap().unwrap().0,
            b"lower"
        );
    }

    #[test]
    fn a_record_is_a_file_named_by_its_keys_digest_that_holds_the_key() {
        let data = tempfile::tempdir().unwrap();
        let store = FileLocalStore::open(data.path(), WriteDurability::System).unwrap();
        store
            .compare_and_swap(&key(&["operation", "journal.json"]), None, b"held")
            .unwrap()
            .unwrap();
        let files = std::fs::read_dir(JOURNAL.root(data.path()))
            .unwrap()
            .map(|entry| entry.unwrap())
            .collect::<Vec<_>>();
        assert_eq!(files.len(), 1);
        let name = files[0].file_name().into_string().unwrap();
        assert!(is_record_file(&name), "{name}");
        let file = std::fs::read(files[0].path()).unwrap();
        assert!(file.starts_with(br#"["operation","journal.json"]"#));
        assert!(file.ends_with(b"held"));
    }

    #[test]
    fn an_entry_this_store_did_not_write_stops_a_listing() {
        let entries = [
            (
                format!("{}.json", "0a1b".repeat(16)),
                br#"{"entity":"Note","resource_id":"x"}"#.to_vec(),
            ),
            ("notes.txt".to_owned(), b"anything".to_vec()),
            (
                format!("{}{RECORD_SUFFIX}", "f".repeat(64)),
                b"[\"another\"]\nheld".to_vec(),
            ),
            (
                format!("{}{RECORD_SUFFIX}", sha256_hex(b"[]")),
                b"[]\nheld".to_vec(),
            ),
            (
                format!("{}{RECORD_SUFFIX}", sha256_hex(br#"["a",""]"#)),
                b"[\"a\",\"\"]\nheld".to_vec(),
            ),
        ];
        for (name, bytes) in entries {
            let data = tempfile::tempdir().unwrap();
            let store = FileLocalStore::open(data.path(), WriteDurability::System).unwrap();
            std::fs::create_dir_all(JOURNAL.root(data.path())).unwrap();
            std::fs::write(JOURNAL.root(data.path()).join(&name), bytes).unwrap();
            assert!(store.list(&JournalKey::journal(JOURNAL)).is_err(), "{name}");
        }
    }

    #[test]
    fn a_staged_file_a_write_left_is_passed_over_and_anything_like_one_refused() {
        let data = tempfile::tempdir().unwrap();
        let store = FileLocalStore::open(data.path(), WriteDurability::System).unwrap();
        let record = key(&["held.json"]);
        store.compare_and_swap(&record, None, b"held").unwrap();
        let staged = format!(
            "{}.{}{STAGED_SUFFIX}",
            "0".repeat(64),
            uuid::Uuid::new_v4().hyphenated()
        );
        let root = JOURNAL.root(data.path());
        std::fs::write(root.join(&staged), b"interrupted").unwrap();
        assert_eq!(
            store.list(&JournalKey::journal(JOURNAL)).unwrap(),
            vec![record]
        );
        std::fs::remove_file(root.join(&staged)).unwrap();
        std::fs::create_dir(root.join(&staged)).unwrap();
        assert!(store.list(&JournalKey::journal(JOURNAL)).is_err());
        std::fs::remove_dir(root.join(&staged)).unwrap();
        for unlike in [
            "00000000-0000-0000-0000-000000000000",
            "0B1E2F3A-4C5D-4E6F-8A9B-0C1D2E3F4A5B",
            "0b1e2f3a4c5d4e6f8a9b0c1d2e3f4a5b",
            "0b1e2f3a-4c5d-1e6f-8a9b-0c1d2e3f4a5b",
        ] {
            let name = format!("{}.{unlike}{STAGED_SUFFIX}", "0".repeat(64));
            std::fs::write(root.join(&name), b"unlike").unwrap();
            assert!(
                store.list(&JournalKey::journal(JOURNAL)).is_err(),
                "{unlike}"
            );
            std::fs::remove_file(root.join(&name)).unwrap();
        }
    }

    #[test]
    fn a_file_where_a_journal_lies_is_refused() {
        let data = tempfile::tempdir().unwrap();
        let store = FileLocalStore::open(data.path(), WriteDurability::System).unwrap();
        std::fs::create_dir_all(data.path().join("journals")).unwrap();
        std::fs::write(JOURNAL.root(data.path()), b"not a journal").unwrap();
        assert!(store.list(&JournalKey::journal(JOURNAL)).is_err());
    }

    #[test]
    fn every_open_of_a_data_directory_shares_one_store_at_its_canonical_path() {
        let data = tempfile::tempdir().unwrap();
        std::fs::create_dir(data.path().join("nested")).unwrap();
        let first = FileLocalStore::open(data.path(), WriteDurability::System).unwrap();
        let second =
            FileLocalStore::open(&data.path().join("nested/.."), WriteDurability::System).unwrap();
        assert!(Arc::ptr_eq(&first, &second));
        assert_eq!(first.data_dir, data.path().canonicalize().unwrap());
        assert!(FileLocalStore::open(data.path(), WriteDurability::Device).is_err());
    }

    #[test]
    fn a_lock_no_operation_holds_is_let_go() {
        let data = tempfile::tempdir().unwrap();
        let store = FileLocalStore::open(data.path(), WriteDurability::System).unwrap();
        drop(store.lock("first"));
        let _held = store.lock("second");
        assert_eq!(store.locks.lock().unwrap().len(), 1);
    }

    #[cfg(unix)]
    #[test]
    fn a_link_where_a_journal_or_a_record_lies_is_refused() {
        let outside = tempfile::tempdir().unwrap();
        let record = key(&["operation", "journal.json"]);
        let name = FileLocalStore::file_name(&record);
        for linked in ["journals", "journals/records", "record"] {
            let data = tempfile::tempdir().unwrap();
            let store = FileLocalStore::open(data.path(), WriteDurability::System).unwrap();
            let link = match linked {
                "record" => JOURNAL.root(data.path()).join(&name),
                linked => data.path().join(linked),
            };
            std::fs::create_dir_all(link.parent().unwrap()).unwrap();
            let target = outside.path().join("target");
            std::fs::write(&target, b"outside").unwrap();
            std::os::unix::fs::symlink(
                if linked == "record" {
                    target.clone()
                } else {
                    outside.path().to_path_buf()
                },
                &link,
            )
            .unwrap();
            assert!(store.get(&record).is_err(), "{linked}");
            assert!(
                store.compare_and_swap(&record, None, b"written").is_err(),
                "{linked}"
            );
            assert!(
                store.list(&JournalKey::journal(JOURNAL)).is_err(),
                "{linked}"
            );
            assert_eq!(std::fs::read(&target).unwrap(), b"outside");
        }
    }

    #[cfg(unix)]
    #[test]
    fn an_entry_not_named_in_utf8_is_refused() {
        use std::os::unix::ffi::OsStrExt;
        let data = tempfile::tempdir().unwrap();
        let store = FileLocalStore::open(data.path(), WriteDurability::System).unwrap();
        let root = JOURNAL.root(data.path());
        std::fs::create_dir_all(&root).unwrap();
        let name = std::ffi::OsStr::from_bytes(b"record-\xff.record");
        if std::fs::write(root.join(name), b"held").is_err() {
            // This file system names files in UTF-8 alone.
            return;
        }
        assert!(store.list(&JournalKey::journal(JOURNAL)).is_err());
    }
}
