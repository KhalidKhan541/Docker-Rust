//! Deterministic in-process runtime used to exercise daemon orchestration
//! without executing a process.
//!
//! It is intentionally a first-class implementation of [`Runtime`] rather
//! than a mock hidden in tests: the same supervisor that Milestone E drives
//! with OCI tasks is exercised here. Behaviour is controlled per runtime
//! instance and recorded so tests can assert create/start/kill/delete calls.

use std::io::Cursor;
use std::sync::{Arc, Condvar, Mutex};
use std::time::{Duration, Instant};

use docker_common::ContainerId;
use docker_container_model::config::Signal;
use docker_errors::{Error, Result};

use crate::runtime::{ExecConfig, ExitStatus, Runtime, RuntimeConfig, RuntimeTask, StdioHandles};

/// Scripted task behaviour.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct SyntheticBehavior {
    /// `None` means the task runs until killed.
    pub exit_after: Option<Duration>,
    pub exit_code: i32,
    pub exit_signal: Option<Signal>,
    /// Mirror a process that handles SIGTERM: the stop timer must escalate.
    pub ignore_sigterm: bool,
    pub fail_create: bool,
    pub fail_start: bool,
    pub fail_wait: bool,
}

impl Default for SyntheticBehavior {
    fn default() -> Self {
        Self {
            exit_after: Some(Duration::from_millis(25)),
            exit_code: 0,
            exit_signal: None,
            ignore_sigterm: false,
            fail_create: false,
            fail_start: false,
            fail_wait: false,
        }
    }
}

impl SyntheticBehavior {
    /// Runs until a kill is delivered.
    pub const fn running_until_killed() -> Self {
        Self {
            exit_after: None,
            exit_code: 0,
            exit_signal: None,
            ignore_sigterm: false,
            fail_create: false,
            fail_start: false,
            fail_wait: false,
        }
    }

    /// Exits on its own after `after`, with `exit_code`.
    pub const fn exits_after(after: Duration, exit_code: i32) -> Self {
        Self {
            exit_after: Some(after),
            exit_code,
            exit_signal: None,
            ignore_sigterm: false,
            fail_create: false,
            fail_start: false,
            fail_wait: false,
        }
    }

    /// Refuses task creation.
    pub const fn failing_create() -> Self {
        Self {
            fail_create: true,
            ..Self::running_until_killed()
        }
    }

    /// Creates a task but refuses `start`.
    pub const fn failing_start() -> Self {
        Self {
            fail_start: true,
            ..Self::running_until_killed()
        }
    }

    /// Creates and starts, but wait reports a runtime error.
    pub const fn failing_wait() -> Self {
        Self {
            fail_wait: true,
            ..Self::running_until_killed()
        }
    }

    /// Handles (ignores) SIGTERM; only SIGKILL terminates it.
    pub const fn ignoring_sigterm() -> Self {
        Self {
            ignore_sigterm: true,
            ..Self::running_until_killed()
        }
    }
}

#[derive(Debug, Default)]
struct SyntheticInner {
    behavior: SyntheticBehavior,
    exec_behavior: Option<SyntheticExecBehavior>,
    created: Vec<ContainerId>,
    started: Vec<ContainerId>,
    kills: Vec<(ContainerId, Signal)>,
    deleted: usize,
}

/// Scripted output and exit status for a synthetic `exec` task.
///
/// The production daemon leaves this unset, so the synthetic runtime continues
/// to reject exec explicitly. Tests opt in to exercise the API and actor state
/// machine without a Linux namespace environment.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct SyntheticExecBehavior {
    pub stdout: Vec<u8>,
    pub stderr: Vec<u8>,
    pub exit_code: i32,
    /// `None` keeps the exec running until it is killed.
    pub exit_after: Option<Duration>,
}

