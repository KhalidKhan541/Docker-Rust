//! Native OCI runtime entry point.
//!
//! The platform module owns process creation. This module owns the policy
//! that decides which valid OCI features the current backend can actually
//! honor. A feature is never silently dropped: anything not implemented
//! returns `Unsupported` with the missing capability named.

use std::fmt;
use std::path::{Path, PathBuf};
use std::sync::Arc;

use docker_errors::{Error, Result};

use crate::spec::{LinuxNamespaceType, Process, Spec};
use crate::task::{RootfsProvider, Runtime, RuntimeConfig, RuntimeTask};

#[cfg(target_os = "linux")]
mod linux;
#[cfg(not(target_os = "linux"))]
mod unsupported;

#[cfg(target_os = "linux")]
use linux::create_task;
#[cfg(not(target_os = "linux"))]
use unsupported::create_task;

/// Environment override for the runtime init helper. Useful for packaged
/// installations and for tests that build the helper in a target directory.
pub const RUNTIME_INIT_ENV: &str = "DOCKER_RS_RUNTIME_INIT";

/// Environment override for the namespace-entering exec helper.
pub const RUNTIME_EXEC_ENV: &str = "DOCKER_RS_RUNTIME_EXEC";

/// Native OCI runtime. Bundle generation is portable; process execution is
/// currently implemented only on Linux.
#[derive(Clone)]
pub struct OciRuntime {
    pub(crate) state_root: PathBuf,
    #[cfg_attr(not(target_os = "linux"), allow(dead_code))]
    pub(crate) init_program: Option<PathBuf>,
    #[cfg_attr(not(target_os = "linux"), allow(dead_code))]
    pub(crate) exec_program: Option<PathBuf>,
    pub(crate) capture_stdio: bool,
    pub(crate) rootfs_provider: Option<Arc<dyn RootfsProvider>>,
}

impl fmt::Debug for OciRuntime {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("OciRuntime")
            .field("state_root", &self.state_root)
            .field("init_program", &self.init_program)
            .field("exec_program", &self.exec_program)
            .field("capture_stdio", &self.capture_stdio)
            .field("has_rootfs_provider", &self.rootfs_provider.is_some())
            .finish()
    }
}

impl OciRuntime {
    /// Create a runtime that stores bundles under `state_root`.
    pub fn new(state_root: impl Into<PathBuf>) -> Self {
        Self {
            state_root: state_root.into(),
            init_program: None,
            exec_program: None,
            capture_stdio: false,
            rootfs_provider: None,
        }
    }

    /// Supply the provider used to materialize root filesystems when a task
    /// is created without `RuntimeConfig::with_rootfs`.
    pub fn with_rootfs_provider(mut self, provider: Arc<dyn RootfsProvider>) -> Self {
        self.rootfs_provider = Some(provider);
        self
    }

    /// Override the init helper path instead of resolving it next to the
    /// current executable.
    pub fn with_init_program(mut self, program: impl Into<PathBuf>) -> Self {
        self.init_program = Some(program.into());
        self
    }

    /// Override the exec helper path. By default the helper is resolved next
    /// to the init helper, then next to the daemon executable.
    pub fn with_exec_program(mut self, program: impl Into<PathBuf>) -> Self {
        self.exec_program = Some(program.into());
        self
    }

    /// Capture stdout/stderr instead of inheriting the daemon's streams.
    /// Attach/log consumers must enable this before `create`.
    pub fn with_capture_stdio(mut self, capture: bool) -> Self {
        self.capture_stdio = capture;
        self
    }

    pub fn state_root(&self) -> &Path {
        &self.state_root
    }

