//! Content-addressed blob storage for docker-rs.
//!
//! Blobs live under `<root>/sha256/<64-hex>` — the same shape as the OCI
//! image layout blobs directory (upstream `image-spec/specs-go/v1/layout.go`
//! constant `ImageBlobsDir = "blobs"` plus the algorithm directory), so a
//! store root IS an OCI blobs directory and can be exported as part of an
//! image layout directly.
//!
//! Integrity rules (verified by tests):
//! - writes are verified when the caller expects a digest; a mismatch never
//!   touches disk;
//! - content addressing is enforced on duplicates: an existing blob name
//!   whose bytes hash differently is store corruption, reported as
//!   `Internal` — never silently overwritten;
//! - reads can re-verify on demand (`get_verified`);
//! - foreign filenames in the algorithm directory (except dot-prefixed temp
//!   leftovers from a crashed write) are store corruption.

use std::fs;
use std::path::{Path, PathBuf};

use docker_common::{rng, Digest};
use docker_errors::{Error, Result};

/// File name of the layout marker (upstream `ImageLayoutFile`).
pub const IMAGE_LAYOUT_FILE: &str = "oci-layout";
/// Directory containing content-addressable blobs (upstream `ImageBlobsDir`).
pub const BLOBS_DIR: &str = "blobs";
/// Layout version understood (upstream `ImageLayoutVersion`).
pub const IMAGE_LAYOUT_VERSION: &str = "1.0.0";

/// Content-addressed blob store rooted at `<root>/sha256/...`.
#[derive(Debug)]
pub struct BlobStore {
    root: PathBuf,
}

/// Listing entry of a stored blob.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct BlobInfo {
    pub digest: Digest,
    pub size: u64,
}

impl BlobStore {
    /// Open (and create if missing) a blob store at `root`. The root must be
    /// absolute — relative store roots would silently fork content between
    /// daemon working directories.
    pub fn open(root: PathBuf) -> Result<Self> {
        if !root.is_absolute() {
            return Err(Error::invalid_argument(format!(
                "blob store root `{}` must be an absolute path",
                root.display()
            )));
        }
        fs::create_dir_all(root.join("sha256"))?;
        Ok(Self { root })
    }

    pub fn root(&self) -> &Path {
        &self.root
    }

    /// On-disk path of a blob (`<root>/sha256/<hex>`).
    pub fn blob_path(&self, digest: &Digest) -> PathBuf {
        self.root.join(digest.algor().as_str()).join(digest.hex())
    }

    /// Store `data`, returning its digest and size. Re-storing identical
    /// content is a no-op (content addressing), even across processes.
    pub fn put(&self, data: &[u8]) -> Result<(Digest, u64)> {
        let digest = Digest::sha256(data);
        let target = self.blob_path(&digest);
        if target.exists() {
            // Content addressing: an existing blob with this name must hold
            // identical bytes; anything else is store corruption.
            let existing = fs::read(&target)?;
            let existing_digest = Digest::sha256(&existing);
            if existing_digest != digest {
                return Err(Error::internal(format!(
                    "content-addressed blob {} already exists with different content (hashes to {}); store corrupted",
                    digest.full(),
                    existing_digest.full()
                )));
            }
            return Ok((digest, data.len() as u64));
        }
        let tmp = target.with_file_name(format!(".tmp-{}", rng::random_hex(24)?));
        {
            use std::io::Write as _;
            let mut f = fs::File::create(&tmp)?;
            f.write_all(data)?;
            f.sync_all()?;
        }
        fs::rename(&tmp, &target)?;
        Ok((digest, data.len() as u64))
    }

    /// Store `data`, verifying it matches `expected` before touching disk.
    pub fn put_verified(&self, expected: &Digest, data: &[u8]) -> Result<(Digest, u64)> {
        let computed = Digest::sha256(data);
        if computed != *expected {
            return Err(Error::invalid_argument(format!(
                "digest mismatch: expected {}, got {}",
                expected.full(),
                computed.full()
            )));
        }
        self.put(data)
    }

    /// Read a blob. Missing blobs are `NotFound`.
    pub fn get(&self, digest: &Digest) -> Result<Vec<u8>> {
        let target = self.blob_path(digest);
        if !target.exists() {
            return Err(Error::not_found(format!("no such blob: {}", digest.full())));
        }
        Ok(fs::read(&target)?)
    }

