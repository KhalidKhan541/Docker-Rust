//! Lexically safe filesystem path resolution.
//!
//! Container layers, volumes, and content files live under the daemon data
//! root. Image archives arriving from registries are untrusted input, and a
//! classic failure mode (documented in countless CVE analyses, e.g. the
//! `tar` path traversal family) is letting an archive escape its
//! destination directory. This module provides one narrow, tested primitive
//! — lexical resolution under a root — that layer extraction, snapshot
//! management, and the content store all must route through.
//!
//! This is a *lexical* resolver: it touches neither the filesystem nor
//! symlinks. It rejects traversal attempts instead of following them, which
//! is the strongest posture when the input path itself should never contain
//! `..` (destination paths produced by our own layer tooling).

use std::path::{Path, PathBuf};

use docker_errors::{Error, Result};

/// Zero-allocation-safe resolution: `root.join(rel)` with strict checks.
pub struct SafePath;

impl SafePath {
    /// Resolve `rel` under `root`, rejecting absolute inputs, parent
    /// traversal, NUL bytes, and Windows drive-letter/UNC tricks. Returns a
    /// path that is guaranteed to stay inside the tree rooted at `root`.
    ///
    /// `rel` may use both `/` and `\` separators; this makes the guarantee
    /// hold regardless of which separator an attacker controlled the
    /// archive element with (Windows path separators on Linux count as
    /// regular filename characters there, so we keep the strictest
    /// interpretation).
    pub fn resolve_under(root: &Path, rel: &str) -> Result<PathBuf> {
        if rel.is_empty() {
            return Err(Error::invalid_argument("empty relative path"));
        }
        if rel.contains('\0') {
            return Err(Error::invalid_argument("path contains NUL byte"));
        }
        if rel.starts_with('/') || rel.starts_with('\\') {
            return Err(Error::invalid_argument(format!(
                "absolute path `{rel}` not allowed under data root"
            )));
        }
        if rel.len() >= 3 && rel.as_bytes()[1] == b':' {
            // Windows drive-letter style `C:\...` — rejected as traversal.
            return Err(Error::invalid_argument(format!(
                "drive-letter path `{rel}` not allowed under data root"
            )));
        }
        let mut out: Vec<std::ffi::OsString> = Vec::new();
        for part in rel.split(['/', '\\']) {
            match part {
                "" | "." => continue,
                ".." => {
                    return Err(Error::invalid_argument(format!(
                        "path traversal rejected: `..` component found in `{rel}`"
                    )))
                }
                other => out.push(other.into()),
            }
        }
        if out.is_empty() {
            return Err(Error::invalid_argument(format!(
                "path `{rel}` resolves to nothing under data root"
            )));
        }
        let mut joined = root.to_path_buf();
        for part in out {
            joined.push(part);
        }
        Ok(joined)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn joins_simple_rel() {
        let p = SafePath::resolve_under(Path::new("/data/root"), "containers/abc/123").unwrap();
        // On Windows, join produces backslashes; compare lexically.
        let s = p.to_string_lossy().to_lowercase().replace('\\', "/");
        assert_eq!(s, "/data/root/containers/abc/123");
    }

    #[test]
    fn skips_dot_components() {
        let p = SafePath::resolve_under(Path::new("/data/root"), "./containers/./x.json").unwrap();
        let s = p.to_string_lossy().to_lowercase().replace('\\', "/");
        assert_eq!(s, "/data/root/containers/x.json");
    }

    #[test]
    fn rejects_parent_traversal() {
        assert!(SafePath::resolve_under(Path::new("/data/root"), "../etc/passwd").is_err());
        assert!(SafePath::resolve_under(Path::new("/data/root"), "ok/../../etc").is_err());
        assert!(SafePath::resolve_under(Path::new("/data/root"), "..\\..\\etc\\shadow").is_err());
    }

    #[test]
    fn rejects_drive_letter_and_nul() {
        assert!(SafePath::resolve_under(Path::new("/data/root"), "C:\\Windows\\evil").is_err());
        assert!(SafePath::resolve_under(Path::new("/data/root"), "evil\u{0}name").is_err());
        assert!(SafePath::resolve_under(Path::new("/data/root"), "").is_err());
        assert!(SafePath::resolve_under(Path::new("/data/root"), "/abs").is_err());
        assert!(SafePath::resolve_under(Path::new("/data/root"), ".").is_err());
        assert!(SafePath::resolve_under(Path::new("/data/root"), "././.").is_err());
    }

    #[test]
    fn posix_backslash_is_strict_separator() {
        // Even on Unix, where `\` is a legal filename character for docker,
        // layer paths from archives are always `/`-joined. Strictest rule
        // keeps both worlds consistent.
        let p = SafePath::resolve_under(Path::new("/data/root"), "a\\b.json").unwrap();
        let s = p.to_string_lossy().to_lowercase().replace('\\', "/");
        assert_eq!(s, "/data/root/a/b.json");
    }
}
