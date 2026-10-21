//! Single-owner daemon orchestration actor.
//!
//! One OS thread owns the container store, the runtime tasks, restart timers,
//! and all pending API completions. Commands arrive over an `mpsc` channel;
//! replies travel back on per-command channels. No lock protects container
//! state, so lifecycle transitions cannot interleave.
//!
//! Supervision mirrors Moby's `daemon/monitor.go`: each started task gets a
//! monitor thread that blocks in `RuntimeTask::wait`, deletes the task, and
//! posts the exit back to the actor. The actor then applies the restart policy
//! from `restartmanager/restartmanager.go`, emits `die`, and either schedules
//! `start` again (state `restarting`) or settles in `exited`.

use std::collections::BTreeMap;
use std::collections::HashMap;
use std::sync::mpsc::{self, Receiver, RecvTimeoutError, Sender};
use std::sync::{Arc, Mutex};
use std::thread;
use std::time::{Duration, Instant};

use docker_common::{ContainerId, Name};
use docker_container_model::config::{ContainerConfig, HostConfig, Signal};
use docker_container_model::state::{validate_transition, LifecycleState, StateSnapshot};
use docker_container_model::store::{ContainerFilter, ContainerRecord, ContainerStore};
use docker_errors::{Error, Result};
use docker_events::{event_now, Actor, EventAction, EventBroker, EventType, KnownAction};
use serde::Serialize;

use crate::restart::RestartManager;
use crate::runtime::{ExitStatus, Runtime, RuntimeConfig, RuntimeTask};

/// Grace period between the escalated SIGKILL and declaring the runtime
/// unresponsive. Mirrors the force-kill wait in `daemon/kill.go`.
const FORCE_KILL_TIMEOUT: Duration = Duration::from_secs(10);
/// Moby hard-codes three seconds for force-remove (`daemon/delete.go`).
const FORCE_REMOVE_TIMEOUT: Duration = Duration::from_secs(3);

/// Create-container request. `name` is optional like
/// `POST /containers/create?name=...`; unnamed containers currently keep
/// `None` (the upstream random-name generator is not part of Milestone D).
#[derive(Debug, Clone)]
pub struct CreateRequest {
    pub name: Option<String>,
    pub config: ContainerConfig,
    pub host_config: HostConfig,
}

impl CreateRequest {
    pub fn new(config: ContainerConfig) -> Self {
        Self {
            name: None,
            config,
            host_config: HostConfig::default(),
        }
    }

    pub fn with_name(mut self, name: impl Into<String>) -> Self {
        self.name = Some(name.into());
        self
    }

    pub fn with_host_config(mut self, host_config: HostConfig) -> Self {
        self.host_config = host_config;
        self
    }
}

/// Immutable-ish view returned to callers. The store remains private to the
/// actor.
#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct ContainerView {
    pub id: ContainerId,
    pub name: Option<String>,
    pub config: ContainerConfig,
    pub host_config: HostConfig,
    pub state: StateSnapshot,
    pub restart_count: u32,
}

impl ContainerView {
    fn from_record(record: &ContainerRecord, restart_count: u32) -> Self {
        Self {
            id: record.id.clone(),
            name: record.name.clone(),
            config: record.config.clone(),
            host_config: record.host_config.clone(),
            state: record.state.clone(),
            restart_count,
        }
    }
}

/// Public handle to the daemon actor.
pub struct Daemon {
    tx: Mutex<Sender<Command>>,
    worker: Option<thread::JoinHandle<()>>,
}

impl Daemon {
    /// Spawn the actor. The returned handle is cheap to clone only through
    /// `Arc<Daemon>`; commands are serialized by the actor.
    pub fn spawn(runtime: Arc<dyn Runtime>, broker: Arc<EventBroker>) -> Result<Self> {
        let (tx, rx) = mpsc::channel();
        let engine_tx = tx.clone();
        let worker = thread::Builder::new()
            .name("docker-daemon".into())
            .spawn(move || {
                Engine::new(runtime, broker, engine_tx).run(rx);
            })
            .map_err(|e| Error::internal(format!("cannot spawn daemon actor: {e}")))?;
        Ok(Self {
            tx: Mutex::new(tx),
            worker: Some(worker),
        })
    }

    fn call<T>(&self, make: impl FnOnce(Sender<Result<T>>) -> Command) -> Result<T> {
        let (reply_tx, reply_rx) = mpsc::channel();
        self.send(make(reply_tx))?;
        reply_rx
            .recv()
            .map_err(|_| Error::internal("daemon actor dropped the command reply"))?
    }

    fn send(&self, command: Command) -> Result<()> {
        self.tx
            .lock()
            .map_err(|_| Error::internal("daemon actor channel lock poisoned"))?
            .send(command)
            .map_err(|_| Error::internal("daemon actor is not running"))
    }

