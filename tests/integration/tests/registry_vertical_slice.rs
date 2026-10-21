//! End-to-end OCI distribution-spec vertical slice against an in-process fake
//! registry over loopback HTTP.
//!
//! This exercises the Milestone C exit criteria without any Docker or Go
//! binary: a bearer challenge → token → retry flow, blob push (monolithic and
//! chunked with `416` resume) and digest-verified pull through `BlobStore`,
//! byte-identical round trips, manifest PUT/GET with content negotiation, tag
//! pagination through `Link: rel="next"`, the referrers API plus tag-schema
//! fallback, and classified failures.
//!
//! The fake registry is intentionally strict where the spec is strict:
//! `PATCH` offsets must match `Content-Range`, upload digests are recomputed,
//! manifest `Content-Type` must match the document's `mediaType`, and every
//! `/v2/` request requires the bearer token (so a missing token in any code
//! path fails loudly).

use std::collections::{BTreeMap, HashMap};
use std::io::{BufRead, BufReader, Read, Write};
use std::net::{Shutdown, SocketAddr, TcpListener, TcpStream};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex, MutexGuard};
use std::time::Duration;

use docker_common::Digest;
use docker_distribution::{BlobPresence, RegistryClient, StdTransport, Url};
use docker_errors::ErrorKind;
use docker_image::{
    Descriptor, Index, Manifest, MEDIA_TYPE_IMAGE_CONFIG, MEDIA_TYPE_IMAGE_INDEX,
    MEDIA_TYPE_IMAGE_LAYER, MEDIA_TYPE_IMAGE_MANIFEST,
};
use docker_storage::BlobStore;
use serde_json::json;
use tempfile::TempDir;

// ---------------------------------------------------------------------------
// Fake registry
// ---------------------------------------------------------------------------

struct StoredManifest {
    media_type: String,
    bytes: Vec<u8>,
    digest: String,
}

struct Upload {
    data: Vec<u8>,
    /// The first `PATCH` of every upload is rejected with `416` to exercise
    /// the resumable-upload status query and offset seek.
    reject_next_patch: bool,
}

struct State {
    self_addr: SocketAddr,
    cdn_addr: SocketAddr,
    blobs: HashMap<String, Vec<u8>>,
    manifests: HashMap<String, StoredManifest>,
    /// tag -> manifest digest
    tags: BTreeMap<String, String>,
    uploads: HashMap<String, Upload>,
    next_upload: u64,
    token_requests: Vec<String>,
    patches: usize,
    patch_targets: Vec<String>,
    status_queries: usize,
    redirect_blob: Option<String>,
    tamper_blob: Option<String>,
    cdn_authorization_present: Option<bool>,
    /// `OCI-Chunk-Min-Length` advertised by upload sessions.
    min_chunk: Option<u64>,
    /// subject digest -> referrers index JSON
    referrers: HashMap<String, String>,
    allow_delete: bool,
}

impl State {
    fn new(self_addr: SocketAddr, cdn_addr: SocketAddr) -> Self {
        Self {
            self_addr,
            cdn_addr,
            blobs: HashMap::new(),
            manifests: HashMap::new(),
            tags: BTreeMap::new(),
            uploads: HashMap::new(),
            next_upload: 0,
            token_requests: Vec::new(),
            patches: 0,
            patch_targets: Vec::new(),
            status_queries: 0,
            redirect_blob: None,
            tamper_blob: None,
            cdn_authorization_present: None,
            min_chunk: None,
            referrers: HashMap::new(),
            allow_delete: false,
        }
    }
}

struct FakeRegistry {
    addr: SocketAddr,
    state: Arc<Mutex<State>>,
    shutdown: Arc<AtomicBool>,
    threads: Vec<std::thread::JoinHandle<()>>,
}

impl FakeRegistry {
    fn start() -> Self {
        let registry_listener = TcpListener::bind("127.0.0.1:0").expect("bind registry");
        let addr = registry_listener.local_addr().expect("registry addr");
        let cdn_listener = TcpListener::bind("127.0.0.1:0").expect("bind cdn");
        let cdn_addr = cdn_listener.local_addr().expect("cdn addr");
        for listener in [&registry_listener, &cdn_listener] {
            listener
                .set_nonblocking(true)
                .expect("nonblocking listener");
        }
        let state = Arc::new(Mutex::new(State::new(addr, cdn_addr)));
        let shutdown = Arc::new(AtomicBool::new(false));
        let threads = vec![
            spawn_server(
                registry_listener,
                state.clone(),
                shutdown.clone(),
                handle_registry,
            ),
            spawn_server(cdn_listener, state.clone(), shutdown.clone(), handle_cdn),
        ];
        Self {
            addr,
            state,
            shutdown,
            threads,
        }
    }

