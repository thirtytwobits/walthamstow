//! Copyright (c) Scott A Dixon
//!
//! The lease an operation runs under. One engine holds a workspace: a daemon for as long as it
//! runs, or an in-process front end for one operation at a time, which is refused with
//! `workspace_held` while another engine holds it. Within an engine, operations on the
//! workspace run one at a time.

use crate::codes;
use serde::{Deserialize, Serialize};
use std::fmt;
use std::path::PathBuf;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use walthamstow_kernel::leases::FileLeases;
use walthamstow_kernel::KernelError;

/// Which engine holds the workspace while an operation runs.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, schemars::JsonSchema)]
#[serde(rename_all = "snake_case")]
pub enum Holder {
    /// The daemon, for as long as it runs.
    Daemon,
    /// The process running the operation, for the operation alone.
    Process,
}

type Take = dyn Fn() -> anyhow::Result<Box<dyn Send>> + Send + Sync;

/// The right to run operations against a workspace.
pub struct Lease {
    holder: Holder,
    /// Takes the workspace for one operation, and holds it until what it returns is dropped.
    take: Box<Take>,
    /// Held by each operation, so operations on the workspace run one at a time.
    serial: Arc<tokio::sync::Mutex<()>>,
}

impl fmt::Debug for Lease {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("Lease")
            .field("holder", &self.holder)
            .finish_non_exhaustive()
    }
}

/// The workspace, held for one operation.
pub(crate) struct Hold {
    _taken: Box<dyn Send>,
    _serial: tokio::sync::OwnedMutexGuard<()>,
}

impl Lease {
    /// A lease for a process that runs operations in-process: each opens the leases over the
    /// workspace rooted at `root`, taking its workspace lock, or is refused while another engine
    /// holds it.
    pub fn in_process(root: impl Into<PathBuf>) -> Self {
        let root = root.into();
        Self::new(Holder::Process, move || {
            Ok(Box::new(FileLeases::open(&root)?) as Box<dyn Send>)
        })
    }

    /// A lease for a process that runs operations in-process over `lock`, a workspace lock in
    /// memory.
    pub fn in_process_memory(lock: MemoryLock) -> Self {
        Self::new(Holder::Process, move || {
            Ok(Box::new(lock.acquire()?) as Box<dyn Send>)
        })
    }

    /// A lease for the daemon, over `held`, the workspace lock it holds for as long as it
    /// runs: the [`FileLeases`] it opened or a [`MemoryHold`].
    pub fn daemon(held: impl Send + Sync + 'static) -> Self {
        let held = Arc::new(held);
        Self::new(Holder::Daemon, move || {
            Ok(Box::new(held.clone()) as Box<dyn Send>)
        })
    }

    fn new(
        holder: Holder,
        take: impl Fn() -> anyhow::Result<Box<dyn Send>> + Send + Sync + 'static,
    ) -> Self {
        Self {
            holder,
            take: Box::new(take),
            serial: Arc::new(tokio::sync::Mutex::new(())),
        }
    }

    /// Which engine holds the workspace while an operation runs.
    pub fn holder(&self) -> Holder {
        self.holder
    }

    /// Holds the workspace for one operation, once every earlier operation is done.
    pub(crate) async fn hold(&self) -> anyhow::Result<Hold> {
        let serial = self.serial.clone().lock_owned().await;
        Ok(Hold {
            _taken: (self.take)()?,
            _serial: serial,
        })
    }
}

/// A workspace lock kept in memory, for an engine with no disk and for tests. Clones share
/// one lock.
#[derive(Debug, Clone, Default)]
pub struct MemoryLock(Arc<AtomicBool>);

impl MemoryLock {
    pub fn new() -> Self {
        Self::default()
    }

    /// Takes the lock, or refuses with `workspace_held` when it is taken.
    pub fn acquire(&self) -> anyhow::Result<MemoryHold> {
        if self.0.swap(true, Ordering::AcqRel) {
            return Err(KernelError::conflict(
                "Another engine holds the workspace. Stop it before starting this one.",
            )
            .with_reason(codes::WORKSPACE_HELD)
            .into());
        }
        Ok(MemoryHold(self.0.clone()))
    }
}

/// A [`MemoryLock`], taken until this is dropped.
#[derive(Debug)]
pub struct MemoryHold(Arc<AtomicBool>);

impl Drop for MemoryHold {
    fn drop(&mut self) {
        self.0.store(false, Ordering::Release);
    }
}
