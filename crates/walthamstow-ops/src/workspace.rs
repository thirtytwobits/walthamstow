//! Copyright (c) Scott A Dixon
//!
//! The workspace Walthamstow's operations run against: its entities' documents, the upgrades
//! that replace them, and recovery of an upgrade a stop interrupted. [`FileWorkspace`] is a
//! workspace's files; [`MemoryWorkspace`] keeps documents in memory, for an engine with no disk
//! and for tests.

use crate::codes;
use std::collections::BTreeMap;
use std::path::{Path, PathBuf};
use std::sync::{Mutex, PoisonError};
use walthamstow_kernel::durability::{write_synced, WriteDurability};
use walthamstow_kernel::KernelError;
use walthamstow_upgrades::publication::{check_relative_path, read_regular};
use walthamstow_upgrades::workspace::{self as upgrades, Recovery};
use walthamstow_workspace::documents::{list_documents, supported_document};

/// The documents a workspace keeps, and the upgrades that replace them.
pub trait Workspace: Send + Sync {
    /// The workspace's root, as the operator names it.
    fn root(&self) -> String;

    /// Each document of `entity`: its path relative to the root, with `/` between components,
    /// and its bytes, in path order.
    fn documents(&self, entity: &str) -> anyhow::Result<Vec<(String, Vec<u8>)>>;

    /// Replaces each document `changes` names with its bytes there, as the upgrade
    /// `operation_id`. Every document must still hold what `sources` says it held when the
    /// upgrade read it, or nothing is replaced and the upgrade is refused with
    /// [`codes::UPGRADE_STALE`]. An unfinished upgrade journalled under `operation_id` refuses
    /// it with [`codes::UPGRADE_PENDING`].
    fn publish(
        &self,
        operation_id: &str,
        sources: &BTreeMap<String, Vec<u8>>,
        changes: &BTreeMap<String, Vec<u8>>,
    ) -> anyhow::Result<()>;

    /// Finishes each upgrade a journal authorised and discards staging no journal authorises.
    fn recover(&self) -> anyhow::Result<Recovery>;
}

fn stale(path: &str) -> anyhow::Error {
    KernelError::conflict(format!("{path} changed after the upgrade read it."))
        .with_data(serde_json::json!({ "path": path }))
        .with_reason(codes::UPGRADE_STALE)
        .into()
}

/// A workspace's files, upgraded through the journals under the engine's data directory.
#[derive(Debug)]
pub struct FileWorkspace {
    root: PathBuf,
    data_dir: PathBuf,
    durability: WriteDurability,
    /// Each entity's directory, resolved.
    directories: BTreeMap<String, PathBuf>,
}

impl FileWorkspace {
    /// The workspace at `root`, upgraded under `data_dir`.
    pub fn new(
        root: impl Into<PathBuf>,
        data_dir: impl Into<PathBuf>,
        durability: WriteDurability,
    ) -> Self {
        Self {
            root: root.into(),
            data_dir: data_dir.into(),
            durability,
            directories: BTreeMap::new(),
        }
    }

    /// Keeps `entity`'s documents in `directory`, which the product resolved with
    /// [`resolve_directories`](walthamstow_workspace::paths::resolve_directories).
    pub fn directory(mut self, entity: impl Into<String>, directory: impl Into<PathBuf>) -> Self {
        self.directories.insert(entity.into(), directory.into());
        self
    }

    fn work(&self, operation_id: &str) -> anyhow::Result<PathBuf> {
        let work = upgrades::work_dir(&self.data_dir, operation_id)?;
        if work.exists() && !upgrades::retire(&work)? {
            return Err(KernelError::conflict(format!(
                "The upgrade {operation_id} is journalled and not finished. Recover the \
                 workspace first."
            ))
            .with_data(serde_json::json!({ "operation_id": operation_id }))
            .with_reason(codes::UPGRADE_PENDING)
            .into());
        }
        Ok(work)
    }
}

/// The path of `file` relative to `root`, its components joined by `/`. A component holding a
/// backslash is refused, since joined it would name another file.
fn document_path(root: &Path, file: &Path) -> anyhow::Result<String> {
    let relative = file.strip_prefix(root)?;
    let mut components = Vec::new();
    for component in relative.components() {
        let std::path::Component::Normal(name) = component else {
            anyhow::bail!("{} does not lie within {}.", file.display(), root.display());
        };
        let Some(name) = name.to_str() else {
            anyhow::bail!("{} is not named in UTF-8.", file.display());
        };
        if name.contains('\\') {
            anyhow::bail!(
                "{} is named with a backslash, which names another file on Windows.",
                file.display()
            );
        }
        components.push(name);
    }
    Ok(components.join("/"))
}

