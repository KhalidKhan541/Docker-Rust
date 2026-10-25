//! Linux process backend.
//!
//! Preparation is split between `create` (bundle materialization) and
//! `start` (spawn the init helper). The helper receives the bundle path and
//! applies the OCI namespaces/rootfs/identity before spawning the configured
//! process. This keeps the daemon's actor free of `pre_exec` hooks and lets
//! `wait` remain a plain child wait.

use std::fs;
use std::os::unix::process::CommandExt;
use std::process::{Child, Command, Stdio};
use std::sync::Mutex;

use docker_container_model::config::Signal;
use docker_errors::{Error, Result};
use nix::errno::Errno;
use nix::sys::signal::{kill, killpg, Signal as NixSignal};
use nix::unistd::Pid;

use crate::bundle::{spec_from_container, Bundle};
use crate::oci::{ensure_supported, OciRuntime};
use crate::task::{ExitStatus, RuntimeConfig, RuntimeTask, StdioHandles};

pub(crate) fn create_task(
    runtime: &OciRuntime,
    config: &RuntimeConfig,
) -> Result<Box<dyn RuntimeTask>> {
    if !nix::unistd::Uid::effective().is_root() {
        return Err(Error::permission_denied(
            "the native OCI runtime requires root; run docker-rsd as root or select --runtime synthetic",
        ));
    }

    let prepared = match &config.rootfs {
        Some(_) => None,
        None => {
            let provider = runtime.rootfs_provider.as_ref().ok_or_else(|| {
                Error::invalid_argument(
                    "native OCI runtime requires a prepared rootfs or a configured image-rootfs provider",
                )
            })?;
            Some(provider.prepare(&config.id, &config.config, config.image_id.as_ref())?)
        }
    };
    let rootfs = config
        .rootfs
        .as_deref()
        .or(prepared.as_deref())
        .expect("rootfs is present when prepared is None");

    let spec = spec_from_container(&config.id, &config.config, &config.host_config, rootfs)?;
    ensure_supported(&spec)?;

    fs::create_dir_all(&runtime.state_root).map_err(|err| {
        Error::internal(format!(
            "creating runtime state root `{}`: {err}",
            runtime.state_root.display()
        ))
    })?;
    let state_root = fs::canonicalize(&runtime.state_root).map_err(|err| {
        Error::internal(format!(
            "canonicalizing runtime state root `{}`: {err}",
            runtime.state_root.display()
        ))
    })?;

    let init = runtime.resolve_init_program()?;
    let bundle = Bundle::create(state_root.join(config.id.as_str()), spec)?;
    Ok(Box::new(OciTask::new(
        bundle,
        config.config.open_stdin,
        runtime.capture_stdio || config.capture_stdio,
        init,
    )))
}

struct TaskState {
    child: Option<Child>,
    pid: Option<i32>,
    status: Option<ExitStatus>,
    started: bool,
    deleted: bool,
}

struct OciTask {
    bundle: Bundle,
    open_stdin: bool,
    capture_stdio: bool,
    init_program: std::path::PathBuf,
    state: Mutex<TaskState>,
}

impl OciTask {
    fn new(
        bundle: Bundle,
        open_stdin: bool,
        capture_stdio: bool,
        init_program: std::path::PathBuf,
    ) -> Self {
        Self {
            bundle,
            open_stdin,
            capture_stdio,
            init_program,
            state: Mutex::new(TaskState {
                child: None,
                pid: None,
                status: None,
                started: false,
                deleted: false,
            }),
        }
    }
}

impl RuntimeTask for OciTask {
    fn start(&self) -> Result<()> {
        let mut state = self
            .state
            .lock()
            .map_err(|_| Error::internal("OCI task state lock poisoned"))?;
        if state.deleted {
            return Err(Error::conflict("OCI task was deleted"));
        }
        if state.started {
            return Err(Error::conflict("OCI task was already started"));
        }
        if state.status.is_some() {
            return Err(Error::conflict("OCI task already exited"));
        }

        let mut command = Command::new(&self.init_program);
        command
            .arg("--bundle")
            .arg(self.bundle.root())
            .process_group(0)
            .stdin(if self.open_stdin {
                Stdio::piped()
            } else {
                Stdio::null()
            })
            .stdout(if self.capture_stdio {
                Stdio::piped()
            } else {
                Stdio::inherit()
            })
            .stderr(if self.capture_stdio {
                Stdio::piped()
            } else {
                Stdio::inherit()
            });
        let child = command.spawn().map_err(|err| {
            Error::unavailable(format!(
                "starting runtime init for container `{}`: {err}",
                self.bundle.root().display()
            ))
        })?;
        state.child = Some(child);
        state.pid = state.child.as_ref().map(|child| child.id() as i32);
        state.started = true;
        Ok(())
    }

