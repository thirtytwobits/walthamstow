//! Copyright (c) Scott A Dixon
//!
//! The reason codes the upgrade framework names its refusals with. A product's catalogue of
//! reason codes includes them.

/// A file an upgrade replaces no longer holds what the upgrade read.
pub const UPGRADE_STALE: &str = "upgrade_stale";
