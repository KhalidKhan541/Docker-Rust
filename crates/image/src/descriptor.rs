//! Content descriptors — port of `upstream/image-spec/specs-go/v1/descriptor.go`
//! with JSON tags verified verbatim against the pinned upstream revision.

use std::collections::BTreeMap;

use docker_common::Digest;
use docker_errors::{Error, Result};
use serde::{Deserialize, Serialize};

/// Go's `encoding/json` codec for the embedded `data` field (`[]byte` is
/// base64-encoded automatically by encoding/json; we mirror that exactly).
mod base64_bytes {
    use base64::Engine as _;
    use serde::{Deserialize, Deserializer, Serializer};

    pub fn serialize<S: Serializer>(
        v: &Option<Vec<u8>>,
        s: S,
    ) -> std::result::Result<S::Ok, S::Error> {
        match v {
            Some(b) => s.serialize_str(&base64::engine::general_purpose::STANDARD.encode(b)),
            None => s.serialize_none(),
        }
    }

    pub fn deserialize<'de, D: Deserializer<'de>>(
        d: D,
    ) -> std::result::Result<Option<Vec<u8>>, D::Error> {
        let Some(s) = Option::<String>::deserialize(d)? else {
            return Ok(None);
        };
        base64::engine::general_purpose::STANDARD
            .decode(s.as_bytes())
            .map(Some)
            .map_err(|e| {
                serde::de::Error::custom(format!("invalid base64 in descriptor data: {e}"))
            })
    }
}

/// Describes the disposition of targeted content.
///
/// Port of the upstream `Descriptor` struct. Unknown fields are ignored on
/// deserialize, mirroring Go's `encoding/json` behavior (strict conformance
/// validation happens at protocol boundaries, not parse time).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Descriptor {
    /// Media type of the object this schema refers to.
    #[serde(rename = "mediaType")]
    pub media_type: String,

    /// Digest of the targeted content.
    pub digest: Digest,

    /// Size in bytes of the blob.
    pub size: i64,

    /// URLs from which this object MAY be downloaded.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub urls: Vec<String>,

    /// Arbitrary metadata relating to the targeted content.
    #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
    pub annotations: BTreeMap<String, String>,

    /// Optional embedding of the targeted content, base64 on the wire.
    #[serde(
        default,
        with = "base64_bytes",
        skip_serializing_if = "Option::is_none"
    )]
    pub data: Option<Vec<u8>>,

    /// Platform of the referred manifest; only valid when the descriptor
    /// refers to a manifest.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub platform: Option<Platform>,

    /// IANA media type of this artifact.
    #[serde(
        rename = "artifactType",
        default,
        skip_serializing_if = "Option::is_none"
    )]
    pub artifact_type: Option<String>,
}

/// Describes the platform which the image in the manifest runs on.
/// Port of the upstream `Platform` struct.
#[derive(Debug, Clone, PartialEq, Eq, Default, Serialize, Deserialize)]
pub struct Platform {
    /// CPU architecture, for example `amd64` or `ppc64le`.
    pub architecture: String,

    /// Operating system, for example `linux` or `windows`.
    pub os: String,

    /// Optional operating system version, for example `10.0.14393.1066`.
    #[serde(
        rename = "os.version",
        default,
        skip_serializing_if = "Option::is_none"
    )]
    pub os_version: Option<String>,

    /// Optional array of required OS features (for example `win32k`).
    #[serde(
        rename = "os.features",
        default,
        skip_serializing_if = "Option::is_none"
    )]
    pub os_features: Option<Vec<String>>,

    /// Optional CPU variant, for example `v7` for ARMv7.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub variant: Option<String>,
}

/// Manual `Default` (not derived): `Digest` deliberately has no zero value,
/// so the default digest is the real digest of empty content; an unfilled
/// descriptor still fails `validate()` because its mediaType is empty.
impl Default for Descriptor {
    fn default() -> Self {
        Self {
            media_type: String::new(),
            digest: Digest::sha256(b""),
            size: 0,
            urls: Vec::new(),
            annotations: BTreeMap::new(),
            data: None,
            platform: None,
            artifact_type: None,
        }
    }
}

impl Descriptor {
    /// Hex portion of the upstream `DescriptorEmptyJSON` digest
    /// (`sha256` of the literal `{}`), from `descriptor.go`.
    pub const EMPTY_JSON_DIGEST_HEX: &'static str =
        "44136fa355b3678a1146ad16f7e8649e94fb4fc21fe77e8310c060f61caaff8a";

    /// Upstream `DescriptorEmptyJSON`: descriptor of the literal `{}` blob.
    pub fn empty_json() -> Self {
        Self {
            media_type: crate::MEDIA_TYPE_EMPTY_JSON.to_string(),
            digest: Digest::parse(&format!("sha256:{}", Self::EMPTY_JSON_DIGEST_HEX))
                .expect("hardcoded empty-JSON digest must parse"),
            size: 2,
            data: Some(b"{}".to_vec()),
            ..Self::default()
        }
    }

