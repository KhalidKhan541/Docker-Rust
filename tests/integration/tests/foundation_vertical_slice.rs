//! End-to-end Milestone A flow: configuration -> logging init -> container
//! creation -> lifecycle transitions -> events -> container log driver.
//!
//! This exercises every foundation crate through its public surface and the
//! crossing points the engine (Milestone B) will rely on.

use std::sync::Arc;

use docker_common::{ContainerId, ImageRef, Name};
use docker_configuration::{DaemonConfig, StorageDriverSetting};
use docker_container_model::config::{
    ContainerConfig, HostConfig, NetworkMode, PortKey, PortProto, Signal,
};
use docker_container_model::state::{validate_transition, LifecycleState, StateSnapshot};
use docker_container_model::store::{ContainerFilter, ContainerRecord, ContainerStore};
use docker_events::{attrs, event_now, EventAction, EventBroker, EventType, KnownAction};
use docker_logging::{JsonLinesDriver, LogDriver, LogEntry, LogStream};

/// Full happy path across all foundation crates.
#[test]
fn foundation_vertical_slice() {
    // 1. Configuration: defaults + JSON file layer + env layer.
    let base = DaemonConfig::default();
    let file: docker_configuration::FileConfig = serde_json::from_str(
        r#"{ "log-level": "debug", "storage-driver": "vfs", "labels": {"team": "eng"} }"#,
    )
    .unwrap();
    let cfg = base
        .overlay_file(file)
        .unwrap()
        .overlay_env(|k| (k == "DRC_DEBUG").then(|| "true".to_string()))
        .unwrap();
    cfg.validate()
        .unwrap_or_else(|e| panic!("config invalid: {e}"));
    assert_eq!(cfg.storage_driver, StorageDriverSetting::Vfs);
    assert!(cfg.debug);
    assert_eq!(cfg.labels.get("team").map(String::as_str), Some("eng"));

    // 2. Identify: typed IDs and references.
    let id = ContainerId::generate().unwrap();
    let img = ImageRef::parse("registry.example.com:5000/team/app:v1.2.3").unwrap();
    assert_eq!(img.registry(), Some("registry.example.com:5000"));
    let name = Name::new("web-1").unwrap();

    // 3. Container configuration validation + effective argv.
    let config = ContainerConfig {
        image: img.clone(),
        entrypoint: vec!["/bin/sh".into()],
        cmd: vec!["-c".into(), "for i in 1 2 3; do echo tick$i; done".into()],
        env: vec!["APP_MODE=serve".into()],
        labels: cfg.labels.clone(),
        working_dir: Some("/app".into()),
        user: None,
        hostname: Some(name.as_str().into()),
        tty: false,
        open_stdin: false,
        stop_signal: Signal::Sigterm,
        stop_timeout_secs: 10,
        exposed_ports: vec![PortKey {
            port: 8080,
            proto: PortProto::Tcp,
        }],
    };
    config
        .validate()
        .unwrap_or_else(|e| panic!("container config invalid: {e}"));
    let host_config = HostConfig {
        network_mode: NetworkMode::Bridge,
        ..HostConfig::default()
    };
    host_config
        .validate(false)
        .unwrap_or_else(|e| panic!("host config invalid: {e}"));

    assert_eq!(
        config.effective_argv().join(" "),
        "/bin/sh -c for i in 1 2 3; do echo tick$i; done"
    );

    // 4. Lifecycle transitions with conflict guards.
    assert_eq!(
        validate_transition(LifecycleState::Created, LifecycleState::Running).unwrap(),
        LifecycleState::Running
    );
    assert!(validate_transition(LifecycleState::Removing, LifecycleState::Running).is_err());

    // 5. Store + filters.
    let mut store = ContainerStore::new();
    let record = ContainerRecord {
        id: id.clone(),
        name: Some(name.as_str().into()),
        created_at: 1_700_000_000,
        config: config.clone(),
        state: StateSnapshot {
            status: LifecycleState::Running,
            exit_code: None,
            error_signal: None,
            started_at: Some(1_700_000_001),
            finished_at: None,
        },
        host_config: HostConfig::default(),
    };
    store.put(record).unwrap();
    assert_eq!(
        store.resolve(name.as_str()).unwrap().state.status,
        LifecycleState::Running
    );
    assert_eq!(
        store
            .list(&[ContainerFilter::Status("running".into())])
            .len(),
        1
    );
    assert_eq!(
        store
            .list(&[
                ContainerFilter::Status("exited".into()),
                ContainerFilter::Name("web-1".into())
            ])
            .len(),
        0
    );

    // 6. Event broker: subscribe, publish, receive with upstream wire shape.
    let broker = Arc::new(EventBroker::new());
    let sub = broker.subscribe(16).unwrap();
    let ev = event_now(
        EventType::Container,
        EventAction::Known(KnownAction::Start),
        attrs::container(id.as_str(), &img.to_string(), name.as_str()),
    );
    // Serialization must keep upstream key casing.
    let wire = serde_json::to_string(&ev).unwrap();
    assert!(wire.contains(r#"""#) && wire.contains("\"type\""));
    assert_eq!(broker.publish(ev), 1);
    let received = sub.recv_blocking().unwrap();
    assert_eq!(received.r#type, EventType::Container);
    assert_eq!(
        received.actor.attributes.get("name").map(String::as_str),
        Some("web-1")
    );

    // 7. Container log driver round trip.
    let dir = tempfile::tempdir().unwrap();
    let log_path = dir
        .path()
        .join("containers")
        .join(id.as_str())
        .join("container-logs.json");
    let mut driver = JsonLinesDriver::open(&log_path, 10).unwrap();
    let entry1 = LogEntry::now(LogStream::Stdout, "tick1");
    driver.write(&entry1).unwrap();
    let entry2 = LogEntry::now(LogStream::Stderr, "boom");
    driver.write(&entry2).unwrap();
    let raw = std::fs::read_to_string(&log_path).unwrap();
    let mut lines = raw.lines();
    let line1 = lines.next().unwrap();
    assert!(line1.contains(r#""log":"tick1""#));
    assert!(line1.contains(r#""stream":"stdout""#));

    // 8. Cleanup semantics: removing a record is a not-found on repeat.
    store.remove(&id).unwrap();
    assert_eq!(
        store.remove(&id).unwrap_err().kind(),
        docker_errors::ErrorKind::NotFound
    );
}

/// Failure path: config, validation, and store errors surface through the
/// shared taxonomy with stable kinds.
#[test]
fn foundation_error_paths() {
    // Bad storage driver from env -> unsupported naming what's broken.
    let cfg = DaemonConfig::default();
    let err = cfg
        .overlay_env(|k| (k == "DRC_STORAGE_DRIVER").then(|| "zfs".to_string()))
        .unwrap_err();
    assert_eq!(err.kind(), docker_errors::ErrorKind::Unsupported);
    assert!(err.message().contains("zfs"));

    // Ambiguous prefix -> invalid argument.
    let mut store = ContainerStore::new();
    let a = ContainerId::new(&"aa".repeat(32)).unwrap();
    let b = ContainerId::new(&"ab".repeat(32)).unwrap();
    let mk = |id: &ContainerId, name: &str| ContainerRecord {
        id: id.clone(),
        name: Some(name.into()),
        created_at: 1_700_000_000,
        config: ContainerConfig {
            image: ImageRef::parse("alpine").unwrap(),
            entrypoint: vec![],
            cmd: vec![],
            env: vec![],
            labels: Default::default(),
            working_dir: None,
            user: None,
            hostname: None,
            tty: false,
            open_stdin: false,
            stop_signal: Signal::Sigterm,
            stop_timeout_secs: 10,
            exposed_ports: vec![],
        },
        state: StateSnapshot {
            status: LifecycleState::Created,
            exit_code: None,
            error_signal: None,
            started_at: None,
            finished_at: None,
        },
        host_config: HostConfig::default(),
    };
    store.put(mk(&a, "one")).unwrap();
    store.put(mk(&b, "two")).unwrap();
    let err = store.resolve("a").unwrap_err();
    assert_eq!(err.kind(), docker_errors::ErrorKind::InvalidArgument);
    // Exact id wins over the ambiguity.
    assert_eq!(store.resolve(a.as_str()).unwrap().id, a);

    // Invalid image references never become containers.
    assert_eq!(
        ImageRef::parse("BadName").unwrap_err().kind(),
        docker_errors::ErrorKind::InvalidArgument
    );

    // Illegal transitions keep their conflict kind.
    assert_eq!(
        validate_transition(LifecycleState::Created, LifecycleState::Dead).ok(),
        Some(LifecycleState::Dead)
    );
    assert_eq!(
        validate_transition(LifecycleState::Dead, LifecycleState::Running)
            .unwrap_err()
            .kind(),
        docker_errors::ErrorKind::Conflict
    );
}
