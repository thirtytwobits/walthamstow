//! Copyright (c) Scott A Dixon
//!
//! Blobs kept as files people can open: each at `<document directory>/<kind>/<sha256>.<extension>`.

use super::{name, require_limit, BlobKinds, BlobStore};
use crate::fault::BlobFault;
use crate::layout::{internal, refuse_link, DocumentLayout};
use crate::types::{BlobKind, BlobRef};
use std::collections::BTreeSet;
use std::fmt;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use walthamstow_kernel::durability::{
    create_dir_all_synced, staged_by_write, write_synced, WriteDurability,
};
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

    /// The directory of `kind`'s blobs in `document`, and the file holding `blob`.
    fn path(&self, document: &Path, blob: &BlobRef) -> KernelResult<(PathBuf, PathBuf)> {
        let kind = self.kinds.get(blob.kind())?;
        let directory = document.join(kind.name());
        refuse_link(&directory)?;
        Ok((
            directory.clone(),
            directory.join(file_name(kind, blob.sha256())),
        ))
    }

    /// The bytes of the regular file at `path`, or `None` when there is none. Anything else
    /// there is refused unopened.
    fn read(path: &Path) -> KernelResult<Option<Vec<u8>>> {
        match std::fs::symlink_metadata(path) {
            Ok(metadata) if metadata.file_type().is_file() => Ok(Some(
                std::fs::read(path).map_err(|error| internal(path, error))?,
            )),
            Ok(_) => Err(internal(
                path,
                "a blob is a regular file; this is not followed or opened",
            )),
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(None),
            Err(error) => Err(internal(path, error)),
        }
    }

    /// The bytes `blob` names in `document`, or the fault that stands in their place.
    fn fetch(
        &self,
        document: &DocumentId,
        blob: &BlobRef,
    ) -> KernelResult<Result<Vec<u8>, BlobFault>> {
        let directory = self.layout.document_directory(document)?;
        let (_, path) = self.path(&directory, blob)?;
        let Some(bytes) = Self::read(&path)? else {
            return Ok(Err(BlobFault::Missing(blob.clone())));
        };
        if sha256_hex(&bytes) != blob.sha256() {
            return Ok(Err(BlobFault::Corrupt(blob.clone())));
        }
        Ok(Ok(bytes))
    }

    /// Every blob of `kind` in the document at `document`, in digest order. An entry the store
    /// did not write is refused unopened; a file a write staged and did not finish is passed over.
    fn blobs_of(document: &Path, kind: &BlobKind) -> KernelResult<Vec<BlobRef>> {
        let directory = document.join(kind.name());
        let entries = match std::fs::symlink_metadata(&directory) {
            Ok(metadata) if metadata.is_dir() => {
                std::fs::read_dir(&directory).map_err(|error| internal(&directory, error))?
            }
            Ok(_) => {
                return Err(internal(
                    &directory,
                    "a kind's blobs are a directory; this is not followed or read",
                ))
            }
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(Vec::new()),
            Err(error) => return Err(internal(&directory, error)),
        };
        let mut blobs = Vec::new();
        for entry in entries {
            let entry = entry.map_err(|error| internal(&directory, error))?;
            let path = entry.path();
            let regular = entry
                .file_type()
                .map_err(|error| internal(&path, error))?
                .is_file();
            let file = entry.file_name().into_string().ok().filter(|_| regular);
            let Some(file) = file else {
                return Err(internal(&path, "this store did not write this entry"));
            };
            if staged_by_write(&file) {
                continue;
            }
            let Some(digest) = digest_of(kind, &file) else {
                return Err(internal(&path, "this store did not write this entry"));
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
        self.leases.as_ref().fenced(lease, || {
            let document = self
                .layout
                .create_document_directory(self.durability, lease.document())?;
            let (directory, path) = self.path(&document, &blob)?;
            create_dir_all_synced(self.durability, &directory)
                .map_err(|error| internal(&directory, error))?;
            if Self::read(&path)?.is_some_and(|held| held == bytes) {
                return Ok(());
            }
            write_synced(self.durability, &path, bytes).map_err(|error| internal(&path, error))
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
        let directory = self.layout.document_directory(document)?;
        let mut blobs = Vec::new();
        for kind in self.kinds.iter() {
            blobs.extend(Self::blobs_of(&directory, kind)?);
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
            let document = self.layout.document_directory(lease.document())?;
            let mut directories = BTreeSet::new();
            for blob in blobs {
                let (directory, path) = self.path(&document, blob)?;
                match std::fs::symlink_metadata(&path) {
                    Ok(metadata) if metadata.file_type().is_file() => {
                        std::fs::remove_file(&path).map_err(|error| internal(&path, error))?;
                        directories.insert(directory);
                    }
                    Ok(_) => {
                        return Err(internal(
                            &path,
                            "a blob is a regular file; this is not followed or removed",
                        ))
                    }
                    Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
                    Err(error) => return Err(internal(&path, error)),
                }
            }
            for directory in directories {
                self.durability
                    .sync_directory(&directory)
                    .map_err(|error| internal(&directory, error))?;
            }
            Ok(())
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::conformance::blobs::{kinds, BlobBreaker, BlobHarness};
    use crate::layout::entity_directory_name;
    use walthamstow_kernel::leases::MemoryLeases;

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
                DocumentLayout::new(self.root.path())
                    .with_files("Log", ["head.json", "log.ndjson"]),
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
                .join(document.resource())
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

    /// A store and a lease over `document`, with no lease lapsing.
    fn open(harness: &FileHarness, document: &DocumentId) -> (Box<dyn BlobStore>, Lease) {
        let leases = Arc::new(MemoryLeases::new(
            std::time::Duration::from_secs(60),
            walthamstow_kernel::clock::Moment::now,
        ));
        let lease = leases
            .acquire(
                document,
                &walthamstow_kernel::leases::LeaseHolder::new("test"),
            )
            .unwrap()
            .unwrap();
        (harness.store(leases, kinds()), lease)
    }

    #[test]
    fn a_blob_is_a_file_people_can_open() {
        let harness = FileHarness::new();
        let document = document("turn-1");
        let (store, lease) = open(&harness, &document);
        let blob = store.put(&lease, "capture", b"pixels").unwrap();
        let path = harness.path(&document, &blob);
        assert_eq!(std::fs::read(&path).unwrap(), b"pixels");
        assert!(path.ends_with(format!("log/turn-1/capture/{}.png", sha256_hex(b"pixels"))));
    }

    #[test]
    fn a_kind_matching_a_file_the_log_store_names_is_refused() {
        let root = tempfile::tempdir().unwrap();
        let layout = Arc::new(DocumentLayout::new(root.path()).with_files("Log", ["Capture"]));
        let leases: Arc<dyn Leases> = Arc::new(MemoryLeases::new(
            std::time::Duration::from_secs(60),
            walthamstow_kernel::clock::Moment::now,
        ));
        assert!(FileBlobStore::new(layout, WriteDurability::System, leases, kinds()).is_err());
    }

    #[test]
    fn a_resource_that_is_not_a_safe_directory_name_is_refused() {
        let harness = FileHarness::new();
        for resource in ["..", ".hidden", "a/b", "con"] {
            let document = document(resource);
            let (store, lease) = open(&harness, &document);
            assert!(
                store.put(&lease, "capture", b"one").is_err(),
                "{resource:?}"
            );
            assert!(store.list(&document, None, 1).is_err(), "{resource:?}");
        }
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
            [blob.clone()]
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
    fn a_link_where_a_blob_or_its_directory_should_be_is_refused_unopened() {
        let harness = FileHarness::new();
        let document = document("turn-1");
        let (store, lease) = open(&harness, &document);
        let blob = store.put(&lease, "capture", b"one").unwrap();
        let linked = BlobRef::new("capture", sha256_hex(b"elsewhere")).unwrap();
        let elsewhere = harness.root.path().join("elsewhere.png");
        std::fs::write(&elsewhere, b"elsewhere").unwrap();
        std::os::unix::fs::symlink(&elsewhere, harness.path(&document, &linked)).unwrap();
        assert!(store.get(&document, &linked).is_err());
        assert!(store.check(&document, &[linked.clone()]).is_err());
        assert!(store.list(&document, None, usize::MAX).is_err());
        assert!(store.remove(&lease, &[linked.clone()]).is_err());
        assert!(store.put(&lease, "capture", b"elsewhere").is_err());
        assert!(elsewhere.exists());

        let text = harness
            .path(&document, &blob)
            .parent()
            .unwrap()
            .with_file_name("text");
        std::os::unix::fs::symlink(harness.root.path(), &text).unwrap();
        assert!(store.put(&lease, "text", b"one").is_err());
        assert!(store
            .get(
                &document,
                &BlobRef::new("text", sha256_hex(b"one")).unwrap()
            )
            .is_err());
    }
}
