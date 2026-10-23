//! Copyright (c) Scott A Dixon
//!
//! A canonical store kept as workspace files. Each part of a document is a file under its
//! entity's directory, at the part's name. A file holds no version, so the store keeps, in the
//! engine's local records, the version each document's files render and a digest of each
//! file. Files whose digests differ from what the store published were written outside any
//! engine, and the store holds them with no version. The workspace lock's lease fences every
//! write, and each entity's files are read and written under one lock.

use super::{
    between, distinct, document_key, lapsed, rendered, CanonicalStore, DocumentCodec, Part,
    PartName, Publication, Pulled,
};
use async_trait::async_trait;
use clerkenwell_store::{CollaborationDocumentId, CollaborationVersion};
use serde::{Deserialize, Serialize};
use std::collections::{BTreeMap, BTreeSet};
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex, PoisonError};
use walthamstow_kernel::durability::WriteDurability;
use walthamstow_kernel::files::identity;
use walthamstow_kernel::hashing::sha256_hex;
use walthamstow_kernel::journals::Journal;
use walthamstow_kernel::leases::{FileLeases, Lease};
use walthamstow_kernel::local_store::{LocalStore, Tag};
use walthamstow_kernel::{KernelError, KernelResult};

/// The journal recording what each document's files render.
pub const RENDERINGS: Journal = Journal::new("canonical-files");

/// What one document's files render, as the store published them.
#[derive(Debug, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct Rendering {
    entity: String,
    resource_id: String,
    /// The version the files render; `None` for files an upgrade wrote.
    version: Option<CollaborationVersion>,
    /// The SHA-256 of each part's file, by the part's name.
    parts: BTreeMap<String, String>,
}

impl Rendering {
    /// Whether `parts` are exactly the parts this rendering published.
    fn renders(&self, parts: &[Part]) -> bool {
        parts.len() == self.parts.len()
            && parts
                .iter()
                .all(|part| self.parts.get(part.name.as_str()) == Some(&sha256_hex(&part.bytes)))
    }
}

#[derive(Debug)]
struct Entity {
    directory: PathBuf,
    codec: Arc<dyn DocumentCodec>,
    /// Held while the entity's files and renderings are read or written, so two documents
    /// never write one file at once and a read sees a whole publication. Every store over one
    /// entity directory in this process shares it.
    lock: Arc<Mutex<()>>,
}

/// What the store holds of one document.
struct Held {
    rendering: Option<(Rendering, Tag)>,
    parts: Vec<Part>,
    /// Whether the rendering renders `parts`.
    rendered: bool,
}

/// Documents kept as files under each entity's workspace directory.
#[derive(Debug)]
pub struct FileCanonicalStore {
    leases: Arc<FileLeases>,
    local: Arc<dyn LocalStore>,
    durability: WriteDurability,
    entities: BTreeMap<String, Entity>,
}

fn internal(path: &Path, error: impl std::fmt::Display) -> KernelError {
    KernelError::internal(format!("Workspace file {}: {error}", path.display()))
}

/// The lock of each entity directory a store in this process keeps documents in, by the
/// directory as an absolute path, its case folded. Every spelling of a directory on a file
/// system that folds case shares its lock, and directories whose names differ only in case
/// share one anywhere, which costs only concurrency.
static ENTITY_LOCKS: Mutex<BTreeMap<String, std::sync::Weak<Mutex<()>>>> =
    Mutex::new(BTreeMap::new());

/// The lock every store in this process shares over `directory`.
fn entity_lock(directory: &Path) -> Arc<Mutex<()>> {
    let key = std::path::absolute(directory)
        .unwrap_or_else(|_| directory.to_path_buf())
        .to_string_lossy()
        .to_lowercase();
    let mut locks = ENTITY_LOCKS.lock().unwrap_or_else(PoisonError::into_inner);
    locks.retain(|_, lock| lock.strong_count() > 0);
    if let Some(lock) = locks.get(&key).and_then(std::sync::Weak::upgrade) {
        return lock;
    }
    let lock = Arc::new(Mutex::new(()));
    locks.insert(key, Arc::downgrade(&lock));
    lock
}

impl FileCanonicalStore {
    /// A store whose writes `leases` fence, recording what each document's files render in
    /// `local`, and writing as durably as `durability` asks.
    pub fn new(
        leases: Arc<FileLeases>,
        local: Arc<dyn LocalStore>,
        durability: WriteDurability,
    ) -> Self {
        Self {
            leases,
            local,
            durability,
            entities: BTreeMap::new(),
        }
    }

    /// The store with `entity`'s documents kept under `directory`, as `codec` renders them.
    /// The product resolves `directory` within its workspace.
    pub fn with_entity(
        mut self,
        entity: &str,
        directory: PathBuf,
        codec: Arc<dyn DocumentCodec>,
    ) -> Self {
        self.entities.insert(
            entity.to_owned(),
            Entity {
                lock: entity_lock(&directory),
                directory,
                codec,
            },
        );
        self
    }

    fn entity(&self, entity: &str) -> KernelResult<&Entity> {
        self.entities.get(entity).ok_or_else(|| {
            KernelError::internal(format!("No workspace directory keeps {entity} documents."))
        })
    }

    fn rendering(
        &self,
        document: &CollaborationDocumentId,
    ) -> KernelResult<Option<(Rendering, Tag)>> {
        let key = document_key(RENDERINGS, document);
        let Some((bytes, tag)) = self.local.get(&key)? else {
            return Ok(None);
        };
        let rendering: Rendering = serde_json::from_slice(&bytes)
            .map_err(|error| KernelError::internal(format!("Rendering record {key}: {error}")))?;
        Ok(Some((rendering, tag)))
    }

    /// The file a part is kept in, each directory of it that exists refused when it is not a
    /// directory, so nothing read or written through it leaves the entity's directory.
    fn path(directory: &Path, name: &PartName) -> KernelResult<PathBuf> {
        let mut path = directory.to_path_buf();
        for component in name.as_str().split('/') {
            match std::fs::symlink_metadata(&path) {
                Ok(metadata) if !metadata.is_dir() => {
                    return Err(internal(
                        &path,
                        "this is not a directory, and is not followed",
                    ))
                }
                Ok(_) => {}
                Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
                Err(error) => return Err(internal(&path, error)),
            }
            path.push(component);
        }
        Ok(path)
    }

