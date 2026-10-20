//! Copyright (c) Scott A Dixon
//!
//! One engine owns a workspace. Engines that share work do so through a
//! replication protocol, never through one workspace's files.

use anyhow::{Context, Result};
use std::fs::File;
use std::path::Path;

/// An engine's exclusive hold on its workspace, kept for as long as the
/// engine runs. The hold is taken on the workspace root directory itself, so
/// the workspace gains no file. The operating system releases it when the
/// process exits, however it exits.
#[derive(Debug)]
pub struct WorkspaceLock {
    _directory: File,
}

/// Why a hold was not taken.
enum Refusal {
    /// Another hold is taken.
    Held,
    /// The root could not be opened or held.
    Failed(std::io::Error),
}

impl WorkspaceLock {
    /// Takes the workspace rooted at `root_dir`, or refuses when another
    /// engine holds it.
    pub fn acquire(root_dir: &Path) -> Result<Self> {
        match hold(root_dir) {
            Ok(directory) => Ok(Self {
                _directory: directory,
            }),
            Err(Refusal::Held) => Err(crate::KernelError::conflict(format!(
                "Another engine holds the workspace at {}. Stop it before starting this one.",
                root_dir.display()
            ))
            .with_reason(crate::reason_codes::WORKSPACE_HELD)
            .into()),
            Err(Refusal::Failed(error)) => Err(error).with_context(|| {
                format!("Could not hold the workspace at {}.", root_dir.display())
            }),
        }
    }
}

/// The root directory, under an exclusive advisory lock.
#[cfg(not(windows))]
fn hold(root_dir: &Path) -> Result<File, Refusal> {
    use fs2::FileExt;
    let directory = File::open(root_dir).map_err(Refusal::Failed)?;
    match FileExt::try_lock_exclusive(&directory) {
        Ok(()) => Ok(directory),
        Err(error) if error.kind() == std::io::ErrorKind::WouldBlock => Err(Refusal::Held),
        Err(error) => Err(Refusal::Failed(error)),
    }
}

/// The root directory, opened for deletion while sharing everything but deletion, so a
/// second hold fails with a sharing violation and every other open of the root succeeds.
#[cfg(windows)]
fn hold(root_dir: &Path) -> Result<File, Refusal> {
    use std::os::windows::fs::OpenOptionsExt;
    const DELETE: u32 = 0x0001_0000;
    const FILE_SHARE_READ: u32 = 0x0000_0001;
    const FILE_SHARE_WRITE: u32 = 0x0000_0002;
    const FILE_FLAG_BACKUP_SEMANTICS: u32 = 0x0200_0000;
    const ERROR_SHARING_VIOLATION: i32 = 32;
    std::fs::OpenOptions::new()
        .access_mode(DELETE)
        .share_mode(FILE_SHARE_READ | FILE_SHARE_WRITE)
        .custom_flags(FILE_FLAG_BACKUP_SEMANTICS)
        .open(root_dir)
        .map_err(|error| {
            if error.raw_os_error() == Some(ERROR_SHARING_VIOLATION) {
                Refusal::Held
            } else {
                Refusal::Failed(error)
            }
        })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::durability::{write_synced, WriteDurability};

    fn is_held(error: &anyhow::Error) -> bool {
        error
            .downcast_ref::<crate::KernelError>()
            .and_then(crate::KernelError::reason_code)
            == Some(crate::reason_codes::WORKSPACE_HELD)
    }

    #[test]
    fn a_held_workspace_refuses_a_second_hold_until_the_first_is_released() {
        let root = tempfile::tempdir().unwrap();
        let first = WorkspaceLock::acquire(root.path()).unwrap();
        let refused = WorkspaceLock::acquire(root.path()).unwrap_err();
        assert!(is_held(&refused), "{refused:#}");
        drop(first);
        WorkspaceLock::acquire(root.path()).unwrap();
    }

    #[test]
    fn a_held_workspace_stays_readable_and_writable() {
        for durability in [WriteDurability::Device, WriteDurability::System] {
            let root = tempfile::tempdir().unwrap();
            let _held = WorkspaceLock::acquire(root.path()).unwrap();
            for relative in ["top.json", "data/nested.json"] {
                let path = root.path().join(relative);
                write_synced(durability, &path, b"first").unwrap();
                write_synced(durability, &path, b"second").unwrap();
                assert_eq!(std::fs::read(&path).unwrap(), b"second");
            }
            durability.sync_directory(root.path()).unwrap();
            assert_eq!(std::fs::read_dir(root.path()).unwrap().count(), 2);
            std::fs::remove_file(root.path().join("top.json")).unwrap();
        }
    }

    #[test]
    fn a_root_that_does_not_exist_is_refused_as_a_failure() {
        let root = tempfile::tempdir().unwrap();
        let missing = root.path().join("missing");
        let error = WorkspaceLock::acquire(&missing).unwrap_err();
        assert!(!is_held(&error));
    }
}
