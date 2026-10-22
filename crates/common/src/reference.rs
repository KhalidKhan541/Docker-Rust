//! Docker image reference parsing (`registry.example.com/ns/repo:tag@digest`).
//!
//! Subset implementation of the reference grammar from Docker
//! distribution (`github.com/distribution/reference`, vendored into moby)
//! covering the forms that appear in day-to-day engine operations. Full
//! OCI distribution-spec grammar nuances (e.g. ARM-style lowercase
//! enforcement on some registries, IDN normalization) are out of scope and
//! documented in `docs/OCI_COMPATIBILITY.md`.
//!
//! Accepted forms:
//!
//! ```text
//! repo                                       -> implicit docker.io, lib "library"
//! repo:tag                                   -> tag validation applies
//! ns/repo, a/b/c                             -> path with 1..N components
//! registry.example.com:443/ns/repo:tag       -> explicit registry + port
//! localhost/ns/repo                          -> localhost is a registry
//! ns/repo:tag@sha256:abcd...                 -> pinned digest
//! ```
//!
//! Rules enforced:
//! * Path components after the registry component must be lowercase
//!   ASCII alphanumerics, with separators being single `.`/`_`/`-` or
//!   runs of `-`/`__` (distribution `reference` daemon rules).
//! * The first component is a registry when it contains `.`, `:`, or is
//!   exactly `localhost`.
//! * Tag characters `[a-zA-Z0-9_][a-zA-Z0-9._-]*`, 1..=128 bytes.
//! * Digest must parse as a supported [`crate::digest::Digest`].

use std::fmt;

use docker_errors::{Error, Result};
use serde::{Deserialize, Serialize};

use crate::digest::Digest;
use crate::names::MAX_NAME_LEN;

/// A parsed image reference with its components separated.
#[derive(Debug, Clone, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub struct ImageRef {
    /// Registry host, only when the reference had an explicit registry part.
    registry: Option<String>,
    /// Repository path components (no registry).
    path: Vec<String>,
    /// Tag, when present.
    tag: Option<String>,
    /// Content digest, when pinned explicitly.
    digest: Option<String>,
}

impl ImageRef {
    /// Parse a reference string into its canonical parts.
    pub fn parse(input: &str) -> Result<Self> {
        if input.is_empty() {
            return Err(Error::invalid_argument("image reference must not be empty"));
        }
        let (body, digest) = split_digest(input)?;
        let (path_part, tag) = split_tag(body)?;
        let (registry, path) = split_registry(path_part)?;
        for (i, comp) in path.iter().enumerate() {
            validate_path_component(comp, i)?;
        }
        if path.is_empty() {
            return Err(Error::invalid_argument(format!(
                "invalid reference `{input}`: empty repository path"
            )));
        }
        Ok(Self {
            registry: registry.map(str::to_string),
            path: path.iter().map(|s| s.to_string()).collect(),
            tag: tag.map(str::to_string),
            digest,
        })
    }

    /// Registry host (None means the implicit default registry).
    pub fn registry(&self) -> Option<&str> {
        self.registry.as_deref()
    }

    /// Repository path as a single string.
    pub fn repo(&self) -> String {
        self.path.join("/")
    }

    /// Canonical repo path the daemon resolves to when pulling: the Docker
    /// Hub "library/" namespace is implicit for single-component repos
    /// without an explicit registry.
    pub fn canonical_repo(&self) -> String {
        if self.registry.is_none() && self.path.len() == 1 {
            return format!("library/{}", self.repo());
        }
        self.repo()
    }

    /// Explicit tag, if the reference carried one.
    pub fn tag(&self) -> Option<&str> {
        self.tag.as_deref()
    }

    /// Effective tag, or `latest` when absent — the Engine API resolves a
    /// bare reference to `latest`.
    pub fn effective_tag(&self) -> &str {
        self.tag.as_deref().unwrap_or("latest")
    }

    pub fn digest(&self) -> Option<&str> {
        self.digest.as_deref()
    }
}

impl fmt::Display for ImageRef {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        if let Some(r) = &self.registry {
            f.write_str(r)?;
            f.write_str("/")?;
        }
        f.write_str(&self.repo())?;
        if let Some(t) = &self.tag {
            write!(f, ":{t}")?;
        }
        if let Some(d) = &self.digest {
            write!(f, "@{d}")?;
        }
        Ok(())
    }
}