    /// The bytes of the file at `path`; `None` when there is none. Anything there but a
    /// regular file is refused unopened.
    fn read(path: &Path) -> KernelResult<Option<Vec<u8>>> {
        match std::fs::symlink_metadata(path) {
            Ok(metadata) if metadata.file_type().is_file() => std::fs::read(path)
                .map(Some)
                .map_err(|error| internal(path, error)),
            Ok(_) => Err(internal(
                path,
                "a document is a regular file; this is not followed or opened",
            )),
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(None),
            Err(error) => Err(internal(path, error)),
        }
    }

    /// The files a rendering names that still hold what the store wrote there.
    fn unchanged(entity: &Entity, rendering: &Rendering) -> KernelResult<Vec<Part>> {
        let mut parts = Vec::new();
        for (name, digest) in &rendering.parts {
            let name = PartName::new(name.clone())?;
            let Some(bytes) = Self::read(&Self::path(&entity.directory, &name)?)? else {
                continue;
            };
            if sha256_hex(&bytes) == *digest {
                parts.push(Part { name, bytes });
            }
        }
        Ok(parts)
    }

    /// Every file under the entity's directory, as a part. A hidden entry is passed over; a
    /// link or special file is refused.
    fn discover(entity: &Entity) -> KernelResult<Vec<Part>> {
        fn visit(directory: &Path, prefix: &str, parts: &mut Vec<Part>) -> KernelResult<()> {
            let entries = match std::fs::read_dir(directory) {
                Ok(entries) => entries,
                Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(()),
                Err(error) => return Err(internal(directory, error)),
            };
            for entry in entries {
                let entry = entry.map_err(|error| internal(directory, error))?;
                let path = entry.path();
                let name = entry
                    .file_name()
                    .into_string()
                    .map_err(|_| internal(&path, "this is not named in UTF-8"))?;
                if name.starts_with('.') {
                    continue;
                }
                let entry_name = name.clone();
                let relative = if prefix.is_empty() {
                    name
                } else {
                    format!("{prefix}/{name}")
                };
                let kind = entry.file_type().map_err(|error| internal(&path, error))?;
                if kind.is_dir() {
                    visit(&path, &relative, parts)?;
                } else if kind.is_file() {
                    if walthamstow_kernel::durability::staged_by_write(&entry_name) {
                        continue;
                    }
                    let bytes = std::fs::read(&path).map_err(|error| internal(&path, error))?;
                    parts.push(Part {
                        name: PartName::new(relative)?,
                        bytes,
                    });
                } else {
                    return Err(internal(
                        &path,
                        "a document is a regular file; this is not followed or opened",
                    ));
                }
            }
            Ok(())
        }
        let mut parts = Vec::new();
        visit(&entity.directory, "", &mut parts)?;
        parts.sort_by(|left, right| left.name.cmp(&right.name));
        Ok(parts)
    }

    /// The parts found under the entity's directory that `resource_id`'s document holds.
    fn discovered(entity: &Entity, resource_id: &str) -> KernelResult<Vec<Part>> {
        let mut parts = Vec::new();
        for part in Self::discover(entity)? {
            if entity.codec.resource_of(&part)?.as_deref() == Some(resource_id) {
                parts.push(part);
            }
        }
        Ok(parts)
    }

    /// What the store holds of `document`: its rendering, when it has one; every file that is
    /// the document's, those the rendering names that still hold what the store wrote and those
    /// the codec names as the document's; and whether the rendering renders exactly those.
    fn held(&self, entity: &Entity, document: &CollaborationDocumentId) -> KernelResult<Held> {
        let rendering = self.rendering(document)?;
        let mut parts = match &rendering {
            Some((held, _)) => Self::unchanged(entity, held)?,
            None => Vec::new(),
        };
        for part in Self::discovered(entity, &document.resource_id)? {
            if !parts.iter().any(|held| held.name == part.name) {
                parts.push(part);
            }
        }
        parts.sort_by(|left, right| left.name.cmp(&right.name));
        let rendered = rendering
            .as_ref()
            .is_some_and(|(held, _)| held.renders(&parts));
        Ok(Held {
            rendering,
            parts,
            rendered,
        })
    }

    /// Whether `bytes`, a file at a part's name, are `document`'s: the bytes the store wrote
    /// there for it, by `digest`, or bytes the codec names as `document`'s.
    fn belongs(
        entity: &Entity,
        document: &CollaborationDocumentId,
        name: &PartName,
        bytes: &[u8],
        digest: Option<&String>,
    ) -> KernelResult<bool> {
        if digest == Some(&sha256_hex(bytes)) {
            return Ok(true);
        }
        Ok(entity
            .codec
            .resource_of(&Part {
                name: name.clone(),
                bytes: bytes.to_vec(),
            })?
            .as_deref()
            == Some(document.resource_id.as_str()))
    }

