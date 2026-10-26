//! Docker save/load (`/images/get`, `/images/load`) archive support.
//!
//! Export writes the OCI layout used by current Docker (`oci-layout`,
//! `index.json`, `blobs/...`) plus the legacy `manifest.json` compatibility
//! file. Layer blobs are copied byte-for-byte from the verified content store;
//! the loader understands gzip-compressed layers exactly like Moby's
//! `tarexport` loader, so imported archives preserve their original digests.
//!
//! Import never extracts symlinks or device nodes, never follows `..`, and
//! bounds both the number of entries and the bytes written to the temporary
//! extraction directory.

use std::collections::{BTreeMap, BTreeSet};
use std::fs::{self, File, OpenOptions};
use std::io::{self, BufReader, Read, Seek, SeekFrom, Write};
use std::path::{Component, Path, PathBuf};

use docker_common::{Digest, ImageRef};
use docker_errors::{Error, Result};
use docker_image::{
    Descriptor, Image, Index, Manifest, Platform, RootFS, MEDIA_TYPE_IMAGE_CONFIG,
    MEDIA_TYPE_IMAGE_LAYER, MEDIA_TYPE_IMAGE_MANIFEST,
};
use flate2::read::GzDecoder;
use serde::{Deserialize, Serialize};
use sha2::Digest as _;
use tar::{Archive, Builder, Header};

use crate::reference::{
    canonical_registry_repo, canonical_tag, familiar_reference, platform_matches,
};
use crate::store::{unix_now, ImageRecord, ImageStore};

const OCI_LAYOUT: &str = r#"{"imageLayoutVersion":"1.0.0"}"#;
const MAX_ARCHIVE_ENTRIES: usize = 100_000;
const MAX_METADATA_BYTES: u64 = 16 * 1024 * 1024;
const MAX_EXTRACTED_BYTES: u64 = 64 * 1024 * 1024 * 1024;

/// One image reported by `/images/load`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct LoadedImage {
    pub id: Digest,
    pub tags: Vec<String>,
}

/// Result of a successful archive import.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct LoadOutcome {
    pub images: Vec<LoadedImage>,
}

#[derive(Debug, Serialize, Deserialize)]
struct LegacyManifestItem {
    #[serde(rename = "Config")]
    config: String,
    #[serde(rename = "RepoTags", default)]
    repo_tags: Vec<String>,
    #[serde(rename = "Layers", default)]
    layers: Vec<String>,
}

#[derive(Debug)]
struct SaveTarget {
    record: ImageRecord,
    tags: BTreeSet<String>,
}

#[derive(Debug)]
struct LoadedImageData {
    config_bytes: Vec<u8>,
    config: Image,
    platform: Platform,
    layers: Vec<Descriptor>,
    layer_paths: Vec<PathBuf>,
    manifest_bytes: Vec<u8>,
    manifest_digest: Digest,
    tags: BTreeSet<String>,
}

impl ImageStore {
    /// Write one or more images as a Docker/OCI tar archive.
    ///
    /// `names` accepts tags, repository names (all tags), image IDs, and
    /// digest references, matching `docker save`. An optional platform must
    /// match the stored single-platform image.
    pub fn export_archive<W: Write>(
        &self,
        names: &[String],
        platform: Option<&Platform>,
        out: W,
    ) -> Result<()> {
        if names.is_empty() {
            return Err(Error::invalid_argument("image name cannot be blank"));
        }
        let targets = self.save_targets(names, platform)?;
        let mut builder = Builder::new(out);

        let mut blobs: BTreeMap<String, Digest> = BTreeMap::new();
        let mut generated: BTreeMap<String, Vec<u8>> = BTreeMap::new();
        let mut legacy = Vec::with_capacity(targets.len());
        let mut index = Index::new();

        for target in &targets {
            let record = &target.record;
            let config_bytes = self.blob_store().get_verified(&record.id)?;
            blobs.insert(record.id.full(), record.id.clone());
            generated.insert(record.id.full(), config_bytes);

            let (manifest_digest, manifest_bytes) = self.export_manifest(record)?;
            blobs.insert(manifest_digest.full(), manifest_digest.clone());
            generated.insert(manifest_digest.full(), manifest_bytes);

            for layer in &record.layers {
                if !self.blob_store().exists(&layer.digest) {
                    return Err(Error::internal(format!(
                        "image {} is missing layer blob {}; run an image pull or load it again",
                        record.id.full(),
                        layer.digest.full()
                    )));
                }
                blobs.insert(layer.digest.full(), layer.digest.clone());
            }

            let layer_paths = record
                .layers
                .iter()
                .map(|layer| blob_path(&layer.digest))
                .collect();
            legacy.push(LegacyManifestItem {
                config: blob_path(&record.id),
                repo_tags: target
                    .tags
                    .iter()
                    .map(|tag| familiar_reference(tag))
                    .collect(),
                layers: layer_paths,
            });

            let mut descriptors = Vec::new();
            for tag in &target.tags {
                let parsed = ImageRef::parse(tag)?;
                let mut annotations = BTreeMap::new();
                annotations.insert("io.containerd.image.name".to_string(), tag.clone());
                annotations.insert(
                    "org.opencontainers.image.ref.name".to_string(),
                    parsed.effective_tag().to_string(),
                );
                descriptors.push(Descriptor {
                    media_type: MEDIA_TYPE_IMAGE_MANIFEST.to_string(),
                    digest: manifest_digest.clone(),
                    size: generated
                        .get(&manifest_digest.full())
                        .map(|bytes| bytes.len() as i64)
                        .unwrap_or(0),
                    annotations,
                    ..Descriptor::default()
                });
            }
            if descriptors.is_empty() {
                descriptors.push(Descriptor {
                    media_type: MEDIA_TYPE_IMAGE_MANIFEST.to_string(),
                    digest: manifest_digest.clone(),
                    size: generated
                        .get(&manifest_digest.full())
                        .map(|bytes| bytes.len() as i64)
                        .unwrap_or(0),
                    ..Descriptor::default()
                });
            }
            index.manifests.extend(descriptors);
        }

        for (_, digest) in blobs {
            if let Some(bytes) = generated.get(&digest.full()) {
                append_bytes(&mut builder, &blob_path(&digest), bytes)?;
            } else {
                append_blob(&mut builder, self, &digest)?;
            }
        }

        append_bytes(
            &mut builder,
            "manifest.json",
            &serde_json::to_vec(&legacy).map_err(|error| {
                Error::internal(format!("failed to encode manifest.json: {error}"))
            })?,
        )?;
        append_bytes(&mut builder, "oci-layout", OCI_LAYOUT.as_bytes())?;
        append_bytes(
            &mut builder,
            "index.json",
            &serde_json::to_vec(&index).map_err(|error| {
                Error::internal(format!("failed to encode index.json: {error}"))
            })?,
        )?;
        builder
            .finish()
            .map_err(|error| Error::internal(format!("failed to finish image archive: {error}")))?;
        Ok(())
    }

