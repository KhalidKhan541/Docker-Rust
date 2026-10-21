//! Re-export of the runtime boundary.
//!
//! The traits moved to `docker-runtime` in Milestone E so the OCI
//! implementation and the daemon can share one contract without a dependency
//! cycle. Existing daemon-internal paths remain valid.

pub use docker_runtime::task::{ExitStatus, Runtime, RuntimeConfig, RuntimeTask, StdioHandles};