    /// The files writing `parts` as `document`'s changes, each checked before any is written: a
    /// file there that the codec names as another document's refuses them all. `digests` are
    /// the files the store wrote for `document`, by name, `removed` the files this publication
    /// removes before it writes, which a part may replace with a directory or a file, and
    /// `renames` the files it moves onto a part's file before it writes, which leave their own
    /// bytes there.
    fn writes<'a>(
        entity: &Entity,
        document: &CollaborationDocumentId,
        digests: &BTreeMap<String, String>,
        removed: &[PathBuf],
        renames: &[(PathBuf, PathBuf)],
        parts: &'a [Part],
    ) -> KernelResult<Vec<(PathBuf, &'a Part)>> {
        let mut writes = Vec::new();
        for part in parts {
            // Each directory on the way that exists is a directory, or a file this publication
            // removes, which the part's directories take the place of.
            let components = part.name.as_str().split('/').collect::<Vec<_>>();
            let (last, directories) = components.split_last().expect("a name has a component");
            let mut path = entity.directory.clone();
            let mut replaced = false;
            for (depth, component) in std::iter::once(&"").chain(directories).enumerate() {
                if depth > 0 {
                    path.push(component);
                }
                if replaced {
                    continue;
                }
                match std::fs::symlink_metadata(&path) {
                    Ok(metadata) if metadata.is_dir() => {}
                    Ok(metadata) if metadata.file_type().is_file() && among(removed, &path)? => {
                        replaced = true;
                    }
                    Ok(_) => {
                        return Err(internal(
                            &path,
                            "this is not a directory, and is not followed",
                        ))
                    }
                    Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
                    Err(error) => return Err(internal(&path, error)),
                }
            }
            path.push(last);
            let held = match std::fs::symlink_metadata(&path) {
                _ if replaced => None,
                Ok(metadata) if metadata.is_dir() && Self::holds_only(&path, removed)? => None,
                _ => Self::read(&path)?,
            };
            // A file renamed onto the part's leaves its own bytes there, and replaces the file
            // it lands on, unless that is the renamed file itself, whatever that file holds. A
            // file written over is replaced only when its bytes change.
            let (after, overwritten) = match renames.iter().find(|(_, to)| *to == path) {
                Some((from, _)) => {
                    let moved = Self::read(from)?;
                    let landing = match held {
                        Some(_) if among(std::slice::from_ref(from), &path)? => None,
                        held => held,
                    };
                    (moved, landing)
                }
                None => {
                    let changed = held
                        .clone()
                        .filter(|bytes| bytes.as_slice() != part.bytes.as_slice());
                    (held, changed)
                }
            };
            if let Some(bytes) = overwritten {
                let owner = entity.codec.resource_of(&Part {
                    name: part.name.clone(),
                    bytes: bytes.clone(),
                })?;
                let ours = digests.get(part.name.as_str()) == Some(&sha256_hex(&bytes));
                if !ours && owner.is_some_and(|owner| owner != document.resource_id) {
                    return Err(KernelError::conflict(format!(
                        "{} is another {} document's file.",
                        path.display(),
                        document.entity
                    )));
                }
            }
            if after.as_deref() == Some(part.bytes.as_slice()) {
                continue;
            }
            writes.push((path, part));
        }
        Ok(writes)
    }

    /// Whether `directory` holds nothing but directories, files among `removed` and files a
    /// stopped write staged.
    fn holds_only(directory: &Path, removed: &[PathBuf]) -> KernelResult<bool> {
        for entry in std::fs::read_dir(directory).map_err(|error| internal(directory, error))? {
            let entry = entry.map_err(|error| internal(directory, error))?;
            let path = entry.path();
            let kind = entry.file_type().map_err(|error| internal(&path, error))?;
            let held = if kind.is_dir() {
                Self::holds_only(&path, removed)?
            } else {
                kind.is_file() && (staged(&entry.file_name()) || among(removed, &path)?)
            };
            if !held {
                return Ok(false);
            }
        }
        Ok(true)
    }

    /// The files of `names` that are still `document`'s, to remove: those the store wrote for it,
    /// by `digests`, or that the codec names as its. A file edited into another document's, or
    /// into none, is left.
    fn removals(
        entity: &Entity,
        document: &CollaborationDocumentId,
        digests: &BTreeMap<String, String>,
        names: impl IntoIterator<Item = PartName>,
    ) -> KernelResult<Vec<PathBuf>> {
        let mut removals = Vec::new();
        for name in names {
            let path = Self::path(&entity.directory, &name)?;
            let Some(bytes) = Self::read(&path)? else {
                continue;
            };
            if Self::belongs(entity, document, &name, &bytes, digests.get(name.as_str()))? {
                removals.push(path);
            }
        }
        Ok(removals)
    }

    fn write(&self, writes: Vec<(PathBuf, &Part)>) -> KernelResult<()> {
        for (path, part) in writes {
            walthamstow_kernel::durability::write_synced(self.durability, &path, &part.bytes)
                .map_err(|error| internal(&path, error))?;
        }
        Ok(())
    }

    /// The files of `renamed` parts that are still `document`'s, each with the file its new
    /// name gives it.
    fn renames(
        entity: &Entity,
        document: &CollaborationDocumentId,
        digests: &BTreeMap<String, String>,
        renamed: Vec<(PartName, PartName)>,
    ) -> KernelResult<Vec<(PathBuf, PathBuf)>> {
        let mut renames = Vec::new();
        for (from, to) in renamed {
            for path in Self::removals(entity, document, digests, [from])? {
                renames.push((path, Self::path(&entity.directory, &to)?));
            }
        }
        Ok(renames)
    }

    /// Renames each part renamed only in case to its new name before it is written, so the
    /// document never lacks it, whether or not the file system folds case.
    fn rename_files(&self, renames: Vec<(PathBuf, PathBuf)>) -> KernelResult<()> {
        for (from, to) in renames {
            let parent = to.parent().expect("a part lies in its entity's directory");
            walthamstow_kernel::durability::create_dir_all_synced(self.durability, parent)
                .map_err(|error| internal(parent, error))?;
            std::fs::rename(&from, &to).map_err(|error| internal(&from, error))?;
            for directory in [parent, from.parent().expect("a part lies in a directory")] {
                self.durability
                    .sync_directory(directory)
                    .map_err(|error| internal(directory, error))?;
            }
        }
        Ok(())
    }

    /// Removes each file, and each directory between it and `directory` that it leaves holding
    /// nothing but files a stopped write staged, which go with it.
    fn remove_files(&self, directory: &Path, paths: Vec<PathBuf>) -> KernelResult<()> {
        for path in paths {
            std::fs::remove_file(&path).map_err(|error| internal(&path, error))?;
            let mut emptied = path
                .parent()
                .expect("a part lies in its entity's directory")
                .to_path_buf();
            while emptied != directory && emptied.starts_with(directory) {
                if !Self::remove_staged_directory(&emptied)? {
                    break;
                }
                emptied.pop();
            }
            self.durability
                .sync_directory(&emptied)
                .map_err(|error| internal(&emptied, error))?;
        }
        Ok(())
    }

    /// Removes `directory` when it holds nothing but files a stopped write staged, with them,
    /// and reports whether it did.
    fn remove_staged_directory(directory: &Path) -> KernelResult<bool> {
        let mut staged_files = Vec::new();
        for entry in std::fs::read_dir(directory).map_err(|error| internal(directory, error))? {
            let entry = entry.map_err(|error| internal(directory, error))?;
            let kind = entry
                .file_type()
                .map_err(|error| internal(&entry.path(), error))?;
            if !kind.is_file() || !staged(&entry.file_name()) {
                return Ok(false);
            }
            staged_files.push(entry.path());
        }
        for path in staged_files {
            std::fs::remove_file(&path).map_err(|error| internal(&path, error))?;
        }
        std::fs::remove_dir(directory).map_err(|error| internal(directory, error))?;
        Ok(true)
    }

    /// Records that `document`'s files render `parts` at `version`, over the record `held`
    /// names, if any.
    fn record(
        &self,
        document: &CollaborationDocumentId,
        held: Option<&Tag>,
        version: Option<CollaborationVersion>,
        parts: &[Part],
    ) -> KernelResult<()> {
        let next = Rendering {
            entity: document.entity.clone(),
            resource_id: document.resource_id.clone(),
            version,
            parts: current(parts),
        };
        let bytes = serde_json::to_vec(&next).expect("a rendering encodes");
        let key = document_key(RENDERINGS, document);
        self.local
            .compare_and_swap(&key, held, &bytes)?
            .ok_or_else(|| {
                KernelError::internal(format!(
                    "Rendering record {key} changed while it was written."
                ))
            })?;
        Ok(())
    }

    fn remove_rendering(&self, document: &CollaborationDocumentId, tag: &Tag) -> KernelResult<()> {
        let key = document_key(RENDERINGS, document);
        if !self.local.remove(&key, tag)? {
            return Err(KernelError::internal(format!(
                "Rendering record {key} changed while it was removed."
            )));
        }
        Ok(())
    }

    fn require_lease(&self, document: &CollaborationDocumentId, lease: &Lease) -> KernelResult<()> {
        if lease.document != *document || !self.leases.holds(lease) {
            return Err(lapsed(document));
        }
        Ok(())
    }
}

