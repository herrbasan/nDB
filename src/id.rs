//! NanoID-style document ID generation.
//!
//! 16-character base62 IDs: `[a-zA-Z0-9]`.
//! Collision space: 62^16 ≈ 4.7 × 10^28 — effectively zero risk.
//! PRNG-based with O(1) uniqueness check against existing HashMap.

use fastrand::Rng;
use std::collections::HashSet;

const ID_LENGTH: usize = 16;
const BASE62: &[u8] = b"abcdefghijklmnopqrstuvwxyzABCDEFGHIJKLMNOPQRSTUVWXYZ0123456789";

/// Generate a new NanoID-style ID (16 chars, base62).
pub fn generate() -> String {
    let mut rng = Rng::new();
    let id: String = (0..ID_LENGTH)
        .map(|_| BASE62[rng.usize(..62)] as char)
        .collect();
    id
}

/// Generate a NanoID-style ID with a prefix.
/// Result: `{prefix}_{random}` e.g. `conv_V1StGXR8Z5jdHi6B`
pub fn generate_with_prefix(prefix: &str) -> String {
    format!("{}_{}", prefix, generate())
}

/// Generate a unique ID, checking against existing keys.
/// Retries up to 10 times on collision (astronomically unlikely).
pub fn generate_unique(existing: &HashSet<String>) -> String {
    for _ in 0..10 {
        let id = generate();
        if !existing.contains(&id) {
            return id;
        }
    }
    // After 10 collisions something is deeply wrong
    panic!("ndb: failed to generate unique ID after 10 attempts");
}

/// Generate a unique prefixed ID, checking against existing keys.
pub fn generate_unique_with_prefix(prefix: &str, existing: &HashSet<String>) -> String {
    for _ in 0..10 {
        let id = generate_with_prefix(prefix);
        if !existing.contains(&id) {
            return id;
        }
    }
    panic!("ndb: failed to generate unique prefixed ID after 10 attempts");
}

/// Validate that a string is a valid ndb ID.
/// Must be non-empty, only base62 chars (and optionally one underscore separator for prefixed IDs).
pub fn is_valid_id(id: &str) -> bool {
    !id.is_empty() && id.chars().all(|c| c.is_ascii_alphanumeric() || c == '_')
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn generate_id_length() {
        let id = generate();
        assert_eq!(id.len(), ID_LENGTH);
    }

    #[test]
    fn generate_id_base62() {
        let id = generate();
        assert!(id.chars().all(|c| c.is_ascii_alphanumeric()));
    }

    #[test]
    fn generate_id_unique() {
        let ids: std::collections::HashSet<String> = (0..1000).map(|_| generate()).collect();
        assert_eq!(ids.len(), 1000, "generated duplicate IDs in 1000 attempts");
    }

    #[test]
    fn generate_prefixed() {
        let id = generate_with_prefix("conv");
        assert!(id.starts_with("conv_"));
        assert_eq!(id.len(), ID_LENGTH + 5); // "conv_" + 16
    }

    #[test]
    fn generate_unique_no_collision() {
        let mut existing = HashSet::new();
        existing.insert("aaaaaaaaaaaaaaaa".to_string());
        let id = generate_unique(&existing);
        assert_ne!(id, "aaaaaaaaaaaaaaaa");
    }

    #[test]
    fn is_valid_id_checks() {
        assert!(is_valid_id("V1StGXR8Z5jdHi6B"));
        assert!(is_valid_id("conv_V1StGXR8Z5jdHi6B"));
        assert!(!is_valid_id(""));
        assert!(!is_valid_id("has spaces"));
        assert!(!is_valid_id("has-dash"));
    }
}
