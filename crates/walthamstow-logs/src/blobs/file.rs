//! Copyright (c) Scott A Dixon
//!
//! Blobs kept as files people can open: each at
//! `<document directory>/<kind>/<sha256>.<extension>`, reached through the layout's `cap-std`
//! root.

use super::{name, require_limit, BlobKinds, BlobStore};
use crate::fault::BlobFault;
use crate::layout::{internal, DocumentDir, DocumentLayout};
use crate::types::{BlobKind, BlobRef};
use cap_std::fs::{Dir, OpenOptions};
use std::collections::BTreeSet;
use std::fmt;
use std::io::Write;
use std::path::PathBuf;
use std::sync::Arc;
use walthamstow_kernel::durability::{staged_by_write, WriteDurability};
use walthamstow_kernel::hashing::sha256_hex;
use walthamstow_kernel::leases::{Lease, Leases};
use walthamstow_kernel::{DocumentId, KernelError, KernelResult};

/// Blobs kept as files under a document layout, written as durably as the store is told and
/// under the leases it is given.
pub struct FileBlobStore {
    layout: Arc<DocumentLayout>,
    durability: WriteDurability,
    leases: Arc<dyn Leases>,
    kinds: BlobKinds,
}

impl fmt::Debug for FileBlobStore {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("FileBlobStore")
            .field("root", &self.layout.root())
            .field("durability", &self.durability)
            .field("kinds", &self.kinds)
            .finish_non_exhaustive()
    }
}

/// The name of the file holding the blob of `kind` whose digest is `sha256`.
fn file_name(kind: &BlobKind, sha256: &str) -> String {
    format!("{sha256}.{}", kind.extension())
}

/// The digest a blob file of `kind` named `file` holds, or `None` when the name is not one this
/// store writes.
fn digest_of(kind: &BlobKind, file: &str) -> Option<String> {
    let digest = file.strip_suffix(kind.extension())?.strip_suffix('.')?;
    BlobRef::new(kind.name(), digest)
        .ok()
        .map(|blob| blob.sha256().to_owned())
}

/// A kind's directory within a document, open, and where it lies under the root.
struct KindDir {
    dir: Dir,
    relative: PathBuf,
}

impl FileBlobStore {
    /// Blobs of `kinds` under `layout`, written as durably as `durability` asks and under
    /// `leases`. A kind whose name matches a file the log store names in a document, compared
    /// case-folded, is refused as an invalid request.
    pub fn new(
        layout: Arc<DocumentLayout>,
        durability: WriteDurability,
        leases: Arc<dyn Leases>,
        kinds: BlobKinds,
    ) -> KernelResult<Self> {
        for kind in kinds.iter() {
            if let Some(file) = layout
                .files()
                .find(|file| file.to_lowercase() == kind.name())
            {
                return Err(KernelError::invalid_request(format!(
                    "The blob kind {:?} matches the file {file:?} a log document holds, compared \
                     case-folded.",
                    kind.name()
                )));
            }
        }
        Ok(Self {
            layout,
            durability,
            leases,
            kinds,
        })
    }

    /// The directory of `kind`'s blobs in `document`, or `None` when it does not exist.
    fn kind_dir(&self, document: &DocumentDir, kind: &BlobKind) -> KernelResult<Option<KindDir>> {
        let relative = document.relative.join(kind.name());
        match document.dir.open_dir(kind.name()) {
            Ok(dir) => Ok(Some(KindDir { dir, relative })),
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(None),
            Err(error) => Err(internal(&self.layout.root().join(relative), error)),
        }
    }

    /// The bytes of the regular file `file` in `dir`, or `None` when there is none. Anything
    /// else there is refused unopened.
    fn read(&self, dir: &KindDir, file: &str) -> KernelResult<Option<Vec<u8>>> {
        let path = || self.layout.root().join(&dir.relative).join(file);
        match dir.dir.metadata(file) {
            Ok(metadata) if metadata.is_file() => Ok(Some(
                dir.dir
                    .read(file)
                    .map_err(|error| internal(&path(), error))?,
            )),
            Ok(_) => Err(internal(
                &path(),
                "a blob is a regular file; this is not opened",
            )),
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(None),
            Err(error) => Err(internal(&path(), error)),
        }
    }

