//! Registry pull into the local image store.
//!
//! The distribution client handles manifests, authentication challenges and
//! verified blob downloads. This module resolves a multi-platform index,
//! downloads the selected manifest, config and layers, and records the image
//! under its config digest.

use std::net::IpAddr;
use std::str::FromStr;

use base64::Engine as _;
use docker_common::{Digest, ImageRef};
use docker_distribution::client::parse_manifest_document;
use docker_distribution::{ManifestDocument, RegistryClient, StdTransport, Url};
use docker_errors::{Error, Result};
use docker_image::{Image, Manifest, Platform, MEDIA_TYPE_IMAGE_CONFIG};
use serde_json::Value;

use crate::reference::{
    canonical_registry_repo, canonical_repo_digest, canonical_tag, familiar_reference,
    host_platform, parse_platform, platform_matches,
};
use crate::store::{unix_now, ImageRecord, ImageStore};

const DOCKER_CONFIG_MEDIA_TYPE: &str = "application/vnd.docker.container.image.v1+json";

/// Credentials extracted from Docker's `X-Registry-Auth` header.
#[derive(Clone)]
pub struct RegistryAuth {
    username: String,
    password: String,
}

impl std::fmt::Debug for RegistryAuth {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("RegistryAuth")
            .field("username", &self.username)
            .field("password", &"<redacted>")
            .finish()
    }
}

/// Decode the base64 JSON auth blob the Docker client sends on pull.
///
/// Anonymous auth (`e30=`, `{}`) becomes `None`. Username/password and the
/// legacy `auth` field are supported. Identity/registry tokens are rejected
/// explicitly rather than silently dropped.
pub fn parse_registry_auth(header: &str) -> Result<Option<RegistryAuth>> {
    let header = header.trim();
    if header.is_empty() {
        return Ok(None);
    }
    let decoded = base64::engine::general_purpose::STANDARD
        .decode(header)
        .or_else(|_| base64::engine::general_purpose::STANDARD_NO_PAD.decode(header))
        .map_err(|error| {
            Error::invalid_argument(format!("invalid X-Registry-Auth base64: {error}"))
        })?;
    let value: Value = serde_json::from_slice(&decoded).map_err(|error| {
        Error::invalid_argument(format!("invalid X-Registry-Auth JSON: {error}"))
    })?;
    let object = value
        .as_object()
        .ok_or_else(|| Error::invalid_argument("X-Registry-Auth must be a JSON object"))?;

    for key in ["identitytoken", "registrytoken"] {
        if object
            .get(key)
            .and_then(Value::as_str)
            .is_some_and(|token| !token.is_empty())
        {
            return Err(Error::unsupported(format!(
                "{key} credentials are not implemented; use username/password or anonymous pulls"
            )));
        }
    }

    let mut username = object
        .get("username")
        .and_then(Value::as_str)
        .unwrap_or_default()
        .to_string();
    let mut password = object
        .get("password")
        .and_then(Value::as_str)
        .unwrap_or_default()
        .to_string();
    if let Some(auth) = object.get("auth").and_then(Value::as_str) {
        if !auth.is_empty() {
            let decoded = base64::engine::general_purpose::STANDARD
                .decode(auth)
                .map_err(|error| {
                    Error::invalid_argument(format!("invalid legacy auth field base64: {error}"))
                })?;
            let pair = String::from_utf8(decoded)
                .map_err(|_| Error::invalid_argument("legacy auth field is not valid UTF-8"))?;
            let (user, pass) = pair.split_once(':').ok_or_else(|| {
                Error::invalid_argument("legacy auth field must contain `username:password`")
            })?;
            if username.is_empty() {
                username = user.to_string();
            }
            if password.is_empty() {
                password = pass.to_string();
            }
        }
    }
    if username.is_empty() && password.is_empty() {
        return Ok(None);
    }
    if username.is_empty() {
        return Err(Error::invalid_argument(
            "registry auth password was provided without a username",
        ));
    }
    Ok(Some(RegistryAuth { username, password }))
}

/// Result of a pull, including whether the exact manifest was already known.
#[derive(Debug, Clone)]
pub struct PullResult {
    pub image: ImageRecord,
    pub reference: String,
    pub up_to_date: bool,
}

