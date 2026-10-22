//! Docker Engine image wire types.
//!
//! Field names mirror `upstream/moby/api/types/image`; the local image model
//! is deliberately smaller, so conversion fills Docker's informational fields
//! with honest values (`Containers: -1`, `SharedSize: -1`) instead of guessing.

use std::collections::BTreeMap;

use chrono::{DateTime, SecondsFormat, Utc};
use docker_image::ImageConfig;
use docker_image_store::ImageRecord;
use serde::Serialize;

/// `GET /images/json` entry (`image.Summary`).
#[derive(Debug, Clone, Serialize)]
pub struct ImageSummary {
    #[serde(rename = "Containers")]
    pub containers: i64,
    #[serde(rename = "Created")]
    pub created: i64,
    #[serde(rename = "Id")]
    pub id: String,
    #[serde(rename = "Labels")]
    pub labels: BTreeMap<String, String>,
    #[serde(rename = "ParentId")]
    pub parent_id: String,
    #[serde(rename = "RepoDigests")]
    pub repo_digests: Option<Vec<String>>,
    #[serde(rename = "RepoTags")]
    pub repo_tags: Option<Vec<String>>,
    #[serde(rename = "SharedSize")]
    pub shared_size: i64,
    #[serde(rename = "Size")]
    pub size: i64,
    #[serde(rename = "VirtualSize")]
    pub virtual_size: i64,
}

impl From<&ImageRecord> for ImageSummary {
    fn from(record: &ImageRecord) -> Self {
        let tags = record.display_tags();
        let digests = record.display_repo_digests();
        Self {
            containers: -1,
            created: record.created,
            id: record.id.full(),
            labels: record.config.config.labels.clone(),
            parent_id: String::new(),
            repo_digests: (!digests.is_empty()).then_some(digests),
            repo_tags: (!tags.is_empty()).then_some(tags),
            shared_size: -1,
            size: record.size,
            virtual_size: record.size,
        }
    }
}

/// Storage-driver metadata (`storage.DriverData`).
#[derive(Debug, Clone, Serialize)]
pub struct GraphDriverData {
    #[serde(rename = "Data")]
    pub data: BTreeMap<String, String>,
    #[serde(rename = "Name")]
    pub name: String,
}

/// Root filesystem metadata (`image.RootFS`).
#[derive(Debug, Clone, Serialize)]
pub struct ImageRootFs {
    #[serde(rename = "Type")]
    pub rootfs_type: String,
    #[serde(rename = "Layers")]
    pub layers: Vec<String>,
}

/// `GET /images/{name}/json` (`image.InspectResponse`).
#[derive(Debug, Clone, Serialize)]
pub struct ImageInspectResponse {
    #[serde(rename = "Id")]
    pub id: String,
    #[serde(rename = "RepoTags")]
    pub repo_tags: Option<Vec<String>>,
    #[serde(rename = "RepoDigests")]
    pub repo_digests: Option<Vec<String>>,
    #[serde(rename = "Parent")]
    pub parent: String,
    #[serde(rename = "Comment")]
    pub comment: String,
    #[serde(rename = "Created", skip_serializing_if = "String::is_empty")]
    pub created: String,
    #[serde(rename = "Container")]
    pub container: String,
    #[serde(rename = "DockerVersion")]
    pub docker_version: String,
    #[serde(rename = "Author")]
    pub author: String,
    #[serde(rename = "Config")]
    pub config: ImageConfig,
    #[serde(rename = "Architecture")]
    pub architecture: String,
    #[serde(rename = "Variant", skip_serializing_if = "String::is_empty")]
    pub variant: String,
    #[serde(rename = "Os")]
    pub os: String,
    #[serde(rename = "OsVersion", skip_serializing_if = "String::is_empty")]
    pub os_version: String,
    #[serde(rename = "Size")]
    pub size: i64,
    #[serde(rename = "VirtualSize")]
    pub virtual_size: i64,
    #[serde(rename = "GraphDriver")]
    pub graph_driver: GraphDriverData,
    #[serde(rename = "RootFS")]
    pub rootfs: ImageRootFs,
    #[serde(rename = "Metadata", skip_serializing_if = "Option::is_none")]
    pub metadata: Option<ImageMetadata>,
}