    pub fn create(&self, request: CreateRequest) -> Result<ContainerView> {
        self.call(|reply| Command::Create {
            request: Box::new(request),
            reply,
        })
    }

    pub fn start(&self, reference: &str) -> Result<()> {
        self.call(|reply| Command::Start {
            reference: reference.to_string(),
            reply,
        })
    }

    pub fn stop(&self, reference: &str, timeout: Option<Duration>) -> Result<()> {
        self.call(|reply| Command::Stop {
            reference: reference.to_string(),
            timeout,
            reply,
        })
    }

    pub fn kill(&self, reference: &str, signal: Signal) -> Result<()> {
        self.call(|reply| Command::Kill {
            reference: reference.to_string(),
            signal,
            reply,
        })
    }

    pub fn restart(&self, reference: &str, timeout: Option<Duration>) -> Result<()> {
        self.call(|reply| Command::Restart {
            reference: reference.to_string(),
            timeout,
            reply,
        })
    }

    pub fn remove(&self, reference: &str, force: bool) -> Result<()> {
        self.call(|reply| Command::Remove {
            reference: reference.to_string(),
            force,
            reply,
        })
    }

    pub fn inspect(&self, reference: &str) -> Result<ContainerView> {
        self.call(|reply| Command::Inspect {
            reference: reference.to_string(),
            reply,
        })
    }

    pub fn list(&self, filters: &[ContainerFilter]) -> Result<Vec<ContainerView>> {
        self.call(|reply| Command::List {
            filters: filters.to_vec(),
            reply,
        })
    }

    /// Stop all tasks and join the actor. Idempotent.
    pub fn shutdown(&mut self) -> Result<()> {
        let Some(worker) = self.worker.take() else {
            return Ok(());
        };
        let (reply_tx, reply_rx) = mpsc::channel();
        let sent = self.send(Command::Shutdown { reply: reply_tx });
        let reply = if sent.is_ok() {
            reply_rx
                .recv()
                .map_err(|_| Error::internal("daemon actor dropped the shutdown reply"))
        } else {
            Err(Error::internal("daemon actor is not running"))
        };
        let joined = worker
            .join()
            .map_err(|_| Error::internal("daemon actor panicked"));
        reply??;
        joined
    }
}

impl Drop for Daemon {
    fn drop(&mut self) {
        if self.worker.is_some() {
            let _ = self.shutdown();
        }
    }
}

enum Command {
    Create {
        request: Box<CreateRequest>,
        reply: Sender<Result<ContainerView>>,
    },
    Start {
        reference: String,
        reply: Sender<Result<()>>,
    },
    Stop {
        reference: String,
        timeout: Option<Duration>,
        reply: Sender<Result<()>>,
    },
    Kill {
        reference: String,
        signal: Signal,
        reply: Sender<Result<()>>,
    },
    Restart {
        reference: String,
        timeout: Option<Duration>,
        reply: Sender<Result<()>>,
    },
    Remove {
        reference: String,
        force: bool,
        reply: Sender<Result<()>>,
    },
    Inspect {
        reference: String,
        reply: Sender<Result<ContainerView>>,
    },
    List {
        filters: Vec<ContainerFilter>,
        reply: Sender<Result<Vec<ContainerView>>>,
    },
    TaskExited {
        id: ContainerId,
        generation: u64,
        result: Result<ExitStatus>,
    },
    Shutdown {
        reply: Sender<Result<()>>,
    },
}

#[derive(Debug, Clone)]
struct Meta {
    restart_manager: RestartManager,
    restart_count: u32,
    manually_stopped: bool,
    last_exit: Option<ExitStatus>,
}

impl Meta {
    fn new(host: &HostConfig) -> Self {
        Self {
            restart_manager: RestartManager::new(host.restart_policy, host.max_retry_count, 0),
            restart_count: 0,
            manually_stopped: false,
            last_exit: None,
        }
    }
}

struct ActiveTask {
    task: Arc<dyn RuntimeTask>,
    generation: u64,
    started_at: Instant,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum StopKind {
    Stop,
    Restart,
    Remove,
}

struct PendingStop {
    reply: Sender<Result<()>>,
    kind: StopKind,
    escalate_at: Option<Instant>,
    hard_at: Option<Instant>,
}

impl PendingStop {
    fn next_deadline(&self) -> Option<Instant> {
        self.escalate_at.or(self.hard_at)
    }
}

struct Engine {
    store: ContainerStore,
    meta: HashMap<ContainerId, Meta>,
    tasks: HashMap<ContainerId, ActiveTask>,
    pending_stops: HashMap<ContainerId, PendingStop>,
    pending_restarts: HashMap<ContainerId, Instant>,
    runtime: Arc<dyn Runtime>,
    broker: Arc<EventBroker>,
    command_tx: Sender<Command>,
    shutting_down: bool,
    next_generation: u64,
}

impl Engine {
    fn new(
        runtime: Arc<dyn Runtime>,
        broker: Arc<EventBroker>,
        command_tx: Sender<Command>,
    ) -> Self {
        Self {
            store: ContainerStore::new(),
            meta: HashMap::new(),
            tasks: HashMap::new(),
            pending_stops: HashMap::new(),
            pending_restarts: HashMap::new(),
            runtime,
            broker,
            command_tx,
            shutting_down: false,
            next_generation: 0,
        }
    }