    fn wait(&self) -> Result<ExitStatus> {
        let mut child = {
            let mut state = self
                .state
                .lock()
                .map_err(|_| Error::internal("OCI task state lock poisoned"))?;
            if let Some(status) = &state.status {
                return Ok(status.clone());
            }
            state.child.take().ok_or_else(|| {
                if state.started {
                    Error::conflict("another waiter is already waiting for this OCI task")
                } else {
                    Error::conflict("OCI task has not been started")
                }
            })?
        };

        let status = child.wait();
        let mapped = match &status {
            Ok(status) => Ok(map_exit_status(status)),
            Err(err) => Err(Error::internal(format!(
                "waiting for OCI task `{}`: {err}",
                self.bundle.root().display()
            ))),
        };

        let mut state = self
            .state
            .lock()
            .map_err(|_| Error::internal("OCI task state lock poisoned"))?;
        if let Ok(exit) = &mapped {
            state.status = Some(exit.clone());
        } else if state.child.is_none() {
            // Preserve the child handle if wait failed so delete can still
            // clean up. The process has not been confidently reaped.
            state.child = Some(child);
        }
        mapped
    }

    fn kill(&self, signal: Signal) -> Result<()> {
        let state = self
            .state
            .lock()
            .map_err(|_| Error::internal("OCI task state lock poisoned"))?;
        if state.status.is_some() {
            return Ok(());
        }
        let pid = state
            .pid
            .or_else(|| state.child.as_ref().map(|child| child.id() as i32))
            .ok_or_else(|| Error::conflict("OCI task is not running"))?;
        let nix_signal = NixSignal::try_from(signal.number())
            .map_err(|_| Error::invalid_argument(format!("unsupported signal {signal:?}")))?;

        let result = if signal == Signal::Sigkill {
            // The helper is the process-group leader; SIGKILL cannot be
            // forwarded, so terminate the whole group atomically.
            killpg(Pid::from_raw(pid), nix_signal)
        } else {
            kill(Pid::from_raw(pid), nix_signal)
        };
        match result {
            Ok(()) | Err(Errno::ESRCH) => Ok(()),
            Err(err) => Err(Error::internal(format!(
                "signaling OCI task pid {pid} with {signal:?}: {err}"
            ))),
        }
    }

    fn delete(&self) -> Result<()> {
        let child = {
            let mut state = self
                .state
                .lock()
                .map_err(|_| Error::internal("OCI task state lock poisoned"))?;
            if state.deleted {
                return Ok(());
            }
            if state.status.is_none() && state.child.is_none() && state.started {
                return Err(Error::conflict(
                    "cannot delete an OCI task while a wait is in progress",
                ));
            }
            state.deleted = true;
            state.child.take()
        };

        if let Some(mut child) = child {
            match killpg(Pid::from_raw(child.id() as i32), NixSignal::SIGKILL) {
                Ok(()) | Err(Errno::ESRCH) => {}
                Err(err) => {
                    self.restore_after_failed_delete(Some(child));
                    return Err(Error::internal(format!(
                        "killing OCI task group during delete: {err}"
                    )));
                }
            }
            let status = match child.wait() {
                Ok(status) => status,
                Err(err) => {
                    self.restore_after_failed_delete(Some(child));
                    return Err(Error::internal(format!(
                        "waiting for OCI task during delete `{}`: {err}",
                        self.bundle.root().display()
                    )));
                }
            };
            let mut state = self
                .state
                .lock()
                .map_err(|_| Error::internal("OCI task state lock poisoned"))?;
            state.status = Some(map_exit_status(&status));
            state.pid = None;
        }

        if let Err(err) = self.bundle.remove() {
            self.restore_after_failed_delete(None);
            return Err(err);
        }
        Ok(())
    }

    fn take_stdio(&self) -> Result<Option<StdioHandles>> {
        let mut state = self
            .state
            .lock()
            .map_err(|_| Error::internal("OCI task state lock poisoned"))?;
        let Some(child) = state.child.as_mut() else {
            return Ok(None);
        };
        let handles = StdioHandles {
            stdin: child.stdin.take(),
            stdout: child.stdout.take(),
            stderr: child.stderr.take(),
        };
        if handles.is_empty() {
            Ok(None)
        } else {
            Ok(Some(handles))
        }
    }
}

impl OciTask {
    fn restore_after_failed_delete(&self, child: Option<Child>) {
        if let Ok(mut state) = self.state.lock() {
            state.deleted = false;
            if child.is_some() {
                state.child = child;
            }
        }
    }
}

impl Drop for OciTask {
    fn drop(&mut self) {
        if let Ok(state) = self.state.get_mut() {
            if let Some(mut child) = state.child.take() {
                let _ = killpg(Pid::from_raw(child.id() as i32), NixSignal::SIGKILL);
                let _ = child.wait();
            }
        }
        let _ = self.bundle.remove();
    }
}

fn map_exit_status(status: &std::process::ExitStatus) -> ExitStatus {
    use std::os::unix::process::ExitStatusExt;
    if let Some(code) = status.code() {
        ExitStatus::from_code(code)
    } else if let Some(signal) = status.signal() {
        ExitStatus::from_code(128 + signal)
    } else {
        ExitStatus {
            exit_code: 255,
            signal: None,
        }
    }
}
