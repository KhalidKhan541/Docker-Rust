//! OCI distribution-spec client operations.
//!
//! [`RegistryClient`] wraps a [`Transport`] with the distribution protocol:
//! bearer/basic authentication (including token caching), manifest
//! GET/PUT with content negotiation, blob HEAD/GET/PUT (monolithic and
//! chunked with resume), tag listing with `Link: rel="next"` pagination, the
//! referrers API (with the spec-mandated fallback tag schema) and deletion.
//!
//! Blob bytes always move through [`docker_storage::BlobStore`]:
//! - pulls stream the response body into the store and verify the digest
//!   before the blob is committed;
//! - pushes stream the file from the store, so layers are never buffered
//!   whole in memory.
//!
//! Failure classification follows the distribution-spec error codes
//! (`BLOB_UNKNOWN`, `MANIFEST_UNKNOWN`, `DENIED`, `UNAUTHORIZED`,
//! `TOOMANYREQUESTS`, ...) and maps HTTP status onto
//! [`docker_errors::ErrorKind`].

use std::collections::{HashMap, HashSet};
use std::io::{Read, Seek, SeekFrom};
use std::sync::Mutex;
use std::time::{Duration, Instant};

use docker_common::{Digest, ImageRef};
use docker_errors::{Error, ErrorKind, Result};
use docker_image::{Index, Manifest};
use docker_storage::BlobStore;
use serde::Deserialize;

use crate::auth::{basic_authorization, parse_challenges, BearerChallenge, Challenge};
use crate::http::{Body, Method, Request, Response, Transport};
use crate::url::{encode_query_value, Url};
use crate::{
    DOCKER_MANIFEST_LIST_V2, DOCKER_MANIFEST_V1, DOCKER_MANIFEST_V1_JWS, DOCKER_MANIFEST_V2,
    MAX_TAG_PAGES, MEDIA_TYPE_IMAGE_INDEX, MEDIA_TYPE_IMAGE_MANIFEST, TOKEN_EXPIRY_MARGIN_SECS,
};

/// Manifests are capped at 16 MiB (the spec requires supporting at least
/// 4 MiB).
const MAX_MANIFEST_BYTES: usize = 16 * 1024 * 1024;
/// Error bodies are capped at 64 KiB.
const MAX_ERROR_BYTES: usize = 64 * 1024;
/// Tag reference length limit from the distribution grammar.
const MAX_TAG_LEN: usize = 128;
/// Chunked uploads are capped at 64 MiB per request. This is far above any
/// real `OCI-Chunk-Min-Length`, and prevents a hostile registry header from
/// forcing an unbounded allocation.
const MAX_CHUNK_SIZE: usize = 64 * 1024 * 1024;

/// Media types requested by default on manifest GET, most preferred first
/// (see `upstream/distribution-spec/content-negotiation.md`).
pub const DEFAULT_ACCEPT: &[&str] = &[
    MEDIA_TYPE_IMAGE_INDEX,
    MEDIA_TYPE_IMAGE_MANIFEST,
    DOCKER_MANIFEST_LIST_V2,
    DOCKER_MANIFEST_V2,
];

/// Result of `HEAD /v2/<name>/blobs/<digest>`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum BlobPresence {
    /// 404: the registry does not have the blob.
    Absent,
    /// 200: the blob exists. `size` is the `Content-Length` value when the
    /// registry provided one (the spec says registries SHOULD).
    Present { size: Option<u64> },
}

/// A manifest fetched from a registry, with its bytes, negotiated media type
/// and verified digest.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct FetchedManifest {
    pub media_type: String,
    pub digest: Digest,
    pub bytes: Vec<u8>,
}

/// One page of `GET /v2/<name>/tags/list`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TagsPage {
    pub name: String,
    pub tags: Vec<String>,
    /// Absolute URL from the `Link: <...>; rel="next"` header, if any.
    pub next: Option<Url>,
}

/// A parsed manifest document: index (manifest list) or image manifest.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ManifestDocument {
    Index(Box<Index>),
    Manifest(Box<Manifest>),
}

impl ManifestDocument {
    pub fn media_type(&self) -> &'static str {
        match self {
            ManifestDocument::Index(_) => MEDIA_TYPE_IMAGE_INDEX,
            ManifestDocument::Manifest(_) => MEDIA_TYPE_IMAGE_MANIFEST,
        }
    }
}

/// Parse a manifest body according to its media type.
///
/// `application/json` (or a missing type) is sniffed, matching the
/// content-negotiation table's legacy behavior; schema 1 is rejected as
/// `Unsupported` rather than mis-parsed.
pub fn parse_manifest_document(media_type: &str, bytes: &[u8]) -> Result<ManifestDocument> {
    let content_type = media_type
        .split(';')
        .next()
        .unwrap_or("")
        .trim()
        .to_ascii_lowercase();
    let invalid = |what: &str, e: serde_json::Error| {
        Error::invalid_argument(format!("invalid {what} document: {e}"))
    };
    match content_type.as_str() {
        MEDIA_TYPE_IMAGE_INDEX | DOCKER_MANIFEST_LIST_V2 => serde_json::from_slice(bytes)
            .map(|index| ManifestDocument::Index(Box::new(index)))
            .map_err(|e| invalid("image index", e)),
        MEDIA_TYPE_IMAGE_MANIFEST | DOCKER_MANIFEST_V2 => serde_json::from_slice(bytes)
            .map(|manifest| ManifestDocument::Manifest(Box::new(manifest)))
            .map_err(|e| invalid("image manifest", e)),
        DOCKER_MANIFEST_V1 | DOCKER_MANIFEST_V1_JWS => Err(Error::unsupported(format!(
            "schema 1 manifests (`{content_type}`) are not supported; ask the publisher to push an OCI or Docker schema 2 manifest"
        ))),
        "" | "application/json" => {
            let value: serde_json::Value = serde_json::from_slice(bytes)
                .map_err(|e| Error::invalid_argument(format!("manifest is not valid JSON: {e}")))?;
            if value.get("manifests").is_some() {
                serde_json::from_value(value)
                    .map(|index| ManifestDocument::Index(Box::new(index)))
                    .map_err(|e| invalid("image index", e))
            } else if value.get("config").is_some() || value.get("layers").is_some() {
                serde_json::from_value(value)
                    .map(|manifest| ManifestDocument::Manifest(Box::new(manifest)))
                    .map_err(|e| invalid("image manifest", e))
            } else {
                Err(Error::unsupported(
                    "cannot determine manifest media type from the response body",
                ))
            }
        }
        other => Err(Error::unsupported(format!(
            "unsupported manifest media type `{other}`"
        ))),
    }
}

#[derive(Debug)]
struct CachedToken {
    token: String,
    expires: Instant,
}

