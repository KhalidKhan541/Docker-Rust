//! Opt-in static-file serving for the local docker-rs console.
//!
//! This is a docker-rs extension, not an Engine API endpoint. The router calls
//! it only for paths outside the reserved API namespaces, and the resolver
//! never lists directories. Paths are decoded exactly once, checked for
//! traversal/dotfile/Windows-device tricks, and canonicalized so a symlink can
//! not escape the configured root.

use std::fs;
use std::io;
use std::path::{Path, PathBuf};

/// Static assets larger than this are rejected instead of being buffered.
const MAX_STATIC_BYTES: u64 = 64 * 1024 * 1024;

#[derive(Debug)]
pub(crate) struct StaticFiles {
    root: PathBuf,
}

#[derive(Debug)]
pub(crate) enum StaticOutcome {
    Found {
        body: Vec<u8>,
        content_type: &'static str,
        /// Size of the GET representation, even when `body` is empty for HEAD.
        length: usize,
    },
    NotFound,
    TooLarge,
    Io(io::Error),
}

impl StaticFiles {
    pub(crate) fn open(root: impl AsRef<Path>) -> io::Result<Self> {
        let root = fs::canonicalize(root)?;
        let metadata = fs::metadata(&root)?;
        if !metadata.is_dir() {
            return Err(io::Error::new(
                io::ErrorKind::InvalidInput,
                "web root is not a directory",
            ));
        }
        Ok(Self { root })
    }

    pub(crate) fn serve(&self, url_path: &str, head: bool) -> StaticOutcome {
        let Some(relative) = relative_path(url_path) else {
            return StaticOutcome::NotFound;
        };
        let candidate = self.root.join(relative);
        let Some(resolved) = self.resolve(&candidate) else {
            return StaticOutcome::NotFound;
        };
        let metadata = match fs::metadata(&resolved) {
            Ok(metadata) => metadata,
            Err(error) => return StaticOutcome::Io(error),
        };
        if metadata.len() > MAX_STATIC_BYTES {
            return StaticOutcome::TooLarge;
        }
        let content_type = content_type(&resolved);
        let length = metadata.len() as usize;
        if head {
            return StaticOutcome::Found {
                body: Vec::new(),
                content_type,
                length,
            };
        }
        match fs::read(&resolved) {
            Ok(body) => StaticOutcome::Found {
                body,
                content_type,
                length,
            },
            Err(error) => StaticOutcome::Io(error),
        }
    }

    fn resolve(&self, candidate: &Path) -> Option<PathBuf> {
        if let Some(path) = self.file(candidate) {
            return Some(path);
        }
        if fs::metadata(candidate).is_ok_and(|metadata| metadata.is_dir()) {
            if let Some(path) = self.file(&candidate.join("index.html")) {
                return Some(path);
            }
        }
        if candidate.extension().is_none() {
            if let Some(path) = self.file(&candidate.with_extension("html")) {
                return Some(path);
            }
        }
        None
    }

    /// Canonicalize a candidate and require a regular file inside the root.
    /// The canonical check is the authoritative defense against symlinks,
    /// junctions, and platform-specific path normalization escaping the root.
    fn file(&self, candidate: &Path) -> Option<PathBuf> {
        let resolved = fs::canonicalize(candidate).ok()?;
        if !resolved.starts_with(&self.root) {
            return None;
        }
        let metadata = fs::metadata(&resolved).ok()?;
        metadata.is_file().then_some(resolved)
    }
}

fn relative_path(url_path: &str) -> Option<PathBuf> {
    let path = url_path
        .split_once('?')
        .map_or(url_path, |(path, _query)| path);
    let path = path.strip_prefix('/')?;
    let decoded = percent_decode(path)?;
    if decoded.chars().any(char::is_control) {
        return None;
    }

    let mut relative = PathBuf::new();
    for segment in decoded.split('/') {
        if segment.is_empty() || segment == "." {
            continue;
        }
        if segment == ".."
            || segment.starts_with('.')
            || segment.ends_with('.')
            || segment.ends_with(' ')
            || segment.contains('\\')
            || segment.contains(':')
            || reserved_device(segment)
        {
            return None;
        }
        relative.push(segment);
    }
    Some(relative)
}

/// Decode percent escapes exactly once. A second decoding pass would turn a
/// literal `%2e%2e` filename into `..`, which is not what the client asked for.
fn percent_decode(input: &str) -> Option<String> {
    let bytes = input.as_bytes();
    let mut output = Vec::with_capacity(bytes.len());
    let mut index = 0;
    while index < bytes.len() {
        if bytes[index] == b'%' {
            let high = hex(bytes.get(index + 1).copied()?)?;
            let low = hex(bytes.get(index + 2).copied()?)?;
            output.push((high << 4) | low);
            index += 3;
        } else {
            output.push(bytes[index]);
            index += 1;
        }
    }
    String::from_utf8(output).ok()
}

fn hex(byte: u8) -> Option<u8> {
    match byte {
        b'0'..=b'9' => Some(byte - b'0'),
        b'a'..=b'f' => Some(byte - b'a' + 10),
        b'A'..=b'F' => Some(byte - b'A' + 10),
        _ => None,
    }
}

/// Windows resolves these names to devices even without a device prefix. The
/// check runs on every platform so a web root behaves the same when copied
/// between hosts.
fn reserved_device(segment: &str) -> bool {
    let stem = segment
        .split('.')
        .next()
        .unwrap_or(segment)
        .trim_end_matches(' ')
        .to_ascii_uppercase();
    matches!(
        stem.as_str(),
        "CON" | "PRN" | "AUX" | "NUL" | "CLOCK$" | "CONIN$" | "CONOUT$"
    ) || (stem.len() == 4
        && (stem.starts_with("COM") || stem.starts_with("LPT"))
        && stem.as_bytes()[3].is_ascii_digit())
}