    #[cfg_attr(not(target_os = "linux"), allow(dead_code))]
    pub(crate) fn resolve_init_program(&self) -> Result<PathBuf> {
        if let Some(program) = &self.init_program {
            if program.is_file() {
                return Ok(program.clone());
            }
            return Err(Error::unavailable(format!(
                "runtime init helper `{}` does not exist",
                program.display()
            )));
        }
        if let Ok(value) = std::env::var(RUNTIME_INIT_ENV) {
            let program = PathBuf::from(value);
            if program.is_file() {
                return Ok(program);
            }
            return Err(Error::unavailable(format!(
                "{RUNTIME_INIT_ENV} points at `{}`, which is not a file",
                program.display()
            )));
        }

        let name = if cfg!(windows) {
            "docker-runtime-init.exe"
        } else {
            "docker-runtime-init"
        };
        let current = std::env::current_exe().map_err(|err| {
            Error::internal(format!(
                "locating current executable for runtime init: {err}"
            ))
        })?;
        let mut candidates = Vec::new();
        if let Some(parent) = current.parent() {
            candidates.push(parent.join(name));
            if let Some(grandparent) = parent.parent() {
                candidates.push(grandparent.join(name));
            }
        }
        for candidate in candidates {
            if candidate.is_file() {
                return Ok(candidate);
            }
        }
        Err(Error::unavailable(format!(
            "runtime init helper `{name}` was not found next to `{}`; set {RUNTIME_INIT_ENV}",
            current.display()
        )))
    }

    #[cfg_attr(not(target_os = "linux"), allow(dead_code))]
    pub(crate) fn resolve_exec_program(&self) -> Result<PathBuf> {
        if let Some(program) = &self.exec_program {
            if program.is_file() {
                return Ok(program.clone());
            }
            return Err(Error::unavailable(format!(
                "runtime exec helper `{}` does not exist",
                program.display()
            )));
        }
        if let Ok(value) = std::env::var(RUNTIME_EXEC_ENV) {
            let program = PathBuf::from(value);
            if program.is_file() {
                return Ok(program);
            }
            return Err(Error::unavailable(format!(
                "{RUNTIME_EXEC_ENV} points at `{}`, which is not a file",
                program.display()
            )));
        }

        let name = if cfg!(windows) {
            "docker-runtime-exec.exe"
        } else {
            "docker-runtime-exec"
        };
        let init = self.resolve_init_program()?;
        let sibling = init.with_file_name(name);
        if sibling.is_file() {
            return Ok(sibling);
        }
        let current = std::env::current_exe().map_err(|err| {
            Error::internal(format!(
                "locating current executable for runtime exec: {err}"
            ))
        })?;
        if let Some(parent) = current.parent() {
            for candidate in [parent.join(name), parent.join("..").join(name)] {
                if candidate.is_file() {
                    return Ok(candidate);
                }
            }
        }
        Err(Error::unavailable(format!(
            "runtime exec helper `{name}` was not found next to the init helper `{}`; set {RUNTIME_EXEC_ENV}",
            init.display()
        )))
    }

    /// Verify the host can execute native tasks before the daemon starts
    /// serving requests. This reports unsupported hosts and missing
    /// privileges up front instead of failing on the first `start`.
    pub fn preflight(&self) -> Result<()> {
        #[cfg(target_os = "linux")]
        {
            self.resolve_init_program()?;
            self.resolve_exec_program()?;
            if !nix::unistd::Uid::effective().is_root() {
                return Err(Error::permission_denied(
                    "the native OCI runtime requires root (or a privileged user namespace, which is not implemented yet); \
                     run docker-rsd as root or select --runtime synthetic",
                ));
            }
            Ok(())
        }
        #[cfg(not(target_os = "linux"))]
        {
            Err(Error::unsupported(format!(
                "native OCI execution requires Linux namespaces and mount/chroot privileges; this host is {}",
                std::env::consts::OS
            )))
        }
    }
}

impl Runtime for OciRuntime {
    fn create(&self, config: &RuntimeConfig) -> Result<Box<dyn RuntimeTask>> {
        create_task(self, config)
    }

    fn cleanup(&self, id: &docker_common::ContainerId) -> Result<()> {
        if let Some(provider) = &self.rootfs_provider {
            provider.cleanup(id)?;
        }
        let bundle = self.state_root.join(id.as_str());
        match std::fs::remove_dir_all(&bundle) {
            Ok(()) => Ok(()),
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(()),
            Err(error) => Err(Error::internal(format!(
                "removing OCI bundle `{}`: {error}",
                bundle.display()
            ))),
        }
    }
}