#[derive(Debug, Clone)]
enum Credential {
    Anonymous,
    Basic { username: String, password: String },
}

/// Upload session returned by `POST /v2/<name>/blobs/uploads/`.
struct UploadSession {
    location: Url,
    /// `OCI-Chunk-Min-Length`: smallest chunk (except the final one) the
    /// registry will accept for chunked uploads.
    min_chunk: Option<u64>,
}

/// Result of `GET <upload-location>` used to resume after a `416`.
struct UploadStatus {
    /// `Range: 0-<end>`: position of the last uploaded byte.
    last_offset: Option<u64>,
    /// The registry may rotate the upload URL; follow it when present.
    location: Option<Url>,
}

#[derive(Debug, Deserialize)]
struct TokenBody {
    token: Option<String>,
    access_token: Option<String>,
    expires_in: Option<u64>,
}

#[derive(Debug, Deserialize)]
struct TagsBody {
    name: String,
    tags: Option<Vec<String>>,
}

#[derive(Debug, Deserialize)]
struct ErrorEnvelope {
    errors: Vec<RegistryError>,
}

#[derive(Debug, Deserialize)]
struct RegistryError {
    code: String,
    message: Option<String>,
}

/// A repository-scoped client for one registry.
pub struct RegistryClient<T: Transport> {
    transport: T,
    base: Url,
    repository: String,
    credential: Credential,
    tokens: Mutex<HashMap<String, CachedToken>>,
    accept: Vec<String>,
}

impl<T: Transport> RegistryClient<T> {
    /// Create a client for `repository` on the registry at `base`.
    ///
    /// `base` must be an origin root (`https://registry.example.com/`).
    /// Registries mounted under a path prefix are rejected rather than
    /// silently addressed at the wrong path.
    pub fn new(transport: T, base: Url, repository: impl Into<String>) -> Result<Self> {
        let repository = repository.into();
        validate_repository(&repository)?;
        if base.path() != "/" {
            return Err(Error::unsupported(format!(
                "registry base URL `{base}` has a path prefix; path-mounted registries are not supported"
            )));
        }
        Ok(Self {
            transport,
            base,
            repository,
            credential: Credential::Anonymous,
            tokens: Mutex::new(HashMap::new()),
            accept: DEFAULT_ACCEPT.iter().map(|s| s.to_string()).collect(),
        })
    }

    /// Build a client from an image reference, mapping Docker Hub
    /// (`docker.io`, `index.docker.io`) to its API host
    /// `registry-1.docker.io` and using the canonical `library/` repository
    /// path.
    pub fn from_reference(transport: T, image: &ImageRef) -> Result<Self> {
        let host = image.registry().unwrap_or("docker.io");
        let api_host = match host.to_ascii_lowercase().as_str() {
            "docker.io" | "index.docker.io" | "registry.hub.docker.com" => {
                "registry-1.docker.io".to_string()
            }
            other => other.to_string(),
        };
        let base = Url::parse(&format!("https://{api_host}"))?;
        Self::new(transport, base, image.canonical_repo())
    }

    /// Use HTTP Basic credentials. They are sent directly to registries that
    /// only offer the `Basic` challenge, and to bearer token endpoints.
    pub fn with_basic_auth(
        mut self,
        username: impl Into<String>,
        password: impl Into<String>,
    ) -> Self {
        self.credential = Credential::Basic {
            username: username.into(),
            password: password.into(),
        };
        self
    }

    /// Override the manifest `Accept` list (most preferred first).
    pub fn with_accept(mut self, media_types: Vec<String>) -> Self {
        self.accept = media_types;
        self
    }

    pub fn repository(&self) -> &str {
        &self.repository
    }

    pub fn base_url(&self) -> &Url {
        &self.base
    }

    fn scope(&self, actions: &str) -> String {
        format!("repository:{}:{}", self.repository, actions)
    }

    fn api_path(&self, suffix: &str) -> String {
        format!("/v2/{}/{}", self.repository, suffix)
    }

    fn url_with_query(&self, path: &str, query: &str) -> Result<Url> {
        let url = self.base.join(path)?;
        Ok(if query.is_empty() {
            url
        } else {
            url.with_query(Some(query.to_string()))
        })
    }

    fn cached_token(&self, scope: &str) -> Option<String> {
        let tokens = self
            .tokens
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        tokens
            .get(scope)
            .filter(|cached| cached.expires > Instant::now())
            .map(|cached| cached.token.clone())
    }

    fn forget_token(&self, scope: &str) {
        let mut tokens = self
            .tokens
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        tokens.remove(scope);
    }

    /// Execute one registry request, handling a `401` by acquiring a bearer
    /// token (or retrying with Basic credentials) exactly once.
    ///
    /// Requests to another origin (offloaded upload/download locations) are
    /// sent without registry credentials: the spec allows `Location` to point
    /// at a third-party host, and tokens must not leak there.
    fn execute(&self, request: Request, scope_actions: &str) -> Result<Response> {
        if request.url.origin() != self.base.origin() {
            return self.transport.execute(&request);
        }
        let scope = self.scope(scope_actions);
        let mut request = request;
        if let Some(token) = self.cached_token(&scope) {
            request
                .headers
                .insert("authorization", format!("Bearer {token}"));
        }
        let response = self.transport.execute(&request)?;
        if response.status != 401 {
            return Ok(response);
        }
        let challenges = {
            let values = response.headers.get_all("www-authenticate");
            parse_challenges(&values)
        };
        for challenge in &challenges {
            match challenge {
                Challenge::Bearer(bearer) => {
                    self.forget_token(&scope);
                    let token = self.token_for(bearer, &scope)?;
                    request
                        .headers
                        .insert("authorization", format!("Bearer {token}"));
                    let retry = self.transport.execute(&request)?;
                    if retry.status != 401 {
                        return Ok(retry);
                    }
                }
                Challenge::Basic { .. } => {
                    if let Credential::Basic { username, password } = &self.credential {
                        request
                            .headers
                            .insert("authorization", basic_authorization(username, password));
                        let retry = self.transport.execute(&request)?;
                        if retry.status != 401 {
                            return Ok(retry);
                        }
                    }
                }
            }
        }
        Err(Error::unauthorized(format!(
            "authentication required for repository `{}` on `{}`{}",
            self.repository,
            self.base.origin(),
            if challenges.is_empty() {
                " (registry sent no WWW-Authenticate challenge)"
            } else {
                ""
            }
        )))
    }

