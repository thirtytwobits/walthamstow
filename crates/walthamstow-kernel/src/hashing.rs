//! Copyright (c) Scott A Dixon
//!
//! Content digests for the engine's own files and records.

use sha2::{Digest, Sha256};

/// The lowercase SHA-256 hex digest of `bytes`.
pub fn sha256_hex(bytes: &[u8]) -> String {
    format!("{:x}", Sha256::digest(bytes))
}
