//! Milestone D vertical slice: the daemon actor drives create -> start ->
//! die/stop, restart policy, removal, auto-remove, and event emission against
//! the deterministic synthetic runtime.

use std::collections::BTreeMap;
use std::sync::Arc;
use std::thread;
use std::time::{Duration, Instant};

use docker_common::ImageRef;
use docker_container_model::config::{ContainerConfig, HostConfig, RestartPolicy, Signal};
use docker_container_model::state::{LifecycleState, StateSnapshot};
use docker_daemon::{CreateRequest, Daemon, Runtime, SyntheticBehavior, SyntheticRuntime};
use docker_errors::ErrorKind;
use docker_events::{Event, EventBroker, Subscription};

fn test_config() -> ContainerConfig {
    ContainerConfig {
        image: ImageRef::parse("alpine:latest").unwrap(),
        entrypoint: vec!["/bin/sh".into()],
        cmd: vec!["-c".into(), "true".into()],
        env: vec!["MODE=test".into()],
        labels: BTreeMap::from([("com.example.component".to_string(), "daemon".to_string())]),
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

fn spawn(behavior: SyntheticBehavior) -> (Daemon, Arc<SyntheticRuntime>, Subscription) {
    let runtime = Arc::new(SyntheticRuntime::with_behavior(behavior));
    let broker = Arc::new(EventBroker::new());
    let subscription = broker.subscribe(4096).unwrap();
    let runtime_dyn: Arc<dyn Runtime> = runtime.clone();
    let daemon = Daemon::spawn(runtime_dyn, broker).unwrap();
    (daemon, runtime, subscription)
}

fn wait_for(mut condition: impl FnMut() -> bool, description: &str) {
    let deadline = Instant::now() + Duration::from_secs(8);
    while !condition() {
        assert!(
            Instant::now() < deadline,
            "timed out waiting for {description}"
        );
        thread::sleep(Duration::from_millis(5));
    }
}

fn wait_for_status(
    daemon: &Daemon,
    id: &str,
    status: LifecycleState,
) -> docker_daemon::ContainerView {
    let deadline = Instant::now() + Duration::from_secs(8);
    loop {
        if let Ok(view) = daemon.inspect(id) {
            if view.state.status == status {
                return view;
            }
        }
        assert!(
            Instant::now() < deadline,
            "timed out waiting for container {id} to reach {status}"
        );
        thread::sleep(Duration::from_millis(5));
    }
}

fn wait_for_missing(daemon: &Daemon, id: &str) {
    let deadline = Instant::now() + Duration::from_secs(8);
    while daemon.inspect(id).is_ok() {
        assert!(
            Instant::now() < deadline,
            "timed out waiting for container {id} to be removed"
        );
        thread::sleep(Duration::from_millis(5));
    }
}

fn drain(subscription: &Subscription) -> Vec<Event> {
    std::iter::from_fn(|| subscription.try_recv()).collect()
}

fn action_names(events: &[Event]) -> Vec<String> {
    events
        .iter()
        .map(|event| event.action.to_string())
        .collect()
}

fn assert_actions(events: &[Event], expected: &[&str]) {
    let actual = action_names(events);
    let expected: Vec<String> = expected.iter().map(|name| (*name).to_string()).collect();
    assert_eq!(actual, expected);
}

fn count(events: &[Event], action: &str) -> usize {
    events
        .iter()
        .filter(|event| event.action.to_string() == action)
        .count()
}

#[test]
fn lifecycle_emits_create_start_die_stop() {
    let (mut daemon, runtime, subscription) = spawn(SyntheticBehavior::running_until_killed());
    let created = daemon
        .create(CreateRequest::new(test_config()).with_name("web-1"))
        .unwrap();
    let id = created.id.clone();
    assert_eq!(created.state.status, LifecycleState::Created);
    assert_eq!(created.name.as_deref(), Some("web-1"));

    daemon.start(id.as_str()).unwrap();
    let running = daemon.inspect(id.as_str()).unwrap();
    assert_eq!(running.state.status, LifecycleState::Running);
    assert_eq!(running.state.exit_code, None);

    daemon.stop(id.as_str(), None).unwrap();
    let stopped = daemon.inspect(id.as_str()).unwrap();
    assert_eq!(stopped.state.status, LifecycleState::Exited);
    assert_eq!(stopped.state.exit_code, Some(143));
    assert_eq!(stopped.state.error_signal.as_deref(), Some("SIGTERM"));

    let events = drain(&subscription);
    assert_actions(&events, &["create", "start", "kill", "die", "stop"]);
    let create = events.first().unwrap();
    assert_eq!(create.actor.id, id.as_str());
    assert_eq!(create.actor.attributes.get("name").unwrap(), "web-1");
    assert_eq!(
        create.actor.attributes.get("image").unwrap(),
        "alpine:latest"
    );
    assert_eq!(
        create
            .actor
            .attributes
            .get("com.example.component")
            .unwrap(),
        "daemon"
    );
    let die = events
        .iter()
        .find(|event| event.action.to_string() == "die")
        .unwrap();
    assert_eq!(die.actor.attributes.get("exitCode").unwrap(), "143");
    assert!(die.actor.attributes.contains_key("execDuration"));
    let kill = events
        .iter()
        .find(|event| event.action.to_string() == "kill")
        .unwrap();
    assert_eq!(kill.actor.attributes.get("signal").unwrap(), "15");

    assert_eq!(runtime.created(), vec![id.clone()]);
    assert_eq!(runtime.started(), vec![id.clone()]);
    assert_eq!(runtime.kills(), vec![(id.clone(), Signal::Sigterm)]);

    // Stopping an already stopped container is a no-op, not another event.
    daemon.stop(id.as_str(), None).unwrap();
    assert!(drain(&subscription).is_empty());
    assert_eq!(runtime.started().len(), 1);
    daemon.shutdown().unwrap();
}

#[test]
fn stop_escalates_to_sigkill_after_timeout() {
    let (mut daemon, runtime, subscription) = spawn(SyntheticBehavior::ignoring_sigterm());
    let created = daemon
        .create(CreateRequest::new(test_config()).with_name("stubborn"))
        .unwrap();
    let id = created.id;
    daemon.start(id.as_str()).unwrap();
    daemon
        .stop(id.as_str(), Some(Duration::from_millis(30)))
        .unwrap();

    let stopped = daemon.inspect(id.as_str()).unwrap();
    assert_eq!(stopped.state.status, LifecycleState::Exited);
    assert_eq!(stopped.state.exit_code, Some(137));
    assert_eq!(stopped.state.error_signal.as_deref(), Some("SIGKILL"));
    assert_eq!(
        runtime.kills(),
        vec![(id.clone(), Signal::Sigterm), (id.clone(), Signal::Sigkill)]
    );

    let events = drain(&subscription);
    assert_actions(&events, &["create", "start", "kill", "kill", "die", "stop"]);
    let kill_signals: Vec<&str> = events
        .iter()
        .filter(|event| event.action.to_string() == "kill")
        .map(|event| event.actor.attributes.get("signal").unwrap().as_str())
        .collect();
    assert_eq!(kill_signals, ["15", "9"]);
    daemon.shutdown().unwrap();
}

#[test]
fn on_failure_restarts_with_backoff_and_caps() {
    let (mut daemon, runtime, subscription) =
        spawn(SyntheticBehavior::exits_after(Duration::from_millis(10), 1));
    let host = HostConfig {
        restart_policy: RestartPolicy::OnFailure,
        max_retry_count: 2,
        ..HostConfig::default()
    };
    let created = daemon
        .create(
            CreateRequest::new(test_config())
                .with_name("retry-twice")
                .with_host_config(host),
        )
        .unwrap();
    let id = created.id;
    daemon.start(id.as_str()).unwrap();

    let deadline = Instant::now() + Duration::from_secs(8);
    loop {
        if let Ok(view) = daemon.inspect(id.as_str()) {
            if view.state.status == LifecycleState::Exited && view.restart_count == 2 {
                break;
            }
        }
        assert!(
            Instant::now() < deadline,
            "timed out waiting for retries to be exhausted"
        );
        thread::sleep(Duration::from_millis(5));
    }

    let final_view = daemon.inspect(id.as_str()).unwrap();
    assert_eq!(final_view.state.exit_code, Some(1));
    assert_eq!(final_view.restart_count, 2);
    assert_eq!(runtime.started().len(), 3);
    let events = drain(&subscription);
    assert_eq!(count(&events, "start"), 3);
    assert_eq!(count(&events, "die"), 3);
    assert_eq!(count(&events, "kill"), 0);
    assert_eq!(count(&events, "stop"), 0);
    daemon.shutdown().unwrap();
}

#[test]
fn stop_during_restart_backoff_cancels_the_timer() {
    let (mut daemon, runtime, subscription) =
        spawn(SyntheticBehavior::exits_after(Duration::from_millis(5), 1));
    let host = HostConfig {
        restart_policy: RestartPolicy::OnFailure,
        ..HostConfig::default()
    };
    let created = daemon
        .create(
            CreateRequest::new(test_config())
                .with_name("backoff-stop")
                .with_host_config(host),
        )
        .unwrap();
    let id = created.id;
    daemon.start(id.as_str()).unwrap();
    wait_for_status(&daemon, id.as_str(), LifecycleState::Restarting);

    daemon.stop(id.as_str(), None).unwrap();
    let stopped = daemon.inspect(id.as_str()).unwrap();
    assert_eq!(stopped.state.status, LifecycleState::Exited);
    let started_after_stop = runtime.started().len();
    thread::sleep(Duration::from_millis(250));
    assert_eq!(runtime.started().len(), started_after_stop);
    assert_eq!(
        daemon.inspect(id.as_str()).unwrap().state.status,
        LifecycleState::Exited
    );
    let events = drain(&subscription);
    assert!(events
        .iter()
        .any(|event| event.action.to_string() == "stop"));
    daemon.shutdown().unwrap();
}

#[test]
fn remove_while_restarting_requires_force_and_cancels_timer() {
    let (mut daemon, runtime, _subscription) =
        spawn(SyntheticBehavior::exits_after(Duration::from_millis(5), 1));
    let host = HostConfig {
        restart_policy: RestartPolicy::OnFailure,
        ..HostConfig::default()
    };
    let created = daemon
        .create(
            CreateRequest::new(test_config())
                .with_name("restart-remove")
                .with_host_config(host),
        )
        .unwrap();
    let id = created.id;
    daemon.start(id.as_str()).unwrap();
    wait_for_status(&daemon, id.as_str(), LifecycleState::Restarting);

    // Public start/rm treat a scheduled restart as still running.
    daemon.start(id.as_str()).unwrap();
    let error = daemon.remove(id.as_str(), false).unwrap_err();
    assert_eq!(error.kind(), ErrorKind::Conflict);
    daemon.remove(id.as_str(), true).unwrap();
    wait_for_missing(&daemon, id.as_str());
    let started_after_remove = runtime.started().len();
    thread::sleep(Duration::from_millis(200));
    assert_eq!(runtime.started().len(), started_after_remove);
    daemon.shutdown().unwrap();
}

#[test]
fn manual_restart_emits_restart_and_resets_count() {
    let (mut daemon, runtime, subscription) = spawn(SyntheticBehavior::running_until_killed());
    let host = HostConfig {
        restart_policy: RestartPolicy::Always,
        ..HostConfig::default()
    };
    let created = daemon
        .create(
            CreateRequest::new(test_config())
                .with_name("manual-restart")
                .with_host_config(host),
        )
        .unwrap();
    let id = created.id;
    daemon.start(id.as_str()).unwrap();
    daemon.restart(id.as_str(), None).unwrap();

    let running = daemon.inspect(id.as_str()).unwrap();
    assert_eq!(running.state.status, LifecycleState::Running);
    assert_eq!(running.restart_count, 0);
    assert_eq!(runtime.started().len(), 2);
    let events = drain(&subscription);
    assert_actions(
        &events,
        &["create", "start", "kill", "die", "stop", "start", "restart"],
    );

    // `stop` cancels the Always policy; no automatic start follows.
    daemon.stop(id.as_str(), None).unwrap();
    assert_eq!(
        daemon.inspect(id.as_str()).unwrap().state.status,
        LifecycleState::Exited
    );
    thread::sleep(Duration::from_millis(200));
    assert_eq!(runtime.started().len(), 2);
    daemon.shutdown().unwrap();
}

#[test]
fn always_policy_reignites_after_signal_but_not_after_stop() {
    let (mut daemon, runtime, subscription) = spawn(SyntheticBehavior::running_until_killed());
    let host = HostConfig {
        restart_policy: RestartPolicy::Always,
        ..HostConfig::default()
    };
    let created = daemon
        .create(
            CreateRequest::new(test_config())
                .with_name("always")
                .with_host_config(host),
        )
        .unwrap();
    let id = created.id;
    daemon.start(id.as_str()).unwrap();

    // SIGHUP is not the configured stop signal, so ExitOnNext is not
    // called and `always` restarts the container.
    daemon.kill(id.as_str(), Signal::Sighup).unwrap();
    wait_for(
        || {
            runtime.started().len() >= 2
                && daemon
                    .inspect(id.as_str())
                    .map(|view| view.state.status == LifecycleState::Running)
                    .unwrap_or(false)
        },
        "always policy restart",
    );
    let restarted = daemon.inspect(id.as_str()).unwrap();
    assert_eq!(restarted.restart_count, 1);
    let events = drain(&subscription);
    assert_actions(&events, &["create", "start", "kill", "die", "start"]);
    assert_eq!(count(&events, "restart"), 0);

    daemon.stop(id.as_str(), None).unwrap();
    assert_eq!(
        daemon.inspect(id.as_str()).unwrap().state.status,
        LifecycleState::Exited
    );
    thread::sleep(Duration::from_millis(200));
    assert_eq!(runtime.started().len(), 2);
    daemon.shutdown().unwrap();
}

#[test]
fn force_remove_kills_then_destroys() {
    let (mut daemon, runtime, subscription) = spawn(SyntheticBehavior::running_until_killed());
    let created = daemon
        .create(CreateRequest::new(test_config()).with_name("force-rm"))
        .unwrap();
    let id = created.id;
    daemon.start(id.as_str()).unwrap();

    let error = daemon.remove(id.as_str(), false).unwrap_err();
    assert_eq!(error.kind(), ErrorKind::Conflict);
    assert_eq!(
        daemon.inspect(id.as_str()).unwrap().state.status,
        LifecycleState::Running
    );

    daemon.remove(id.as_str(), true).unwrap();
    wait_for_missing(&daemon, id.as_str());
    assert!(daemon.list(&[]).unwrap().is_empty());
    assert_eq!(runtime.kills(), vec![(id.clone(), Signal::Sigterm)]);
    let events = drain(&subscription);
    assert_actions(
        &events,
        &["create", "start", "kill", "die", "stop", "destroy"],
    );
    daemon.shutdown().unwrap();
}

#[test]
fn auto_remove_destroys_after_clean_exit() {
    let (mut daemon, runtime, subscription) =
        spawn(SyntheticBehavior::exits_after(Duration::from_millis(20), 0));
    let host = HostConfig {
        auto_remove: true,
        ..HostConfig::default()
    };
    let created = daemon
        .create(
            CreateRequest::new(test_config())
                .with_name("ephemeral")
                .with_host_config(host),
        )
        .unwrap();
    let id = created.id;
    daemon.start(id.as_str()).unwrap();
    wait_for_missing(&daemon, id.as_str());

    assert!(daemon.list(&[]).unwrap().is_empty());
    assert_eq!(runtime.started().len(), 1);
    assert_eq!(runtime.deleted(), 1);
    let events = drain(&subscription);
    assert_actions(&events, &["create", "start", "die", "destroy"]);
    daemon.shutdown().unwrap();
}

#[test]
fn auto_remove_waits_for_manual_restart_to_finish() {
    let (mut daemon, runtime, subscription) = spawn(SyntheticBehavior::running_until_killed());
    let host = HostConfig {
        auto_remove: true,
        ..HostConfig::default()
    };
    let created = daemon
        .create(
            CreateRequest::new(test_config())
                .with_name("ephemeral-restart")
                .with_host_config(host),
        )
        .unwrap();
    let id = created.id;
    daemon.start(id.as_str()).unwrap();
    daemon.restart(id.as_str(), None).unwrap();

    // The intermediate stop during `restart` must not auto-remove the record.
    assert_eq!(
        daemon.inspect(id.as_str()).unwrap().state.status,
        LifecycleState::Running
    );
    daemon.stop(id.as_str(), None).unwrap();
    wait_for_missing(&daemon, id.as_str());

    assert_eq!(runtime.started().len(), 2);
    assert_eq!(runtime.deleted(), 2);
    let events = drain(&subscription);
    assert_actions(
        &events,
        &[
            "create", "start", "kill", "die", "stop", "start", "restart", "kill", "die", "stop",
            "destroy",
        ],
    );
    daemon.shutdown().unwrap();
}

#[test]
fn create_validation_rejects_bad_names_env_and_privileged() {
    let (mut daemon, _runtime, _subscription) = spawn(SyntheticBehavior::running_until_killed());
    daemon
        .create(CreateRequest::new(test_config()).with_name("dup"))
        .unwrap();
    let error = daemon
        .create(CreateRequest::new(test_config()).with_name("dup"))
        .unwrap_err();
    assert_eq!(error.kind(), ErrorKind::Conflict);

    let mut bad_env = test_config();
    bad_env.env = vec!["MISSING_EQUALS".into()];
    let error = daemon
        .create(CreateRequest::new(bad_env).with_name("bad-env"))
        .unwrap_err();
    assert_eq!(error.kind(), ErrorKind::InvalidArgument);

    let privileged = HostConfig {
        privileged: true,
        ..HostConfig::default()
    };
    let error = daemon
        .create(
            CreateRequest::new(test_config())
                .with_name("privileged")
                .with_host_config(privileged),
        )
        .unwrap_err();
    assert_eq!(error.kind(), ErrorKind::Forbidden);

    let error = daemon
        .create(CreateRequest::new(test_config()).with_name("bad name"))
        .unwrap_err();
    assert_eq!(error.kind(), ErrorKind::InvalidArgument);

    assert_eq!(daemon.list(&[]).unwrap().len(), 1);
    daemon.shutdown().unwrap();
}

#[test]
fn start_failure_leaves_container_created_and_can_retry() {
    let (mut daemon, runtime, subscription) = spawn(SyntheticBehavior::failing_start());
    let created = daemon
        .create(CreateRequest::new(test_config()).with_name("start-fail"))
        .unwrap();
    let id = created.id;
    let error = daemon.start(id.as_str()).unwrap_err();
    assert_eq!(error.kind(), ErrorKind::Unavailable);
    assert_eq!(
        daemon.inspect(id.as_str()).unwrap().state,
        StateSnapshot {
            status: LifecycleState::Created,
            exit_code: None,
            error_signal: None,
        }
    );
    assert_eq!(runtime.created().len(), 1);
    assert!(runtime.started().is_empty());
    assert_actions(&drain(&subscription), &["create"]);

    runtime.set_behavior(SyntheticBehavior::running_until_killed());
    daemon.start(id.as_str()).unwrap();
    assert_eq!(
        daemon.inspect(id.as_str()).unwrap().state.status,
        LifecycleState::Running
    );
    daemon.stop(id.as_str(), None).unwrap();
    assert_eq!(runtime.started().len(), 1);
    assert_eq!(runtime.deleted(), 2);
    daemon.shutdown().unwrap();
}

#[test]
fn create_failure_leaves_container_created() {
    let (mut daemon, runtime, _subscription) = spawn(SyntheticBehavior::failing_create());
    let created = daemon
        .create(CreateRequest::new(test_config()).with_name("create-fail"))
        .unwrap();
    let id = created.id;
    let error = daemon.start(id.as_str()).unwrap_err();
    assert_eq!(error.kind(), ErrorKind::Unavailable);
    assert_eq!(
        daemon.inspect(id.as_str()).unwrap().state.status,
        LifecycleState::Created
    );
    assert!(runtime.created().is_empty());
    assert!(runtime.started().is_empty());
    daemon.shutdown().unwrap();
}

#[test]
fn concurrent_lifecycle_stress_leaves_no_records() {
    let runtime = Arc::new(SyntheticRuntime::with_behavior(
        SyntheticBehavior::running_until_killed(),
    ));
    let broker = Arc::new(EventBroker::new());
    let runtime_dyn: Arc<dyn Runtime> = runtime.clone();
    let daemon = Arc::new(Daemon::spawn(runtime_dyn, broker).unwrap());

    let mut workers = Vec::new();
    for worker in 0..8 {
        let daemon = Arc::clone(&daemon);
        workers.push(thread::spawn(move || {
            for index in 0..10 {
                let request =
                    CreateRequest::new(test_config()).with_name(format!("stress-{worker}-{index}"));
                let view = daemon.create(request).unwrap();
                daemon.start(view.id.as_str()).unwrap();
                daemon.stop(view.id.as_str(), None).unwrap();
                daemon.remove(view.id.as_str(), false).unwrap();
            }
        }));
    }
    for worker in workers {
        worker.join().unwrap();
    }

    assert!(daemon.list(&[]).unwrap().is_empty());
    assert_eq!(runtime.created().len(), 80);
    assert_eq!(runtime.started().len(), 80);
    assert_eq!(runtime.kills().len(), 80);
    assert_eq!(runtime.deleted(), 80);
    drop(daemon);
}
