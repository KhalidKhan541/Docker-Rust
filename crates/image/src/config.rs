//! Image configuration — port of `upstream/image-spec/specs-go/v1/config.go`.
//!
//! The in-image `config` execution parameters use **PascalCase** JSON keys
//! (`User`, `ExposedPorts`, `Env`, `Entrypoint`, `Cmd`, `Volumes`,
//! `WorkingDir`, `Labels`, `StopSignal`, `ArgsEscaped`) — a quirk inherited
//! from the Docker image format, verified verbatim from the upstream Go
//! JSON tags. The surrounding image structures use lowercase keys.

use std::collections::BTreeMap;

use chrono::{DateTime, Utc};
use docker_common::Digest;
use docker_errors::{Error, Result};
use serde::{Deserialize, Serialize};

fn is_false(v: &bool) -> bool {
    !*v
}

/// Go `map[string]struct{}` wire presence: keys with `{}` values.
/// Go's `encoding/json` accepts any JSON object for these (unknown keys are
/// ignored), so decoding is lenient here too.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct EmptyObject;

impl Serialize for EmptyObject {
    fn serialize<S: serde::ser::Serializer>(&self, s: S) -> std::result::Result<S::Ok, S::Error> {
        use serde::ser::SerializeMap as _;
        let map = s.serialize_map(Some(0))?;
        map.end()
    }
}

impl<'de> Deserialize<'de> for EmptyObject {
    fn deserialize<D: serde::Deserializer<'de>>(d: D) -> std::result::Result<Self, D::Error> {
        struct V;
        impl<'de> serde::de::Visitor<'de> for V {
            type Value = EmptyObject;
            fn expecting(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
                f.write_str("a JSON object")
            }
            fn visit_map<A: serde::de::MapAccess<'de>>(
                self,
                _m: A,
            ) -> std::result::Result<Self::Value, A::Error> {
                Ok(EmptyObject)
            }
        }
        d.deserialize_map(V)
    }
}

/// Execution parameters used as the base when running a container from an
/// image. Port of the upstream `ImageConfig` struct (PascalCase wire keys).
#[derive(Debug, Clone, PartialEq, Eq, Default, Serialize, Deserialize)]
pub struct ImageConfig {
    /// Username or UID the process in the container runs as.
    #[serde(rename = "User", default, skip_serializing_if = "String::is_empty")]
    pub user: String,

    /// Ports to expose from containers running this image.
    #[serde(
        rename = "ExposedPorts",
        default,
        skip_serializing_if = "BTreeMap::is_empty"
    )]
    pub exposed_ports: BTreeMap<String, EmptyObject>,

    /// Environment variables for the container.
    #[serde(rename = "Env", default, skip_serializing_if = "Vec::is_empty")]
    pub env: Vec<String>,

    /// Arguments used as the command to execute at container start.
    #[serde(rename = "Entrypoint", default, skip_serializing_if = "Vec::is_empty")]
    pub entrypoint: Vec<String>,

    /// Default arguments to the entrypoint.
    #[serde(rename = "Cmd", default, skip_serializing_if = "Vec::is_empty")]
    pub cmd: Vec<String>,

    /// Directories the process is likely to write data into.
    #[serde(
        rename = "Volumes",
        default,
        skip_serializing_if = "BTreeMap::is_empty"
    )]
    pub volumes: BTreeMap<String, EmptyObject>,

    /// Current working directory of the entrypoint process.
    #[serde(
        rename = "WorkingDir",
        default,
        skip_serializing_if = "String::is_empty"
    )]
    pub working_dir: String,

    /// Arbitrary metadata for the container.
    #[serde(rename = "Labels", default, skip_serializing_if = "BTreeMap::is_empty")]
    pub labels: BTreeMap<String, String>,

    /// System call signal sent to the container to exit.
    #[serde(
        rename = "StopSignal",
        default,
        skip_serializing_if = "String::is_empty"
    )]
    pub stop_signal: String,

    /// Deprecated Docker-only escape flag; kept for round-tripping
    /// existing images, never produced by docker-rs builders.
    #[serde(rename = "ArgsEscaped", default, skip_serializing_if = "is_false")]
    pub args_escaped: bool,
}

