#![recursion_limit = "512"]

//! End-to-end Engine API tests over a real loopback listener.
//!
//! The client is intentionally tiny and dependency-free: it sends HTTP/1.1
//! and decodes `Content-Length`/chunked bodies. That keeps the API tests
//! exercising the actual `tiny_http` transport instead of a mock router.

use std::collections::BTreeMap;
use std::fs;
use std::io::{BufRead, BufReader, Cursor, Read, Write};
use std::net::{SocketAddr, TcpStream};
use std::path::Path;
use std::sync::atomic::{AtomicBool, Ordering as AtomicOrdering};
use std::sync::Arc;
use std::thread;
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use docker_api::{ApiServer, RunningServer};
use docker_common::{Digest, ImageRef};
use docker_daemon::{Daemon, SyntheticBehavior, SyntheticRuntime};
use docker_events::EventBroker;
use docker_image::{
    Descriptor, EmptyObject, Image, ImageConfig, Manifest, Platform, RootFS,
    MEDIA_TYPE_IMAGE_CONFIG, MEDIA_TYPE_IMAGE_LAYER_GZIP, MEDIA_TYPE_IMAGE_MANIFEST,
};
use docker_image_store::{canonical_repo_digest, canonical_tag, ImageRecord, ImageStore};
use serde_json::{json, Value};

struct Response {
    status: u16,
    headers: BTreeMap<String, String>,
    body: Vec<u8>,
}

impl Response {
    fn json(&self) -> Value {
        serde_json::from_slice(&self.body).unwrap_or_else(|error| {
            panic!(
                "response was not JSON ({error}): {}",
                String::from_utf8_lossy(&self.body)
            )
        })
    }

    fn text(&self) -> String {
        String::from_utf8_lossy(&self.body).into_owned()
    }

    fn header(&self, name: &str) -> Option<&str> {
        self.headers
            .get(&name.to_ascii_lowercase())
            .map(String::as_str)
    }
}

fn http(
    addr: SocketAddr,
    method: &str,
    path: &str,
    body: Option<&[u8]>,
    content_type: Option<&str>,
) -> Response {
    let mut stream = TcpStream::connect(addr).expect("connect to API listener");
    stream
        .set_read_timeout(Some(Duration::from_secs(10)))
        .unwrap();
    stream
        .set_write_timeout(Some(Duration::from_secs(10)))
        .unwrap();
    let body = body.unwrap_or_default();
    let mut request =
        format!("{method} {path} HTTP/1.1\r\nHost: localhost\r\nConnection: close\r\n");
    if !body.is_empty() {
        request.push_str(&format!(
            "Content-Type: {}\r\nContent-Length: {}\r\n",
            content_type.unwrap_or("application/json"),
            body.len()
        ));
    }
    request.push_str("\r\n");
    stream.write_all(request.as_bytes()).unwrap();
    if !body.is_empty() {
        stream.write_all(body).unwrap();
    }
    stream.flush().unwrap();

    let mut reader = BufReader::new(stream);
    let mut line = String::new();
    reader.read_line(&mut line).unwrap();
    let mut parts = line.split_whitespace();
    let _http = parts.next();
    let status: u16 = parts.next().expect("status code").parse().unwrap();
    let mut headers = BTreeMap::new();
    loop {
        line.clear();
        reader.read_line(&mut line).unwrap();
        let line = line.trim_end_matches(['\r', '\n']);
        if line.is_empty() {
            break;
        }
        if let Some((name, value)) = line.split_once(':') {
            headers
                .entry(name.trim().to_ascii_lowercase())
                .or_insert_with(|| value.trim().to_string());
        }
    }
    let chunked = headers
        .get("transfer-encoding")
        .is_some_and(|value| value.to_ascii_lowercase().contains("chunked"));
    let content_length = headers
        .get("content-length")
        .and_then(|value| value.parse::<usize>().ok());
    let mut response_body = Vec::new();
    if method == "HEAD" || status == 204 || status == 304 {
        // No response body by HTTP semantics.
    } else if chunked {
        loop {
            let mut size_line = String::new();
            reader.read_line(&mut size_line).unwrap();
            let size_text = size_line
                .trim_end_matches(['\r', '\n'])
                .split(';')
                .next()
                .unwrap_or("");
            let size = usize::from_str_radix(size_text, 16)
                .unwrap_or_else(|_| panic!("invalid chunk size {size_text:?}"));
            if size == 0 {
                // Consume trailers.
                loop {
                    let mut trailer = String::new();
                    reader.read_line(&mut trailer).unwrap();
                    if trailer.trim_end_matches(['\r', '\n']).is_empty() {
                        break;
                    }
                }
                break;
            }
            let mut chunk = vec![0u8; size];
            reader.read_exact(&mut chunk).unwrap();
            response_body.extend_from_slice(&chunk);
            let mut crlf = [0u8; 2];
            reader.read_exact(&mut crlf).unwrap();
        }
    } else if let Some(length) = content_length {
        response_body.resize(length, 0);
        reader.read_exact(&mut response_body).unwrap();
    } else {
        reader.read_to_end(&mut response_body).unwrap();
    }
    Response {
        status,
        headers,
        body: response_body,
    }
}

fn spawn_api() -> (RunningServer, Arc<SyntheticRuntime>) {
    let runtime = Arc::new(SyntheticRuntime::with_behavior(
        SyntheticBehavior::running_until_killed(),
    ));
    let broker = Arc::new(EventBroker::new());
    let daemon = Arc::new(Daemon::spawn(runtime.clone(), broker.clone()).unwrap());
    let api = Arc::new(ApiServer::new(daemon, broker));
    let server = api.serve("127.0.0.1:0").unwrap();
    (server, runtime)
}

fn spawn_api_with_web_root(root: &Path) -> RunningServer {
    let runtime = Arc::new(SyntheticRuntime::with_behavior(
        SyntheticBehavior::running_until_killed(),
    ));
    let broker = Arc::new(EventBroker::new());
    let daemon = Arc::new(Daemon::spawn(runtime, broker.clone()).unwrap());
    let api = ApiServer::new(daemon, broker).with_web_root(root).unwrap();
    Arc::new(api).serve("127.0.0.1:0").unwrap()
}