impl ImageStore {
    /// Pull one image reference for `platform` (`os/arch[/variant]`).
    ///
    /// Loopback registries are allowed over plain HTTP for local development;
    /// all other registries require TLS. Tag+digest references are rejected
    /// until the Engine API can verify both.
    pub fn pull(
        &self,
        reference: &str,
        platform: Option<&str>,
        auth: Option<&RegistryAuth>,
    ) -> Result<PullResult> {
        let image_ref = ImageRef::parse(reference)?;
        if image_ref.tag().is_some() && image_ref.digest().is_some() {
            return Err(Error::unsupported(
                "pull by tag@digest is not implemented; use a tag or a digest",
            ));
        }
        let requested_platform = platform.map(parse_platform).transpose()?;
        let tag = if image_ref.digest().is_none() {
            Some(canonical_tag(&image_ref)?)
        } else {
            None
        };
        let previous = self
            .resolve(tag.as_deref().unwrap_or(reference))
            .ok()
            .map(|record| record.manifest_digest);

        let (registry, repository) = canonical_registry_repo(&image_ref);
        let api_host = if registry == "docker.io" {
            "registry-1.docker.io".to_string()
        } else {
            registry
        };
        let scheme = if is_loopback_registry(&api_host) {
            "http"
        } else {
            "https"
        };
        let base = Url::parse(&format!("{scheme}://{api_host}"))?;
        let mut client = RegistryClient::new(StdTransport::default(), base, repository)?;
        if let Some(auth) = auth {
            client = client.with_basic_auth(auth.username.clone(), auth.password.clone());
        }

        let fetch_reference = image_ref
            .digest()
            .map(str::to_string)
            .unwrap_or_else(|| image_ref.effective_tag().to_string());
        let top = client.get_manifest(&fetch_reference)?;
        let document = parse_manifest_document(&top.media_type, &top.bytes)?;
        let (manifest, manifest_blobs, selected_platform) = match document {
            ManifestDocument::Manifest(manifest) => {
                validate_manifest(&manifest)?;
                (
                    manifest,
                    vec![(top.digest.clone(), top.bytes.clone())],
                    None,
                )
            }
            ManifestDocument::Index(index) => {
                index.validate()?;
                let wanted = requested_platform.clone().unwrap_or_else(host_platform);
                let descriptor = select_platform(&index.manifests, &wanted)?;
                let nested = client.get_manifest(&descriptor.digest.full())?;
                let nested_document = parse_manifest_document(&nested.media_type, &nested.bytes)?;
                let ManifestDocument::Manifest(manifest) = nested_document else {
                    return Err(Error::unsupported("nested image indexes are not supported"));
                };
                validate_manifest(&manifest)?;
                (
                    manifest,
                    vec![
                        (top.digest.clone(), top.bytes.clone()),
                        (nested.digest, nested.bytes),
                    ],
                    descriptor.platform.clone(),
                )
            }
        };
        let manifest_digests: Vec<Digest> = manifest_blobs
            .iter()
            .map(|(digest, _)| digest.clone())
            .collect();

        let expected_config_size = manifest.config.size;
        let config_size = client.pull_blob_to_store(&manifest.config.digest, self.blob_store())?;
        if expected_config_size >= 0 && config_size != expected_config_size as u64 {
            return Err(Error::invalid_argument(format!(
                "image config size mismatch: descriptor says {expected_config_size}, downloaded {config_size}"
            )));
        }
        let config_bytes = self.blob_store().get_verified(&manifest.config.digest)?;
        let config: Image = serde_json::from_slice(&config_bytes).map_err(|error| {
            Error::invalid_argument(format!("invalid image config JSON: {error}"))
        })?;
        config.validate()?;
        if config.rootfs.diff_ids.len() != manifest.layers.len() {
            return Err(Error::invalid_argument(format!(
                "image config has {} diff_ids but manifest has {} layers",
                config.rootfs.diff_ids.len(),
                manifest.layers.len()
            )));
        }

        let config_platform = Platform {
            os: config.os.clone(),
            architecture: config.architecture.clone(),
            variant: selected_platform
                .as_ref()
                .and_then(|platform| platform.variant.clone()),
            os_version: selected_platform
                .as_ref()
                .and_then(|platform| platform.os_version.clone()),
            os_features: selected_platform
                .as_ref()
                .and_then(|platform| platform.os_features.clone()),
        };
        if let Some(selected) = &selected_platform {
            if selected.os != config_platform.os
                || selected.architecture != config_platform.architecture
            {
                return Err(Error::invalid_argument(format!(
                    "manifest list selected {}/{} but the image config says {}/{}",
                    selected.os,
                    selected.architecture,
                    config_platform.os,
                    config_platform.architecture
                )));
            }
        }
        if let Some(wanted) = &requested_platform {
            if config_platform.os != wanted.os
                || config_platform.architecture != wanted.architecture
            {
                return Err(Error::not_found(format!(
                    "image platform {}/{} does not match requested {}/{}",
                    config_platform.os,
                    config_platform.architecture,
                    wanted.os,
                    wanted.architecture
                )));
            }
            if let (Some(wanted_variant), Some(actual_variant)) =
                (&wanted.variant, &config_platform.variant)
            {
                if wanted_variant != actual_variant {
                    return Err(Error::not_found(format!(
                        "image platform variant {actual_variant} does not match requested {wanted_variant}"
                    )));
                }
            }
        }

        let mut layer_size = 0i64;
        for layer in &manifest.layers {
            let expected = layer.size;
            if expected < 0 {
                return Err(Error::invalid_argument(format!(
                    "layer {} has a negative descriptor size",
                    layer.digest
                )));
            }
            let downloaded = client.pull_blob_to_store(&layer.digest, self.blob_store())?;
            if downloaded != expected as u64 {
                return Err(Error::invalid_argument(format!(
                    "layer {} size mismatch: descriptor says {expected}, downloaded {downloaded}",
                    layer.digest
                )));
            }
            layer_size = layer_size
                .checked_add(expected)
                .ok_or_else(|| Error::invalid_argument("image layer sizes overflow i64"))?;
        }

        for (digest, bytes) in &manifest_blobs {
            self.blob_store().put_verified(digest, bytes)?;
        }

        let created = config
            .created
            .as_ref()
            .map(|created| created.timestamp())
            .unwrap_or_else(unix_now);
        let config_size = i64::try_from(config_size)
            .map_err(|_| Error::invalid_argument("image config is too large"))?;
        let size = config_size
            .checked_add(layer_size)
            .ok_or_else(|| Error::invalid_argument("image size overflows i64"))?;
        let tags = tag.iter().cloned().collect();
        let repo_digests = [canonical_repo_digest(&image_ref, &top.digest)]
            .into_iter()
            .collect();
        let record = ImageRecord {
            id: manifest.config.digest,
            manifest_digest: top.digest,
            manifest_digests,
            tags,
            repo_digests,
            created,
            pulled_at: unix_now(),
            size,
            config_size,
            platform: config_platform,
            config,
            layers: manifest.layers,
        };
        self.insert(record.clone())?;
        let image = self.resolve(&record.id.full())?;
        let reference = tag.as_deref().map(familiar_reference).unwrap_or_else(|| {
            let parsed = ImageRef::parse(reference).expect("reference parsed at pull start");
            familiar_reference(&canonical_repo_digest(&parsed, &record.manifest_digest))
        });
        Ok(PullResult {
            image,
            reference,
            up_to_date: previous.is_some_and(|previous| previous == record.manifest_digest),
        })
    }
}

