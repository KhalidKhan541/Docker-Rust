//! Persistent local image records and their content-addressed blobs.
//!
//! The metadata index is a small JSON file next to a [`BlobStore`]. Manifest,
//! config and layer bytes are content-addressed and verified by the store;
//! the JSON index only names references and records platform metadata.
//!
//! Writes are serialized through a mutex and persisted by writing a temporary
//! file followed by a rename. Windows cannot atomically replace an existing
//! file with `fs::rename`, so the old index is removed first there; a crash
//! between remove and rename leaves the previous index recoverable only from
//! backup media. This limitation is documented in API_COMPATIBILITY.md rather
//! than hidden.

use std::collections::BTreeSet;
use std::fs;
use std::path::{Path, PathBuf};
use std::sync::Mutex;
use std::time::{SystemTime, UNIX_EPOCH};

use docker_common::{rng, Digest};
use docker_errors::{Error, ErrorKind, Result};
use docker_image::{Descriptor, Image, Platform};
use docker_storage::BlobStore;
use serde::{Deserialize, Serialize};

use crate::reference::{canonical_repo_digest, canonical_tag, familiar_reference};

const INDEX_VERSION: u32 = 1;

/// One locally available image. `id` is the image config digest (Docker's
/// image ID); `manifest_digest` is the top-level manifest/index digest used
/// for repository digests.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ImageRecord {
    pub id: Digest,
    pub manifest_digest: Digest,
    /// Every manifest blob fetched for this image (top-level index plus the
    /// platform manifest, when the registry returned a multi-platform index).
    #[serde(default)]
    pub manifest_digests: Vec<Digest>,
    /// Canonical tags (`docker.io/library/alpine:latest`).
    #[serde(default)]
    pub tags: BTreeSet<String>,
    /// Canonical repository digests (`docker.io/library/alpine@sha256:...`).
    #[serde(default)]
    pub repo_digests: BTreeSet<String>,
    /// Unix seconds from the image config, or pull time when absent.
    pub created: i64,
    pub pulled_at: i64,
    /// Sum of config and compressed layer sizes.
    pub size: i64,
    pub config_size: i64,
    pub platform: Platform,
    pub config: Image,
    pub layers: Vec<Descriptor>,
}

impl ImageRecord {
    /// Validate the parts of the record that the image spec makes mandatory
    /// and that the rest of the daemon relies on.
    pub fn validate(&self) -> Result<()> {
        self.config.validate()?;
        if self.config.rootfs.diff_ids.len() != self.layers.len() {
            return Err(Error::invalid_argument(format!(
                "image {} has {} rootfs diff_ids but {} layers",
                self.id.full(),
                self.config.rootfs.diff_ids.len(),
                self.layers.len()
            )));
        }
        if self.size < 0 || self.config_size < 0 {
            return Err(Error::invalid_argument("image sizes must be non-negative"));
        }
        for tag in &self.tags {
            if tag.contains('@') {
                return Err(Error::invalid_argument(format!(
                    "image tag {tag:?} must not contain a digest"
                )));
            }
        }
        Ok(())
    }

    pub fn display_tags(&self) -> Vec<String> {
        self.tags
            .iter()
            .map(|tag| familiar_reference(tag))
            .collect()
    }

    pub fn display_repo_digests(&self) -> Vec<String> {
        self.repo_digests
            .iter()
            .map(|digest| familiar_reference(digest))
            .collect()
    }

    /// Docker short ID as displayed by `docker ps`/`docker images` consumers.
    pub fn short_id(&self) -> &str {
        &self.id.hex()[..12.min(self.id.hex().len())]
    }

    fn all_blob_digests(&self) -> impl Iterator<Item = &Digest> {
        std::iter::once(&self.id)
            .chain(self.manifest_digests.iter())
            .chain(self.layers.iter().map(|layer| &layer.digest))
    }
}

/// Result of a delete operation, shaped for Moby's `DeleteResponse` array.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct DeleteOutcome {
    pub untagged: Vec<String>,
    pub deleted: Vec<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
struct IndexFile {
    version: u32,
    images: Vec<ImageRecord>,
}

#[derive(Debug, Clone)]
struct Inner {
    images: Vec<ImageRecord>,
}