    fn run(mut self, rx: Receiver<Command>) {
        loop {
            self.process_timers();
            let wait = self
                .next_deadline()
                .map(|deadline| deadline.saturating_duration_since(Instant::now()));
            let command = match wait {
                Some(wait) => match rx.recv_timeout(wait) {
                    Ok(command) => Some(command),
                    Err(RecvTimeoutError::Timeout) => None,
                    Err(RecvTimeoutError::Disconnected) => break,
                },
                None => match rx.recv() {
                    Ok(command) => Some(command),
                    Err(_) => break,
                },
            };
            if let Some(command) = command {
                if self.handle(command) {
                    break;
                }
            }
        }
        self.hard_shutdown();
    }

    fn handle(&mut self, command: Command) -> bool {
        match command {
            Command::Create { request, reply } => {
                let result = self.create(*request);
                let _ = reply.send(result);
            }
            Command::Start { reference, reply } => {
                let result = self.start(&reference);
                let _ = reply.send(result);
            }
            Command::Stop {
                reference,
                timeout,
                reply,
            } => self.stop(&reference, timeout, reply),
            Command::Kill {
                reference,
                signal,
                reply,
            } => {
                let result = self.kill(&reference, signal);
                let _ = reply.send(result);
            }
            Command::Restart {
                reference,
                timeout,
                reply,
            } => self.restart(&reference, timeout, reply),
            Command::Remove {
                reference,
                force,
                reply,
            } => self.remove(&reference, force, reply),
            Command::Inspect { reference, reply } => {
                let result = self.inspect(&reference);
                let _ = reply.send(result);
            }
            Command::List { filters, reply } => {
                let result = self.list(&filters);
                let _ = reply.send(result);
            }
            Command::TaskExited {
                id,
                generation,
                result,
            } => self.task_exited(&id, generation, result),
            Command::Shutdown { reply } => {
                self.hard_shutdown();
                let _ = reply.send(Ok(()));
                return true;
            }
        }
        false
    }

    fn create(&mut self, request: CreateRequest) -> Result<ContainerView> {
        request.config.validate()?;
        request.host_config.validate(false)?;
        let name = match &request.name {
            Some(name) => Some(Name::new(name)?.as_str().to_string()),
            None => None,
        };
        if let Some(name) = &name {
            if self
                .store
                .list(&[])
                .iter()
                .any(|record| record.name.as_deref() == Some(name.as_str()))
            {
                return Err(Error::conflict(format!(
                    "container name \"/{name}\" is already in use"
                )));
            }
        }

        let id = ContainerId::generate()?;
        let record = ContainerRecord {
            id: id.clone(),
            name,
            config: request.config,
            host_config: request.host_config,
            state: StateSnapshot {
                status: LifecycleState::Created,
                exit_code: None,
                error_signal: None,
            },
        };
        let view = ContainerView::from_record(&record, 0);
        self.store.put(record.clone())?;
        self.meta.insert(id, Meta::new(&record.host_config));
        self.emit_container(&record, KnownAction::Create, &[]);
        Ok(view)
    }

    fn start(&mut self, reference: &str) -> Result<()> {
        let id = self.resolve_id(reference)?;
        if self.pending_stops.contains_key(&id) {
            return Err(Error::conflict("container stop is already in progress"));
        }
        let state = self.store.get(&id)?.state.status;
        match state {
            // Moby keeps `Running` true while a restart is scheduled, so a
            // public start is a 304 no-op rather than an immediate restart.
            LifecycleState::Running | LifecycleState::Restarting => Ok(()),
            LifecycleState::Paused => Err(Error::conflict(
                "cannot start a paused container, try unpause instead",
            )),
            LifecycleState::Created | LifecycleState::Exited => {
                self.pending_restarts.remove(&id);
                self.start_task(&id, true)
            }
            LifecycleState::Removing => Err(Error::conflict(
                "container is marked for removal and cannot be started",
            )),
            LifecycleState::Dead => Err(Error::conflict("container is dead and cannot be started")),
        }
    }

