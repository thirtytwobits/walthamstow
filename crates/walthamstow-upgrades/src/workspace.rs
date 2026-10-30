//! Copyright (c) Scott A Dixon
//!
//! The journal of a workspace upgrade and its recovery. Each upgrade works in its own
//! directory under the engine's workspace-upgrade journals: `prepared/` holds the files it
//! publishes, `original/` the files they replace, and `operation.json` the journal that
//! authorises publication.

use anyhow::{bail, Result};
use cap_std::fs::Dir;
use serde::{Deserialize, Serialize};
use std::collections::BTreeMap;
use std::path::{Path, PathBuf};
use walthamstow_kernel::durability::{
    create_dir_all_synced, write_synced, write_synced_in, WriteDurability,
};
use walthamstow_kernel::hashing::sha256_hex;
use walthamstow_kernel::journals::Journal;
use walthamstow_kernel::publication::{
    check_publication_paths, check_relative_path, publish, publish_first, read_regular, Replacement,
};
use walthamstow_kernel::reason_codes::{PUBLICATION_PATHS_COLLIDE, PUBLICATION_STALE};
use walthamstow_kernel::KernelError;

/// The journal every workspace upgrade works under.
pub const JOURNAL: Journal = Journal::new("workspace-upgrades");

const JOURNAL_FILE: &str = "operation.json";
const COMMITTING: &str = "committing";
const COMPLETE: &str = "complete";
const PREPARED: &str = "prepared";

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

/// The directory an upgrade works in, named by `operation_id`, which is where its originals
/// are found. An id holds only lowercase ASCII letters, digits, `-` and `_`, so no two ids name
/// one directory on a case-insensitive file system; any other id is refused.
pub fn work_dir(data_dir: &Path, operation_id: &str) -> Result<PathBuf> {
    walthamstow_kernel::files::safe_id(operation_id)?;
    if !operation_id
        .chars()
        .all(|character| matches!(character, 'a'..='z' | '0'..='9' | '-' | '_'))
    {
        bail!("An upgrade's operation id holds only lowercase letters, digits, '-' and '_'.");
    }
    if walthamstow_kernel::files::is_windows_device_name(operation_id) {
        bail!("{operation_id} names a Windows device, which no directory can be named.");
    }
    let work = JOURNAL.unlinked_root(data_dir)?.join(operation_id);
    match std::fs::symlink_metadata(&work) {
        Ok(metadata) if !metadata.is_dir() => bail!(
            "{} is not a directory; an upgrade does not work through a link.",
            work.display()
        ),
        Ok(_) => Ok(work),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(work),
        Err(error) => Err(error.into()),
    }
}

/// Where an upgrade stages the files it publishes.
pub fn prepared_dir(work: &Path) -> PathBuf {
    work.join(PREPARED)
}

/// Refuses `paths` unless each is relative, by normal components alone, and no two differ only
/// in case. Two that do are refused as a conflict with the reason
/// [`UPGRADE_PATHS_COLLIDE`](crate::reason_codes::UPGRADE_PATHS_COLLIDE), naming the second as
/// `path`.
pub fn check_paths<'a>(paths: impl IntoIterator<Item = &'a Path>) -> Result<()> {
    check_publication_paths(paths).map_err(as_upgrade)
}

/// A refusal from the kernel's publication, named by the reason an upgrade promises:
/// [`UPGRADE_STALE`](crate::reason_codes::UPGRADE_STALE) for a stale file and
/// [`UPGRADE_PATHS_COLLIDE`](crate::reason_codes::UPGRADE_PATHS_COLLIDE) for a collision.
fn as_upgrade(error: anyhow::Error) -> anyhow::Error {
    let Some(refusal) = error.downcast_ref::<KernelError>() else {
        return error;
    };
    let reason = match refusal.reason_code() {
        Some(PUBLICATION_STALE) => crate::reason_codes::UPGRADE_STALE,
        Some(PUBLICATION_PATHS_COLLIDE) => crate::reason_codes::UPGRADE_PATHS_COLLIDE,
        _ => return error,
    };
    refusal.clone().with_reason(reason).into()
}