    fn base_url(&self) -> String {
        format!("http://{}", self.addr)
    }

    fn client(&self, repository: &str) -> RegistryClient<StdTransport> {
        let transport =
            StdTransport::default().with_timeouts(Duration::from_secs(5), Duration::from_secs(10));
        RegistryClient::new(
            transport,
            Url::parse(&self.base_url()).expect("base url"),
            repository,
        )
        .expect("client")
    }

    fn state(&self) -> MutexGuard<'_, State> {
        self.state
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
    }
}

impl Drop for FakeRegistry {
    fn drop(&mut self) {
        self.shutdown.store(true, Ordering::SeqCst);
        // Wake both accept loops.
        let _ = TcpStream::connect(self.addr);
        let cdn_addr = self
            .state
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .cdn_addr;
        let _ = TcpStream::connect(cdn_addr);
        for thread in self.threads.drain(..) {
            let _ = thread.join();
        }
    }
}

fn spawn_server(
    listener: TcpListener,
    state: Arc<Mutex<State>>,
    shutdown: Arc<AtomicBool>,
    handler: fn(&FakeRequest, &mut State) -> Reply,
) -> std::thread::JoinHandle<()> {
    std::thread::spawn(move || {
        while !shutdown.load(Ordering::SeqCst) {
            match listener.accept() {
                Ok((mut stream, _)) => {
                    let _ = stream.set_nonblocking(false);
                    let _ = stream.set_read_timeout(Some(Duration::from_secs(5)));
                    let _ = stream.set_write_timeout(Some(Duration::from_secs(5)));
                    let Some(request) = read_request(&mut stream) else {
                        continue;
                    };
                    let head_only = request.method == "HEAD";
                    let reply = {
                        let mut guard = state
                            .lock()
                            .unwrap_or_else(|poisoned| poisoned.into_inner());
                        handler(&request, &mut guard)
                    };
                    write_reply(stream, reply, head_only);
                }
                Err(error) if error.kind() == std::io::ErrorKind::WouldBlock => {
                    std::thread::sleep(Duration::from_millis(5));
                }
                Err(_) => break,
            }
        }
    })
}

struct FakeRequest {
    method: String,
    target: String,
    headers: Vec<(String, String)>,
    body: Vec<u8>,
}

impl FakeRequest {
    fn header(&self, name: &str) -> Option<&str> {
        self.headers
            .iter()
            .find(|(header, _)| header == name)
            .map(|(_, value)| value.as_str())
    }

    fn path(&self) -> &str {
        self.target.split('?').next().unwrap_or("")
    }

    fn query_param(&self, name: &str) -> Option<&str> {
        self.target.split_once('?')?.1.split('&').find_map(|pair| {
            let (key, value) = pair.split_once('=')?;
            if key == name {
                Some(value)
            } else {
                None
            }
        })
    }
}

fn read_request(stream: &mut TcpStream) -> Option<FakeRequest> {
    let mut reader = BufReader::new(stream.try_clone().ok()?);
    let mut request_line = String::new();
    if reader.read_line(&mut request_line).ok()? == 0 {
        return None;
    }
    let mut parts = request_line.split_whitespace();
    let method = parts.next()?.to_string();
    let target = parts.next()?.to_string();
    let mut headers = Vec::new();
    loop {
        let mut line = String::new();
        if reader.read_line(&mut line).ok()? == 0 {
            break;
        }
        let line = line.trim_end_matches(['\r', '\n']);
        if line.is_empty() {
            break;
        }
        if let Some((name, value)) = line.split_once(':') {
            headers.push((name.trim().to_ascii_lowercase(), value.trim().to_string()));
        }
    }
    let length = headers
        .iter()
        .find(|(name, _)| name == "content-length")
        .and_then(|(_, value)| value.parse::<usize>().ok())
        .unwrap_or(0);
    let mut body = vec![0u8; length];
    if length > 0 {
        reader.read_exact(&mut body).ok()?;
    }
    Some(FakeRequest {
        method,
        target,
        headers,
        body,
    })
}