fn validate_manifest(manifest: &Manifest) -> Result<()> {
    if manifest.schema_version != docker_image::SCHEMA_VERSION {
        return Err(Error::invalid_argument(format!(
            "invalid manifest: schemaVersion {} unsupported",
            manifest.schema_version
        )));
    }
    if !matches!(
        manifest.config.media_type.as_str(),
        MEDIA_TYPE_IMAGE_CONFIG | DOCKER_CONFIG_MEDIA_TYPE
    ) {
        return Err(Error::unsupported(format!(
            "unsupported image config media type `{}`",
            manifest.config.media_type
        )));
    }
    if manifest.config.size < 0 {
        return Err(Error::invalid_argument(
            "image config descriptor has a negative size",
        ));
    }
    for layer in &manifest.layers {
        if layer.size < 0 {
            return Err(Error::invalid_argument(format!(
                "layer {} has a negative size",
                layer.digest
            )));
        }
    }
    Ok(())
}

fn select_platform<'a>(
    manifests: &'a [docker_image::Descriptor],
    wanted: &Platform,
) -> Result<&'a docker_image::Descriptor> {
    let mut available = Vec::new();
    for descriptor in manifests {
        let Some(platform) = &descriptor.platform else {
            continue;
        };
        available.push(format!(
            "{}/{}{}",
            platform.os,
            platform.architecture,
            platform
                .variant
                .as_deref()
                .map(|variant| format!("/{variant}"))
                .unwrap_or_default()
        ));
        if platform_matches(platform, wanted) {
            return Ok(descriptor);
        }
    }
    Err(Error::not_found(format!(
        "no matching manifest for {}/{}{} in the manifest list entries; available: {}",
        wanted.os,
        wanted.architecture,
        wanted
            .variant
            .as_deref()
            .map(|variant| format!("/{variant}"))
            .unwrap_or_default(),
        if available.is_empty() {
            "<none>".to_string()
        } else {
            available.join(", ")
        }
    )))
}