/// Layer content hashes, bottom-most first. Port of the upstream `RootFS`.
#[derive(Debug, Clone, PartialEq, Eq, Default, Serialize, Deserialize)]
pub struct RootFS {
    /// Type of the rootfs; MUST be `layers` (spec conformance-checked).
    #[serde(rename = "type")]
    pub rootfs_type: String,

    /// Layer content hashes (DiffIDs), bottom-most to top-most.
    #[serde(rename = "diff_ids")]
    pub diff_ids: Vec<Digest>,
}

impl RootFS {
    /// The only rootfs type the spec allows.
    pub const TYPE_LAYERS: &'static str = "layers";

    pub fn layers(diff_ids: Vec<Digest>) -> Self {
        Self {
            rootfs_type: Self::TYPE_LAYERS.to_string(),
            diff_ids,
        }
    }
}

/// One build step of an image's history. Port of the upstream `History`.
#[derive(Debug, Clone, PartialEq, Eq, Default, Serialize, Deserialize)]
pub struct History {
    /// Creation time of the layer (RFC 3339 on the wire).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub created: Option<DateTime<Utc>>,

    /// Command which created the layer.
    #[serde(
        rename = "created_by",
        default,
        skip_serializing_if = "Option::is_none"
    )]
    pub created_by: Option<String>,

    /// Author of the build point.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub author: Option<String>,

    /// Custom message set when creating the layer.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub comment: Option<String>,

    /// Marks history items that created no filesystem diff.
    #[serde(rename = "empty_layer", default, skip_serializing_if = "is_false")]
    pub empty_layer: bool,
}

/// The image configuration JSON. Port of the upstream `Image` struct.
///
/// Note on the Go *embedded* `Platform`: `architecture` and `os` appear at
/// the TOP LEVEL of the image config JSON (verified from the upstream
/// embedding), not under a `platform` key.
#[derive(Debug, Clone, PartialEq, Eq, Default, Serialize, Deserialize)]
pub struct Image {
    /// Creation time of the image (RFC 3339 on the wire).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub created: Option<DateTime<Utc>>,

    /// Name and/or email of the image maintainer.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub author: Option<String>,

    /// Embedded platform fields (upstream Go embedding of `Platform`).
    pub architecture: String,
    pub os: String,

    /// Execution parameters; always serialized like Go (`omitempty` has no
    /// effect on structs in `encoding/json`).
    #[serde(rename = "config", default)]
    pub config: ImageConfig,

    /// Layer content addresses. Required by the spec schema.
    pub rootfs: RootFS,

    /// Per-layer history.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub history: Vec<History>,
}

