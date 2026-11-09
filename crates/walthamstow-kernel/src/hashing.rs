//! Copyright (c) Scott A Dixon
//!
//! Content digests for the engine's own files and records.

use sha2::{Digest, Sha256};
use std::fmt::Write;

/// The lowercase SHA-256 hex digest of `bytes`.
pub fn sha256_hex(bytes: &[u8]) -> String {
    Sha256::digest(bytes)
        .iter()
        .fold(String::with_capacity(64), |mut hex, byte| {
            write!(hex, "{byte:02x}").expect("writing to a String succeeds");
            hex
        })
}

#[cfg(test)]
mod tests {
    use super::sha256_hex;

    #[test]
    fn the_digest_is_sixty_four_lowercase_hex_digits() {
        // The FIPS 180-2 example digests of "abc" and of the empty message.
        assert_eq!(
            sha256_hex(b"abc"),
            "ba7816bf8f01cfea414140de5dae2223b00361a396177a9cb410ff61f20015ad"
        );
        assert_eq!(
            sha256_hex(b""),
            "e3b0c44298fc1c149afbf4c8996fb92427ae41e4649b934ca495991b7852b855"
        );
    }
}