impl SyntheticExecBehavior {
    /// Emit the given streams and exit immediately with `exit_code`.
    pub fn immediate(
        stdout: impl Into<Vec<u8>>,
        stderr: impl Into<Vec<u8>>,
        exit_code: i32,
    ) -> Self {
        Self {
            stdout: stdout.into(),
            stderr: stderr.into(),
            exit_code,
            exit_after: Some(Duration::ZERO),
        }
    }
}

/// Scripted runtime. Cloneable handles share observation state.
#[derive(Debug, Clone)]
pub struct SyntheticRuntime {
    inner: Arc<Mutex<SyntheticInner>>,
}

impl Default for SyntheticRuntime {
    fn default() -> Self {
        Self::new()
    }
}

impl SyntheticRuntime {
    pub fn new() -> Self {
        Self {
            inner: Arc::new(Mutex::new(SyntheticInner {
                behavior: SyntheticBehavior::default(),
                ..SyntheticInner::default()
            })),
        }
    }

    pub fn with_behavior(behavior: SyntheticBehavior) -> Self {
        let runtime = Self::new();
        runtime.set_behavior(behavior);
        runtime
    }

    /// Applies to tasks created after this call.
    pub fn set_behavior(&self, behavior: SyntheticBehavior) {
        self.inner
            .lock()
            .expect("synthetic runtime lock poisoned")
            .behavior = behavior;
    }

    /// Enable scripted `exec` support. Without this, `RuntimeTask::exec`
    /// returns `Unsupported` like the production synthetic runtime.
    pub fn with_exec_behavior(self, behavior: SyntheticExecBehavior) -> Self {
        self.set_exec_behavior(behavior);
        self
    }

    pub fn set_exec_behavior(&self, behavior: SyntheticExecBehavior) {
        self.inner
            .lock()
            .expect("synthetic runtime lock poisoned")
            .exec_behavior = Some(behavior);
    }

    pub fn created(&self) -> Vec<ContainerId> {
        self.inner
            .lock()
            .expect("synthetic runtime lock poisoned")
            .created
            .clone()
    }

    pub fn started(&self) -> Vec<ContainerId> {
        self.inner
            .lock()
            .expect("synthetic runtime lock poisoned")
            .started
            .clone()
    }

    pub fn kills(&self) -> Vec<(ContainerId, Signal)> {
        self.inner
            .lock()
            .expect("synthetic runtime lock poisoned")
            .kills
            .clone()
    }

    pub fn deleted(&self) -> usize {
        self.inner
            .lock()
            .expect("synthetic runtime lock poisoned")
            .deleted
    }

    fn behavior(&self) -> SyntheticBehavior {
        self.inner
            .lock()
            .expect("synthetic runtime lock poisoned")
            .behavior
    }

    fn exec_behavior(&self) -> Option<SyntheticExecBehavior> {
        self.inner
            .lock()
            .expect("synthetic runtime lock poisoned")
            .exec_behavior
            .clone()
    }

    fn record_start(&self, id: &ContainerId) {
        self.inner
            .lock()
            .expect("synthetic runtime lock poisoned")
            .started
            .push(id.clone());
    }

    fn record_kill(&self, id: &ContainerId, signal: Signal) {
        self.inner
            .lock()
            .expect("synthetic runtime lock poisoned")
            .kills
            .push((id.clone(), signal));
    }

    fn record_delete(&self) {
        self.inner
            .lock()
            .expect("synthetic runtime lock poisoned")
            .deleted += 1;
    }
}

impl Runtime for SyntheticRuntime {
    fn create(&self, config: &RuntimeConfig) -> Result<Box<dyn RuntimeTask>> {
        let behavior = self.behavior();
        if behavior.fail_create {
            return Err(Error::unavailable(
                "synthetic runtime refused task creation",
            ));
        }
        {
            let mut inner = self.inner.lock().expect("synthetic runtime lock poisoned");
            inner.created.push(config.id.clone());
        }
        Ok(Box::new(SyntheticTask {
            id: config.id.clone(),
            behavior,
            state: Arc::new((
                Mutex::new(TaskInner {
                    started_at: None,
                    exit: None,
                    deleted: false,
                }),
                Condvar::new(),
            )),
            runtime: self.clone(),
        }))
    }
}