/// A local image store rooted at `<root>/images.json` with blobs under
/// `<root>/blobs`.
#[derive(Debug)]
pub struct ImageStore {
    root: PathBuf,
    index_path: PathBuf,
    blobs: BlobStore,
    inner: Mutex<Inner>,
}

impl ImageStore {
    /// Open or initialize a store. The root must be absolute so the daemon
    /// cannot accidentally read/write a store relative to its current
    /// directory.
    pub fn open(root: PathBuf) -> Result<Self> {
        if !root.is_absolute() {
            return Err(Error::invalid_argument(format!(
                "image store root `{}` must be an absolute path",
                root.display()
            )));
        }
        fs::create_dir_all(&root)?;
        let blobs = BlobStore::open(root.join("blobs"))?;
        let index_path = root.join("images.json");
        let inner = if index_path.exists() {
            let bytes = fs::read(&index_path)?;
            let file: IndexFile = serde_json::from_slice(&bytes).map_err(|error| {
                Error::internal(format!(
                    "image index `{}` is not valid JSON: {error}",
                    index_path.display()
                ))
            })?;
            if file.version != INDEX_VERSION {
                return Err(Error::unsupported(format!(
                    "image index version {} is not supported (expected {INDEX_VERSION})",
                    file.version
                )));
            }
            validate_index(&file.images)?;
            Inner {
                images: file.images,
            }
        } else {
            Inner { images: Vec::new() }
        };
        Ok(Self {
            root,
            index_path,
            blobs,
            inner: Mutex::new(inner),
        })
    }

    pub fn root(&self) -> &Path {
        &self.root
    }

    pub fn blob_store(&self) -> &BlobStore {
        &self.blobs
    }

    /// All images, including dangling images when `include_dangling` is true.
    pub fn list(&self, include_dangling: bool) -> Vec<ImageRecord> {
        let state = self.inner.lock().expect("image store lock poisoned");
        let mut images: Vec<ImageRecord> = state
            .images
            .iter()
            .filter(|image| include_dangling || !image.tags.is_empty())
            .cloned()
            .collect();
        images.sort_by(|left, right| {
            right
                .created
                .cmp(&left.created)
                .then_with(|| left.id.hex().cmp(right.id.hex()))
        });
        images
    }

    pub fn history(&self) -> Vec<ImageRecord> {
        self.inner
            .lock()
            .expect("image store lock poisoned")
            .images
            .clone()
    }

    /// Resolve a tag, repository digest, full/truncated image ID, or unique
    /// ID prefix. Exact tag/digest names win over ID-prefix interpretation,
    /// matching Docker's name-first lookup.
    pub fn resolve(&self, name: &str) -> Result<ImageRecord> {
        let name = name.trim().trim_start_matches('/');
        if name.is_empty() {
            return Err(Error::invalid_argument("image name must not be empty"));
        }
        let state = self.inner.lock().expect("image store lock poisoned");
        resolve_in(&state.images, name)
    }

    /// Insert a fully downloaded image. Returns true when an image with the
    /// same config digest already existed. A new tag moves from any previous
    /// image to this record.
    pub fn insert(&self, record: ImageRecord) -> Result<bool> {
        record.validate()?;
        let mut state = self.inner.lock().expect("image store lock poisoned");
        let mut next = state.clone();
        for image in &mut next.images {
            if image.id != record.id {
                image.tags.retain(|tag| !record.tags.contains(tag));
                image
                    .repo_digests
                    .retain(|digest| !record.repo_digests.contains(digest));
            }
        }
        let existing = next.images.iter().position(|image| image.id == record.id);
        let existed = existing.is_some();
        if let Some(index) = existing {
            let image = &mut next.images[index];
            image.tags.extend(record.tags);
            image.repo_digests.extend(record.repo_digests);
            for digest in record.manifest_digests {
                if !image.manifest_digests.contains(&digest) {
                    image.manifest_digests.push(digest);
                }
            }
        } else {
            next.images.push(record);
        }
        next.images
            .sort_by(|left, right| left.id.hex().cmp(right.id.hex()));
        self.persist(&next)?;
        *state = next;
        Ok(existed)
    }

