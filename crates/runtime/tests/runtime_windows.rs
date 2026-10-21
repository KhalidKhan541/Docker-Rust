//! Host-specific behavior. The native backend is Linux-only, so Windows must
//! report a clear `Unsupported` error instead of creating a fake task.

#![cfg(not(target_os = "linux"))]

use std::collections::BTreeMap;

use docker_common::{ContainerId, ImageRef};
use docker_container_model::config::{ContainerConfig, HostConfig, NetworkMode, Signal};
use docker_errors::ErrorKind;
use docker_runtime::{OciRuntime, Runtime, RuntimeConfig};
use tempfile::TempDir;

fn config() -> RuntimeConfig {
    RuntimeConfig::new(
        ContainerId::from_generated("c".repeat(64)).unwrap(),
        ContainerConfig {
            image: ImageRef::parse("alpine:3.19").unwrap(),
            entrypoint: Vec::new(),
            cmd: vec!["/bin/true".into()],
            env: Vec::new(),
            labels: BTreeMap::new(),
            working_dir: None,
            user: None,
            hostname: None,
            tty: false,
            open_stdin: false,
            stop_signal: Signal::Sigterm,
            stop_timeout_secs: 10,
            exposed_ports: Vec::new(),
        },
        HostConfig {
            network_mode: NetworkMode::None,
            ..HostConfig::default()
        },
    )
}

#[test]
fn windows_create_fails_explicitly() {
    let state = TempDir::new().unwrap();
    let rootfs = TempDir::new().unwrap();
    let runtime = OciRuntime::new(state.path());
    let error = match runtime.create(&config().with_rootfs(rootfs.path())) {
        Ok(_) => panic!("Windows create unexpectedly succeeded"),
        Err(error) => error,
    };
    assert_eq!(error.kind(), ErrorKind::Unsupported);
    let message = error.to_string();
    assert!(message.contains("Linux"));
    assert!(message.contains(std::env::consts::OS));
    assert_eq!(
        std::fs::read_dir(state.path()).unwrap().count(),
        0,
        "unsupported create must not leave runtime state behind"
    );
}
