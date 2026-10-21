//! Runtime boundary used by the daemon supervisor.
//!
//! This is the Rust equivalent of the slice of containerd's task API Moby
//! uses from `libcontainerd` (`Create`/`Start`/`Wait`/`Kill`/`Delete`). The
//! daemon deliberately does not know how tasks are executed: Milestone E
//! supplies the OCI implementation, while tests use the deterministic
//! [`crate::synthetic::SyntheticRuntime`].
//!
//! Contract expected by the actor:
//!
//! * `create` allocates a task but does not run it.
//! * `start` starts it exactly once.
//! * `wait` blocks until exit and is safe to call concurrently with `kill`;
//!   it may be called more than once and must return the cached status.
//! * `kill` only sends a signal; it does not wait for exit.
//! * `delete` releases the task after `wait` completed.

use docker_common::ContainerId;
use docker_container_model::config::{ContainerConfig, HostConfig, Signal};
use docker_errors::Result;

/// Everything a runtime needs to create one task. The full OCI bundle is
/// built in Milestone E; the supervisor passes the validated model onwards.
#[derive(Debug, Clone)]
pub struct RuntimeConfig {
    pub id: ContainerId,
    pub config: ContainerConfig,
    pub host_config: HostConfig,
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

/// Factory for tasks. Implementations must be usable from the daemon actor
/// and from monitor threads.
pub trait Runtime: Send + Sync + 'static {
    fn create(&self, config: &RuntimeConfig) -> Result<Box<dyn RuntimeTask>>;
}

/// One task (container init process) managed by the daemon.
pub trait RuntimeTask: Send + Sync {
    fn start(&self) -> Result<()>;
    fn wait(&self) -> Result<ExitStatus>;
    fn kill(&self, signal: Signal) -> Result<()>;
    fn delete(&self) -> Result<()>;
}
