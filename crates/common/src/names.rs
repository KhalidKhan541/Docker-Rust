//! Validated object names shared by containers, volumes, and networks.
//!
//! Docker's daemon accepts the same restricted character class for
//! container names ([moby][moby-repo] daemon name validation:
//! `[a-zA-Z0-9][a-zA-Z0-9_.-]*`) and applies a close variant for volume and
//! network names. The upper length bound implemented here (255 bytes) is a
//! docker-rs design choice: upstream has no single hard limit for container
//! names, but every downstream consumer — filesystem paths, the content
//! store, z-ordering in listings — is happier with one. This bound is
//! documented in docs/API_COMPATIBILITY.md.
//!
//! [moby-repo]: https://github.com/moby/moby (pinned revision v28.5.2)

use std::fmt;

use docker_errors::{Error, Result};
use serde::{Deserialize, Serialize};

/// Upper bound on accepted object names, in bytes. Allowing more than the
/// 232-byte POSIX filename limit serves no purpose even on newer
/// filesystems; 255 is the historical EXT filename limit.
pub const MAX_NAME_LEN: usize = 255;

/// A validated object name (container, volume, or network).
#[derive(Debug, Clone, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub struct Name(String);

impl Name {
    /// Validate and normalize an object name.
    ///
    /// Rules:
    /// 1. Non-empty, 1..=255 bytes.
    /// 2. First byte is ASCII alphanumeric.
    /// 3. Remaining bytes are alphanumerics or one of `.`, `_`, `-`.
    pub fn new(input: &str) -> Result<Self> {
        let bytes = input.as_bytes();
        if bytes.is_empty() || bytes.len() > MAX_NAME_LEN {
            return Err(Error::invalid_argument(format!(
                "invalid name `{input}`: length must be 1..={MAX_NAME_LEN}, got {}",
                bytes.len()
            )));
        }
        let first_ok = bytes[0].is_ascii_alphanumeric();
        let rest_ok = bytes[1..]
            .iter()
            .all(|&b| b.is_ascii_alphanumeric() || b == b'.' || b == b'_' || b == b'-');
        if !first_ok || !rest_ok {
            return Err(Error::invalid_argument(format!(
                "invalid name `{input}`: characters outside [a-zA-Z0-9_.-]"
            )));
        }
        Ok(Self(input.to_string()))
    }

    pub fn as_str(&self) -> &str {
        &self.0
    }
}

impl fmt::Display for Name {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.0)
    }
}

impl AsRef<str> for Name {
    fn as_ref(&self) -> &str {
        &self.0
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn accepts_valid_names() {
        for n in ["web", "my_app-1.0", "x9", "A_b-C.d9", "a.b_c-"] {
            Name::new(n).unwrap_or_else(|e| panic!("expected `{n}` valid: {e}"));
        }
    }

    #[test]
    fn rejects_invalid_names() {
        // Trailing separators are legal upstream (`[a-zA-Z0-9][a-zA-Z0-9_.-]*`),
        // so `a.b_c-` belongs in accepts_valid_names, not here.
        for n in ["", "-leading", "_under", "a b", "значение", "dot|"] {
            let r = Name::new(n);
            assert!(r.is_err(), "expected `{n}` rejected");
        }
    }

    #[test]
    fn rejects_oversized() {
        let n = "a".repeat(256);
        assert!(Name::new(&n).is_err());
        let n_ok = "a".repeat(255);
        assert!(Name::new(&n_ok).is_ok());
    }
}