    /// Add a tag to `name`. An existing destination tag is moved rather than
    /// duplicated.
    pub fn tag(&self, name: &str, repo: &str, tag: &str) -> Result<ImageRecord> {
        let destination = crate::reference::canonical_tag_from_parts(repo, tag)?;
        let mut state = self.inner.lock().expect("image store lock poisoned");
        let source = resolve_in(&state.images, name)?;
        let mut next = state.clone();
        for image in &mut next.images {
            if image.id != source.id {
                image.tags.remove(&destination);
            }
        }
        let image = next
            .images
            .iter_mut()
            .find(|image| image.id == source.id)
            .expect("resolved image must still exist");
        image.tags.insert(destination);
        let record = image.clone();
        self.persist(&next)?;
        *state = next;
        Ok(record)
    }

    /// Remove a tag or an image by ID. Returns the untagged/deleted names that
    /// Moby reports. `force` is required to remove an image referenced by more
    /// than one tag.
    pub fn delete(&self, name: &str, force: bool) -> Result<DeleteOutcome> {
        let name = name.trim().trim_start_matches('/');
        if name.is_empty() {
            return Err(Error::invalid_argument("image name must not be empty"));
        }
        let mut state = self.inner.lock().expect("image store lock poisoned");
        let record = resolve_in(&state.images, name)?;
        let tag_key = canonical_tag_for_lookup(name);
        let deleting_tag = tag_key
            .as_ref()
            .is_some_and(|tag| record.tags.contains(tag));
        let mut outcome = DeleteOutcome::default();
        let mut next = state.clone();
        let index = next
            .images
            .iter()
            .position(|image| image.id == record.id)
            .expect("resolved image must still exist");

        if deleting_tag {
            let tag = tag_key.expect("deleting_tag implies a tag key");
            next.images[index].tags.remove(&tag);
            outcome.untagged.push(familiar_reference(&tag));
        } else if !force {
            let repositories: BTreeSet<&str> = record
                .tags
                .iter()
                .filter_map(|tag| tag.rsplit_once(':').map(|(repo, _)| repo))
                .collect();
            if repositories.len() > 1 || record.tags.len() > 1 {
                return Err(Error::conflict(format!(
                    "conflict: unable to delete {} (must be forced) - image is referenced in multiple repositories",
                    record.short_id()
                )));
            }
        }

        let remove_record = !deleting_tag || next.images[index].tags.is_empty();
        if remove_record {
            for tag in &next.images[index].tags {
                outcome.untagged.push(familiar_reference(tag));
            }
            outcome.deleted.push(record.id.full());
            next.images.remove(index);
        }

        outcome.untagged.sort();
        outcome.untagged.dedup();
        self.persist(&next)?;
        *state = next;

        if remove_record {
            let referenced: BTreeSet<String> = state
                .images
                .iter()
                .flat_map(|image| image.all_blob_digests())
                .map(Digest::full)
                .collect();
            for digest in record.all_blob_digests() {
                if !referenced.contains(&digest.full()) {
                    match self.blobs.delete(digest) {
                        Ok(()) => {}
                        Err(error) if error.kind() == ErrorKind::NotFound => {}
                        Err(error) => {
                            tracing::warn!(digest = %digest, error = %error, "failed to garbage-collect image blob");
                        }
                    }
                }
            }
        }
        Ok(outcome)
    }

    fn persist(&self, state: &Inner) -> Result<()> {
        let file = IndexFile {
            version: INDEX_VERSION,
            images: state.images.clone(),
        };
        let bytes = serde_json::to_vec_pretty(&file)
            .map_err(|error| Error::internal(format!("failed to encode image index: {error}")))?;
        let temp = self.index_path.with_file_name(format!(
            ".images-{}.tmp",
            rng::random_hex(12).map_err(|error| Error::internal(error.message()))?
        ));
        fs::write(&temp, bytes)?;
        #[cfg(windows)]
        if self.index_path.exists() {
            fs::remove_file(&self.index_path)?;
        }
        fs::rename(&temp, &self.index_path)?;
        Ok(())
    }
}

fn validate_index(images: &[ImageRecord]) -> Result<()> {
    let mut ids = BTreeSet::new();
    let mut tags = BTreeSet::new();
    for image in images {
        image.validate().map_err(|error| {
            Error::internal(format!(
                "image index entry {} is invalid: {error}",
                image.id.full()
            ))
        })?;
        if !ids.insert(image.id.full()) {
            return Err(Error::internal(format!(
                "image index contains duplicate image ID {}",
                image.id.full()
            )));
        }
        for tag in &image.tags {
            if !tags.insert(tag.clone()) {
                return Err(Error::internal(format!(
                    "image index contains duplicate tag {tag}"
                )));
            }
        }
    }
    Ok(())
}

