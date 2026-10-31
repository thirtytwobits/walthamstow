//! Copyright (c) Scott A Dixon
//!
//! Where a product's log documents and their blobs are laid out on disk. Each document is the
//! directory `<root>/<entity directory>/<resource directory>/`, which the file log store and
//! the file blob store share. This module owns the layout, so each store takes it rather than
//! restating it.
//!
//! The root is held as a `cap-std` directory, and every operation goes through it: a path or a
//! link that would leave the root fails. A resource is encoded into its directory name, so two
//! resources never share a directory, whatever a file system folds. The product registers each
//! entity, and two whose names map to one directory are refused.

use cap_std::fs::Dir;
use percent_encoding::{percent_decode_str, percent_encode_byte, utf8_percent_encode, AsciiSet};
use std::collections::{BTreeMap, BTreeSet};
use std::fmt;
use std::path::{Path, PathBuf};
use std::sync::{Mutex, PoisonError};
use walthamstow_kernel::durability::WriteDurability;
use walthamstow_kernel::files;
use walthamstow_kernel::{DocumentId, KernelError, KernelResult};

/// The root a product's log documents are kept under, and the entities it registers.
#[derive(Debug)]
pub struct DocumentLayout {
    /// The root, through which every operation goes.
    root: Dir,
    /// The path the root was opened at, for messages.
    path: PathBuf,
    /// By entity name.
    entities: BTreeMap<String, Entity>,
}

/// An entity the product registers: the name of its directory, and the files the log store
/// keeps in each of its documents.
#[derive(Debug, Clone)]
struct Entity {
    directory: String,
    files: BTreeSet<String>,
}

/// A document's directory, open, and where it lies under the root.
pub struct DocumentDir {
    pub dir: Dir,
    /// The directory's path relative to the root.
    pub relative: PathBuf,
}

impl DocumentLayout {
    /// Documents kept under `root`, which is used as given, of no entity yet. The root must
    /// exist.
    pub fn open(root: &Path) -> KernelResult<Self> {
        let dir = files::open_dir(root).map_err(|error| internal(root, error))?;
        Ok(Self {
            root: dir,
            path: root.to_path_buf(),
            entities: BTreeMap::new(),
        })
    }

    /// This layout with `entity` registered, `files` being the files the log store keeps in each
    /// of its documents. Refused as an invalid request when the entity's name cannot name a
    /// directory, or when its directory is another registered entity's, since the two would
    /// share their documents.
    pub fn with_entity<I>(mut self, entity: &str, files: I) -> KernelResult<Self>
    where
        I: IntoIterator,
        I::Item: Into<String>,
    {
        let directory = entity_directory_name(entity)?;
        if let Some((other, _)) = self
            .entities
            .iter()
            .find(|(other, registered)| *other != entity && registered.directory == directory)
        {
            return Err(KernelError::invalid_request(format!(
                "The entities {entity:?} and {other:?} both map to the directory {directory:?}."
            )));
        }
        self.entities
            .entry(entity.to_owned())
            .or_insert_with(|| Entity {
                directory,
                files: BTreeSet::new(),
            })
            .files
            .extend(files.into_iter().map(Into::into));
        Ok(self)
    }

    /// The path the root was opened at.
    pub fn root(&self) -> &Path {
        &self.path
    }

    /// The name of every file the log store keeps in a document, of any entity.
    pub fn files(&self) -> impl Iterator<Item = &str> {
        self.entities
            .values()
            .flat_map(|entity| entity.files.iter().map(String::as_str))
    }

    /// The registered entity named `entity`.
    fn entity(&self, entity: &str) -> KernelResult<&Entity> {
        self.entities.get(entity).ok_or_else(|| {
            KernelError::invalid_request(format!(
                "The entity {entity:?} is not registered with the document layout."
            ))
        })
    }

    /// The files the log store keeps in each document of `entity`.
    pub(crate) fn entity_files(&self, entity: &str) -> KernelResult<&BTreeSet<String>> {
        Ok(&self.entity(entity)?.files)
    }