    /// Fetch a bearer token for `needed_scope`. The challenge's own `scope`
    /// wins when present (that is the server's grant), otherwise the scope
    /// computed for the operation is requested.
    fn token_for(&self, challenge: &BearerChallenge, needed_scope: &str) -> Result<String> {
        if let Some(token) = self.cached_token(needed_scope) {
            return Ok(token);
        }
        let mut url = Url::parse(&challenge.realm).map_err(|e| {
            Error::invalid_argument(format!(
                "invalid token realm `{}`: {}",
                challenge.realm,
                e.message()
            ))
        })?;
        if self.base.is_secure() && !url.is_secure() {
            return Err(Error::unsupported(format!(
                "refusing to fetch a token over plaintext from `{url}` for an HTTPS registry"
            )));
        }
        let mut query = Vec::new();
        if let Some(service) = &challenge.service {
            query.push(format!("service={}", encode_query_value(service)));
        }
        query.push(format!(
            "scope={}",
            encode_query_value(challenge.scope.as_deref().unwrap_or(needed_scope))
        ));
        url = url.with_query(Some(query.join("&")));
        let mut request = Request::new(Method::Get, url);
        request.headers.insert("accept", "application/json");
        if let Credential::Basic { username, password } = &self.credential {
            request
                .headers
                .insert("authorization", basic_authorization(username, password));
        }
        let response = self.transport.execute(&request)?;
        if response.status != 200 {
            let status = response.status;
            let body = response.body_bytes_limited(MAX_ERROR_BYTES)?;
            return Err(status_error("token endpoint", status, &body));
        }
        let body = response.body_bytes_limited(MAX_ERROR_BYTES)?;
        let parsed: TokenBody = serde_json::from_slice(&body).map_err(|e| {
            Error::invalid_argument(format!("token endpoint returned invalid JSON: {e}"))
        })?;
        let token = parsed.token.or(parsed.access_token).ok_or_else(|| {
            Error::invalid_argument(
                "token endpoint response contains neither `token` nor `access_token`",
            )
        })?;
        let ttl = parsed
            .expires_in
            .unwrap_or(60)
            .saturating_sub(TOKEN_EXPIRY_MARGIN_SECS)
            .max(10);
        let expires = Instant::now() + Duration::from_secs(ttl);
        let mut tokens = self
            .tokens
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        tokens.insert(
            needed_scope.to_string(),
            CachedToken {
                token: token.clone(),
                expires,
            },
        );
        Ok(token)
    }

    /// Verify the registry's `/v2/` endpoint is reachable and authenticated.
    pub fn ping(&self) -> Result<()> {
        let url = self.base.join("/v2/")?;
        let response = self.execute(Request::new(Method::Get, url), "pull")?;
        if response.status == 200 {
            return Ok(());
        }
        let status = response.status;
        let body = response
            .body_bytes_limited(MAX_ERROR_BYTES)
            .unwrap_or_default();
        Err(status_error(
            &format!("ping for `{}`", self.repository),
            status,
            &body,
        ))
    }

    /// `GET /v2/<name>/manifests/<reference>` with content negotiation.
    ///
    /// The returned digest is computed from the body; a digest reference and
    /// the `Docker-Content-Digest` header are both verified.
    pub fn get_manifest(&self, reference: &str) -> Result<FetchedManifest> {
        validate_reference(reference)?;
        let url = self
            .base
            .join(&self.api_path(&format!("manifests/{reference}")))?;
        let mut request = Request::new(Method::Get, url);
        request.headers.insert("accept", self.accept.join(", "));
        let response = self.execute(request, "pull")?;
        if response.status != 200 {
            let status = response.status;
            let body = response
                .body_bytes_limited(MAX_ERROR_BYTES)
                .unwrap_or_default();
            return Err(status_error(
                &format!("manifest `{reference}`"),
                status,
                &body,
            ));
        }
        let content_type = response
            .headers
            .get("content-type")
            .unwrap_or("")
            .split(';')
            .next()
            .unwrap_or("")
            .trim()
            .to_ascii_lowercase();
        let header_digest = response
            .headers
            .get("docker-content-digest")
            .map(|value| Digest::parse(value.trim()));
        let bytes = response.body_bytes_limited(MAX_MANIFEST_BYTES)?;
        if content_type == DOCKER_MANIFEST_V1 || content_type == DOCKER_MANIFEST_V1_JWS {
            return Err(Error::unsupported(format!(
                "registry returned a schema 1 manifest (`{content_type}`) for `{reference}`; schema 1 is not supported"
            )));
        }
        let digest = Digest::sha256(&bytes);
        if let Some(header_digest) = header_digest {
            let header_digest = header_digest.map_err(|e| {
                Error::invalid_argument(format!(
                    "registry sent an invalid Docker-Content-Digest header: {}",
                    e.message()
                ))
            })?;
            if header_digest != digest {
                return Err(Error::invalid_argument(format!(
                    "registry digest mismatch for manifest `{reference}`: header {}, body {}",
                    header_digest.full(),
                    digest.full()
                )));
            }
        }
        if reference.contains(':') {
            let expected = Digest::parse(reference)?;
            if expected != digest {
                return Err(Error::invalid_argument(format!(
                    "manifest digest mismatch: requested `{reference}`, body hashes to {}",
                    digest.full()
                )));
            }
        }
        let media_type = if content_type.is_empty() || content_type == "application/json" {
            sniff_media_type(&bytes)?
        } else {
            content_type
        };
        Ok(FetchedManifest {
            media_type,
            digest,
            bytes,
        })
    }

    /// `PUT /v2/<name>/manifests/<reference>`.
    ///
    /// Returns the digest of the uploaded bytes; a `Docker-Content-Digest`
    /// response header must agree with it.
    pub fn put_manifest(&self, reference: &str, bytes: &[u8], media_type: &str) -> Result<Digest> {
        validate_reference(reference)?;
        if media_type.trim().is_empty() {
            return Err(Error::invalid_argument(
                "manifest media type must not be empty",
            ));
        }
        let base_type = media_type
            .split(';')
            .next()
            .unwrap_or("")
            .trim()
            .to_ascii_lowercase();
        if matches!(
            base_type.as_str(),
            MEDIA_TYPE_IMAGE_INDEX
                | MEDIA_TYPE_IMAGE_MANIFEST
                | DOCKER_MANIFEST_LIST_V2
                | DOCKER_MANIFEST_V2
        ) {
            let document = parse_manifest_document(&base_type, bytes)?;
            let has_subject = match &document {
                ManifestDocument::Index(index) => index.subject.is_some(),
                ManifestDocument::Manifest(manifest) => manifest.subject.is_some(),
            };
            if has_subject {
                return Err(Error::unsupported(
                    "pushing a manifest with a `subject` requires the referrers tag-schema fallback, which is not implemented",
                ));
            }
        }
        let digest = Digest::sha256(bytes);
        let url = self
            .base
            .join(&self.api_path(&format!("manifests/{reference}")))?;
        let mut request = Request::new(Method::Put, url);
        request.headers.insert("content-type", media_type);
        request.body = Body::Bytes(bytes.to_vec());
        let response = self.execute(request, "pull,push")?;
        if response.status != 201 {
            let status = response.status;
            let body = response
                .body_bytes_limited(MAX_ERROR_BYTES)
                .unwrap_or_default();
            return Err(status_error(
                &format!("manifest upload `{reference}`"),
                status,
                &body,
            ));
        }
        if let Some(header) = response.headers.get("docker-content-digest") {
            let header_digest = Digest::parse(header.trim()).map_err(|e| {
                Error::invalid_argument(format!(
                    "registry sent an invalid Docker-Content-Digest header: {}",
                    e.message()
                ))
            })?;
            if header_digest != digest {
                return Err(Error::internal(format!(
                    "registry stored manifest `{reference}` as {}, but the uploaded bytes hash to {}",
                    header_digest.full(),
                    digest.full()
                )));
            }
        }
        Ok(digest)
    }