/// Whether a file named `name` is one a stopped write staged.
fn staged(name: &std::ffi::OsStr) -> bool {
    name.to_str()
        .is_some_and(walthamstow_kernel::durability::staged_by_write)
}

/// Whether the regular file at `path` is one of `files`, however the file system folds the case
/// of either name.
fn among(files: &[PathBuf], path: &Path) -> KernelResult<bool> {
    let held = identity(path).map_err(|error| internal(path, error))?;
    for file in files {
        if identity(file).map_err(|error| internal(file, error))? == held {
            return Ok(true);
        }
    }
    Ok(false)
}

/// The names of `digests` that `parts` drop: those `parts` name again in another case, each with
/// its new name, and the rest.
#[allow(clippy::type_complexity)]
fn dropped(
    digests: &BTreeMap<String, String>,
    parts: &[Part],
) -> KernelResult<(Vec<(PartName, PartName)>, Vec<PartName>)> {
    let mut renamed = Vec::new();
    let mut dropped = Vec::new();
    for name in digests.keys() {
        if parts.iter().any(|part| part.name.as_str() == name) {
            continue;
        }
        let folded = name.to_lowercase();
        match parts
            .iter()
            .find(|part| part.name.as_str().to_lowercase() == folded)
        {
            Some(part) => renamed.push((PartName::new(name.clone())?, part.name.clone())),
            None => dropped.push(PartName::new(name.clone())?),
        }
    }
    Ok((renamed, dropped))
}

/// The digest of each of `parts`, by name.
fn current(parts: &[Part]) -> BTreeMap<String, String> {
    parts
        .iter()
        .map(|part| (part.name.as_str().to_owned(), sha256_hex(&part.bytes)))
        .collect()
}

fn digests(rendering: &Option<(Rendering, Tag)>) -> BTreeMap<String, String> {
    rendering
        .as_ref()
        .map(|(rendering, _)| rendering.parts.clone())
        .unwrap_or_default()
}

#[async_trait]
impl CanonicalStore for FileCanonicalStore {
    async fn publish(
        &self,
        document: &CollaborationDocumentId,
        lease: &Lease,
        publication: &Publication,
    ) -> KernelResult<CollaborationVersion> {
        self.require_lease(document, lease)?;
        rendered(&publication.parts)?;
        self.admit(document, &publication.parts)?;
        let entity = self.entity(&document.entity)?;
        let _entity = entity.lock.lock().unwrap_or_else(PoisonError::into_inner);
        let held = self.held(entity, document)?;
        let rendering = held.rendering;
        if let Some((rendering, _)) = &rendering {
            if held.rendered
                && rendering.version.as_ref() == Some(&publication.version)
                && rendering.renders(&publication.parts)
            {
                return Ok(publication.version.clone());
            }
        }
        // What the store holds of the document now is its, whatever the rendering records, and
        // what the publication omits of it goes.
        let current = current(&held.parts);
        let (renamed, dropped) = dropped(&current, &publication.parts)?;
        let removals = Self::removals(entity, document, &current, dropped)?;
        let renames = Self::renames(entity, document, &current, renamed)?;
        let writes = Self::writes(
            entity,
            document,
            &current,
            &removals,
            &renames,
            &publication.parts,
        )?;
        // Dropped parts go first, so a part may take the place of one as a file or a directory.
        self.remove_files(&entity.directory, removals)?;
        self.rename_files(renames)?;
        self.write(writes)?;
        self.record(
            document,
            rendering.as_ref().map(|(_, tag)| tag),
            Some(publication.version.clone()),
            &publication.parts,
        )?;
        Ok(publication.version.clone())
    }

