//! Synchronous Engine HTTP transport and route dispatch.
//!
//! `tiny_http` is a thread-per-connection server. That matches the daemon's
//! single-owner actor: each request makes one blocking command/reply call and
//! never holds a daemon lock. `/events` is the only long-lived response; its
//! body is a `Read` implementation fed by the event broker subscription.

use std::collections::{BTreeMap, VecDeque};
use std::fmt::Write as _;
use std::io::{self, Cursor, Read, Seek, SeekFrom};
use std::net::SocketAddr;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use std::thread::{self, JoinHandle};
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use chrono::{SecondsFormat, Utc};
use docker_common::ImageRef;
use docker_container_model::config::Signal;
use docker_container_model::state::LifecycleState;
use docker_errors::{Error, Result};
use docker_events::{Event, EventBroker, Subscription};
use docker_image::Platform;
use docker_image_store::{canonical_registry_repo, parse_registry_auth, ImageStore, RegistryAuth};
use serde::Serialize;
use serde_json::Value;
use tiny_http::{Header, Request, Response, StatusCode};

use crate::dto::{
    self, ContainerSummary, CreateResponse, ErrorResponse, InfoPlugins, InfoResponse,
    InspectResponse, RuntimeInfo, SwarmInfo, VersionComponent, VersionPlatform, VersionResponse,
    WaitResponse, MAX_BODY_BYTES,
};
use crate::image_dto::{
    history_items, ImageDeleteItem, ImageInspectResponse, ImageLoadMessage, ImagePullMessage,
    ImageSummary,
};
use crate::query::{ContainerListOptions, EventFilters, EventOptions, Query};
use crate::version::{self, VersionDecision, VersionGate, VersionUnsupported};

/// Upper bound for an imported image archive. The body is spooled to a
/// temporary file, so this is a disk bound rather than a memory bound.
const MAX_IMAGE_ARCHIVE_BYTES: u64 = 64 * 1024 * 1024 * 1024;

/// API server configuration. The version strings are exposed as fields so a
/// future embedder can advertise a narrowed range without changing routes.
#[derive(Debug, Clone)]
pub struct ServerConfig {
    pub server_version: String,
    pub api_version: String,
    pub min_api_version: String,
    pub runtime_name: String,
    pub runtime_path: String,
    pub synthetic: bool,
}

impl Default for ServerConfig {
    fn default() -> Self {
        Self {
            server_version: env!("CARGO_PKG_VERSION").to_string(),
            api_version: version::DEFAULT_VERSION.to_string(),
            min_api_version: version::MIN_VERSION.to_string(),
            runtime_name: "synthetic".to_string(),
            runtime_path: "in-process".to_string(),
            synthetic: true,
        }
    }
}

/// HTTP API facade over a daemon and its event broker.
pub struct ApiServer {
    daemon: Arc<docker_daemon::Daemon>,
    broker: Arc<EventBroker>,
    images: Option<Arc<ImageStore>>,
    gate: VersionGate,
    config: ServerConfig,
    daemon_id: String,
}

impl ApiServer {
    /// Build a server with this build's supported API range.
    pub fn new(daemon: Arc<docker_daemon::Daemon>, broker: Arc<EventBroker>) -> Self {
        Self::with_config(daemon, broker, ServerConfig::default())
            .expect("default API version configuration is valid")
    }

    /// Build a server with explicit configuration.
    pub fn with_config(
        daemon: Arc<docker_daemon::Daemon>,
        broker: Arc<EventBroker>,
        config: ServerConfig,
    ) -> Result<Self> {
        let gate = VersionGate::new(&config.min_api_version, &config.api_version)
            .map_err(|error| Error::invalid_argument(error.to_string()))?;
        Ok(Self {
            daemon,
            broker,
            images: None,
            gate,
            config,
            daemon_id: random_hex(16),
        })
    }

    /// Attach a persistent local image store. Without one, image endpoints
    /// return an explicit `Unsupported` error instead of pretending no images
    /// exist.
    pub fn with_image_store(mut self, images: Arc<ImageStore>) -> Self {
        self.images = Some(images);
        self
    }

    /// Bind and serve on `addr`. The returned handle stops the accept loop on
    /// drop; request threads finish or are closed by their clients.
    pub fn serve(self: Arc<Self>, addr: &str) -> Result<RunningServer> {
        let server = tiny_http::Server::http(addr).map_err(|error| {
            Error::internal(format!("failed to bind Engine API listener: {error}"))
        })?;
        let local_addr = server
            .server_addr()
            .to_ip()
            .ok_or_else(|| Error::internal("Engine API listener is not an IP socket"))?;
        let stop = Arc::new(AtomicBool::new(false));
        let stop_flag = Arc::clone(&stop);
        let api = Arc::clone(&self);
        let handle = thread::Builder::new()
            .name("docker-api-accept".into())
            .spawn(move || {
                while !stop_flag.load(Ordering::Relaxed) {
                    match server.recv_timeout(Duration::from_millis(50)) {
                        Ok(Some(request)) => {
                            let api = Arc::clone(&api);
                            let _ = thread::Builder::new()
                                .name("docker-api-request".into())
                                .spawn(move || api.handle_request(request));
                        }
                        Ok(None) => {}
                        Err(error) => {
                            if !stop_flag.load(Ordering::Relaxed) {
                                tracing::warn!(error = %error, "Engine API accept loop stopped");
                            }
                            break;
                        }
                    }
                }
            })
            .map_err(|error| {
                Error::internal(format!("failed to start API accept thread: {error}"))
            })?;
        Ok(RunningServer {
            addr: local_addr,
            stop,
            handle: Some(handle),
        })
    }