/// Reject specifications containing features this backend cannot enforce.
///
/// The check is intentionally performed before a bundle is materialized, so
/// an unsupported configuration never leaves half-created state behind.
pub fn ensure_supported(spec: &Spec) -> Result<()> {
    spec.validate()?;

    let process = spec
        .process
        .as_ref()
        .ok_or_else(|| Error::invalid_argument("OCI config has no `process` section"))?;
    check_process(process)?;

    if !spec.domainname.is_empty() {
        return Err(Error::unsupported(
            "OCI domainname is not implemented by the native runtime",
        ));
    }
    if let Some(hooks) = &spec.hooks {
        if !hooks.is_empty() {
            return Err(Error::unsupported(
                "OCI lifecycle hooks are not implemented by the native runtime",
            ));
        }
    }
    if !spec.mounts.is_empty() {
        return Err(Error::unsupported(
            "additional OCI mounts are not implemented by the native runtime",
        ));
    }

    if let Some(network_mode) = spec.annotations.get("io.docker-rs.network-mode") {
        let has_network_namespace = spec
            .linux
            .as_ref()
            .map(|linux| {
                linux
                    .namespaces
                    .iter()
                    .any(|namespace| namespace.typ == LinuxNamespaceType::Network)
            })
            .unwrap_or(false);
        match network_mode.as_str() {
            "host" => {
                if has_network_namespace {
                    return Err(Error::unsupported(
                        "config declares host networking but also creates a network namespace; the native runtime cannot honor both",
                    ));
                }
            }
            "none" => {
                if !has_network_namespace {
                    return Err(Error::unsupported(
                        "config declares `none` networking but has no network namespace; refusing to run with host network access",
                    ));
                }
            }
            "bridge" => {
                return Err(Error::unsupported(
                    "bridge networking requires a bridge driver and veth setup, which the native runtime does not implement yet; use network mode `none` or `host`",
                ));
            }
            other => {
                return Err(Error::unsupported(format!(
                    "network mode `{other}` is not implemented by the native runtime"
                )));
            }
        }
    }

    if let Some(linux) = &spec.linux {
        if !linux.uid_mappings.is_empty() || !linux.gid_mappings.is_empty() {
            return Err(Error::unsupported(
                "user namespaces / UID-GID mappings are not implemented by the native runtime",
            ));
        }
        if !linux.sysctl.is_empty() {
            return Err(Error::unsupported(
                "Linux sysctl configuration is not implemented by the native runtime",
            ));
        }
        if let Some(resources) = &linux.resources {
            if !resources.is_empty() {
                return Err(Error::unsupported(
                    "cgroup resource limits are not implemented by the native runtime",
                ));
            }
        }
        if !linux.cgroups_path.is_empty() {
            return Err(Error::unsupported(
                "explicit cgroupsPath is not implemented by the native runtime",
            ));
        }
        if !linux.devices.is_empty() || !linux.net_devices.is_empty() {
            return Err(Error::unsupported(
                "Linux device configuration is not implemented by the native runtime",
            ));
        }
        if linux.seccomp.is_some() {
            return Err(Error::unsupported(
                "seccomp profiles are not implemented by the native runtime",
            ));
        }
        if !linux.rootfs_propagation.is_empty() && linux.rootfs_propagation != "private" {
            return Err(Error::unsupported(format!(
                "rootfsPropagation `{}` is not implemented; only private propagation is supported",
                linux.rootfs_propagation
            )));
        }
        if !linux.masked_paths.is_empty() || !linux.readonly_paths.is_empty() {
            return Err(Error::unsupported(
                "masked/read-only OCI paths are not implemented by the native runtime",
            ));
        }
        if !linux.mount_label.is_empty() {
            return Err(Error::unsupported(
                "SELinux mount labels are not implemented by the native runtime",
            ));
        }
        if linux.intel_rdt.is_some()
            || linux.memory_policy.is_some()
            || linux.personality.is_some()
            || !linux.time_offsets.is_empty()
        {
            return Err(Error::unsupported(
                "Intel RDT, NUMA memory policy, personality, and time namespaces are not implemented by the native runtime",
            ));
        }

        for namespace in &linux.namespaces {
            if !namespace.path.is_empty() {
                return Err(Error::unsupported(format!(
                    "joining an existing {} namespace is not implemented",
                    namespace.typ
                )));
            }
            if matches!(
                namespace.typ,
                LinuxNamespaceType::User | LinuxNamespaceType::Cgroup | LinuxNamespaceType::Time
            ) {
                return Err(Error::unsupported(format!(
                    "{} namespaces are not implemented by the native runtime",
                    namespace.typ
                )));
            }
        }
    }

    let has_namespace = |wanted: LinuxNamespaceType| {
        spec.linux
            .as_ref()
            .map(|linux| {
                linux
                    .namespaces
                    .iter()
                    .any(|namespace| namespace.typ == wanted)
            })
            .unwrap_or(false)
    };
    if !has_namespace(LinuxNamespaceType::Pid) {
        return Err(Error::unsupported(
            "native runtime requires a PID namespace; refusing to mount /proc with host PID visibility",
        ));
    }
    if !has_namespace(LinuxNamespaceType::Mount) {
        return Err(Error::unsupported(
            "native runtime requires a mount namespace for rootfs and proc isolation",
        ));
    }
    if !has_namespace(LinuxNamespaceType::Uts) {
        return Err(Error::unsupported(
            "native runtime requires a UTS namespace to set the container hostname safely",
        ));
    }
    if !has_namespace(LinuxNamespaceType::Ipc) {
        return Err(Error::unsupported(
            "native runtime requires an IPC namespace; host IPC sharing is not implemented",
        ));
    }

    Ok(())
}