#[derive(Debug)]
struct TaskInner {
    started_at: Option<Instant>,
    exit: Option<ExitStatus>,
    deleted: bool,
}

#[derive(Debug)]
struct SyntheticTask {
    id: ContainerId,
    behavior: SyntheticBehavior,
    state: Arc<(Mutex<TaskInner>, Condvar)>,
    runtime: SyntheticRuntime,
}

impl SyntheticTask {
    fn natural_exit(&self) -> ExitStatus {
        ExitStatus {
            exit_code: self.behavior.exit_code,
            signal: self.behavior.exit_signal,
        }
    }
}

impl RuntimeTask for SyntheticTask {
    fn start(&self) -> Result<()> {
        let (lock, cv) = &*self.state;
        let mut inner = lock.lock().expect("synthetic task lock poisoned");
        if inner.started_at.is_some() {
            return Err(Error::conflict("synthetic task already started"));
        }
        if self.behavior.fail_start {
            return Err(Error::unavailable("synthetic runtime refused task start"));
        }
        inner.started_at = Some(Instant::now());
        self.runtime.record_start(&self.id);
        cv.notify_all();
        Ok(())
    }

    fn wait(&self) -> Result<ExitStatus> {
        if self.behavior.fail_wait {
            return Err(Error::unavailable("synthetic runtime wait failure"));
        }
        let (lock, cv) = &*self.state;
        let mut inner = lock.lock().expect("synthetic task lock poisoned");
        loop {
            if let Some(exit) = &inner.exit {
                return Ok(exit.clone());
            }
            let Some(started_at) = inner.started_at else {
                return Err(Error::unavailable("synthetic task was not started"));
            };
            if let Some(after) = self.behavior.exit_after {
                let deadline = started_at + after;
                let now = Instant::now();
                if now >= deadline {
                    let exit = self.natural_exit();
                    inner.exit = Some(exit.clone());
                    cv.notify_all();
                    return Ok(exit);
                }
                let (guard, _) = cv
                    .wait_timeout(inner, deadline - now)
                    .expect("synthetic task lock poisoned");
                inner = guard;
            } else {
                inner = cv.wait(inner).expect("synthetic task lock poisoned");
            }
        }
    }

    fn kill(&self, signal: Signal) -> Result<()> {
        let (lock, cv) = &*self.state;
        let mut inner = lock.lock().expect("synthetic task lock poisoned");
        if inner.exit.is_some() {
            self.runtime.record_kill(&self.id, signal);
            return Ok(());
        }
        if signal == Signal::Sigterm && self.behavior.ignore_sigterm {
            self.runtime.record_kill(&self.id, signal);
            return Ok(());
        }
        let exit = ExitStatus {
            exit_code: 128 + signal.number(),
            signal: Some(signal),
        };
        inner.exit = Some(exit);
        cv.notify_all();
        drop(inner);
        self.runtime.record_kill(&self.id, signal);
        Ok(())
    }

    fn delete(&self) -> Result<()> {
        let (lock, _) = &*self.state;
        let mut inner = lock.lock().expect("synthetic task lock poisoned");
        if !inner.deleted {
            inner.deleted = true;
            self.runtime.record_delete();
        }
        Ok(())
    }

    fn exec(&self, config: &ExecConfig) -> Result<Box<dyn RuntimeTask>> {
        if config.argv.is_empty() {
            return Err(Error::invalid_argument("exec argv must not be empty"));
        }
        let behavior = self.runtime.exec_behavior().ok_or_else(|| {
            Error::unsupported("exec is not implemented by this synthetic runtime")
        })?;
        let (lock, _) = &*self.state;
        let inner = lock.lock().expect("synthetic task lock poisoned");
        if inner.deleted {
            return Err(Error::conflict("synthetic task was deleted"));
        }
        if inner.started_at.is_none() {
            return Err(Error::conflict("synthetic task has not been started"));
        }
        if inner.exit.is_some() {
            return Err(Error::conflict("synthetic task has already exited"));
        }
        drop(inner);
        Ok(Box::new(SyntheticExecTask::new(
            behavior,
            config.capture_stdio,
        )))
    }
}