    fn save_targets(
        &self,
        names: &[String],
        platform: Option<&Platform>,
    ) -> Result<Vec<SaveTarget>> {
        let mut targets: BTreeMap<String, SaveTarget> = BTreeMap::new();
        for name in names {
            let name = name.trim().trim_start_matches('/');
            if name.is_empty() {
                return Err(Error::invalid_argument("image name cannot be blank"));
            }
            let parsed = ImageRef::parse(name).ok();
            let is_digest_id = name.starts_with("sha256:");
            let repo_only = parsed
                .as_ref()
                .is_some_and(|reference| reference.tag().is_none() && reference.digest().is_none())
                && !is_digest_id;

            let mut matched = false;
            if repo_only {
                let reference = parsed
                    .as_ref()
                    .expect("repo_only implies a parsed reference");
                let (registry, repository) = canonical_registry_repo(reference);
                let prefix = format!("{registry}/{repository}:");
                for record in self.history() {
                    let tags: BTreeSet<String> = record
                        .tags
                        .iter()
                        .filter(|tag| tag.starts_with(&prefix))
                        .cloned()
                        .collect();
                    if tags.is_empty() {
                        continue;
                    }
                    matched = true;
                    merge_target(&mut targets, record, tags);
                }
            }
            if matched {
                continue;
            }

            let record = self.resolve(name)?;
            let mut tags = BTreeSet::new();
            if let Some(reference) = parsed {
                if reference.digest().is_some() || is_digest_id {
                    // Docker exports a digest/ID selection without tag names.
                } else if reference.tag().is_some() {
                    tags.insert(canonical_tag(&reference)?);
                } else {
                    // A short ID or an unresolved bare repository: preserve
                    // the tags that belong to the record.
                    tags.extend(record.tags.iter().cloned());
                }
            }
            merge_target(&mut targets, record, tags);
        }

        if targets.is_empty() {
            return Err(Error::invalid_argument("image name cannot be blank"));
        }
        if let Some(wanted) = platform {
            for target in targets.values() {
                if !platform_matches(&target.record.platform, wanted) {
                    return Err(Error::invalid_argument(format!(
                        "no suitable export target found for platform {}/{}",
                        wanted.os, wanted.architecture
                    )));
                }
            }
        }
        Ok(targets.into_values().collect())
    }

    fn export_manifest(&self, record: &ImageRecord) -> Result<(Digest, Vec<u8>)> {
        for digest in &record.manifest_digests {
            let Ok(bytes) = self.blob_store().get_verified(digest) else {
                continue;
            };
            let Ok(manifest) = serde_json::from_slice::<Manifest>(&bytes) else {
                continue;
            };
            if manifest.config.digest == record.id
                && manifest.layers.len() == record.layers.len()
                && manifest
                    .layers
                    .iter()
                    .zip(&record.layers)
                    .all(|(left, right)| {
                        left.digest == right.digest && left.media_type == right.media_type
                    })
            {
                return Ok((digest.clone(), bytes));
            }
        }

        let config_size = self.blob_store().stat(&record.id)? as i64;
        let manifest = Manifest::new(
            Descriptor {
                media_type: MEDIA_TYPE_IMAGE_CONFIG.to_string(),
                digest: record.id.clone(),
                size: config_size,
                ..Descriptor::default()
            },
            record.layers.clone(),
        );
        let bytes = serde_json::to_vec(&manifest).map_err(|error| {
            Error::internal(format!("failed to encode image manifest: {error}"))
        })?;
        let digest = Digest::sha256(&bytes);
        Ok((digest, bytes))
    }