    fn stop(&mut self, reference: &str, timeout: Option<Duration>, reply: Sender<Result<()>>) {
        let id = match self.resolve_id(reference) {
            Ok(id) => id,
            Err(error) => {
                let _ = reply.send(Err(error));
                return;
            }
        };
        if self.pending_stops.contains_key(&id) {
            let _ = reply.send(Err(Error::conflict(
                "container stop is already in progress",
            )));
            return;
        }
        let state = match self.store.get(&id) {
            Ok(record) => record.state.status,
            Err(error) => {
                let _ = reply.send(Err(error));
                return;
            }
        };
        match state {
            LifecycleState::Running | LifecycleState::Paused => {
                self.begin_stop(&id, timeout, StopKind::Stop, reply);
            }
            LifecycleState::Restarting => {
                let result = self.stop_restarting(&id);
                let _ = reply.send(result);
            }
            LifecycleState::Created | LifecycleState::Exited | LifecycleState::Dead => {
                // Moby maps "already stopped" to 304; the headless actor
                // reports success without emitting a synthetic event.
                let _ = reply.send(Ok(()));
            }
            LifecycleState::Removing => {
                let _ = reply.send(Err(Error::conflict("container is being removed")));
            }
        }
    }

    fn kill(&mut self, reference: &str, signal: Signal) -> Result<()> {
        let id = self.resolve_id(reference)?;
        if self.pending_stops.contains_key(&id) {
            return Err(Error::conflict("container stop is already in progress"));
        }
        let record = self.store.get(&id)?.clone();
        if !matches!(
            record.state.status,
            LifecycleState::Running | LifecycleState::Paused
        ) {
            return Err(Error::conflict(format!(
                "container {} is not running",
                id.as_str()
            )));
        }
        let task = Arc::clone(
            &self
                .tasks
                .get(&id)
                .ok_or_else(|| Error::internal(format!("container {id} has no runtime task")))?
                .task,
        );
        if let Some(meta) = self.meta.get_mut(&id) {
            meta.manually_stopped = true;
            if signal == record.config.stop_signal || signal == Signal::Sigkill {
                meta.restart_manager.cancel();
            }
        }
        task.kill(signal)?;
        self.emit_container(
            &record,
            KnownAction::Kill,
            &[("signal", signal.number().to_string())],
        );
        Ok(())
    }

    fn restart(&mut self, reference: &str, timeout: Option<Duration>, reply: Sender<Result<()>>) {
        let id = match self.resolve_id(reference) {
            Ok(id) => id,
            Err(error) => {
                let _ = reply.send(Err(error));
                return;
            }
        };
        if self.pending_stops.contains_key(&id) {
            let _ = reply.send(Err(Error::conflict(
                "container stop is already in progress",
            )));
            return;
        }
        let state = match self.store.get(&id) {
            Ok(record) => record.state.status,
            Err(error) => {
                let _ = reply.send(Err(error));
                return;
            }
        };
        match state {
            LifecycleState::Running | LifecycleState::Paused => {
                self.begin_stop(&id, timeout, StopKind::Restart, reply);
            }
            LifecycleState::Restarting | LifecycleState::Created | LifecycleState::Exited => {
                self.pending_restarts.remove(&id);
                let result = self.start_task(&id, true).and_then(|()| {
                    let record = self.store.get(&id)?.clone();
                    self.emit_container(&record, KnownAction::Restart, &[]);
                    Ok(())
                });
                if result.is_err() && state == LifecycleState::Restarting {
                    self.fail_started_after_restarting(&id);
                }
                let _ = reply.send(result);
            }
            LifecycleState::Removing => {
                let _ = reply.send(Err(Error::conflict("container is being removed")));
            }
            LifecycleState::Dead => {
                let _ = reply.send(Err(Error::conflict(
                    "container is dead and cannot be restarted",
                )));
            }
        }
    }

    fn remove(&mut self, reference: &str, force: bool, reply: Sender<Result<()>>) {
        let id = match self.resolve_id(reference) {
            Ok(id) => id,
            Err(error) => {
                let _ = reply.send(Err(error));
                return;
            }
        };
        if self.pending_stops.contains_key(&id) {
            let _ = reply.send(Err(Error::conflict(
                "container stop is already in progress",
            )));
            return;
        }
        let state = match self.store.get(&id) {
            Ok(record) => record.state.status,
            Err(error) => {
                let _ = reply.send(Err(error));
                return;
            }
        };
        match state {
            LifecycleState::Running | LifecycleState::Paused => {
                if !force {
                    let message = if state == LifecycleState::Paused {
                        "container is paused and must be unpaused first".to_string()
                    } else {
                        "container is running: stop the container before removing or force remove"
                            .to_string()
                    };
                    let _ = reply.send(Err(Error::conflict(message)));
                } else {
                    self.begin_stop(&id, Some(FORCE_REMOVE_TIMEOUT), StopKind::Remove, reply);
                }
            }
            LifecycleState::Restarting => {
                if !force {
                    let _ = reply.send(Err(Error::conflict(
                        "container is restarting: stop the container before removing or force remove",
                    )));
                } else {
                    self.pending_restarts.remove(&id);
                    if let Some(meta) = self.meta.get_mut(&id) {
                        meta.manually_stopped = true;
                        meta.restart_manager.cancel();
                    }
                    let result = self.remove_record(&id);
                    let _ = reply.send(result);
                }
            }
            LifecycleState::Created | LifecycleState::Exited | LifecycleState::Dead => {
                let result = self.remove_record(&id);
                let _ = reply.send(result);
            }
            LifecycleState::Removing => {
                let _ = reply.send(Err(Error::conflict("removal is already in progress")));
            }
        }
    }