fn is_loopback_registry(authority: &str) -> bool {
    let host = if let Some(rest) = authority.strip_prefix('[') {
        rest.split_once(']').map(|(host, _)| host).unwrap_or(rest)
    } else {
        authority
            .rsplit_once(':')
            .map(|(host, _)| host)
            .unwrap_or(authority)
    };
    host.eq_ignore_ascii_case("localhost")
        || IpAddr::from_str(host).is_ok_and(|address| address.is_loopback())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn header(value: &str) -> String {
        base64::engine::general_purpose::STANDARD.encode(value)
    }

    #[test]
    fn anonymous_registry_auth_is_none() {
        assert!(parse_registry_auth("").unwrap().is_none());
        assert!(parse_registry_auth(&header("{}")).unwrap().is_none());
        assert!(
            parse_registry_auth(&header(r#"{"username":"","password":""}"#))
                .unwrap()
                .is_none()
        );
    }

    #[test]
    fn parses_username_password_and_legacy_auth() {
        let auth = parse_registry_auth(&header(
            r#"{"username":"user","password":"pass","serveraddress":"registry.example.com"}"#,
        ))
        .unwrap()
        .unwrap();
        assert_eq!(auth.username, "user");
        assert_eq!(auth.password, "pass");

        let legacy = format!(
            r#"{{"auth":"{}"}}"#,
            base64::engine::general_purpose::STANDARD.encode("user:pass")
        );
        let auth = parse_registry_auth(&header(&legacy)).unwrap().unwrap();
        assert_eq!(auth.username, "user");
        assert_eq!(auth.password, "pass");
    }

    #[test]
    fn rejects_tokens_and_malformed_headers() {
        assert_eq!(
            parse_registry_auth(&header(r#"{"identitytoken":"token"}"#))
                .unwrap_err()
                .kind(),
            docker_errors::ErrorKind::Unsupported
        );
        assert!(parse_registry_auth("not-base64!").is_err());
        assert!(parse_registry_auth(&header("[]")).is_err());
    }

    #[test]
    fn loopback_detection_handles_ports_and_ipv6() {
        assert!(is_loopback_registry("localhost"));
        assert!(is_loopback_registry("localhost:5000"));
        assert!(is_loopback_registry("127.0.0.1:5000"));
        assert!(is_loopback_registry("[::1]:5000"));
        assert!(!is_loopback_registry("registry.example.com"));
    }

    #[test]
    fn selects_requested_platform() {
        let linux_amd64 = Platform {
            os: "linux".into(),
            architecture: "amd64".into(),
            ..Platform::default()
        };
        let linux_arm64 = Platform {
            os: "linux".into(),
            architecture: "arm64".into(),
            variant: Some("v8".into()),
            ..Platform::default()
        };
        let descriptor = |platform: Platform| docker_image::Descriptor {
            media_type: docker_image::MEDIA_TYPE_IMAGE_MANIFEST.into(),
            platform: Some(platform),
            ..docker_image::Descriptor::default()
        };
        let manifests = vec![
            descriptor(linux_amd64.clone()),
            descriptor(linux_arm64.clone()),
        ];
        assert_eq!(
            select_platform(&manifests, &linux_amd64)
                .unwrap()
                .platform
                .as_ref()
                .unwrap()
                .architecture,
            "amd64"
        );
        let error = select_platform(
            &manifests,
            &Platform {
                os: "windows".into(),
                architecture: "amd64".into(),
                ..Platform::default()
            },
        )
        .unwrap_err();
        assert_eq!(error.kind(), docker_errors::ErrorKind::NotFound);
        assert!(error.message().contains("linux/amd64"));
    }
}