    /// Import a `docker save` archive from `reader`.
    ///
    /// The reader must be seekable so gzip-compressed archives can be
    /// decompressed to a bounded temporary file. OCI layout archives and the
    /// legacy `manifest.json` format are both accepted.
    pub fn load_archive<R: Read + Seek>(
        &self,
        reader: &mut R,
        platform: Option<&Platform>,
    ) -> Result<LoadOutcome> {
        let directory = tempfile::tempdir()?;
        let compression = detect_archive_compression(reader)?;
        reader.seek(SeekFrom::Start(0))?;
        match compression {
            ArchiveCompression::Plain => unpack_archive(reader, directory.path())?,
            ArchiveCompression::Gzip => {
                let mut decompressed = tempfile::tempfile()?;
                let mut decoder = GzDecoder::new(reader);
                copy_limited(&mut decoder, &mut decompressed, MAX_EXTRACTED_BYTES)?;
                decompressed.seek(SeekFrom::Start(0))?;
                unpack_archive(&mut decompressed, directory.path())?;
            }
            ArchiveCompression::Zstd => {
                return Err(Error::unsupported(
                    "zstd-compressed image archives are not implemented; use an uncompressed or gzip tar",
                ))
            }
        }

        let index_bytes = read_optional(directory.path(), "index.json")?;
        let legacy_bytes = read_optional(directory.path(), "manifest.json")?;
        if index_bytes.is_none() && legacy_bytes.is_none() {
            return Err(Error::invalid_argument(
                "archive contains neither index.json nor manifest.json",
            ));
        }

        let mut images = if let Some(bytes) = &index_bytes {
            self.load_oci_layout(directory.path(), bytes, platform)?
        } else {
            Vec::new()
        };
        if let Some(bytes) = &legacy_bytes {
            let items: Vec<LegacyManifestItem> =
                serde_json::from_slice(bytes).map_err(|error| {
                    Error::invalid_argument(format!("invalid manifest.json: {error}"))
                })?;
            if images.is_empty() {
                images = self.load_legacy(directory.path(), &items, platform)?;
            } else {
                self.attach_legacy_tags(directory.path(), &items, &mut images)?;
            }
        }
        if images.is_empty() {
            return Err(Error::invalid_argument(
                "archive contains no supported image manifests",
            ));
        }

        let mut outcome = LoadOutcome::default();
        for image in images {
            let id = Digest::sha256(&image.config_bytes);
            self.blob_store().put_verified(&id, &image.config_bytes)?;
            self.blob_store()
                .put_verified(&image.manifest_digest, &image.manifest_bytes)?;
            for (layer, path) in image.layers.iter().zip(&image.layer_paths) {
                let expected_size = u64::try_from(layer.size).map_err(|_| {
                    Error::invalid_argument(format!(
                        "layer {} has a negative size",
                        layer.digest.full()
                    ))
                })?;
                let mut file = File::open(path)?;
                self.blob_store().put_stream(
                    Some(&layer.digest),
                    Some(expected_size),
                    &mut file,
                )?;
            }

            let manifest_digest = image.manifest_digest;
            let config_size = image.config_bytes.len() as i64;
            let size = config_size
                + image
                    .layers
                    .iter()
                    .map(|layer| layer.size.max(0))
                    .sum::<i64>();
            let record = ImageRecord {
                id,
                manifest_digest: manifest_digest.clone(),
                manifest_digests: vec![manifest_digest],
                tags: image.tags.clone(),
                repo_digests: BTreeSet::new(),
                created: image
                    .config
                    .created
                    .as_ref()
                    .map(|created| created.timestamp())
                    .unwrap_or_else(unix_now),
                pulled_at: unix_now(),
                size,
                config_size,
                platform: image.platform.clone(),
                config: image.config,
                layers: image.layers,
            };
            self.insert(record.clone())?;
            outcome.images.push(LoadedImage {
                id: record.id.clone(),
                tags: record.display_tags(),
            });
        }
        Ok(outcome)
    }