    /// `HEAD /v2/<name>/blobs/<digest>`.
    pub fn head_blob(&self, digest: &Digest) -> Result<BlobPresence> {
        let url = self
            .base
            .join(&self.api_path(&format!("blobs/{}", digest.full())))?;
        let response = self.execute(Request::new(Method::Head, url), "pull")?;
        match response.status {
            200 => {
                let size = response
                    .headers
                    .get("content-length")
                    .and_then(|value| value.trim().parse::<u64>().ok());
                Ok(BlobPresence::Present { size })
            }
            404 => Ok(BlobPresence::Absent),
            status => {
                let body = response
                    .body_bytes_limited(MAX_ERROR_BYTES)
                    .unwrap_or_default();
                Err(status_error(
                    &format!("blob {}", digest.full()),
                    status,
                    &body,
                ))
            }
        }
    }

    /// Download a blob into `store`, verifying the digest (and the declared
    /// size when present) before the blob is committed.
    ///
    /// Returns the blob size. If the store already has the digest, no network
    /// request is made.
    pub fn pull_blob_to_store(&self, digest: &Digest, store: &BlobStore) -> Result<u64> {
        if store.exists(digest) {
            return store.stat(digest);
        }
        let url = self
            .base
            .join(&self.api_path(&format!("blobs/{}", digest.full())))?;
        let response = self.execute(Request::new(Method::Get, url), "pull")?;
        if response.status != 200 {
            let status = response.status;
            let body = response
                .body_bytes_limited(MAX_ERROR_BYTES)
                .unwrap_or_default();
            return Err(status_error(
                &format!("blob {}", digest.full()),
                status,
                &body,
            ));
        }
        let expected_size = response
            .headers
            .get("content-length")
            .and_then(|value| value.trim().parse::<u64>().ok());
        let mut body = response.body;
        let (stored, size) = store.put_stream(Some(digest), expected_size, &mut *body)?;
        debug_assert_eq!(stored, *digest);
        Ok(size)
    }

    /// Upload a blob from the store with the monolithic `POST` + `PUT` flow.
    ///
    /// Returns `true` when the blob was uploaded and `false` when the
    /// registry already had it (verified by `HEAD`).
    pub fn push_blob_from_store(&self, digest: &Digest, store: &BlobStore) -> Result<bool> {
        match self.head_blob(digest)? {
            BlobPresence::Present { size } => {
                if let Some(remote) = size {
                    let local = store.stat(digest)?;
                    if remote != local {
                        return Err(Error::internal(format!(
                            "registry reports blob {} with size {remote}, but the local blob is {local} bytes; content-addressed identity is inconsistent",
                            digest.full()
                        )));
                    }
                }
                return Ok(false);
            }
            BlobPresence::Absent => {}
        }
        let size = store.stat(digest)?;
        let session = self.start_upload()?;
        let url = with_digest_query(&session.location, digest);
        let mut request = Request::new(Method::Put, url);
        request
            .headers
            .insert("content-type", "application/octet-stream");
        request.body = Body::File {
            path: store.blob_path(digest),
            len: size,
        };
        let response = self.execute(request, "pull,push")?;
        if response.status != 201 {
            let status = response.status;
            let body = response
                .body_bytes_limited(MAX_ERROR_BYTES)
                .unwrap_or_default();
            return Err(status_error(
                &format!("blob upload {}", digest.full()),
                status,
                &body,
            ));
        }
        Ok(true)
    }

    /// Upload a blob in `chunk_size` pieces using `PATCH`, resuming once or
    /// twice after a `416 Range Not Satisfiable` by querying the upload
    /// offset (`GET <location>` → `204` + `Range` + `Location`).
    ///
    /// A registry-advertised `OCI-Chunk-Min-Length` raises the effective
    /// chunk size so every non-final chunk respects the minimum.
    pub fn push_blob_chunked(
        &self,
        digest: &Digest,
        store: &BlobStore,
        chunk_size: usize,
    ) -> Result<bool> {
        if chunk_size == 0 {
            return Err(Error::invalid_argument("chunk size must be positive"));
        }
        if chunk_size > MAX_CHUNK_SIZE {
            return Err(Error::invalid_argument(format!(
                "chunk size {chunk_size} exceeds the client ceiling of {MAX_CHUNK_SIZE} bytes"
            )));
        }
        match self.head_blob(digest)? {
            BlobPresence::Present { .. } => return Ok(false),
            BlobPresence::Absent => {}
        }
        let size = store.stat(digest)?;
        let session = self.start_upload()?;
        let mut location = session.location;
        let chunk_size = match session.min_chunk {
            None => chunk_size,
            Some(min) if min <= MAX_CHUNK_SIZE as u64 => chunk_size.max(min as usize),
            Some(min) => {
                return Err(Error::unsupported(format!(
                    "registry requires a minimum chunk size of {min} bytes, above the client ceiling of {MAX_CHUNK_SIZE} bytes"
                )))
            }
        };
        let mut file = std::fs::File::open(store.blob_path(digest))?;
        let mut offset: u64 = 0;
        let mut restarts = 0usize;
        let mut buffer = vec![0u8; chunk_size];
        while offset < size {
            let want = ((size - offset).min(chunk_size as u64)) as usize;
            file.read_exact(&mut buffer[..want]).map_err(|e| {
                Error::internal(format!(
                    "failed reading blob {} from the store: {e}",
                    digest.full()
                ))
            })?;
            let end = offset + want as u64 - 1;
            let mut request = Request::new(Method::Patch, location.clone());
            request
                .headers
                .insert("content-type", "application/octet-stream");
            request
                .headers
                .insert("content-range", format!("{offset}-{end}"));
            request.body = Body::Bytes(buffer[..want].to_vec());
            let response = self.execute(request, "pull,push")?;
            match response.status {
                202 => {
                    if let Some(next) = response.headers.get("location") {
                        location = self.base.join(next)?;
                    }
                    offset += want as u64;
                }
                416 => {
                    if restarts >= 2 {
                        let body = response
                            .body_bytes_limited(MAX_ERROR_BYTES)
                            .unwrap_or_default();
                        return Err(status_error("chunked blob upload", 416, &body));
                    }
                    restarts += 1;
                    let status = self.upload_status(&location)?;
                    if let Some(next_location) = status.location {
                        location = next_location;
                    }
                    let next = status
                        .last_offset
                        .map(|last| last.saturating_add(1))
                        .unwrap_or(0);
                    if next > size {
                        return Err(Error::invalid_argument(format!(
                            "registry reports upload offset {next} beyond blob size {size} for {}",
                            digest.full()
                        )));
                    }
                    file.seek(SeekFrom::Start(next))?;
                    offset = next;
                }
                status => {
                    let body = response
                        .body_bytes_limited(MAX_ERROR_BYTES)
                        .unwrap_or_default();
                    return Err(status_error("chunked blob upload", status, &body));
                }
            }
        }
        let url = with_digest_query(&location, digest);
        let mut request = Request::new(Method::Put, url);
        request.headers.insert("content-length", "0");
        let response = self.execute(request, "pull,push")?;
        if response.status != 201 {
            let status = response.status;
            let body = response
                .body_bytes_limited(MAX_ERROR_BYTES)
                .unwrap_or_default();
            return Err(status_error(
                &format!("chunked blob upload {}", digest.full()),
                status,
                &body,
            ));
        }
        Ok(true)
    }

