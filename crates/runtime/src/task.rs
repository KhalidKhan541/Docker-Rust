//! Runtime boundary used by the daemon supervisor.
//!
//! This is the Rust equivalent of the slice of containerd's task API Moby
//! uses from `libcontainerd` (`Create`/`Start`/`Wait`/`Kill`/`Delete`). The
//! daemon deliberately does not know how tasks are executed: this repository
//! provides the OCI implementation in [`crate::oci::OciRuntime`], while the
//! daemon's deterministic test runtime implements the same traits.
//!
//! Contract expected by the actor:
//!
//! * `create` allocates a task but does not run it.
//! * `start` starts it exactly once.
//! * `wait` blocks until exit and is safe to call concurrently with `kill`;
//!   it may be called more than once and must return the cached status.
//! * `kill` only sends a signal; it does not wait for exit.
//! * `delete` releases the task after `wait` completed.
//!
//! `take_stdio` is separate from `start` so the HTTP attach layer can claim
//! the pipes without changing the supervisor's lifecycle contract. Runtimes
//! that do not expose streams keep the default implementation.

use std::path::PathBuf;
use std::process::{ChildStderr, ChildStdin, ChildStdout};

use docker_common::{ContainerId, Digest};
use docker_container_model::config::{ContainerConfig, HostConfig, Signal};
use docker_errors::Result;

/// Everything a runtime needs to create one task.
#[derive(Debug, Clone)]
pub struct RuntimeConfig {
    pub id: ContainerId,
    pub config: ContainerConfig,
    pub host_config: HostConfig,
    /// Image ID resolved when the container was created. It pins the layer
    /// set across tag moves and daemon restarts; the daemon supplies it when
    /// the create request resolved an image.
    pub image_id: Option<Digest>,
    /// Already prepared root filesystem. When absent, the configured
    /// [`RootfsProvider`] materializes it from `image_id`/`config.image`.
    pub rootfs: Option<PathBuf>,
    /// Capture a container's stdio into runtime-owned pipes so the daemon can
    /// claim them with `RuntimeTask::take_stdio` (log capture, attach). When
    /// false, streams are inherited by the daemon process.
    pub capture_stdio: bool,
}

impl RuntimeConfig {
    pub fn new(id: ContainerId, config: ContainerConfig, host_config: HostConfig) -> Self {
        Self {
            id,
            config,
            host_config,
            image_id: None,
            rootfs: None,
            capture_stdio: false,
        }
    }

    pub fn with_rootfs(mut self, rootfs: impl Into<PathBuf>) -> Self {
        self.rootfs = Some(rootfs.into());
        self
    }

    pub fn with_image_id(mut self, image_id: Digest) -> Self {
        self.image_id = Some(image_id);
        self
    }

    /// Request runtime-owned stdio pipes (see `RuntimeTask::take_stdio`).
    pub fn with_capture_stdio(mut self, capture: bool) -> Self {
        self.capture_stdio = capture;
        self
    }
}

/// Prepares and removes per-container root filesystems.
///
/// The runtime owns execution, not image storage. A provider is therefore
/// supplied by the daemon binary and called lazily on first start. It must be
/// safe to call `prepare` again after a crash: a complete rootfs may be
/// reused, while a partial one must be replaced atomically.
pub trait RootfsProvider: Send + Sync + 'static {
    fn prepare(
        &self,
        id: &ContainerId,
        config: &ContainerConfig,
        image_id: Option<&Digest>,
    ) -> Result<PathBuf>;

    /// Remove the persisted rootfs for a container that is being deleted.
    fn cleanup(&self, id: &ContainerId) -> Result<()> {
        let _ = id;
        Ok(())
    }
}

/// Pipes owned by the runtime until the attach layer claims them.
#[derive(Debug)]
pub struct StdioHandles {
    pub stdin: Option<ChildStdin>,
    pub stdout: Option<ChildStdout>,
    pub stderr: Option<ChildStderr>,
}

impl StdioHandles {
    pub fn is_empty(&self) -> bool {
        self.stdin.is_none() && self.stdout.is_none() && self.stderr.is_none()
    }
}

/// Process termination reported by the runtime.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ExitStatus {
    /// Process exit code (`128 + signal` for signal termination, matching
    /// containerd's shim reporting).
    pub exit_code: i32,
    /// Terminating signal when the runtime reports one.
    pub signal: Option<Signal>,
}

impl ExitStatus {
    /// Build a status from a conventional exit code; signal termination is
    /// recognized for the POSIX `128 + signo` convention.
    pub fn from_code(exit_code: i32) -> Self {
        let signal = match exit_code {
            129 => Some(Signal::Sighup),
            130 => Some(Signal::Sigint),
            131 => Some(Signal::Sigquit),
            137 => Some(Signal::Sigkill),
            138 => Some(Signal::Sigusr1),
            140 => Some(Signal::Sigusr2),
            141 => Some(Signal::Sigpipe),
            142 => Some(Signal::Sigalrm),
            143 => Some(Signal::Sigterm),
            _ => None,
        };
        Self { exit_code, signal }
    }

    pub fn success(&self) -> bool {
        self.exit_code == 0
    }
}

/// Factory for tasks. Implementations must be usable from the daemon actor
/// and from monitor threads.
pub trait Runtime: Send + Sync + 'static {
    fn create(&self, config: &RuntimeConfig) -> Result<Box<dyn RuntimeTask>>;

    /// Release runtime-owned resources for a container that was removed.
    /// Synthetic runtimes keep no such resources and use the default.
    fn cleanup(&self, id: &ContainerId) -> Result<()> {
        let _ = id;
        Ok(())
    }
}

/// One task (container init process) managed by the daemon.
pub trait RuntimeTask: Send + Sync {
    fn start(&self) -> Result<()>;
    fn wait(&self) -> Result<ExitStatus>;
    fn kill(&self, signal: Signal) -> Result<()>;
    fn delete(&self) -> Result<()>;

    /// Claim the task's stdio pipes, if this runtime exposes them. At most
    /// one caller can take them; later calls return `Ok(None)`.
    fn take_stdio(&self) -> Result<Option<StdioHandles>> {
        Ok(None)
    }
}
