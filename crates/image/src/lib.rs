//! OCI image-spec v1.1.1 wire types, ported from the pinned upstream
//! `image-spec/specs-go/v1` package (commit recorded in the root README).
//!
//! Two casing regimes coexist in the OCI spec, and this crate pins both by
//! test rather than by naming taste (see `docs/OCI_COMPATIBILITY.md`):
//!
//! 1. descriptors/manifests/indexes use camelCase/tree keys
//!    (`mediaType`, `artifactType`, `schemaVersion`, ...);
//! 2. the in-image execution parameters use PascalCase keys
//!    (`Env`, `Entrypoint`, `ExposedPorts`, ...) — a quirk inherited from
//!    the Docker image format, verified verbatim from the Go JSON tags.
//!
//! Differences from upstream Go are minimal and documented: `digest.Digest`
//! maps to `docker-common::Digest` (whose custom serde emits the canonical
//! `algo:hex` string, exactly like `go-digest`), and `[]byte` blob-embedding
//! data is base64-encoded through `base64_bytes`, mirroring encoding/json.

mod config;
mod descriptor;
mod index;
mod manifest;
pub mod mediatype;

pub use config::{EmptyObject, History, Image, ImageConfig, RootFS};
pub use descriptor::{Descriptor, Platform};
pub use index::Index;
pub use manifest::{Manifest, SCHEMA_VERSION};
pub use mediatype::*;
