//! Content hashing.
//!
//! One helper, used in two places that must agree on a stable digest: a module's
//! source hash (recorded in `module_metadata` and used as an effect's script
//! hash) and the content-hash key of an effect's journaled calls.

use std::fmt::Write as _;

use sha2::{Digest, Sha256};

/// The lowercase-hex SHA-256 of `bytes`.
pub fn sha256_hex(bytes: &[u8]) -> String {
    let digest = Sha256::digest(bytes);
    let mut out = String::with_capacity(digest.len() * 2);
    for byte in digest.iter() {
        let _ = write!(out, "{byte:02x}");
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn matches_a_known_vector() {
        // The SHA-256 of the empty input.
        assert_eq!(
            sha256_hex(b""),
            "e3b0c44298fc1c149afbf4c8996fb92427ae41e4649b934ca495991b7852b855"
        );
    }

    #[test]
    fn differs_by_input() {
        assert_ne!(sha256_hex(b"a"), sha256_hex(b"b"));
    }
}