    async fn pull(&self, document: &CollaborationDocumentId) -> KernelResult<Option<Pulled>> {
        let entity = self.entity(&document.entity)?;
        let _entity = entity.lock.lock().unwrap_or_else(PoisonError::into_inner);
        let held = self.held(entity, document)?;
        if held.parts.is_empty() {
            return Ok(None);
        }
        Ok(Some(Pulled {
            version: held
                .rendering
                .filter(|_| held.rendered)
                .and_then(|(rendering, _)| rendering.version),
            operations: None,
            parts: held.parts,
        }))
    }

    async fn list(&self, entity: &str) -> KernelResult<Vec<CollaborationDocumentId>> {
        let kept = self.entity(entity)?;
        let _entity = kept.lock.lock().unwrap_or_else(PoisonError::into_inner);
        let mut resources = BTreeSet::new();
        for part in Self::discover(kept)? {
            if let Some(resource_id) = kept.codec.resource_of(&part)? {
                resources.insert(resource_id);
            }
        }
        Ok(resources
            .into_iter()
            .map(|resource_id| CollaborationDocumentId::new(entity, resource_id))
            .collect())
    }

    fn admit(&self, document: &CollaborationDocumentId, parts: &[Part]) -> KernelResult<()> {
        distinct(parts)?;
        let entity = self.entity(&document.entity)?;
        for part in parts {
            if let Some(owner) = entity.codec.resource_of(part)? {
                if owner != document.resource_id {
                    return Err(KernelError::invalid_request(format!(
                        "{} \"{}\" cannot hold {}, which names {} \"{owner}\".",
                        document.entity, document.resource_id, part.name, document.entity
                    )));
                }
            }
        }
        Ok(())
    }

    async fn remove(&self, document: &CollaborationDocumentId, lease: &Lease) -> KernelResult<()> {
        self.require_lease(document, lease)?;
        let entity = self.entity(&document.entity)?;
        let _entity = entity.lock.lock().unwrap_or_else(PoisonError::into_inner);
        let rendering = self.rendering(document)?;
        let digests = digests(&rendering);
        let mut names = digests
            .keys()
            .map(|name| PartName::new(name.clone()))
            .collect::<KernelResult<BTreeSet<_>>>()?;
        names.extend(
            Self::discovered(entity, &document.resource_id)?
                .into_iter()
                .map(|part| part.name),
        );
        let removals = Self::removals(entity, document, &digests, names)?;
        self.remove_files(&entity.directory, removals)?;
        if let Some((_, tag)) = &rendering {
            self.remove_rendering(document, tag)?;
        }
        Ok(())
    }

    async fn replace(
        &self,
        document: &CollaborationDocumentId,
        lease: &Lease,
        expected: &[Part],
        parts: &[Part],
    ) -> KernelResult<bool> {
        self.require_lease(document, lease)?;
        self.admit(document, parts)?;
        let entity = self.entity(&document.entity)?;
        let _entity = entity.lock.lock().unwrap_or_else(PoisonError::into_inner);
        let held = self.held(entity, document)?;
        if !between(&held.parts, expected, parts) {
            return Ok(false);
        }
        // What the store holds of the document now is its, whatever the rendering records.
        let current = current(&held.parts);
        let (renamed, dropped) = dropped(&current, parts)?;
        let removals = Self::removals(entity, document, &current, dropped)?;
        let renames = Self::renames(entity, document, &current, renamed)?;
        let writes = Self::writes(entity, document, &current, &removals, &renames, parts)?;
        self.remove_files(&entity.directory, removals)?;
        self.rename_files(renames)?;
        self.write(writes)?;
        // No version names what the upgrade wrote, and the record holds its files as the
        // document's.
        let rendering = held.rendering.as_ref().map(|(_, tag)| tag);
        if !parts.is_empty() {
            self.record(document, rendering, None, parts)?;
        } else if let Some(tag) = rendering {
            self.remove_rendering(document, tag)?;
        }
        Ok(true)
    }
}

#[cfg(test)]
mod tests {
    use super::super::tests::{
        document, holder, holds_the_canonical_store_contract, part, publication, version, NoteCodec,
    };
    use super::*;
    use walthamstow_kernel::leases::Leases;
    use walthamstow_kernel::local_store::FileLocalStore;
    use walthamstow_kernel::workspace_lock::WorkspaceLock;

    const DURABILITY: WriteDurability = WriteDurability::System;

    struct Workspace {
        root: tempfile::TempDir,
        _data: tempfile::TempDir,
        leases: Arc<FileLeases>,
        store: FileCanonicalStore,
    }

    fn workspace() -> Workspace {
        let root = tempfile::tempdir().unwrap();
        let data = tempfile::tempdir().unwrap();
        let local = FileLocalStore::open(data.path(), DURABILITY).unwrap();
        let leases = Arc::new(
            FileLeases::new(
                WorkspaceLock::acquire(root.path()).unwrap(),
                holder("engine"),
                local.as_ref(),
            )
            .unwrap(),
        );
        let store = FileCanonicalStore::new(leases.clone(), local, DURABILITY).with_entity(
            "Note",
            root.path().join("notes"),
            Arc::new(NoteCodec),
        );
        Workspace {
            root,
            _data: data,
            leases,
            store,
        }
    }

    async fn lease(workspace: &Workspace, resource_id: &str) -> Lease {
        workspace
            .leases
            .acquire(&document(resource_id), &holder("engine"))
            .await
            .unwrap()
            .unwrap()
    }

    #[tokio::test]
    async fn workspace_files_hold_the_canonical_store_contract() {
        let workspace = workspace();
        holds_the_canonical_store_contract(
            &workspace.store,
            workspace.leases.as_ref(),
            &holder("engine"),
        )
        .await;
    }

