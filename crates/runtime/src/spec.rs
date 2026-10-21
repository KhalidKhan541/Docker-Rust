//! Open Container Initiative runtime-spec 1.3.0 configuration and state.
//!
//! The structures mirror the JSON field names and omit-empty behavior of
//! `runtime-spec/specs-go/config.go` and `state.go` at the pinned revision
//! (`92249139ee…`). They intentionally cover the Linux subset this runtime
//! emits; platform sections that docker-rs does not implement are rejected
//! at deserialization time rather than silently ignored.
//!
//! Unknown fields are denied on every struct. That is stricter than the Go
//! `encoding/json` default and is deliberate: a runtime must never accept a
//! security-relevant field it does not understand and then execute anyway.

use std::collections::{BTreeMap, BTreeSet};
use std::fmt;
use std::path::Path;

use docker_errors::{Error, Result};
use serde::{Deserialize, Serialize};

/// Version string emitted into every bundle (`runtime-spec` 1.3.0).
pub const OCI_VERSION: &str = "1.3.0";

/// Container configuration (`config.json`).
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Spec {
    #[serde(rename = "ociVersion")]
    pub version: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub process: Option<Process>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub root: Option<Root>,
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub hostname: String,
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub domainname: String,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub mounts: Vec<Mount>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub hooks: Option<Hooks>,
    #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
    pub annotations: BTreeMap<String, String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub linux: Option<Linux>,
}

impl Default for Spec {
    fn default() -> Self {
        Self {
            version: OCI_VERSION.to_string(),
            process: None,
            root: None,
            hostname: String::new(),
            domainname: String::new(),
            mounts: Vec::new(),
            hooks: None,
            annotations: BTreeMap::new(),
            linux: None,
        }
    }
}

impl Spec {
    /// Structural and semantic checks required before a bundle is written or
    /// loaded. Schema conformance is checked separately against the pinned
    /// JSON schemas in the test suite.
    pub fn validate(&self) -> Result<()> {
        if self.version != OCI_VERSION {
            return Err(Error::invalid_argument(format!(
                "unsupported OCI version `{}`: this runtime implements {OCI_VERSION}",
                self.version
            )));
        }

        let root = self
            .root
            .as_ref()
            .ok_or_else(|| Error::invalid_argument("OCI config is missing `root`"))?;
        if root.path.trim().is_empty() {
            return Err(Error::invalid_argument("`root.path` must not be empty"));
        }
        if !Path::new(&root.path).is_absolute() {
            return Err(Error::invalid_argument(format!(
                "`root.path` must be an absolute host path, got `{}`",
                root.path
            )));
        }

        if let Some(process) = &self.process {
            process.validate()?;
        }

        let mut destinations = BTreeSet::new();
        for mount in &self.mounts {
            mount.validate()?;
            if !destinations.insert(mount.destination.as_str()) {
                return Err(Error::invalid_argument(format!(
                    "duplicate mount destination `{}`",
                    mount.destination
                )));
            }
        }

        if let Some(hooks) = &self.hooks {
            hooks.validate()?;
        }

        if let Some(linux) = &self.linux {
            linux.validate()?;
        }

        Ok(())
    }

    /// Parse a bundle's `config.json` body, preserving strict unknown-field
    /// rejection.
    pub fn from_json(bytes: &[u8]) -> Result<Self> {
        let spec: Self = serde_json::from_slice(bytes)
            .map_err(|err| Error::invalid_argument(format!("invalid OCI config.json: {err}")))?;
        spec.validate()?;
        Ok(spec)
    }

    pub fn to_json_pretty(&self) -> Result<Vec<u8>> {
        self.validate()?;
        let mut bytes = serde_json::to_vec_pretty(self)
            .map_err(|err| Error::internal(format!("serializing OCI config: {err}")))?;
        bytes.push(b'\n');
        Ok(bytes)
    }
}

