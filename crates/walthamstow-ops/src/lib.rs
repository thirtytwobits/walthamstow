//! Copyright (c) Scott A Dixon
//!
//! Walthamstow's operations, contributed to Clapham under the `walthamstow` namespace: the
//! workspace's status and documents, its upgrades and recovery, and the engine's journals.
//! Each operation runs against the workspace under its lease, in the daemon or in-process.
//!
//! Operations reach Walthamstow through the product's gate, which has decided whether the
//! principal may run them. Walthamstow does not judge principals.

pub mod codes;
pub mod formats;
pub mod lease;
pub mod operations;
pub mod workspace;

pub use formats::{DocumentFormat, FormatStatus, Upgrade};
pub use lease::{Holder, Lease, MemoryHold, MemoryLock};
pub use operations::{Entity, WalthamstowOps, NAMESPACE};
pub use workspace::{FileWorkspace, MemoryWorkspace, Workspace};
