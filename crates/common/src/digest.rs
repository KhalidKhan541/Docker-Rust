//! Content-addressed digests (OCI Image Spec §Digest).
//!
//! Structure mirrors the OCI digest algorithm prefix:
//!
//! ```text
//! sha256:2c26b46b68ffc68ff99b453c1d30413413422d706483bfa0f98a5e886266e7ae
//! ```
//!
//! The canonical algorithm set for docker-rs follows the pinned
//! OCI Image Spec and the Go distribution implementation
//! (`github.com/opencontainers/image-spec`, pinned revision `v1.1.1`,
//! `specs-go/v1/descriptor.go`): `sha256` is mandatory, `sha512` optional.
//! Image manifests must validate against these digests before use, so
//! parsing here is strict and rejection messages are actionable.

use std::fmt;

use docker_errors::{Error, Result};
use serde::{Deserialize, Serialize};

/// Known digest algorithms.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub enum Algorithm {
    Sha256,
}

impl Algorithm {
    /// Hex-encoded digest length in characters for this algorithm.
    pub fn hex_len(self) -> usize {
        match self {
            Algorithm::Sha256 => 64,
        }
    }

    /// Registry string form.
    pub fn as_str(self) -> &'static str {
        match self {
            Algorithm::Sha256 => "sha256",
        }
    }
}

impl fmt::Display for Algorithm {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.as_str())
    }
}

/// A validated `algorithm:hex` digest.
///
/// The hex portion is normalized to lowercase on construction, matching how
/// every consumer in the Docker ecosystem treats digests. Serde maps the
/// value to its canonical `algorithm:hex` string form — content stores,
/// manifests, and the Engine API all carry digests as plain strings.
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub struct Digest {
    algor: Algorithm,
    hex: String,
}

impl Serialize for Digest {
    fn serialize<S: serde::Serializer>(
        &self,
        serializer: S,
    ) -> std::result::Result<S::Ok, S::Error> {
        serializer.serialize_str(&self.full())
    }
}

impl<'de> Deserialize<'de> for Digest {
    fn deserialize<D: serde::Deserializer<'de>>(
        deserializer: D,
    ) -> std::result::Result<Self, D::Error> {
        let s = String::deserialize(deserializer)?;
        Self::parse(&s).map_err(serde::de::Error::custom)
    }
}

impl Digest {
    /// Parse and validate a digest string.
    pub fn parse(input: &str) -> Result<Self> {
        let (algor_s, hex) = input.split_once(':').ok_or_else(|| {
            Error::invalid_argument(format!(
                "invalid digest `{input}`: missing algorithm prefix"
            ))
        })?;
        let algor = match algor_s.to_ascii_lowercase().as_str() {
            "sha256" => Algorithm::Sha256,
            other => {
                return Err(Error::unsupported(format!(
                    "unknown digest algorithm `{other}`; supported: sha256"
                )))
            }
        };
        if hex.len() != algor.hex_len() {
            return Err(Error::invalid_argument(format!(
                "invalid digest `{input}`: expected {algo} hex length {want}, got {got}",
                algo = algor,
                want = algor.hex_len(),
                got = hex.len()
            )));
        }
        let hex = hex.to_ascii_lowercase();
        if !hex.bytes().all(|b| b.is_ascii_hexdigit()) {
            return Err(Error::invalid_argument(format!(
                "invalid digest `{input}`: hex portion contains non-hex characters"
            )));
        }
        Ok(Self { algor, hex })
    }

    /// Construct from an already-computed digest value.
    pub fn sha256(bytes: &[u8]) -> Self {
        let mut hasher = <sha2::Sha256 as sha2::Digest>::new();
        sha2::Digest::update(&mut hasher, bytes);
        let sum = sha2::Digest::finalize(hasher);
        Self {
            algor: Algorithm::Sha256,
            hex: hex_from(&sum),
        }
    }

    pub fn algor(&self) -> Algorithm {
        self.algor
    }

    pub fn hex(&self) -> &str {
        &self.hex
    }

    /// Full string form `algorithm:hex`.
    pub fn full(&self) -> String {
        format!("{}:{}", self.algor, self.hex)
    }
}

impl fmt::Display for Digest {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{}:{}", self.algor, self.hex)
    }
}

fn hex_from(bytes: &[u8]) -> String {
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
    fn parse_accepts_valid_sha256() {
        // Exactly 64 hex characters; input case is normalized to lowercase.
        let hex = "abcdef0123abcdef0123abcdef0123abcdef0123abcdef0123abcdef0123abcd";
        assert_eq!(hex.len(), 64);
        let d = Digest::parse(&format!("SHA256:{hex}")).unwrap();
        assert_eq!(d.algor(), Algorithm::Sha256);
        assert_eq!(d.hex(), hex);
        assert_eq!(d.full(), format!("sha256:{hex}"));
    }

    #[test]
    fn parse_rejects_wrong_shape() {
        assert!(Digest::parse("plainhexstring").is_err());
        assert!(Digest::parse("md5:d41d8cd98f00b204e9800998ecf8427e").is_err());
        let short = Digest::parse("sha256:2c26");
        assert!(short.is_err());
        let bad = Digest::parse(
            "sha256:zz26b46b68ffc68ff99b453c1d30413413422d706483bfa0f98a5e886266e7ae",
        );
        assert!(bad.is_err());
    }

    #[test]
    fn sha256_matches_known_vector() {
        // Empty-string SHA-256 reference vector.
        let d = Digest::sha256(b"");
        assert_eq!(
            d.full(),
            "sha256:e3b0c44298fc1c149afbf4c8996fb92427ae41e4649b934ca495991b7852b855"
        );
    }

    #[test]
    fn serde_round_trip() {
        let d = Digest::sha256(b"");
        let json = serde_json::to_string(&d).unwrap();
        assert_eq!(
            json,
            "\"sha256:e3b0c44298fc1c149afbf4c8996fb92427ae41e4649b934ca495991b7852b855\""
        );
        let parsed: Digest = serde_json::from_str(&json).unwrap();
        assert_eq!(parsed, d);
    }
}