/// Process configuration.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Process {
    #[serde(default, skip_serializing_if = "is_false")]
    pub terminal: bool,
    #[serde(
        rename = "consoleSize",
        default,
        skip_serializing_if = "Option::is_none"
    )]
    pub console_size: Option<ConsoleSize>,
    pub user: User,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub args: Vec<String>,
    #[serde(
        rename = "commandLine",
        default,
        skip_serializing_if = "String::is_empty"
    )]
    pub command_line: String,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub env: Vec<String>,
    pub cwd: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub capabilities: Option<LinuxCapabilities>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub rlimits: Vec<PosixRlimit>,
    #[serde(rename = "noNewPrivileges", default, skip_serializing_if = "is_false")]
    pub no_new_privileges: bool,
    #[serde(
        rename = "apparmorProfile",
        default,
        skip_serializing_if = "String::is_empty"
    )]
    pub apparmor_profile: String,
    #[serde(
        rename = "oomScoreAdj",
        default,
        skip_serializing_if = "Option::is_none"
    )]
    pub oom_score_adj: Option<i32>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub scheduler: Option<Scheduler>,
    #[serde(
        rename = "selinuxLabel",
        default,
        skip_serializing_if = "String::is_empty"
    )]
    pub selinux_label: String,
    #[serde(
        rename = "ioPriority",
        default,
        skip_serializing_if = "Option::is_none"
    )]
    pub io_priority: Option<LinuxIoPriority>,
    #[serde(
        rename = "execCPUAffinity",
        default,
        skip_serializing_if = "Option::is_none"
    )]
    pub exec_cpu_affinity: Option<CpuAffinity>,
}

impl Process {
    fn validate(&self) -> Result<()> {
        if self.command_line.is_empty() && self.args.is_empty() {
            return Err(Error::invalid_argument(
                "process has no `args` (or Windows `commandLine`) to execute",
            ));
        }
        if self.args.first().is_some_and(String::is_empty) {
            return Err(Error::invalid_argument(
                "process `args[0]` must not be empty",
            ));
        }
        if self.args.iter().any(|arg| arg.contains('\0')) {
            return Err(Error::invalid_argument(
                "process arguments must not contain NUL bytes",
            ));
        }
        if !is_container_absolute(&self.cwd) {
            return Err(Error::invalid_argument(format!(
                "process `cwd` must be an absolute container path, got `{}`",
                self.cwd
            )));
        }
        if self.cwd.contains('\0') {
            return Err(Error::invalid_argument(
                "process `cwd` must not contain NUL bytes",
            ));
        }
        for entry in &self.env {
            let Some((key, _)) = entry.split_once('=') else {
                return Err(Error::invalid_argument(format!(
                    "process environment entry `{entry}` is not KEY=value"
                )));
            };
            if key.is_empty() {
                return Err(Error::invalid_argument(
                    "process environment entry has an empty key",
                ));
            }
            if key.contains('\0') || entry.contains('\0') {
                return Err(Error::invalid_argument(
                    "process environment entries must not contain NUL bytes",
                ));
            }
        }
        if let Some(size) = &self.console_size {
            if size.height == 0 || size.width == 0 {
                return Err(Error::invalid_argument(
                    "process consoleSize dimensions must be non-zero",
                ));
            }
        }
        for limit in &self.rlimits {
            if limit.soft > limit.hard {
                return Err(Error::invalid_argument(format!(
                    "rlimit {} soft limit {} exceeds hard limit {}",
                    limit.typ, limit.soft, limit.hard
                )));
            }
        }
        Ok(())
    }
}

/// Terminal dimensions.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ConsoleSize {
    pub height: u64,
    pub width: u64,
}

/// Process identity.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct User {
    pub uid: u32,
    pub gid: u32,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub umask: Option<u32>,
    #[serde(
        rename = "additionalGids",
        default,
        skip_serializing_if = "Vec::is_empty"
    )]
    pub additional_gids: Vec<u32>,
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub username: String,
}

/// Linux capability sets, as accepted by the OCI runtime.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct LinuxCapabilities {
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub bounding: Vec<String>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub effective: Vec<String>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub inheritable: Vec<String>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub permitted: Vec<String>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub ambient: Vec<String>,
}

/// POSIX resource limit.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct PosixRlimit {
    #[serde(rename = "type")]
    pub typ: String,
    pub hard: u64,
    pub soft: u64,
}

/// Root filesystem.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Root {
    pub path: String,
    #[serde(default, skip_serializing_if = "is_false")]
    pub readonly: bool,
}