    fn inspect(&self, reference: &str) -> Result<ContainerView> {
        let id = self.resolve_id(reference)?;
        let record = self.store.get(&id)?;
        let restart_count = self
            .meta
            .get(&id)
            .map(|meta| meta.restart_count)
            .unwrap_or(0);
        Ok(ContainerView::from_record(record, restart_count))
    }

    fn list(&self, filters: &[ContainerFilter]) -> Result<Vec<ContainerView>> {
        let mut views: Vec<ContainerView> = self
            .store
            .list(filters)
            .into_iter()
            .map(|record| {
                ContainerView::from_record(
                    record,
                    self.meta
                        .get(&record.id)
                        .map(|meta| meta.restart_count)
                        .unwrap_or(0),
                )
            })
            .collect();
        views.sort_by(|left, right| left.id.as_str().cmp(right.id.as_str()));
        Ok(views)
    }

    fn resolve_id(&self, reference: &str) -> Result<ContainerId> {
        self.store
            .resolve(reference)
            .map(|record| record.id.clone())
    }

    fn start_task(&mut self, id: &ContainerId, manual: bool) -> Result<()> {
        let record = self.store.get(id)?.clone();
        if manual {
            if let Some(meta) = self.meta.get_mut(id) {
                meta.restart_manager = RestartManager::new(
                    record.host_config.restart_policy,
                    record.host_config.max_retry_count,
                    0,
                );
                meta.restart_count = 0;
                meta.manually_stopped = false;
            }
        }

        let runtime_config = RuntimeConfig::new(
            id.clone(),
            record.config.clone(),
            record.host_config.clone(),
        );
        let task_box = self.runtime.create(&runtime_config)?;
        if let Err(error) = task_box.start() {
            let _ = task_box.delete();
            return Err(error);
        }
        let task: Arc<dyn RuntimeTask> = Arc::from(task_box);

        let generation = self.next_generation;
        self.next_generation += 1;
        let monitor_task = Arc::clone(&task);
        let monitor_tx = self.command_tx.clone();
        let monitor_id = id.clone();
        let spawned = thread::Builder::new()
            .name(format!("docker-monitor-{}", &id.as_str()[..12]))
            .spawn(move || {
                let result = monitor_task.wait();
                let _ = monitor_task.delete();
                let _ = monitor_tx.send(Command::TaskExited {
                    id: monitor_id,
                    generation,
                    result,
                });
            });
        if let Err(error) = spawned {
            let _ = task.kill(Signal::Sigkill);
            let _ = task.delete();
            return Err(Error::internal(format!(
                "cannot spawn task monitor: {error}"
            )));
        }

        if let Err(error) = self.set_running(id) {
            let _ = task.kill(Signal::Sigkill);
            let _ = task.delete();
            return Err(error);
        }
        self.tasks.insert(
            id.clone(),
            ActiveTask {
                task,
                generation,
                started_at: Instant::now(),
            },
        );
        if let Some(meta) = self.meta.get_mut(id) {
            meta.last_exit = None;
            meta.manually_stopped = false;
        }
        self.emit_container(&record, KnownAction::Start, &[]);
        Ok(())
    }

    fn fail_started_after_restarting(&mut self, id: &ContainerId) {
        let exit = self
            .meta
            .get(id)
            .and_then(|meta| meta.last_exit.clone())
            .unwrap_or(ExitStatus {
                exit_code: 255,
                signal: None,
            });
        let _ = self.set_exit_state(id, LifecycleState::Exited, &exit);
        self.maybe_auto_remove(id);
    }

    fn begin_stop(
        &mut self,
        id: &ContainerId,
        timeout: Option<Duration>,
        kind: StopKind,
        reply: Sender<Result<()>>,
    ) {
        match self.send_stop_signal(id, timeout) {
            Ok((escalate_at, hard_at)) => {
                self.pending_stops.insert(
                    id.clone(),
                    PendingStop {
                        reply,
                        kind,
                        escalate_at,
                        hard_at,
                    },
                );
            }
            Err(error) => {
                let _ = reply.send(Err(error));
            }
        }
    }