fn canonical_tag_for_lookup(name: &str) -> Option<String> {
    let image = docker_common::ImageRef::parse(name).ok()?;
    if image.digest().is_some() {
        return None;
    }
    canonical_tag(&image).ok()
}

fn resolve_in(images: &[ImageRecord], name: &str) -> Result<ImageRecord> {
    if let Ok(image) = docker_common::ImageRef::parse(name) {
        if let Some(digest) = image.digest() {
            let parsed =
                Digest::parse(digest).map_err(|error| Error::invalid_argument(error.message()))?;
            let repo_digest = canonical_repo_digest(&image, &parsed);
            if let Some(record) = images
                .iter()
                .find(|record| record.repo_digests.contains(&repo_digest))
            {
                return Ok(record.clone());
            }
            // A digest-pinned reference may use the platform manifest digest
            // instead of the top-level index digest.
            if let Some(record) = images.iter().find(|record| {
                record.manifest_digests.contains(&parsed)
                    && record.repo_digests.iter().any(|stored| {
                        stored.rsplit_once('@').is_some_and(|(repo, _)| {
                            repo == repo_digest.rsplit_once('@').unwrap_or(("", "")).0
                        })
                    })
            }) {
                return Ok(record.clone());
            }
            return Err(Error::not_found(format!("No such image: {name}")));
        }
        if let Ok(tag) = canonical_tag(&image) {
            if let Some(record) = images.iter().find(|record| record.tags.contains(&tag)) {
                return Ok(record.clone());
            }
        }
    }

    let hex = name.strip_prefix("sha256:").unwrap_or(name);
    if hex.len() >= 4 && hex.len() <= 64 && hex.bytes().all(|byte| byte.is_ascii_hexdigit()) {
        let hex = hex.to_ascii_lowercase();
        let matches: Vec<&ImageRecord> = images
            .iter()
            .filter(|record| record.id.hex().starts_with(&hex))
            .collect();
        return match matches.as_slice() {
            [] => Err(Error::not_found(format!("No such image: {name}"))),
            [record] => Ok((*record).clone()),
            _ => Err(Error::conflict(format!(
                "image ID prefix {name:?} is ambiguous ({} matches)",
                matches.len()
            ))),
        };
    }

    Err(Error::not_found(format!("No such image: {name}")))
}

pub(crate) fn unix_now() -> i64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|elapsed| elapsed.as_secs() as i64)
        .unwrap_or(0)
}

#[cfg(test)]
mod tests {
    use super::*;
    use docker_image::{RootFS, SCHEMA_VERSION};

    fn digest(hex: char) -> Digest {
        Digest::parse(&format!("sha256:{}", hex.to_string().repeat(64))).unwrap()
    }

    fn record() -> ImageRecord {
        let layer = Descriptor {
            media_type: docker_image::MEDIA_TYPE_IMAGE_LAYER_GZIP.into(),
            digest: digest('b'),
            size: 123,
            ..Descriptor::default()
        };
        ImageRecord {
            id: digest('a'),
            manifest_digest: digest('c'),
            manifest_digests: vec![digest('c')],
            tags: [crate::reference::canonical_tag(
                &docker_common::ImageRef::parse("alpine:3.20").unwrap(),
            )
            .unwrap()]
            .into_iter()
            .collect(),
            repo_digests: [crate::reference::canonical_repo_digest(
                &docker_common::ImageRef::parse("alpine:3.20").unwrap(),
                &digest('c'),
            )]
            .into_iter()
            .collect(),
            created: 1_700_000_000,
            pulled_at: 1_700_000_001,
            size: 200,
            config_size: 77,
            platform: Platform {
                os: "linux".into(),
                architecture: "amd64".into(),
                ..Platform::default()
            },
            config: Image {
                architecture: "amd64".into(),
                os: "linux".into(),
                rootfs: RootFS::layers(vec![digest('d')]),
                ..Image::default()
            },
            layers: vec![layer],
        }
    }