/// A mount inside the container.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Mount {
    pub destination: String,
    #[serde(rename = "type", default, skip_serializing_if = "String::is_empty")]
    pub typ: String,
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub source: String,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub options: Vec<String>,
    #[serde(rename = "uidMappings", default, skip_serializing_if = "Vec::is_empty")]
    pub uid_mappings: Vec<LinuxIdMapping>,
    #[serde(rename = "gidMappings", default, skip_serializing_if = "Vec::is_empty")]
    pub gid_mappings: Vec<LinuxIdMapping>,
}

impl Mount {
    fn validate(&self) -> Result<()> {
        if !is_container_absolute(&self.destination) {
            return Err(Error::invalid_argument(format!(
                "mount destination `{}` must be an absolute container path",
                self.destination
            )));
        }
        if self.typ.is_empty() && self.source.is_empty() {
            return Err(Error::invalid_argument(format!(
                "mount `{}` has neither a type nor a source",
                self.destination
            )));
        }
        Ok(())
    }
}

/// Lifecycle hooks. The current native backend rejects non-empty hooks.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Hooks {
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub prestart: Vec<Hook>,
    #[serde(
        rename = "createRuntime",
        default,
        skip_serializing_if = "Vec::is_empty"
    )]
    pub create_runtime: Vec<Hook>,
    #[serde(
        rename = "createContainer",
        default,
        skip_serializing_if = "Vec::is_empty"
    )]
    pub create_container: Vec<Hook>,
    #[serde(
        rename = "startContainer",
        default,
        skip_serializing_if = "Vec::is_empty"
    )]
    pub start_container: Vec<Hook>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub poststart: Vec<Hook>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub poststop: Vec<Hook>,
}

impl Hooks {
    pub fn is_empty(&self) -> bool {
        self.prestart.is_empty()
            && self.create_runtime.is_empty()
            && self.create_container.is_empty()
            && self.start_container.is_empty()
            && self.poststart.is_empty()
            && self.poststop.is_empty()
    }

    fn validate(&self) -> Result<()> {
        let groups = [
            self.prestart.as_slice(),
            self.create_runtime.as_slice(),
            self.create_container.as_slice(),
            self.start_container.as_slice(),
            self.poststart.as_slice(),
            self.poststop.as_slice(),
        ];
        for group in groups {
            for hook in group {
                if hook.path.is_empty() {
                    return Err(Error::invalid_argument("hook path must not be empty"));
                }
                if let Some(timeout) = hook.timeout {
                    if timeout <= 0 {
                        return Err(Error::invalid_argument(
                            "hook timeout must be a positive number of seconds",
                        ));
                    }
                }
            }
        }
        Ok(())
    }
}

/// One hook command.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Hook {
    pub path: String,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub args: Vec<String>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub env: Vec<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub timeout: Option<i64>,
}

/// Linux-specific configuration.
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Linux {
    #[serde(rename = "uidMappings", default, skip_serializing_if = "Vec::is_empty")]
    pub uid_mappings: Vec<LinuxIdMapping>,
    #[serde(rename = "gidMappings", default, skip_serializing_if = "Vec::is_empty")]
    pub gid_mappings: Vec<LinuxIdMapping>,
    #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
    pub sysctl: BTreeMap<String, String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub resources: Option<LinuxResources>,
    #[serde(
        rename = "cgroupsPath",
        default,
        skip_serializing_if = "String::is_empty"
    )]
    pub cgroups_path: String,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub namespaces: Vec<LinuxNamespace>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub devices: Vec<LinuxDevice>,
    #[serde(
        rename = "netDevices",
        default,
        skip_serializing_if = "BTreeMap::is_empty"
    )]
    pub net_devices: BTreeMap<String, LinuxNetDevice>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub seccomp: Option<LinuxSeccomp>,
    #[serde(
        rename = "rootfsPropagation",
        default,
        skip_serializing_if = "String::is_empty"
    )]
    pub rootfs_propagation: String,
    #[serde(rename = "maskedPaths", default, skip_serializing_if = "Vec::is_empty")]
    pub masked_paths: Vec<String>,
    #[serde(
        rename = "readonlyPaths",
        default,
        skip_serializing_if = "Vec::is_empty"
    )]
    pub readonly_paths: Vec<String>,
    #[serde(
        rename = "mountLabel",
        default,
        skip_serializing_if = "String::is_empty"
    )]
    pub mount_label: String,
    #[serde(rename = "intelRdt", default, skip_serializing_if = "Option::is_none")]
    pub intel_rdt: Option<LinuxIntelRdt>,
    #[serde(
        rename = "memoryPolicy",
        default,
        skip_serializing_if = "Option::is_none"
    )]
    pub memory_policy: Option<LinuxMemoryPolicy>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub personality: Option<LinuxPersonality>,
    #[serde(
        rename = "timeOffsets",
        default,
        skip_serializing_if = "BTreeMap::is_empty"
    )]
    pub time_offsets: BTreeMap<String, LinuxTimeOffset>,
}