/// The directory at `path`, held as a `Dir`.
fn open(path: &Path) -> std::io::Result<Dir> {
    Dir::open_ambient_dir(path, cap_std::ambient_authority())
}

/// Authorises publishing `changes` over the workspace files they replace, which `sources`
/// holds by path: keeps each replaced file under `original/` and writes the committing journal.
/// A path that does not stay beneath the workspace, or two that differ only in case, refuse the
/// upgrade before anything is written.
pub fn authorise(
    durability: WriteDurability,
    work: &Path,
    operation_id: &str,
    sources: &BTreeMap<PathBuf, Vec<u8>>,
    changes: &BTreeMap<PathBuf, Vec<u8>>,
) -> Result<Operation> {
    check_paths(changes.keys().map(PathBuf::as_path))?;
    create_dir_all_synced(durability, work)?;
    let work_root = open(work)?;
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
            write_synced_in(
                durability,
                &work_root,
                &Path::new("original").join(path),
                bytes,
            )?;
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
/// removes its staging. A file already at its target is left alone, so finishing an
/// interrupted publication completes it.
pub fn finish(
    durability: WriteDurability,
    root: &Path,
    work: &Path,
    operation: &mut Operation,
) -> Result<()> {
    finish_as(durability, root, work, operation, true)
}

/// Publishes an upgrade just authorised, as [`finish`] does, but only while every file it
/// replaces still holds its source. A file at its target refuses it too, with the reason
/// [`UPGRADE_STALE`](crate::reason_codes::UPGRADE_STALE), before any file is written.
pub fn finish_first(
    durability: WriteDurability,
    root: &Path,
    work: &Path,
    operation: &mut Operation,
) -> Result<()> {
    finish_as(durability, root, work, operation, false)
}

fn finish_as(
    durability: WriteDurability,
    root: &Path,
    work: &Path,
    operation: &mut Operation,
    resuming: bool,
) -> Result<()> {
    for path in &operation.paths {
        check_relative_path(path)?;
    }
    let work_root = open(work)?;
    let mut replacements = Vec::with_capacity(operation.paths.len());
    for path in &operation.paths {
        let (Some(source_hash), Some(target_hash)) = (
            operation.source_hashes.get(path),
            operation.target_hashes.get(path),
        ) else {
            bail!("The upgrade journal omits {}.", path.display());
        };
        if let Some(expected) = source_hash {
            let original = read_regular(&work_root, &Path::new("original").join(path))?;
            if sha256_hex(&original) != *expected {
                bail!("Upgrade original checksum mismatch.");
            }
        }
        replacements.push(Replacement {
            path: path.clone(),
            source_hash: source_hash.clone(),
            target_hash: Some(target_hash.clone()),
        });
    }
    if !replacements.is_empty() {
        let (live, staged) = (open(root)?, work_root.open_dir(PREPARED)?);
        if resuming {
            publish(durability, &live, &staged, &replacements)
        } else {
            publish_first(durability, &live, &staged, &replacements)
        }
        .map_err(as_upgrade)?;
    }
    operation.status = COMPLETE.into();
    write_journal(durability, work, operation)?;
    remove_staging(work).map(drop)
}

/// What [`recover`] did, each upgrade named by its operation id, in id order.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Recovery {
    /// The upgrades a journal authorised, which recovery published.
    pub finished: Vec<String>,
    /// The upgrades whose staging no journal authorised, which recovery discarded.
    pub discarded: Vec<String>,
}

/// One upgrade's directory: its operation id, its path and its journal, when it has one.
type Found = (String, PathBuf, Option<Operation>);

