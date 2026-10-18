//! Container identity and reference resolution.
//!
//! The Docker daemon identifies containers by a 64-character lowercase hex
//! identifier (Moby `pkg/stringid.GenerateID`, 32 random bytes hex-encoded)
//! and expects clients to address objects by unambiguous prefixes of that
//! identifier. These rules are encoded here once and reused by the API
//! layer, the container manager, and the runtime.

use std::fmt;

use docker_errors::{Error, Result};
use serde::{Deserialize, Serialize};

use crate::rng;

/// A full, canonical container identifier: exactly 64 lowercase hex
/// characters.
#[derive(Debug, Clone, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub struct ContainerId(String);

impl ContainerId {
    /// Construct from a full ID string. Both cases are accepted and
    /// normalized to lowercase, mirroring how the daemon treats IDs found in
    /// API requests (`docker inspect DEADBEEF...` works).
    pub fn new(input: &str) -> Result<Self> {
        validate_hex_id(input, 64, 64)?;
        Ok(Self(input.to_ascii_lowercase()))
    }

    /// Generate a fresh identifier (32 bytes of entropy hex-encoded).
    pub fn generate() -> Result<Self> {
        let s = rng::generate_id()?;
        Ok(Self(s))
    }

    pub fn from_generated(hex: String) -> Result<Self> {
        validate_hex_id(&hex, 64, 64)?;
        Ok(Self(hex.to_ascii_lowercase()))
    }

    pub fn as_str(&self) -> &str {
        &self.0
    }

    /// Whether a client-supplied prefix unambiguously (by prefix rule)
    /// references this container. Length rules are delegated to the caller;
    /// this only checks the string prefix property.
    pub fn starts_with(&self, prefix: &str) -> bool {
        self.0.starts_with(prefix)
    }
}

impl fmt::Display for ContainerId {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.0)
    }
}

impl AsRef<str> for ContainerId {
    fn as_ref(&self) -> &str {
        &self.0
    }
}

/// Validate a hex identifier with inclusive length bounds and reject
/// non-hex characters. Returns an actionable error.
fn validate_hex_id(input: &str, min: usize, max: usize) -> Result<()> {
    if input.len() < min || input.len() > max {
        return Err(Error::invalid_argument(format!(
            "invalid ID `{input}`: {min}..={max} characters required, got {}",
            input.len()
        )));
    }
    if !input.bytes().all(|b| b.is_ascii_hexdigit()) {
        return Err(Error::invalid_argument(format!(
            "invalid ID `{input}`: contains non-hexadecimal characters"
        )));
    }
    Ok(())
}

/// Parse a client-supplied ID reference (full ID or prefix, 1..=64 hex
/// characters). The daemon lookup machinery (`docker ps deadb`, `docker
/// inspect <prefix>`) works on these; full IDs still require exactly 64.
pub fn parse_id_prefix(input: &str) -> Result<String> {
    validate_hex_id(input, 1, 64)?;
    Ok(input.to_ascii_lowercase())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn accepts_64_hex() {
        let id =
            ContainerId::new("DEADBEEF0123456789ABCDEF0123456789ABCDEF0123456789ABCDEF01234567")
                .unwrap();
        assert_eq!(id.as_str().len(), 64);
        assert!(id.as_str().bytes().all(|c| !c.is_ascii_uppercase()));
    }

    #[test]
    fn rejects_bad_ids() {
        assert!(ContainerId::new("").is_err());
        assert!(ContainerId::new("abcd").is_err());
        assert!(ContainerId::new(&"z".repeat(64)).is_err());
        let too_long = format!("0{}", "0".repeat(64));
        assert!(ContainerId::new(&too_long).is_err());
    }

    #[test]
    fn generated_ids_are_canonical() {
        let id = ContainerId::generate().unwrap();
        assert_eq!(id.as_str().len(), 64);
        assert!(id
            .as_str()
            .bytes()
            .all(|c| c.is_ascii_hexdigit() && !c.is_ascii_uppercase()));
    }

    #[test]
    fn prefix_parsing() {
        assert_eq!(parse_id_prefix("DEADbeef").unwrap(), "deadbeef");
        assert!(parse_id_prefix("").is_err());
        assert!(parse_id_prefix("nothex").is_err());
    }

    #[test]
    fn starts_with_works() {
        let id =
            ContainerId::new("0123456789ABCDEF0123456789ABCDEF0123456789ABCDEF0123456789ABCDEF")
                .unwrap();
        assert!(id.starts_with("012345"));
        assert!(id.starts_with("01234"));
        assert!(!id.starts_with("987654"));
    }
}