/// Split an optional `@digest` suffix off the input.
fn split_digest(input: &str) -> Result<(&str, Option<String>)> {
    if let Some(idx) = input.rfind('@') {
        let digest_str = &input[idx + 1..];
        let parsed = Digest::parse(digest_str)
            .map_err(|e| Error::invalid_argument(format!("invalid reference:{}", e.message())))?;
        Ok((&input[..idx], Some(parsed.full())))
    } else {
        Ok((input, None))
    }
}

/// Split an optional `:tag` suffix. Per distribution rules, a colon only
/// introduces a tag if no `/` follows it; otherwise it belongs to the
/// registry port.
fn split_tag(input: &str) -> Result<(&str, Option<&str>)> {
    match input.rfind(':') {
        // If a `/` appears later than the colon, the colon is part of the
        // registry host[:port], not a tag.
        Some(idx) if !input[idx..].contains('/') => {
            let tag = &input[idx + 1..];
            validate_tag(tag)?;
            Ok((&input[..idx], Some(tag)))
        }
        _ => Ok((input, None)),
    }
}

/// Validate a tag against `[a-zA-Z0-9_][a-zA-Z0-9._-]{0,127}`.
fn validate_tag(tag: &str) -> Result<()> {
    let bytes = tag.as_bytes();
    if bytes.is_empty() || bytes.len() > 128 {
        return Err(Error::invalid_argument(format!(
            "invalid tag `{tag}`: length must be 1..=128, got {}",
            bytes.len()
        )));
    }
    let first_ok = bytes[0].is_ascii_alphanumeric() || bytes[0] == b'_';
    let rest_ok = bytes[1..]
        .iter()
        .all(|&b| b.is_ascii_alphanumeric() || b == b'.' || b == b'_' || b == b'-');
    if !first_ok || !rest_ok {
        return Err(Error::invalid_argument(format!(
            "invalid tag `{tag}`: characters must match [a-zA-Z0-9_][a-zA-Z0-9._-]*"
        )));
    }
    Ok(())
}

/// Decide whether the first path component is a registry host. Detection
/// mirrors distribution's `reference` package: domains must contain `.` or
/// `:`, or be exactly `localhost`; they may be uppercase.
fn split_registry(rest: &str) -> Result<(Option<&str>, Vec<&str>)> {
    let (host, path) = match rest.find('/') {
        Some(idx) => (&rest[..idx], Some(&rest[idx + 1..])),
        None => (rest, None),
    };
    let is_domain = host.contains('.') || host.contains(':') || host == "localhost";
    if is_domain {
        validate_registry_host(host)?;
        let path = path.unwrap_or("");
        let comps = split_path(path)?;
        Ok((Some(host), comps))
    } else {
        let comps = split_path(rest)?;
        Ok((None, comps))
    }
}

fn validate_registry_host(host: &str) -> Result<()> {
    if host.is_empty() || host.len() > 255 {
        return Err(Error::invalid_argument(format!(
            "invalid registry host `{host}`"
        )));
    }
    if !host
        .bytes()
        .all(|b| b.is_ascii_alphanumeric() || b == b'.' || b == b':' || b == b'-' || b == b'_')
    {
        return Err(Error::invalid_argument(format!(
            "invalid registry host `{host}`"
        )));
    }
    Ok(())
}

fn split_path(path: &str) -> Result<Vec<&str>> {
    let comps: Vec<&str> = path.split('/').collect();
    Ok(comps)
}