    fn send_stop_signal(
        &mut self,
        id: &ContainerId,
        timeout: Option<Duration>,
    ) -> Result<(Option<Instant>, Option<Instant>)> {
        let record = self.store.get(id)?.clone();
        let task = Arc::clone(
            &self
                .tasks
                .get(id)
                .ok_or_else(|| Error::internal(format!("container {id} has no runtime task")))?
                .task,
        );
        let signal = record.config.stop_signal;
        if let Some(meta) = self.meta.get_mut(id) {
            meta.manually_stopped = true;
            meta.restart_manager.cancel();
        }
        task.kill(signal)?;
        self.emit_container(
            &record,
            KnownAction::Kill,
            &[("signal", signal.number().to_string())],
        );

        let now = Instant::now();
        let grace = timeout.unwrap_or_else(|| Duration::from_secs(record.config.stop_timeout_secs));
        let escalate_at = if signal == Signal::Sigkill {
            None
        } else if grace.is_zero() {
            Some(now)
        } else {
            Some(now.checked_add(grace).unwrap_or(now + FORCE_KILL_TIMEOUT))
        };
        let hard_at = if signal == Signal::Sigkill {
            Some(now + FORCE_KILL_TIMEOUT)
        } else {
            None
        };
        Ok((escalate_at, hard_at))
    }

    fn stop_restarting(&mut self, id: &ContainerId) -> Result<()> {
        self.pending_restarts.remove(id);
        let record = self.store.get(id)?.clone();
        let exit = self
            .meta
            .get(id)
            .and_then(|meta| meta.last_exit.clone())
            .unwrap_or(ExitStatus {
                exit_code: 0,
                signal: None,
            });
        if let Some(meta) = self.meta.get_mut(id) {
            meta.manually_stopped = true;
            meta.restart_manager.cancel();
        }
        self.set_exit_state(id, LifecycleState::Exited, &exit)?;
        self.emit_container(&record, KnownAction::Stop, &[]);
        self.maybe_auto_remove(id);
        Ok(())
    }

    fn task_exited(&mut self, id: &ContainerId, generation: u64, result: Result<ExitStatus>) {
        let matches = self
            .tasks
            .get(id)
            .map(|active| active.generation == generation)
            .unwrap_or(false);
        if !matches {
            return;
        }
        let Some(active) = self.tasks.remove(id) else {
            tracing::error!(container = %id.as_str(), "matched task disappeared from the actor map");
            return;
        };
        let duration = active.started_at.elapsed();

        let exit = match result {
            Ok(exit) => exit,
            Err(error) => {
                self.task_failed(id, duration, error);
                return;
            }
        };
        if let Some(meta) = self.meta.get_mut(id) {
            meta.last_exit = Some(exit.clone());
        }

        if let Some(pending) = self.pending_stops.remove(id) {
            let PendingStop { reply, kind, .. } = pending;
            let outcome = self.finish_pending_stop(id, &exit, duration, kind);
            let _ = reply.send(outcome);
            return;
        }

        if self.shutting_down {
            let _ = self.set_exit_state(id, LifecycleState::Exited, &exit);
            return;
        }

        let record = match self.store.get(id) {
            Ok(record) => record.clone(),
            Err(_) => return,
        };
        if record.state.status == LifecycleState::Dead {
            self.maybe_auto_remove(id);
            return;
        }

        let decision = self
            .meta
            .get_mut(id)
            .map(|meta| {
                meta.restart_manager
                    .should_restart(exit.exit_code, meta.manually_stopped, duration)
            })
            .unwrap_or_else(|| {
                Ok(crate::restart::RestartDecision {
                    restart: false,
                    delay: None,
                })
            });
        if let Some(meta) = self.meta.get_mut(id) {
            meta.restart_count = meta.restart_manager.restart_count();
        }

        match decision {
            Ok(decision) if decision.restart => {
                if let Err(error) = self.set_exit_state(id, LifecycleState::Restarting, &exit) {
                    tracing::error!(container = %id.as_str(), error = %error, "cannot enter restarting state");
                }
                self.emit_die(&record, &exit, duration);
                let delay = decision.delay.unwrap_or_default();
                self.pending_restarts
                    .insert(id.clone(), Instant::now() + delay);
            }
            Ok(_) => {
                if let Err(error) = self.set_exit_state(id, LifecycleState::Exited, &exit) {
                    tracing::error!(container = %id.as_str(), error = %error, "cannot enter exited state");
                }
                self.emit_die(&record, &exit, duration);
                self.maybe_auto_remove(id);
            }
            Err(error) => {
                tracing::warn!(
                    container = %id.as_str(),
                    error = %error,
                    "restart decision failed; container will not restart"
                );
                if let Err(state_error) = self.set_exit_state(id, LifecycleState::Exited, &exit) {
                    tracing::error!(container = %id.as_str(), error = %state_error, "cannot enter exited state");
                }
                self.emit_die(&record, &exit, duration);
                self.maybe_auto_remove(id);
            }
        }
    }