    #[tokio::test]
    async fn each_part_is_a_file_under_its_entity_directory() {
        let workspace = workspace();
        let published = publication(
            version("a", 1),
            vec![
                part("nested/one.yaml", "one\nbody"),
                part("one.yaml", "one\nhead"),
            ],
        );
        workspace
            .store
            .publish(
                &document("one"),
                &lease(&workspace, "one").await,
                &published,
            )
            .await
            .unwrap();
        let notes = workspace.root.path().join("notes");
        assert_eq!(
            std::fs::read(notes.join("nested/one.yaml")).unwrap(),
            b"one\nbody"
        );
        assert_eq!(std::fs::read(notes.join("one.yaml")).unwrap(), b"one\nhead");

        workspace
            .store
            .publish(
                &document("one"),
                &lease(&workspace, "one").await,
                &publication(version("a", 2), vec![part("one.yaml", "one\nonly")]),
            )
            .await
            .unwrap();
        assert!(
            !notes.join("nested/one.yaml").exists(),
            "a part the document no longer has is removed"
        );
    }

    #[tokio::test]
    async fn files_edited_outside_the_engine_are_held_with_no_version() {
        let workspace = workspace();
        workspace
            .store
            .publish(
                &document("one"),
                &lease(&workspace, "one").await,
                &publication(version("a", 1), vec![part("one.yaml", "one\nfirst")]),
            )
            .await
            .unwrap();
        std::fs::write(workspace.root.path().join("notes/one.yaml"), "one\nedited").unwrap();
        let pulled = workspace
            .store
            .pull(&document("one"))
            .await
            .unwrap()
            .unwrap();
        assert_eq!(pulled.version, None);
        assert_eq!(pulled.parts, vec![part("one.yaml", "one\nedited")]);
    }

    #[tokio::test]
    async fn a_file_someone_added_is_a_document_with_no_version() {
        let workspace = workspace();
        let notes = workspace.root.path().join("notes");
        std::fs::create_dir_all(notes.join("deeper")).unwrap();
        std::fs::write(notes.join("deeper/added.yaml"), "added\nby hand").unwrap();
        std::fs::write(notes.join(".DS_Store"), "ignored").unwrap();
        std::fs::write(notes.join("README"), "").unwrap();
        assert_eq!(
            workspace.store.list("Note").await.unwrap(),
            vec![document("added")]
        );
        let pulled = workspace
            .store
            .pull(&document("added"))
            .await
            .unwrap()
            .unwrap();
        assert_eq!(pulled.version, None);
        assert_eq!(
            pulled.parts,
            vec![part("deeper/added.yaml", "added\nby hand")]
        );
    }

    #[tokio::test]
    async fn another_documents_file_is_never_written_over() {
        let workspace = workspace();
        workspace
            .store
            .publish(
                &document("one"),
                &lease(&workspace, "one").await,
                &publication(version("a", 1), vec![part("shared.yaml", "one\nfirst")]),
            )
            .await
            .unwrap();
        let refused = workspace
            .store
            .publish(
                &document("two"),
                &lease(&workspace, "two").await,
                &publication(version("b", 1), vec![part("shared.yaml", "two\nsecond")]),
            )
            .await;
        assert!(refused.is_err());
        assert_eq!(
            std::fs::read(workspace.root.path().join("notes/shared.yaml")).unwrap(),
            b"one\nfirst"
        );
    }

    #[tokio::test]
    async fn a_file_edited_into_another_documents_is_neither_written_over_nor_removed() {
        let workspace = workspace();
        workspace
            .store
            .publish(
                &document("one"),
                &lease(&workspace, "one").await,
                &publication(version("a", 1), vec![part("shared.yaml", "one\nfirst")]),
            )
            .await
            .unwrap();
        let shared = workspace.root.path().join("notes/shared.yaml");
        std::fs::write(&shared, "two\nnow the second note").unwrap();

        assert!(workspace
            .store
            .publish(
                &document("one"),
                &lease(&workspace, "one").await,
                &publication(version("a", 2), vec![part("shared.yaml", "one\nsecond")]),
            )
            .await
            .is_err());
        workspace
            .store
            .remove(&document("one"), &lease(&workspace, "one").await)
            .await
            .unwrap();
        assert_eq!(
            std::fs::read(&shared).unwrap(),
            b"two\nnow the second note",
            "the file is the second note's now"
        );
    }

    #[tokio::test]
    async fn a_publication_refused_for_one_part_writes_none() {
        let workspace = workspace();
        workspace
            .store
            .publish(
                &document("two"),
                &lease(&workspace, "two").await,
                &publication(version("b", 1), vec![part("two.yaml", "two\nheld")]),
            )
            .await
            .unwrap();
        assert!(workspace
            .store
            .publish(
                &document("one"),
                &lease(&workspace, "one").await,
                &publication(
                    version("a", 1),
                    vec![
                        part("one.yaml", "one\nfirst"),
                        part("two.yaml", "one\ntaken")
                    ]
                ),
            )
            .await
            .is_err());
        assert!(!workspace.root.path().join("notes/one.yaml").exists());
        assert_eq!(
            std::fs::read(workspace.root.path().join("notes/two.yaml")).unwrap(),
            b"two\nheld"
        );
    }

    #[tokio::test]
    async fn a_part_renamed_only_in_case_is_kept_under_its_new_name() {
        let workspace = workspace();
        let one = document("one");
        workspace
            .store
            .publish(
                &one,
                &lease(&workspace, "one").await,
                &publication(version("a", 1), vec![part("note.yaml", "one\nfirst")]),
            )
            .await
            .unwrap();
        let renamed = vec![part("NOTE.yaml", "one\nsecond")];
        workspace
            .store
            .publish(
                &one,
                &lease(&workspace, "one").await,
                &publication(version("a", 2), renamed.clone()),
            )
            .await
            .unwrap();
        let pulled = workspace.store.pull(&one).await.unwrap().unwrap();
        assert_eq!(pulled.parts, renamed);
        assert_eq!(pulled.version, Some(version("a", 2)));
    }

    #[tokio::test]
    async fn a_file_a_stopped_write_staged_is_no_part() {
        let workspace = workspace();
        let one = document("one");
        let first = vec![part("one.yaml", "one\nfirst")];
        workspace
            .store
            .publish(
                &one,
                &lease(&workspace, "one").await,
                &publication(version("a", 1), first.clone()),
            )
            .await
            .unwrap();
        let staged = workspace
            .root
            .path()
            .join("notes")
            .join("one.0b1e2f3a-4c5d-4e6f-8a9b-0c1d2e3f4a5b.tmp");
        std::fs::write(&staged, "one\nhalf written").unwrap();
        let pulled = workspace.store.pull(&one).await.unwrap().unwrap();
        assert_eq!(pulled.parts, first);
        assert_eq!(pulled.version, Some(version("a", 1)));
        assert_eq!(workspace.store.list("Note").await.unwrap(), vec![one]);
    }