    fn handle_request(&self, mut request: Request) {
        let result = self.route(&mut request);
        match result {
            Ok(Reply::Buffered {
                status,
                content_type,
                body,
                headers,
            }) => {
                let mut response_headers = self.base_headers();
                response_headers.extend(headers);
                if let Some(content_type) = content_type {
                    response_headers.push(header("Content-Type", content_type));
                }
                let length = body.len();
                let response = Response::new(
                    StatusCode(status),
                    response_headers,
                    Cursor::new(body),
                    Some(length),
                    None,
                );
                if let Err(error) = request.respond(response) {
                    tracing::debug!(error = %error, "failed to write Engine API response");
                }
            }
            Ok(Reply::Stream {
                content_type,
                body,
                headers,
            }) => {
                let mut response_headers = self.base_headers();
                response_headers.extend(headers);
                response_headers.push(header("Content-Type", content_type));
                let response = Response::new(StatusCode(200), response_headers, body, None, None);
                if let Err(error) = request.respond(response) {
                    tracing::debug!(error = %error, "failed to stream Engine API response");
                }
            }
            Err(error) => {
                let body = serde_json::to_vec(&ErrorResponse {
                    message: &error.message,
                })
                .unwrap_or_else(|_| br#"{"message":"internal error"}"#.to_vec());
                let mut headers = self.base_headers();
                headers.push(header("Content-Type", "application/json"));
                let response = Response::new(
                    StatusCode(error.status),
                    headers,
                    Cursor::new(body.clone()),
                    Some(body.len()),
                    None,
                );
                if let Err(write_error) = request.respond(response) {
                    tracing::debug!(error = %write_error, "failed to write Engine API error");
                }
            }
        }
    }

    fn route(&self, request: &mut Request) -> std::result::Result<Reply, ApiError> {
        let url = request.url();
        let (path, raw_query) = match url.split_once('?') {
            Some((path, query)) => (path, query),
            None => (url, ""),
        };
        let query = Query::parse(raw_query).map_err(ApiError::from)?;
        let (requested, path) = version::split_version(path);
        let decision = self.gate.check(requested);
        if let VersionDecision::TooOld { requested, minimum } = &decision {
            return Err(ApiError::bad_request(
                VersionUnsupported {
                    requested: requested.clone(),
                    minimum: Some(minimum.clone()),
                    maximum: None,
                }
                .to_string(),
            ));
        }
        if let VersionDecision::TooNew { requested, maximum } = &decision {
            return Err(ApiError::bad_request(
                VersionUnsupported {
                    requested: requested.clone(),
                    minimum: None,
                    maximum: Some(maximum.clone()),
                }
                .to_string(),
            ));
        }
        let path = if path.is_empty() { "/" } else { path };
        let segments: Vec<&str> = path
            .trim_matches('/')
            .split('/')
            .filter(|segment| !segment.is_empty())
            .collect();
        let method = request.method().as_str();

        match segments.as_slice() {
            ["_ping"] => self.ping(method),
            ["version"] if method == "GET" => Ok(self.version()),
            ["info"] if method == "GET" => self.info(),
            ["events"] if method == "GET" => self.events(&query),
            ["containers", "json"] if method == "GET" => self.list(query),
            ["containers", "create"] if method == "POST" => self.create(request, &query),
            ["containers", id, "json"] if method == "GET" => self.inspect(id),
            ["containers", id, "start"] if method == "POST" => self.action(id, Action::Start),
            ["containers", id, "stop"] if method == "POST" => self.stop(id, &query),
            ["containers", id, "restart"] if method == "POST" => self.restart(id, &query),
            ["containers", id, "kill"] if method == "POST" => self.kill(id, &query),
            ["containers", id, "wait"] if method == "POST" || method == "GET" => {
                self.wait(id, &query)
            }
            ["containers", id] if method == "DELETE" => self.remove(id, &query),
            ["containers", id, action] => self.unsupported_container_action(id, action),
            ["images", "json"] if method == "GET" => self.image_list(&query),
            ["images", "create"] if method == "POST" => self.image_pull(request, &query),
            ["images", "prune"] if method == "POST" => Err(ApiError::from(Error::unsupported(
                "image prune is not implemented",
            ))),
            ["images", "search"] if method == "GET" => Err(ApiError::from(Error::unsupported(
                "image search is not implemented",
            ))),
            ["images", "get"] if method == "GET" => self.image_save(&query),
            ["images", "load"] if method == "POST" => self.image_load(request, &query),
            ["images", rest @ ..] => self.route_image(method, rest, &query),
            _ => Err(ApiError::not_found("page not found")),
        }
    }

    fn ping(&self, method: &str) -> std::result::Result<Reply, ApiError> {
        if method != "GET" && method != "HEAD" {
            return Err(ApiError::not_found("page not found"));
        }
        let body = if method == "HEAD" {
            Vec::new()
        } else {
            b"OK".to_vec()
        };
        Ok(
            Reply::buffered(200, "text/plain; charset=utf-8", body).with_headers(vec![
                header("Cache-Control", "no-cache, no-store, must-revalidate"),
                header("Pragma", "no-cache"),
                header("Swarm", "inactive"),
            ]),
        )
    }

    fn version(&self) -> Reply {
        let os = std::env::consts::OS.to_string();
        let arch = rust_arch();
        let mut details = BTreeMap::new();
        details.insert("ApiVersion".to_string(), self.config.api_version.clone());
        details.insert("Arch".to_string(), arch.clone());
        details.insert("BuildTime".to_string(), String::new());
        details.insert("Experimental".to_string(), "false".to_string());
        details.insert("GitCommit".to_string(), String::new());
        details.insert("GoVersion".to_string(), String::new());
        details.insert("KernelVersion".to_string(), String::new());
        details.insert(
            "MinAPIVersion".to_string(),
            self.config.min_api_version.clone(),
        );
        details.insert("Os".to_string(), os.clone());
        let body = VersionResponse {
            platform: VersionPlatform {
                name: "Docker Engine - Community".to_string(),
            },
            components: vec![VersionComponent {
                name: "Engine".to_string(),
                version: self.config.server_version.clone(),
                details,
            }],
            version: self.config.server_version.clone(),
            api_version: self.config.api_version.clone(),
            min_api_version: self.config.min_api_version.clone(),
            git_commit: String::new(),
            go_version: String::new(),
            os,
            arch,
            kernel_version: String::new(),
            experimental: false,
            build_time: String::new(),
        };
        Reply::json(200, &body)
    }

    fn info(&self) -> std::result::Result<Reply, ApiError> {
        let views = self.daemon.list(&[]).map_err(ApiError::from)?;
        let total = views.len();
        let running = views
            .iter()
            .filter(|view| view.state.status == LifecycleState::Running)
            .count();
        let paused = views
            .iter()
            .filter(|view| view.state.status == LifecycleState::Paused)
            .count();
        let stopped = total.saturating_sub(running + paused);
        let mut runtimes = BTreeMap::new();
        runtimes.insert(
            self.config.runtime_name.clone(),
            RuntimeInfo {
                path: self.config.runtime_path.clone(),
                status: BTreeMap::new(),
            },
        );
        let warnings = if self.config.synthetic {
            vec![
                "synthetic runtime: container lifecycle is real, but no host process is executed"
                    .to_string(),
            ]
        } else {
            Vec::new()
        };
        let image_count = self
            .images
            .as_ref()
            .map(|images| images.list(true).len())
            .unwrap_or(0);
        let image_status = if self.images.is_some() {
            "connected"
        } else {
            "not connected"
        };
        let body = InfoResponse {
            id: self.daemon_id.clone(),
            containers: total,
            containers_running: running,
            containers_paused: paused,
            containers_stopped: stopped,
            images: image_count,
            driver: "docker-rs".to_string(),
            driver_status: vec![
                ["Runtime".to_string(), self.config.runtime_name.clone()],
                ["Images".to_string(), image_status.to_string()],
            ],
            plugins: InfoPlugins {
                volume: Vec::new(),
                network: Vec::new(),
                authorization: None,
                log: vec!["none".to_string()],
            },
            memory_limit: false,
            swap_limit: false,
            kernel_memory: false,
            cpu_cfs_period: false,
            cpu_cfs_quota: false,
            cpu_shares: false,
            cpu_set: false,
            pids_limit: false,
            ipv4_forwarding: false,
            debug: false,
            nfd: 0,
            oom_kill_disable: false,
            n_goroutines: 0,
            system_time: now_rfc3339(),
            logging_driver: "none".to_string(),
            cgroup_driver: String::new(),
            cgroup_version: String::new(),
            n_events_listener: self.broker.subscriber_count(),
            kernel_version: String::new(),
            operating_system: operating_system(),
            os_version: String::new(),
            os_type: std::env::consts::OS.to_string(),
            architecture: rust_arch(),
            index_server_address: "https://index.docker.io/v1/".to_string(),
            registry_config: Value::Object(Default::default()),
            ncpu: thread::available_parallelism()
                .map(std::num::NonZeroUsize::get)
                .unwrap_or(1),
            mem_total: 0,
            generic_resources: Vec::new(),
            docker_root_dir: String::new(),
            http_proxy: std::env::var("HTTP_PROXY").unwrap_or_default(),
            https_proxy: std::env::var("HTTPS_PROXY").unwrap_or_default(),
            no_proxy: std::env::var("NO_PROXY").unwrap_or_default(),
            name: std::env::var("COMPUTERNAME")
                .or_else(|_| std::env::var("HOSTNAME"))
                .unwrap_or_else(|_| "docker-rs".to_string()),
            labels: Vec::new(),
            experimental_build: false,
            server_version: self.config.server_version.clone(),
            runtimes,
            default_runtime: self.config.runtime_name.clone(),
            swarm: SwarmInfo {
                local_node_state: "inactive".to_string(),
                control_available: false,
                error: String::new(),
            },
            live_restore_enabled: false,
            isolation: String::new(),
            init_binary: "docker-runtime-init".to_string(),
            containerd_commit: dto::CommitInfo {
                id: String::new(),
                expected: String::new(),
            },
            runc_commit: dto::CommitInfo {
                id: String::new(),
                expected: String::new(),
            },
            init_commit: dto::CommitInfo {
                id: String::new(),
                expected: String::new(),
            },
            security_options: Vec::new(),
            warnings,
        };
        Ok(Reply::json(200, &body))
    }

    fn list(&self, query: Query) -> std::result::Result<Reply, ApiError> {
        let options = ContainerListOptions::parse(&query).map_err(ApiError::from)?;
        let mut views = self.daemon.list(&[]).map_err(ApiError::from)?;
        views.retain(|view| {
            (options.all || view.state.status.is_alive()) && options.filters.matches(view)
        });
        views.sort_by(|left, right| {
            right
                .created_at
                .cmp(&left.created_at)
                .then_with(|| left.id.as_str().cmp(right.id.as_str()))
        });
        if let Some(limit) = options.limit {
            views.truncate(limit);
        }
        let body: Vec<ContainerSummary> = views.iter().map(ContainerSummary::from).collect();
        Ok(Reply::json(200, &body))
    }

    fn image_store(&self) -> std::result::Result<&ImageStore, ApiError> {
        self.images.as_deref().ok_or_else(|| {
            ApiError::from(Error::unsupported(
                "image store is not configured (start docker-rsd with a data root)",
            ))
        })
    }

    fn route_image(
        &self,
        method: &str,
        rest: &[&str],
        query: &Query,
    ) -> std::result::Result<Reply, ApiError> {
        if rest.is_empty() {
            return Err(ApiError::not_found("page not found"));
        }
        if method == "DELETE" {
            return self.image_delete(&decode_image_name(rest)?, query);
        }
        if rest.len() >= 2 {
            let (action, name_parts) = rest.split_last().expect("rest is non-empty");
            if !name_parts.is_empty() {
                let name = decode_image_name(name_parts)?;
                match (*action, method) {
                    ("json", "GET") => return self.image_inspect(&name, query),
                    ("history", "GET") => return self.image_history(&name, query),
                    ("tag", "POST") => return self.image_tag(&name, query),
                    ("get", "GET") => return self.image_save_named(&name, query),
                    ("push", "POST") => {
                        return Err(ApiError::from(Error::unsupported(
                            "image push is not implemented",
                        )))
                    }
                    _ => {}
                }
            }
        }
        Err(ApiError::not_found("page not found"))
    }

    fn image_list(&self, query: &Query) -> std::result::Result<Reply, ApiError> {
        if let Some(raw) = query
            .get("filters")
            .map(str::trim)
            .filter(|raw| !raw.is_empty())
        {
            let parsed: Value = serde_json::from_str(raw)
                .map_err(|error| ApiError::bad_request(format!("invalid filters JSON: {error}")))?;
            match parsed {
                Value::Object(filters) if filters.is_empty() => {}
                Value::Object(_) => {
                    return Err(ApiError::from(Error::unsupported(
                        "image list filters are not implemented",
                    )))
                }
                _ => return Err(ApiError::bad_request("filters must be a JSON object")),
            }
        }
        let store = self.image_store()?;
        let all = query.bool_value("all", false);
        let body: Vec<ImageSummary> = store.list(all).iter().map(ImageSummary::from).collect();
        Ok(Reply::json(200, &body))
    }

    fn image_save(&self, query: &Query) -> std::result::Result<Reply, ApiError> {
        let names: Vec<String> = query.get_all("names").map(str::to_string).collect();
        self.image_save_names(&names, query)
    }

    fn image_save_named(&self, name: &str, query: &Query) -> std::result::Result<Reply, ApiError> {
        self.image_save_names(&[name.to_string()], query)
    }

    fn image_save_names(
        &self,
        names: &[String],
        query: &Query,
    ) -> std::result::Result<Reply, ApiError> {
        let platform = decode_platform(query)?;
        let mut archive = tempfile::tempfile().map_err(|error| {
            ApiError::from(Error::internal(format!(
                "failed to create image archive spool: {error}"
            )))
        })?;
        self.image_store()?
            .export_archive(names, platform.as_ref(), &mut archive)
            .map_err(ApiError::from)?;
        archive.seek(SeekFrom::Start(0)).map_err(|error| {
            ApiError::from(Error::internal(format!(
                "failed to rewind image archive: {error}"
            )))
        })?;
        Ok(Reply::stream("application/x-tar", Box::new(archive)))
    }

    fn image_load(
        &self,
        request: &mut Request,
        query: &Query,
    ) -> std::result::Result<Reply, ApiError> {
        let platform = decode_platform(query)?;
        let mut archive = tempfile::tempfile().map_err(|error| {
            ApiError::from(Error::internal(format!(
                "failed to create image load spool: {error}"
            )))
        })?;
        copy_request_body(request, &mut archive, MAX_IMAGE_ARCHIVE_BYTES)?;
        archive.seek(SeekFrom::Start(0)).map_err(|error| {
            ApiError::from(Error::internal(format!(
                "failed to rewind image load spool: {error}"
            )))
        })?;
        let outcome = self
            .image_store()?
            .load_archive(&mut archive, platform.as_ref())
            .map_err(ApiError::from)?;

        let mut body = Vec::new();
        for image in outcome.images {
            if image.tags.is_empty() {
                push_load_message(&mut body, format!("Loaded image ID: {}\n", image.id.full()))?;
            } else {
                for tag in image.tags {
                    push_load_message(&mut body, format!("Loaded image: {tag}\n"))?;
                }
            }
        }
        Ok(Reply::buffered(200, "application/json", body))
    }

    fn image_pull(
        &self,
        request: &mut Request,
        query: &Query,
    ) -> std::result::Result<Reply, ApiError> {
        if query
            .get("fromSrc")
            .is_some_and(|value| !value.trim().is_empty())
        {
            return Err(ApiError::from(Error::unsupported(
                "importing images from a source is not implemented",
            )));
        }
        let from_image = query
            .get("fromImage")
            .map(str::trim)
            .filter(|value| !value.is_empty())
            .ok_or_else(|| ApiError::bad_request("fromImage must not be empty"))?;
        let tag = query
            .get("tag")
            .map(str::trim)
            .filter(|tag| !tag.is_empty());
        let reference = pull_reference(from_image, tag)?;
        let platform = query
            .get("platform")
            .map(str::trim)
            .filter(|platform| !platform.is_empty());
        let auth = registry_auth(request)?;
        let store = self.image_store()?;
        let result = store
            .pull(&reference, platform, auth.as_ref())
            .map_err(ApiError::from)?;
        let parsed = ImageRef::parse(&reference).map_err(ApiError::from)?;
        let (registry, repository) = canonical_registry_repo(&parsed);
        let pull_repository = if registry == "docker.io" {
            repository
        } else {
            format!("{registry}/{repository}")
        };
        let id = parsed
            .digest()
            .map(str::to_string)
            .unwrap_or_else(|| parsed.effective_tag().to_string());
        let status = if result.up_to_date {
            format!("Image is up to date for {}", result.reference)
        } else {
            format!("Downloaded newer image for {}", result.reference)
        };
        let messages = [
            ImagePullMessage::with_id(format!("Pulling from {pull_repository}"), id),
            ImagePullMessage::status(format!("Digest: {}", result.image.manifest_digest.full())),
            ImagePullMessage::status(format!("Status: {status}")),
        ];
        let mut body = Vec::new();
        for message in &messages {
            serde_json::to_writer(&mut body, message).map_err(|error| {
                Error::internal(format!("failed to encode pull status: {error}"))
            })?;
            body.push(b'\n');
        }
        Ok(Reply::buffered(200, "application/json", body))
    }

    fn image_inspect(&self, name: &str, query: &Query) -> std::result::Result<Reply, ApiError> {
        if query.bool_value("manifests", false) {
            return Err(ApiError::from(Error::unsupported(
                "image manifest inspection is not implemented",
            )));
        }
        if query
            .get("platform")
            .is_some_and(|platform| !platform.trim().is_empty())
        {
            return Err(ApiError::from(Error::unsupported(
                "inspecting an image for another platform is not implemented",
            )));
        }
        let record = self.image_store()?.resolve(name).map_err(ApiError::from)?;
        Ok(Reply::json(200, &ImageInspectResponse::from(&record)))
    }

    fn image_history(&self, name: &str, query: &Query) -> std::result::Result<Reply, ApiError> {
        if query
            .get("platform")
            .is_some_and(|platform| !platform.trim().is_empty())
        {
            return Err(ApiError::from(Error::unsupported(
                "image history for another platform is not implemented",
            )));
        }
        let record = self.image_store()?.resolve(name).map_err(ApiError::from)?;
        Ok(Reply::json(200, &history_items(&record)))
    }

    fn image_tag(&self, name: &str, query: &Query) -> std::result::Result<Reply, ApiError> {
        let repo = query
            .get("repo")
            .map(str::trim)
            .filter(|repo| !repo.is_empty())
            .ok_or_else(|| ApiError::bad_request("repo must not be empty"))?;
        let tag = query.get("tag").map(str::trim).unwrap_or("latest");
        self.image_store()?
            .tag(name, repo, tag)
            .map_err(ApiError::from)?;
        Ok(Reply::empty(201))
    }

    fn image_delete(&self, name: &str, query: &Query) -> std::result::Result<Reply, ApiError> {
        let force = query.bool_value("force", false);
        let outcome = self
            .image_store()?
            .delete(name, force)
            .map_err(ApiError::from)?;
        let mut body = Vec::with_capacity(outcome.untagged.len() + outcome.deleted.len());
        for untagged in outcome.untagged {
            body.push(ImageDeleteItem {
                untagged: Some(untagged),
                deleted: None,
            });
        }
        for deleted in outcome.deleted {
            body.push(ImageDeleteItem {
                untagged: None,
                deleted: Some(deleted),
            });
        }
        Ok(Reply::json(200, &body))
    }

    fn inspect(&self, id: &str) -> std::result::Result<Reply, ApiError> {
        let view = self.daemon.inspect(id).map_err(ApiError::from)?;
        Ok(Reply::json(200, &InspectResponse::from(&view)))
    }

    fn create(&self, request: &mut Request, query: &Query) -> std::result::Result<Reply, ApiError> {
        let body = read_json_body(request)?;
        let dto: dto::CreateRequestDto = serde_json::from_slice(&body)
            .map_err(|error| ApiError::bad_request(format!("invalid JSON: {error}")))?;
        let requested_name = query
            .get("name")
            .map(str::trim)
            .filter(|name| !name.is_empty())
            .map(|name| name.trim_start_matches('/').to_string());
        let name = requested_name.unwrap_or_else(generated_name);
        let image = match &self.images {
            Some(store) => Some(store.resolve(dto.image.trim()).map_err(ApiError::from)?),
            None => None,
        };
        let mut create = dto
            .into_daemon_with_image(Some(name), image.as_ref().map(|record| &record.config))
            .map_err(ApiError::from)?;
        if let Some(image) = &image {
            create = create.with_image_id(image.id.clone());
        }
        let view = self.daemon.create(create).map_err(ApiError::from)?;
        Ok(Reply::json(
            201,
            &CreateResponse {
                id: view.id.as_str().to_string(),
                warnings: Vec::new(),
            },
        ))
    }

    fn action(&self, id: &str, action: Action) -> std::result::Result<Reply, ApiError> {
        match action {
            Action::Start => {
                // Moby returns 304 when start is a no-op. The actor has the
                // same idempotent semantics, so inspect first to preserve the
                // REST status distinction for clients such as `docker start`.
                let view = self.daemon.inspect(id).map_err(ApiError::from)?;
                if matches!(
                    view.state.status,
                    LifecycleState::Running | LifecycleState::Restarting
                ) {
                    return Ok(Reply::empty(304));
                }
                self.daemon.start(id).map_err(ApiError::from)?;
            }
        }
        Ok(Reply::empty(204))
    }

    fn stop(&self, id: &str, query: &Query) -> std::result::Result<Reply, ApiError> {
        let view = self.daemon.inspect(id).map_err(ApiError::from)?;
        if matches!(
            view.state.status,
            LifecycleState::Created | LifecycleState::Exited | LifecycleState::Dead
        ) {
            return Ok(Reply::empty(304));
        }
        let timeout = stop_timeout(query)?;
        self.daemon.stop(id, timeout).map_err(ApiError::from)?;
        Ok(Reply::empty(204))
    }

    fn restart(&self, id: &str, query: &Query) -> std::result::Result<Reply, ApiError> {
        let timeout = stop_timeout(query)?;
        self.daemon.restart(id, timeout).map_err(ApiError::from)?;
        Ok(Reply::empty(204))
    }

    fn kill(&self, id: &str, query: &Query) -> std::result::Result<Reply, ApiError> {
        let signal = match query.get("signal").filter(|signal| !signal.is_empty()) {
            None => Signal::Sigkill,
            Some(raw) => parse_signal(raw)?,
        };
        self.daemon.kill(id, signal).map_err(ApiError::from)?;
        Ok(Reply::empty(204))
    }

    fn remove(&self, id: &str, query: &Query) -> std::result::Result<Reply, ApiError> {
        let force = query.bool_value("force", false);
        self.daemon.remove(id, force).map_err(ApiError::from)?;
        Ok(Reply::empty(204))
    }

    fn wait(&self, id: &str, query: &Query) -> std::result::Result<Reply, ApiError> {
        match query.get("condition").unwrap_or("not-running") {
            "" | "not-running" => {}
            "next-exit" | "removed" => {
                return Err(ApiError::from(Error::unsupported(format!(
                    "wait condition {:?} is not implemented; use \"not-running\"",
                    query.get("condition").unwrap_or_default()
                ))))
            }
            other => {
                return Err(ApiError::bad_request(format!(
                    "invalid wait condition {other:?}"
                )))
            }
        }
        loop {
            let view = self.daemon.inspect(id).map_err(ApiError::from)?;
            if !view.state.status.is_alive() {
                return Ok(Reply::json(
                    200,
                    &WaitResponse {
                        status_code: i64::from(view.state.exit_code.unwrap_or(0)),
                        error: None,
                    },
                ));
            }
            thread::sleep(Duration::from_millis(25));
        }
    }

    fn unsupported_container_action(
        &self,
        id: &str,
        action: &str,
    ) -> std::result::Result<Reply, ApiError> {
        match action {
            "attach" | "attach/ws" | "exec" | "logs" | "stats" | "top" | "changes" | "export"
            | "archive" | "pause" | "unpause" | "update" | "rename" | "resize" => {
                Err(ApiError::from(Error::unsupported(format!(
                    "container endpoint /containers/{id}/{action} is not implemented"
                ))))
            }
            _ => Err(ApiError::not_found("page not found")),
        }
    }

    fn events(&self, query: &Query) -> std::result::Result<Reply, ApiError> {
        let options = EventOptions::parse(query).map_err(ApiError::from)?;
        let since = event_replay_since(&options);
        let (replay, subscription) = self
            .broker
            .subscribe_since(since, 1024)
            .map_err(ApiError::from)?;
        let stream = EventStream::new(replay, subscription, options.filters, options.until_nano);
        Ok(Reply::stream("application/json", Box::new(stream)))
    }

    fn base_headers(&self) -> Vec<Header> {
        vec![
            header("Api-Version", &self.config.api_version),
            header(
                "Server",
                &format!(
                    "Docker/{} ({})",
                    self.config.server_version,
                    std::env::consts::OS
                ),
            ),
            header("Ostype", std::env::consts::OS),
        ]
    }
}

/// Handle returned by [`ApiServer::serve`]. Dropping it stops accepting new
/// connections after at most one 50 ms poll interval.
pub struct RunningServer {
    addr: SocketAddr,
    stop: Arc<AtomicBool>,
    handle: Option<JoinHandle<()>>,
}

impl RunningServer {
    pub fn addr(&self) -> SocketAddr {
        self.addr
    }
}

impl Drop for RunningServer {
    fn drop(&mut self) {
        self.stop.store(true, Ordering::Relaxed);
        if let Some(handle) = self.handle.take() {
            let _ = handle.join();
        }
    }
}

enum Reply {
    Buffered {
        status: u16,
        content_type: Option<&'static str>,
        body: Vec<u8>,
        headers: Vec<Header>,
    },
    Stream {
        content_type: &'static str,
        body: Box<dyn Read + Send>,
        headers: Vec<Header>,
    },
}

impl Reply {
    fn buffered(status: u16, content_type: &'static str, body: Vec<u8>) -> Self {
        Self::Buffered {
            status,
            content_type: Some(content_type),
            body,
            headers: Vec::new(),
        }
    }