fn check_process(process: &Process) -> Result<()> {
    if process.terminal {
        return Err(Error::unsupported(
            "TTY allocation is not implemented; run with `tty: false`",
        ));
    }
    if process.console_size.is_some() {
        return Err(Error::unsupported(
            "consoleSize requires TTY allocation, which is not implemented",
        ));
    }
    if !process.command_line.is_empty() {
        return Err(Error::unsupported(
            "Windows commandLine processes are not implemented",
        ));
    }
    if let Some(capabilities) = &process.capabilities {
        if !capabilities.bounding.is_empty()
            || !capabilities.effective.is_empty()
            || !capabilities.inheritable.is_empty()
            || !capabilities.permitted.is_empty()
            || !capabilities.ambient.is_empty()
        {
            return Err(Error::unsupported(
                "Linux capability sets are not implemented by the native runtime",
            ));
        }
    }
    if !process.rlimits.is_empty() {
        return Err(Error::unsupported(
            "POSIX rlimits are not implemented by the native runtime",
        ));
    }
    if !process.apparmor_profile.is_empty() {
        return Err(Error::unsupported(
            "AppArmor profiles are not implemented by the native runtime",
        ));
    }
    if process.oom_score_adj.is_some() {
        return Err(Error::unsupported(
            "oomScoreAdj is not implemented by the native runtime",
        ));
    }
    if process.scheduler.is_some() {
        return Err(Error::unsupported(
            "scheduler attributes are not implemented by the native runtime",
        ));
    }
    if !process.selinux_label.is_empty() {
        return Err(Error::unsupported(
            "SELinux labels are not implemented by the native runtime",
        ));
    }
    if process.io_priority.is_some() {
        return Err(Error::unsupported(
            "I/O priority is not implemented by the native runtime",
        ));
    }
    if process.exec_cpu_affinity.is_some() {
        return Err(Error::unsupported(
            "exec CPU affinity is not implemented by the native runtime",
        ));
    }
    if !process.user.username.is_empty() {
        return Err(Error::unsupported(
            "Windows username processes are not implemented",
        ));
    }
    if process.user.umask.is_some() {
        return Err(Error::unsupported(
            "process umask is not implemented by the native runtime",
        ));
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::spec::{
        Linux, LinuxNamespace, LinuxNamespaceType, LinuxPids, LinuxResources, Root, User,
    };

    fn minimal_spec() -> Spec {
        Spec {
            process: Some(Process {
                user: User::default(),
                args: vec!["/bin/true".to_string()],
                cwd: "/".to_string(),
                ..empty_process()
            }),
            root: Some(Root {
                path: std::env::temp_dir().to_string_lossy().into_owned(),
                readonly: false,
            }),
            linux: Some(Linux {
                namespaces: vec![
                    LinuxNamespace {
                        typ: LinuxNamespaceType::Mount,
                        path: String::new(),
                    },
                    LinuxNamespace {
                        typ: LinuxNamespaceType::Pid,
                        path: String::new(),
                    },
                    LinuxNamespace {
                        typ: LinuxNamespaceType::Ipc,
                        path: String::new(),
                    },
                    LinuxNamespace {
                        typ: LinuxNamespaceType::Uts,
                        path: String::new(),
                    },
                ],
                ..Linux::default()
            }),
            ..Spec::default()
        }
    }

    fn empty_process() -> Process {
        Process {
            terminal: false,
            console_size: None,
            user: User::default(),
            args: Vec::new(),
            command_line: String::new(),
            env: Vec::new(),
            cwd: "/".to_string(),
            capabilities: None,
            rlimits: Vec::new(),
            no_new_privileges: false,
            apparmor_profile: String::new(),
            oom_score_adj: None,
            scheduler: None,
            selinux_label: String::new(),
            io_priority: None,
            exec_cpu_affinity: None,
        }
    }

    #[test]
    fn minimal_spec_is_supported_policy_wise() {
        ensure_supported(&minimal_spec()).unwrap();
    }

    #[test]
    fn tty_is_rejected_explicitly() {
        let mut spec = minimal_spec();
        spec.process.as_mut().unwrap().terminal = true;
        let err = ensure_supported(&spec).unwrap_err();
        assert_eq!(err.kind(), docker_errors::ErrorKind::Unsupported);
        assert!(err.to_string().contains("TTY"));
    }

    #[test]
    fn bridge_network_is_rejected_explicitly() {
        let mut spec = minimal_spec();
        spec.annotations.insert(
            "io.docker-rs.network-mode".to_string(),
            "bridge".to_string(),
        );
        let err = ensure_supported(&spec).unwrap_err();
        assert_eq!(err.kind(), docker_errors::ErrorKind::Unsupported);
        assert!(err.to_string().contains("bridge"));
    }

    #[test]
    fn missing_pid_namespace_is_rejected() {
        let mut spec = minimal_spec();
        spec.linux = Some(Linux::default());
        let err = ensure_supported(&spec).unwrap_err();
        assert_eq!(err.kind(), docker_errors::ErrorKind::Unsupported);
        assert!(err.to_string().contains("PID namespace"));
    }

    #[test]
    fn cgroup_resources_are_rejected_until_implemented() {
        let mut spec = minimal_spec();
        spec.linux.as_mut().unwrap().resources = Some(LinuxResources {
            pids: Some(LinuxPids { limit: Some(64) }),
            ..LinuxResources::default()
        });
        let err = ensure_supported(&spec).unwrap_err();
        assert_eq!(err.kind(), docker_errors::ErrorKind::Unsupported);
        assert!(err.to_string().contains("cgroup"));
    }

    #[test]
    fn none_network_requires_a_network_namespace() {
        let mut spec = minimal_spec();
        spec.annotations
            .insert("io.docker-rs.network-mode".to_string(), "none".to_string());
        let err = ensure_supported(&spec).unwrap_err();
        assert_eq!(err.kind(), docker_errors::ErrorKind::Unsupported);
        assert!(err.to_string().contains("network namespace"));
    }

    #[test]
    fn host_network_rejects_a_network_namespace() {
        let mut spec = minimal_spec();
        spec.annotations
            .insert("io.docker-rs.network-mode".to_string(), "host".to_string());
        spec.linux
            .as_mut()
            .unwrap()
            .namespaces
            .push(LinuxNamespace {
                typ: LinuxNamespaceType::Network,
                path: String::new(),
            });
        let err = ensure_supported(&spec).unwrap_err();
        assert_eq!(err.kind(), docker_errors::ErrorKind::Unsupported);
        assert!(err.to_string().contains("host networking"));
    }
}
