//! Copyright (c) Scott A Dixon
//!
//! Where a product's log documents and their blobs are laid out on disk. Each document is the
//! directory `<root>/<entity directory>/<resource>/`, which the file log store and the file blob
//! store share. This module owns the directory and its rules, so each store takes it rather
//! than restating them.

use std::collections::{BTreeMap, BTreeSet};
use std::fmt;
use std::path::{Path, PathBuf};
use walthamstow_kernel::durability::{create_dir_all_synced, WriteDurability};
use walthamstow_kernel::files;
use walthamstow_kernel::{DocumentId, KernelError, KernelResult};

/// The root a product's log documents are kept under, and the files the log store names in
/// each entity's documents.
#[derive(Debug, Clone)]
pub struct DocumentLayout {
    root: PathBuf,
    /// By entity: the names of the files the log store keeps in each of its documents.
    files: BTreeMap<String, BTreeSet<String>>,
}

impl DocumentLayout {
    /// Documents kept under `root`, which is used as given.
    pub fn new(root: impl Into<PathBuf>) -> Self {
        Self {
            root: root.into(),
            files: BTreeMap::new(),
        }
    }

    /// This layout, with `names` the files the log store keeps in each document of `entity`.
    pub fn with_files<I>(mut self, entity: &str, names: I) -> Self
    where
        I: IntoIterator,
        I::Item: Into<String>,
    {
        self.files
            .entry(entity.to_owned())
            .or_default()
            .extend(names.into_iter().map(Into::into));
        self
    }

    pub fn root(&self) -> &Path {
        &self.root
    }

    /// The name of every file the log store keeps in a document, of any entity.
    pub fn files(&self) -> impl Iterator<Item = &str> {
        self.files
            .values()
            .flat_map(|names| names.iter().map(String::as_str))
    }

    /// The directory holding `entity`'s documents, refused when it is a link.
    pub fn entity_directory(&self, entity: &str) -> KernelResult<PathBuf> {
        let directory = self.root.join(entity_directory_name(entity)?);
        refuse_link(&directory)?;
        Ok(directory)
    }

    /// The directory holding `document`, refused when its resource is not a safe directory
    /// name or when it or the entity's directory is a link. It need not exist.
    pub fn document_directory(&self, document: &DocumentId) -> KernelResult<PathBuf> {
        check_resource(document.resource())?;
        let directory = self
            .entity_directory(document.entity())?
            .join(document.resource());
        refuse_link(&directory)?;
        Ok(directory)
    }

    /// Creates the directory holding `document`, durable as `durability` asks, and returns it.
    /// Refused when the resource matches an existing document's directory compared case-folded,
    /// since a file system that folds case would hold the two as one.
    pub fn create_document_directory(
        &self,
        durability: WriteDurability,
        document: &DocumentId,
    ) -> KernelResult<PathBuf> {
        let directory = self.document_directory(document)?;
        let entity = directory.parent().expect("a document lies in its entity");
        refuse_folded_collision(entity, document.resource())?;
        create_dir_all_synced(durability, &directory)
            .map_err(|error| internal(&directory, error))?;
        Ok(directory)
    }
}

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
/// visible component, with no separator, that Windows allows.
pub fn check_resource(resource: &str) -> KernelResult<()> {
    if files::safe_id(resource).is_err() || !files::windows_allows(resource) {
        return Err(KernelError::invalid_request(format!(
            "The resource {resource:?} is not a safe directory name."
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
        for refused in ["", ".hidden", "a/b", "a\\b", "..", "con", "note.", "what?"] {
            assert!(check_resource(refused).is_err(), "{refused:?}");
        }
    }

    #[test]
    fn a_document_directory_is_root_entity_resource() {
        let layout = DocumentLayout::new("/data");
        assert_eq!(
            layout
                .document_directory(&DocumentId::new("Session", "turn-1"))
                .unwrap(),
            Path::new("/data/session/turn-1")
        );
    }

    #[test]
    fn a_resource_matching_another_document_case_folded_is_refused() {
        let root = tempfile::tempdir().unwrap();
        let layout = DocumentLayout::new(root.path());
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
        layout
            .create_document_directory(durability, &DocumentId::new("Session", "turn-2"))
            .unwrap();
    }

    #[cfg(unix)]
    #[test]
    fn a_linked_entity_or_document_directory_is_refused() {
        let root = tempfile::tempdir().unwrap();
        let elsewhere = tempfile::tempdir().unwrap();
        let layout = DocumentLayout::new(root.path());
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
