//! Random identifier generation.
//!
//! Equivalent of Moby's `pkg/stringid` package. The original
//! (`pkg/stringid/generate.go`) derives a 32-octet value from crypto/rand
//! when the platform CSPRNG is available, hex-encodes it, and truncates to
//! the requested length. We do the same using the `getrandom` crate, which
//! is a thin binding over the OS CSPRNG (`RtlGenRandom`-family on Windows,
//! `getrandom(2)`/`arc4random` on BSD/macOS, `getentropy` on Linux fallback
//! paths).

use docker_errors::{Error, Result};

/// Maximum identifier length produced by upstream tooling. Moby's
/// `GenerateRandomID` returns 64 hex characters and short IDs are prefixes
/// of the full value.
pub const MAX_ID_LEN: usize = 64;

/// Generate `n` cryptographically random octets and return their lowercase
/// hex encoding.
pub fn random_hex(n: usize) -> Result<String> {
    let mut buf = vec![0u8; n];
    getrandom::getrandom(&mut buf)
        .map_err(|e| Error::internal(format!("entropy source unavailable: {e}")))?;
    Ok(hex_lower(&buf))
}

/// Generate a 64-character random hex identifier, matching
/// `pkg/stringid.GenerateID` for container IDs.
pub fn generate_id() -> Result<String> {
    random_hex(32)
}

/// Encode bytes as lowercase hex.
fn hex_lower(bytes: &[u8]) -> String {
    const HEX: &[u8; 16] = b"0123456789abcdef";
    let mut out = String::with_capacity(bytes.len() * 2);
    for b in bytes {
        out.push(HEX[(b >> 4) as usize] as char);
        out.push(HEX[(b & 0x0f) as usize] as char);
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn generate_id_is_64_lowercase_hex() {
        let id = generate_id().unwrap();
        assert_eq!(id.len(), 64);
        assert!(
            id.bytes()
                .all(|c| c.is_ascii_hexdigit() && !c.is_ascii_uppercase()),
            "id must be lowercase hex, got {id}"
        );
    }

    #[test]
    fn ids_differ() {
        let a = generate_id().unwrap();
        let b = generate_id().unwrap();
        assert_ne!(a, b);
    }

    #[test]
    fn random_hex_respects_length() {
        assert_eq!(random_hex(11).unwrap().len(), 22);
        assert_eq!(random_hex(1).unwrap().len(), 2);
        assert_eq!(random_hex(0).unwrap(), "");
    }
}
