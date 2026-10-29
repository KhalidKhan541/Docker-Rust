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
use std::io::Read;
use std::path::Path;
use std::sync::mpsc::{self, Receiver, RecvTimeoutError, Sender};
use std::sync::{Arc, Mutex};
use std::thread;
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use docker_common::{ContainerId, Digest, Name};
use docker_container_model::config::{ContainerConfig, HostConfig, RestartPolicy, Signal};
use docker_container_model::state::{validate_transition, LifecycleState, StateSnapshot};
use docker_container_model::store::{ContainerFilter, ContainerRecord, ContainerStore};
use docker_errors::{Error, Result};
use docker_events::{event_now, Actor, EventAction, EventBroker, EventType, KnownAction};
use docker_runtime::bundle::{replace_or_append_env, DEFAULT_PATH_ENV};
use serde::Serialize;

use crate::logs::{LogStore, Stream, DEFAULT_LOG_CAPACITY};
use crate::persist::{ContainerDir, RestoredContainer};
use crate::restart::RestartManager;
use crate::runtime::{
    ExecConfig as RuntimeExecConfig, ExitStatus, Runtime, RuntimeConfig, RuntimeTask,
};

/// Grace period between the escalated SIGKILL and declaring the runtime
/// unresponsive. Mirrors the force-kill wait in `daemon/kill.go`.
const FORCE_KILL_TIMEOUT: Duration = Duration::from_secs(10);
/// Moby hard-codes three seconds for force-remove (`daemon/delete.go`).
const FORCE_REMOVE_TIMEOUT: Duration = Duration::from_secs(3);

fn unix_now() -> i64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|elapsed| elapsed.as_secs() as i64)
        .unwrap_or(0)
}

/// Create-container request. `name` is optional like
/// `POST /containers/create?name=...`; unnamed containers currently keep
/// `None` (the upstream random-name generator is not part of Milestone D).
#[derive(Debug, Clone)]
pub struct CreateRequest {
    pub name: Option<String>,
    pub config: ContainerConfig,
    /// Image ID resolved by the API layer. `None` is used by direct daemon
    /// tests and by synthetic-runtime callers that do not resolve images.
    pub image_id: Option<Digest>,
    pub host_config: HostConfig,
}

impl CreateRequest {
    pub fn new(config: ContainerConfig) -> Self {
        Self {
            name: None,
            config,
            image_id: None,
            host_config: HostConfig::default(),
        }
    }

    pub fn with_image_id(mut self, image_id: Digest) -> Self {
        self.image_id = Some(image_id);
        self
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
    pub created_at: i64,
    pub config: ContainerConfig,
    pub image_id: Option<Digest>,
    pub host_config: HostConfig,
    pub state: StateSnapshot,
    pub restart_count: u32,
}

impl ContainerView {
    fn from_record(record: &ContainerRecord, restart_count: u32) -> Self {
        Self {
            id: record.id.clone(),
            name: record.name.clone(),
            created_at: record.created_at,
            config: record.config.clone(),
            image_id: record.image_id.clone(),
            host_config: record.host_config.clone(),
            state: record.state.clone(),
            restart_count,
        }
    }
}

/// `condition` accepted by `GET/POST /containers/{id}/wait`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum WaitCondition {
    /// Return as soon as the container is not running (the historical Moby
    /// default). A created container is already "not running".
    NotRunning,
    /// Wait for the next process exit, including an exit that is followed by
    /// a restart-policy restart. If the container has already exited, return
    /// its current exit status immediately.
    NextExit,
    /// Wait until the container record has been removed.
    Removed,
}

/// Result of a wait request. `StatusCode` mirrors Moby's `WaitResponse`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct WaitOutcome {
    pub status_code: i32,
}

/// A request to create an exec instance for a running container.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ExecConfig {
    pub command: Vec<String>,
    pub env: Vec<String>,
    pub working_dir: Option<String>,
    pub user: Option<String>,
    pub attach_stdin: bool,
    pub attach_stdout: bool,
    pub attach_stderr: bool,
    pub tty: bool,
    pub privileged: bool,
}

impl ExecConfig {
    pub fn new(command: impl IntoIterator<Item = impl Into<String>>) -> Self {
        Self {
            command: command.into_iter().map(Into::into).collect(),
            env: Vec::new(),
            working_dir: None,
            user: None,
            attach_stdin: false,
            attach_stdout: true,
            attach_stderr: true,
            tty: false,
            privileged: false,
        }
    }
}

/// Public projection of an exec instance, matching the fields consumed by
/// `docker exec` and `GET /exec/{id}/json`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ExecView {
    pub id: String,
    pub container_id: ContainerId,
    pub started: bool,
    pub running: bool,
    pub exit_code: i32,
    pub pid: i32,
    pub process_config: ExecProcessConfig,
    pub open_stdin: bool,
    pub open_stdout: bool,
    pub open_stderr: bool,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ExecProcessConfig {
    pub entrypoint: String,
    pub arguments: Vec<String>,
    pub user: String,
    pub tty: bool,
    pub privileged: bool,
}

