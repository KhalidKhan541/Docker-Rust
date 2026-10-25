//! Native OCI runtime support (Milestone E).
//!
//! * [`spec`] contains the runtime-spec 1.3.0 Linux configuration and state
//!   types with strict field handling.
//! * [`bundle`] translates the daemon's container model into a validated
//!   OCI bundle (`config.json` plus an external rootfs path).
//! * [`task`] is the runtime/task boundary the daemon supervisor consumes.
//! * [`oci`] is the native implementation. Bundle generation is portable;
//!   execution currently requires Linux and fails explicitly elsewhere.
//!
//! The Linux init helper lives in `src/bin/docker-runtime-init.rs` and is
//! launched by `OciRuntime`; it is not a public daemon API.

pub mod bundle;
#[cfg(target_os = "linux")]
pub mod init;
pub mod oci;
pub mod spec;
pub mod task;

pub use bundle::{spec_from_container, Bundle, DEFAULT_PATH_ENV};
pub use oci::{ensure_supported, OciRuntime, RUNTIME_INIT_ENV};
pub use spec::{ContainerState, Spec, State, OCI_VERSION};
pub use task::{ExitStatus, RootfsProvider, Runtime, RuntimeConfig, RuntimeTask, StdioHandles};
