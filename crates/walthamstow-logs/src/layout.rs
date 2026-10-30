//! Copyright (c) Scott A Dixon
//!
//! Where a product's log documents and their blobs are laid out on disk. Each document is the
//! directory `<root>/<entity directory>/<resource>/`, which the file log store and the file blob
//! store share. This module owns the directory and its rules, so each store takes it rather
//! than restating them. The product registers each entity, and two whose names map to one
//! directory are refused, so no two documents share a directory.

use std::collections::{BTreeMap, BTreeSet};
use std::fmt;
use std::path::{Path, PathBuf};
use std::sync::{Mutex, PoisonError};
use walthamstow_kernel::durability::{create_dir_all_synced, WriteDurability};
use walthamstow_kernel::files;
use walthamstow_kernel::{DocumentId, KernelError, KernelResult};

/// The root a product's log documents are kept under, and the entities it registers.
#[derive(Debug, Clone)]
pub struct DocumentLayout {
    root: PathBuf,
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

impl DocumentLayout {
    /// Documents kept under `root`, which is used as given, of no entity yet.
    pub fn new(root: impl Into<PathBuf>) -> Self {
        Self {
            root: root.into(),
            entities: BTreeMap::new(),
        }
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

    pub fn root(&self) -> &Path {
        &self.root
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

    /// The directory holding `entity`'s documents, refused when the entity is not registered or
    /// the directory is a link.
    pub fn entity_directory(&self, entity: &str) -> KernelResult<PathBuf> {
        let directory = self.root.join(&self.entity(entity)?.directory);
        refuse_link(&directory)?;
        Ok(directory)
    }

    /// The directory holding `document`, which need not exist. Refused when the resource is not
    /// a safe directory name, when it matches another document's directory compared case-folded,
    /// since a file system that folds case would hold the two as one, or when it or the entity's
    /// directory is a link.
    pub fn document_directory(&self, document: &DocumentId) -> KernelResult<PathBuf> {
        check_resource(document.resource())?;
        let entity = self.entity_directory(document.entity())?;
        refuse_folded_collision(&entity, document.resource())?;
        let directory = entity.join(document.resource());
        refuse_link(&directory)?;
        Ok(directory)
    }

    /// Creates the directory holding `document`, durable as `durability` asks, and returns it.
    /// The collision check and the creation run under one lock, so of two spellings of a
    /// resource created at once only the first is accepted.
    pub fn create_document_directory(
        &self,
        durability: WriteDurability,
        document: &DocumentId,
    ) -> KernelResult<PathBuf> {
        let _creating = CREATING.lock().unwrap_or_else(PoisonError::into_inner);
        let directory = self.document_directory(document)?;
        create_dir_all_synced(durability, &directory)
            .map_err(|error| internal(&directory, error))?;
        Ok(directory)
    }
}

/// Held from a document directory's collision check to its creation. One engine owns a
/// workspace, so a lock within the process is enough.
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

/// Refuses `resource` unless it is a safe directory name on every supported platform: one
/// visible component in ASCII, with no separator, that Windows allows. ASCII, because a file
/// system may fold two spellings of one name into one entry, by case or by Unicode
/// normalisation, and comparing names case-folded catches every such fold only in ASCII.
pub fn check_resource(resource: &str) -> KernelResult<()> {
    if !resource.is_ascii() || files::safe_id(resource).is_err() || !files::windows_allows(resource)
    {
        return Err(KernelError::invalid_request(format!(
            "The resource {resource:?} is not a safe directory name in ASCII."
        )));
    }
    Ok(())
}

/// Refuses `resource` when another entry of `entity` matches it compared case-folded.
fn refuse_folded_collision(entity: &Path, resource: &str) -> KernelResult<()> {
    let entries = match std::fs::read_dir(entity) {
        Ok(entries) => entries,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(()),
        Err(error) => return Err(internal(entity, error)),
    };
    let folded = resource.to_lowercase();
    for entry in entries {
        let entry = entry.map_err(|error| internal(entity, error))?;
        let name = entry.file_name();
        let Some(name) = name.to_str() else {
            continue;
        };
        if name != resource && name.to_lowercase() == folded {
            return Err(KernelError::invalid_request(format!(
                "The resource {resource:?} matches the document directory {name:?} compared \
                 case-folded."
            )));
        }
    }
    Ok(())
}

/// Refuses `path` when it is a link, which the stores do not follow.
pub(crate) fn refuse_link(path: &Path) -> KernelResult<()> {
    match std::fs::symlink_metadata(path) {
        Ok(metadata) if metadata.file_type().is_symlink() => Err(internal(
            path,
            "this is a link, which the store does not follow",
        )),
        Ok(_) => Ok(()),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(()),
        Err(error) => Err(internal(path, error)),
    }
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
    fn a_resource_is_one_safe_visible_directory_name() {
        check_resource("turn-1").unwrap();
        for refused in [
            "",
            ".hidden",
            "a/b",
            "a\\b",
            "..",
            "con",
            "note.",
            "what?",
            "caf\u{e9}",
            "cafe\u{301}",
        ] {
            assert!(check_resource(refused).is_err(), "{refused:?}");
        }
    }

    /// A layout with `Session` registered.
    fn layout(root: impl Into<PathBuf>) -> DocumentLayout {
        DocumentLayout::new(root)
            .with_entity("Session", ["head.json"])
            .unwrap()
    }

    #[test]
    fn a_document_directory_is_root_entity_resource() {
        assert_eq!(
            layout("/data")
                .document_directory(&DocumentId::new("Session", "turn-1"))
                .unwrap(),
            Path::new("/data/session/turn-1")
        );
    }

    #[test]
    fn an_entity_is_registered_once_and_shares_its_directory_with_no_other() {
        let layout = layout("/data")
            .with_entity("Session", ["log.ndjson"])
            .unwrap();
        assert_eq!(
            layout.files().collect::<Vec<_>>(),
            ["head.json", "log.ndjson"]
        );
        assert!(layout
            .document_directory(&DocumentId::new("Notes", "one"))
            .is_err());
        assert!(layout
            .clone()
            .with_entity("My Notes", ["head.json"])
            .unwrap()
            .with_entity("My-Notes", ["head.json"])
            .is_err());
        assert!(layout.with_entity("", ["head.json"]).is_err());
    }

    #[test]
    fn a_resource_matching_another_document_case_folded_is_refused() {
        let root = tempfile::tempdir().unwrap();
        let layout = layout(root.path());
        let durability = WriteDurability::System;
        layout
            .create_document_directory(durability, &DocumentId::new("Session", "Turn"))
            .unwrap();
        layout
            .create_document_directory(durability, &DocumentId::new("Session", "Turn"))
            .unwrap();
        assert!(layout
            .create_document_directory(durability, &DocumentId::new("Session", "turn"))
            .is_err());
        assert!(layout
            .document_directory(&DocumentId::new("Session", "turn"))
            .is_err());
        layout
            .create_document_directory(durability, &DocumentId::new("Session", "turn-2"))
            .unwrap();
    }

    #[cfg(unix)]
    #[test]
    fn a_linked_entity_or_document_directory_is_refused() {
        let root = tempfile::tempdir().unwrap();
        let elsewhere = tempfile::tempdir().unwrap();
        let layout = layout(root.path());
        std::os::unix::fs::symlink(elsewhere.path(), root.path().join("session")).unwrap();
        assert!(layout
            .document_directory(&DocumentId::new("Session", "turn"))
            .is_err());
        std::fs::remove_file(root.path().join("session")).unwrap();
        std::fs::create_dir(root.path().join("session")).unwrap();
        std::os::unix::fs::symlink(elsewhere.path(), root.path().join("session/turn")).unwrap();
        assert!(layout
            .document_directory(&DocumentId::new("Session", "turn"))
            .is_err());
    }
}