/// Validate a repository path component per the distribution rules:
/// lowercase alphanumerics separated by single `.`/`_`/`-`, or double `__`,
/// or runs of `-`.
fn validate_path_component(comp: &str, idx: usize) -> Result<()> {
    if comp.is_empty() || comp.len() > MAX_NAME_LEN {
        return Err(Error::invalid_argument(format!(
            "invalid reference: path component {idx} empty or too long"
        )));
    }
    let bytes = comp.as_bytes();
    if !bytes[0].is_ascii_lowercase() && !bytes[0].is_ascii_digit() {
        return Err(Error::invalid_argument(format!(
            "invalid reference: path component `{comp}` must start [a-z0-9]"
        )));
    }
    let mut prev_sep = Separator::None;
    for &b in bytes {
        let is_alnum = b.is_ascii_lowercase() || b.is_ascii_digit();
        let is_sep = b == b'.' || b == b'_' || b == b'-';
        if !is_alnum && !is_sep {
            return Err(Error::invalid_argument(format!(
                "invalid reference: path component `{comp}` contains character outside [a-z0-9._-]"
            )));
        }
        let cur = match b {
            b'.' => Separator::Dot,
            b'_' => Separator::Underscore,
            b'-' => Separator::Dash,
            _ => Separator::None,
        };
        // Distribution allows single separators, or `__`, or `--`+ runs,
        // but never mixed ambiguous sequences like `.-` or `._`.
        if prev_sep != Separator::None && cur != Separator::None {
            let legal = matches!(
                (prev_sep, cur),
                (Separator::Underscore, Separator::Underscore) | (Separator::Dash, Separator::Dash)
            );
            if !legal {
                return Err(Error::invalid_argument(format!(
                    "invalid reference: path component `{comp}` has illegal separator sequence"
                )));
            }
        }
        prev_sep = cur;
    }
    Ok(())
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Separator {
    None,
    Dot,
    Underscore,
    Dash,
}

#[cfg(test)]
mod tests {
    use super::*;

    fn ok(s: &str) -> ImageRef {
        ImageRef::parse(s).unwrap_or_else(|e| panic!("expected `{s}` valid, got: {e}"))
    }

    #[test]
    fn simple_repo_gets_library_namespace() {
        let r = ok("ubuntu");
        assert!(r.registry().is_none());
        assert_eq!(r.repo(), "ubuntu");
        assert_eq!(r.canonical_repo(), "library/ubuntu");
        assert_eq!(r.effective_tag(), "latest");
    }

    #[test]
    fn namespaces_and_tags() {
        let r = ok("myorg/webapp:1.2.3");
        assert!(r.registry().is_none());
        assert_eq!(r.repo(), "myorg/webapp");
        assert_eq!(r.effective_tag(), "1.2.3");
        assert_eq!(r.to_string(), "myorg/webapp:1.2.3");
    }

    #[test]
    fn registry_with_port() {
        let r = ok("localhost:5000/team/app");
        assert_eq!(r.registry(), Some("localhost:5000"));
        assert_eq!(r.repo(), "team/app");
        assert_eq!(r.canonical_repo(), "team/app");
        assert_eq!(r.effective_tag(), "latest");
    }

    #[test]
    fn domain_lookalike_single_component_is_path_not_registry() {
        // No dot/colon/localhost and no slash: it is a plain repo, and a
        // single-component repo gets the implicit docker.io/library path.
        let r = ok("myapp");
        assert!(r.registry().is_none());
        assert_eq!(r.repo(), "myapp");
        // Uppercase in a *path* component is invalid (no domain detected).
        assert!(ImageRef::parse("myApp/path").is_err());
        // ...but a dotted first component is a registry host, and hosts may
        // contain uppercase per the distribution grammar.
        let reg = ok("myApp.bad/path");
        assert_eq!(reg.registry(), Some("myApp.bad"));
        assert_eq!(reg.repo(), "path");
    }

    #[test]
    fn digest_pinned() {
        let hex = "0123456789abcdef0123456789abcdef0123456789abcdef0123456789abcdef";
        let r = ok(&format!("nginx:alpine@sha256:{hex}"));
        assert_eq!(r.digest(), Some(format!("sha256:{hex}").as_str()));
        assert!(ImageRef::parse(&format!("nginx:alpine@sha256:{hex}x")).is_err());
    }

    #[test]
    fn rejected_references() {
        assert!(ImageRef::parse("").is_err());
        assert!(ImageRef::parse("-bad/repo").is_err());
        assert!(ImageRef::parse("UPPER/path").is_err());
        assert!(ImageRef::parse("repo:tag ").is_err());
        assert!(ImageRef::parse("a//b").is_err());
        assert!(ImageRef::parse("a/b.-c").is_err());
        assert!(ImageRef::parse("registry..:22/ns").is_ok());
    }

    #[test]
    fn tag_chars() {
        assert!(ImageRef::parse("a/b:x-._y").is_ok());
        assert!(ImageRef::parse("a/b:_x").is_ok());
        assert!(ImageRef::parse("a/b:-x").is_err());
        let long_tag = "t".repeat(129);
        assert!(ImageRef::parse(&format!("a/b:{long_tag}")).is_err());
        let ok_tag = "t".repeat(128);
        assert!(ImageRef::parse(&format!("a/b:{ok_tag}")).is_ok());
    }
}