    #[tokio::test]
    async fn a_part_renamed_only_in_case_keeps_its_bytes_under_its_new_name() {
        let workspace = workspace();
        let one = document("one");
        for (counter, name) in [(1, "note.yaml"), (2, "NOTE.yaml")] {
            workspace
                .store
                .publish(
                    &one,
                    &lease(&workspace, "one").await,
                    &publication(version("a", counter), vec![part(name, "one\nsame")]),
                )
                .await
                .unwrap();
        }
        let pulled = workspace.store.pull(&one).await.unwrap().unwrap();
        assert_eq!(pulled.parts, vec![part("NOTE.yaml", "one\nsame")]);
        assert_eq!(pulled.version, Some(version("a", 2)));
    }

    /// Whether the file system `directory` lies on names one file by two names that differ
    /// only in case.
    fn folds_case(directory: &Path) -> bool {
        std::fs::create_dir_all(directory).unwrap();
        std::fs::write(directory.join("probe"), b"").unwrap();
        let folds = directory.join("PROBE").exists();
        std::fs::remove_file(directory.join("probe")).unwrap();
        folds
    }

    #[tokio::test]
    async fn a_part_renamed_only_in_case_onto_a_file_of_its_new_name_holds_its_new_bytes() {
        let workspace = workspace();
        let notes = workspace.root.path().join("notes");
        if folds_case(&notes) {
            // One file holds both names, so neither is left beside the other.
            return;
        }
        let one = document("one");
        let first = vec![part("note.yaml", "one\nfirst")];
        let second = vec![part("NOTE.yaml", "one\nsecond")];
        workspace
            .store
            .publish(
                &one,
                &lease(&workspace, "one").await,
                &publication(version("a", 1), first.clone()),
            )
            .await
            .unwrap();
        std::fs::write(notes.join("NOTE.yaml"), &second[0].bytes).unwrap();
        workspace
            .store
            .publish(
                &one,
                &lease(&workspace, "one").await,
                &publication(version("a", 2), second.clone()),
            )
            .await
            .unwrap();
        assert_eq!(
            workspace.store.pull(&one).await.unwrap().unwrap().parts,
            second
        );

        // As an upgrade replaces it.
        std::fs::write(notes.join("note.yaml"), &first[0].bytes).unwrap();
        assert!(workspace
            .store
            .replace(&one, &lease(&workspace, "one").await, &first, &second)
            .await
            .unwrap());
        assert_eq!(
            workspace.store.pull(&one).await.unwrap().unwrap().parts,
            second
        );
    }

    #[tokio::test]
    async fn a_part_may_take_the_place_of_a_dropped_one_as_a_file_or_a_directory() {
        let workspace = workspace();
        let one = document("one");
        let shapes = [
            vec![part("section", "one\nas a file")],
            vec![part("section/item.yaml", "one\nwithin a directory")],
            vec![part("section", "one\nas a file again")],
        ];
        for (counter, parts) in shapes.into_iter().enumerate() {
            workspace
                .store
                .publish(
                    &one,
                    &lease(&workspace, "one").await,
                    &publication(version("a", counter as u64 + 1), parts.clone()),
                )
                .await
                .unwrap();
            assert_eq!(
                workspace.store.pull(&one).await.unwrap().unwrap().parts,
                parts
            );
        }
    }

    #[tokio::test]
    async fn a_part_may_take_the_place_of_a_dropped_one_named_in_another_case() {
        let workspace = workspace();
        let one = document("one");
        let shapes = [
            vec![part("Section", "one\nas a file")],
            vec![part("section/item.yaml", "one\nwithin a directory")],
            vec![part("SECTION", "one\nas a file again")],
        ];
        for (counter, parts) in shapes.into_iter().enumerate() {
            workspace
                .store
                .publish(
                    &one,
                    &lease(&workspace, "one").await,
                    &publication(version("a", counter as u64 + 1), parts.clone()),
                )
                .await
                .unwrap();
            assert_eq!(
                workspace.store.pull(&one).await.unwrap().unwrap().parts,
                parts
            );
        }
    }

    #[tokio::test]
    async fn a_part_takes_the_place_of_a_directory_a_stopped_write_staged_a_file_in() {
        let workspace = workspace();
        let one = document("one");
        let nested = vec![part("section/item.yaml", "one\nwithin a directory")];
        let flat = vec![part("section", "one\nas a file")];
        workspace
            .store
            .publish(
                &one,
                &lease(&workspace, "one").await,
                &publication(version("a", 1), nested),
            )
            .await
            .unwrap();
        let staged = workspace
            .root
            .path()
            .join("notes/section/item.0b1e2f3a-4c5d-4e6f-8a9b-0c1d2e3f4a5b.tmp");
        std::fs::write(&staged, "one\nhalf written").unwrap();
        workspace
            .store
            .publish(
                &one,
                &lease(&workspace, "one").await,
                &publication(version("a", 2), flat.clone()),
            )
            .await
            .unwrap();
        assert_eq!(
            workspace.store.pull(&one).await.unwrap().unwrap().parts,
            flat
        );
        assert!(!staged.exists());
    }

    #[tokio::test]
    async fn a_part_naming_another_document_is_refused_unwritten() {
        let workspace = workspace();
        assert!(workspace
            .store
            .publish(
                &document("one"),
                &lease(&workspace, "one").await,
                &publication(version("a", 1), vec![part("one.yaml", "two\nborrowed")]),
            )
            .await
            .is_err());
        assert!(!workspace.root.path().join("notes/one.yaml").exists());
        assert!(workspace
            .store
            .admit(&document("one"), &[part("one.yaml", "one\nits own")])
            .is_ok());
    }

