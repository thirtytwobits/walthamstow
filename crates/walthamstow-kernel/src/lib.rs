//! Copyright (c) Scott A Dixon
//!
//! The engine kernel's foundation, which every part of the engine builds on whatever product
//! it serves: errors and events, the daemon's lifecycle, the clocks, durable writes, content
//! hashes, the data directory's journal layout, the engine's local records, leases, the
//! workspace lock and collaboration envelope storage.

pub mod clock;
pub mod collaboration_storage;
pub mod daemon;
pub mod durability;
pub mod error;
pub mod files;
pub mod hashing;
pub mod journals;
pub mod leases;
pub mod local_store;
pub mod reason_codes;
pub mod workspace_lock;

pub use clock::WallClock;
pub use error::{KernelError, KernelResult};