struct Reply {
    status: u16,
    headers: Vec<(String, String)>,
    body: Vec<u8>,
}

impl Reply {
    fn empty(status: u16) -> Self {
        Self {
            status,
            headers: Vec::new(),
            body: Vec::new(),
        }
    }

    fn bytes(status: u16, body: Vec<u8>) -> Self {
        Self {
            status,
            headers: Vec::new(),
            body,
        }
    }

    fn json(status: u16, body: Vec<u8>) -> Self {
        Self::bytes(status, body).with_header("content-type", "application/json")
    }

    fn error(status: u16, code: &str, message: &str) -> Self {
        let body = serde_json::to_vec(&json!({
            "errors": [{ "code": code, "message": message }]
        }))
        .expect("serialize error body");
        Self::json(status, body)
    }

    fn with_header(mut self, name: &str, value: impl Into<String>) -> Self {
        self.headers.push((name.to_string(), value.into()));
        self
    }

    fn with_optional_chunk_min(self, min: Option<u64>) -> Self {
        match min {
            Some(min) => self.with_header("OCI-Chunk-Min-Length", min.to_string()),
            None => self,
        }
    }
}

fn write_reply(mut stream: TcpStream, reply: Reply, head_only: bool) {
    let mut head = format!(
        "HTTP/1.1 {} {}\r\nContent-Length: {}\r\n",
        reply.status,
        reason(reply.status),
        reply.body.len()
    );
    for (name, value) in &reply.headers {
        head.push_str(name);
        head.push_str(": ");
        head.push_str(value);
        head.push_str("\r\n");
    }
    head.push_str("Connection: close\r\n\r\n");
    let _ = stream.write_all(head.as_bytes());
    if !head_only {
        let _ = stream.write_all(&reply.body);
    }
    let _ = stream.flush();
    let _ = stream.shutdown(Shutdown::Both);
}

fn reason(status: u16) -> &'static str {
    match status {
        200 => "OK",
        201 => "Created",
        202 => "Accepted",
        204 => "No Content",
        307 => "Temporary Redirect",
        400 => "Bad Request",
        401 => "Unauthorized",
        403 => "Forbidden",
        404 => "Not Found",
        405 => "Method Not Allowed",
        416 => "Range Not Satisfiable",
        _ => "Status",
    }
}

fn empty_index() -> String {
    r#"{"schemaVersion":2,"mediaType":"application/vnd.oci.image.index.v1+json","manifests":[]}"#
        .to_string()
}

fn challenge(state: &State) -> Reply {
    Reply::error(401, "UNAUTHORIZED", "authentication required").with_header(
        "WWW-Authenticate",
        format!(
            "Bearer realm=\"http://{}/token\",service=\"fake-registry\"",
            state.self_addr
        ),
    )
}