    fn start_upload(&self) -> Result<UploadSession> {
        let url = self.base.join(&self.api_path("blobs/uploads/"))?;
        let mut request = Request::new(Method::Post, url);
        request.headers.insert("content-length", "0");
        let response = self.execute(request, "pull,push")?;
        if response.status != 202 {
            let status = response.status;
            let body = response
                .body_bytes_limited(MAX_ERROR_BYTES)
                .unwrap_or_default();
            return Err(status_error("blob upload session", status, &body));
        }
        let min_chunk = match response.headers.get("oci-chunk-min-length") {
            None => None,
            Some(value) => Some(value.trim().parse::<u64>().map_err(|_| {
                Error::invalid_argument(format!(
                    "registry sent an invalid OCI-Chunk-Min-Length header `{value}`"
                ))
            })?),
        };
        let location = response.headers.get("location").ok_or_else(|| {
            Error::invalid_argument(
                "registry accepted the upload session without a Location header",
            )
        })?;
        Ok(UploadSession {
            location: self.base.join(location)?,
            min_chunk,
        })
    }

    fn upload_status(&self, location: &Url) -> Result<UploadStatus> {
        let response = self.execute(Request::new(Method::Get, location.clone()), "pull,push")?;
        if response.status != 204 {
            let status = response.status;
            let body = response
                .body_bytes_limited(MAX_ERROR_BYTES)
                .unwrap_or_default();
            return Err(status_error("chunked blob upload status", status, &body));
        }
        let next_location = match response.headers.get("location") {
            None => None,
            Some(value) => Some(self.base.join(value)?),
        };
        let last_offset = match response.headers.get("range") {
            None => None,
            Some(range) => {
                let last = range
                    .rsplit('-')
                    .next()
                    .and_then(|value| value.trim().parse::<u64>().ok())
                    .ok_or_else(|| {
                        Error::invalid_argument(format!(
                            "registry sent an invalid upload Range header `{range}`"
                        ))
                    })?;
                Some(last)
            }
        };
        Ok(UploadStatus {
            last_offset,
            location: next_location,
        })
    }

    /// `GET /v2/<name>/tags/list` with optional page size and cursor.
    pub fn list_tags(&self, n: Option<u32>, last: Option<&str>) -> Result<TagsPage> {
        let mut query = Vec::new();
        if let Some(n) = n {
            query.push(format!("n={n}"));
        }
        if let Some(last) = last {
            query.push(format!("last={}", encode_query_value(last)));
        }
        let url = self.url_with_query(&self.api_path("tags/list"), &query.join("&"))?;
        self.fetch_tags_page(url)
    }

    /// Follow `Link: rel="next"` until the registry stops paginating.
    pub fn list_all_tags(&self, page_size: u32) -> Result<Vec<String>> {
        if page_size == 0 {
            return Err(Error::invalid_argument("page size must be positive"));
        }
        let mut url =
            self.url_with_query(&self.api_path("tags/list"), &format!("n={page_size}"))?;
        let mut seen = HashSet::new();
        let mut tags = Vec::new();
        for _ in 0..MAX_TAG_PAGES {
            let page = self.fetch_tags_page(url.clone())?;
            tags.extend(page.tags);
            match page.next {
                None => return Ok(tags),
                Some(next) => {
                    if !seen.insert(next.to_string()) {
                        return Err(Error::internal(
                            "registry tag pagination repeated a Link URL",
                        ));
                    }
                    url = next;
                }
            }
        }
        Err(Error::unavailable(format!(
            "registry tag listing exceeded {MAX_TAG_PAGES} pages"
        )))
    }

    fn fetch_tags_page(&self, url: Url) -> Result<TagsPage> {
        let mut request = Request::new(Method::Get, url);
        request.headers.insert("accept", "application/json");
        let response = self.execute(request, "pull")?;
        if response.status != 200 {
            let status = response.status;
            let body = response
                .body_bytes_limited(MAX_ERROR_BYTES)
                .unwrap_or_default();
            return Err(status_error("tag list", status, &body));
        }
        let link = response.headers.get("link").map(str::to_string);
        let body = response.body_bytes_limited(MAX_MANIFEST_BYTES)?;
        let parsed: TagsBody = serde_json::from_slice(&body)
            .map_err(|e| Error::invalid_argument(format!("invalid tag list response: {e}")))?;
        let next = match link.as_deref().and_then(parse_link_next) {
            Some(reference) => {
                let next = self.base.join(&reference)?;
                if next.origin() != self.base.origin() {
                    return Err(Error::unsupported(format!(
                        "tag pagination Link points at another origin `{}`; registry credentials are only sent to `{}`",
                        next.origin(),
                        self.base.origin()
                    )));
                }
                Some(next)
            }
            None => None,
        };
        Ok(TagsPage {
            name: parsed.name,
            tags: parsed.tags.unwrap_or_default(),
            next,
        })
    }