    /// Read a blob and re-verify its digest — protects against on-disk
    /// corruption; raises `Internal` on mismatch.
    pub fn get_verified(&self, digest: &Digest) -> Result<Vec<u8>> {
        let data = self.get(digest)?;
        let computed = Digest::sha256(&data);
        if computed != *digest {
            return Err(Error::internal(format!(
                "content-addressed blob {} exists with different content (hashes to {}); store corrupted",
                digest.full(),
                computed.full()
            )));
        }
        Ok(data)
    }

    pub fn exists(&self, digest: &Digest) -> bool {
        self.blob_path(digest).exists()
    }

    /// Size in bytes of a stored blob.
    pub fn stat(&self, digest: &Digest) -> Result<u64> {
        let target = self.blob_path(digest);
        if !target.exists() {
            return Err(Error::not_found(format!("no such blob: {}", digest.full())));
        }
        Ok(fs::metadata(&target)?.len())
    }

    pub fn delete(&self, digest: &Digest) -> Result<()> {
        let target = self.blob_path(digest);
        if !target.exists() {
            return Err(Error::not_found(format!("no such blob: {}", digest.full())));
        }
        fs::remove_file(&target)?;
        Ok(())
    }

    /// List all stored blobs, sorted by digest. Dot-prefixed names are
    /// treated as leftover temp files from a crashed write and skipped;
    /// any other foreign filename is store corruption.
    pub fn list(&self) -> Result<Vec<BlobInfo>> {
        let mut out = Vec::new();
        for entry in fs::read_dir(self.root.join("sha256"))? {
            let entry = entry?;
            let path = entry.path();
            if !path.is_file() {
                continue;
            }
            let name = match path.file_name().and_then(|n| n.to_str()) {
                Some(n) => n.to_string(),
                None => {
                    return Err(Error::internal(format!(
                        "content-addressed store contains non-UTF-8 entry `{}`",
                        path.display()
                    )))
                }
            };
            if name.starts_with('.') {
                continue;
            }
            let digest = Digest::parse(&format!("sha256:{name}")).map_err(|e| {
                Error::internal(format!(
                    "content-addressed store contains unexpected entry `{name}`: {}",
                    e.message()
                ))
            })?;
            let size = fs::metadata(&path)?.len();
            out.push(BlobInfo { digest, size });
        }
        out.sort_by_key(|a| a.digest.full());
        Ok(out)
    }
}

/// Parse an `oci-layout` marker and return its `imageLayoutVersion`.
fn parse_layout_marker(contents: &str) -> Result<String> {
    let v: serde_json::Value = serde_json::from_str(contents).map_err(|e| {
        Error::invalid_argument(format!(
            "invalid `{IMAGE_LAYOUT_FILE}` marker: not valid JSON: {e}"
        ))
    })?;
    let obj = v.as_object().ok_or_else(|| {
        Error::invalid_argument(format!(
            "invalid `{IMAGE_LAYOUT_FILE}` marker: not a JSON object"
        ))
    })?;
    if obj.len() != 1 {
        return Err(Error::invalid_argument(format!(
            "invalid `{IMAGE_LAYOUT_FILE}` marker: expected only `imageLayoutVersion`, got {} keys",
            obj.len()
        )));
    }
    let ver = obj
        .get("imageLayoutVersion")
        .ok_or_else(|| {
            Error::invalid_argument(format!(
                "invalid `{IMAGE_LAYOUT_FILE}` marker: missing `imageLayoutVersion`"
            ))
        })?
        .as_str()
        .ok_or_else(|| {
            Error::invalid_argument(format!(
                "invalid `{IMAGE_LAYOUT_FILE}` marker: `imageLayoutVersion` must be a string"
            ))
        })?;
    Ok(ver.to_string())
}