fn create_body(image: &str, name_label: Option<&str>) -> Value {
    let mut labels = serde_json::Map::new();
    if let Some(label) = name_label {
        labels.insert("role".to_string(), Value::String(label.to_string()));
    }
    // This mirrors the field set the pinned Docker CLI sends. Zero-valued
    // fields must be accepted without silently requesting unimplemented
    // behavior.
    json!({
        "Hostname": "",
        "Domainname": "",
        "User": "",
        "AttachStdin": false,
        "AttachStdout": true,
        "AttachStderr": true,
        "ExposedPorts": null,
        "Tty": false,
        "OpenStdin": false,
        "StdinOnce": false,
        "Env": ["A=B"],
        "Cmd": ["/bin/sh", "-c", "echo hi"],
        "Image": image,
        "Volumes": null,
        "WorkingDir": "/",
        "Entrypoint": null,
        "OnBuild": null,
        "Labels": labels,
        "StopSignal": "",
        "StopTimeout": null,
        "Shell": null,
        "NetworkingConfig": {"EndpointsConfig": {"default": {}}},
        "HostConfig": {
            "Binds": null,
            "ContainerIDFile": "",
            "LogConfig": {"Type": "", "Config": {}},
            "NetworkMode": "none",
            "PortBindings": {},
            "RestartPolicy": {"Name": "no", "MaximumRetryCount": 0},
            "AutoRemove": false,
            "VolumeDriver": "",
            "VolumesFrom": null,
            "ConsoleSize": [0, 0],
            "Annotations": {},
            "CapAdd": null,
            "CapDrop": null,
            "CgroupnsMode": "",
            "Dns": [],
            "DnsOptions": [],
            "DnsSearch": [],
            "ExtraHosts": null,
            "GroupAdd": null,
            "IpcMode": "",
            "Cgroup": "",
            "Links": null,
            "OomScoreAdj": 0,
            "PidMode": "",
            "Privileged": false,
            "PublishAllPorts": false,
            "ReadonlyRootfs": false,
            "SecurityOpt": null,
            "UTSMode": "",
            "UsernsMode": "",
            "ShmSize": 0,
            "Runtime": "",
            "Isolation": "",
            "CpuShares": 0,
            "Memory": 0,
            "NanoCpus": 0,
            "CgroupParent": "",
            "BlkioWeight": 0,
            "BlkioWeightDevice": [],
            "BlkioDeviceReadBps": [],
            "BlkioDeviceWriteBps": [],
            "BlkioDeviceReadIOps": [],
            "BlkioDeviceWriteIOps": [],
            "CpuPeriod": 0,
            "CpuQuota": 0,
            "CpuRealtimePeriod": 0,
            "CpuRealtimeRuntime": 0,
            "CpusetCpus": "",
            "CpusetMems": "",
            "Devices": [],
            "DeviceCgroupRules": null,
            "DeviceRequests": [],
            "MemoryReservation": 0,
            "MemorySwap": 0,
            "MemorySwappiness": -1,
            "OomKillDisable": false,
            "PidsLimit": -1,
            "Ulimits": [],
            "CpuCount": 0,
            "CpuPercent": 0,
            "IOMaximumIOps": 0,
            "IOMaximumBandwidth": 0,
            "MaskedPaths": [],
            "ReadonlyPaths": [],
            "Mounts": [],
            "Init": null,
            "Tmpfs": {},
            "Sysctls": {},
            "StorageOpt": {}
        }
    })
}

fn create(addr: SocketAddr, name: &str, image: &str, role: Option<&str>) -> String {
    let body = serde_json::to_vec(&create_body(image, role)).unwrap();
    let response = http(
        addr,
        "POST",
        &format!("/v1.51/containers/create?name={name}"),
        Some(&body),
        Some("application/json"),
    );
    assert_eq!(response.status, 201, "create failed: {}", response.text());
    response.json()["Id"].as_str().unwrap().to_string()
}

fn wait_for_state(addr: SocketAddr, id: &str, expected: &str) -> Value {
    for _ in 0..100 {
        let response = http(addr, "GET", &format!("/containers/{id}/json"), None, None);
        assert_eq!(response.status, 200, "inspect failed: {}", response.text());
        let inspect = response.json();
        if inspect["State"]["Status"] == expected {
            return inspect;
        }
        thread::sleep(Duration::from_millis(10));
    }
    panic!("container {id} did not reach state {expected}");
}

fn encoded(value: &str) -> String {
    let mut output = String::new();
    for byte in value.bytes() {
        if byte.is_ascii_alphanumeric() || matches!(byte, b'-' | b'_' | b'.' | b'~') {
            output.push(byte as char);
        } else {
            output.push_str(&format!("%{byte:02X}"));
        }
    }
    output
}

fn now_seconds() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap()
        .as_secs()
}

#[test]
fn ping_version_gate_and_system_endpoints() {
    let (server, _) = spawn_api();
    let addr = server.addr();

    for path in ["/_ping", "/v1.24/_ping", "/v1.51/_ping"] {
        let response = http(addr, "GET", path, None, None);
        assert_eq!(response.status, 200, "{path}: {}", response.text());
        assert_eq!(response.text(), "OK");
        assert_eq!(response.header("api-version"), Some("1.51"));
        assert_eq!(response.header("ostype"), Some(std::env::consts::OS));
        assert!(response
            .header("server")
            .unwrap_or_default()
            .starts_with("Docker/"));
    }

    let too_old = http(addr, "GET", "/v1.23/_ping", None, None);
    assert_eq!(too_old.status, 400);
    assert_eq!(too_old.header("api-version"), Some("1.51"));
    assert!(too_old.text().contains("too old"));
    assert!(too_old.text().contains("1.24"));

    let too_new = http(addr, "GET", "/v1.52/_ping", None, None);
    assert_eq!(too_new.status, 400);
    assert!(too_new.text().contains("too new"));
    assert!(too_new.text().contains("1.51"));

    let version = http(addr, "GET", "/version", None, None);
    assert_eq!(version.status, 200);
    let version = version.json();
    assert_eq!(version["ApiVersion"], "1.51");
    assert_eq!(version["MinAPIVersion"], "1.24");
    assert_eq!(version["Version"], env!("CARGO_PKG_VERSION"));
    assert_eq!(version["Os"], std::env::consts::OS);

    let info = http(addr, "GET", "/info", None, None);
    assert_eq!(info.status, 200);
    let info = info.json();
    assert_eq!(info["Driver"], "docker-rs");
    assert_eq!(info["DefaultRuntime"], "synthetic");
    assert_eq!(info["Containers"], 0);
    assert!(info["Warnings"][0]
        .as_str()
        .unwrap()
        .contains("synthetic runtime"));

    let missing = http(addr, "GET", "/nope", None, None);
    assert_eq!(missing.status, 404);
    assert_eq!(missing.header("api-version"), Some("1.51"));
    assert_eq!(missing.json()["message"], "page not found");

    // Static serving is opt-in; without a web root `/` stays an API 404.
    assert_eq!(http(addr, "GET", "/", None, None).status, 404);
}