    /// `GET /v2/<name>/referrers/<digest>`.
    ///
    /// On `404` (registry without the referrers API) the spec's fallback tag
    /// schema is used: the referrers index is stored as manifest with tag
    /// `<algorithm>-<hex>`. A missing or unparsable fallback manifest means
    /// "no referrers". `artifact_type` is filtered client-side when the
    /// response does not advertise `OCI-Filters-Applied: artifactType`.
    pub fn referrers(&self, subject: &Digest, artifact_type: Option<&str>) -> Result<Index> {
        let path = self.api_path(&format!("referrers/{}", subject.full()));
        let url = match artifact_type {
            Some(value) => self.url_with_query(
                &path,
                &format!("artifactType={}", encode_query_value(value)),
            )?,
            None => self.base.join(&path)?,
        };
        let mut request = Request::new(Method::Get, url);
        request.headers.insert("accept", MEDIA_TYPE_IMAGE_INDEX);
        let response = self.execute(request, "pull")?;
        match response.status {
            200 => {
                let filters_applied = response
                    .headers
                    .get("oci-filters-applied")
                    .map(|value| {
                        value
                            .split(',')
                            .any(|part| part.trim().eq_ignore_ascii_case("artifactType"))
                    })
                    .unwrap_or(false);
                let body = response.body_bytes_limited(MAX_MANIFEST_BYTES)?;
                let mut index: Index = serde_json::from_slice(&body).map_err(|e| {
                    Error::invalid_argument(format!("invalid referrers response: {e}"))
                })?;
                if let Some(filter) = artifact_type {
                    if !filters_applied {
                        index.manifests.retain(|descriptor| {
                            descriptor.artifact_type.as_deref() == Some(filter)
                        });
                    }
                }
                Ok(index)
            }
            404 => {
                let tag = format!("{}-{}", subject.algor().as_str(), subject.hex());
                match self.get_manifest(&tag) {
                    Ok(fetched) => {
                        let mut index = serde_json::from_slice::<Index>(&fetched.bytes)
                            .unwrap_or_else(|_| Index::new());
                        if let Some(filter) = artifact_type {
                            index.manifests.retain(|descriptor| {
                                descriptor.artifact_type.as_deref() == Some(filter)
                            });
                        }
                        Ok(index)
                    }
                    // The spec: if the fallback tag does not resolve to a
                    // valid image index, clients SHOULD assume no referrers.
                    Err(e) if matches!(e.kind(), ErrorKind::NotFound | ErrorKind::Unsupported) => {
                        Ok(Index::new())
                    }
                    Err(e) => Err(e),
                }
            }
            status => {
                let body = response
                    .body_bytes_limited(MAX_ERROR_BYTES)
                    .unwrap_or_default();
                Err(status_error("referrers", status, &body))
            }
        }
    }

    /// `DELETE /v2/<name>/manifests/<reference>`.
    pub fn delete_manifest(&self, reference: &str) -> Result<()> {
        validate_reference(reference)?;
        let url = self
            .base
            .join(&self.api_path(&format!("manifests/{reference}")))?;
        self.delete(url, &format!("manifest `{reference}`"))
    }

    /// `DELETE /v2/<name>/blobs/<digest>`.
    pub fn delete_blob(&self, digest: &Digest) -> Result<()> {
        let url = self
            .base
            .join(&self.api_path(&format!("blobs/{}", digest.full())))?;
        self.delete(url, &format!("blob {}", digest.full()))
    }

    fn delete(&self, url: Url, what: &str) -> Result<()> {
        let response = self.execute(Request::new(Method::Delete, url), "pull,delete")?;
        match response.status {
            200 | 202 => Ok(()),
            404 => Err(Error::not_found(format!("registry does not have {what}"))),
            405 => Err(Error::unsupported(format!(
                "registry does not support deleting {what}"
            ))),
            status => {
                let body = response
                    .body_bytes_limited(MAX_ERROR_BYTES)
                    .unwrap_or_default();
                Err(status_error(what, status, &body))
            }
        }
    }
}

fn with_digest_query(location: &Url, digest: &Digest) -> Url {
    let addition = format!("digest={}", encode_query_value(&digest.full()));
    let query = match location.query() {
        Some(existing) if !existing.is_empty() => format!("{existing}&{addition}"),
        _ => addition,
    };
    location.with_query(Some(query))
}

fn sniff_media_type(bytes: &[u8]) -> Result<String> {
    let value: serde_json::Value = serde_json::from_slice(bytes)
        .map_err(|e| Error::invalid_argument(format!("manifest is not valid JSON: {e}")))?;
    if value.get("manifests").is_some() {
        Ok(MEDIA_TYPE_IMAGE_INDEX.to_string())
    } else if value.get("config").is_some() || value.get("layers").is_some() {
        Ok(MEDIA_TYPE_IMAGE_MANIFEST.to_string())
    } else {
        Err(Error::unsupported(
            "cannot determine manifest media type from the response body",
        ))
    }
}

fn status_error(context: &str, status: u16, body: &[u8]) -> Error {
    let kind = match status {
        400 | 413 | 416 => ErrorKind::InvalidArgument,
        401 => ErrorKind::Unauthorized,
        403 => ErrorKind::Forbidden,
        404 => ErrorKind::NotFound,
        405 => ErrorKind::Unsupported,
        409 => ErrorKind::Conflict,
        429 => ErrorKind::Throttle,
        500..=599 => ErrorKind::Unavailable,
        300..=399 => ErrorKind::Unavailable,
        _ => ErrorKind::Internal,
    };
    let mut message = format!("registry {context} failed: HTTP {status}");
    if let Some(detail) = registry_error_detail(body) {
        message.push_str(": ");
        message.push_str(&detail);
    }
    Error::of_kind(kind, message)
}

fn registry_error_detail(body: &[u8]) -> Option<String> {
    let envelope: ErrorEnvelope = serde_json::from_slice(body).ok()?;
    if envelope.errors.is_empty() {
        return None;
    }
    Some(
        envelope
            .errors
            .iter()
            .map(|error| match &error.message {
                Some(message) if !message.is_empty() => {
                    format!("{}: {}", error.code, message)
                }
                _ => error.code.clone(),
            })
            .collect::<Vec<_>>()
            .join("; "),
    )
}

/// Extract `<...>; rel="next"` from a `Link` header, resolving nothing here.
fn parse_link_next(value: &str) -> Option<String> {
    let mut links = Vec::new();
    let mut current = String::new();
    let mut in_angle = false;
    for ch in value.chars() {
        match ch {
            '<' => {
                in_angle = true;
                current.push(ch);
            }
            '>' => {
                in_angle = false;
                current.push(ch);
            }
            ',' if !in_angle => links.push(std::mem::take(&mut current)),
            _ => current.push(ch),
        }
    }
    if !current.trim().is_empty() {
        links.push(current);
    }
    for link in links {
        let link = link.trim();
        let Some(rest) = link.strip_prefix('<') else {
            continue;
        };
        let Some(end) = rest.find('>') else {
            continue;
        };
        let url = &rest[..end];
        let params = &rest[end + 1..];
        let is_next = params.split(';').any(|param| {
            let param = param.trim();
            let Some((name, value)) = param.split_once('=') else {
                return false;
            };
            name.trim().eq_ignore_ascii_case("rel")
                && value
                    .trim()
                    .trim_matches('"')
                    .split_whitespace()
                    .any(|relation| relation.eq_ignore_ascii_case("next"))
        });
        if is_next {
            return Some(url.to_string());
        }
    }
    None
}