    #[tokio::test]
    async fn a_part_renamed_in_case_onto_another_documents_file_is_refused() {
        let workspace = workspace();
        let notes = workspace.root.path().join("notes");
        if folds_case(&notes) {
            // One file holds both names, so neither is another document's.
            return;
        }
        let one = document("one");
        workspace
            .store
            .publish(
                &one,
                &lease(&workspace, "one").await,
                &publication(version("a", 1), vec![part("note.yaml", "one\nsame")]),
            )
            .await
            .unwrap();
        std::fs::write(notes.join("NOTE.yaml"), "two\nheld").unwrap();
        assert!(workspace
            .store
            .publish(
                &one,
                &lease(&workspace, "one").await,
                &publication(version("a", 2), vec![part("NOTE.yaml", "one\nsame")]),
            )
            .await
            .is_err());
        assert_eq!(
            std::fs::read(notes.join("NOTE.yaml")).unwrap(),
            b"two\nheld"
        );
        assert_eq!(
            std::fs::read(notes.join("note.yaml")).unwrap(),
            b"one\nsame"
        );
    }

    #[tokio::test]
    async fn a_replacement_holds_what_it_wrote_and_writes_no_part_of_another_document() {
        let workspace = workspace();
        let one = document("one");
        let first = vec![part("one.yaml", "one\nformat: 1")];
        workspace
            .store
            .publish(
                &one,
                &lease(&workspace, "one").await,
                &publication(version("a", 1), first.clone()),
            )
            .await
            .unwrap();
        assert!(workspace
            .store
            .replace(
                &one,
                &lease(&workspace, "one").await,
                &first,
                &[part("one.yaml", "two\nformat: 2")]
            )
            .await
            .is_err());
        assert_eq!(
            workspace.store.pull(&one).await.unwrap().unwrap().parts,
            first
        );

        // Parts the codec names as no document's are the document's as the store wrote them.
        let unnamed = vec![part("one.yaml", "\nformat: 2")];
        assert!(workspace
            .store
            .replace(&one, &lease(&workspace, "one").await, &first, &unnamed)
            .await
            .unwrap());
        let pulled = workspace.store.pull(&one).await.unwrap().unwrap();
        assert_eq!((pulled.version, pulled.parts), (None, unnamed));
    }

    #[tokio::test]
    async fn every_store_over_one_entity_directory_shares_its_lock() {
        let workspace = workspace();
        let data = tempfile::tempdir().unwrap();
        let other = FileCanonicalStore::new(
            workspace.leases.clone(),
            FileLocalStore::open(data.path(), DURABILITY).unwrap(),
            DURABILITY,
        )
        .with_entity(
            "Note",
            workspace.root.path().join("notes"),
            Arc::new(NoteCodec),
        )
        .with_entity(
            "Shouted",
            workspace.root.path().join("NOTES"),
            Arc::new(NoteCodec),
        );
        for entity in ["Note", "Shouted"] {
            assert!(
                Arc::ptr_eq(
                    &workspace.store.entities["Note"].lock,
                    &other.entities[entity].lock
                ),
                "{entity}"
            );
        }
    }

    #[tokio::test]
    async fn a_file_added_for_a_published_document_is_held_with_no_version_until_omitted() {
        let workspace = workspace();
        let one = document("one");
        let first = vec![part("one.yaml", "one\nfirst")];
        workspace
            .store
            .publish(
                &one,
                &lease(&workspace, "one").await,
                &publication(version("a", 1), first.clone()),
            )
            .await
            .unwrap();
        std::fs::write(
            workspace.root.path().join("notes/extra.yaml"),
            "one\nadded by hand",
        )
        .unwrap();
        let pulled = workspace.store.pull(&one).await.unwrap().unwrap();
        assert_eq!(pulled.version, None);
        assert_eq!(
            pulled.parts,
            vec![part("extra.yaml", "one\nadded by hand"), first[0].clone()]
        );

        workspace
            .store
            .publish(
                &one,
                &lease(&workspace, "one").await,
                &publication(version("a", 2), first.clone()),
            )
            .await
            .unwrap();
        let pulled = workspace.store.pull(&one).await.unwrap().unwrap();
        assert_eq!(
            (pulled.version, pulled.parts),
            (Some(version("a", 2)), first),
            "a publication that omits the added file removes it"
        );
    }

    #[tokio::test]
    async fn a_lease_of_an_earlier_hold_of_the_workspace_writes_nothing() {
        let workspace = workspace();
        let stale = Lease {
            epoch: lease(&workspace, "one").await.epoch.wrapping_add(1),
            ..lease(&workspace, "one").await
        };
        assert!(workspace
            .store
            .publish(
                &document("one"),
                &stale,
                &publication(version("a", 1), vec![part("one.yaml", "one\nlost")]),
            )
            .await
            .is_err());
        assert!(!workspace.root.path().join("notes/one.yaml").exists());
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn a_link_where_a_part_lies_is_refused_and_its_target_left_alone() {
        let outside = tempfile::tempdir().unwrap();
        std::fs::write(outside.path().join("one.yaml"), "outside").unwrap();
        for linked in ["notes/one.yaml", "notes/nested"] {
            let workspace = workspace();
            let link = workspace.root.path().join(linked);
            std::fs::create_dir_all(link.parent().unwrap()).unwrap();
            let target = if linked.ends_with(".yaml") {
                outside.path().join("one.yaml")
            } else {
                outside.path().to_path_buf()
            };
            std::os::unix::fs::symlink(&target, &link).unwrap();
            let name = if linked.ends_with(".yaml") {
                "one.yaml"
            } else {
                "nested/one.yaml"
            };
            assert!(
                workspace
                    .store
                    .publish(
                        &document("one"),
                        &lease(&workspace, "one").await,
                        &publication(version("a", 1), vec![part(name, "one\nwritten")]),
                    )
                    .await
                    .is_err(),
                "{linked}"
            );
            assert!(workspace.store.list("Note").await.is_err(), "{linked}");
            assert_eq!(
                std::fs::read(outside.path().join("one.yaml")).unwrap(),
                b"outside"
            );
        }
    }
}
