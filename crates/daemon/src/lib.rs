//! Docker daemon orchestration core (Milestone D).
//!
//! This crate owns the container lifecycle but not process execution:
//!
//! * [`engine::Daemon`] is a single-owner actor around the container store.
//! * [`runtime`] is the containerd-like task boundary used by the supervisor.
//! * [`restart`] ports Moby's restart manager backoff/count rules.
//! * [`synthetic`] is a deterministic runtime for headless lifecycle tests.
//!
//! Milestone F/G exposes this actor over the Engine HTTP API; Milestone E
//! supplies the real OCI runtime implementation.

pub mod engine;
pub mod restart;
pub mod runtime;
pub mod synthetic;

pub use engine::{ContainerView, CreateRequest, Daemon};
pub use restart::{RestartDecision, RestartManager};
pub use runtime::{ExitStatus, Runtime, RuntimeConfig, RuntimeTask};
pub use synthetic::{SyntheticBehavior, SyntheticRuntime};
