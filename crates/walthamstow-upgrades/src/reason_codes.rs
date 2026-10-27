//! Copyright (c) Scott A Dixon
//!
//! The reason codes the upgrade framework names its refusals with. A product's catalogue of
//! reason codes includes them.

/// A file an upgrade replaces no longer holds what the upgrade read.
pub const UPGRADE_STALE: &str = "upgrade_stale";

/// Two files an upgrade replaces differ only in case, so a file system that folds case holds
/// them as one.
pub const UPGRADE_PATHS_COLLIDE: &str = "upgrade_paths_collide";
