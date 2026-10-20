//! Copyright (c) Scott A Dixon
//!
//! The journal of a workspace upgrade and its recovery. Each upgrade works in its own
//! directory under the engine's workspace-upgrade journals: `prepared/` holds the files it
//! publishes, `original/` the files they replace, and `operation.json` the journal that
//! authorises publication.

use crate::publication::{check_relative_path, publish, Replacement};
use anyhow::{bail, Result};
use serde::{Deserialize, Serialize};
use std::collections::BTreeMap;
use std::path::{Path, PathBuf};
use walthamstow_kernel::durability::{write_synced, WriteDurability};
use walthamstow_kernel::hashing::sha256_hex;
use walthamstow_kernel::journals::Journal;

/// The journal every workspace upgrade works under.
pub const JOURNAL: Journal = Journal::new("workspace-upgrades");

const JOURNAL_FILE: &str = "operation.json";
const COMMITTING: &str = "committing";
const COMPLETE: &str = "complete";

/// One workspace upgrade's journal.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Operation {
    pub operation_id: String,
    status: String,
    /// The workspace files the upgrade changes, relative to the workspace root.
    pub paths: Vec<PathBuf>,
    source_hashes: BTreeMap<PathBuf, Option<String>>,
    target_hashes: BTreeMap<PathBuf, String>,
}

/// The directory an upgrade works in, named by a hash of `operation_id`, so ids that differ
/// only in case keep directories of their own on a case-insensitive file system. An
/// `operation_id` that is not one visible path component is refused.
pub fn work_dir(data_dir: &Path, operation_id: &str) -> Result<PathBuf> {
    walthamstow_kernel::files::safe_id(operation_id)?;
    Ok(JOURNAL
        .root(data_dir)
        .join(sha256_hex(operation_id.as_bytes())))
}

/// Where an upgrade stages the files it publishes.
pub fn prepared_dir(work: &Path) -> PathBuf {
    work.join("prepared")
}

/// Authorises publishing `changes` over the workspace files they replace, which `sources`
/// holds by path: keeps each replaced file under `original/` and writes the committing journal.
/// A path that does not stay beneath the workspace refuses the upgrade before anything is
/// written.
pub fn authorise(
    durability: WriteDurability,
    work: &Path,
    operation_id: &str,
    sources: &BTreeMap<PathBuf, Vec<u8>>,
    changes: &BTreeMap<PathBuf, Vec<u8>>,
) -> Result<Operation> {
    for path in changes.keys() {
        check_relative_path(path)?;
    }
    let mut operation = Operation {
        operation_id: operation_id.to_owned(),
        status: COMMITTING.into(),
        paths: changes.keys().cloned().collect(),
        source_hashes: BTreeMap::new(),
        target_hashes: BTreeMap::new(),
    };
    for (path, bytes) in changes {
        let original = sources.get(path);
        if let Some(bytes) = original {
            write_synced(durability, &work.join("original").join(path), bytes)?;
        }
        operation
            .source_hashes
            .insert(path.clone(), original.map(|bytes| sha256_hex(bytes)));
        operation
            .target_hashes
            .insert(path.clone(), sha256_hex(bytes));
    }
    write_journal(durability, work, &operation)?;
    Ok(operation)
}

/// Publishes an authorised upgrade into the workspace at `root`, marks it complete and
/// removes its staging.
pub fn finish(
    durability: WriteDurability,
    root: &Path,
    work: &Path,
    operation: &mut Operation,
) -> Result<()> {
    let mut replacements = Vec::with_capacity(operation.paths.len());
    for path in &operation.paths {
        let (Some(source_hash), Some(target_hash)) = (
            operation.source_hashes.get(path),
            operation.target_hashes.get(path),
        ) else {
            bail!("The upgrade journal omits {}.", path.display());
        };
        if let Some(expected) = source_hash {
            let original = std::fs::read(work.join("original").join(path))?;
            if sha256_hex(&original) != *expected {
                bail!("Upgrade original checksum mismatch.");
            }
        }
        replacements.push(Replacement {
            path: path.clone(),
            source_hash: source_hash.clone(),
            target_hash: target_hash.clone(),
        });
    }
    publish(durability, root, &prepared_dir(work), &replacements)?;
    operation.status = COMPLETE.into();
    write_journal(durability, work, operation)?;
    remove_staging(work)
}