    /// The bytes `blob` names in `document`, or the fault that stands in their place.
    fn fetch(
        &self,
        document: &DocumentId,
        blob: &BlobRef,
    ) -> KernelResult<Result<Vec<u8>, BlobFault>> {
        let kind = self.kinds.get(blob.kind())?;
        let missing = || Ok(Err(BlobFault::Missing(blob.clone())));
        let Some(document) = self.layout.document_dir(document)? else {
            return missing();
        };
        let Some(dir) = self.kind_dir(&document, kind)? else {
            return missing();
        };
        let Some(bytes) = self.read(&dir, &file_name(kind, blob.sha256()))? else {
            return missing();
        };
        if sha256_hex(&bytes) != blob.sha256() {
            return Ok(Err(BlobFault::Corrupt(blob.clone())));
        }
        Ok(Ok(bytes))
    }

    /// Writes `bytes` to `file` in `dir` by a rename, durable as the store asks once it returns:
    /// a staged file beside it, synced, renamed into place, and the directory synced.
    fn write(&self, dir: &KindDir, file: &str, bytes: &[u8]) -> KernelResult<()> {
        let path = |name: &str| self.layout.root().join(&dir.relative).join(name);
        let stem = file.rsplit_once('.').map_or(file, |(stem, _)| stem);
        let staged = format!("{stem}.{}.tmp", uuid::Uuid::new_v4());
        let written = (|| -> std::io::Result<()> {
            let mut options = OpenOptions::new();
            options.write(true).create_new(true);
            let mut opened = dir.dir.open_with(&staged, &options)?.into_std();
            opened.write_all(bytes)?;
            self.durability.sync(&opened)?;
            drop(opened);
            dir.dir.rename(&staged, &dir.dir, file)
        })();
        if let Err(error) = written {
            let _ = dir.dir.remove_file(&staged);
            return Err(internal(&path(file), error));
        }
        self.layout.sync(self.durability, &dir.relative)
    }

    /// Every blob of `kind` in `document`, in digest order. An entry the store did not write is
    /// refused unopened; a file a write staged and did not finish is passed over.
    fn blobs_of(&self, document: &DocumentDir, kind: &BlobKind) -> KernelResult<Vec<BlobRef>> {
        let Some(dir) = self.kind_dir(document, kind)? else {
            return Ok(Vec::new());
        };
        let directory = self.layout.root().join(&dir.relative);
        let mut blobs = Vec::new();
        for entry in dir
            .dir
            .entries()
            .map_err(|error| internal(&directory, error))?
        {
            let entry = entry.map_err(|error| internal(&directory, error))?;
            let regular = entry
                .file_type()
                .map_err(|error| internal(&directory, error))?
                .is_file();
            let file = entry.file_name().into_string().ok().filter(|_| regular);
            let Some(file) = file else {
                return Err(internal(
                    &directory.join(entry.file_name()),
                    "this store did not write this entry",
                ));
            };
            if staged_by_write(&file) {
                continue;
            }
            let Some(digest) = digest_of(kind, &file) else {
                return Err(internal(
                    &directory.join(file),
                    "this store did not write this entry",
                ));
            };
            blobs.push(BlobRef::new(kind.name(), digest).expect("a kind and a digest"));
        }
        blobs.sort();
        Ok(blobs)
    }
}

impl BlobStore for FileBlobStore {
    fn put(&self, lease: &Lease, kind: &str, bytes: &[u8]) -> KernelResult<BlobRef> {
        let kind = self.kinds.get(kind)?.clone();
        let blob = name(&kind, bytes);
        let file = file_name(&kind, blob.sha256());
        self.leases.as_ref().fenced(lease, || {
            let document = self
                .layout
                .create_document_dir(self.durability, lease.document())?;
            let dir = KindDir {
                dir: self
                    .layout
                    .create_dir(self.durability, &document.relative, kind.name())?,
                relative: document.relative.join(kind.name()),
            };
            if self.read(&dir, &file)?.is_some_and(|held| held == bytes) {
                // A put syncs its file before the rename, so a put that renamed the file and
                // failed left only the directory unsynced. It is settled here, so a put that
                // succeeds has left a durable blob.
                return self.layout.sync(self.durability, &dir.relative);
            }
            self.write(&dir, &file, bytes)
        })?;
        Ok(blob)
    }

    fn get(&self, document: &DocumentId, blob: &BlobRef) -> KernelResult<Option<Vec<u8>>> {
        match self.fetch(document, blob)? {
            Ok(bytes) => Ok(Some(bytes)),
            Err(BlobFault::Missing(_)) => Ok(None),
            Err(fault) => Err(fault.error(document)),
        }
    }

