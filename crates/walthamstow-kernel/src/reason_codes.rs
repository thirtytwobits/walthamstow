//! Copyright (c) Scott A Dixon
//!
//! The reason codes the kernel names its own refusals with. A product's
//! catalogue of reason codes includes them.

/// Another engine holds the workspace.
pub const WORKSPACE_HELD: &str = "workspace_held";

/// A write was made under a lease that has been released or has lapsed.
pub const LEASE_LOST: &str = "lease_lost";

/// A file a publication replaces no longer holds what was read when it was staged.
pub const PUBLICATION_STALE: &str = "publication_stale";

/// Two files a publication replaces differ only in case, so a file system that folds case holds
/// them as one.
pub const PUBLICATION_PATHS_COLLIDE: &str = "publication_paths_collide";