/// Each upgrade's directory under `data_dir`, in id order. Anything there but a directory, a
/// directory whose name is not UTF-8, and a journal that cannot be read are refused.
fn operations(data_dir: &Path) -> Result<Vec<Found>> {
    let entries = match std::fs::read_dir(JOURNAL.unlinked_root(data_dir)?) {
        Ok(entries) => entries,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(Vec::new()),
        Err(error) => return Err(error.into()),
    };
    let mut found = Vec::new();
    for entry in entries {
        let entry = entry?;
        let work = entry.path();
        if !entry.file_type()?.is_dir() {
            bail!(
                "{} is not a directory; recovery does not follow a link or open a file there.",
                work.display()
            );
        }
        let Ok(id) = entry.file_name().into_string() else {
            bail!(
                "{} is not named in UTF-8; recovery names each upgrade it finds.",
                work.display()
            );
        };
        let operation = read_journal(&work)?;
        found.push((id, work, operation));
    }
    found.sort_by(|left, right| left.0.cmp(&right.0));
    Ok(found)
}

/// The operation id of each upgrade under `data_dir` a journal authorises and no publication
/// has finished, in id order. [`recover`] finishes them.
pub fn pending(data_dir: &Path) -> Result<Vec<String>> {
    Ok(operations(data_dir)?
        .into_iter()
        .filter(|(_, _, operation)| {
            operation
                .as_ref()
                .is_some_and(|operation| operation.status == COMMITTING)
        })
        .map(|(id, ..)| id)
        .collect())
}

/// Finishes each authorised upgrade and discards staging no journal authorises. An upgrade's
/// directory whose name is not UTF-8 is refused.
pub fn recover(root: &Path, data_dir: &Path, durability: WriteDurability) -> Result<Recovery> {
    let mut recovery = Recovery::default();
    for (id, work, operation) in operations(data_dir)? {
        match operation {
            None => {
                std::fs::remove_dir_all(&work)?;
                recovery.discarded.push(id);
            }
            Some(mut operation) if operation.status == COMMITTING => {
                finish(durability, root, &work, &mut operation)?;
                recovery.finished.push(id);
            }
            Some(operation) => {
                if remove_staging(&work)? && operation.status != COMPLETE {
                    recovery.discarded.push(id);
                }
            }
        }
    }
    Ok(recovery)
}

/// Removes the directory `work` of a completed upgrade, with the originals it replaced, and
/// returns whether it did. An upgrade with no journal, or one still to finish, is left for
/// [`recover`].
pub fn retire(work: &Path) -> Result<bool> {
    if read_journal(work)?.is_none_or(|operation| operation.status != COMPLETE) {
        return Ok(false);
    }
    std::fs::remove_dir_all(work)?;
    Ok(true)
}

/// Removes the directory `work` of an authorised upgrade whose [`finish_first`] refused, which
/// wrote no file, so recovery does not try it again. An upgrade a publication has begun to
/// write is finished by [`recover`], never abandoned. The journal is removed, durably, before
/// the files it names, so an abandonment that stops midway leaves staging no journal
/// authorises, which recovery discards.
pub fn abandon(durability: WriteDurability, work: &Path) -> Result<()> {
    disarm(durability, work)?;
    std::fs::remove_dir_all(work)?;
    Ok(())
}

/// Removes the journal that authorises the upgrade in `work`, durably.
fn disarm(durability: WriteDurability, work: &Path) -> Result<()> {
    match std::fs::remove_file(work.join(JOURNAL_FILE)) {
        Err(error) if error.kind() != std::io::ErrorKind::NotFound => return Err(error.into()),
        _ => {}
    }
    durability.sync_directory(work)?;
    Ok(())
}

/// The journal of the upgrade in `work`, or `None` when it has none.
fn read_journal(work: &Path) -> Result<Option<Operation>> {
    let work = match open(work) {
        Ok(work) => work,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(None),
        Err(error) => return Err(error.into()),
    };
    match work.symlink_metadata(JOURNAL_FILE) {
        Ok(_) => Ok(Some(serde_json::from_slice(&read_regular(
            &work,
            Path::new(JOURNAL_FILE),
        )?)?)),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(None),
        Err(error) => Err(error.into()),
    }
}

fn write_journal(durability: WriteDurability, work: &Path, operation: &Operation) -> Result<()> {
    write_synced(
        durability,
        &work.join(JOURNAL_FILE),
        &serde_json::to_vec_pretty(operation)?,
    )
}