    fn finish_pending_stop(
        &mut self,
        id: &ContainerId,
        exit: &ExitStatus,
        duration: Duration,
        kind: StopKind,
    ) -> Result<()> {
        let record = self.store.get(id)?.clone();
        self.set_exit_state(id, LifecycleState::Exited, exit)?;
        self.emit_die(&record, exit, duration);
        self.emit_container(&record, KnownAction::Stop, &[]);
        match kind {
            StopKind::Stop => {
                self.maybe_auto_remove(id);
                Ok(())
            }
            StopKind::Restart => {
                self.start_task(id, true)?;
                let record = self.store.get(id)?.clone();
                self.emit_container(&record, KnownAction::Restart, &[]);
                Ok(())
            }
            StopKind::Remove => self.remove_record(id),
        }
    }

    fn task_failed(&mut self, id: &ContainerId, duration: Duration, error: Error) {
        tracing::error!(container = %id.as_str(), error = %error, "runtime wait failed");
        let exit = ExitStatus {
            exit_code: 255,
            signal: None,
        };
        if let Some(meta) = self.meta.get_mut(id) {
            meta.last_exit = Some(exit.clone());
        }
        let record = match self.store.get(id) {
            Ok(record) => record.clone(),
            Err(_) => return,
        };
        let _ = self.set_dead(id, &exit);
        self.emit_die(&record, &exit, duration);
        if let Some(pending) = self.pending_stops.remove(id) {
            let _ = pending.reply.send(Err(error));
            return;
        }
        self.maybe_auto_remove(id);
    }

    fn process_timers(&mut self) {
        let now = Instant::now();
        let mut restarts: Vec<ContainerId> = self
            .pending_restarts
            .iter()
            .filter(|(_, deadline)| **deadline <= now)
            .map(|(id, _)| id.clone())
            .collect();
        restarts.sort_by(|left, right| left.as_str().cmp(right.as_str()));
        for id in restarts {
            self.pending_restarts.remove(&id);
            self.begin_auto_restart(&id);
        }

        let now = Instant::now();
        let mut stops: Vec<ContainerId> = self
            .pending_stops
            .iter()
            .filter(|(_, pending)| {
                pending
                    .next_deadline()
                    .map(|deadline| deadline <= now)
                    .unwrap_or(false)
            })
            .map(|(id, _)| id.clone())
            .collect();
        stops.sort_by(|left, right| left.as_str().cmp(right.as_str()));
        for id in stops {
            self.handle_stop_timer(&id);
        }
    }

    fn begin_auto_restart(&mut self, id: &ContainerId) {
        let state = self.store.get(id).map(|record| record.state.status);
        if !matches!(state, Ok(LifecycleState::Restarting)) {
            return;
        }
        if let Some(meta) = self.meta.get_mut(id) {
            meta.restart_manager.complete_active();
        }
        if let Err(error) = self.start_task(id, false) {
            tracing::warn!(container = %id.as_str(), error = %error, "automatic restart failed");
            let exit = self
                .meta
                .get(id)
                .and_then(|meta| meta.last_exit.clone())
                .unwrap_or(ExitStatus {
                    exit_code: 255,
                    signal: None,
                });
            let _ = self.set_exit_state(id, LifecycleState::Exited, &exit);
            self.maybe_auto_remove(id);
        }
    }

    fn handle_stop_timer(&mut self, id: &ContainerId) {
        let now = Instant::now();
        let (escalate, hard) = match self.pending_stops.get(id) {
            Some(pending) => (
                pending
                    .escalate_at
                    .map(|deadline| deadline <= now)
                    .unwrap_or(false),
                pending
                    .hard_at
                    .map(|deadline| deadline <= now)
                    .unwrap_or(false),
            ),
            None => return,
        };
        if escalate {
            let task = self.tasks.get(id).map(|active| Arc::clone(&active.task));
            let killed = if let Some(task) = task {
                match task.kill(Signal::Sigkill) {
                    Ok(()) => true,
                    Err(error) => {
                        tracing::warn!(container = %id.as_str(), error = %error, "SIGKILL escalation failed");
                        false
                    }
                }
            } else {
                false
            };
            if killed {
                if let Ok(record) = self.store.get(id) {
                    let record = record.clone();
                    self.emit_container(
                        &record,
                        KnownAction::Kill,
                        &[("signal", Signal::Sigkill.number().to_string())],
                    );
                }
            }
            if let Some(pending) = self.pending_stops.get_mut(id) {
                pending.escalate_at = None;
                pending.hard_at = Some(now + FORCE_KILL_TIMEOUT);
            }
            return;
        }
        if hard {
            if let Some(pending) = self.pending_stops.remove(id) {
                let exit = ExitStatus {
                    exit_code: 128 + Signal::Sigkill.number(),
                    signal: Some(Signal::Sigkill),
                };
                let _ = self.set_dead(id, &exit);
                let _ = pending.reply.send(Err(Error::unavailable(format!(
                    "container {} did not exit after SIGKILL",
                    id.as_str()
                ))));
            }
            self.tasks.remove(id);
            self.maybe_auto_remove(id);
        }
    }