#[test]
fn serves_a_web_console_without_shadowing_api_paths() {
    let web = tempfile::tempdir().unwrap();
    fs::write(web.path().join("index.html"), b"<h1>docker-rs</h1>").unwrap();
    fs::write(web.path().join("console.html"), b"<h1>console</h1>").unwrap();
    fs::create_dir(web.path().join("assets")).unwrap();
    fs::write(web.path().join("assets/app.js"), b"console.log('ok');").unwrap();
    fs::create_dir(web.path().join("containers")).unwrap();
    fs::write(web.path().join("containers/json"), b"must not shadow").unwrap();
    fs::write(web.path().join("networks"), b"must not shadow").unwrap();
    fs::create_dir(web.path().join("v1.51")).unwrap();
    fs::write(web.path().join("v1.51/console.html"), b"versioned").unwrap();

    let server = spawn_api_with_web_root(web.path());
    let addr = server.addr();

    let index = http(addr, "GET", "/", None, None);
    assert_eq!(index.status, 200, "{}", index.text());
    assert_eq!(
        index.header("content-type"),
        Some("text/html; charset=utf-8")
    );
    assert_eq!(index.header("x-content-type-options"), Some("nosniff"));
    assert_eq!(index.text(), "<h1>docker-rs</h1>");

    let console = http(addr, "GET", "/console", None, None);
    assert_eq!(console.status, 200);
    assert_eq!(console.text(), "<h1>console</h1>");

    let js = http(addr, "GET", "/assets/app.js", None, None);
    assert_eq!(js.status, 200);
    assert_eq!(
        js.header("content-type"),
        Some("text/javascript; charset=utf-8")
    );
    assert_eq!(js.text(), "console.log('ok');");

    // API namespaces win over files with the same name.
    let containers = http(addr, "GET", "/containers/json", None, None);
    assert_eq!(containers.status, 200);
    assert!(containers.json().is_array());
    assert!(!containers.text().contains("must not shadow"));

    let networks = http(addr, "GET", "/networks", None, None);
    assert_eq!(networks.status, 404);
    assert!(!networks.text().contains("must not shadow"));

    // Version-prefixed paths are API paths even when the static tree has a
    // matching file.
    let versioned = http(addr, "GET", "/v1.51/console.html", None, None);
    assert_eq!(versioned.status, 404);
    assert!(!versioned.text().contains("versioned"));

    assert_eq!(http(addr, "GET", "/missing", None, None).status, 404);
    assert_eq!(
        http(addr, "GET", "/%2e%2e/Cargo.toml", None, None).status,
        404
    );

    let post = http(addr, "POST", "/console.html", None, None);
    assert_eq!(post.status, 405);
    assert_eq!(post.header("allow"), Some("GET, HEAD"));
    assert_eq!(http(addr, "POST", "/missing", None, None).status, 404);

    let head = http(addr, "HEAD", "/index.html", None, None);
    assert_eq!(head.status, 200);
    assert!(head.body.is_empty());
    assert_eq!(head.header("content-length"), Some("18"));
}

#[test]
fn container_lifecycle_over_http() {
    let (server, _) = spawn_api();
    let addr = server.addr();

    let body = serde_json::to_vec(&create_body("alpine:3.20", Some("web"))).unwrap();
    let created = http(
        addr,
        "POST",
        "/containers/create?name=web",
        Some(&body),
        Some("application/json"),
    );
    assert_eq!(created.status, 201, "{}", created.text());
    let created = created.json();
    let id = created["Id"].as_str().unwrap().to_string();
    assert_eq!(id.len(), 64);
    assert_eq!(created["Warnings"], json!([]));

    let inspect = http(addr, "GET", &format!("/containers/{id}/json"), None, None);
    assert_eq!(inspect.status, 200);
    let inspect = inspect.json();
    assert_eq!(inspect["Name"], "/web");
    assert_eq!(inspect["Config"]["Image"], "alpine:3.20");
    assert_eq!(
        inspect["Config"]["Cmd"],
        json!(["/bin/sh", "-c", "echo hi"])
    );
    assert_eq!(inspect["State"]["Status"], "created");
    assert_eq!(inspect["State"]["Running"], false);
    assert_eq!(inspect["HostConfig"]["NetworkMode"], "none");
    assert_eq!(inspect["HostConfig"]["MemorySwappiness"], -1);

    // Default list hides stopped containers; all=1 includes them.
    let default_list = http(addr, "GET", "/containers/json", None, None);
    assert_eq!(default_list.status, 200);
    assert_eq!(default_list.json(), json!([]));
    let all = http(addr, "GET", "/containers/json?all=1", None, None);
    assert_eq!(all.status, 200);
    let all = all.json();
    assert_eq!(all.as_array().unwrap().len(), 1);
    assert_eq!(all[0]["Names"], json!(["/web"]));
    assert_eq!(all[0]["State"], "created");
    assert_eq!(all[0]["Image"], "alpine:3.20");
    assert_eq!(all[0]["Labels"]["role"], "web");

    // A second start is a 304, exactly like Moby.
    assert_eq!(
        http(addr, "POST", &format!("/containers/{id}/start"), None, None).status,
        204
    );
    let running = wait_for_state(addr, &id, "running");
    assert_eq!(running["State"]["Running"], true);
    assert_eq!(all[0]["Status"], "Created");
    assert_eq!(
        http(addr, "POST", &format!("/containers/{id}/start"), None, None).status,
        304
    );

    let running_list = http(addr, "GET", "/containers/json", None, None);
    let running_list = running_list.json();
    assert_eq!(running_list.as_array().unwrap().len(), 1);
    assert_eq!(running_list[0]["State"], "running");
    assert!(running_list[0]["Status"]
        .as_str()
        .unwrap()
        .starts_with("Up"));

    assert_eq!(
        http(
            addr,
            "POST",
            &format!("/containers/{id}/stop?t=1"),
            None,
            None
        )
        .status,
        204
    );
    let exited = wait_for_state(addr, &id, "exited");
    assert_eq!(exited["State"]["Running"], false);
    assert_eq!(
        http(addr, "POST", &format!("/containers/{id}/stop"), None, None).status,
        304
    );

    // Start again, then kill with the numeric SIGKILL spelling.
    assert_eq!(
        http(addr, "POST", &format!("/containers/{id}/start"), None, None).status,
        204
    );
    wait_for_state(addr, &id, "running");
    let wait_id = id.clone();
    let waiter = thread::spawn(move || {
        http(
            addr,
            "POST",
            &format!("/containers/{wait_id}/wait?condition=not-running"),
            None,
            None,
        )
    });
    thread::sleep(Duration::from_millis(50));
    assert_eq!(
        http(
            addr,
            "POST",
            &format!("/containers/{id}/kill?signal=9"),
            None,
            None
        )
        .status,
        204
    );
    let waited = waiter.join().unwrap();
    assert_eq!(waited.status, 200, "{}", waited.text());
    assert_eq!(waited.json()["StatusCode"], 137);
    wait_for_state(addr, &id, "exited");

    assert_eq!(
        http(addr, "DELETE", &format!("/containers/{id}"), None, None).status,
        204
    );
    let gone = http(addr, "GET", &format!("/containers/{id}/json"), None, None);
    assert_eq!(gone.status, 404);
    assert!(gone.text().contains("no such container"));
}