/// Removes the upgrade's staging, and returns whether there was any.
fn remove_staging(work: &Path) -> Result<bool> {
    match std::fs::remove_dir_all(prepared_dir(work)) {
        Ok(()) => Ok(true),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(false),
        Err(error) => Err(error.into()),
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
            let recovery = recover(root, data.path(), DURABILITY).unwrap();
            assert_eq!(recovery.finished, ["authorised"]);
            assert!(recovery.discarded.is_empty());
            for (index, path) in paths.iter().enumerate() {
                assert_eq!(std::fs::read(root.join(path)).unwrap(), targets[index]);
            }
            assert!(!prepared_dir(&work).exists());
            let before = read_files(root).unwrap();
            let again = recover(root, data.path(), DURABILITY).unwrap();
            assert_eq!(again, Recovery::default());
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
        let recovery = recover(workspace.path(), data.path(), DURABILITY).unwrap();
        assert_eq!(recovery, Recovery::default());
        assert_eq!(read_files(workspace.path()).unwrap(), before);
    }

    #[test]
    fn recovery_reports_the_staging_it_discards_but_not_a_finished_upgrade_s_leftovers() {
        let workspace = tempfile::tempdir().unwrap();
        let data = tempfile::tempdir().unwrap();
        for (id, status) in [("uncommitted", "preparing"), ("finished", COMPLETE)] {
            let work = work_dir(data.path(), id).unwrap();
            let operation = Operation {
                operation_id: id.into(),
                status: status.into(),
                paths: vec![],
                source_hashes: BTreeMap::new(),
                target_hashes: BTreeMap::new(),
            };
            write_journal(DURABILITY, &work, &operation).unwrap();
            write_synced(
                DURABILITY,
                &prepared_dir(&work).join("data/first.json"),
                b"staged",
            )
            .unwrap();
        }
        let recovery = recover(workspace.path(), data.path(), DURABILITY).unwrap();
        assert!(recovery.finished.is_empty());
        assert_eq!(recovery.discarded, ["uncommitted"]);
        for id in ["uncommitted", "finished"] {
            assert!(!prepared_dir(&work_dir(data.path(), id).unwrap()).exists());
        }
    }

    #[test]
    fn an_upgrade_naming_two_files_that_differ_only_in_case_is_refused_unjournalled() {
        let data = tempfile::tempdir().unwrap();
        let work = work_dir(data.path(), "colliding").unwrap();
        let changes = BTreeMap::from([
            (PathBuf::from("data/A.json"), b"upper".to_vec()),
            (PathBuf::from("data/a.json"), b"lower".to_vec()),
        ]);
        let refused = authorise(DURABILITY, &work, "colliding", &changes, &changes).unwrap_err();
        assert_eq!(
            refused
                .downcast_ref::<walthamstow_kernel::KernelError>()
                .and_then(walthamstow_kernel::KernelError::reason_code),
            Some(crate::reason_codes::UPGRADE_PATHS_COLLIDE)
        );
        assert!(!work.exists());
        assert!(pending(data.path()).unwrap().is_empty());
    }

    #[test]
    fn only_an_authorised_upgrade_still_to_finish_is_pending() {
        let data = tempfile::tempdir().unwrap();
        for (id, status) in [
            ("authorised", COMMITTING),
            ("finished", COMPLETE),
            ("preparing", "preparing"),
        ] {
            let operation = Operation {
                operation_id: id.into(),
                status: status.into(),
                paths: vec![],
                source_hashes: BTreeMap::new(),
                target_hashes: BTreeMap::new(),
            };
            write_journal(DURABILITY, &work_dir(data.path(), id).unwrap(), &operation).unwrap();
        }
        let unjournalled = work_dir(data.path(), "unjournalled").unwrap();
        write_synced(
            DURABILITY,
            &prepared_dir(&unjournalled).join("a.json"),
            b"a",
        )
        .unwrap();
        assert_eq!(pending(data.path()).unwrap(), ["authorised"]);
    }

    #[test]
    fn only_a_completed_upgrade_is_retired() {
        let data = tempfile::tempdir().unwrap();
        for (id, status, retired) in [
            ("finished", COMPLETE, true),
            ("authorised", COMMITTING, false),
        ] {
            let work = work_dir(data.path(), id).unwrap();
            let operation = Operation {
                operation_id: id.into(),
                status: status.into(),
                paths: vec![],
                source_hashes: BTreeMap::new(),
                target_hashes: BTreeMap::new(),
            };
            write_journal(DURABILITY, &work, &operation).unwrap();
            assert_eq!(retire(&work).unwrap(), retired, "{id}");
            assert_eq!(work.exists(), !retired, "{id}");
        }
        let unjournalled = work_dir(data.path(), "unjournalled").unwrap();
        write_synced(
            DURABILITY,
            &prepared_dir(&unjournalled).join("a.json"),
            b"a",
        )
        .unwrap();
        assert!(!retire(&unjournalled).unwrap());
        assert!(unjournalled.exists());
    }

    #[test]
    fn an_abandonment_that_stops_after_disarming_its_journal_is_discarded_by_recovery() {
        let workspace = tempfile::tempdir().unwrap();
        let data = tempfile::tempdir().unwrap();
        let path = PathBuf::from("data/first.json");
        write_synced(DURABILITY, &workspace.path().join(&path), b"edited").unwrap();
        let work = work_dir(data.path(), "abandoned").unwrap();
        write_synced(DURABILITY, &prepared_dir(&work).join(&path), b"upgraded").unwrap();
        authorise(
            DURABILITY,
            &work,
            "abandoned",
            &BTreeMap::from([(path.clone(), b"original".to_vec())]),
            &BTreeMap::from([(path.clone(), b"upgraded".to_vec())]),
        )
        .unwrap();
        disarm(DURABILITY, &work).unwrap();
        std::fs::remove_dir_all(work.join("original")).unwrap();
        let recovery = recover(workspace.path(), data.path(), DURABILITY).unwrap();
        assert_eq!(recovery.discarded, ["abandoned"]);
        assert!(!work.exists());
        assert_eq!(
            std::fs::read(workspace.path().join(&path)).unwrap(),
            b"edited"
        );
    }

    // APFS and NTFS refuse names that are not UTF-8.
    #[cfg(target_os = "linux")]
    #[test]
    fn recovery_refuses_an_upgrade_it_cannot_name_and_leaves_it_alone() {
        use std::os::unix::ffi::OsStrExt;
        let workspace = tempfile::tempdir().unwrap();
        let data = tempfile::tempdir().unwrap();
        let unnamed = JOURNAL
            .root(data.path())
            .join(std::ffi::OsStr::from_bytes(b"\xff"));
        std::fs::create_dir_all(prepared_dir(&unnamed)).unwrap();
        assert!(recover(workspace.path(), data.path(), DURABILITY).is_err());
        assert!(prepared_dir(&unnamed).exists());
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
        let recovery = recover(workspace.path(), data.path(), DURABILITY).unwrap();
        assert_eq!(recovery.discarded, ["abandoned"]);
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
    fn an_operation_id_that_case_could_alias_is_refused() {
        let data = tempfile::tempdir().unwrap();
        for aliasable in ["Upgrade", "UPGRADE", "upgrade-É"] {
            assert!(work_dir(data.path(), aliasable).is_err(), "{aliasable}");
        }
        for id in [
            "upgrade",
            "6f1c0e2a-9b3d-4c57-8a1e-2f4b6d8c0a13",
            "an_upgrade",
        ] {
            let work = work_dir(data.path(), id).unwrap();
            assert_eq!(work.file_name().and_then(|name| name.to_str()), Some(id));
        }
    }

    #[cfg(unix)]
    #[test]
    fn a_journal_that_cannot_be_read_stops_recovery() {
        let workspace = tempfile::tempdir().unwrap();
        let data = tempfile::tempdir().unwrap();
        std::fs::write(data.path().join("journals"), b"not a directory").unwrap();
        assert!(recover(workspace.path(), data.path(), DURABILITY).is_err());
        let empty = tempfile::tempdir().unwrap();
        recover(workspace.path(), empty.path(), DURABILITY).unwrap();
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

    #[test]
    fn an_operation_id_that_names_a_windows_device_is_refused() {
        let data = tempfile::tempdir().unwrap();
        for device in ["con", "prn", "aux", "nul", "com1", "com9", "lpt1", "lpt9"] {
            assert!(work_dir(data.path(), device).is_err(), "{device}");
        }
        for ordinary in ["console", "com", "com0", "lpt10", "null"] {
            assert!(work_dir(data.path(), ordinary).is_ok(), "{ordinary}");
        }
    }

    #[cfg(unix)]
    #[test]
    fn recovery_refuses_a_linked_operation_and_leaves_its_target_alone() {
        let workspace = tempfile::tempdir().unwrap();
        let data = tempfile::tempdir().unwrap();
        let outside = tempfile::tempdir().unwrap();
        write_synced(
            DURABILITY,
            &prepared_dir(outside.path()).join("data/first.json"),
            b"x",
        )
        .unwrap();
        std::fs::create_dir_all(JOURNAL.root(data.path())).unwrap();
        std::os::unix::fs::symlink(outside.path(), JOURNAL.root(data.path()).join("linked"))
            .unwrap();
        assert!(recover(workspace.path(), data.path(), DURABILITY).is_err());
        assert!(prepared_dir(outside.path())
            .join("data/first.json")
            .exists());
    }

    #[cfg(unix)]
    #[test]
    fn a_linked_operation_journal_is_refused_unread() {
        let workspace = tempfile::tempdir().unwrap();
        let data = tempfile::tempdir().unwrap();
        let outside = tempfile::tempdir().unwrap();
        let work = work_dir(data.path(), "linked-journal").unwrap();
        std::fs::create_dir_all(&work).unwrap();
        std::fs::write(outside.path().join(JOURNAL_FILE), b"{}").unwrap();
        std::os::unix::fs::symlink(outside.path().join(JOURNAL_FILE), work.join(JOURNAL_FILE))
            .unwrap();
        let error = recover(workspace.path(), data.path(), DURABILITY).unwrap_err();
        assert!(error.to_string().contains("not a regular file"), "{error}");
    }

    #[test]
    fn a_recovered_path_is_checked_before_its_original_is_read() {
        let workspace = tempfile::tempdir().unwrap();
        let data = tempfile::tempdir().unwrap();
        let work = work_dir(data.path(), "escaping").unwrap();
        let path = PathBuf::from("/walthamstow-absent/escape.json");
        let mut operation = Operation {
            operation_id: "escaping".into(),
            status: COMMITTING.into(),
            paths: vec![path.clone()],
            source_hashes: BTreeMap::from([(path.clone(), Some(sha256_hex(b"original")))]),
            target_hashes: BTreeMap::from([(path, sha256_hex(b"target"))]),
        };
        let error = finish(DURABILITY, workspace.path(), &work, &mut operation).unwrap_err();
        assert!(
            error.to_string().contains("Invalid publication path"),
            "{error}"
        );
    }

    #[cfg(unix)]
    #[test]
    fn a_linked_upgrade_journal_is_neither_recovered_nor_written() {
        let workspace = tempfile::tempdir().unwrap();
        let outside = tempfile::tempdir().unwrap();
        for linked in ["journals", "journals/workspace-upgrades"] {
            let data = tempfile::tempdir().unwrap();
            let link = data.path().join(linked);
            std::fs::create_dir_all(link.parent().unwrap()).unwrap();
            std::os::unix::fs::symlink(outside.path(), &link).unwrap();
            assert!(
                recover(workspace.path(), data.path(), DURABILITY).is_err(),
                "{linked}"
            );
            assert!(work_dir(data.path(), "an-upgrade").is_err(), "{linked}");
        }
    }

    #[cfg(unix)]
    #[test]
    fn a_linked_operation_directory_is_not_worked_in() {
        let data = tempfile::tempdir().unwrap();
        let outside = tempfile::tempdir().unwrap();
        let root = JOURNAL.root(data.path());
        std::fs::create_dir_all(&root).unwrap();
        std::os::unix::fs::symlink(outside.path(), root.join("an-upgrade")).unwrap();
        assert!(work_dir(data.path(), "an-upgrade").is_err());
    }

    #[cfg(unix)]
    #[test]
    fn originals_are_neither_kept_nor_read_through_a_link() {
        let data = tempfile::tempdir().unwrap();
        let outside = tempfile::tempdir().unwrap();
        let work = work_dir(data.path(), "linked-originals").unwrap();
        std::fs::create_dir_all(&work).unwrap();
        std::os::unix::fs::symlink(outside.path(), work.join("original")).unwrap();
        let path = PathBuf::from("data/first.json");
        let refused = authorise(
            DURABILITY,
            &work,
            "linked-originals",
            &BTreeMap::from([(path.clone(), b"original".to_vec())]),
            &BTreeMap::from([(path.clone(), b"upgraded".to_vec())]),
        );
        assert!(refused.is_err());
        assert_eq!(std::fs::read_dir(outside.path()).unwrap().count(), 0);

        write_synced(DURABILITY, &outside.path().join(&path), b"original").unwrap();
        let workspace = tempfile::tempdir().unwrap();
        let mut operation = Operation {
            operation_id: "linked-originals".into(),
            status: COMMITTING.into(),
            paths: vec![path.clone()],
            source_hashes: BTreeMap::from([(path.clone(), Some(sha256_hex(b"original")))]),
            target_hashes: BTreeMap::from([(path, sha256_hex(b"upgraded"))]),
        };
        assert!(finish(DURABILITY, workspace.path(), &work, &mut operation).is_err());
    }

    #[test]
    fn a_file_changed_since_the_upgrade_read_it_is_refused_as_upgrade_stale() {
        let workspace = tempfile::tempdir().unwrap();
        let data = tempfile::tempdir().unwrap();
        let work = work_dir(data.path(), "stale").unwrap();
        let path = PathBuf::from("data/first.json");
        write_synced(DURABILITY, &workspace.path().join(&path), b"edited").unwrap();
        write_synced(DURABILITY, &prepared_dir(&work).join(&path), b"upgraded").unwrap();
        let mut operation = authorise(
            DURABILITY,
            &work,
            "stale",
            &BTreeMap::from([(path.clone(), b"original".to_vec())]),
            &BTreeMap::from([(path.clone(), b"upgraded".to_vec())]),
        )
        .unwrap();
        let refused =
            finish_first(DURABILITY, workspace.path(), &work, &mut operation).unwrap_err();
        let refused = refused.downcast_ref::<KernelError>().unwrap();
        assert_eq!(
            refused.reason_code(),
            Some(crate::reason_codes::UPGRADE_STALE)
        );
        assert_eq!(refused.data.as_ref().unwrap()["path"], "data/first.json");
        assert_eq!(
            std::fs::read(workspace.path().join(&path)).unwrap(),
            b"edited"
        );
    }

    #[cfg(unix)]
    #[test]
    fn a_linked_staging_directory_is_not_published() {
        let workspace = tempfile::tempdir().unwrap();
        let data = tempfile::tempdir().unwrap();
        let outside = tempfile::tempdir().unwrap();
        let work = work_dir(data.path(), "linked-staging").unwrap();
        let path = PathBuf::from("data/first.json");
        write_synced(DURABILITY, &workspace.path().join(&path), b"original").unwrap();
        write_synced(DURABILITY, &outside.path().join(&path), b"upgraded").unwrap();
        let mut operation = authorise(
            DURABILITY,
            &work,
            "linked-staging",
            &BTreeMap::from([(path.clone(), b"original".to_vec())]),
            &BTreeMap::from([(path.clone(), b"upgraded".to_vec())]),
        )
        .unwrap();
        std::os::unix::fs::symlink(outside.path(), prepared_dir(&work)).unwrap();
        assert!(finish(DURABILITY, workspace.path(), &work, &mut operation).is_err());
        assert_eq!(
            std::fs::read(workspace.path().join(&path)).unwrap(),
            b"original"
        );
    }
}