#[derive(Debug)]
struct ExecInner {
    started_at: Option<Instant>,
    exit: Option<ExitStatus>,
    deleted: bool,
    stdio_taken: bool,
}

#[derive(Debug)]
struct SyntheticExecTask {
    behavior: SyntheticExecBehavior,
    capture_stdio: bool,
    state: Arc<(Mutex<ExecInner>, Condvar)>,
}

impl SyntheticExecTask {
    fn new(behavior: SyntheticExecBehavior, capture_stdio: bool) -> Self {
        Self {
            behavior,
            capture_stdio,
            state: Arc::new((
                Mutex::new(ExecInner {
                    started_at: None,
                    exit: None,
                    deleted: false,
                    stdio_taken: false,
                }),
                Condvar::new(),
            )),
        }
    }

    fn natural_exit(&self) -> ExitStatus {
        ExitStatus::from_code(self.behavior.exit_code)
    }
}

impl RuntimeTask for SyntheticExecTask {
    fn start(&self) -> Result<()> {
        let (lock, cv) = &*self.state;
        let mut inner = lock.lock().expect("synthetic exec lock poisoned");
        if inner.deleted {
            return Err(Error::conflict("synthetic exec was deleted"));
        }
        if inner.started_at.is_some() {
            return Err(Error::conflict("synthetic exec already started"));
        }
        inner.started_at = Some(Instant::now());
        cv.notify_all();
        Ok(())
    }

    fn wait(&self) -> Result<ExitStatus> {
        let (lock, cv) = &*self.state;
        let mut inner = lock.lock().expect("synthetic exec lock poisoned");
        loop {
            if let Some(exit) = &inner.exit {
                return Ok(exit.clone());
            }
            let Some(started_at) = inner.started_at else {
                return Err(Error::unavailable("synthetic exec was not started"));
            };
            if let Some(after) = self.behavior.exit_after {
                let deadline = started_at + after;
                let now = Instant::now();
                if now >= deadline {
                    let exit = self.natural_exit();
                    inner.exit = Some(exit.clone());
                    cv.notify_all();
                    return Ok(exit);
                }
                let (guard, _) = cv
                    .wait_timeout(inner, deadline - now)
                    .expect("synthetic exec lock poisoned");
                inner = guard;
            } else {
                inner = cv.wait(inner).expect("synthetic exec lock poisoned");
            }
        }
    }

    fn kill(&self, signal: Signal) -> Result<()> {
        let (lock, cv) = &*self.state;
        let mut inner = lock.lock().expect("synthetic exec lock poisoned");
        if inner.exit.is_none() {
            inner.exit = Some(ExitStatus {
                exit_code: 128 + signal.number(),
                signal: Some(signal),
            });
            cv.notify_all();
        }
        Ok(())
    }

    fn delete(&self) -> Result<()> {
        let (lock, _) = &*self.state;
        let mut inner = lock.lock().expect("synthetic exec lock poisoned");
        inner.deleted = true;
        Ok(())
    }

    fn take_stdio(&self) -> Result<Option<StdioHandles>> {
        let (lock, _) = &*self.state;
        let mut inner = lock.lock().expect("synthetic exec lock poisoned");
        if !self.capture_stdio || inner.stdio_taken {
            return Ok(None);
        }
        inner.stdio_taken = true;
        Ok(Some(StdioHandles {
            stdin: None,
            stdout: Some(Box::new(Cursor::new(self.behavior.stdout.clone()))),
            stderr: Some(Box::new(Cursor::new(self.behavior.stderr.clone()))),
        }))
    }
}
