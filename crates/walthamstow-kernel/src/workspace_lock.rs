//! Copyright (c) Scott A Dixon
//!
//! One engine owns a workspace. Engines that share work do so through a
//! replication protocol, never through one workspace's files.

use anyhow::{Context, Result};
use fs2::FileExt;
use std::fs::File;
use std::io::ErrorKind;
use std::path::Path;

/// An engine's exclusive hold on its workspace, kept for as long as the
/// engine runs. The lock is taken on the workspace root directory itself, so
/// the workspace gains no file. The operating system releases it when the
/// process exits, however it exits.
#[derive(Debug)]
pub struct WorkspaceLock {
    _directory: File,
}

impl WorkspaceLock {
    /// Takes the workspace rooted at `root_dir`, or refuses when another
    /// engine holds it.
    pub fn acquire(root_dir: &Path) -> Result<Self> {
        let directory = File::open(root_dir)
            .with_context(|| format!("Could not open the workspace {}.", root_dir.display()))?;
        if let Err(error) = FileExt::try_lock_exclusive(&directory) {
            if error.kind() != ErrorKind::WouldBlock {
                return Err(error).with_context(|| {
                    format!("Could not lock the workspace at {}.", root_dir.display())
                });
            }
            return Err(crate::KernelError::conflict(format!(
                "Another engine holds the workspace at {}. Stop it before starting this one.",
                root_dir.display()
            ))
            .with_reason(crate::reason_codes::WORKSPACE_HELD)
            .into());
        }
        Ok(Self {
            _directory: directory,
        })
    }
}