    /// `entity`'s directory and where it lies under the root, or `None` when it does not exist.
    pub(crate) fn entity_dir(&self, entity: &str) -> KernelResult<Option<(Dir, PathBuf)>> {
        let relative = PathBuf::from(&self.entity(entity)?.directory);
        Ok(self.open_dir(&relative)?.map(|dir| (dir, relative)))
    }

    /// Removes `document`'s directory whole, every entry in it included, and makes the removal
    /// durable as `durability` asks. A directory that does not exist is left so.
    pub(crate) fn remove_document_dir(
        &self,
        durability: WriteDurability,
        document: &DocumentId,
    ) -> KernelResult<()> {
        let relative = self.document_path(document)?;
        match self.root.remove_dir_all(&relative) {
            Ok(()) => {}
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
            Err(error) => return Err(internal(&self.path.join(&relative), error)),
        }
        self.sync(
            durability,
            relative.parent().expect("a document lies in its entity"),
        )
    }

    /// Where `document`'s directory lies under the root, whether or not it exists.
    pub fn document_path(&self, document: &DocumentId) -> KernelResult<PathBuf> {
        Ok(Path::new(&self.entity(document.entity())?.directory)
            .join(resource_directory_name(document.resource())?))
    }

    /// `document`'s directory, or `None` when it does not exist.
    pub fn document_dir(&self, document: &DocumentId) -> KernelResult<Option<DocumentDir>> {
        let relative = self.document_path(document)?;
        Ok(self
            .open_dir(&relative)?
            .map(|dir| DocumentDir { dir, relative }))
    }

    /// The directory at `relative` under the root, resolved through the root, or `None` when it
    /// does not exist.
    pub(crate) fn open_dir(&self, relative: &Path) -> KernelResult<Option<Dir>> {
        match self.root.open_dir(relative) {
            Ok(dir) => Ok(Some(dir)),
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(None),
            Err(error) => Err(internal(&self.path.join(relative), error)),
        }
    }

    /// `document`'s directory, created durably as `durability` asks when it does not exist.
    pub fn create_document_dir(
        &self,
        durability: WriteDurability,
        document: &DocumentId,
    ) -> KernelResult<DocumentDir> {
        if let Some(open) = self.document_dir(document)? {
            return Ok(open);
        }
        let relative = self.document_path(document)?;
        let _creating = CREATING.lock().unwrap_or_else(PoisonError::into_inner);
        let entity = relative.parent().expect("a document lies in its entity");
        self.create_level(durability, entity)?;
        self.create_level(durability, &relative)?;
        let dir = self
            .root
            .open_dir(&relative)
            .map_err(|error| internal(&self.path.join(&relative), error))?;
        Ok(DocumentDir { dir, relative })
    }

    /// Creates `relative`'s sub-directory `name` within the root if it does not exist, and
    /// returns it open.
    pub(crate) fn create_dir(
        &self,
        durability: WriteDurability,
        relative: &Path,
        name: &str,
    ) -> KernelResult<Dir> {
        let path = relative.join(name);
        self.create_level(durability, &path)?;
        self.root
            .open_dir(&path)
            .map_err(|error| internal(&self.path.join(&path), error))
    }

    /// Creates the directory at `relative` under the root, whose parent exists, and makes its
    /// entry durable. A directory that exists is left as it is: a creation whose sync fails is
    /// undone, so that a directory found existing has a durable entry, and a repeated creation
    /// syncs again.
    fn create_level(&self, durability: WriteDurability, relative: &Path) -> KernelResult<()> {
        match self.root.create_dir(relative) {
            Ok(()) => {}
            Err(error) if error.kind() == std::io::ErrorKind::AlreadyExists => return Ok(()),
            Err(error) => return Err(internal(&self.path.join(relative), error)),
        }
        let parent = relative.parent().expect("a level lies under the root");
        if let Err(error) = self.sync(durability, parent) {
            let _ = self.root.remove_dir(relative);
            return Err(error);
        }
        Ok(())
    }

