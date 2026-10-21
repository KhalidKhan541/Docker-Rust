//! OCI distribution-spec v1.1.1 client — a from-spec port of the registry
//! protocol (pinned text in `upstream/distribution-spec/spec.md`).
//!
//! Layers:
//!
//! - [`url`] — narrow absolute-URL handling (schemes, authority, relative
//!   `Location` resolution, query encoding);
//! - [`http`] — a blocking HTTP/1.1 client with chunked decoding, redirects
//!   (auth stripped on cross-origin hops) and TLS via the platform backend
//!   (`native-tls`: SChannel on Windows). The [`Transport`] trait lets tests
//!   and later daemon code substitute transports.
//! - [`auth`] — `WWW-Authenticate` parsing (Bearer/Basic) per RFC 7235.
//! - [`client`] — the registry operations: ping, manifest GET/PUT, blob
//!   HEAD/GET/PUT (monolithic and chunked), tag listing with `Link`
//!   pagination, referrers API plus the spec-mandated referrers-tag
//!   fallback, and delete.
//!
//! Content flows through [`docker_storage::BlobStore`] with digest
//! verification on both directions; layer bytes are streamed, never buffered
//! whole.
//!
//! Scope boundaries for this milestone (documented, not silent):
//! - HTTP/2, proxies, cookies and connection reuse are not implemented.
//! - Schema 1 manifests are rejected with `Unsupported`.
//! - Cross-repository blob mounting (end-11) is not wired into push yet.
//! - The referrers-tag-schema *writes* required when pushing manifests with
//!   a `subject` (spec §Pushing Manifests with Subject) are not implemented;
//!   pushing such manifests against registries without the referrers API is
//!   reported as `Unsupported` by the caller path rather than half-done.

pub mod auth;
pub mod client;
pub mod http;
pub mod url;

pub use client::{BlobPresence, FetchedManifest, ManifestDocument, RegistryClient, TagsPage};
pub use http::{Body, Headers, Method, Request, Response, StdTransport, Transport};
pub use url::{Scheme, Url};

/// OCI image index media type (from `docker-image`, which pins the value).
pub const MEDIA_TYPE_IMAGE_INDEX: &str = docker_image::MEDIA_TYPE_IMAGE_INDEX;
/// OCI image manifest media type.
pub const MEDIA_TYPE_IMAGE_MANIFEST: &str = docker_image::MEDIA_TYPE_IMAGE_MANIFEST;

/// Docker schema 2 manifest media type (still emitted by older registries).
pub const DOCKER_MANIFEST_V2: &str = "application/vnd.docker.distribution.manifest.v2+json";
/// Docker schema 2 manifest-list media type.
pub const DOCKER_MANIFEST_LIST_V2: &str =
    "application/vnd.docker.distribution.manifest.list.v2+json";
/// Docker schema 1 manifest media type (rejected: `Unsupported`).
pub const DOCKER_MANIFEST_V1: &str = "application/vnd.docker.distribution.manifest.v1+json";
/// Docker schema 1 signed manifest media type (rejected: `Unsupported`).
pub const DOCKER_MANIFEST_V1_JWS: &str =
    "application/vnd.docker.distribution.manifest.v1+prettyjws";

/// Bearer tokens are treated as expiring this many seconds before the
/// registry-declared deadline, to absorb clock skew and request latency.
pub(crate) const TOKEN_EXPIRY_MARGIN_SECS: u64 = 30;

/// Hard cap for tag-listing pages when following `Link: rel="next"`.
pub(crate) const MAX_TAG_PAGES: usize = 100;