    fn load_oci_layout(
        &self,
        directory: &Path,
        index_bytes: &[u8],
        platform: Option<&Platform>,
    ) -> Result<Vec<LoadedImageData>> {
        let index: Index = serde_json::from_slice(index_bytes)
            .map_err(|error| Error::invalid_argument(format!("invalid index.json: {error}")))?;
        index.validate()?;
        let mut descriptors: BTreeMap<String, (Digest, Descriptor, BTreeSet<String>)> =
            BTreeMap::new();
        for descriptor in &index.manifests {
            if descriptor.media_type != MEDIA_TYPE_IMAGE_MANIFEST {
                continue;
            }
            let tags = tags_from_annotations(&descriptor.annotations);
            descriptors
                .entry(descriptor.digest.full())
                .and_modify(|(_, _, existing)| existing.extend(tags.clone()))
                .or_insert_with(|| (descriptor.digest.clone(), descriptor.clone(), tags));
        }

        let mut images: Vec<LoadedImageData> = Vec::new();
        for (_, (digest, descriptor, tags)) in descriptors {
            let bytes = read_blob_file(directory, &digest)?;
            if bytes.len() as i64 != descriptor.size && descriptor.size != 0 {
                return Err(Error::invalid_argument(format!(
                    "manifest {} size mismatch: archive has {}, descriptor says {}",
                    digest.full(),
                    bytes.len(),
                    descriptor.size
                )));
            }
            let manifest: Manifest = serde_json::from_slice(&bytes).map_err(|error| {
                Error::invalid_argument(format!("invalid manifest {}: {error}", digest.full()))
            })?;
            manifest.validate()?;
            let config_bytes = read_blob_file(directory, &manifest.config.digest)?;
            if config_bytes.len() as i64 != manifest.config.size && manifest.config.size != 0 {
                return Err(Error::invalid_argument(format!(
                    "config {} size mismatch",
                    manifest.config.digest.full()
                )));
            }
            let config: Image = serde_json::from_slice(&config_bytes).map_err(|error| {
                Error::invalid_argument(format!(
                    "invalid image config {}: {error}",
                    manifest.config.digest.full()
                ))
            })?;
            let config_platform = image_platform(&config, &config_bytes)?;
            if let Some(wanted) = platform {
                if !platform_matches(&config_platform, wanted) {
                    continue;
                }
            }
            if let Some(descriptor_platform) = &descriptor.platform {
                if !platform_matches(&config_platform, descriptor_platform) {
                    continue;
                }
            }

            let mut diff_ids = Vec::with_capacity(manifest.layers.len());
            let mut layer_paths = Vec::with_capacity(manifest.layers.len());
            for layer in &manifest.layers {
                let path = blob_file(directory, &layer.digest);
                check_layer_size(&path, layer)?;
                diff_ids.push(layer_diff_id(&path, &layer.media_type)?);
                layer_paths.push(path);
            }
            if config.rootfs.diff_ids.len() != diff_ids.len() {
                return Err(Error::invalid_argument(format!(
                    "image {} has {} rootfs diffIDs but manifest has {} layers",
                    manifest.config.digest.full(),
                    config.rootfs.diff_ids.len(),
                    diff_ids.len()
                )));
            }
            if config.rootfs.diff_ids != diff_ids {
                return Err(Error::invalid_argument(format!(
                    "image {} layer diffIDs do not match the config",
                    manifest.config.digest.full()
                )));
            }

            // A tag can only point at one image in this store. Reject
            // ambiguous multi-platform archives rather than moving tags
            // unpredictably; callers can pass a platform to disambiguate.
            if let Some(existing) = images.iter().find(|image| !image.tags.is_disjoint(&tags)) {
                return Err(Error::unsupported(format!(
                    "archive contains multiple images for the same tag ({} and {}); select a platform",
                    existing.manifest_digest.full(),
                    digest.full()
                )));
            }
            images.push(LoadedImageData {
                config_bytes,
                config,
                platform: config_platform,
                layers: manifest.layers.clone(),
                layer_paths,
                manifest_bytes: bytes,
                manifest_digest: digest,
                tags,
            });
        }
        Ok(images)
    }

    fn load_legacy(
        &self,
        directory: &Path,
        items: &[LegacyManifestItem],
        platform: Option<&Platform>,
    ) -> Result<Vec<LoadedImageData>> {
        let mut images = Vec::new();
        for item in items {
            let config_path = safe_join(directory, &item.config)?;
            let raw_config = fs::read(&config_path)?;
            let (mut config_bytes, mut config) = parse_image_config(&raw_config)?;
            let config_platform = image_platform(&config, &config_bytes)?;
            if let Some(wanted) = platform {
                if !platform_matches(&config_platform, wanted) {
                    continue;
                }
            }
            let mut layers = Vec::with_capacity(item.layers.len());
            let mut layer_paths = Vec::with_capacity(item.layers.len());
            let mut diff_ids = Vec::with_capacity(item.layers.len());
            for layer_path in &item.layers {
                let path = safe_join(directory, layer_path)?;
                let descriptor = legacy_layer_descriptor(&path)?;
                diff_ids.push(layer_diff_id(&path, &descriptor.media_type)?);
                layers.push(descriptor);
                layer_paths.push(path);
            }

            if config.rootfs.diff_ids.is_empty() && !diff_ids.is_empty() {
                config.rootfs = RootFS::layers(diff_ids.clone());
                config_bytes = serde_json::to_vec(&config).map_err(|error| {
                    Error::internal(format!("failed to encode loaded image config: {error}"))
                })?;
            } else {
                if config.rootfs.diff_ids.len() != diff_ids.len() {
                    return Err(Error::invalid_argument(format!(
                        "manifest.json config has {} diffIDs but {} layers",
                        config.rootfs.diff_ids.len(),
                        diff_ids.len()
                    )));
                }
                if config.rootfs.diff_ids != diff_ids {
                    return Err(Error::invalid_argument(
                        "manifest.json layer diffIDs do not match the image config",
                    ));
                }
            }

            let config_digest = Digest::sha256(&config_bytes);
            let manifest = Manifest::new(
                Descriptor {
                    media_type: MEDIA_TYPE_IMAGE_CONFIG.to_string(),
                    digest: config_digest,
                    size: config_bytes.len() as i64,
                    ..Descriptor::default()
                },
                layers.clone(),
            );
            let manifest_bytes = serde_json::to_vec(&manifest).map_err(|error| {
                Error::internal(format!("failed to encode loaded image manifest: {error}"))
            })?;
            images.push(LoadedImageData {
                config_bytes,
                config,
                platform: config_platform,
                layers,
                layer_paths,
                manifest_digest: Digest::sha256(&manifest_bytes),
                manifest_bytes,
                tags: canonical_tags(&item.repo_tags)?,
            });
        }
        Ok(images)
    }

    fn attach_legacy_tags(
        &self,
        directory: &Path,
        items: &[LegacyManifestItem],
        images: &mut [LoadedImageData],
    ) -> Result<()> {
        for item in items {
            let config_path = match safe_join(directory, &item.config) {
                Ok(path) => path,
                Err(_) => continue,
            };
            let Ok(raw_config) = fs::read(config_path) else {
                continue;
            };
            let config_digest = Digest::sha256(&raw_config);
            let tags = canonical_tags(&item.repo_tags)?;
            for image in images.iter_mut() {
                if Digest::sha256(&image.config_bytes) == config_digest {
                    image.tags.extend(tags.clone());
                }
            }
        }
        Ok(())
    }
}