/// An exec process started and captured by the actor. The API layer can
/// stream `logs` without holding actor state; `open_*` comes from the create
/// request so stdout/stderr filtering matches the client's attach options.
#[derive(Debug, Clone)]
pub struct ExecSession {
    pub logs: Arc<LogStore>,
    pub open_stdout: bool,
    pub open_stderr: bool,
}

/// Public handle to the daemon actor.
pub struct Daemon {
    tx: Mutex<Sender<Command>>,
    worker: Option<thread::JoinHandle<()>>,
}

impl Daemon {
    /// Spawn the actor with an ephemeral in-memory container store.
    ///
    /// The returned handle is cheap to clone only through `Arc<Daemon>`;
    /// commands are serialized by the actor.
    pub fn spawn(runtime: Arc<dyn Runtime>, broker: Arc<EventBroker>) -> Result<Self> {
        Self::spawn_inner(runtime, broker, None, Vec::new())
    }

    /// Spawn the actor with container records persisted under
    /// `<data_root>/containers/<id>/`.
    ///
    /// Records that were running when the previous daemon stopped are restored
    /// as exited (exit code 255) because their processes cannot be adopted.
    pub fn spawn_persistent(
        runtime: Arc<dyn Runtime>,
        broker: Arc<EventBroker>,
        data_root: impl AsRef<Path>,
    ) -> Result<Self> {
        let dir = ContainerDir::open(data_root.as_ref())?;
        let records = dir.restore()?;
        Self::spawn_inner(runtime, broker, Some(dir), records)
    }