    /// Spec conformance checks: mediaType required, non-negative size, and
    /// embedded `data` (when present) must match `size`.
    pub fn validate(&self) -> Result<()> {
        if self.media_type.is_empty() {
            return Err(Error::invalid_argument(
                "invalid descriptor: mediaType is required",
            ));
        }
        if self.size < 0 {
            return Err(Error::invalid_argument(format!(
                "invalid descriptor: size {} is negative",
                self.size
            )));
        }
        if let Some(data) = &self.data {
            if self.size != data.len() as i64 {
                return Err(Error::invalid_argument(format!(
                    "invalid descriptor: size {} does not match embedded data length {}",
                    self.size,
                    data.len()
                )));
            }
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use base64::Engine as _;
    use serde_json::json;

    const D64: &str = "0123456789abcdef0123456789abcdef0123456789abcdef0123456789abcdef";

    fn d() -> Digest {
        Digest::parse(&format!("sha256:{D64}")).unwrap()
    }

    #[test]
    fn minimal_wire_shape_matches_go_tags() {
        let desc = Descriptor {
            media_type: crate::MEDIA_TYPE_IMAGE_CONFIG.into(),
            digest: d(),
            size: 707,
            ..Default::default()
        };
        let v = serde_json::to_value(&desc).unwrap();
        assert_eq!(
            v,
            json!({
                "mediaType": "application/vnd.oci.image.config.v1+json",
                "digest": format!("sha256:{D64}"),
                "size": 707
            })
        );
        let back: Descriptor = serde_json::from_value(v).unwrap();
        assert_eq!(back, desc);
    }

    #[test]
    fn full_fields_wire_casing() {
        let desc = Descriptor {
            media_type: crate::MEDIA_TYPE_IMAGE_LAYER_GZIP.into(),
            digest: d(),
            size: 9,
            urls: vec!["https://mirror.example/a.tar.gz".into()],
            annotations: [(
                "org.opencontainers.image.title".to_string(),
                "hello".to_string(),
            )]
            .into_iter()
            .collect(),
            data: Some(b"raw-layer".to_vec()),
            platform: Some(Platform {
                architecture: "amd64".into(),
                os: "linux".into(),
                os_version: Some("10.0.14393.1066".into()),
                os_features: Some(vec!["win32k".into()]),
                variant: Some("v2".into()),
            }),
            artifact_type: Some("application/vnd.example.type".into()),
        };
        let v = serde_json::to_value(&desc).unwrap();
        assert_eq!(v["urls"][0], "https://mirror.example/a.tar.gz");
        assert_eq!(v["annotations"]["org.opencontainers.image.title"], "hello");
        assert_eq!(
            v["data"],
            base64::engine::general_purpose::STANDARD.encode(b"raw-layer")
        );
        assert_eq!(v["platform"]["architecture"], "amd64");
        assert_eq!(v["platform"]["os"], "linux");
        assert_eq!(v["platform"]["os.version"], "10.0.14393.1066");
        assert_eq!(v["platform"]["os.features"][0], "win32k");
        assert_eq!(v["platform"]["variant"], "v2");
        assert_eq!(v["artifactType"], "application/vnd.example.type");
        assert_eq!(v["size"], 9);
        assert_eq!(v["digest"], format!("sha256:{D64}"));
        assert_eq!(
            v["mediaType"],
            "application/vnd.oci.image.layer.v1.tar+gzip"
        );

        let back: Descriptor = serde_json::from_value(v).unwrap();
        assert_eq!(back, desc);
    }

    #[test]
    fn unknown_fields_ignored_like_go_unmarshal() {
        let raw = json!({
            "mediaType": "application/vnd.oci.image.layer.v1.tar",
            "digest": format!("sha256:{D64}"),
            "size": 5,
            "futureField": { "x": 1 }
        });
        let desc: Descriptor = serde_json::from_value(raw).unwrap();
        assert_eq!(desc.size, 5);
    }

    #[test]
    fn validate_rules() {
        let mut desc = Descriptor {
            media_type: "x".into(),
            digest: d(),
            size: -1,
            ..Default::default()
        };
        assert_eq!(
            desc.validate().unwrap_err().kind(),
            docker_errors::ErrorKind::InvalidArgument
        );
        desc.size = 3;
        desc.validate().unwrap();
        desc.data = Some(vec![1, 2]);
        assert!(desc.validate().is_err()); // size 3 != embedded len 2
        desc.data = Some(vec![1, 2, 3]);
        desc.validate().unwrap();
        desc.media_type = String::new();
        assert!(desc.validate().is_err());
    }

    #[test]
    fn empty_json_descriptor() {
        let e = Descriptor::empty_json();
        assert_eq!(e.size, 2);
        assert_eq!(e.data.as_deref(), Some(b"{}".as_slice()));
        assert_eq!(e.media_type, crate::MEDIA_TYPE_EMPTY_JSON);
        assert_eq!(e.digest.hex(), Descriptor::EMPTY_JSON_DIGEST_HEX);
        e.validate().unwrap();
        let v = serde_json::to_value(&e).unwrap();
        assert_eq!(v["data"], "e30=");
    }
}