impl Linux {
    fn validate(&self) -> Result<()> {
        validate_mappings(&self.uid_mappings, "uidMappings")?;
        validate_mappings(&self.gid_mappings, "gidMappings")?;
        let mut seen = BTreeSet::new();
        for namespace in &self.namespaces {
            let name = namespace.typ.as_str();
            if !seen.insert(name) {
                return Err(Error::invalid_argument(format!(
                    "duplicate Linux namespace `{name}`"
                )));
            }
            if !namespace.path.is_empty() && !is_container_absolute(&namespace.path) {
                return Err(Error::invalid_argument(format!(
                    "namespace `{name}` path must be absolute, got `{}`",
                    namespace.path
                )));
            }
        }
        for path in self.masked_paths.iter().chain(self.readonly_paths.iter()) {
            if !is_container_absolute(path) {
                return Err(Error::invalid_argument(format!(
                    "Linux path `{path}` must be an absolute container path"
                )));
            }
        }
        if let Some(resources) = &self.resources {
            resources.validate()?;
        }
        if let Some(seccomp) = &self.seccomp {
            if seccomp.default_action.is_empty() {
                return Err(Error::invalid_argument(
                    "seccomp defaultAction must not be empty",
                ));
            }
        }
        Ok(())
    }
}

/// Linux namespace type.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum LinuxNamespaceType {
    Pid,
    Network,
    Mount,
    Ipc,
    Uts,
    User,
    Cgroup,
    Time,
}

impl LinuxNamespaceType {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Pid => "pid",
            Self::Network => "network",
            Self::Mount => "mount",
            Self::Ipc => "ipc",
            Self::Uts => "uts",
            Self::User => "user",
            Self::Cgroup => "cgroup",
            Self::Time => "time",
        }
    }
}

impl fmt::Display for LinuxNamespaceType {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.as_str())
    }
}

/// One namespace to create or join.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct LinuxNamespace {
    #[serde(rename = "type")]
    pub typ: LinuxNamespaceType,
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub path: String,
}

/// UID/GID mapping for user namespaces.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct LinuxIdMapping {
    #[serde(rename = "containerID")]
    pub container_id: u32,
    #[serde(rename = "hostID")]
    pub host_id: u32,
    pub size: u32,
}

fn validate_mappings(mappings: &[LinuxIdMapping], field: &str) -> Result<()> {
    for mapping in mappings {
        if mapping.size == 0 {
            return Err(Error::invalid_argument(format!(
                "Linux {field} entry has size 0"
            )));
        }
    }
    Ok(())
}

/// Time namespace offset.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct LinuxTimeOffset {
    #[serde(default, skip_serializing_if = "is_zero_i64")]
    pub secs: i64,
    #[serde(default, skip_serializing_if = "is_zero_u32")]
    pub nanosecs: u32,
}

/// cgroup resource constraints.
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct LinuxResources {
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub devices: Vec<LinuxDeviceCgroup>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub memory: Option<LinuxMemory>,
    #[serde(rename = "cpu", default, skip_serializing_if = "Option::is_none")]
    pub cpu: Option<LinuxCpu>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub pids: Option<LinuxPids>,
    #[serde(rename = "blockIO", default, skip_serializing_if = "Option::is_none")]
    pub block_io: Option<LinuxBlockIo>,
    #[serde(
        rename = "hugepageLimits",
        default,
        skip_serializing_if = "Vec::is_empty"
    )]
    pub hugepage_limits: Vec<LinuxHugepageLimit>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub network: Option<LinuxNetwork>,
    #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
    pub rdma: BTreeMap<String, LinuxRdma>,
    #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
    pub unified: BTreeMap<String, String>,
}

