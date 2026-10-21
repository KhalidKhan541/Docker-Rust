#![recursion_limit = "512"]

//! End-to-end Engine API tests over a real loopback listener.
//!
//! The client is intentionally tiny and dependency-free: it sends HTTP/1.1
//! and decodes `Content-Length`/chunked bodies. That keeps the API tests
//! exercising the actual `tiny_http` transport instead of a mock router.

use std::collections::BTreeMap;
use std::io::{BufRead, BufReader, Read, Write};
use std::net::{SocketAddr, TcpStream};
use std::sync::Arc;
use std::thread;
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use docker_api::{ApiServer, RunningServer};
use docker_daemon::{Daemon, SyntheticBehavior, SyntheticRuntime};
use docker_events::EventBroker;
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
    if status == 204 || status == 304 {
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
    let logs = http(addr, "GET", &format!("/containers/{id}/logs"), None, None);
    assert_eq!(logs.status, 501);
    assert!(logs.text().contains("not implemented"));

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