fn validate_reference(reference: &str) -> Result<()> {
    if reference.is_empty() {
        return Err(Error::invalid_argument(
            "manifest reference must not be empty",
        ));
    }
    if reference.contains(':') {
        Digest::parse(reference)?;
        return Ok(());
    }
    let bytes = reference.as_bytes();
    if bytes.len() > MAX_TAG_LEN {
        return Err(Error::invalid_argument(format!(
            "invalid tag `{reference}`: exceeds {MAX_TAG_LEN} characters"
        )));
    }
    let first_ok = bytes[0].is_ascii_alphanumeric() || bytes[0] == b'_';
    let rest_ok = bytes[1..]
        .iter()
        .all(|b| b.is_ascii_alphanumeric() || matches!(b, b'.' | b'_' | b'-'));
    if !first_ok || !rest_ok {
        return Err(Error::invalid_argument(format!(
            "invalid manifest reference `{reference}`: expected a tag or a digest"
        )));
    }
    Ok(())
}

fn validate_repository(repository: &str) -> Result<()> {
    if repository.is_empty() {
        return Err(Error::invalid_argument("repository name must not be empty"));
    }
    for component in repository.split('/') {
        validate_repository_component(component, repository)?;
    }
    Ok(())
}

fn validate_repository_component(component: &str, repository: &str) -> Result<()> {
    let bytes = component.as_bytes();
    let invalid = |reason: &str| {
        Error::invalid_argument(format!(
            "invalid repository name `{repository}`: component `{component}` {reason}"
        ))
    };
    if bytes.is_empty() {
        return Err(invalid("is empty"));
    }
    if !(bytes[0].is_ascii_lowercase() || bytes[0].is_ascii_digit()) {
        return Err(invalid("must start with a lowercase letter or digit"));
    }
    let mut i = 1;
    while i < bytes.len() {
        let b = bytes[i];
        if b.is_ascii_lowercase() || b.is_ascii_digit() {
            i += 1;
            continue;
        }
        match b {
            b'.' | b'_' => {
                let mut next = i + 1;
                if b == b'_' && bytes.get(next) == Some(&b'_') {
                    next += 1; // `__` is allowed
                }
                match bytes.get(next) {
                    Some(byte) if byte.is_ascii_lowercase() || byte.is_ascii_digit() => {
                        i = next;
                    }
                    _ => return Err(invalid("has a separator without a following alphanumeric")),
                }
            }
            b'-' => {
                let mut next = i;
                while bytes.get(next) == Some(&b'-') {
                    next += 1;
                }
                match bytes.get(next) {
                    Some(byte) if byte.is_ascii_lowercase() || byte.is_ascii_digit() => {
                        i = next;
                    }
                    _ => return Err(invalid("has a separator without a following alphanumeric")),
                }
            }
            _ => return Err(invalid(
                "may only contain lowercase letters, digits, and `.`, `_`, `__`, `-` separators",
            )),
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::VecDeque;
    use std::sync::Arc;

    use crate::http::Headers;

    const D64: &str = "0123456789abcdef0123456789abcdef0123456789abcdef0123456789abcdef";

    /// Scripted transport: returns queued responses and records requests.
    type ScriptedResponse = (u16, Vec<(String, String)>, Vec<u8>);

    #[derive(Clone)]
    struct MockTransport {
        responses: Arc<Mutex<VecDeque<ScriptedResponse>>>,
        requests: Arc<Mutex<Vec<Request>>>,
    }

    impl MockTransport {
        fn new(responses: Vec<ScriptedResponse>) -> Self {
            Self {
                responses: Arc::new(Mutex::new(VecDeque::from(responses))),
                requests: Arc::new(Mutex::new(Vec::new())),
            }
        }
    }

    impl Transport for MockTransport {
        fn execute(&self, request: &Request) -> Result<Response> {
            self.requests
                .lock()
                .unwrap_or_else(|poisoned| poisoned.into_inner())
                .push(request.clone());
            let (status, headers, body) = self
                .responses
                .lock()
                .unwrap_or_else(|poisoned| poisoned.into_inner())
                .pop_front()
                .expect("scripted response");
            let mut response_headers = Headers::new();
            for (name, value) in headers {
                response_headers.append(name, value);
            }
            Ok(Response {
                status,
                headers: response_headers,
                body: Box::new(std::io::Cursor::new(body)),
            })
        }
    }

    #[test]
    fn rejects_subject_manifest_before_upload() {
        let transport = MockTransport::new(Vec::new());
        let observer = transport.clone();
        let client = RegistryClient::new(
            transport,
            Url::parse("https://registry.example.com").unwrap(),
            "a/b",
        )
        .unwrap();
        let digest = format!("sha256:{D64}");
        let bytes = serde_json::to_vec(&serde_json::json!({
            "schemaVersion": 2,
            "mediaType": MEDIA_TYPE_IMAGE_MANIFEST,
            "config": {
                "mediaType": "application/vnd.oci.image.config.v1+json",
                "digest": digest,
                "size": 2
            },
            "layers": [],
            "subject": {
                "mediaType": MEDIA_TYPE_IMAGE_MANIFEST,
                "digest": digest,
                "size": 2
            }
        }))
        .unwrap();
        let err = client
            .put_manifest("v1", &bytes, MEDIA_TYPE_IMAGE_MANIFEST)
            .unwrap_err();
        assert_eq!(err.kind(), ErrorKind::Unsupported);
        assert!(err.message().contains("subject"));
        assert!(
            observer
                .requests
                .lock()
                .unwrap_or_else(|poisoned| poisoned.into_inner())
                .is_empty(),
            "rejection must happen before any registry request"
        );
    }

    #[test]
    fn basic_challenge_retries_with_credentials() {
        let transport = MockTransport::new(vec![
            (
                401,
                vec![(
                    "WWW-Authenticate".to_string(),
                    r#"Basic realm="registry""#.to_string(),
                )],
                Vec::new(),
            ),
            (
                200,
                vec![("Content-Type".to_string(), "application/json".to_string())],
                br#"{"name":"a/b","tags":["v1"]}"#.to_vec(),
            ),
        ]);
        let observer = transport.clone();
        let client = RegistryClient::new(
            transport,
            Url::parse("https://registry.example.com").unwrap(),
            "a/b",
        )
        .unwrap()
        .with_basic_auth("user", "pass");
        let page = client.list_tags(None, None).unwrap();
        assert_eq!(page.tags, vec!["v1".to_string()]);
        let requests = observer
            .requests
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        assert_eq!(requests.len(), 2);
        assert_eq!(requests[0].headers.get("authorization"), None);
        assert_eq!(
            requests[1].headers.get("authorization"),
            Some(basic_authorization("user", "pass").as_str())
        );
    }

    #[test]
    fn accepts_valid_repository_names() {
        for name in [
            "alpine",
            "library/alpine",
            "team/sub/app",
            "a/b_c",
            "a/b__c",
            "a/b---c",
            "a/b.c-d_e",
            "0/1",
        ] {
            validate_repository(name).unwrap_or_else(|e| panic!("{name}: {e}"));
        }
    }

    #[test]
    fn rejects_invalid_repository_names() {
        for name in [
            "", "Upper", "a//b", "-a", "a/", "a/.b", "a/b.", "a/b__", "a/b_-c", "a b",
        ] {
            assert!(
                validate_repository(name).is_err(),
                "expected `{name}` to be rejected"
            );
        }
    }

    #[test]
    fn parses_link_next_headers() {
        assert_eq!(
            parse_link_next(r#"</v2/a/tags/list?n=10&last=z>; rel="next""#),
            Some("/v2/a/tags/list?n=10&last=z".to_string())
        );
        assert_eq!(
            parse_link_next(
                r#"<https://r.example/v2/a/tags/list?last=q>; rel="next", <https://r.example/v2/a/tags/list?last=a>; rel="prev""#
            ),
            Some("https://r.example/v2/a/tags/list?last=q".to_string())
        );
        assert_eq!(
            parse_link_next(r#"</x>; rel="prev next""#),
            Some("/x".to_string())
        );
        assert_eq!(parse_link_next(r#"</x>; rel="prev""#), None);
    }

    #[test]
    fn maps_http_status_to_error_kind() {
        let cases = [
            (400, ErrorKind::InvalidArgument),
            (401, ErrorKind::Unauthorized),
            (403, ErrorKind::Forbidden),
            (404, ErrorKind::NotFound),
            (405, ErrorKind::Unsupported),
            (409, ErrorKind::Conflict),
            (413, ErrorKind::InvalidArgument),
            (429, ErrorKind::Throttle),
            (500, ErrorKind::Unavailable),
            (503, ErrorKind::Unavailable),
            (418, ErrorKind::Internal),
        ];
        for (status, kind) in cases {
            assert_eq!(status_error("test", status, b"").kind(), kind);
        }
    }

    #[test]
    fn includes_registry_error_codes_in_messages() {
        let body = br#"{"errors":[{"code":"MANIFEST_UNKNOWN","message":"manifest unknown"},{"code":"DENIED"}]}"#;
        let error = status_error("manifest `v1`", 404, body);
        assert_eq!(error.kind(), ErrorKind::NotFound);
        assert!(error
            .message()
            .contains("MANIFEST_UNKNOWN: manifest unknown"));
        assert!(error.message().contains("DENIED"));
    }

    #[test]
    fn parses_oci_manifest_and_index() {
        let manifest = format!(
            r#"{{"schemaVersion":2,"mediaType":"{MEDIA_TYPE_IMAGE_MANIFEST}","config":{{"mediaType":"application/vnd.oci.image.config.v1+json","digest":"sha256:{D64}","size":2}},"layers":[]}}"#
        );
        let parsed =
            parse_manifest_document(MEDIA_TYPE_IMAGE_MANIFEST, manifest.as_bytes()).unwrap();
        assert!(matches!(parsed, ManifestDocument::Manifest(_)));

        let index = format!(
            r#"{{"schemaVersion":2,"mediaType":"{MEDIA_TYPE_IMAGE_INDEX}","manifests":[]}}"#
        );
        let parsed = parse_manifest_document(MEDIA_TYPE_IMAGE_INDEX, index.as_bytes()).unwrap();
        assert!(matches!(parsed, ManifestDocument::Index(_)));
    }

    #[test]
    fn sniffs_manifests_with_generic_content_type() {
        let index = br#"{"schemaVersion":2,"manifests":[]}"#;
        let parsed = parse_manifest_document("application/json", index).unwrap();
        assert!(matches!(parsed, ManifestDocument::Index(_)));

        let manifest = format!(
            r#"{{"schemaVersion":2,"config":{{"mediaType":"application/vnd.oci.image.config.v1+json","digest":"sha256:{D64}","size":2}},"layers":[]}}"#
        );
        let parsed = parse_manifest_document("", manifest.as_bytes()).unwrap();
        assert!(matches!(parsed, ManifestDocument::Manifest(_)));
    }

    #[test]
    fn rejects_schema1_as_unsupported() {
        let error = parse_manifest_document(DOCKER_MANIFEST_V1_JWS, b"{}").unwrap_err();
        assert_eq!(error.kind(), ErrorKind::Unsupported);
        assert!(error.message().contains("schema 1"));
    }

    #[test]
    fn rejects_unknown_manifest_media_type() {
        let error = parse_manifest_document("application/vnd.example.thing", b"{}").unwrap_err();
        assert_eq!(error.kind(), ErrorKind::Unsupported);
    }

    #[test]
    fn maps_docker_hub_references_to_registry_1() {
        let image = ImageRef::parse("alpine:3.20").unwrap();
        let client =
            RegistryClient::from_reference(crate::http::StdTransport::default(), &image).unwrap();
        assert_eq!(client.repository(), "library/alpine");
        assert_eq!(
            client.base_url().to_string(),
            "https://registry-1.docker.io/"
        );

        let image = ImageRef::parse("docker.io/library/alpine:3.20").unwrap();
        let client =
            RegistryClient::from_reference(crate::http::StdTransport::default(), &image).unwrap();
        assert_eq!(client.repository(), "library/alpine");
        assert_eq!(
            client.base_url().to_string(),
            "https://registry-1.docker.io/"
        );

        let image = ImageRef::parse("quay.io/team/app:tag").unwrap();
        let client =
            RegistryClient::from_reference(crate::http::StdTransport::default(), &image).unwrap();
        assert_eq!(client.repository(), "team/app");
        assert_eq!(client.base_url().to_string(), "https://quay.io/");
    }

    #[test]
    fn rejects_path_mounted_registry_bases() {
        let base = Url::parse("https://example.com/registry").unwrap();
        let error = RegistryClient::new(crate::http::StdTransport::default(), base, "a/b")
            .err()
            .expect("path-mounted registry must be rejected");
        assert_eq!(error.kind(), ErrorKind::Unsupported);
    }

    #[test]
    fn builds_upload_digest_query() {
        let location = Url::parse("https://r.example/v2/a/blobs/uploads/abc?_state=x").unwrap();
        let digest = Digest::parse(&format!("sha256:{D64}")).unwrap();
        let url = with_digest_query(&location, &digest);
        assert_eq!(
            url.query(),
            Some(format!("_state=x&digest=sha256:{D64}").as_str())
        );
    }
}