impl LinuxResources {
    pub fn is_empty(&self) -> bool {
        self.devices.is_empty()
            && self.memory.is_none()
            && self.cpu.is_none()
            && self.pids.is_none()
            && self.block_io.is_none()
            && self.hugepage_limits.is_empty()
            && self.network.is_none()
            && self.rdma.is_empty()
            && self.unified.is_empty()
    }

    fn validate(&self) -> Result<()> {
        if let Some(pids) = &self.pids {
            if matches!(pids.limit, Some(limit) if limit <= 0) {
                return Err(Error::invalid_argument(
                    "Linux pids limit must be greater than zero",
                ));
            }
        }
        Ok(())
    }
}

/// Device cgroup rule.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct LinuxDeviceCgroup {
    pub allow: bool,
    #[serde(rename = "type", default, skip_serializing_if = "String::is_empty")]
    pub typ: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub major: Option<i64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub minor: Option<i64>,
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub access: String,
}

/// Device node to create in the container.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct LinuxDevice {
    pub path: String,
    #[serde(rename = "type")]
    pub typ: String,
    pub major: i64,
    pub minor: i64,
    #[serde(rename = "fileMode", default, skip_serializing_if = "Option::is_none")]
    pub file_mode: Option<u32>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub uid: Option<u32>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub gid: Option<u32>,
}

/// Network device moved into the container namespace.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct LinuxNetDevice {
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub name: String,
}

/// Memory cgroup settings.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct LinuxMemory {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub limit: Option<i64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub reservation: Option<i64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub swap: Option<i64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub kernel: Option<i64>,
    #[serde(rename = "kernelTCP", default, skip_serializing_if = "Option::is_none")]
    pub kernel_tcp: Option<i64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub swappiness: Option<u64>,
    #[serde(
        rename = "disableOOMKiller",
        default,
        skip_serializing_if = "Option::is_none"
    )]
    pub disable_oom_killer: Option<bool>,
    #[serde(
        rename = "useHierarchy",
        default,
        skip_serializing_if = "Option::is_none"
    )]
    pub use_hierarchy: Option<bool>,
    #[serde(
        rename = "checkBeforeUpdate",
        default,
        skip_serializing_if = "Option::is_none"
    )]
    pub check_before_update: Option<bool>,
}

/// CPU cgroup settings.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct LinuxCpu {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub shares: Option<u64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub quota: Option<i64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub burst: Option<u64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub period: Option<u64>,
    #[serde(
        rename = "realtimeRuntime",
        default,
        skip_serializing_if = "Option::is_none"
    )]
    pub realtime_runtime: Option<i64>,
    #[serde(
        rename = "realtimePeriod",
        default,
        skip_serializing_if = "Option::is_none"
    )]
    pub realtime_period: Option<u64>,
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub cpus: String,
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub mems: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub idle: Option<i64>,
}

/// PIDs cgroup settings.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct LinuxPids {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub limit: Option<i64>,
}