    fn next_deadline(&self) -> Option<Instant> {
        self.pending_stops
            .values()
            .filter_map(PendingStop::next_deadline)
            .chain(self.pending_restarts.values().copied())
            .min()
    }

    fn set_running(&mut self, id: &ContainerId) -> Result<()> {
        let record = self.store.get_mut(id)?;
        validate_transition(record.state.status, LifecycleState::Running)?;
        record.state = StateSnapshot {
            status: LifecycleState::Running,
            exit_code: None,
            error_signal: None,
        };
        Ok(())
    }

    fn set_exit_state(
        &mut self,
        id: &ContainerId,
        status: LifecycleState,
        exit: &ExitStatus,
    ) -> Result<()> {
        let record = self.store.get_mut(id)?;
        validate_transition(record.state.status, status)?;
        record.state.status = status;
        record.state.exit_code = Some(exit.exit_code);
        record.state.error_signal = exit.signal.map(|signal| signal.as_str().to_string());
        Ok(())
    }

    fn set_dead(&mut self, id: &ContainerId, exit: &ExitStatus) -> Result<()> {
        let record = self.store.get_mut(id)?;
        validate_transition(record.state.status, LifecycleState::Dead)?;
        record.state = StateSnapshot {
            status: LifecycleState::Dead,
            exit_code: Some(exit.exit_code),
            error_signal: exit.signal.map(|signal| signal.as_str().to_string()),
        };
        Ok(())
    }

    fn remove_record(&mut self, id: &ContainerId) -> Result<()> {
        let record = self.store.get(id)?.clone();
        self.store.remove(id)?;
        self.meta.remove(id);
        self.tasks.remove(id);
        self.pending_restarts.remove(id);
        self.emit_container(&record, KnownAction::Destroy, &[]);
        Ok(())
    }

    fn maybe_auto_remove(&mut self, id: &ContainerId) {
        let auto_remove = self
            .store
            .get(id)
            .map(|record| record.host_config.auto_remove)
            .unwrap_or(false);
        if auto_remove {
            let _ = self.remove_record(id);
        }
    }

    fn emit_die(&self, record: &ContainerRecord, exit: &ExitStatus, duration: Duration) {
        self.emit_container(
            record,
            KnownAction::Die,
            &[
                ("exitCode", exit.exit_code.to_string()),
                ("execDuration", duration.as_secs().to_string()),
            ],
        );
    }

    fn emit_container(
        &self,
        record: &ContainerRecord,
        action: KnownAction,
        extra: &[(&str, String)],
    ) {
        self.broker.publish(event_now(
            EventType::Container,
            EventAction::Known(action),
            container_actor(record, extra),
        ));
    }

    fn hard_shutdown(&mut self) {
        if self.shutting_down {
            return;
        }
        self.shutting_down = true;
        let ids: Vec<ContainerId> = self.tasks.keys().cloned().collect();
        for id in ids {
            if let Some(meta) = self.meta.get_mut(&id) {
                meta.manually_stopped = true;
                meta.restart_manager.cancel();
            }
            if let Some(active) = self.tasks.get(&id) {
                let _ = active.task.kill(Signal::Sigkill);
            }
        }
        self.tasks.clear();
        for (_, pending) in self.pending_stops.drain() {
            let _ = pending
                .reply
                .send(Err(Error::unavailable("daemon is shutting down")));
        }
        self.pending_restarts.clear();
    }
}

/// Build the actor attributes exactly like `LogContainerEventWithAttributes`:
/// caller extras first, then labels (which may override), then `image` and
/// `name` (which always win).
fn container_actor(record: &ContainerRecord, extra: &[(&str, String)]) -> Actor {
    let mut attributes = BTreeMap::new();
    for (key, value) in extra {
        attributes.insert((*key).to_string(), value.clone());
    }
    for (key, value) in &record.config.labels {
        attributes.insert(key.clone(), value.clone());
    }
    let image = record.config.image.to_string();
    if !image.is_empty() {
        attributes.insert("image".to_string(), image);
    }
    if let Some(name) = &record.name {
        attributes.insert("name".to_string(), name.trim_start_matches('/').to_string());
    }
    Actor {
        id: record.id.as_str().to_string(),
        attributes,
    }
}
