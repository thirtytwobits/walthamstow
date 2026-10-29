//! Copyright (c) Scott A Dixon
//!
//! The reason codes the kernel names its own refusals with. A product's
//! catalogue of reason codes includes them.

/// Another engine holds the workspace.
pub const WORKSPACE_HELD: &str = "workspace_held";

/// A write was made under a lease that has been released or has lapsed.
pub const LEASE_LOST: &str = "lease_lost";