/// Create (or verify) the OCI image layout skeleton at `dir`:
/// `dir/oci-layout` marker plus `dir/blobs/sha256/`. Returns the blob store
/// rooted at `dir/blobs`.
pub fn init_image_layout(dir: &Path) -> Result<BlobStore> {
    fs::create_dir_all(dir)?;
    let blobs = dir.join(BLOBS_DIR);
    fs::create_dir_all(blobs.join("sha256"))?;
    let marker = dir.join(IMAGE_LAYOUT_FILE);
    match fs::read_to_string(&marker) {
        Ok(existing) => {
            let ver = parse_layout_marker(&existing)?;
            if ver != IMAGE_LAYOUT_VERSION {
                return Err(Error::unsupported(format!(
                    "oci-layout version `{ver}` unsupported; supported: {IMAGE_LAYOUT_VERSION}"
                )));
            }
        }
        Err(_) => {
            fs::write(
                &marker,
                format!("{{\"imageLayoutVersion\":\"{IMAGE_LAYOUT_VERSION}\"}}"),
            )?;
        }
    }
    BlobStore::open(blobs)
}

/// Verify that `dir` is an OCI image layout with a supported marker.
pub fn check_image_layout(dir: &Path) -> Result<()> {
    let existing = fs::read_to_string(dir.join(IMAGE_LAYOUT_FILE)).map_err(|e| {
        Error::not_found(format!("no OCI image layout at `{}`: {e}", dir.display()))
    })?;
    let ver = parse_layout_marker(&existing)?;
    if ver != IMAGE_LAYOUT_VERSION {
        return Err(Error::unsupported(format!(
            "oci-layout version `{ver}` unsupported; supported: {IMAGE_LAYOUT_VERSION}"
        )));
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn tmp_store() -> (tempfile::TempDir, BlobStore) {
        let dir = tempfile::tempdir().unwrap();
        let store = BlobStore::open(dir.path().to_path_buf()).unwrap();
        (dir, store)
    }

    #[test]
    fn open_rejects_relative_roots() {
        let err = BlobStore::open(PathBuf::from("relative/dir")).unwrap_err();
        assert_eq!(err.kind(), docker_errors::ErrorKind::InvalidArgument);
    }

    #[test]
    fn put_get_round_trip_with_known_sha256() {
        let (_dir, store) = tmp_store();
        let (digest, size) = store.put(b"hello world").unwrap();
        // sha256("hello world"), known constant — independent of our hasher.
        assert_eq!(
            digest.full(),
            "sha256:b94d27b9934d3e08a52e52d7da7dabfac484efe37a5380ee9088f7ace2efcde9"
        );
        assert_eq!(size, 11);
        assert!(store.exists(&digest));
        assert_eq!(store.get(&digest).unwrap(), b"hello world");
        assert_eq!(store.stat(&digest).unwrap(), 11);
        assert!(store.blob_path(&digest).exists());
    }

    #[test]
    fn put_verified_rejects_mismatch_and_writes_nothing() {
        let (_dir, store) = tmp_store();
        let wrong = Digest::sha256(b"not-the-content");
        let err = store.put_verified(&wrong, b"actual-content").unwrap_err();
        assert_eq!(err.kind(), docker_errors::ErrorKind::InvalidArgument);
        assert!(err.message().contains("digest mismatch"));
        assert!(err.message().contains("expected"));
        assert_eq!(store.list().unwrap().len(), 0);
    }

    #[test]
    fn put_verified_accepts_matching_expectation() {
        let (_dir, store) = tmp_store();
        let expected = Digest::sha256(b"actual-content");
        let (d, s) = store.put_verified(&expected, b"actual-content").unwrap();
        assert_eq!(d, expected);
        assert_eq!(s, 14);
    }

    #[test]
    fn duplicate_put_is_dedup() {
        let (_dir, store) = tmp_store();
        let (d1, _) = store.put(b"same").unwrap();
        let (d2, _) = store.put(b"same").unwrap();
        assert_eq!(d1, d2);
        assert_eq!(store.list().unwrap().len(), 1);
    }

    #[test]
    fn get_missing_is_not_found() {
        let (_dir, store) = tmp_store();
        let d = Digest::sha256(b"never-stored");
        assert_eq!(
            store.get(&d).unwrap_err().kind(),
            docker_errors::ErrorKind::NotFound
        );
        assert_eq!(
            store.stat(&d).unwrap_err().kind(),
            docker_errors::ErrorKind::NotFound
        );
        assert_eq!(
            store.delete(&d).unwrap_err().kind(),
            docker_errors::ErrorKind::NotFound
        );
    }

    #[test]
    fn get_verified_detects_corruption() {
        let (_dir, store) = tmp_store();
        let (d, _) = store.put(b"original").unwrap();
        // Simulate on-disk corruption behind the store's back.
        fs::write(store.blob_path(&d), b"tampered").unwrap();
        assert!(store.get(&d).is_ok()); // unverified read still serves bytes
        let err = store.get_verified(&d).unwrap_err();
        assert_eq!(err.kind(), docker_errors::ErrorKind::Internal);
        assert!(err.message().contains("store corrupted"));
    }

    #[test]
    fn delete_removes_blob() {
        let (_dir, store) = tmp_store();
        let (d, _) = store.put(b"gone").unwrap();
        store.delete(&d).unwrap();
        assert!(!store.exists(&d));
        assert_eq!(
            store.get(&d).unwrap_err().kind(),
            docker_errors::ErrorKind::NotFound
        );
    }

    #[test]
    fn list_is_strict_about_foreign_names() {
        let (_dir, store) = tmp_store();
        store.put(b"q").unwrap();
        // Dot-prefixed leftovers are tolerated (crashed write).
        fs::write(store.root().join("sha256").join(".tmp-leftover"), b"junk").unwrap();
        assert_eq!(store.list().unwrap().len(), 1);
        // Any other foreign name is corruption.
        fs::write(store.root().join("sha256").join("nothex"), b"junk").unwrap();
        let err = store.list().unwrap_err();
        assert_eq!(err.kind(), docker_errors::ErrorKind::Internal);
        assert!(err.message().contains("unexpected entry"));
    }

    #[test]
    fn list_is_sorted_by_digest() {
        let (_dir, store) = tmp_store();
        let a = store.put(b"a").unwrap().0;
        let b = store.put(b"b").unwrap().0;
        let z = store.put(b"z").unwrap().0;
        let digests: Vec<String> = store
            .list()
            .unwrap()
            .into_iter()
            .map(|i| i.digest.full())
            .collect();
        let mut sorted = digests.clone();
        sorted.sort();
        assert_eq!(digests, sorted);
        assert_eq!(digests.len(), 3);
        assert!(digests.contains(&a.full()));
        assert!(digests.contains(&b.full()));
        assert!(digests.contains(&z.full()));
    }

    #[test]
    fn image_layout_init_and_idempotence() {
        let dir = tempfile::tempdir().unwrap();
        let store = init_image_layout(dir.path()).unwrap();
        let marker = fs::read_to_string(dir.path().join(IMAGE_LAYOUT_FILE)).unwrap();
        assert_eq!(marker, r#"{"imageLayoutVersion":"1.0.0"}"#);
        assert!(dir.path().join(BLOBS_DIR).join("sha256").exists());
        store.put(b"blob").unwrap();
        // Re-init with an intact marker is idempotent and reopens the store.
        let again = init_image_layout(dir.path()).unwrap();
        assert_eq!(again.list().unwrap().len(), 1);
        check_image_layout(dir.path()).unwrap();
    }

    #[test]
    fn image_layout_rejects_unsupported_and_invalid_markers() {
        let dir = tempfile::tempdir().unwrap();
        fs::write(
            dir.path().join(IMAGE_LAYOUT_FILE),
            r#"{"imageLayoutVersion":"9.9.9"}"#,
        )
        .unwrap();
        let err = init_image_layout(dir.path()).unwrap_err();
        assert_eq!(err.kind(), docker_errors::ErrorKind::Unsupported);
        assert!(err.message().contains("9.9.9"));

        fs::write(
            dir.path().join(IMAGE_LAYOUT_FILE),
            r#"{"imageLayoutVersion":"1.0.0","extra":true}"#,
        )
        .unwrap();
        assert!(init_image_layout(dir.path()).is_err());

        fs::write(dir.path().join(IMAGE_LAYOUT_FILE), "not json").unwrap();
        assert!(init_image_layout(dir.path()).is_err());
    }

    #[test]
    fn check_image_layout_missing_is_not_found() {
        let dir = tempfile::tempdir().unwrap();
        assert_eq!(
            check_image_layout(dir.path()).unwrap_err().kind(),
            docker_errors::ErrorKind::NotFound
        );
    }
}