    fn json<T: Serialize>(status: u16, body: &T) -> Self {
        match serde_json::to_vec(body) {
            Ok(body) => Self::buffered(status, "application/json", body),
            Err(error) => Self::Buffered {
                status: 500,
                content_type: Some("application/json"),
                body: format!("{{\"message\":\"JSON encoding failed: {error}\"}}").into_bytes(),
                headers: Vec::new(),
            },
        }
    }

    fn empty(status: u16) -> Self {
        Self::Buffered {
            status,
            content_type: None,
            body: Vec::new(),
            headers: Vec::new(),
        }
    }

    fn stream(content_type: &'static str, body: Box<dyn Read + Send>) -> Self {
        Self::Stream {
            content_type,
            body,
            headers: Vec::new(),
        }
    }

    fn with_headers(mut self, extra: Vec<Header>) -> Self {
        match &mut self {
            Self::Buffered { headers, .. } | Self::Stream { headers, .. } => headers.extend(extra),
        }
        self
    }
}

#[derive(Debug)]
struct ApiError {
    status: u16,
    message: String,
}

impl ApiError {
    fn bad_request(message: impl Into<String>) -> Self {
        Self {
            status: 400,
            message: message.into(),
        }
    }

    fn not_found(message: impl Into<String>) -> Self {
        Self {
            status: 404,
            message: message.into(),
        }
    }
}

impl From<Error> for ApiError {
    fn from(error: Error) -> Self {
        Self {
            status: error.kind().http_status(),
            message: error.message().to_string(),
        }
    }
}

#[derive(Debug, Clone, Copy)]
enum Action {
    Start,
}

fn stop_timeout(query: &Query) -> std::result::Result<Option<Duration>, ApiError> {
    match query.get("t") {
        None | Some("") => Ok(None),
        Some("-1") => Err(ApiError::from(Error::unsupported(
            "stop timeout -1 (wait forever) is not implemented",
        ))),
        Some(raw) => raw
            .parse::<u64>()
            .map(|seconds| Some(Duration::from_secs(seconds)))
            .map_err(|_| ApiError::bad_request(format!("invalid stop timeout {raw:?}"))),
    }
}

fn parse_signal(raw: &str) -> std::result::Result<Signal, ApiError> {
    if let Ok(signal) = raw.parse::<Signal>() {
        return Ok(signal);
    }
    let number = raw
        .parse::<i32>()
        .map_err(|_| ApiError::bad_request(format!("invalid signal {raw:?}")))?;
    match number {
        1 => Ok(Signal::Sighup),
        2 => Ok(Signal::Sigint),
        3 => Ok(Signal::Sigquit),
        9 => Ok(Signal::Sigkill),
        10 => Ok(Signal::Sigusr1),
        12 => Ok(Signal::Sigusr2),
        13 => Ok(Signal::Sigpipe),
        14 => Ok(Signal::Sigalrm),
        15 => Ok(Signal::Sigterm),
        _ => Err(ApiError::bad_request(format!(
            "signal {number} is not supported"
        ))),
    }
}

fn decode_platform(query: &Query) -> std::result::Result<Option<Platform>, ApiError> {
    let values: Vec<&str> = query.get_all("platform").collect();
    if values.len() > 1 {
        return Err(ApiError::bad_request(
            "multiple platform parameters not supported",
        ));
    }
    let Some(raw) = values.first().copied() else {
        return Ok(None);
    };
    if raw.trim().is_empty() {
        return Ok(None);
    }
    let platform: Platform = serde_json::from_str(raw)
        .map_err(|error| ApiError::bad_request(format!("failed to parse platform: {error}")))?;
    if platform.os.is_empty() || platform.architecture.is_empty() {
        return Err(ApiError::bad_request(
            "both OS and Architecture must be provided",
        ));
    }
    Ok(Some(platform))
}

fn copy_request_body(
    request: &mut Request,
    output: &mut std::fs::File,
    limit: u64,
) -> std::result::Result<u64, ApiError> {
    if let Some(length) = request.body_length() {
        if length as u64 > limit {
            return Err(ApiError {
                status: 413,
                message: format!("request body exceeds {limit} bytes"),
            });
        }
    }
    let copied = io::copy(&mut request.as_reader().take(limit + 1), output)
        .map_err(|error| ApiError::bad_request(format!("failed to read request body: {error}")))?;
    if copied > limit {
        return Err(ApiError {
            status: 413,
            message: format!("request body exceeds {limit} bytes"),
        });
    }
    if copied == 0 {
        return Err(ApiError::bad_request("request body is empty"));
    }
    Ok(copied)
}

fn push_load_message(body: &mut Vec<u8>, stream: String) -> std::result::Result<(), ApiError> {
    serde_json::to_writer(&mut *body, &ImageLoadMessage { stream })
        .map_err(|error| Error::internal(format!("failed to encode image load status: {error}")))?;
    body.push(b'\n');
    Ok(())
}

fn read_json_body(request: &mut Request) -> std::result::Result<Vec<u8>, ApiError> {
    let content_type = request
        .headers()
        .iter()
        .find(|header| header.field.equiv("Content-Type"))
        .map(|header| header.value.as_str().to_string());
    let has_body = request
        .body_length()
        .map(|length| length > 0)
        .unwrap_or(true);
    if has_body {
        let Some(content_type) = content_type.as_deref() else {
            return Err(ApiError::bad_request(
                "invalid content type: Content-Type must be application/json",
            ));
        };
        let media_type = content_type
            .split(';')
            .next()
            .unwrap_or("")
            .trim()
            .to_ascii_lowercase();
        if media_type != "application/json" {
            return Err(ApiError::bad_request(format!(
                "invalid content type {content_type:?}: expected application/json"
            )));
        }
    }
    let mut body = Vec::new();
    request
        .as_reader()
        .take((MAX_BODY_BYTES + 1) as u64)
        .read_to_end(&mut body)
        .map_err(|error| ApiError::bad_request(format!("failed to read request body: {error}")))?;
    if body.len() > MAX_BODY_BYTES {
        return Err(ApiError {
            status: 413,
            message: format!("request body exceeds {MAX_BODY_BYTES} bytes"),
        });
    }
    if body.is_empty() {
        return Err(ApiError::bad_request(
            "invalid JSON: got EOF while reading request body",
        ));
    }
    Ok(body)
}

fn pull_reference(from_image: &str, tag: Option<&str>) -> std::result::Result<String, ApiError> {
    let Some(tag) = tag else {
        return Ok(from_image.to_string());
    };
    if from_image.contains('@') {
        return Err(ApiError::from(Error::unsupported(
            "combining a digest-pinned fromImage with a tag is not implemented",
        )));
    }
    let last = from_image.rsplit('/').next().unwrap_or(from_image);
    if last.contains(':') {
        if from_image.ends_with(&format!(":{tag}")) {
            return Ok(from_image.to_string());
        }
        return Err(ApiError::bad_request(
            "fromImage already contains a tag that conflicts with the tag parameter",
        ));
    }
    Ok(format!("{from_image}:{tag}"))
}

fn registry_auth(request: &Request) -> std::result::Result<Option<RegistryAuth>, ApiError> {
    let value = request
        .headers()
        .iter()
        .find(|header| header.field.equiv("X-Registry-Auth"))
        .map(|header| header.value.as_str());
    match value {
        Some(value) => parse_registry_auth(value).map_err(ApiError::from),
        None => Ok(None),
    }
}

fn decode_image_name(parts: &[&str]) -> std::result::Result<String, ApiError> {
    let joined = parts.join("/");
    let decoded = percent_decode_path(&joined)?;
    let name = decoded.trim_start_matches('/');
    if name.is_empty() {
        return Err(ApiError::bad_request("image name must not be empty"));
    }
    Ok(name.to_string())
}

/// Percent-decode a path component. Unlike query parameters, `+` is a literal
/// plus sign in URL paths.
fn percent_decode_path(value: &str) -> std::result::Result<String, ApiError> {
    let bytes = value.as_bytes();
    let mut output = Vec::with_capacity(bytes.len());
    let mut index = 0;
    while index < bytes.len() {
        if bytes[index] == b'%' {
            if index + 2 >= bytes.len() {
                return Err(ApiError::bad_request("invalid URL escape in image name"));
            }
            let high = hex_digit(bytes[index + 1])
                .ok_or_else(|| ApiError::bad_request("invalid URL escape in image name"))?;
            let low = hex_digit(bytes[index + 2])
                .ok_or_else(|| ApiError::bad_request("invalid URL escape in image name"))?;
            output.push(high << 4 | low);
            index += 3;
        } else {
            output.push(bytes[index]);
            index += 1;
        }
    }
    String::from_utf8(output).map_err(|_| ApiError::bad_request("image name is not valid UTF-8"))
}

fn hex_digit(byte: u8) -> Option<u8> {
    match byte {
        b'0'..=b'9' => Some(byte - b'0'),
        b'a'..=b'f' => Some(byte - b'a' + 10),
        b'A'..=b'F' => Some(byte - b'A' + 10),
        _ => None,
    }
}

fn header(name: &str, value: &str) -> Header {
    Header::from_bytes(name.as_bytes(), value.as_bytes()).unwrap_or_else(|_| {
        Header::from_bytes(&b"X-Docker-RS"[..], &b"invalid-header"[..]).expect("static header")
    })
}

fn random_hex(bytes: usize) -> String {
    let mut buffer = vec![0u8; bytes];
    if getrandom::getrandom(&mut buffer).is_err() {
        let fallback = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .map(|elapsed| elapsed.as_nanos())
            .unwrap_or(0);
        for (index, byte) in buffer.iter_mut().enumerate() {
            *byte = (fallback >> ((index % 16) * 8)) as u8;
        }
    }
    let mut output = String::with_capacity(bytes * 2);
    for byte in buffer {
        let _ = write!(&mut output, "{byte:02x}");
    }
    output
}

fn generated_name() -> String {
    format!("docker-rs_{}", random_hex(6))
}

fn now_rfc3339() -> String {
    Utc::now().to_rfc3339_opts(SecondsFormat::Nanos, true)
}

fn operating_system() -> String {
    match std::env::consts::OS {
        "windows" => "Microsoft Windows".to_string(),
        "linux" => "Linux".to_string(),
        "macos" => "macOS".to_string(),
        other => other.to_string(),
    }
}

fn rust_arch() -> String {
    match std::env::consts::ARCH {
        "x86_64" => "amd64".to_string(),
        "aarch64" => "arm64".to_string(),
        "x86" => "386".to_string(),
        other => other.to_string(),
    }
}

/// Choose the broker replay cutoff. Moby's `loadBufferedEvents` returns no
/// history when both `since` and `until` are zero, and treats `until` alone
/// as "all buffered events up to until". `i64::MAX` is a replay cutoff above
/// any real timestamp, so no historical event is returned while the
/// subscription still registers atomically.
fn event_replay_since(options: &EventOptions) -> Option<i64> {
    match (options.since_nano, options.until_nano) {
        (Some(since), _) => Some(since),
        (None, Some(_)) => None,
        (None, None) => Some(i64::MAX),
    }
}

/// Long-lived `/events` response body. Events are serialized one JSON object
/// per line, exactly like Moby's `json.Encoder`.
struct EventStream {
    replay: VecDeque<Vec<u8>>,
    current: Vec<u8>,
    offset: usize,
    subscription: Option<Subscription>,
    filters: EventFilters,
    until_nano: Option<i64>,
}

impl EventStream {
    fn new(
        replay: Vec<Event>,
        subscription: Subscription,
        filters: EventFilters,
        until_nano: Option<i64>,
    ) -> Self {
        Self {
            replay: replay
                .into_iter()
                .filter(|event| filters.matches(event))
                .filter_map(encode_event)
                .collect(),
            current: Vec::new(),
            offset: 0,
            subscription: Some(subscription),
            filters,
            until_nano,
        }
    }
}

impl Read for EventStream {
    fn read(&mut self, output: &mut [u8]) -> io::Result<usize> {
        if output.is_empty() {
            return Ok(0);
        }
        loop {
            if self.offset < self.current.len() {
                let available = &self.current[self.offset..];
                let count = available.len().min(output.len());
                output[..count].copy_from_slice(&available[..count]);
                self.offset += count;
                return Ok(count);
            }
            self.current.clear();
            self.offset = 0;
            if let Some(next) = self.replay.pop_front() {
                self.current = next;
                continue;
            }
            let Some(subscription) = &self.subscription else {
                return Ok(0);
            };
            if let Some(until) = self.until_nano {
                if now_nano() >= until {
                    self.subscription = None;
                    return Ok(0);
                }
            }
            let wait = self
                .until_nano
                .map(|until| {
                    Duration::from_millis(
                        ((until - now_nano()).max(0) as u64 / 1_000_000).clamp(1, 50),
                    )
                })
                .unwrap_or(Duration::from_millis(100));
            match subscription.recv_timeout(wait) {
                Ok(event) => {
                    if self.until_nano.is_some_and(|until| event.time_nano > until) {
                        self.subscription = None;
                        return Ok(0);
                    }
                    if !self.filters.matches(&event) {
                        continue;
                    }
                    if let Some(encoded) = encode_event(event) {
                        self.current = encoded;
                    }
                }
                Err(std::sync::mpsc::RecvTimeoutError::Timeout) => continue,
                Err(std::sync::mpsc::RecvTimeoutError::Disconnected) => {
                    self.subscription = None;
                    return Ok(0);
                }
            }
        }
    }
}

fn encode_event(event: Event) -> Option<Vec<u8>> {
    let mut body = serde_json::to_vec(&event).ok()?;
    body.push(b'\n');
    Some(body)
}

fn now_nano() -> i64 {
    Utc::now()
        .timestamp_nanos_opt()
        .unwrap_or_else(|| Utc::now().timestamp() * 1_000_000_000)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::Arc;

    #[test]
    fn generated_names_are_valid_and_unique_enough() {
        let first = generated_name();
        let second = generated_name();
        assert!(first.starts_with("docker-rs_"));
        assert_ne!(first, second);
        docker_common::Name::new(&first).unwrap();
    }

    #[test]
    fn signal_numbers_map_to_names() {
        assert_eq!(parse_signal("15").unwrap(), Signal::Sigterm);
        assert_eq!(parse_signal("SIGKILL").unwrap(), Signal::Sigkill);
        assert!(parse_signal("999").is_err());
    }

    #[test]
    fn event_replay_cutoff_matches_moby_zero_time_rules() {
        let options = EventOptions {
            since_nano: None,
            until_nano: None,
            filters: EventFilters::default(),
        };
        assert_eq!(event_replay_since(&options), Some(i64::MAX));

        let options = EventOptions {
            since_nano: None,
            until_nano: Some(1),
            filters: EventFilters::default(),
        };
        assert_eq!(event_replay_since(&options), None);

        let options = EventOptions {
            since_nano: Some(42),
            until_nano: None,
            filters: EventFilters::default(),
        };
        assert_eq!(event_replay_since(&options), Some(42));
    }

    #[test]
    fn event_stream_replays_and_stops_at_until() {
        let broker = Arc::new(EventBroker::new());
        let (_, subscription) = broker.subscribe_since(None, 8).unwrap();
        let mut event = docker_events::event_now(
            docker_events::EventType::Container,
            docker_events::EventAction::Known(docker_events::KnownAction::Start),
            docker_events::attrs::container("abc", "alpine", "web"),
        );
        event.time_nano = now_nano() - 1_000_000;
        let mut stream = EventStream::new(
            vec![event],
            subscription,
            EventFilters::default(),
            Some(now_nano()),
        );
        let mut body = Vec::new();
        stream.read_to_end(&mut body).unwrap();
        assert!(String::from_utf8(body)
            .unwrap()
            .contains("\"action\":\"start\""));
    }
}