/// Block I/O cgroup settings.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct LinuxBlockIo {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub weight: Option<u16>,
    #[serde(
        rename = "leafWeight",
        default,
        skip_serializing_if = "Option::is_none"
    )]
    pub leaf_weight: Option<u16>,
    #[serde(
        rename = "weightDevice",
        default,
        skip_serializing_if = "Vec::is_empty"
    )]
    pub weight_device: Vec<LinuxWeightDevice>,
    #[serde(
        rename = "throttleReadBpsDevice",
        default,
        skip_serializing_if = "Vec::is_empty"
    )]
    pub throttle_read_bps_device: Vec<LinuxThrottleDevice>,
    #[serde(
        rename = "throttleWriteBpsDevice",
        default,
        skip_serializing_if = "Vec::is_empty"
    )]
    pub throttle_write_bps_device: Vec<LinuxThrottleDevice>,
    #[serde(
        rename = "throttleReadIOPSDevice",
        default,
        skip_serializing_if = "Vec::is_empty"
    )]
    pub throttle_read_iops_device: Vec<LinuxThrottleDevice>,
    #[serde(
        rename = "throttleWriteIOPSDevice",
        default,
        skip_serializing_if = "Vec::is_empty"
    )]
    pub throttle_write_iops_device: Vec<LinuxThrottleDevice>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct LinuxWeightDevice {
    pub major: i64,
    pub minor: i64,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub weight: Option<u16>,
    #[serde(
        rename = "leafWeight",
        default,
        skip_serializing_if = "Option::is_none"
    )]
    pub leaf_weight: Option<u16>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct LinuxThrottleDevice {
    pub major: i64,
    pub minor: i64,
    pub rate: u64,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct LinuxHugepageLimit {
    #[serde(rename = "pageSize")]
    pub page_size: String,
    pub limit: u64,
}

#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct LinuxNetwork {
    #[serde(rename = "classID", default, skip_serializing_if = "Option::is_none")]
    pub class_id: Option<u32>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub priorities: Vec<LinuxInterfacePriority>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct LinuxInterfacePriority {
    pub name: String,
    pub priority: u32,
}

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct LinuxRdma {
    #[serde(
        rename = "hcaHandles",
        default,
        skip_serializing_if = "Option::is_none"
    )]
    pub hca_handles: Option<u32>,
    #[serde(
        rename = "hcaObjects",
        default,
        skip_serializing_if = "Option::is_none"
    )]
    pub hca_objects: Option<u32>,
}

/// Seccomp policy.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct LinuxSeccomp {
    #[serde(rename = "defaultAction")]
    pub default_action: String,
    #[serde(
        rename = "defaultErrnoRet",
        default,
        skip_serializing_if = "Option::is_none"
    )]
    pub default_errno_ret: Option<u32>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub architectures: Vec<String>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub flags: Vec<String>,
    #[serde(
        rename = "listenerPath",
        default,
        skip_serializing_if = "String::is_empty"
    )]
    pub listener_path: String,
    #[serde(
        rename = "listenerMetadata",
        default,
        skip_serializing_if = "String::is_empty"
    )]
    pub listener_metadata: String,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub syscalls: Vec<LinuxSyscall>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct LinuxSyscall {
    pub names: Vec<String>,
    pub action: String,
    #[serde(rename = "errnoRet", default, skip_serializing_if = "Option::is_none")]
    pub errno_ret: Option<u32>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub args: Vec<LinuxSeccompArg>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct LinuxSeccompArg {
    pub index: u32,
    pub value: u64,
    #[serde(rename = "valueTwo", default, skip_serializing_if = "is_zero_u64")]
    pub value_two: u64,
    pub op: String,
}

/// Scheduler attributes.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Scheduler {
    pub policy: String,
    #[serde(default, skip_serializing_if = "is_zero_i32")]
    pub nice: i32,
    #[serde(default, skip_serializing_if = "is_zero_i32")]
    pub priority: i32,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub flags: Vec<String>,
    #[serde(default, skip_serializing_if = "is_zero_u64")]
    pub runtime: u64,
    #[serde(default, skip_serializing_if = "is_zero_u64")]
    pub deadline: u64,
    #[serde(default, skip_serializing_if = "is_zero_u64")]
    pub period: u64,
}

/// I/O priority.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct LinuxIoPriority {
    pub class: String,
    pub priority: i32,
}

/// CPU affinity for exec.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct CpuAffinity {
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub initial: String,
    #[serde(rename = "final", default, skip_serializing_if = "String::is_empty")]
    pub final_: String,
}

/// Intel RDT configuration.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields, rename_all = "camelCase")]
pub struct LinuxIntelRdt {
    #[serde(rename = "closID", default, skip_serializing_if = "String::is_empty")]
    pub clos_id: String,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub schemata: Vec<String>,
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub l3_cache_schema: String,
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub mem_bw_schema: String,
    #[serde(default, skip_serializing_if = "is_false")]
    pub enable_monitoring: bool,
}

/// NUMA memory policy.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct LinuxMemoryPolicy {
    pub mode: String,
    pub nodes: String,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub flags: Vec<String>,
}

/// Personality domain.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct LinuxPersonality {
    pub domain: String,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub flags: Vec<String>,
}

/// Runtime state (`state.json` shape).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct State {
    #[serde(rename = "ociVersion")]
    pub version: String,
    pub id: String,
    pub status: ContainerState,
    #[serde(default, skip_serializing_if = "is_zero_i32")]
    pub pid: i32,
    pub bundle: String,
    #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
    pub annotations: BTreeMap<String, String>,
}