/// Finishes each authorised upgrade and discards staging no journal authorises.
pub fn recover(root: &Path, data_dir: &Path, durability: WriteDurability) -> Result<()> {
    let operations = JOURNAL.root(data_dir);
    if !operations.exists() {
        return Ok(());
    }
    for entry in std::fs::read_dir(operations)? {
        let work = entry?.path();
        let path = work.join(JOURNAL_FILE);
        if !path.exists() {
            std::fs::remove_dir_all(&work)?;
            continue;
        }
        let mut operation: Operation = serde_json::from_slice(&std::fs::read(path)?)?;
        if operation.status == COMMITTING {
            finish(durability, root, &work, &mut operation)?;
        } else {
            remove_staging(&work)?;
        }
    }
    Ok(())
}

fn write_journal(durability: WriteDurability, work: &Path, operation: &Operation) -> Result<()> {
    write_synced(
        durability,
        &work.join(JOURNAL_FILE),
        &serde_json::to_vec_pretty(operation)?,
    )
}

fn remove_staging(work: &Path) -> Result<()> {
    match std::fs::remove_dir_all(prepared_dir(work)) {
        Err(error) if error.kind() != std::io::ErrorKind::NotFound => Err(error.into()),
        _ => Ok(()),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use walthamstow_kernel::files::read_files;

    const DURABILITY: WriteDurability = WriteDurability::System;

    #[test]
    fn authorised_workspace_upgrade_recovers_each_partial_publication_and_clears_its_staging() {
        for published in 0..=2 {
            let workspace = tempfile::tempdir().unwrap();
            let data = tempfile::tempdir().unwrap();
            let root = workspace.path();
            let work = work_dir(data.path(), "authorised").unwrap();
            let paths = [
                PathBuf::from("data/first.json"),
                PathBuf::from("data/second.json"),
            ];
            let originals = [b"original first".as_slice(), b"original second".as_slice()];
            let targets = [b"upgraded first".as_slice(), b"upgraded second".as_slice()];
            let sources = paths.iter().cloned().zip(originals.map(<[u8]>::to_vec));
            let changes = paths.iter().cloned().zip(targets.map(<[u8]>::to_vec));
            for (index, path) in paths.iter().enumerate() {
                write_synced(DURABILITY, &prepared_dir(&work).join(path), targets[index]).unwrap();
                write_synced(
                    DURABILITY,
                    &root.join(path),
                    if index < published {
                        targets[index]
                    } else {
                        originals[index]
                    },
                )
                .unwrap();
            }
            authorise(
                DURABILITY,
                &work,
                "authorised",
                &sources.collect(),
                &changes.collect(),
            )
            .unwrap();
            recover(root, data.path(), DURABILITY).unwrap();
            for (index, path) in paths.iter().enumerate() {
                assert_eq!(std::fs::read(root.join(path)).unwrap(), targets[index]);
            }
            assert!(!prepared_dir(&work).exists());
            let before = read_files(root).unwrap();
            recover(root, data.path(), DURABILITY).unwrap();
            assert_eq!(read_files(root).unwrap(), before);
        }
    }

    #[test]
    fn workspace_recovery_does_not_start_an_uncommitted_upgrade() {
        let workspace = tempfile::tempdir().unwrap();
        let data = tempfile::tempdir().unwrap();
        let work = work_dir(data.path(), "uncommitted").unwrap();
        let operation = Operation {
            operation_id: "uncommitted".into(),
            status: "preparing".into(),
            paths: vec![],
            source_hashes: BTreeMap::new(),
            target_hashes: BTreeMap::new(),
        };
        write_journal(DURABILITY, &work, &operation).unwrap();
        std::fs::write(workspace.path().join("authored.json"), b"authored").unwrap();
        let before = read_files(workspace.path()).unwrap();
        recover(workspace.path(), data.path(), DURABILITY).unwrap();
        assert_eq!(read_files(workspace.path()).unwrap(), before);
    }

    #[test]
    fn workspace_recovery_discards_staging_that_no_journal_authorises() {
        let workspace = tempfile::tempdir().unwrap();
        let data = tempfile::tempdir().unwrap();
        let work = work_dir(data.path(), "abandoned").unwrap();
        write_synced(
            DURABILITY,
            &prepared_dir(&work).join("data/first.json"),
            b"staged",
        )
        .unwrap();
        recover(workspace.path(), data.path(), DURABILITY).unwrap();
        assert!(!work.exists());
    }

    #[test]
    fn a_path_that_leaves_the_workspace_refuses_the_upgrade_before_anything_is_written() {
        let data = tempfile::tempdir().unwrap();
        let work = work_dir(data.path(), "escaping").unwrap();
        for path in ["../../escape.json", "/absolute.json"] {
            let path = PathBuf::from(path);
            let bytes = b"bytes".to_vec();
            assert!(authorise(
                DURABILITY,
                &work,
                "escaping",
                &BTreeMap::from([(path.clone(), bytes.clone())]),
                &BTreeMap::from([(path.clone(), bytes)]),
            )
            .is_err());
        }
        assert!(
            !JOURNAL.root(data.path()).exists(),
            "nothing is written for a refused upgrade"
        );
    }

    #[test]
    fn an_operation_id_that_is_not_one_path_component_is_refused() {
        let data = tempfile::tempdir().unwrap();
        for operation_id in ["", "../../other", "nested/id", "/absolute", ".hidden", ".."] {
            assert!(
                work_dir(data.path(), operation_id).is_err(),
                "{operation_id:?}"
            );
        }
        let work = work_dir(data.path(), "an-upgrade").unwrap();
        assert_eq!(work.parent(), Some(JOURNAL.root(data.path()).as_path()));
    }

    #[test]
    fn ids_that_differ_only_in_case_work_in_directories_of_their_own() {
        let data = tempfile::tempdir().unwrap();
        let upper = work_dir(data.path(), "Upgrade").unwrap();
        let lower = work_dir(data.path(), "upgrade").unwrap();
        assert_ne!(
            upper.to_string_lossy().to_lowercase(),
            lower.to_string_lossy().to_lowercase()
        );
    }

    #[test]
    fn an_upgrade_in_a_directory_named_by_its_id_is_recovered() {
        let workspace = tempfile::tempdir().unwrap();
        let data = tempfile::tempdir().unwrap();
        let work = JOURNAL.root(data.path()).join("named-by-its-id");
        let path = PathBuf::from("data/first.json");
        let (original, target) = (b"original".to_vec(), b"upgraded".to_vec());
        write_synced(DURABILITY, &workspace.path().join(&path), &original).unwrap();
        write_synced(DURABILITY, &prepared_dir(&work).join(&path), &target).unwrap();
        authorise(
            DURABILITY,
            &work,
            "named-by-its-id",
            &BTreeMap::from([(path.clone(), original)]),
            &BTreeMap::from([(path.clone(), target.clone())]),
        )
        .unwrap();
        recover(workspace.path(), data.path(), DURABILITY).unwrap();
        assert_eq!(std::fs::read(workspace.path().join(&path)).unwrap(), target);
    }

    #[test]
    fn a_tampered_original_stops_the_publication() {
        let workspace = tempfile::tempdir().unwrap();
        let data = tempfile::tempdir().unwrap();
        let work = work_dir(data.path(), "tampered").unwrap();
        let path = PathBuf::from("data/first.json");
        let (original, target) = (b"original".to_vec(), b"upgraded".to_vec());
        write_synced(DURABILITY, &workspace.path().join(&path), &original).unwrap();
        write_synced(DURABILITY, &prepared_dir(&work).join(&path), &target).unwrap();
        let mut operation = authorise(
            DURABILITY,
            &work,
            "tampered",
            &BTreeMap::from([(path.clone(), original.clone())]),
            &BTreeMap::from([(path.clone(), target)]),
        )
        .unwrap();
        write_synced(DURABILITY, &work.join("original").join(&path), b"altered").unwrap();
        assert!(finish(DURABILITY, workspace.path(), &work, &mut operation).is_err());
        assert_eq!(
            std::fs::read(workspace.path().join(&path)).unwrap(),
            original
        );
    }
}