fn content_type(path: &Path) -> &'static str {
    let extension = path
        .extension()
        .and_then(|extension| extension.to_str())
        .unwrap_or_default()
        .to_ascii_lowercase();
    match extension.as_str() {
        "html" | "htm" => "text/html; charset=utf-8",
        "css" => "text/css; charset=utf-8",
        "js" | "mjs" => "text/javascript; charset=utf-8",
        "json" | "map" => "application/json",
        "webmanifest" => "application/manifest+json",
        "svg" => "image/svg+xml",
        "png" => "image/png",
        "jpg" | "jpeg" => "image/jpeg",
        "gif" => "image/gif",
        "webp" => "image/webp",
        "avif" => "image/avif",
        "ico" => "image/x-icon",
        "woff" => "font/woff",
        "woff2" => "font/woff2",
        "ttf" => "font/ttf",
        "otf" => "font/otf",
        "wasm" => "application/wasm",
        "txt" | "md" => "text/plain; charset=utf-8",
        "xml" => "application/xml",
        "pdf" => "application/pdf",
        _ => "application/octet-stream",
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn web_root() -> (tempfile::TempDir, StaticFiles) {
        let dir = tempfile::tempdir().unwrap();
        fs::write(dir.path().join("index.html"), b"index page").unwrap();
        fs::write(dir.path().join("console.html"), b"console page").unwrap();
        fs::create_dir(dir.path().join("assets")).unwrap();
        fs::write(dir.path().join("assets/app.js"), b"console.log(1);").unwrap();
        fs::write(dir.path().join("assets/index.html"), b"assets index").unwrap();
        let files = StaticFiles::open(dir.path()).unwrap();
        (dir, files)
    }

    fn found(outcome: StaticOutcome) -> (Vec<u8>, &'static str) {
        match outcome {
            StaticOutcome::Found {
                body, content_type, ..
            } => (body, content_type),
            other => panic!("expected a file, got {other:?}"),
        }
    }

    #[test]
    fn serves_index_clean_urls_and_directory_indexes() {
        let (_dir, files) = web_root();
        assert_eq!(found(files.serve("/", false)).0, b"index page");
        assert_eq!(found(files.serve("/console", false)).0, b"console page");
        assert_eq!(
            found(files.serve("/console.html", false)).0,
            b"console page"
        );
        assert_eq!(found(files.serve("/assets", false)).0, b"assets index");
        let (body, content_type) = found(files.serve("/assets/app.js", false));
        assert_eq!(body, b"console.log(1);");
        assert_eq!(content_type, "text/javascript; charset=utf-8");
        match files.serve("/", true) {
            StaticOutcome::Found { body, length, .. } => {
                assert!(body.is_empty());
                assert_eq!(length, b"index page".len());
            }
            other => panic!("expected a HEAD result, got {other:?}"),
        }
    }

    #[test]
    fn percent_decodes_safe_paths_and_rejects_bad_escapes() {
        let (_dir, files) = web_root();
        assert_eq!(found(files.serve("/con%73ole", false)).0, b"console page");
        assert!(matches!(
            files.serve("/%ZZ", false),
            StaticOutcome::NotFound
        ));
        assert!(matches!(
            files.serve("/%00", false),
            StaticOutcome::NotFound
        ));
    }

    #[test]
    fn rejects_traversal_dotfiles_backslashes_and_devices() {
        let (_dir, files) = web_root();
        for path in [
            "/../Cargo.toml",
            "/%2e%2e/Cargo.toml",
            "/..%2fCargo.toml",
            "/assets/../../Cargo.toml",
            "/.env",
            "/.git/config",
            "/assets\\..\\Cargo.toml",
            "/CON",
            "/CON .txt",
            "/nul.txt",
            "/COM1",
            "/lpt9.log",
            "/conin$",
            "/console.html:stream",
            "/console.html.",
            "/console.html ",
        ] {
            assert!(
                matches!(files.serve(path, false), StaticOutcome::NotFound),
                "{path} should not resolve"
            );
        }
    }

    #[test]
    fn missing_files_and_file_roots_are_rejected() {
        let (_dir, files) = web_root();
        assert!(matches!(
            files.serve("/missing", false),
            StaticOutcome::NotFound
        ));
        let dir = tempfile::tempdir().unwrap();
        let file = dir.path().join("index.html");
        fs::write(&file, b"x").unwrap();
        assert!(StaticFiles::open(&file).is_err());
        assert!(StaticFiles::open(dir.path().join("missing")).is_err());
    }

    #[test]
    fn maps_content_types() {
        assert_eq!(
            content_type(Path::new("a.html")),
            "text/html; charset=utf-8"
        );
        assert_eq!(content_type(Path::new("a.css")), "text/css; charset=utf-8");
        assert_eq!(content_type(Path::new("a.svg")), "image/svg+xml");
        assert_eq!(
            content_type(Path::new("a.unknown")),
            "application/octet-stream"
        );
    }

    #[cfg(unix)]
    #[test]
    fn rejects_symlinks_that_escape_the_root() {
        use std::os::unix::fs::symlink;

        let (dir, files) = web_root();
        let outside = tempfile::tempdir().unwrap();
        fs::write(outside.path().join("secret"), b"secret").unwrap();
        symlink(outside.path().join("secret"), dir.path().join("leak")).unwrap();
        assert!(matches!(
            files.serve("/leak", false),
            StaticOutcome::NotFound
        ));
        symlink(dir.path().join("index.html"), dir.path().join("alias")).unwrap();
        assert_eq!(found(files.serve("/alias", false)).0, b"index page");
    }
}
