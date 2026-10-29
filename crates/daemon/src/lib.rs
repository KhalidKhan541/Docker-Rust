//! Docker daemon orchestration core (Milestone D).
//!
//! This crate owns the container lifecycle but not process execution:
//!
//! * [`engine::Daemon`] is a single-owner actor around the container store.
//! * [`runtime`] is the containerd-like task boundary used by the supervisor.
//! * [`logs`] captures container stdio into bounded in-memory ring buffers,
//!   the in-memory analog of Moby's json-file log driver.
//! * [`restart`] ports Moby's restart manager backoff/count rules.
//! * [`synthetic`] is a deterministic runtime for headless lifecycle tests.
//!
//! With `Daemon::spawn_persistent`, container records are written under
//! `<data-root>/containers/<id>/container.json` and restored at startup;
//! `Daemon::spawn` keeps the original ephemeral, in-memory behavior.
//!
//! Milestone F/G exposes this actor over the Engine HTTP API; Milestone E
//! supplies the real OCI runtime implementation.

pub mod engine;
pub mod logs;
mod persist;
pub mod restart;
pub mod runtime;
pub mod synthetic;

pub use engine::{
    ContainerView, CreateRequest, Daemon, ExecConfig, ExecProcessConfig, ExecSession, ExecView,
    WaitCondition, WaitOutcome,
};
pub use logs::{LogEntry, LogStore, Stream, WriterLease};
pub use restart::{RestartDecision, RestartManager};
pub use runtime::{ExitStatus, Runtime, RuntimeConfig, RuntimeTask};
pub use synthetic::{SyntheticBehavior, SyntheticExecBehavior, SyntheticRuntime};