    /// Makes the entries of the directory at `relative` under the root durable, through the
    /// root's handle. A directory that does not exist has no entries to make durable.
    pub(crate) fn sync(&self, durability: WriteDurability, relative: &Path) -> KernelResult<()> {
        let synced = if relative.as_os_str().is_empty() {
            durability.sync_dir(&self.root)
        } else {
            match self.root.open_dir(relative) {
                Ok(dir) => durability.sync_dir(&dir),
                Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(()),
                Err(error) => Err(error),
            }
        };
        synced.map_err(|error| internal(&self.path.join(relative), error))
    }
}

/// Held from a document directory's absence being seen to its creation.
static CREATING: Mutex<()> = Mutex::new(());

/// The name of the directory holding `entity`'s documents: the entity's name in lowercase, with
/// every character except ASCII letters, digits, `-` and `_` written as `-`. An empty name, or
/// one that maps to a device name Windows reserves, is refused.
pub fn entity_directory_name(entity: &str) -> KernelResult<String> {
    let name: String = entity
        .chars()
        .map(|character| {
            if character.is_ascii_alphanumeric() || character == '-' || character == '_' {
                character.to_ascii_lowercase()
            } else {
                '-'
            }
        })
        .collect();
    if name.is_empty() || files::is_windows_device_name(&name) {
        return Err(KernelError::invalid_request(format!(
            "The entity {entity:?} maps to {name:?}, which cannot name its directory."
        )));
    }
    Ok(name)
}

/// Every byte a resource directory name encodes: everything but lowercase ASCII letters,
/// digits, `-` and `_`. Uppercase letters are encoded too, so no two names differ only in case,
/// and so is every non-ASCII byte, so no two names differ only in normalisation.
const ENCODED: &AsciiSet = &percent_encoding::NON_ALPHANUMERIC
    .remove(b'-')
    .remove(b'_')
    .add(b'A')
    .add(b'B')
    .add(b'C')
    .add(b'D')
    .add(b'E')
    .add(b'F')
    .add(b'G')
    .add(b'H')
    .add(b'I')
    .add(b'J')
    .add(b'K')
    .add(b'L')
    .add(b'M')
    .add(b'N')
    .add(b'O')
    .add(b'P')
    .add(b'Q')
    .add(b'R')
    .add(b'S')
    .add(b'T')
    .add(b'U')
    .add(b'V')
    .add(b'W')
    .add(b'X')
    .add(b'Y')
    .add(b'Z');

/// The name of the directory holding the document whose resource is `resource`: the resource
/// percent-encoded, with every byte but lowercase ASCII letters, digits, `-` and `_` written as
/// `%` and two uppercase hex digits, and the first byte written so when the result would be a
/// device name Windows reserves. Every file system holds two such names as two entries. An
/// empty resource, or one whose name is longer than Windows allows, is refused.
pub fn resource_directory_name(resource: &str) -> KernelResult<String> {
    let mut name = utf8_percent_encode(resource, ENCODED).to_string();
    if files::is_windows_device_name(&name) {
        let first = name.remove(0);
        name.insert_str(0, percent_encode_byte(first as u8));
    }
    if name.is_empty() || name.len() > 255 {
        return Err(KernelError::invalid_request(format!(
            "The resource {resource:?} cannot name a directory: it is empty or too long."
        )));
    }
    Ok(name)
}

/// The resource whose directory [`resource_directory_name`] names `name`, or `None` when
/// `name` is not one it writes.
pub fn resource_of(name: &str) -> Option<String> {
    let resource = percent_decode_str(name).decode_utf8().ok()?.into_owned();
    (resource_directory_name(&resource).ok()? == name).then_some(resource)
}