/// `documents` keyed by path.
fn by_path(documents: &BTreeMap<String, Vec<u8>>) -> anyhow::Result<BTreeMap<PathBuf, Vec<u8>>> {
    documents
        .iter()
        .map(|(path, bytes)| {
            let path = PathBuf::from(path);
            check_relative_path(&path)?;
            Ok((path, bytes.clone()))
        })
        .collect()
}

impl Workspace for FileWorkspace {
    fn root(&self) -> String {
        self.root.display().to_string()
    }

    fn documents(&self, entity: &str) -> anyhow::Result<Vec<(String, Vec<u8>)>> {
        let directory = self
            .directories
            .get(entity)
            .ok_or_else(|| anyhow::anyhow!("The workspace keeps no directory for {entity}."))?;
        list_documents(directory)?
            .into_iter()
            .map(|file| Ok((document_path(&self.root, &file)?, read_regular(&file)?)))
            .collect()
    }

    fn publish(
        &self,
        operation_id: &str,
        sources: &BTreeMap<String, Vec<u8>>,
        changes: &BTreeMap<String, Vec<u8>>,
    ) -> anyhow::Result<()> {
        let sources = by_path(sources)?;
        let changes = by_path(changes)?;
        let work = self.work(operation_id)?;
        let prepared = upgrades::prepared_dir(&work);
        for (path, bytes) in &changes {
            write_synced(self.durability, &prepared.join(path), bytes)?;
        }
        let mut operation =
            upgrades::authorise(self.durability, &work, operation_id, &sources, &changes)?;
        let finished = upgrades::finish_first(self.durability, &self.root, &work, &mut operation);
        if let Err(error) = &finished {
            // The first publication checks every file holds its source before it writes one.
            if error
                .downcast_ref::<KernelError>()
                .and_then(KernelError::reason_code)
                == Some(codes::UPGRADE_STALE)
            {
                upgrades::abandon(self.durability, &work)?;
            }
        }
        finished
    }

    fn recover(&self) -> anyhow::Result<Recovery> {
        upgrades::recover(&self.root, &self.data_dir, self.durability)
    }
}

/// Documents kept in memory, each under its path relative to the root.
#[derive(Debug)]
pub struct MemoryWorkspace {
    root: String,
    /// Each entity's directory, relative to the root.
    directories: BTreeMap<String, String>,
    documents: Mutex<BTreeMap<String, Vec<u8>>>,
}

impl MemoryWorkspace {
    /// An empty workspace, its root named `root`.
    pub fn new(root: impl Into<String>) -> Self {
        Self {
            root: root.into(),
            directories: BTreeMap::new(),
            documents: Mutex::new(BTreeMap::new()),
        }
    }

    /// Keeps `entity`'s documents in `directory`, relative to the root.
    pub fn directory(mut self, entity: impl Into<String>, directory: impl Into<String>) -> Self {
        self.directories.insert(entity.into(), directory.into());
        self
    }

    /// Writes `bytes` at `path`, relative to the root, as a person editing the workspace would.
    pub fn write(&self, path: impl Into<String>, bytes: impl Into<Vec<u8>>) {
        self.documents().insert(path.into(), bytes.into());
    }

    /// The bytes at `path`, relative to the root.
    pub fn read(&self, path: &str) -> Option<Vec<u8>> {
        self.documents().get(path).cloned()
    }

    fn documents(&self) -> std::sync::MutexGuard<'_, BTreeMap<String, Vec<u8>>> {
        self.documents
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
    }
}

impl Workspace for MemoryWorkspace {
    fn root(&self) -> String {
        self.root.clone()
    }

    fn documents(&self, entity: &str) -> anyhow::Result<Vec<(String, Vec<u8>)>> {
        let directory = self
            .directories
            .get(entity)
            .ok_or_else(|| anyhow::anyhow!("The workspace keeps no directory for {entity}."))?;
        let prefix = format!("{directory}/");
        Ok(self
            .documents()
            .iter()
            .filter(|(path, _)| path.starts_with(&prefix) && supported_document(Path::new(path)))
            .map(|(path, bytes)| (path.clone(), bytes.clone()))
            .collect())
    }

    fn publish(
        &self,
        _operation_id: &str,
        sources: &BTreeMap<String, Vec<u8>>,
        changes: &BTreeMap<String, Vec<u8>>,
    ) -> anyhow::Result<()> {
        let mut documents = self.documents();
        for path in changes.keys() {
            if documents.get(path) != sources.get(path) {
                return Err(stale(path));
            }
        }
        for (path, bytes) in changes {
            documents.insert(path.clone(), bytes.clone());
        }
        Ok(())
    }

    fn recover(&self) -> anyhow::Result<Recovery> {
        Ok(Recovery::default())
    }
}
