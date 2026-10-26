//! Container records must survive a daemon restart, including the honest
//! handling of processes that cannot be adopted.

use std::sync::Arc;
use std::time::Duration;

use docker_common::ImageRef;
use docker_container_model::config::{ContainerConfig, HostConfig, Signal};
use docker_container_model::state::LifecycleState;
use docker_daemon::{CreateRequest, Daemon, Runtime, SyntheticBehavior, SyntheticRuntime};
use docker_events::EventBroker;

fn config() -> ContainerConfig {
    ContainerConfig {
        image: ImageRef::parse("alpine:latest").unwrap(),
        entrypoint: vec![],
        cmd: vec!["sleep".into(), "100".into()],
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
    }
}

fn spawn(data_root: &std::path::Path, broker: Arc<EventBroker>) -> Daemon {
    let runtime: Arc<dyn Runtime> = Arc::new(SyntheticRuntime::with_behavior(
        SyntheticBehavior::running_until_killed(),
    ));
    Daemon::spawn_persistent(runtime, broker, data_root).unwrap()
}

#[test]
fn container_records_survive_a_restart() {
    let temp = tempfile::tempdir().unwrap();
    let broker = Arc::new(EventBroker::new());

    let daemon = spawn(temp.path(), Arc::clone(&broker));
    let created = daemon
        .create(
            CreateRequest::new(config())
                .with_name("persisted")
                .with_host_config(HostConfig::default()),
        )
        .unwrap();
    let id = created.id.clone();
    let record = temp
        .path()
        .join("containers")
        .join(id.as_str())
        .join("container.json");
    assert!(record.is_file(), "container record was not written");
    drop(daemon);

    let daemon = spawn(temp.path(), Arc::clone(&broker));
    let restored = daemon.inspect("persisted").unwrap();
    assert_eq!(restored.id, id);
    assert_eq!(restored.state.status, LifecycleState::Created);
    assert_eq!(restored.config.cmd, config().cmd);
    assert_eq!(restored.name.as_deref(), Some("persisted"));

    daemon.remove("persisted", false).unwrap();
    assert!(
        !record.parent().unwrap().exists(),
        "remove kept the directory"
    );
    drop(daemon);

    let daemon = spawn(temp.path(), broker);
    assert!(daemon.list(&[]).unwrap().is_empty());
}

#[test]
fn a_running_container_is_reported_exited_after_a_restart() {
    let temp = tempfile::tempdir().unwrap();
    let broker = Arc::new(EventBroker::new());

    let daemon = spawn(temp.path(), Arc::clone(&broker));
    daemon
        .create(CreateRequest::new(config()).with_name("probe"))
        .unwrap();
    daemon.start("probe").unwrap();
    assert_eq!(
        daemon.inspect("probe").unwrap().state.status,
        LifecycleState::Running
    );
    drop(daemon);

    let daemon = spawn(temp.path(), Arc::clone(&broker));
    let restored = daemon.inspect("probe").unwrap();
    assert_eq!(restored.state.status, LifecycleState::Exited);
    assert_eq!(restored.state.exit_code, Some(255));
    assert!(restored.state.finished_at.is_some());

    // The restored container remains usable: a fresh start is allowed.
    daemon.start("probe").unwrap();
    assert_eq!(
        daemon.inspect("probe").unwrap().state.status,
        LifecycleState::Running
    );
    daemon.stop("probe", Some(Duration::from_secs(1))).unwrap();
}