/// An internal failure at `path`.
pub(crate) fn internal(path: &Path, error: impl fmt::Display) -> KernelError {
    KernelError::internal(format!("Log document storage {}: {error}", path.display()))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn an_entity_directory_is_its_name_lowered_to_a_safe_alphabet() {
        assert_eq!(entity_directory_name("Session").unwrap(), "session");
        assert_eq!(entity_directory_name("My Notes/v2").unwrap(), "my-notes-v2");
        for refused in ["", "CON", "lpt1"] {
            assert!(entity_directory_name(refused).is_err(), "{refused:?}");
        }
    }

    #[test]
    fn a_resource_directory_is_the_resource_encoded_and_reads_back() {
        for (resource, name) in [
            ("turn-1", "turn-1"),
            ("Turn", "%54urn"),
            ("caf\u{e9}", "caf%C3%A9"),
            ("a/b", "a%2Fb"),
            ("a\\b", "a%5Cb"),
            (".hidden", "%2Ehidden"),
            ("..", "%2E%2E"),
            ("note.", "note%2E"),
            ("with space", "with%20space"),
            ("100%", "100%25"),
            ("con", "%63on"),
            ("COM1", "%43%4F%4D1"),
            ("lpt1", "%6Cpt1"),
            ("console", "console"),
        ] {
            assert_eq!(
                resource_directory_name(resource).unwrap(),
                name,
                "{resource:?}"
            );
            assert_eq!(resource_of(name).as_deref(), Some(resource), "{name:?}");
        }
        assert!(resource_directory_name("").is_err());
        assert!(resource_directory_name(&"a".repeat(256)).is_err());
        assert!(resource_directory_name(&"\u{e9}".repeat(43)).is_err());
        for not_written in ["Turn", "%2ehidden", "%", "%zz", "con", "%74urn", "%C3", ""] {
            assert_eq!(resource_of(not_written), None, "{not_written:?}");
        }
    }

    #[test]
    fn two_resources_never_share_a_directory_under_any_folding() {
        let names: Vec<String> = [
            "Turn",
            "turn",
            "TURN",
            "caf\u{e9}",
            "cafe\u{301}",
            "con",
            "CON",
        ]
        .into_iter()
        .map(|resource| resource_directory_name(resource).unwrap())
        .collect();
        let folded: BTreeSet<String> = names.iter().map(|name| name.to_lowercase()).collect();
        assert_eq!(folded.len(), names.len(), "{names:?}");
    }

    /// A layout over `root` with `Session` registered.
    fn layout(root: &Path) -> DocumentLayout {
        DocumentLayout::open(root)
            .unwrap()
            .with_entity("Session", ["head.json"])
            .unwrap()
    }

    #[test]
    fn an_entity_is_registered_once_and_shares_its_directory_with_no_other() {
        let root = tempfile::tempdir().unwrap();
        let layout = layout(root.path())
            .with_entity("Session", ["log.ndjson"])
            .unwrap();
        assert_eq!(
            layout.files().collect::<Vec<_>>(),
            ["head.json", "log.ndjson"]
        );
        assert!(layout
            .document_path(&DocumentId::new("Notes", "one"))
            .is_err());
        assert_eq!(
            layout
                .document_path(&DocumentId::new("Session", "Turn"))
                .unwrap(),
            Path::new("session/%54urn")
        );
        assert!(layout
            .with_entity("My Notes", ["head.json"])
            .unwrap()
            .with_entity("My-Notes", ["head.json"])
            .is_err());
        assert!(DocumentLayout::open(root.path())
            .unwrap()
            .with_entity("", ["head.json"])
            .is_err());
    }

    #[test]
    fn a_document_directory_is_created_once_and_found_after() {
        let root = tempfile::tempdir().unwrap();
        let layout = layout(root.path());
        let document = DocumentId::new("Session", "Turn");
        assert!(layout.document_dir(&document).unwrap().is_none());
        let created = layout
            .create_document_dir(WriteDurability::System, &document)
            .unwrap();
        assert_eq!(created.relative, Path::new("session/%54urn"));
        assert!(root.path().join("session/%54urn").is_dir());
        layout
            .create_document_dir(WriteDurability::System, &document)
            .unwrap();
        assert!(layout.document_dir(&document).unwrap().is_some());
        assert!(layout
            .document_dir(&DocumentId::new("Session", "turn"))
            .unwrap()
            .is_none());
    }

    #[cfg(unix)]
    #[test]
    fn a_link_leaving_the_root_is_not_followed() {
        let root = tempfile::tempdir().unwrap();
        let elsewhere = tempfile::tempdir().unwrap();
        let layout = layout(root.path());
        std::os::unix::fs::symlink(elsewhere.path(), root.path().join("session")).unwrap();
        assert!(layout
            .document_dir(&DocumentId::new("Session", "turn"))
            .is_err());
    }
}
