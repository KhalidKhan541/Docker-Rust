//! Persistent local image store and registry pulls for docker-rs.
//!
//! [`ImageStore`] owns a JSON reference index plus a content-addressed
//! [`docker_storage::BlobStore`]. [`ImageStore::pull`] downloads a
//! multi-platform-aware image through `docker-distribution`; the API layer
//! only formats the result.

#![forbid(unsafe_code)]

pub mod archive;
pub mod pull;
pub mod reference;
pub mod store;

pub use archive::{LoadOutcome, LoadedImage};
pub use pull::{parse_registry_auth, PullResult, RegistryAuth};
pub use reference::{
    canonical_registry_repo, canonical_repo_digest, canonical_tag, canonical_tag_from_parts,
    familiar_reference, host_platform, parse_platform,
};
pub use store::{DeleteOutcome, ImageRecord, ImageStore};