    fn spawn_inner(
        runtime: Arc<dyn Runtime>,
        broker: Arc<EventBroker>,
        container_dir: Option<ContainerDir>,
        records: Vec<RestoredContainer>,
    ) -> Result<Self> {
        let (tx, rx) = mpsc::channel();
        let engine_tx = tx.clone();
        let worker = thread::Builder::new()
            .name("docker-daemon".into())
            .spawn(move || {
                Engine::new(runtime, broker, engine_tx, container_dir, records).run(rx);
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

    /// Block until the requested container wait condition is satisfied.
    pub fn wait(&self, reference: &str, condition: WaitCondition) -> Result<WaitOutcome> {
        self.call(|reply| Command::Wait {
            reference: reference.to_string(),
            condition,
            reply,
        })
    }

    pub fn list(&self, filters: &[ContainerFilter]) -> Result<Vec<ContainerView>> {
        self.call(|reply| Command::List {
            filters: filters.to_vec(),
            reply,
        })
    }

    /// Shared handle to a container's captured stdio log store, so API
    /// streaming handlers can read (and follow) without owning actor state.
    /// `None` would mean the record vanished mid-flight, which callers treat
    /// as 404.
    pub fn log_store(&self, reference: &str) -> Result<Option<Arc<LogStore>>> {
        self.call(|reply| Command::Logs {
            reference: reference.to_string(),
            reply,
        })
    }

    /// Create an exec configuration. The process does not start until
    /// [`Daemon::exec_start`].
    pub fn exec_create(&self, reference: &str, config: ExecConfig) -> Result<ExecView> {
        self.call(|reply| Command::ExecCreate {
            reference: reference.to_string(),
            config: Box::new(config),
            reply,
        })
    }

    /// Start an exec instance and return its captured output stream. The
    /// actor owns the process and updates the exit code asynchronously.
    pub fn exec_start(&self, id: &str) -> Result<ExecSession> {
        self.call(|reply| Command::ExecStart {
            id: id.to_string(),
            reply,
        })
    }

    pub fn exec_inspect(&self, id: &str) -> Result<ExecView> {
        self.call(|reply| Command::ExecInspect {
            id: id.to_string(),
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
    Wait {
        reference: String,
        condition: WaitCondition,
        reply: Sender<Result<WaitOutcome>>,
    },
    List {
        filters: Vec<ContainerFilter>,
        reply: Sender<Result<Vec<ContainerView>>>,
    },
    Logs {
        reference: String,
        reply: Sender<Result<Option<Arc<LogStore>>>>,
    },
    ExecCreate {
        reference: String,
        config: Box<ExecConfig>,
        reply: Sender<Result<ExecView>>,
    },
    ExecStart {
        id: String,
        reply: Sender<Result<ExecSession>>,
    },
    ExecInspect {
        id: String,
        reply: Sender<Result<ExecView>>,
    },
    ExecExited {
        id: String,
        result: Result<ExitStatus>,
        /// Acknowledges that the actor published the exit code. The monitor
        /// holds the log writer lease until this arrives so an attach EOF
        /// can never race ahead of `ExecInspect`.
        acknowledged: Sender<()>,
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
    /// Monotonic counter for wait clients using `condition=next-exit`.
    exit_sequence: u64,
    /// Captured stdio ring buffer; lives for the whole container lifetime and
    /// accumulates across restarts like Moby's json-file log driver.
    logs: Arc<LogStore>,
}

impl Meta {
    fn new(host: &HostConfig, logs: Arc<LogStore>) -> Self {
        Self {
            restart_manager: RestartManager::new(host.restart_policy, host.max_retry_count, 0),
            restart_count: 0,
            manually_stopped: false,
            last_exit: None,
            exit_sequence: 0,
            logs,
        }
    }
}

struct ActiveTask {
    task: Arc<dyn RuntimeTask>,
    generation: u64,
    started_at: Instant,
}

struct ExecState {
    container_id: ContainerId,
    config: ExecConfig,
    task: Option<Arc<dyn RuntimeTask>>,
    logs: Option<Arc<LogStore>>,
    started: bool,
    running: bool,
    exit_code: i32,
    pid: i32,
}

/// A blocked `/wait` request. Waiters are held by the actor (not by the HTTP
/// handler), so they survive restarts and are notified exactly once.
struct Waiter {
    condition: WaitCondition,
    /// Exit counter observed when a next-exit waiter registered.
    exit_sequence: u64,
    reply: Sender<Result<WaitOutcome>>,
}

/// Exec user lookup is intentionally limited to numeric `UID[:GID]`. Validate
/// at the API boundary so a named user is an explicit unsupported error rather
/// than a helper failure after the exec has already started.
fn validate_numeric_exec_user(value: &str) -> Result<()> {
    let value = value.trim();
    if value.is_empty() {
        return Ok(());
    }
    let valid = match value.split_once(':') {
        Some((uid, gid)) => {
            !uid.is_empty()
                && !gid.is_empty()
                && uid.parse::<u32>().is_ok()
                && gid.parse::<u32>().is_ok()
        }
        None => value.parse::<u32>().is_ok(),
    };
    if valid {
        Ok(())
    } else {
        Err(Error::unsupported(format!(
            "exec user `{value}` requires numeric UID[:GID]; name lookup is not implemented"
        )))
    }
}

fn exec_view(state: &ExecState) -> ExecView {
    let mut command = state.config.command.iter();
    let entrypoint = command.next().cloned().unwrap_or_default();
    let arguments = command.cloned().collect();
    ExecView {
        id: String::new(),
        container_id: state.container_id.clone(),
        started: state.started,
        running: state.running,
        exit_code: state.exit_code,
        pid: state.pid,
        process_config: ExecProcessConfig {
            entrypoint,
            arguments,
            user: state.config.user.clone().unwrap_or_default(),
            tty: state.config.tty,
            privileged: state.config.privileged,
        },
        open_stdin: state.config.attach_stdin,
        open_stdout: state.config.attach_stdout,
        open_stderr: state.config.attach_stderr,
    }
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
    execs: HashMap<String, ExecState>,
    waiters: HashMap<ContainerId, Vec<Waiter>>,
    pending_stops: HashMap<ContainerId, PendingStop>,
    pending_restarts: HashMap<ContainerId, Instant>,
    runtime: Arc<dyn Runtime>,
    broker: Arc<EventBroker>,
    command_tx: Sender<Command>,
    /// `None` for an ephemeral daemon (tests and in-process callers).
    container_dir: Option<ContainerDir>,
    /// Containers recovered from `running` (or paused/restarting) at startup.
    /// The engine applies their restart policy before serving commands.
    recovered: Vec<ContainerId>,
    shutting_down: bool,
    next_generation: u64,
}

impl Engine {
    fn new(
        runtime: Arc<dyn Runtime>,
        broker: Arc<EventBroker>,
        command_tx: Sender<Command>,
        container_dir: Option<ContainerDir>,
        records: Vec<RestoredContainer>,
    ) -> Self {
        let mut store = ContainerStore::new();
        let mut meta = HashMap::new();
        let mut recovered = Vec::new();
        for restored in records {
            let record = restored.record;
            let id = record.id.clone();
            if let Err(error) = store.put(record.clone()) {
                tracing::error!(
                    container = %id.as_str(),
                    error = %error,
                    "ignoring duplicate persisted container record"
                );
                continue;
            }
            let logs = match &container_dir {
                Some(dir) => match LogStore::open(dir.log_path(&id), DEFAULT_LOG_CAPACITY) {
                    Ok(store) => Arc::new(store),
                    Err(error) => {
                        tracing::error!(
                            container = %id.as_str(),
                            error = %error,
                            "cannot restore container logs; using an empty in-memory buffer"
                        );
                        Arc::new(LogStore::new(DEFAULT_LOG_CAPACITY))
                    }
                },
                None => Arc::new(LogStore::new(DEFAULT_LOG_CAPACITY)),
            };
            let entry = Meta::new(&record.host_config, logs);
            // Nothing can append until the container is started, so a follow
            // request on a restored container must see EOF after the replay.
            entry.logs.close_if_idle();
            meta.insert(id.clone(), entry);
            if restored.recovered {
                recovered.push(id);
            }
        }
        Self {
            store,
            meta,
            tasks: HashMap::new(),
            execs: HashMap::new(),
            waiters: HashMap::new(),
            pending_stops: HashMap::new(),
            pending_restarts: HashMap::new(),
            runtime,
            broker,
            command_tx,
            container_dir,
            recovered,
            shutting_down: false,
            next_generation: 0,
        }
    }

    /// Honor restart policies for containers that were alive when the
    /// previous daemon stopped. `no` keeps the recovered `exited` state.
    fn restart_recovered(&mut self) {
        for id in std::mem::take(&mut self.recovered) {
            let policy = match self.store.get(&id) {
                Ok(record) => record.host_config.restart_policy,
                Err(_) => continue,
            };
            if policy == RestartPolicy::No {
                continue;
            }
            tracing::info!(
                container = %id.as_str(),
                ?policy,
                "restarting container recovered from the previous daemon"
            );
            if let Err(error) = self.start_task(&id, true) {
                tracing::error!(
                    container = %id.as_str(),
                    error = %error,
                    "cannot restart container after daemon restart"
                );
            }
        }
    }

    fn run(mut self, rx: Receiver<Command>) {
        self.restart_recovered();
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
            Command::Wait {
                reference,
                condition,
                reply,
            } => self.wait(&reference, condition, reply),
            Command::List { filters, reply } => {
                let result = self.list(&filters);
                let _ = reply.send(result);
            }
            Command::Logs { reference, reply } => {
                let result = self.log_store(&reference);
                let _ = reply.send(result);
            }
            Command::ExecCreate {
                reference,
                config,
                reply,
            } => {
                let result = self.exec_create(&reference, *config);
                let _ = reply.send(result);
            }
            Command::ExecStart { id, reply } => {
                let result = self.exec_start(&id);
                let _ = reply.send(result);
            }
            Command::ExecInspect { id, reply } => {
                let result = self.exec_inspect(&id);
                let _ = reply.send(result);
            }
            Command::ExecExited {
                id,
                result,
                acknowledged,
            } => {
                self.exec_exited(&id, result);
                let _ = acknowledged.send(());
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
            created_at: unix_now(),
            config: request.config,
            image_id: request.image_id,
            host_config: request.host_config,
            state: StateSnapshot::created(),
        };
        self.store.put(record.clone())?;
        if let Err(error) = self.persist_record(&record) {
            let _ = self.store.remove(&id);
            if let Some(dir) = &self.container_dir {
                let _ = dir.remove(&id);
            }
            return Err(error);
        }
        let logs = match self.open_logs(&id) {
            Ok(logs) => logs,
            Err(error) => {
                let _ = self.store.remove(&id);
                if let Some(dir) = &self.container_dir {
                    let _ = dir.remove(&id);
                }
                return Err(error);
            }
        };
        self.meta.insert(id, Meta::new(&record.host_config, logs));
        self.emit_container(&record, KnownAction::Create, &[]);
        Ok(ContainerView::from_record(&record, 0))
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

    fn wait(
        &mut self,
        reference: &str,
        condition: WaitCondition,
        reply: Sender<Result<WaitOutcome>>,
    ) {
        let id = match self.resolve_id(reference) {
            Ok(id) => id,
            Err(error) => {
                let _ = reply.send(Err(error));
                return;
            }
        };
        let record = match self.store.get(&id) {
            Ok(record) => record,
            Err(error) => {
                let _ = reply.send(Err(error));
                return;
            }
        };
        let status = record.state.status;
        let current_exit = record.state.exit_code.unwrap_or(0);
        match condition {
            WaitCondition::NotRunning if !status.is_alive() => {
                let _ = reply.send(Ok(WaitOutcome {
                    status_code: current_exit,
                }));
                return;
            }
            WaitCondition::NextExit if status == LifecycleState::Removing => {
                let _ = reply.send(Err(Error::conflict("container is being removed")));
                return;
            }
            _ => {}
        }
        let exit_sequence = self
            .meta
            .get(&id)
            .map(|meta| meta.exit_sequence)
            .unwrap_or(0);
        self.waiters.entry(id).or_default().push(Waiter {
            condition,
            exit_sequence,
            reply,
        });
    }

    /// Fulfil all waiters for `id` matching the predicate. Other conditions
    /// remain blocked until their own state transition.
    fn fulfill_waiters(
        &mut self,
        id: &ContainerId,
        should_notify: impl Fn(&Waiter) -> bool,
        status_code: i32,
    ) {
        let Some(waiters) = self.waiters.remove(id) else {
            return;
        };
        let mut remaining = Vec::new();
        for waiter in waiters {
            if should_notify(&waiter) {
                let _ = waiter.reply.send(Ok(WaitOutcome { status_code }));
            } else {
                remaining.push(waiter);
            }
        }
        if !remaining.is_empty() {
            self.waiters.insert(id.clone(), remaining);
        }
    }

    fn notify_next_exit(&mut self, id: &ContainerId, status_code: i32) {
        let sequence = self
            .meta
            .get(id)
            .map(|meta| meta.exit_sequence)
            .unwrap_or(0);
        self.fulfill_waiters(
            id,
            |waiter| waiter.condition == WaitCondition::NextExit && waiter.exit_sequence < sequence,
            status_code,
        );
    }

    fn notify_not_running(&mut self, id: &ContainerId, status_code: i32) {
        self.fulfill_waiters(
            id,
            |waiter| waiter.condition == WaitCondition::NotRunning,
            status_code,
        );
    }

    /// Wake both terminal wait conditions after a transition that will never
    /// produce another process exit (failed restart or an unresponsive stop).
    /// A `next-exit` waiter that survived the last process exit is waiting for
    /// a replacement task, so the terminal failure is its final outcome.
    fn notify_terminal(&mut self, id: &ContainerId, status_code: i32) {
        self.fulfill_waiters(
            id,
            |waiter| {
                matches!(
                    waiter.condition,
                    WaitCondition::NextExit | WaitCondition::NotRunning
                )
            },
            status_code,
        );
    }

    fn notify_removed(&mut self, id: &ContainerId, status_code: i32) {
        self.fulfill_waiters(
            id,
            |waiter| waiter.condition == WaitCondition::Removed,
            status_code,
        );
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

    fn log_store(&mut self, reference: &str) -> Result<Option<Arc<LogStore>>> {
        let id = self.resolve_id(reference)?;
        Ok(self.meta.get(&id).map(|meta| Arc::clone(&meta.logs)))
    }

    fn exec_create(&mut self, reference: &str, mut config: ExecConfig) -> Result<ExecView> {
        if config.command.is_empty() {
            return Err(Error::invalid_argument("exec command must not be empty"));
        }
        if config.tty {
            return Err(Error::unsupported(
                "exec TTY allocation is not implemented; omit --tty/-t",
            ));
        }
        if config.privileged {
            return Err(Error::unsupported(
                "privileged exec is not implemented; omit --privileged",
            ));
        }
        if let Some(user) = &config.user {
            validate_numeric_exec_user(user)?;
        }
        if config.attach_stdin {
            return Err(Error::unsupported(
                "exec stdin is not implemented; omit --interactive/-i",
            ));
        }
        if config.user.as_deref() == Some("") {
            config.user = None;
        }
        if let Some(dir) = &config.working_dir {
            if dir.is_empty() {
                config.working_dir = None;
            } else if !dir.starts_with('/') {
                return Err(Error::invalid_argument(format!(
                    "exec working directory `{dir}` must be absolute"
                )));
            }
        }
        for entry in &config.env {
            let valid = entry
                .split_once('=')
                .map(|(key, _)| !key.is_empty())
                .unwrap_or(false);
            if !valid {
                return Err(Error::invalid_argument(format!(
                    "exec environment entry `{entry}` must be KEY=VALUE"
                )));
            }
        }

        let container_id = self.resolve_id(reference)?;
        let record = self.store.get(&container_id)?;
        if record.state.status != LifecycleState::Running {
            return Err(Error::conflict(format!(
                "container {} is not running",
                container_id.as_str()
            )));
        }
        let id = docker_common::rng::generate_id()?;
        let state = ExecState {
            container_id,
            config,
            task: None,
            logs: None,
            started: false,
            running: false,
            exit_code: 0,
            pid: 0,
        };
        let mut view = exec_view(&state);
        view.id = id.clone();
        self.execs.insert(id, state);
        Ok(view)
    }

    fn exec_start(&mut self, id: &str) -> Result<ExecSession> {
        let (container_id, config) = {
            let state = self
                .execs
                .get(id)
                .ok_or_else(|| Error::not_found(format!("no such exec: {id}")))?;
            if state.started {
                return Err(Error::conflict(format!(
                    "exec instance {id} has already started"
                )));
            }
            (state.container_id.clone(), state.config.clone())
        };
        let record = self.store.get(&container_id)?;
        if record.state.status != LifecycleState::Running {
            return Err(Error::conflict(format!(
                "container {} is not running",
                container_id.as_str()
            )));
        }
        if let Some(user) = config
            .user
            .as_deref()
            .or(record.config.user.as_deref())
            .filter(|value| !value.trim().is_empty())
        {
            validate_numeric_exec_user(user)?;
        }
        let runtime_config = RuntimeExecConfig {
            argv: config.command.clone(),
            env: {
                let mut env = replace_or_append_env(&record.config.env, &config.env);
                if !env.iter().any(|entry| entry.starts_with("PATH=")) {
                    env.push(format!("PATH={DEFAULT_PATH_ENV}"));
                }
                env
            },
            cwd: config
                .working_dir
                .clone()
                .or_else(|| record.config.working_dir.clone())
                .unwrap_or_else(|| "/".to_string()),
            user: config.user.clone().or_else(|| record.config.user.clone()),
            capture_stdio: true,
        };
        let active = self.tasks.get(&container_id).ok_or_else(|| {
            Error::conflict(format!(
                "container {} has no active runtime task",
                container_id.as_str()
            ))
        })?;
        let task_box = active.task.exec(&runtime_config)?;
        task_box.start()?;
        let task: Arc<dyn RuntimeTask> = Arc::from(task_box);
        let logs = Arc::new(LogStore::new(DEFAULT_LOG_CAPACITY));
        // Keep the stream open until the exit status has been published to the
        // actor. Capture pipes can reach EOF before `wait()` returns; without
        // this lease the client could see EOF and issue ExecInspect while the
        // actor still reports the exec as running with a stale exit code.
        let close_hold = logs.take_writer();
        let handles = match task.take_stdio() {
            Ok(handles) => handles,
            Err(error) => {
                let _ = task.kill(Signal::Sigkill);
                let _ = task.delete();
                return Err(error);
            }
        };
        if let Some(mut stdio) = handles {
            // Exec stdin is rejected at create; dropping the pipe keeps the
            // child from waiting on a descriptor the daemon never feeds.
            drop(stdio.stdin.take());
            if let Some(stdout) = stdio.stdout.take() {
                Self::spawn_pipe_capture(&container_id, &logs, stdout, Stream::Stdout);
            }
            if let Some(stderr) = stdio.stderr.take() {
                Self::spawn_pipe_capture(&container_id, &logs, stderr, Stream::Stderr);
            }
        }

        let pid = task.pid().unwrap_or(0);
        let monitor_task = Arc::clone(&task);
        let monitor_logs = Arc::clone(&logs);
        let monitor_hold = close_hold;
        let command_tx = self.command_tx.clone();
        let exec_id = id.to_string();
        let monitor = thread::Builder::new()
            .name(format!("docker-exec-{id}"))
            .spawn(move || {
                let result = monitor_task.wait();
                // Publish the exit code before waiting on capture EOF. The
                // client inspects the exec immediately after the attach
                // stream closes, so the actor must already know the status by
                // then; the capture threads can still drain their final bytes.
                let (acknowledged, ack_rx) = mpsc::channel();
                let _ = command_tx.send(Command::ExecExited {
                    id: exec_id,
                    result,
                    acknowledged,
                });
                // Wait until the actor has actually stored the exit code
                // before releasing the log writer lease. Enqueueing alone
                // does not order this event against a later ExecInspect, and
                // an attach EOF must never expose a stale `ExitCode`.
                let _ = ack_rx.recv();
                drop(monitor_hold);
                let _ = monitor_logs.wait_closed(Duration::from_secs(2));
                let _ = monitor_task.delete();
            });
        if let Err(error) = monitor {
            let _ = task.kill(Signal::Sigkill);
            let _ = task.delete();
            logs.close();
            return Err(Error::internal(format!(
                "cannot start exec monitor thread: {error}"
            )));
        }

        let state = self
            .execs
            .get_mut(id)
            .expect("exec instance exists while starting");
        state.started = true;
        state.running = true;
        state.pid = pid;
        state.logs = Some(Arc::clone(&logs));
        state.task = Some(task);
        Ok(ExecSession {
            logs,
            open_stdout: config.attach_stdout,
            open_stderr: config.attach_stderr,
        })
    }

    fn exec_inspect(&self, id: &str) -> Result<ExecView> {
        let mut view = self
            .execs
            .get(id)
            .map(exec_view)
            .ok_or_else(|| Error::not_found(format!("no such exec: {id}")))?;
        view.id = id.to_string();
        Ok(view)
    }

    fn exec_exited(&mut self, id: &str, result: Result<ExitStatus>) {
        let Some(state) = self.execs.get_mut(id) else {
            return;
        };
        state.running = false;
        state.pid = 0;
        state.task = None;
        match result {
            Ok(status) => state.exit_code = status.exit_code,
            Err(error) => {
                tracing::warn!(exec = %id, error = %error, "exec wait failed");
                state.exit_code = 255;
            }
        }
    }

    /// Drain one captured pipe into the container's log store. Each read
    /// chunk becomes one record; the Engine API re-frames records with
    /// stdcopy headers, so granularity only affects coalescing. The lease
    /// keeps the follow EOF signal off until the pipe reaches EOF.
    fn spawn_pipe_capture(
        id: &ContainerId,
        logs: &Arc<LogStore>,
        pipe: Box<dyn Read + Send>,
        stream: Stream,
    ) {
        let lease = logs.take_writer();
        let sid = &id.as_str()[..12.min(id.as_str().len())];
        let name = format!(
            "docker-log-{sid}-{}",
            match stream {
                Stream::Stdout => "stdout",
                Stream::Stderr => "stderr",
            }
        );
        let spawned = thread::Builder::new().name(name).spawn(move || {
            let mut pipe = pipe;
            let mut buffer = [0u8; 8192];
            loop {
                match pipe.read(&mut buffer) {
                    Ok(0) | Err(_) => break,
                    Ok(count) => lease.append(stream, &buffer[..count]),
                }
            }
            // Lease drop releases the writer slot; the last one closes the
            // store so followers reach EOF.
        });
        // If the thread could not start, `spawn` drops the rejected closure,
        // which releases its lease: one failing pipe keeps the store open,
        // two failing pipes close it, both handled by the drop path.
        if let Err(_error) = spawned {
            tracing::warn!(container = %sid, "cannot start log capture thread");
        }
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

        let mut runtime_config = RuntimeConfig::new(
            id.clone(),
            record.config.clone(),
            record.host_config.clone(),
        );
        if let Some(image_id) = record.image_id.clone() {
            runtime_config = runtime_config.with_image_id(image_id);
        }
        // Capture stdio whenever the runtime supports it (log capture, later
        // attach); runtimes without pipes ignore this flag.
        runtime_config = runtime_config.with_capture_stdio(true);
        let task_box = self.runtime.create(&runtime_config)?;
        if let Err(error) = task_box.start() {
            let _ = task_box.delete();
            return Err(error);
        }
        let task: Arc<dyn RuntimeTask> = Arc::from(task_box);

        // Claim the runtime's pipes and drain them into the container's log
        // store before spawning the monitor, so `docker logs -f` sees every
        // buffered byte and can follow until the pipes close. Failure to
        // claim stdio is logged but never fatal: a running container is more
        // valuable than its captured log.
        if let Some(logs) = self.meta.get(id).map(|meta| Arc::clone(&meta.logs)) {
            logs.reopen();
            match task.take_stdio() {
                Ok(Some(stdio)) => {
                    // Attach is output-only and does not consume stdin, so
                    // closing the writer end gives the child EOF and matches
                    // detached (`-d`) semantics.
                    drop(stdio.stdin);
                    let mut pipes: Vec<(Box<dyn Read + Send>, Stream)> = Vec::new();
                    if let Some(stdout) = stdio.stdout {
                        pipes.push((stdout, Stream::Stdout));
                    }
                    if let Some(stderr) = stdio.stderr {
                        pipes.push((stderr, Stream::Stderr));
                    }
                    for (pipe, stream) in pipes {
                        Self::spawn_pipe_capture(id, &logs, pipe, stream);
                    }
                }
                Ok(None) => {}
                Err(error) => {
                    tracing::warn!(
                        container = %id.as_str(),
                        error = %error,
                        "cannot capture container stdio; logs will be empty"
                    );
                }
            }
        }

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
        self.notify_terminal(id, exit.exit_code);
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
            meta.exit_sequence = meta.exit_sequence.saturating_add(1);
        }
        self.set_exit_state(id, LifecycleState::Exited, &exit)?;
        self.emit_container(&record, KnownAction::Stop, &[]);
        self.notify_next_exit(id, exit.exit_code);
        self.notify_not_running(id, exit.exit_code);
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
        if let Some(meta) = self.meta.get_mut(id) {
            meta.exit_sequence = meta.exit_sequence.saturating_add(1);
        }

        // With no captured stdio (synthetic runtime, or capture failed) no
        // writer will ever flip the close signal; do it here so `docker logs
        // -f` follows until the exit. Containers with capture threads close
        // when those threads drain their pipes instead.
        if let Some(meta) = self.meta.get(id) {
            meta.logs.close_if_idle();
        }

        let exit = match result {
            Ok(exit) => exit,
            Err(error) => {
                self.notify_next_exit(id, 255);
                self.task_failed(id, duration, error);
                self.notify_not_running(id, 255);
                return;
            }
        };
        if let Some(meta) = self.meta.get_mut(id) {
            meta.last_exit = Some(exit.clone());
        }
        // `next-exit` observes the process result even when a restart policy
        // immediately starts a replacement task.
        self.notify_next_exit(id, exit.exit_code);

        if let Some(pending) = self.pending_stops.remove(id) {
            let PendingStop { reply, kind, .. } = pending;
            let outcome = self.finish_pending_stop(id, &exit, duration, kind);
            let _ = reply.send(outcome);
            return;
        }

        if self.shutting_down {
            let _ = self.set_exit_state(id, LifecycleState::Exited, &exit);
            self.notify_not_running(id, exit.exit_code);
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
                self.notify_not_running(id, exit.exit_code);
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
                self.notify_not_running(id, exit.exit_code);
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
        // Restarting is considered alive by `not-running`, so only a final
        // stop/remove satisfies that condition here. A restart publishes its
        // replacement run or a terminal failure instead.
        if kind != StopKind::Restart {
            self.notify_not_running(id, exit.exit_code);
        }
        match kind {
            StopKind::Stop => {
                self.maybe_auto_remove(id);
                Ok(())
            }
            StopKind::Restart => {
                if let Err(error) = self.start_task(id, true) {
                    // The previous task already published its `next-exit`
                    // outcome, but a waiter registered during the restart
                    // window must not be stranded when the replacement fails
                    // to start.
                    self.notify_terminal(id, exit.exit_code);
                    return Err(error);
                }
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
            self.notify_terminal(id, exit.exit_code);
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
                self.notify_terminal(id, exit.exit_code);
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

    fn persist_record(&self, record: &ContainerRecord) -> Result<()> {
        match &self.container_dir {
            Some(dir) => dir.save(record),
            None => Ok(()),
        }
    }

    fn open_logs(&self, id: &ContainerId) -> Result<Arc<LogStore>> {
        match &self.container_dir {
            Some(dir) => Ok(Arc::new(LogStore::open(
                dir.log_path(id),
                DEFAULT_LOG_CAPACITY,
            )?)),
            None => Ok(Arc::new(LogStore::new(DEFAULT_LOG_CAPACITY))),
        }
    }

    fn set_running(&mut self, id: &ContainerId) -> Result<()> {
        let mut updated = self.store.get(id)?.clone();
        validate_transition(updated.state.status, LifecycleState::Running)?;
        updated.state = StateSnapshot {
            status: LifecycleState::Running,
            exit_code: None,
            error_signal: None,
            started_at: Some(unix_now()),
            finished_at: None,
        };
        self.persist_record(&updated)?;
        self.store.get_mut(id)?.state = updated.state;
        Ok(())
    }

    fn set_exit_state(
        &mut self,
        id: &ContainerId,
        status: LifecycleState,
        exit: &ExitStatus,
    ) -> Result<()> {
        let mut updated = self.store.get(id)?.clone();
        validate_transition(updated.state.status, status)?;
        updated.state.status = status;
        updated.state.exit_code = Some(exit.exit_code);
        updated.state.error_signal = exit.signal.map(|signal| signal.as_str().to_string());
        updated.state.finished_at = Some(unix_now());
        // The process exit is an external fact: never keep the actor thinking a
        // container is running because the disk write failed. A stale `running`
        // record is repaired to `exited`/255 by the next daemon start.
        if let Err(error) = self.persist_record(&updated) {
            tracing::error!(
                container = %id.as_str(),
                error = %error,
                "cannot persist the exit state; the record on disk is stale"
            );
        }
        self.store.get_mut(id)?.state = updated.state;
        Ok(())
    }

    fn set_dead(&mut self, id: &ContainerId, exit: &ExitStatus) -> Result<()> {
        let mut updated = self.store.get(id)?.clone();
        validate_transition(updated.state.status, LifecycleState::Dead)?;
        updated.state = StateSnapshot {
            status: LifecycleState::Dead,
            exit_code: Some(exit.exit_code),
            error_signal: exit.signal.map(|signal| signal.as_str().to_string()),
            started_at: updated.state.started_at,
            finished_at: Some(unix_now()),
        };
        if let Err(error) = self.persist_record(&updated) {
            tracing::error!(
                container = %id.as_str(),
                error = %error,
                "cannot persist the dead state; the record on disk is stale"
            );
        }
        self.store.get_mut(id)?.state = updated.state;
        Ok(())
    }

    fn remove_record(&mut self, id: &ContainerId) -> Result<()> {
        let record = self.store.get(id)?.clone();
        let exec_ids: Vec<String> = self
            .execs
            .iter()
            .filter(|(_, exec)| &exec.container_id == id)
            .map(|(exec_id, _)| exec_id.clone())
            .collect();
        for exec_id in exec_ids {
            if let Some(exec) = self.execs.remove(&exec_id) {
                if let Some(task) = exec.task {
                    let _ = task.kill(Signal::Sigkill);
                }
                if let Some(logs) = exec.logs {
                    logs.close();
                }
            }
        }
        self.runtime.cleanup(id)?;
        // Force-close the log store so any follower stream ends even if a
        // capture thread is still draining a lingering pipe. `close` also
        // flushes and drops the log file handle, which Windows requires before
        // the directory can be removed.
        if let Some(meta) = self.meta.get(id) {
            meta.logs.close();
        }
        // Remove the on-disk record before dropping the in-memory one: if the
        // directory cannot be removed, the container stays addressable instead
        // of reappearing after the next daemon start.
        if let Some(dir) = &self.container_dir {
            dir.remove(id)?;
        }
        self.store.remove(id)?;
        // A removed container can no longer produce a new exit. Wake waiters
        // that were waiting for removal (and any not-running waiter that raced
        // with a forced remove) with the last known status.
        let status = self
            .meta
            .get(id)
            .and_then(|meta| meta.last_exit.as_ref())
            .map(|exit| exit.exit_code)
            .or(record.state.exit_code)
            .unwrap_or(0);
        self.notify_removed(id, status);
        self.notify_not_running(id, status);
        // No next exit can occur after removal; drop those blocked replies
        // rather than retaining waiters forever.
        self.waiters.remove(id);
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
        // Exec helpers live in the host PID namespace and are not children of
        // the container init. Kill them explicitly so daemon shutdown does not
        // leave a command running after its container task was torn down.
        for (_, exec) in self.execs.drain() {
            if let Some(task) = exec.task {
                let _ = task.kill(Signal::Sigkill);
            }
            if let Some(logs) = exec.logs {
                logs.close();
            }
        }
        // Nothing should keep followers waiting after the daemon is gone.
        for meta in self.meta.values() {
            meta.logs.close();
        }
        for (_, pending) in self.pending_stops.drain() {
            let _ = pending
                .reply
                .send(Err(Error::unavailable("daemon is shutting down")));
        }
        for (_, waiters) in self.waiters.drain() {
            for waiter in waiters {
                let _ = waiter
                    .reply
                    .send(Err(Error::unavailable("daemon is shutting down")));
            }
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