    fn list(
        &self,
        document: &DocumentId,
        after: Option<&BlobRef>,
        limit: usize,
    ) -> KernelResult<Vec<BlobRef>> {
        require_limit(limit)?;
        let Some(document) = self.layout.document_dir(document)? else {
            return Ok(Vec::new());
        };
        let mut blobs = Vec::new();
        for kind in self.kinds.iter() {
            blobs.extend(self.blobs_of(&document, kind)?);
        }
        Ok(blobs
            .into_iter()
            .filter(|blob| after.is_none_or(|after| blob > after))
            .take(limit)
            .collect())
    }

    fn check(&self, document: &DocumentId, blobs: &[BlobRef]) -> KernelResult<Vec<BlobFault>> {
        let mut faults = Vec::new();
        for blob in blobs {
            if let Err(fault) = self.fetch(document, blob)? {
                faults.push(fault);
            }
        }
        Ok(faults)
    }

    fn remove(&self, lease: &Lease, blobs: &[BlobRef]) -> KernelResult<()> {
        for blob in blobs {
            self.kinds.get(blob.kind())?;
        }
        self.leases.as_ref().fenced(lease, || {
            let Some(document) = self.layout.document_dir(lease.document())? else {
                return Ok(());
            };
            // Every named blob's directory is synced, present or not: a removal that unlinked
            // the file and failed before the sync is settled when it is repeated.
            let mut directories = BTreeSet::new();
            for blob in blobs {
                let kind = self.kinds.get(blob.kind())?;
                let Some(dir) = self.kind_dir(&document, kind)? else {
                    continue;
                };
                let file = file_name(kind, blob.sha256());
                match dir.dir.remove_file(&file) {
                    Ok(()) => {}
                    Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
                    Err(error) => {
                        return Err(internal(
                            &self.layout.root().join(&dir.relative).join(file),
                            error,
                        ))
                    }
                }
                directories.insert(dir.relative);
            }
            for directory in directories {
                self.layout.sync(self.durability, &directory)?;
            }
            Ok(())
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::conformance::blobs::{kinds, BlobBreaker, BlobHarness};
    use crate::layout::{entity_directory_name, resource_directory_name};
    use walthamstow_kernel::clock::Moment;
    use walthamstow_kernel::leases::{LeaseHolder, MemoryLeases};

    /// A store over a scratch root, which the harness holds while the test runs.
    struct FileHarness {
        root: tempfile::TempDir,
    }

    impl FileHarness {
        fn new() -> Self {
            Self {
                root: tempfile::tempdir().unwrap(),
            }
        }

        fn layout(&self) -> Arc<DocumentLayout> {
            Arc::new(
                DocumentLayout::open(self.root.path())
                    .unwrap()
                    .with_entity("Log", ["head.json", "log.ndjson"])
                    .unwrap(),
            )
        }

        /// The file the suite's layout keeps `blob` of `document` in.
        fn path(&self, document: &DocumentId, blob: &BlobRef) -> PathBuf {
            let extension = match blob.kind() {
                "capture" => "png",
                "text" => "txt",
                other => panic!("the suite registers no kind {other}"),
            };
            self.root
                .path()
                .join(entity_directory_name(document.entity()).unwrap())
                .join(resource_directory_name(document.resource()).unwrap())
                .join(blob.kind())
                .join(format!("{}.{extension}", blob.sha256()))
        }
    }

    impl BlobHarness for FileHarness {
        fn store(&self, leases: Arc<dyn Leases>, kinds: BlobKinds) -> Box<dyn BlobStore> {
            Box::new(
                FileBlobStore::new(self.layout(), WriteDurability::System, leases, kinds).unwrap(),
            )
        }
    }

    impl BlobBreaker for FileHarness {
        fn corrupt(&self, document: &DocumentId, blob: &BlobRef) {
            std::fs::write(self.path(document, blob), b"corrupt").unwrap();
        }

        fn delete(&self, document: &DocumentId, blob: &BlobRef) {
            std::fs::remove_file(self.path(document, blob)).unwrap();
        }
    }

    crate::blob_store_conformance!(harness = FileHarness::new());
    crate::blob_store_conformance!(broken = FileHarness::new());

    fn document(resource: &str) -> DocumentId {
        DocumentId::new("Log", resource)
    }

    /// Leases on a clock that never moves.
    fn leases() -> Arc<MemoryLeases> {
        Arc::new(MemoryLeases::new(
            std::time::Duration::from_secs(60),
            || Moment::ORIGIN,
        ))
    }

    /// A store and a lease over `document`.
    fn open(harness: &FileHarness, document: &DocumentId) -> (Box<dyn BlobStore>, Lease) {
        let leases = leases();
        let lease = leases
            .acquire(document, &LeaseHolder::new("test"))
            .unwrap()
            .unwrap();
        (harness.store(leases, kinds()), lease)
    }

    #[test]
    fn a_blob_is_a_file_people_can_open_under_the_encoded_resource() {
        let harness = FileHarness::new();
        let document = document("Turn 1");
        let (store, lease) = open(&harness, &document);
        let blob = store.put(&lease, "capture", b"pixels").unwrap();
        let path = harness.path(&document, &blob);
        assert_eq!(std::fs::read(&path).unwrap(), b"pixels");
        assert!(path.ends_with(format!(
            "log/%54urn%201/capture/{}.png",
            sha256_hex(b"pixels")
        )));
    }

    #[test]
    fn a_kind_matching_a_file_the_log_store_names_is_refused() {
        let root = tempfile::tempdir().unwrap();
        let layout = Arc::new(
            DocumentLayout::open(root.path())
                .unwrap()
                .with_entity("Log", ["Capture"])
                .unwrap(),
        );
        assert!(FileBlobStore::new(layout, WriteDurability::System, leases(), kinds()).is_err());
    }

    #[test]
    fn two_spellings_of_a_resource_are_two_documents() {
        let harness = FileHarness::new();
        let (upper, lower) = (document("Turn"), document("turn"));
        let leases = leases();
        let store = harness.store(leases.clone(), kinds());
        let lease = |document| {
            leases
                .acquire(document, &LeaseHolder::new("test"))
                .unwrap()
                .unwrap()
        };
        let blob = store.put(&lease(&upper), "capture", b"one").unwrap();
        assert_eq!(store.list(&lower, None, usize::MAX).unwrap(), []);
        assert_eq!(store.get(&lower, &blob).unwrap(), None);
        store
            .remove(&lease(&lower), std::slice::from_ref(&blob))
            .unwrap();
        assert_eq!(
            store.list(&upper, None, usize::MAX).unwrap(),
            std::slice::from_ref(&blob)
        );
    }

    #[test]
    fn an_entry_the_store_did_not_write_is_refused_and_a_staged_file_is_passed_over() {
        let harness = FileHarness::new();
        let document = document("turn-1");
        let (store, lease) = open(&harness, &document);
        let blob = store.put(&lease, "capture", b"one").unwrap();
        let directory = harness.path(&document, &blob);
        let directory = directory.parent().unwrap();
        let staged = directory.join(format!("{}.{}.tmp", blob.sha256(), uuid::Uuid::new_v4()));
        std::fs::write(&staged, b"half").unwrap();
        assert_eq!(
            store.list(&document, None, usize::MAX).unwrap(),
            std::slice::from_ref(&blob)
        );
        std::fs::write(directory.join("notes.png"), b"stray").unwrap();
        assert!(store.list(&document, None, usize::MAX).is_err());
        assert_eq!(
            store.get(&document, &blob).unwrap().as_deref(),
            Some(&b"one"[..])
        );
    }

    #[cfg(unix)]
    #[test]
    fn a_link_leaving_the_root_is_not_followed() {
        let harness = FileHarness::new();
        let document = document("turn-1");
        let (store, lease) = open(&harness, &document);
        store.put(&lease, "capture", b"one").unwrap();
        let elsewhere = tempfile::tempdir().unwrap();
        let outside = elsewhere.path().join("elsewhere.png");
        std::fs::write(&outside, b"elsewhere").unwrap();
        let linked = BlobRef::new("capture", sha256_hex(b"elsewhere")).unwrap();
        std::os::unix::fs::symlink(&outside, harness.path(&document, &linked)).unwrap();
        assert!(store.get(&document, &linked).is_err());
        assert!(store
            .check(&document, std::slice::from_ref(&linked))
            .is_err());
        assert!(store.list(&document, None, usize::MAX).is_err());
        assert!(store.put(&lease, "capture", b"elsewhere").is_err());
        assert_eq!(std::fs::read(&outside).unwrap(), b"elsewhere");

        let text = harness
            .path(&document, &linked)
            .parent()
            .unwrap()
            .with_file_name("text");
        std::os::unix::fs::symlink(elsewhere.path(), &text).unwrap();
        assert!(store.put(&lease, "text", b"one").is_err());
        assert!(store
            .get(
                &document,
                &BlobRef::new("text", sha256_hex(b"one")).unwrap()
            )
            .is_err());
    }
}