impl State {
    pub fn validate(&self) -> Result<()> {
        if self.version != OCI_VERSION {
            return Err(Error::invalid_argument(format!(
                "unsupported OCI state version `{}`",
                self.version
            )));
        }
        if self.id.is_empty() {
            return Err(Error::invalid_argument("OCI state id must not be empty"));
        }
        if self.bundle.is_empty() {
            return Err(Error::invalid_argument(
                "OCI state bundle must not be empty",
            ));
        }
        if self.pid < 0 {
            return Err(Error::invalid_argument(
                "OCI state pid must be non-negative",
            ));
        }
        Ok(())
    }
}

/// OCI container state value.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum ContainerState {
    Creating,
    Created,
    Running,
    Stopped,
}

fn is_container_absolute(path: &str) -> bool {
    path.starts_with('/')
}

fn is_false(value: &bool) -> bool {
    !*value
}

fn is_zero_i32(value: &i32) -> bool {
    *value == 0
}

fn is_zero_i64(value: &i64) -> bool {
    *value == 0
}

fn is_zero_u32(value: &u32) -> bool {
    *value == 0
}

fn is_zero_u64(value: &u64) -> bool {
    *value == 0
}

#[cfg(test)]
mod tests {
    use super::*;

    fn process() -> Process {
        Process {
            terminal: false,
            console_size: None,
            user: User::default(),
            args: vec!["/bin/true".to_string()],
            command_line: String::new(),
            env: vec!["PATH=/bin".to_string()],
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

    fn spec() -> Spec {
        Spec {
            process: Some(process()),
            root: Some(Root {
                path: std::env::temp_dir().to_string_lossy().into_owned(),
                readonly: false,
            }),
            ..Spec::default()
        }
    }

    #[test]
    fn minimal_spec_round_trips() {
        let spec = spec();
        let bytes = spec.to_json_pretty().unwrap();
        let parsed = Spec::from_json(&bytes).unwrap();
        assert_eq!(parsed, spec);
    }

    #[test]
    fn relative_cwd_is_rejected() {
        let mut spec = spec();
        spec.process.as_mut().unwrap().cwd = "relative".into();
        let err = spec.validate().unwrap_err();
        assert_eq!(err.kind(), docker_errors::ErrorKind::InvalidArgument);
    }

    #[test]
    fn empty_args_are_rejected() {
        let mut spec = spec();
        spec.process.as_mut().unwrap().args.clear();
        assert!(spec.validate().is_err());
    }

    #[test]
    fn malformed_env_is_rejected() {
        let mut spec = spec();
        spec.process.as_mut().unwrap().env = vec!["NO_EQUALS".into()];
        let err = spec.validate().unwrap_err();
        assert!(err.to_string().contains("KEY=value"));
    }

    #[test]
    fn duplicate_namespaces_are_rejected() {
        let mut spec = spec();
        spec.linux = Some(Linux {
            namespaces: vec![
                LinuxNamespace {
                    typ: LinuxNamespaceType::Pid,
                    path: String::new(),
                },
                LinuxNamespace {
                    typ: LinuxNamespaceType::Pid,
                    path: String::new(),
                },
            ],
            ..Linux::default()
        });
        let err = spec.validate().unwrap_err();
        assert!(err.to_string().contains("duplicate"));
    }

    #[test]
    fn unknown_fields_are_rejected() {
        let body = br#"{"ociVersion":"1.3.0","root":{"path":"/tmp"},"mystery":true}"#;
        let err = Spec::from_json(body).unwrap_err();
        assert!(err.to_string().contains("mystery"));
    }

    #[test]
    fn state_shape_uses_runtime_spec_names() {
        let state = State {
            version: OCI_VERSION.to_string(),
            id: "abc".into(),
            status: ContainerState::Running,
            pid: 123,
            bundle: "/tmp/bundle".into(),
            annotations: BTreeMap::new(),
        };
        let value = serde_json::to_value(&state).unwrap();
        assert_eq!(value["ociVersion"], "1.3.0");
        assert_eq!(value["status"], "running");
        assert_eq!(value["pid"], 123);
    }
}