fn handle_registry(request: &FakeRequest, state: &mut State) -> Reply {
    let path = request.path();
    if path == "/token" {
        let service = request.query_param("service").unwrap_or("").to_string();
        let scope = request.query_param("scope").unwrap_or("").to_string();
        state.token_requests.push(format!("{service}|{scope}"));
        return Reply::json(200, br#"{"token":"testtoken","expires_in":300}"#.to_vec());
    }
    let Some(tail_full) = path.strip_prefix("/v2/") else {
        return Reply::error(404, "NOT_FOUND", "unsupported endpoint");
    };
    let authorized = request.header("authorization") == Some("Bearer testtoken");

    if tail_full.is_empty() {
        if !authorized {
            return challenge(state);
        }
        return Reply::json(200, b"{}".to_vec());
    }

    let mut segments = tail_full.splitn(3, '/');
    let org = segments.next().unwrap_or("");
    let project = segments.next().unwrap_or("");
    let tail = segments.next().unwrap_or("");
    let repository = format!("{org}/{project}");

    if repository == "test/unauthorized" {
        return challenge(state);
    }
    if !authorized {
        return challenge(state);
    }
    if repository == "test/forbidden" {
        return Reply::error(403, "DENIED", "requested access to the resource is denied");
    }

    if tail == "tags/list" {
        return handle_tags(request, state, &repository);
    }
    if let Some(subject) = tail.strip_prefix("referrers/") {
        if repository == "test/fallback" {
            return Reply::error(404, "NOT_FOUND", "referrers API not supported");
        }
        let body = state
            .referrers
            .get(subject)
            .cloned()
            .unwrap_or_else(empty_index);
        return Reply::json(200, body.into_bytes());
    }
    if tail == "blobs/uploads/" && request.method == "POST" {
        let id = state.next_upload.to_string();
        state.next_upload += 1;
        state.uploads.insert(
            id.clone(),
            Upload {
                data: Vec::new(),
                reject_next_patch: true,
            },
        );
        return Reply::empty(202)
            .with_header(
                "Location",
                format!("http://{}{}{}?_state=upload", state.self_addr, path, id),
            )
            .with_header("Docker-Upload-UUID", id)
            .with_header("Range", "0-0")
            .with_optional_chunk_min(state.min_chunk);
    }
    if let Some(upload_id) = tail.strip_prefix("blobs/uploads/") {
        return handle_upload(request, state, upload_id, path);
    }
    if let Some(digest) = tail.strip_prefix("blobs/") {
        return handle_blob(request, state, digest);
    }
    if let Some(reference) = tail.strip_prefix("manifests/") {
        return handle_manifest(request, state, &repository, reference);
    }
    Reply::error(404, "NOT_FOUND", "unsupported endpoint")
}

fn handle_tags(request: &FakeRequest, state: &State, repository: &str) -> Reply {
    let names: Vec<String> = state.tags.keys().cloned().collect();
    let start = match request.query_param("last") {
        Some(last) => names
            .iter()
            .position(|tag| tag == last)
            .map(|index| index + 1)
            .unwrap_or(0),
        None => 0,
    };
    let page_size = request
        .query_param("n")
        .and_then(|value| value.parse::<usize>().ok());
    let end = page_size
        .map(|size| (start + size).min(names.len()))
        .unwrap_or(names.len());
    let page = &names[start..end];
    let mut reply = Reply::json(
        200,
        serde_json::to_vec(&json!({ "name": repository, "tags": page })).expect("serialize tags"),
    );
    if let Some(size) = page_size {
        if end < names.len() {
            if let Some(last) = page.last() {
                reply = reply.with_header(
                    "Link",
                    format!("</v2/{repository}/tags/list?n={size}&last={last}>; rel=\"next\""),
                );
            }
        }
    }
    reply
}

fn handle_upload(request: &FakeRequest, state: &mut State, id: &str, path: &str) -> Reply {
    match request.method.as_str() {
        "PATCH" => {
            state.patches += 1;
            state.patch_targets.push(request.target.clone());
            let Some(upload) = state.uploads.get_mut(id) else {
                return Reply::error(404, "BLOB_UPLOAD_UNKNOWN", "blob upload unknown");
            };
            if upload.reject_next_patch {
                upload.reject_next_patch = false;
                return Reply::error(416, "BLOB_UPLOAD_INVALID", "range not satisfiable");
            }
            let start = request
                .header("content-range")
                .and_then(|value| value.split('-').next())
                .and_then(|value| value.parse::<usize>().ok());
            if start != Some(upload.data.len()) {
                return Reply::error(416, "BLOB_UPLOAD_INVALID", "range not satisfiable");
            }
            upload.data.extend_from_slice(&request.body);
            let end = upload.data.len().saturating_sub(1);
            Reply::empty(202)
                .with_header(
                    "Location",
                    format!("http://{}{}?_state=upload", state.self_addr, path),
                )
                .with_header("Range", format!("0-{end}"))
        }
        "GET" => {
            state.status_queries += 1;
            match state.uploads.get(id) {
                Some(upload) => {
                    let mut reply = Reply::empty(204).with_header(
                        "Location",
                        format!("http://{}{}?_state=resumed", state.self_addr, path),
                    );
                    if !upload.data.is_empty() {
                        reply = reply.with_header("Range", format!("0-{}", upload.data.len() - 1));
                    }
                    reply
                }
                None => Reply::error(404, "BLOB_UPLOAD_UNKNOWN", "blob upload unknown"),
            }
        }
        "PUT" => {
            let Some(mut upload) = state.uploads.remove(id) else {
                return Reply::error(404, "BLOB_UPLOAD_UNKNOWN", "blob upload unknown");
            };
            // Monolithic uploads carry the whole blob on this PUT; chunked
            // uploads have already accumulated it through PATCH.
            upload.data.extend_from_slice(&request.body);
            let expected = request.query_param("digest").unwrap_or("").to_string();
            let actual = Digest::sha256(&upload.data).full();
            if actual != expected {
                return Reply::error(
                    400,
                    "DIGEST_INVALID",
                    "provided digest did not match uploaded content",
                );
            }
            state.blobs.insert(actual.clone(), upload.data);
            Reply::empty(201)
                .with_header(
                    "Location",
                    format!("http://{}/v2/uploaded/blobs/{actual}", state.self_addr),
                )
                .with_header("Docker-Content-Digest", actual)
        }
        _ => Reply::error(405, "UNSUPPORTED", "method not allowed"),
    }
}

fn handle_blob(request: &FakeRequest, state: &State, digest: &str) -> Reply {
    if request.method != "GET" && request.method != "HEAD" {
        return Reply::error(405, "UNSUPPORTED", "method not allowed");
    }
    if state.redirect_blob.as_deref() == Some(digest) {
        return Reply::empty(307).with_header(
            "Location",
            format!("http://{}/cdn/{digest}", state.cdn_addr),
        );
    }
    if state.tamper_blob.as_deref() == Some(digest) {
        return Reply::bytes(200, b"tampered-content".to_vec());
    }
    match state.blobs.get(digest) {
        Some(bytes) => Reply::bytes(200, bytes.clone()),
        None => Reply::error(404, "BLOB_UNKNOWN", "blob unknown to registry"),
    }
}

fn manifest_key(state: &State, reference: &str) -> Option<String> {
    if reference.contains(':') {
        Some(reference.to_string())
    } else {
        state.tags.get(reference).cloned()
    }
}

fn handle_manifest(
    request: &FakeRequest,
    state: &mut State,
    repository: &str,
    reference: &str,
) -> Reply {
    if repository == "test/schema1" && request.method == "GET" {
        return Reply::bytes(200, b"{}".to_vec()).with_header(
            "content-type",
            "application/vnd.docker.distribution.manifest.v1+prettyjws",
        );
    }
    match request.method.as_str() {
        "PUT" => {
            let digest = Digest::sha256(&request.body);
            let media_type = request
                .header("content-type")
                .unwrap_or("application/octet-stream")
                .to_string();
            if let Ok(document) = serde_json::from_slice::<serde_json::Value>(&request.body) {
                if let Some(declared) = document.get("mediaType").and_then(|value| value.as_str()) {
                    if declared != media_type {
                        return Reply::error(
                            400,
                            "MANIFEST_INVALID",
                            "manifest mediaType does not match Content-Type",
                        );
                    }
                }
            }
            state.manifests.insert(
                digest.full(),
                StoredManifest {
                    media_type,
                    bytes: request.body.clone(),
                    digest: digest.full(),
                },
            );
            if !reference.contains(':') {
                state.tags.insert(reference.to_string(), digest.full());
            }
            Reply::empty(201)
                .with_header(
                    "Location",
                    format!(
                        "http://{}/v2/{repository}/manifests/{}",
                        state.self_addr,
                        digest.full()
                    ),
                )
                .with_header("Docker-Content-Digest", digest.full())
        }
        "GET" => {
            let Some(key) = manifest_key(state, reference) else {
                return Reply::error(404, "MANIFEST_UNKNOWN", "manifest unknown");
            };
            match state.manifests.get(&key) {
                Some(stored) => Reply::bytes(200, stored.bytes.clone())
                    .with_header("content-type", stored.media_type.clone())
                    .with_header("Docker-Content-Digest", stored.digest.clone()),
                None => Reply::error(404, "MANIFEST_UNKNOWN", "manifest unknown"),
            }
        }
        "DELETE" => {
            if !state.allow_delete {
                return Reply::error(405, "UNSUPPORTED", "deletion is disabled");
            }
            let Some(key) = manifest_key(state, reference) else {
                return Reply::error(404, "MANIFEST_UNKNOWN", "manifest unknown");
            };
            if !reference.contains(':') {
                state.tags.remove(reference);
            }
            match state.manifests.remove(&key) {
                Some(_) => Reply::empty(202),
                None => Reply::error(404, "MANIFEST_UNKNOWN", "manifest unknown"),
            }
        }
        _ => Reply::error(405, "UNSUPPORTED", "method not allowed"),
    }
}

fn handle_cdn(request: &FakeRequest, state: &mut State) -> Reply {
    state.cdn_authorization_present = Some(request.header("authorization").is_some());
    if let Some(digest) = request.path().strip_prefix("/cdn/") {
        if let Some(bytes) = state.blobs.get(digest) {
            return Reply::bytes(200, bytes.clone());
        }
    }
    Reply::error(404, "BLOB_UNKNOWN", "blob unknown to registry")
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

fn store(directory: &TempDir, name: &str) -> BlobStore {
    BlobStore::open(directory.path().join(name)).expect("open blob store")
}

fn now_manifest(config: &Descriptor, layers: Vec<Descriptor>) -> Vec<u8> {
    let manifest = Manifest::new(config.clone(), layers);
    manifest.validate().expect("valid manifest");
    serde_json::to_vec(&manifest).expect("serialize manifest")
}

fn descriptor_for(bytes: &[u8], media_type: &str) -> Descriptor {
    Descriptor {
        media_type: media_type.to_string(),
        digest: Digest::sha256(bytes),
        size: bytes.len() as i64,
        ..Default::default()
    }
}

fn artifact_descriptor(artifact_type: &str) -> Descriptor {
    Descriptor {
        media_type: MEDIA_TYPE_IMAGE_MANIFEST.to_string(),
        digest: Digest::sha256(artifact_type.as_bytes()),
        size: 2,
        artifact_type: Some(artifact_type.to_string()),
        ..Default::default()
    }
}

#[test]
fn push_pull_round_trip_is_byte_identical() {
    let registry = FakeRegistry::start();
    let client = registry.client("test/repo");
    client.ping().expect("ping");
    client.ping().expect("ping with cached token");
    {
        let state = registry.state();
        assert_eq!(
            state.token_requests.len(),
            1,
            "token should be reused: {:?}",
            state.token_requests
        );
        assert!(state.token_requests[0].contains("repository:test/repo:pull"));
    }

    let directory = TempDir::new().expect("tempdir");
    let source = store(&directory, "source");
    let target = store(&directory, "target");

    let config =
        br#"{"architecture":"amd64","os":"linux","rootfs":{"type":"layers","diff_ids":[]}}"#
            .to_vec();
    let layer: Vec<u8> = (0..100_000u32).map(|index| (index % 251) as u8).collect();
    let (config_digest, config_size) = source.put(&config).expect("store config");
    let (layer_digest, layer_size) = source.put(&layer).expect("store layer");

    let config_descriptor = descriptor_for(&config, MEDIA_TYPE_IMAGE_CONFIG);
    let layer_descriptor = descriptor_for(&layer, MEDIA_TYPE_IMAGE_LAYER);
    let manifest_bytes = now_manifest(&config_descriptor, vec![layer_descriptor]);
    let manifest_digest = Digest::sha256(&manifest_bytes);

    assert!(client
        .push_blob_from_store(&layer_digest, &source)
        .expect("push layer"));
    assert!(
        !client
            .push_blob_from_store(&layer_digest, &source)
            .expect("second push is a no-op"),
        "an existing blob must not be uploaded twice"
    );
    assert!(client
        .push_blob_from_store(&config_digest, &source)
        .expect("push config"));
    assert_eq!(
        client
            .put_manifest("v1", &manifest_bytes, MEDIA_TYPE_IMAGE_MANIFEST)
            .expect("put manifest"),
        manifest_digest
    );

    let fetched = client.get_manifest("v1").expect("get manifest");
    assert_eq!(fetched.media_type, MEDIA_TYPE_IMAGE_MANIFEST);
    assert_eq!(fetched.digest, manifest_digest);
    assert_eq!(fetched.bytes, manifest_bytes);
    let by_digest = client
        .get_manifest(&manifest_digest.full())
        .expect("get manifest by digest");
    assert_eq!(by_digest.digest, manifest_digest);

    assert_eq!(
        client.head_blob(&layer_digest).expect("head layer"),
        BlobPresence::Present {
            size: Some(layer_size)
        }
    );
    assert_eq!(
        client
            .head_blob(&Digest::sha256(b"absent"))
            .expect("head absent"),
        BlobPresence::Absent
    );

    assert_eq!(
        client
            .pull_blob_to_store(&layer_digest, &target)
            .expect("pull layer"),
        layer_size
    );
    assert_eq!(
        client
            .pull_blob_to_store(&config_digest, &target)
            .expect("pull config"),
        config_size
    );
    assert_eq!(target.get_verified(&layer_digest).unwrap(), layer);
    assert_eq!(target.get_verified(&config_digest).unwrap(), config);

    let page = client.list_tags(Some(10), None).expect("list tags");
    assert_eq!(page.name, "test/repo");
    assert_eq!(page.tags, vec!["v1".to_string()]);
    assert!(page.next.is_none());

    let error = client.get_manifest("missing").unwrap_err();
    assert_eq!(error.kind(), ErrorKind::NotFound);
    assert!(
        error.message().contains("MANIFEST_UNKNOWN"),
        "{}",
        error.message()
    );

    let error = client
        .pull_blob_to_store(&Digest::sha256(b"missing-blob"), &target)
        .unwrap_err();
    assert_eq!(error.kind(), ErrorKind::NotFound);
    assert!(
        error.message().contains("BLOB_UNKNOWN"),
        "{}",
        error.message()
    );

    let error = client.delete_manifest("v1").unwrap_err();
    assert_eq!(error.kind(), ErrorKind::Unsupported);

    let schema1 = registry.client("test/schema1");
    let error = schema1.get_manifest("v1").unwrap_err();
    assert_eq!(error.kind(), ErrorKind::Unsupported);
}

#[test]
fn cross_origin_blob_redirect_strips_authorization() {
    let registry = FakeRegistry::start();
    let client = registry.client("test/repo");
    let directory = TempDir::new().expect("tempdir");
    let target = store(&directory, "target");
    let content = b"redirected-blob-content".to_vec();
    let digest = Digest::sha256(&content);
    {
        let mut state = registry.state();
        state.blobs.insert(digest.full(), content.clone());
        state.redirect_blob = Some(digest.full());
    }
    assert_eq!(
        client
            .pull_blob_to_store(&digest, &target)
            .expect("pull via redirect"),
        content.len() as u64
    );
    assert_eq!(target.get_verified(&digest).unwrap(), content);
    assert_eq!(
        registry.state().cdn_authorization_present,
        Some(false),
        "registry bearer token must not leak to an offloaded host"
    );
}

#[test]
fn pull_rejects_corrupted_blob_and_leaves_store_clean() {
    let registry = FakeRegistry::start();
    let client = registry.client("test/repo");
    let directory = TempDir::new().expect("tempdir");
    let target = store(&directory, "target");
    let expected = Digest::sha256(b"expected-bytes");
    registry.state().tamper_blob = Some(expected.full());
    let error = client.pull_blob_to_store(&expected, &target).unwrap_err();
    assert_eq!(error.kind(), ErrorKind::InvalidArgument);
    assert!(
        error.message().contains("digest mismatch"),
        "{}",
        error.message()
    );
    assert!(target.list().unwrap().is_empty());
}

#[test]
fn chunked_upload_resumes_after_416() {
    let registry = FakeRegistry::start();
    let client = registry.client("test/repo");
    let directory = TempDir::new().expect("tempdir");
    let source = store(&directory, "source");
    let target = store(&directory, "target");
    let content: Vec<u8> = (0..4_097u32).map(|index| (index % 253) as u8).collect();
    let (digest, size) = source.put(&content).expect("store blob");
    assert!(client
        .push_blob_chunked(&digest, &source, 1_000)
        .expect("chunked push"));
    {
        let state = registry.state();
        assert!(
            state.patches >= 6,
            "expected one rejected + five accepted PATCH requests, got {}",
            state.patches
        );
        assert!(
            state.status_queries >= 1,
            "resume must query the upload status"
        );
        assert!(
            state
                .patch_targets
                .get(1)
                .is_some_and(|target| target.contains("_state=resumed")),
            "the client must follow the Location returned by the status query: {:?}",
            state.patch_targets
        );
        assert_eq!(state.blobs.get(&digest.full()), Some(&content));
    }
    assert_eq!(
        client
            .pull_blob_to_store(&digest, &target)
            .expect("pull chunked blob"),
        size
    );
    assert_eq!(target.get_verified(&digest).unwrap(), content);
}

#[test]
fn registry_chunk_min_length_is_honored() {
    let registry = FakeRegistry::start();
    {
        let mut state = registry.state();
        state.min_chunk = Some(2_048);
    }
    let client = registry.client("test/repo");
    let directory = TempDir::new().expect("tempdir");
    let source = store(&directory, "source");
    let target = store(&directory, "target");
    let content: Vec<u8> = (0..4_097u32).map(|index| (index % 247) as u8).collect();
    let (digest, size) = source.put(&content).expect("store blob");
    assert!(client
        .push_blob_chunked(&digest, &source, 1_000)
        .expect("chunked push with registry minimum"));
    {
        let state = registry.state();
        // 4,097 bytes at the advertised 2,048-byte minimum: one rejected
        // probe plus 2,048 + 2,048 + 1-byte chunks. Requesting 1,000-byte
        // chunks would have produced six PATCH requests instead of four.
        assert_eq!(
            state.patches, 4,
            "OCI-Chunk-Min-Length must raise the effective chunk size"
        );
        assert_eq!(state.blobs.get(&digest.full()), Some(&content));
    }
    assert_eq!(
        client.pull_blob_to_store(&digest, &target).expect("pull"),
        size
    );
    assert_eq!(target.get_verified(&digest).unwrap(), content);
}

#[test]
fn tag_listing_follows_link_pagination() {
    let registry = FakeRegistry::start();
    let client = registry.client("test/repo");
    let config = b"config".to_vec();
    let manifest_bytes = now_manifest(
        &descriptor_for(&config, MEDIA_TYPE_IMAGE_CONFIG),
        Vec::new(),
    );
    for tag in ["t1", "t2", "t3", "t4", "t5"] {
        client
            .put_manifest(tag, &manifest_bytes, MEDIA_TYPE_IMAGE_MANIFEST)
            .expect("put tag");
    }
    let first = client.list_tags(Some(2), None).expect("first page");
    assert_eq!(first.tags, vec!["t1".to_string(), "t2".to_string()]);
    assert!(first.next.is_some(), "registry must paginate five tags");
    let all = client.list_all_tags(2).expect("all tags");
    assert_eq!(all, vec!["t1", "t2", "t3", "t4", "t5"]);
}

#[test]
fn referrers_api_and_tag_schema_fallback() {
    let registry = FakeRegistry::start();
    let client = registry.client("test/repo");
    let subject = Digest::sha256(b"referrers-subject");
    let index = Index {
        manifests: vec![
            artifact_descriptor("application/vnd.example.a"),
            artifact_descriptor("application/vnd.example.b"),
        ],
        ..Index::new()
    };
    registry
        .state()
        .referrers
        .insert(subject.full(), serde_json::to_string(&index).unwrap());

    let all = client.referrers(&subject, None).expect("referrers");
    assert_eq!(all.manifests.len(), 2);
    let filtered = client
        .referrers(&subject, Some("application/vnd.example.a"))
        .expect("filtered referrers");
    assert_eq!(filtered.manifests.len(), 1);
    assert_eq!(
        filtered.manifests[0].artifact_type.as_deref(),
        Some("application/vnd.example.a")
    );

    let fallback = registry.client("test/fallback");
    let subject = Digest::sha256(b"fallback-subject");
    let index = Index {
        manifests: vec![artifact_descriptor("application/vnd.example.c")],
        ..Index::new()
    };
    let tag = format!("{}-{}", subject.algor().as_str(), subject.hex());
    fallback
        .put_manifest(
            &tag,
            &serde_json::to_vec(&index).unwrap(),
            MEDIA_TYPE_IMAGE_INDEX,
        )
        .expect("put fallback index");
    let result = fallback.referrers(&subject, None).expect("fallback");
    assert_eq!(result.manifests.len(), 1);
    assert_eq!(
        result.manifests[0].artifact_type.as_deref(),
        Some("application/vnd.example.c")
    );

    let empty = fallback
        .referrers(&Digest::sha256(b"no-referrers"), None)
        .expect("no referrers is not an error");
    assert!(empty.manifests.is_empty());
}

#[test]
fn auth_failures_are_classified() {
    let registry = FakeRegistry::start();
    assert!(registry.client("test/repo").ping().is_ok());

    let forbidden = registry.client("test/forbidden");
    let error = forbidden.get_manifest("v1").unwrap_err();
    assert_eq!(error.kind(), ErrorKind::Forbidden);
    assert!(error.message().contains("DENIED"), "{}", error.message());

    let unauthorized = registry.client("test/unauthorized");
    let error = unauthorized.get_manifest("v1").unwrap_err();
    assert_eq!(error.kind(), ErrorKind::Unauthorized);

    assert!(registry
        .state()
        .token_requests
        .iter()
        .any(|request| request.contains("repository:test/repo:pull")));
}
