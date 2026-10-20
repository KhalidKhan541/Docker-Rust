//! Image indexes — port of `upstream/image-spec/specs-go/v1/index.go`
//! plus the embedded `specs.Versioned` (`specs-go/versioned.go`).

use std::collections::BTreeMap;

use docker_errors::{Error, Result};
use serde::{Deserialize, Serialize};

use crate::descriptor::Descriptor;
use crate::mediatype::MEDIA_TYPE_IMAGE_INDEX;
use crate::SCHEMA_VERSION;

/// Port of the upstream `Index` struct: references platform-specific
/// manifests.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Index {
    #[serde(rename = "schemaVersion")]
    pub schema_version: i32,

    #[serde(rename = "mediaType", default, skip_serializing_if = "Option::is_none")]
    pub media_type: Option<String>,

    #[serde(
        rename = "artifactType",
        default,
        skip_serializing_if = "Option::is_none"
    )]
    pub artifact_type: Option<String>,

    /// Platform-specific manifests this index references.
    pub manifests: Vec<Descriptor>,

    /// Optional link to another manifest (referrers association).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub subject: Option<Descriptor>,

    /// Arbitrary metadata for the image index.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub annotations: Option<BTreeMap<String, String>>,
}

impl Default for Index {
    fn default() -> Self {
        Self::new()
    }
}

impl Index {
    /// Builder with the schemaVersion and mediaType the spec expects.
    pub fn new() -> Self {
        Self {
            schema_version: SCHEMA_VERSION,
            media_type: Some(MEDIA_TYPE_IMAGE_INDEX.to_string()),
            artifact_type: None,
            manifests: Vec::new(),
            subject: None,
            annotations: None,
        }
    }

    pub fn add(&mut self, manifest: Descriptor) -> &mut Self {
        self.manifests.push(manifest);
        self
    }

    /// Spec conformance checks: schema version and per-descriptor validity.
    pub fn validate(&self) -> Result<()> {
        if self.schema_version != SCHEMA_VERSION {
            return Err(Error::invalid_argument(format!(
                "invalid index: schemaVersion {} unsupported, expected {SCHEMA_VERSION}",
                self.schema_version
            )));
        }
        for (i, entry) in self.manifests.iter().enumerate() {
            entry.validate().map_err(|e| {
                Error::invalid_argument(format!("invalid index entry {i}: {}", e.message()))
            })?;
        }
        if let Some(subject) = &self.subject {
            subject.validate().map_err(|e| {
                Error::invalid_argument(format!("invalid index subject: {}", e.message()))
            })?;
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const D64: &str = "0123456789abcdef0123456789abcdef0123456789abcdef0123456789abcdef";

    fn d() -> Descriptor {
        Descriptor {
            media_type: crate::MEDIA_TYPE_IMAGE_MANIFEST.into(),
            digest: docker_common::Digest::parse(&format!("sha256:{D64}")).unwrap(),
            size: 472,
            ..Default::default()
        }
    }

    #[test]
    fn wire_shape_matches_go_tags() {
        let mut idx = Index::new();
        idx.add(d());
        idx.artifact_type = Some("application/vnd.example.artifact".into());
        let v = serde_json::to_value(&idx).unwrap();
        assert_eq!(v["schemaVersion"], 2);
        assert_eq!(v["mediaType"], "application/vnd.oci.image.index.v1+json");
        assert_eq!(v["manifests"][0]["digest"], format!("sha256:{D64}"));
        assert_eq!(v["artifactType"], "application/vnd.example.artifact");
        assert!(v.get("subject").is_none());
        assert!(v.get("annotations").is_none());

        let back: Index = serde_json::from_value(v).unwrap();
        assert_eq!(back, idx);
    }

    #[test]
    fn validate_rejects_bad_schema_and_bad_entries() {
        let mut idx = Index::new();
        idx.schema_version = 1;
        assert_eq!(
            idx.validate().unwrap_err().kind(),
            docker_errors::ErrorKind::InvalidArgument
        );
        idx.schema_version = SCHEMA_VERSION;
        let mut bad = d();
        bad.size = -5;
        idx.add(bad);
        let err = idx.validate().unwrap_err();
        assert!(err.message().contains("entry 0"));
    }
}