fn merge_target(
    targets: &mut BTreeMap<String, SaveTarget>,
    record: ImageRecord,
    tags: BTreeSet<String>,
) {
    targets
        .entry(record.id.full())
        .and_modify(|target| target.tags.extend(tags.clone()))
        .or_insert(SaveTarget { record, tags });
}

fn tags_from_annotations(annotations: &BTreeMap<String, String>) -> BTreeSet<String> {
    let mut tags = BTreeSet::new();
    if let Some(name) = annotations
        .get("io.containerd.image.name")
        .or_else(|| annotations.get("org.opencontainers.image.ref.name"))
    {
        if let Ok(reference) = ImageRef::parse(name) {
            if reference.tag().is_some() && reference.digest().is_none() {
                if let Ok(tag) = canonical_tag(&reference) {
                    tags.insert(tag);
                }
            }
        }
    }
    tags
}

fn canonical_tags(repo_tags: &[String]) -> Result<BTreeSet<String>> {
    let mut tags = BTreeSet::new();
    for tag in repo_tags {
        let reference = ImageRef::parse(tag)?;
        tags.insert(canonical_tag(&reference)?);
    }
    Ok(tags)
}

fn image_platform(config: &Image, config_bytes: &[u8]) -> Result<Platform> {
    if config.os.is_empty() || config.architecture.is_empty() {
        return Err(Error::invalid_argument(
            "image config is missing os/architecture",
        ));
    }
    let value: serde_json::Value = serde_json::from_slice(config_bytes)
        .map_err(|error| Error::invalid_argument(format!("invalid image config JSON: {error}")))?;
    Ok(Platform {
        architecture: config.architecture.clone(),
        os: config.os.clone(),
        os_version: value
            .get("os.version")
            .and_then(serde_json::Value::as_str)
            .map(str::to_string),
        os_features: value
            .get("os.features")
            .and_then(serde_json::Value::as_array)
            .map(|features| {
                features
                    .iter()
                    .filter_map(serde_json::Value::as_str)
                    .map(str::to_string)
                    .collect()
            }),
        variant: value
            .get("variant")
            .and_then(serde_json::Value::as_str)
            .map(str::to_string),
    })
}

fn parse_image_config(raw: &[u8]) -> Result<(Vec<u8>, Image)> {
    if let Ok(image) = serde_json::from_slice::<Image>(raw) {
        if !image.os.is_empty() && !image.architecture.is_empty() {
            return Ok((raw.to_vec(), image));
        }
    }

    // Old Docker save archives store the legacy V1 image, whose execution
    // config is nested under `config` and whose rootfs is absent. Reconstruct
    // an OCI config from the fields we understand and compute the rootfs from
    // the layer files in the caller.
    #[derive(Deserialize)]
    struct LegacyConfig {
        #[serde(default)]
        config: docker_image::ImageConfig,
        #[serde(default)]
        architecture: String,
        #[serde(default)]
        os: String,
        #[serde(default)]
        created: Option<chrono::DateTime<chrono::Utc>>,
        #[serde(default)]
        author: Option<String>,
        #[serde(default)]
        history: Vec<docker_image::History>,
    }
    let legacy: LegacyConfig = serde_json::from_slice(raw).map_err(|error| {
        Error::invalid_argument(format!(
            "config is neither an OCI image nor a V1 image: {error}"
        ))
    })?;
    let image = Image {
        created: legacy.created,
        author: legacy.author,
        architecture: legacy.architecture,
        os: legacy.os,
        config: legacy.config,
        rootfs: RootFS::layers(Vec::new()),
        history: legacy.history,
    };
    let bytes = serde_json::to_vec(&image)
        .map_err(|error| Error::internal(format!("failed to encode image config: {error}")))?;
    Ok((bytes, image))
}

