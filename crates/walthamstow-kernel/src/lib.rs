//! Copyright (c) Scott A Dixon
//!
//! The engine kernel's foundation, which every part of the engine builds on whatever product
//! it serves: errors and events, the daemon's lifecycle, the clocks, durable writes, content
//! hashes, the data directory's journal layout, the engine's local records, document names, leases
//! and collaboration envelope storage.

pub mod clock;
pub mod collaboration_storage;
pub mod daemon;
pub mod document;
pub mod durability;
pub mod error;
pub mod files;
pub mod hashing;
pub mod journals;
pub mod leases;
pub mod local_store;
pub mod reason_codes;
mod workspace_lock;

pub use clock::WallClock;
pub use document::DocumentId;
pub use error::{KernelError, KernelResult};