    fn store() -> (tempfile::TempDir, ImageStore) {
        let dir = tempfile::tempdir().unwrap();
        let store = ImageStore::open(dir.path().join("images")).unwrap();
        (dir, store)
    }

    #[test]
    fn inserts_persists_and_resolves_tags_ids_and_prefixes() {
        let (dir, store) = store();
        assert!(!store.insert(record()).unwrap());
        let resolved = store.resolve("alpine:3.20").unwrap();
        assert_eq!(resolved.id, digest('a'));
        assert_eq!(store.resolve("alpine:3.20").unwrap().id, digest('a'));
        assert_eq!(store.resolve("aaaaaaaaaaaa").unwrap().id, digest('a'));
        assert_eq!(
            store
                .resolve("sha256:aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa")
                .unwrap()
                .id,
            digest('a')
        );
        assert!(store.insert(record()).unwrap());

        drop(store);
        let reopened = ImageStore::open(dir.path().join("images")).unwrap();
        assert_eq!(reopened.resolve("alpine:3.20").unwrap().id, digest('a'));
        assert_eq!(reopened.list(false).len(), 1);
    }

    #[test]
    fn tag_moves_between_images() {
        let (_dir, store) = store();
        store.insert(record()).unwrap();
        let mut second = record();
        second.id = digest('e');
        second.manifest_digest = digest('f');
        second.manifest_digests = vec![digest('f')];
        second.tags.clear();
        second.repo_digests.clear();
        store.insert(second).unwrap();
        let tagged = store.tag("eeeeeeee", "team/app", "dev").unwrap();
        assert!(tagged.tags.iter().any(|tag| tag.ends_with("team/app:dev")));
        let moved = store.tag("team/app:dev", "alpine", "3.20").unwrap();
        assert_eq!(moved.id, digest('e'));
        assert!(moved.tags.iter().any(|tag| tag.ends_with("alpine:3.20")));
        assert!(store.list(false).len() == 1);
        assert!(store
            .list(true)
            .iter()
            .any(|image| image.id == digest('a') && image.tags.is_empty()));
    }

    #[test]
    fn delete_untags_last_tag_and_garbage_collects() {
        let (_dir, store) = store();
        store.insert(record()).unwrap();
        let outcome = store.delete("alpine:3.20", false).unwrap();
        assert_eq!(outcome.untagged, vec!["alpine:3.20"]);
        assert_eq!(outcome.deleted, vec![digest('a').full()]);
        assert!(store.list(true).is_empty());
        assert!(matches!(
            store.resolve("aaaaaaaa").unwrap_err().kind(),
            ErrorKind::NotFound
        ));
    }

    #[test]
    fn delete_id_with_multiple_tags_requires_force() {
        let (_dir, store) = store();
        store.insert(record()).unwrap();
        store.tag("aaaaaaaa", "team/app", "dev").unwrap();
        let error = store.delete("aaaaaaaa", false).unwrap_err();
        assert_eq!(error.kind(), ErrorKind::Conflict);
        let outcome = store.delete("aaaaaaaa", true).unwrap();
        assert_eq!(outcome.deleted, vec![digest('a').full()]);
        assert_eq!(outcome.untagged.len(), 2);
        assert!(store.list(true).is_empty());
    }

    #[test]
    fn resolve_prefers_exact_tag_over_id_prefix() {
        let (_dir, store) = store();
        let mut image = record();
        image.tags = ["docker.io/library/deadbeef:latest".to_string()]
            .into_iter()
            .collect();
        store.insert(image).unwrap();
        // `deadbeef` is a valid tag name and not the image ID; it must resolve
        // as a tag rather than a nonexistent ID prefix.
        assert_eq!(store.resolve("deadbeef").unwrap().id, digest('a'));
    }

    #[test]
    fn mismatched_diff_ids_are_rejected() {
        let (_dir, store) = store();
        let mut image = record();
        image.config.rootfs = RootFS::layers(vec![]);
        image.config.rootfs.rootfs_type = RootFS::TYPE_LAYERS.to_string();
        image.layers = vec![Descriptor {
            media_type: docker_image::MEDIA_TYPE_IMAGE_LAYER_GZIP.into(),
            digest: digest('b'),
            size: 1,
            ..Descriptor::default()
        }];
        assert!(store.insert(image).is_err());
        assert_eq!(SCHEMA_VERSION, 2);
    }
}