impl Image {
    /// Spec conformance checks: platform fields required and rootfs type.
    pub fn validate(&self) -> Result<()> {
        if self.architecture.is_empty() {
            return Err(Error::invalid_argument(
                "invalid image config: architecture is required",
            ));
        }
        if self.os.is_empty() {
            return Err(Error::invalid_argument(
                "invalid image config: os is required",
            ));
        }
        if self.rootfs.rootfs_type != RootFS::TYPE_LAYERS {
            return Err(Error::invalid_argument(format!(
                "invalid image config: rootfs.type `{}` must be `{}`",
                self.rootfs.rootfs_type,
                RootFS::TYPE_LAYERS
            )));
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    const D64: &str = "0123456789abcdef0123456789abcdef0123456789abcdef0123456789abcdef";

    fn dg() -> Digest {
        Digest::parse(&format!("sha256:{D64}")).unwrap()
    }

    #[test]
    fn image_config_uses_pascal_case_wire_keys() {
        let c = ImageConfig {
            user: "1000:1000".into(),
            env: vec!["PATH=/usr/bin".into()],
            entrypoint: vec!["/entry".into()],
            cmd: vec!["-v".into()],
            working_dir: "/work".into(),
            labels: [("k".to_string(), "v".to_string())].into_iter().collect(),
            stop_signal: "SIGTERM".into(),
            ..ImageConfig::default()
        };
        let v = serde_json::to_value(&c).unwrap();
        assert_eq!(v["User"], "1000:1000");
        assert_eq!(v["Env"][0], "PATH=/usr/bin");
        assert_eq!(v["Entrypoint"][0], "/entry");
        assert_eq!(v["Cmd"][0], "-v");
        assert_eq!(v["WorkingDir"], "/work");
        assert_eq!(v["Labels"]["k"], "v");
        assert_eq!(v["StopSignal"], "SIGTERM");
        assert!(v.get("ExposedPorts").is_none());
        assert!(v.get("Volumes").is_none());
        assert!(v.get("ArgsEscaped").is_none());
        // A zero config serializes to `{}` exactly like Go's json.Marshal.
        assert_eq!(
            serde_json::to_value(ImageConfig::default()).unwrap(),
            json!({})
        );
    }

    #[test]
    fn exposed_ports_and_volumes_wire() {
        let c = ImageConfig {
            exposed_ports: [("80/tcp".to_string(), EmptyObject)].into_iter().collect(),
            volumes: [("/data".to_string(), EmptyObject)].into_iter().collect(),
            ..ImageConfig::default()
        };
        let v = serde_json::to_value(&c).unwrap();
        assert_eq!(v["ExposedPorts"]["80/tcp"], json!({}));
        assert_eq!(v["Volumes"]["/data"], json!({}));
        let back: ImageConfig = serde_json::from_value(v).unwrap();
        assert_eq!(back, c);
    }

    #[test]
    fn args_escaped_round_trip() {
        let c = ImageConfig {
            args_escaped: true,
            ..ImageConfig::default()
        };
        let v = serde_json::to_value(&c).unwrap();
        assert_eq!(v["ArgsEscaped"], true);
        let back: ImageConfig = serde_json::from_value(v).unwrap();
        assert!(back.args_escaped);
    }

    #[test]
    fn image_wire_embeds_platform_at_top_level() {
        let img = Image {
            created: Some(
                DateTime::parse_from_rfc3339("2026-01-01T00:00:00Z")
                    .unwrap()
                    .with_timezone(&Utc),
            ),
            author: Some("docker-rs".into()),
            architecture: "amd64".into(),
            os: "linux".into(),
            config: ImageConfig {
                env: vec!["A=1".into()],
                ..ImageConfig::default()
            },
            rootfs: RootFS::layers(vec![dg(), dg()]),
            history: vec![History {
                created_by: Some("RUN x".into()),
                ..History::default()
            }],
        };
        let v = serde_json::to_value(&img).unwrap();
        assert_eq!(v["architecture"], "amd64");
        assert_eq!(v["os"], "linux");
        assert_eq!(v["rootfs"]["type"], "layers");
        assert_eq!(v["rootfs"]["diff_ids"][0], format!("sha256:{D64}"));
        assert_eq!(v["config"]["Env"][0], "A=1");
        assert_eq!(v["history"][0]["created_by"], "RUN x");
        assert!(v["created"]
            .as_str()
            .unwrap()
            .starts_with("2026-01-01T00:00:00"));
        img.validate().unwrap();

        let back: Image = serde_json::from_value(v).unwrap();
        assert_eq!(back, img);
    }

    #[test]
    fn image_validate_rejects_bad_rootfs_and_missing_platform() {
        let img = Image {
            rootfs: RootFS {
                rootfs_type: "squashfs".into(),
                diff_ids: vec![],
            },
            ..Image::default()
        };
        assert_eq!(
            img.validate().unwrap_err().kind(),
            docker_errors::ErrorKind::InvalidArgument
        );

        let img2 = Image {
            architecture: "amd64".into(),
            os: String::new(),
            rootfs: RootFS::layers(vec![dg()]),
            ..Image::default()
        };
        assert!(img2.validate().is_err());
    }
}