/// `image.Metadata`; only fields that docker-rs actually knows are populated.
#[derive(Debug, Clone, Serialize)]
pub struct ImageMetadata {
    #[serde(rename = "LastTagTime")]
    pub last_tag_time: String,
}

impl From<&ImageRecord> for ImageInspectResponse {
    fn from(record: &ImageRecord) -> Self {
        let tags = record.display_tags();
        let digests = record.display_repo_digests();
        let created = record
            .config
            .created
            .or_else(|| DateTime::<Utc>::from_timestamp(record.created, 0))
            .map(|created| created.to_rfc3339_opts(SecondsFormat::Nanos, true))
            .unwrap_or_default();
        Self {
            id: record.id.full(),
            repo_tags: (!tags.is_empty()).then_some(tags),
            repo_digests: (!digests.is_empty()).then_some(digests),
            parent: String::new(),
            comment: String::new(),
            created,
            container: String::new(),
            docker_version: String::new(),
            author: record.config.author.clone().unwrap_or_default(),
            config: record.config.config.clone(),
            architecture: record.config.architecture.clone(),
            variant: record.platform.variant.clone().unwrap_or_default(),
            os: record.config.os.clone(),
            os_version: record.platform.os_version.clone().unwrap_or_default(),
            size: record.size,
            virtual_size: record.size,
            graph_driver: GraphDriverData {
                data: BTreeMap::new(),
                name: "docker-rs".to_string(),
            },
            rootfs: ImageRootFs {
                rootfs_type: record.config.rootfs.rootfs_type.clone(),
                layers: record
                    .config
                    .rootfs
                    .diff_ids
                    .iter()
                    .map(|digest| digest.full())
                    .collect(),
            },
            metadata: None,
        }
    }
}

/// One `POST /images/create` progress line. Docker's client parses a stream of
/// these objects; the current implementation buffers a small final summary.
#[derive(Debug, Clone, Serialize)]
pub struct ImagePullMessage {
    pub status: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub id: Option<String>,
}

impl ImagePullMessage {
    pub fn status(status: impl Into<String>) -> Self {
        Self {
            status: status.into(),
            id: None,
        }
    }

    pub fn with_id(status: impl Into<String>, id: impl Into<String>) -> Self {
        Self {
            status: status.into(),
            id: Some(id.into()),
        }
    }
}

/// `DELETE /images/{name}` item (`image.DeleteResponse`).
#[derive(Debug, Clone, Serialize)]
pub struct ImageDeleteItem {
    #[serde(rename = "Untagged", skip_serializing_if = "Option::is_none")]
    pub untagged: Option<String>,
    #[serde(rename = "Deleted", skip_serializing_if = "Option::is_none")]
    pub deleted: Option<String>,
}

/// `GET /images/{name}/history` entry (`image.HistoryResponseItem`).
#[derive(Debug, Clone, Serialize)]
pub struct ImageHistoryItem {
    #[serde(rename = "Id")]
    pub id: String,
    #[serde(rename = "Created")]
    pub created: i64,
    #[serde(rename = "CreatedBy")]
    pub created_by: String,
    #[serde(rename = "Tags")]
    pub tags: Option<Vec<String>>,
    #[serde(rename = "Size")]
    pub size: i64,
    #[serde(rename = "Comment")]
    pub comment: String,
    #[serde(rename = "EmptyLayer")]
    pub empty_layer: bool,
}

/// Build history items from the config history and compressed layer sizes.
/// Empty history entries consume no layer; non-empty entries consume one
/// manifest layer from the top down, matching `docker history` order.
pub fn history_items(record: &ImageRecord) -> Vec<ImageHistoryItem> {
    let tags = record.display_tags();
    let mut remaining: Vec<i64> = record.layers.iter().map(|layer| layer.size).collect();
    let mut items = Vec::with_capacity(record.config.history.len());
    for entry in record.config.history.iter().rev() {
        let empty = entry.empty_layer;
        let size = if empty {
            0
        } else {
            remaining.pop().unwrap_or(0)
        };
        items.push(ImageHistoryItem {
            id: record.id.full(),
            created: entry
                .created
                .as_ref()
                .map(|created| created.timestamp())
                .unwrap_or(0),
            created_by: entry.created_by.clone().unwrap_or_default(),
            tags: (!tags.is_empty()).then_some(tags.clone()),
            size,
            comment: entry.comment.clone().unwrap_or_default(),
            empty_layer: empty,
        });
    }
    items
}