#[test]
fn generated_names_filters_and_honest_errors() {
    let (server, _) = spawn_api();
    let addr = server.addr();

    let body = serde_json::to_vec(&create_body("alpine:3.20", Some("web"))).unwrap();
    let response = http(
        addr,
        "POST",
        "/containers/create",
        Some(&body),
        Some("application/json"),
    );
    assert_eq!(response.status, 201, "{}", response.text());
    let generated_id = response.json()["Id"].as_str().unwrap().to_string();
    let inspect = http(
        addr,
        "GET",
        &format!("/containers/{generated_id}/json"),
        None,
        None,
    )
    .json();
    let generated_name = inspect["Name"].as_str().unwrap();
    assert!(
        generated_name.starts_with("/docker-rs_"),
        "{generated_name}"
    );

    create(addr, "web-1", "alpine:3.20", Some("web"));
    create(addr, "batch-1", "alpine:3.20", None);

    let filters = encoded(r#"{"label":["role=web"]}"#);
    let filtered = http(
        addr,
        "GET",
        &format!("/containers/json?all=1&filters={filters}"),
        None,
        None,
    );
    assert_eq!(filtered.status, 200, "{}", filtered.text());
    let filtered = filtered.json();
    assert_eq!(filtered.as_array().unwrap().len(), 2);

    let status_filters = encoded(r#"{"status":["created"]}"#);
    let created_only = http(
        addr,
        "GET",
        &format!("/containers/json?all=1&filters={status_filters}"),
        None,
        None,
    );
    assert_eq!(created_only.status, 200);
    assert_eq!(created_only.json().as_array().unwrap().len(), 3);

    let name_filters = encoded(r#"{"name":["web-"]}"#);
    let by_name = http(
        addr,
        "GET",
        &format!("/containers/json?all=1&filters={name_filters}"),
        None,
        None,
    );
    assert_eq!(by_name.status, 200);
    let by_name = by_name.json();
    assert_eq!(by_name.as_array().unwrap().len(), 1);
    assert_eq!(by_name[0]["Names"], json!(["/web-1"]));

    let unsupported_filter = encoded(r#"{"ancestor":["alpine"]}"#);
    let unsupported = http(
        addr,
        "GET",
        &format!("/containers/json?filters={unsupported_filter}"),
        None,
        None,
    );
    assert_eq!(unsupported.status, 501);
    assert!(unsupported.text().contains("ancestor"));

    let mut bound = create_body("alpine:3.20", None);
    bound["HostConfig"]["Binds"] = json!(["/host:/container"]);
    let bound = http(
        addr,
        "POST",
        "/containers/create",
        Some(&serde_json::to_vec(&bound).unwrap()),
        Some("application/json"),
    );
    assert_eq!(bound.status, 501);
    assert!(bound.text().contains("Binds"));

    let id = generate_id(addr);
    // Moby's daemon/logs.go rejects requests that select no stream.
    let streamless = http(addr, "GET", &format!("/containers/{id}/logs"), None, None);
    assert_eq!(streamless.status, 400);
    assert!(streamless.text().contains("at least one stream"));
    // Both streams selected: created containers have no output yet, so the
    // response is 200 with an empty multiplexed stream.
    let empty = http(
        addr,
        "GET",
        &format!("/containers/{id}/logs?stdout=1&stderr=1"),
        None,
        None,
    );
    assert_eq!(empty.status, 200);
    assert_eq!(empty.text().len(), 0);

    let bad_signal = http(
        addr,
        "POST",
        &format!("/containers/{id}/kill?signal=999"),
        None,
        None,
    );
    assert_eq!(bad_signal.status, 400);
    assert!(bad_signal.text().contains("999"));

    assert_eq!(
        http(addr, "POST", &format!("/containers/{id}/start"), None, None).status,
        204
    );
    wait_for_state(addr, &id, "running");
    let infinite_timeout = http(
        addr,
        "POST",
        &format!("/containers/{id}/stop?t=-1"),
        None,
        None,
    );
    assert_eq!(infinite_timeout.status, 501);
    assert!(infinite_timeout.text().contains("wait forever"));
    let conflict = http(addr, "DELETE", &format!("/containers/{id}"), None, None);
    assert_eq!(conflict.status, 409);
    let forced = http(
        addr,
        "DELETE",
        &format!("/containers/{id}?force=1"),
        None,
        None,
    );
    assert_eq!(forced.status, 204);
}

fn generate_id(addr: SocketAddr) -> String {
    let body = serde_json::to_vec(&create_body("alpine:3.20", None)).unwrap();
    let response = http(
        addr,
        "POST",
        "/containers/create",
        Some(&body),
        Some("application/json"),
    );
    assert_eq!(response.status, 201, "{}", response.text());
    response.json()["Id"].as_str().unwrap().to_string()
}

#[test]
fn events_replay_filtering_and_live_streaming() {
    let (server, _) = spawn_api();
    let addr = server.addr();
    let id = create(addr, "evented", "alpine:3.20", Some("web"));
    assert_eq!(
        http(addr, "POST", &format!("/containers/{id}/start"), None, None).status,
        204
    );
    wait_for_state(addr, &id, "running");

    let filters = encoded(r#"{"type":["container"],"container":["evented"]}"#);
    let until = now_seconds() + 1;
    let replay = http(
        addr,
        "GET",
        &format!("/events?since=0&until={until}&filters={filters}"),
        None,
        None,
    );
    assert_eq!(replay.status, 200, "{}", replay.text());
    let lines: Vec<Value> = replay
        .text()
        .lines()
        .filter(|line| !line.trim().is_empty())
        .map(|line| serde_json::from_str(line).unwrap())
        .collect();
    assert!(lines.len() >= 2, "expected create/start events: {lines:?}");
    assert!(lines.iter().any(|event| event["action"] == "create"));
    assert!(lines.iter().any(|event| event["action"] == "start"));
    assert!(lines.iter().all(|event| event["type"] == "container"));
    assert!(lines
        .iter()
        .all(|event| event["actor"]["attributes"]["name"] == "evented"));
    assert!(lines.iter().all(|event| event["actor"]["ID"] == id));

    // Live stream: open the response first, publish a create from another
    // connection, then let `until` close the stream.
    let live_until = now_seconds() + 2;
    let live_addr = addr;
    let live = thread::spawn(move || {
        http(
            live_addr,
            "GET",
            &format!("/events?since=0&until={live_until}"),
            None,
            None,
        )
    });
    thread::sleep(Duration::from_millis(100));
    let _live_id = create(addr, "late", "alpine:3.20", None);
    let live = live.join().unwrap();
    assert_eq!(live.status, 200);
    assert!(live.text().contains(r#""action":"create""#));
    assert!(live.text().contains(r#""name":"late""#));
}

fn digest(byte: u8) -> Digest {
    Digest::parse(&format!("sha256:{}", format!("{byte:02x}").repeat(32))).unwrap()
}

fn sample_image() -> ImageRecord {
    let reference = ImageRef::parse("alpine:3.20").unwrap();
    let mut config = ImageConfig {
        cmd: vec!["/bin/sh".into()],
        entrypoint: vec!["/entry".into()],
        env: vec!["PATH=/usr/bin".into()],
        working_dir: "/work".into(),
        user: "1000".into(),
        stop_signal: "SIGINT".into(),
        ..ImageConfig::default()
    };
    config.labels.insert("org.example".into(), "test".into());
    config.exposed_ports.insert("8080/tcp".into(), EmptyObject);
    ImageRecord {
        id: digest(0xaa),
        manifest_digest: digest(0xbb),
        manifest_digests: vec![digest(0xbb)],
        tags: [canonical_tag(&reference).unwrap()].into_iter().collect(),
        repo_digests: [canonical_repo_digest(&reference, &digest(0xbb))]
            .into_iter()
            .collect(),
        created: 1_700_000_000,
        pulled_at: 1_700_000_001,
        size: 200,
        config_size: 77,
        platform: Platform {
            os: "linux".into(),
            architecture: "amd64".into(),
            ..Platform::default()
        },
        config: Image {
            architecture: "amd64".into(),
            os: "linux".into(),
            config,
            rootfs: RootFS::layers(vec![digest(0xcc)]),
            ..Image::default()
        },
        layers: vec![Descriptor {
            media_type: MEDIA_TYPE_IMAGE_LAYER_GZIP.into(),
            digest: digest(0xdd),
            size: 123,
            ..Descriptor::default()
        }],
    }
}

fn spawn_api_with_store(seed: impl FnOnce(&ImageStore)) -> (RunningServer, tempfile::TempDir) {
    let runtime = Arc::new(SyntheticRuntime::with_behavior(
        SyntheticBehavior::running_until_killed(),
    ));
    let broker = Arc::new(EventBroker::new());
    let daemon = Arc::new(Daemon::spawn(runtime, broker.clone()).unwrap());
    let directory = tempfile::tempdir().unwrap();
    let store = ImageStore::open(directory.path().join("images")).unwrap();
    seed(&store);
    let api = Arc::new(ApiServer::new(daemon, broker).with_image_store(Arc::new(store)));
    let server = api.serve("127.0.0.1:0").unwrap();
    (server, directory)
}

fn spawn_api_with_images() -> (RunningServer, tempfile::TempDir) {
    spawn_api_with_store(|store| {
        store.insert(sample_image()).unwrap();
    })
}

/// A digest-consistent image whose config, manifest and layer are present in
/// the content store, unlike `sample_image()`. The layer is a real gzip tar so
/// save/load can verify its diffID.
fn insert_saveable_image(store: &ImageStore) -> ImageRecord {
    let mut tar = tar::Builder::new(Vec::new());
    let mut header = tar::Header::new_gnu();
    header.set_size(6);
    header.set_mode(0o644);
    header.set_cksum();
    tar.append_data(&mut header, "hello.txt", &b"hello\n"[..])
        .unwrap();
    let tar_bytes = tar.into_inner().unwrap();
    let diff_id = Digest::sha256(&tar_bytes);

    let mut encoder = flate2::write::GzEncoder::new(Vec::new(), flate2::Compression::default());
    encoder.write_all(&tar_bytes).unwrap();
    let layer = encoder.finish().unwrap();
    let layer_digest = Digest::sha256(&layer);

    let config = Image {
        architecture: "amd64".into(),
        os: "linux".into(),
        config: ImageConfig {
            cmd: vec!["/hello".into()],
            env: vec!["PATH=/bin".into()],
            labels: [("saveable".to_string(), "yes".to_string())]
                .into_iter()
                .collect(),
            ..ImageConfig::default()
        },
        rootfs: RootFS::layers(vec![diff_id]),
        ..Image::default()
    };
    let config_bytes = serde_json::to_vec(&config).unwrap();
    let config_digest = Digest::sha256(&config_bytes);
    store
        .blob_store()
        .put_verified(&config_digest, &config_bytes)
        .unwrap();

    let manifest = Manifest::new(
        Descriptor {
            media_type: MEDIA_TYPE_IMAGE_CONFIG.into(),
            digest: config_digest.clone(),
            size: config_bytes.len() as i64,
            ..Descriptor::default()
        },
        vec![Descriptor {
            media_type: MEDIA_TYPE_IMAGE_LAYER_GZIP.into(),
            digest: layer_digest.clone(),
            size: layer.len() as i64,
            ..Descriptor::default()
        }],
    );
    let manifest_bytes = serde_json::to_vec(&manifest).unwrap();
    let manifest_digest = store.blob_store().put(&manifest_bytes).unwrap().0;
    store
        .blob_store()
        .put_verified(&layer_digest, &layer)
        .unwrap();

    let reference = ImageRef::parse("saveable:1.0").unwrap();
    let record = ImageRecord {
        id: config_digest,
        manifest_digest: manifest_digest.clone(),
        manifest_digests: vec![manifest_digest],
        tags: [canonical_tag(&reference).unwrap()].into_iter().collect(),
        repo_digests: Default::default(),
        created: 1_700_000_000,
        pulled_at: 1_700_000_001,
        size: (config_bytes.len() + layer.len()) as i64,
        config_size: config_bytes.len() as i64,
        platform: Platform {
            os: "linux".into(),
            architecture: "amd64".into(),
            ..Platform::default()
        },
        config,
        layers: vec![Descriptor {
            media_type: MEDIA_TYPE_IMAGE_LAYER_GZIP.into(),
            digest: layer_digest,
            size: layer.len() as i64,
            ..Descriptor::default()
        }],
    };
    store.insert(record.clone()).unwrap();
    record
}

#[test]
fn image_list_inspect_tag_history_and_delete() {
    let (server, _directory) = spawn_api_with_images();
    let addr = server.addr();

    let list = http(addr, "GET", "/images/json", None, None);
    assert_eq!(list.status, 200, "{}", list.text());
    let list = list.json();
    assert_eq!(list.as_array().unwrap().len(), 1);
    let id = digest(0xaa).full();
    assert_eq!(list[0]["Id"], id);
    assert_eq!(list[0]["RepoTags"], json!(["alpine:3.20"]));
    assert_eq!(
        list[0]["RepoDigests"],
        json!([format!("alpine@{}", digest(0xbb).full())])
    );
    assert_eq!(list[0]["Size"], 200);
    assert_eq!(list[0]["Containers"], -1);
    assert_eq!(list[0]["Labels"]["org.example"], "test");

    for path in [
        "/images/alpine:3.20/json".to_string(),
        "/images/alpine%3A3.20/json".to_string(),
        format!("/images/{}/json", &id[..16]),
    ] {
        let inspect = http(addr, "GET", &path, None, None);
        assert_eq!(inspect.status, 200, "{path}: {}", inspect.text());
        let inspect = inspect.json();
        assert_eq!(inspect["Id"], id);
        assert_eq!(inspect["Architecture"], "amd64");
        assert_eq!(inspect["Os"], "linux");
        assert_eq!(inspect["Config"]["Cmd"], json!(["/bin/sh"]));
        assert_eq!(inspect["RootFS"]["Layers"], json!([digest(0xcc).full()]));
        assert_eq!(inspect["GraphDriver"]["Name"], "docker-rs");
    }

    let tagged = http(
        addr,
        "POST",
        "/images/alpine:3.20/tag?repo=team/app&tag=dev",
        None,
        None,
    );
    assert_eq!(tagged.status, 201, "{}", tagged.text());
    let by_slash_name = http(addr, "GET", "/images/team/app:dev/json", None, None);
    assert_eq!(by_slash_name.status, 200, "{}", by_slash_name.text());
    assert_eq!(by_slash_name.json()["Id"], id);

    let history = http(addr, "GET", "/images/alpine:3.20/history", None, None);
    assert_eq!(history.status, 200, "{}", history.text());
    assert!(history.json().is_array());

    let untag = http(addr, "DELETE", "/images/alpine:3.20", None, None);
    assert_eq!(untag.status, 200, "{}", untag.text());
    assert_eq!(untag.json(), json!([{"Untagged": "alpine:3.20"}]));
    let still_there = http(addr, "GET", "/images/team/app:dev/json", None, None);
    assert_eq!(still_there.status, 200);

    let delete = http(addr, "DELETE", "/images/team/app:dev?force=1", None, None);
    assert_eq!(delete.status, 200, "{}", delete.text());
    let delete = delete.json();
    assert_eq!(delete[0], json!({"Untagged": "team/app:dev"}));
    assert_eq!(delete[1], json!({"Deleted": id}));
    assert_eq!(
        http(addr, "GET", "/images/json", None, None).json(),
        json!([])
    );

    let missing = http(addr, "GET", "/images/alpine:3.20/json", None, None);
    assert_eq!(missing.status, 404);
    assert!(missing.text().contains("No such image"));
}

fn archive_entries(bytes: &[u8]) -> BTreeMap<String, Vec<u8>> {
    let mut entries = BTreeMap::new();
    let mut archive = tar::Archive::new(Cursor::new(bytes));
    for entry in archive.entries().unwrap() {
        let mut entry = entry.unwrap();
        let path = entry.path().unwrap().to_string_lossy().into_owned();
        let mut body = Vec::new();
        entry.read_to_end(&mut body).unwrap();
        entries.insert(path, body);
    }
    entries
}

#[test]
fn image_save_and_load_round_trip_through_the_engine_api() {
    let mut expected = None;
    let (source, _source_directory) = spawn_api_with_store(|store| {
        expected = Some(insert_saveable_image(store));
    });
    let expected = expected.unwrap();
    let source_addr = source.addr();

    let saved = http(source_addr, "GET", "/images/saveable:1.0/get", None, None);
    assert_eq!(saved.status, 200, "{}", saved.text());
    assert_eq!(saved.header("content-type"), Some("application/x-tar"));
    let entries = archive_entries(&saved.body);
    for required in ["oci-layout", "index.json", "manifest.json"] {
        assert!(entries.contains_key(required), "missing {required}");
    }
    let index: Value = serde_json::from_slice(&entries["index.json"]).unwrap();
    assert_eq!(
        index["manifests"][0]["annotations"]["io.containerd.image.name"],
        "docker.io/library/saveable:1.0"
    );
    assert_eq!(
        index["manifests"][0]["annotations"]["org.opencontainers.image.ref.name"],
        "1.0"
    );
    assert_eq!(
        index["manifests"][0]["digest"],
        expected.manifest_digest.full()
    );
    let manifest: Value = serde_json::from_slice(&entries["manifest.json"]).unwrap();
    assert_eq!(manifest[0]["RepoTags"], json!(["saveable:1.0"]));
    assert_eq!(
        manifest[0]["Config"],
        format!("blobs/sha256/{}", expected.id.hex())
    );
    assert!(entries.contains_key(&format!("blobs/sha256/{}", expected.layers[0].digest.hex())));

    let saved_by_query = http(
        source_addr,
        "GET",
        "/images/get?names=saveable:1.0",
        None,
        None,
    );
    assert_eq!(saved_by_query.status, 200, "{}", saved_by_query.text());
    assert_eq!(saved_by_query.body, saved.body);

    let platform = encoded(r#"{"architecture":"amd64","os":"linux"}"#);
    let filtered = http(
        source_addr,
        "GET",
        &format!("/images/saveable:1.0/get?platform={platform}"),
        None,
        None,
    );
    assert_eq!(filtered.status, 200, "{}", filtered.text());
    let wrong_platform = encoded(r#"{"architecture":"arm64","os":"linux"}"#);
    let filtered = http(
        source_addr,
        "GET",
        &format!("/images/saveable:1.0/get?platform={wrong_platform}"),
        None,
        None,
    );
    assert_eq!(filtered.status, 400, "{}", filtered.text());
    let repeated = http(
        source_addr,
        "GET",
        &format!("/images/saveable:1.0/get?platform={platform}&platform={platform}"),
        None,
        None,
    );
    assert_eq!(repeated.status, 400, "{}", repeated.text());
    assert!(repeated.text().contains("multiple platform"));
    let missing = http(source_addr, "GET", "/images/does-not-exist/get", None, None);
    assert_eq!(missing.status, 404, "{}", missing.text());
    let no_names = http(source_addr, "GET", "/images/get", None, None);
    assert_eq!(no_names.status, 400, "{}", no_names.text());

    let (target, _target_directory) = spawn_api_with_store(|_| {});
    let target_addr = target.addr();
    let loaded = http(
        target_addr,
        "POST",
        "/images/load",
        Some(&saved.body),
        Some("application/x-tar"),
    );
    assert_eq!(loaded.status, 200, "{}", loaded.text());
    let messages: Vec<Value> = loaded
        .text()
        .lines()
        .map(|line| serde_json::from_str(line).unwrap())
        .collect();
    assert!(messages.iter().any(|message| message["stream"]
        .as_str()
        .unwrap_or_default()
        .contains("Loaded image: saveable:1.0")));

    let list = http(target_addr, "GET", "/images/json", None, None).json();
    assert_eq!(list.as_array().unwrap().len(), 1);
    assert_eq!(list[0]["Id"], expected.id.full());
    assert_eq!(list[0]["RepoTags"], json!(["saveable:1.0"]));
    assert_eq!(list[0]["Size"], expected.size);
    assert_eq!(list[0]["Labels"]["saveable"], "yes");
    let inspect = http(target_addr, "GET", "/images/saveable:1.0/json", None, None);
    assert_eq!(inspect.status, 200, "{}", inspect.text());
    assert_eq!(
        inspect.json()["RootFS"]["Layers"],
        json!([expected.config.rootfs.diff_ids[0].full()])
    );

    let re_saved = http(target_addr, "GET", "/images/saveable:1.0/get", None, None);
    assert_eq!(re_saved.status, 200, "{}", re_saved.text());
    assert_eq!(re_saved.body, saved.body);

    let empty = http(
        target_addr,
        "POST",
        "/images/load",
        None,
        Some("application/x-tar"),
    );
    assert_eq!(empty.status, 400, "{}", empty.text());
    assert!(empty.text().contains("request body is empty"));

    let mut gzip = flate2::write::GzEncoder::new(Vec::new(), flate2::Compression::default());
    gzip.write_all(&saved.body).unwrap();
    let gzip = gzip.finish().unwrap();
    let (gzip_target, _gzip_directory) = spawn_api_with_store(|_| {});
    let loaded = http(
        gzip_target.addr(),
        "POST",
        "/images/load",
        Some(&gzip),
        Some("application/gzip"),
    );
    assert_eq!(loaded.status, 200, "{}", loaded.text());
    assert!(loaded.text().contains("Loaded image: saveable:1.0"));
}

#[test]
fn container_create_requires_a_local_image_and_inherits_image_defaults() {
    let (server, _directory) = spawn_api_with_images();
    let addr = server.addr();

    let missing = serde_json::to_vec(&json!({"Image": "missing:latest"})).unwrap();
    let response = http(
        addr,
        "POST",
        "/containers/create",
        Some(&missing),
        Some("application/json"),
    );
    assert_eq!(response.status, 404, "{}", response.text());
    assert!(response.text().contains("No such image"));

    let body = serde_json::to_vec(&json!({
        "Image": "alpine:3.20",
        "Env": ["EXTRA=1", "PATH=/override"],
        "Labels": {"role": "web"},
        "ExposedPorts": {"9090/tcp": {}},
        "Cmd": []
    }))
    .unwrap();
    let created = http(
        addr,
        "POST",
        "/containers/create?name=from-image",
        Some(&body),
        Some("application/json"),
    );
    assert_eq!(created.status, 201, "{}", created.text());
    let id = created.json()["Id"].as_str().unwrap().to_string();

    let inspect = http(addr, "GET", &format!("/containers/{id}/json"), None, None);
    assert_eq!(inspect.status, 200, "{}", inspect.text());
    let config = &inspect.json()["Config"];
    assert_eq!(config["Cmd"], json!(["/bin/sh"]));
    assert_eq!(config["Entrypoint"], json!(["/entry"]));
    assert_eq!(config["Env"], json!(["PATH=/override", "EXTRA=1"]));
    assert_eq!(config["Labels"]["org.example"], "test");
    assert_eq!(config["Labels"]["role"], "web");
    assert_eq!(config["WorkingDir"], "/work");
    assert_eq!(config["User"], "1000");
    assert_eq!(config["StopSignal"], "SIGINT");
    assert_eq!(config["ExposedPorts"]["8080/tcp"], json!({}));
    assert_eq!(config["ExposedPorts"]["9090/tcp"], json!({}));
}

#[test]
fn image_endpoints_are_honest_without_a_store_and_for_unimplemented_features() {
    let (server, _) = spawn_api();
    let addr = server.addr();

    let list = http(addr, "GET", "/images/json", None, None);
    assert_eq!(list.status, 501);
    assert!(list.text().contains("image store is not configured"));

    let (server, _directory) = spawn_api_with_images();
    let addr = server.addr();
    let source = http(
        addr,
        "POST",
        "/images/create?fromImage=alpine&fromSrc=somewhere",
        None,
        None,
    );
    assert_eq!(source.status, 501, "{}", source.text());
    assert!(source.text().contains("from a source"), "{}", source.text());
    let prune = http(addr, "POST", "/images/prune", None, None);
    assert_eq!(prune.status, 501);
    let filters = encoded(r#"{"reference":["alpine"]}"#);
    let unsupported = http(
        addr,
        "GET",
        &format!("/images/json?filters={filters}"),
        None,
        None,
    );
    assert_eq!(unsupported.status, 501);
    assert!(unsupported.text().contains("filters"));
}

/// Minimal OCI registry serving one manifest/config/layer pair over loopback.
/// This exercises the real registry client, blob verification and pull status
/// stream without reaching the public internet.
struct RegistryFixture {
    host: String,
    stop: Arc<AtomicBool>,
    handle: Option<thread::JoinHandle<()>>,
}

impl Drop for RegistryFixture {
    fn drop(&mut self) {
        self.stop.store(true, AtomicOrdering::Relaxed);
        if let Some(handle) = self.handle.take() {
            let _ = handle.join();
        }
    }
}

fn registry_fixture() -> RegistryFixture {
    let layer = b"not-a-real-tarball".to_vec();
    let layer_digest = Digest::sha256(&layer);
    let diff_id = Digest::sha256(b"uncompressed-layer");
    let config = serde_json::to_vec(&json!({
        "created": "2024-01-02T03:04:05Z",
        "architecture": "amd64",
        "os": "linux",
        "config": {
            "Cmd": ["/fixture"],
            "Env": ["PATH=/fixture/bin"],
            "Labels": {"fixture": "yes"}
        },
        "rootfs": {
            "type": "layers",
            "diff_ids": [diff_id.full()]
        },
        "history": []
    }))
    .unwrap();
    let config_digest = Digest::sha256(&config);
    let manifest = serde_json::to_vec(&json!({
        "schemaVersion": 2,
        "mediaType": MEDIA_TYPE_IMAGE_MANIFEST,
        "config": {
            "mediaType": MEDIA_TYPE_IMAGE_CONFIG,
            "digest": config_digest.full(),
            "size": config.len()
        },
        "layers": [{
            "mediaType": MEDIA_TYPE_IMAGE_LAYER_GZIP,
            "digest": layer_digest.full(),
            "size": layer.len()
        }]
    }))
    .unwrap();
    let manifest_digest = Digest::sha256(&manifest);

    let server = tiny_http::Server::http("127.0.0.1:0").expect("bind registry fixture");
    let address = server.server_addr().to_ip().expect("TCP registry");
    let stop = Arc::new(AtomicBool::new(false));
    let thread_stop = Arc::clone(&stop);
    let handle = thread::spawn(move || {
        while !thread_stop.load(AtomicOrdering::Relaxed) {
            let request = match server.recv_timeout(Duration::from_millis(50)) {
                Ok(Some(request)) => request,
                Ok(None) => continue,
                Err(_) => break,
            };
            let path = request
                .url()
                .split('?')
                .next()
                .unwrap_or_default()
                .to_string();
            let response = if path == "/v2/" {
                tiny_http::Response::from_data(Vec::new())
            } else if path == "/v2/team/app/manifests/latest"
                || path == "/v2/team/app/manifests/dev"
                || path == format!("/v2/team/app/manifests/{}", manifest_digest.full())
            {
                tiny_http::Response::from_data(manifest.clone())
                    .with_header(
                        tiny_http::Header::from_bytes("Content-Type", MEDIA_TYPE_IMAGE_MANIFEST)
                            .expect("static content type"),
                    )
                    .with_header(
                        tiny_http::Header::from_bytes(
                            "Docker-Content-Digest",
                            manifest_digest.full(),
                        )
                        .expect("digest header"),
                    )
            } else if path == format!("/v2/team/app/blobs/{}", config_digest.full()) {
                tiny_http::Response::from_data(config.clone()).with_header(
                    tiny_http::Header::from_bytes("Content-Type", MEDIA_TYPE_IMAGE_CONFIG)
                        .expect("static content type"),
                )
            } else if path == format!("/v2/team/app/blobs/{}", layer_digest.full()) {
                tiny_http::Response::from_data(layer.clone()).with_header(
                    tiny_http::Header::from_bytes("Content-Type", MEDIA_TYPE_IMAGE_LAYER_GZIP)
                        .expect("static content type"),
                )
            } else {
                tiny_http::Response::from_data(Vec::new()).with_status_code(404)
            };
            let _ = request.respond(response);
        }
    });
    RegistryFixture {
        host: address.to_string(),
        stop,
        handle: Some(handle),
    }
}

#[test]
fn image_pull_uses_the_registry_client_and_persists_metadata() {
    let fixture = registry_fixture();
    let directory = tempfile::tempdir().unwrap();
    let store = ImageStore::open(directory.path().join("images")).unwrap();
    let runtime = Arc::new(SyntheticRuntime::with_behavior(
        SyntheticBehavior::running_until_killed(),
    ));
    let broker = Arc::new(EventBroker::new());
    let daemon = Arc::new(Daemon::spawn(runtime, broker.clone()).unwrap());
    let api = Arc::new(ApiServer::new(daemon, broker).with_image_store(Arc::new(store)));
    let server = api.serve("127.0.0.1:0").unwrap();
    let addr = server.addr();

    let from_image = encoded(&format!("{}/team/app", fixture.host));
    let platform = encoded("linux/amd64");
    let path = format!("/images/create?fromImage={from_image}&tag=dev&platform={platform}");
    let pulled = http(addr, "POST", &path, None, None);
    assert_eq!(pulled.status, 200, "{}", pulled.text());
    let messages: Vec<Value> = pulled
        .text()
        .lines()
        .map(|line| serde_json::from_str(line).unwrap())
        .collect();
    assert!(messages.iter().any(|message| message["status"]
        .as_str()
        .unwrap_or_default()
        .starts_with("Pulling from ")));
    assert!(messages.iter().any(|message| message["status"]
        .as_str()
        .unwrap_or_default()
        .contains("Downloaded newer image for")));

    let list = http(addr, "GET", "/images/json", None, None).json();
    assert_eq!(list.as_array().unwrap().len(), 1);
    assert_eq!(
        list[0]["RepoTags"],
        json!([format!("{}/team/app:dev", fixture.host)])
    );
    let inspect = http(
        addr,
        "GET",
        &format!("/images/{}/team/app:dev/json", fixture.host),
        None,
        None,
    );
    assert_eq!(inspect.status, 200, "{}", inspect.text());
    let inspect = inspect.json();
    assert_eq!(inspect["Config"]["Cmd"], json!(["/fixture"]));
    assert_eq!(
        inspect["RootFS"]["Layers"],
        json!([Digest::sha256(b"uncompressed-layer").full()])
    );

    let again = http(addr, "POST", &path, None, None);
    assert_eq!(again.status, 200, "{}", again.text());
    assert!(again.text().contains("Image is up to date for"));

    drop(server);
    let reopened = ImageStore::open(directory.path().join("images")).unwrap();
    assert_eq!(reopened.list(false).len(), 1);
    assert_eq!(
        reopened
            .resolve(&format!("{}/team/app:dev", fixture.host))
            .unwrap()
            .config
            .config
            .cmd,
        vec!["/fixture".to_string()]
    );
}
