//! Non-Linux backend: never pretend to isolate a Linux container.
//!
//! Windows containers require the Host Compute Service and a Windows base
//! image; the current milestone targets Linux OCI semantics. Reporting
//! `Unsupported` here is deliberate and test-covered.

use docker_errors::{Error, Result};

use crate::oci::OciRuntime;
use crate::task::{RuntimeConfig, RuntimeTask};

pub(crate) fn create_task(
    _runtime: &OciRuntime,
    _config: &RuntimeConfig,
) -> Result<Box<dyn RuntimeTask>> {
    Err(Error::unsupported(format!(
        "native OCI execution requires Linux namespaces and mount/chroot privileges; this host is {}",
        std::env::consts::OS
    )))
}
