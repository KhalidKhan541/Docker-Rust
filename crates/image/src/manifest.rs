//! Image manifests — port of `upstream/image-spec/specs-go/v1/manifest.go`
//! plus the embedded `specs.Versioned` (`specs-go/versioned.go`).

use std::collections::BTreeMap;

use docker_errors::{Error, Result};
use serde::{Deserialize, Serialize};

use crate::descriptor::Descriptor;
use crate::mediatype::{MEDIA_TYPE_IMAGE_CONFIG, MEDIA_TYPE_IMAGE_MANIFEST};

/// Current OCI image manifest/index schema version.
pub const SCHEMA_VERSION: i32 = 2;

/// Port of the upstream `Manifest` struct (schemaVersion from the embedded
/// `specs.Versioned`).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Manifest {
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

    /// Configuration object referenced by digest.
    pub config: Descriptor,

    /// Indexed list of layers referenced by the manifest.
    pub layers: Vec<Descriptor>,

    /// Optional link to another manifest (referrers association).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub subject: Option<Descriptor>,

    /// Arbitrary metadata for the image manifest.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub annotations: Option<BTreeMap<String, String>>,
}

impl Manifest {
    /// Builder with the schemaVersion and mediaType the spec expects.
    pub fn new(config: Descriptor, layers: Vec<Descriptor>) -> Self {
        Self {
            schema_version: SCHEMA_VERSION,
            media_type: Some(MEDIA_TYPE_IMAGE_MANIFEST.to_string()),
            artifact_type: None,
            config,
            layers,
            subject: None,
            annotations: None,
        }
    }

    /// Spec conformance checks: schema version, config descriptor media
    /// type (the manifest schema pins it to the image config type), and
    /// per-descriptor validity.
    pub fn validate(&self) -> Result<()> {
        if self.schema_version != SCHEMA_VERSION {
            return Err(Error::invalid_argument(format!(
                "invalid manifest: schemaVersion {} unsupported, expected {SCHEMA_VERSION}",
                self.schema_version
            )));
        }
        self.config.validate().map_err(|e| {
            Error::invalid_argument(format!("invalid manifest config: {}", e.message()))
        })?;
        if self.config.media_type != MEDIA_TYPE_IMAGE_CONFIG {
            return Err(Error::invalid_argument(format!(
                "invalid manifest: config descriptor mediaType `{}` must be `{MEDIA_TYPE_IMAGE_CONFIG}`",
                self.config.media_type
            )));
        }
        for (i, layer) in self.layers.iter().enumerate() {
            layer.validate().map_err(|e| {
                Error::invalid_argument(format!("invalid manifest layer {i}: {}", e.message()))
            })?;
        }
        if let Some(subject) = &self.subject {
            subject.validate().map_err(|e| {
                Error::invalid_argument(format!("invalid manifest subject: {}", e.message()))
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
            media_type: crate::MEDIA_TYPE_IMAGE_CONFIG.into(),
            digest: docker_common::Digest::parse(&format!("sha256:{D64}")).unwrap(),
            size: 707,
            ..Default::default()
        }
    }

    #[test]
    fn wire_shape_matches_go_tags() {
        let m = Manifest::new(d(), vec![d()]);
        let v = serde_json::to_value(&m).unwrap();
        assert_eq!(v["schemaVersion"], 2);
        assert_eq!(v["mediaType"], "application/vnd.oci.image.manifest.v1+json");
        assert_eq!(
            v["config"]["mediaType"],
            "application/vnd.oci.image.config.v1+json"
        );
        assert_eq!(v["layers"].as_array().unwrap().len(), 1);
        assert!(v.get("subject").is_none());
        assert!(v.get("artifactType").is_none());
        assert!(v.get("annotations").is_none());

        let back: Manifest = serde_json::from_value(v).unwrap();
        assert_eq!(back, m);
    }

    #[test]
    fn optional_fields_serialize_when_present() {
        let mut m = Manifest::new(d(), vec![]);
        m.annotations = Some([("a".to_string(), "b".to_string())].into_iter().collect());
        m.subject = Some(d());
        m.artifact_type = Some("application/vnd.example.type".into());
        let v = serde_json::to_value(&m).unwrap();
        assert_eq!(v["annotations"]["a"], "b");
        assert_eq!(v["subject"]["size"], 707);
        assert_eq!(v["artifactType"], "application/vnd.example.type");
        let back: Manifest = serde_json::from_value(v).unwrap();
        assert_eq!(back, m);
    }

    #[test]
    fn validate_rejects_bad_schema_and_config_mediatype() {
        let mut m = Manifest::new(d(), vec![]);
        m.schema_version = 3;
        let err = m.validate().unwrap_err();
        assert_eq!(err.kind(), docker_errors::ErrorKind::InvalidArgument);
        assert!(err.message().contains("schemaVersion"));

        m.schema_version = SCHEMA_VERSION;
        m.config.media_type = "application/json".into();
        let err = m.validate().unwrap_err();
        assert!(err.message().contains("must be"));
    }

    #[test]
    fn validate_reports_failing_layer_index() {
        let mut layer = d();
        layer.size = -1;
        let m = Manifest::new(d(), vec![layer]);
        let err = m.validate().unwrap_err();
        assert_eq!(err.kind(), docker_errors::ErrorKind::InvalidArgument);
        assert!(err.message().contains("layer 0"));
    }
}