fn detect_archive_compression<R: Read + Seek>(reader: &mut R) -> Result<ArchiveCompression> {
    reader.seek(SeekFrom::Start(0))?;
    let mut magic = [0u8; 4];
    let read = reader.read(&mut magic)?;
    reader.seek(SeekFrom::Start(0))?;
    if read >= 2 && magic[0] == 0x1f && magic[1] == 0x8b {
        return Ok(ArchiveCompression::Gzip);
    }
    if read == 4 && magic == [0x28, 0xb5, 0x2f, 0xfd] {
        return Ok(ArchiveCompression::Zstd);
    }
    Ok(ArchiveCompression::Plain)
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum ArchiveCompression {
    Plain,
    Gzip,
    Zstd,
}

fn unpack_archive<R: Read>(reader: R, directory: &Path) -> Result<()> {
    let mut archive = Archive::new(reader);
    let mut entries = 0usize;
    let mut extracted = 0u64;
    for entry in archive.entries()? {
        entries += 1;
        if entries > MAX_ARCHIVE_ENTRIES {
            return Err(Error::invalid_argument(
                "image archive has too many entries",
            ));
        }
        let mut entry = entry?;
        let path = entry.path()?.into_owned();
        let kind = entry.header().entry_type();
        let Some(relative) = sanitize_relative(&path) else {
            // GNU tar emits a `./` directory entry when archiving `.`. It
            // names the extraction root itself and carries no content, so it
            // is safe to skip. Anything else that sanitizes to nothing
            // (absolute paths, `..`, an empty name) stays an error.
            if kind.is_dir()
                && !path.as_os_str().is_empty()
                && path
                    .components()
                    .all(|component| matches!(component, Component::CurDir))
            {
                continue;
            }
            return Err(Error::invalid_argument(format!(
                "image archive contains an unsafe path: {}",
                path.display()
            )));
        };
        let destination = directory.join(&relative);
        if kind.is_dir() {
            fs::create_dir_all(&destination)?;
            continue;
        }
        if !kind.is_file() {
            return Err(Error::invalid_argument(format!(
                "image archive contains an unsupported entry type at {}",
                path.display()
            )));
        }
        if let Some(parent) = destination.parent() {
            fs::create_dir_all(parent)?;
        }
        let size = entry.size();
        extracted = extracted
            .checked_add(size)
            .ok_or_else(|| Error::invalid_argument("image archive is too large"))?;
        if extracted > MAX_EXTRACTED_BYTES {
            return Err(Error::invalid_argument("image archive is too large"));
        }
        let mut output = OpenOptions::new()
            .create_new(true)
            .write(true)
            .open(&destination)?;
        io::copy(&mut entry, &mut output)?;
    }
    Ok(())
}

fn sanitize_relative(path: &Path) -> Option<PathBuf> {
    let mut output = PathBuf::new();
    for component in path.components() {
        match component {
            Component::Normal(component) => output.push(component),
            Component::CurDir => {}
            Component::ParentDir | Component::RootDir | Component::Prefix(_) => return None,
        }
    }
    if output.as_os_str().is_empty() {
        None
    } else {
        Some(output)
    }
}

fn safe_join(directory: &Path, relative: &str) -> Result<PathBuf> {
    let path = sanitize_relative(Path::new(relative))
        .ok_or_else(|| Error::invalid_argument(format!("unsafe archive path: {relative}")))?;
    Ok(directory.join(path))
}

fn read_optional(directory: &Path, name: &str) -> Result<Option<Vec<u8>>> {
    let path = directory.join(name);
    match fs::metadata(&path) {
        Ok(metadata) => {
            if metadata.len() > MAX_METADATA_BYTES {
                return Err(Error::invalid_argument(format!("{name} is too large")));
            }
            Ok(Some(fs::read(path)?))
        }
        Err(error) if error.kind() == io::ErrorKind::NotFound => Ok(None),
        Err(error) => Err(error.into()),
    }
}

fn blob_path(digest: &Digest) -> String {
    format!("blobs/{}/{}", digest.algor().as_str(), digest.hex())
}

fn blob_file(directory: &Path, digest: &Digest) -> PathBuf {
    directory
        .join("blobs")
        .join(digest.algor().as_str())
        .join(digest.hex())
}

fn read_blob_file(directory: &Path, digest: &Digest) -> Result<Vec<u8>> {
    let path = blob_file(directory, digest);
    let bytes = fs::read(&path).map_err(|error| {
        Error::invalid_argument(format!(
            "archive is missing blob {}: {error}",
            digest.full()
        ))
    })?;
    if Digest::sha256(&bytes) != *digest {
        return Err(Error::invalid_argument(format!(
            "archive blob {} does not match its digest",
            digest.full()
        )));
    }
    Ok(bytes)
}

fn check_layer_size(path: &Path, descriptor: &Descriptor) -> Result<()> {
    let size = fs::metadata(path)?.len();
    if descriptor.size != 0 && size != descriptor.size as u64 {
        return Err(Error::invalid_argument(format!(
            "layer {} size mismatch: archive has {size}, manifest says {}",
            descriptor.digest.full(),
            descriptor.size
        )));
    }
    Ok(())
}

fn layer_diff_id(path: &Path, media_type: &str) -> Result<Digest> {
    let compression = compression_for(media_type);
    if compression == LayerCompression::Zstd {
        return Err(Error::unsupported(format!(
            "zstd layer {} is not implemented",
            path.display()
        )));
    }
    let file = File::open(path)?;
    let mut reader: Box<dyn Read> = match compression {
        LayerCompression::Gzip => Box::new(GzDecoder::new(BufReader::new(file))),
        LayerCompression::Plain => Box::new(BufReader::new(file)),
        LayerCompression::Zstd => unreachable!(),
    };
    let mut hasher = sha2::Sha256::new();
    let mut buffer = [0u8; 64 * 1024];
    loop {
        let read = reader.read(&mut buffer)?;
        if read == 0 {
            break;
        }
        hasher.update(&buffer[..read]);
    }
    Digest::parse(&format!("sha256:{}", hex_bytes(&hasher.finalize())))
}

fn legacy_layer_descriptor(path: &Path) -> Result<Descriptor> {
    let compression = detect_layer_compression(path)?;
    let media_type = match compression {
        LayerCompression::Gzip => "application/vnd.docker.image.rootfs.diff.tar.gzip",
        LayerCompression::Plain => MEDIA_TYPE_IMAGE_LAYER,
        LayerCompression::Zstd => {
            return Err(Error::unsupported(
                "zstd-compressed legacy layers are not implemented",
            ))
        }
    };
    let size = fs::metadata(path)?.len();
    let file = File::open(path)?;
    let mut reader = BufReader::new(file);
    let mut hasher = sha2::Sha256::new();
    let mut buffer = [0u8; 64 * 1024];
    loop {
        let read = reader.read(&mut buffer)?;
        if read == 0 {
            break;
        }
        hasher.update(&buffer[..read]);
    }
    let digest = Digest::parse(&format!("sha256:{}", hex_bytes(&hasher.finalize())))?;
    Ok(Descriptor {
        media_type: media_type.to_string(),
        digest,
        size: i64::try_from(size)
            .map_err(|_| Error::invalid_argument("legacy layer is too large"))?,
        ..Descriptor::default()
    })
}

fn detect_layer_compression(path: &Path) -> Result<LayerCompression> {
    let mut file = File::open(path)?;
    let mut magic = [0u8; 4];
    let read = file.read(&mut magic)?;
    if read >= 2 && magic[0] == 0x1f && magic[1] == 0x8b {
        return Ok(LayerCompression::Gzip);
    }
    if read == 4 && magic == [0x28, 0xb5, 0x2f, 0xfd] {
        return Ok(LayerCompression::Zstd);
    }
    Ok(LayerCompression::Plain)
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum LayerCompression {
    Plain,
    Gzip,
    Zstd,
}

fn compression_for(media_type: &str) -> LayerCompression {
    if media_type.ends_with("+gzip") || media_type.ends_with(".tar.gzip") {
        LayerCompression::Gzip
    } else if media_type.ends_with("+zstd") || media_type.ends_with(".tar.zstd") {
        LayerCompression::Zstd
    } else {
        LayerCompression::Plain
    }
}

fn append_bytes<W: Write>(builder: &mut Builder<W>, path: &str, bytes: &[u8]) -> Result<()> {
    let mut header = Header::new_gnu();
    header.set_size(bytes.len() as u64);
    header.set_mode(0o644);
    header.set_mtime(0);
    header.set_uid(0);
    header.set_gid(0);
    header.set_cksum();
    builder.append_data(&mut header, path, bytes)?;
    Ok(())
}

fn append_blob<W: Write>(
    builder: &mut Builder<W>,
    store: &ImageStore,
    digest: &Digest,
) -> Result<()> {
    let path = store.blob_store().blob_path(digest);
    let file = File::open(&path)?;
    let size = file.metadata()?.len();
    let mut header = Header::new_gnu();
    header.set_size(size);
    header.set_mode(0o644);
    header.set_mtime(0);
    header.set_uid(0);
    header.set_gid(0);
    header.set_cksum();
    builder.append_data(&mut header, blob_path(digest), BufReader::new(file))?;
    Ok(())
}

fn copy_limited<R: Read, W: Write>(reader: &mut R, writer: &mut W, limit: u64) -> Result<u64> {
    let mut total = 0u64;
    let mut buffer = [0u8; 64 * 1024];
    loop {
        let read = reader.read(&mut buffer)?;
        if read == 0 {
            return Ok(total);
        }
        total = total
            .checked_add(read as u64)
            .ok_or_else(|| Error::invalid_argument("image archive is too large"))?;
        if total > limit {
            return Err(Error::invalid_argument("image archive is too large"));
        }
        writer.write_all(&buffer[..read])?;
    }
}

fn hex_bytes(bytes: &[u8]) -> String {
    const HEX: &[u8; 16] = b"0123456789abcdef";
    let mut output = String::with_capacity(bytes.len() * 2);
    for byte in bytes {
        output.push(HEX[(byte >> 4) as usize] as char);
        output.push(HEX[(byte & 0x0f) as usize] as char);
    }
    output
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn safe_paths_reject_escapes() {
        assert!(sanitize_relative(Path::new("blobs/sha256/abc")).is_some());
        assert!(sanitize_relative(Path::new("../escape")).is_none());
        assert!(sanitize_relative(Path::new("/absolute")).is_none());
        assert!(sanitize_relative(Path::new("")).is_none());
    }

    #[test]
    fn layer_compression_suffixes_match_spec() {
        assert_eq!(
            compression_for(docker_image::MEDIA_TYPE_IMAGE_LAYER_GZIP),
            LayerCompression::Gzip
        );
        assert_eq!(
            compression_for(MEDIA_TYPE_IMAGE_LAYER),
            LayerCompression::Plain
        );
        assert_eq!(
            compression_for("application/vnd.docker.image.rootfs.diff.tar.gzip"),
            LayerCompression::Gzip
        );
    }

    fn tar_archive(entries: &[(&str, &[u8])]) -> Vec<u8> {
        let mut builder = Builder::new(Vec::new());
        for (path, bytes) in entries {
            let mut header = Header::new_gnu();
            header.set_size(bytes.len() as u64);
            header.set_mode(0o644);
            header.set_cksum();
            builder.append_data(&mut header, path, *bytes).unwrap();
        }
        builder.into_inner().unwrap()
    }

    fn tar_archive_with_root_entry(entries: &[(&str, &[u8])]) -> Vec<u8> {
        let mut builder = Builder::new(Vec::new());
        // GNU tar emits a `./` directory entry for `tar -C dir -cf x.tar .`.
        let mut root = Header::new_gnu();
        root.set_entry_type(tar::EntryType::Directory);
        root.set_size(0);
        root.set_mode(0o755);
        root.set_cksum();
        builder.append_data(&mut root, "./", io::empty()).unwrap();
        for (path, bytes) in entries {
            let mut header = Header::new_gnu();
            header.set_size(bytes.len() as u64);
            header.set_mode(0o644);
            header.set_cksum();
            builder.append_data(&mut header, path, *bytes).unwrap();
        }
        builder.into_inner().unwrap()
    }

    #[test]
    fn loads_a_legacy_manifest_archive_and_exports_all_repository_tags() {
        let directory = tempfile::tempdir().unwrap();
        let store = ImageStore::open(directory.path().join("images")).unwrap();

        let layer = tar_archive(&[("hello.txt", b"hello\n")]);
        let diff_id = Digest::sha256(&layer);
        let config = serde_json::to_vec(&serde_json::json!({
            "architecture": "amd64",
            "os": "linux",
            "config": {"Cmd": ["/legacy"], "Env": ["PATH=/bin"]},
            "history": []
        }))
        .unwrap();
        let manifest = serde_json::to_vec(&serde_json::json!([{
            "Config": "config.json",
            "RepoTags": ["legacy:1"],
            "Layers": ["layer.tar"]
        }]))
        .unwrap();
        let archive = tar_archive(&[
            ("manifest.json", &manifest),
            ("config.json", &config),
            ("layer.tar", &layer),
        ]);

        let outcome = store
            .load_archive(&mut io::Cursor::new(archive.clone()), None)
            .unwrap();
        assert_eq!(outcome.images.len(), 1);
        assert_eq!(outcome.images[0].tags, vec!["legacy:1".to_string()]);

        let record = store.resolve("legacy:1").unwrap();
        assert_eq!(record.config.config.cmd, vec!["/legacy".to_string()]);
        assert_eq!(record.config.rootfs.diff_ids, vec![diff_id]);
        assert_eq!(record.layers.len(), 1);
        assert_eq!(
            store.blob_store().get_verified(&record.id).unwrap(),
            serde_json::to_vec(&record.config).unwrap()
        );
        assert_eq!(
            store
                .blob_store()
                .get_verified(&record.layers[0].digest)
                .unwrap(),
            layer
        );
        assert!(store.blob_store().exists(&record.manifest_digest));

        let mut exported = Vec::new();
        store
            .export_archive(&["legacy".to_string()], None, &mut exported)
            .unwrap();
        let second_directory = tempfile::tempdir().unwrap();
        let second = ImageStore::open(second_directory.path().join("images")).unwrap();
        let outcome = second
            .load_archive(&mut io::Cursor::new(exported), None)
            .unwrap();
        assert_eq!(outcome.images[0].tags, vec!["legacy:1".to_string()]);

        let mut compressed =
            flate2::write::GzEncoder::new(Vec::new(), flate2::Compression::default());
        compressed.write_all(&archive).unwrap();
        let compressed = compressed.finish().unwrap();
        let third_directory = tempfile::tempdir().unwrap();
        let third = ImageStore::open(third_directory.path().join("images")).unwrap();
        let outcome = third
            .load_archive(&mut io::Cursor::new(compressed), None)
            .unwrap();
        assert_eq!(outcome.images[0].tags, vec!["legacy:1".to_string()]);
    }

    fn raw_tar_entry(name: &str, data: &[u8]) -> Vec<u8> {
        let mut header = [0u8; 512];
        header[..name.len()].copy_from_slice(name.as_bytes());
        header[100..108].copy_from_slice(b"0000644\0");
        header[108..116].copy_from_slice(b"0000000\0");
        header[116..124].copy_from_slice(b"0000000\0");
        header[124..136].copy_from_slice(format!("{:011o}\0", data.len()).as_bytes());
        header[136..148].copy_from_slice(b"00000000000\0");
        header[148..156].copy_from_slice(b"        ");
        header[156] = b'0';
        header[257..263].copy_from_slice(b"ustar\0");
        header[263..265].copy_from_slice(b"00");
        let checksum: u32 = header.iter().map(|byte| u32::from(*byte)).sum();
        header[148..156].copy_from_slice(format!("{checksum:06o}\0 ").as_bytes());
        let mut archive = Vec::new();
        archive.extend_from_slice(&header);
        archive.extend_from_slice(data);
        archive.resize(512 + data.len().div_ceil(512) * 512, 0);
        archive.extend_from_slice(&[0u8; 1024]);
        archive
    }

    #[test]
    fn loads_an_archive_with_a_gnu_tar_root_entry() {
        let directory = tempfile::tempdir().unwrap();
        let store = ImageStore::open(directory.path().join("images")).unwrap();

        let layer = tar_archive(&[("hello.txt", b"hello\n")]);
        let config = serde_json::to_vec(&serde_json::json!({
            "architecture": "amd64",
            "os": "linux",
            "config": {"Cmd": ["/hello"]},
            "history": []
        }))
        .unwrap();
        let manifest = serde_json::to_vec(&serde_json::json!([{
            "Config": "config.json",
            "RepoTags": ["root-entry:1"],
            "Layers": ["layer.tar"]
        }]))
        .unwrap();
        let archive = tar_archive_with_root_entry(&[
            ("manifest.json", &manifest),
            ("config.json", &config),
            ("layer.tar", &layer),
        ]);

        let outcome = store
            .load_archive(&mut io::Cursor::new(archive), None)
            .unwrap();
        assert_eq!(outcome.images[0].tags, vec!["root-entry:1".to_string()]);
    }

    #[test]
    fn load_rejects_archive_path_traversal() {
        let directory = tempfile::tempdir().unwrap();
        let store = ImageStore::open(directory.path().join("images")).unwrap();
        let archive = raw_tar_entry("../escape.txt", b"owned");
        let error = store
            .load_archive(&mut io::Cursor::new(archive), None)
            .unwrap_err();
        assert!(
            error.message().contains("unsafe path"),
            "unexpected error: {}",
            error.message()
        );
    }
}
