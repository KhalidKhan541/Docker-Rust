//! Engine API wire types and conversion to/from the daemon model.
//!
//! The daemon's internal model deliberately uses Rust casing and a smaller
//! field set. The Engine API uses Moby's exported JSON names (`Image`,
//! `HostConfig`, `Id`, ...). Keeping the wire DTOs here means the daemon can
//! evolve without silently changing the compatibility surface.

use std::collections::BTreeMap;

use chrono::{DateTime, SecondsFormat, Utc};
use docker_common::ImageRef;
use docker_container_model::config::{
    ContainerConfig, HostConfig, NetworkMode, PortKey, RestartPolicy, Signal,
};
use docker_container_model::state::{LifecycleState, StateSnapshot};
use docker_container_model::store::ContainerFilter;
use docker_daemon::{ContainerView, CreateRequest};
use docker_errors::{Error, Result};
use docker_image::Image;
use serde::{Deserialize, Serialize};
use serde_json::Value;

/// Maximum accepted create body. An Engine create request is metadata-sized;
/// this bound also keeps a hostile client from pinning a worker thread on an
/// unbounded allocation.
pub const MAX_BODY_BYTES: usize = 1024 * 1024;

/// `POST /containers/create` response.
#[derive(Debug, Serialize)]
pub struct CreateResponse {
    #[serde(rename = "Id")]
    pub id: String,
    #[serde(rename = "Warnings")]
    pub warnings: Vec<String>,
}

/// `GET /containers/json` row.
#[derive(Debug, Serialize)]
pub struct ContainerSummary {
    #[serde(rename = "Id")]
    pub id: String,
    #[serde(rename = "Names")]
    pub names: Vec<String>,
    #[serde(rename = "Image")]
    pub image: String,
    #[serde(rename = "ImageID")]
    pub image_id: String,
    #[serde(rename = "Command")]
    pub command: String,
    #[serde(rename = "Created")]
    pub created: i64,
    #[serde(rename = "Ports")]
    pub ports: Vec<Value>,
    #[serde(rename = "Labels")]
    pub labels: BTreeMap<String, String>,
    #[serde(rename = "State")]
    pub state: String,
    #[serde(rename = "Status")]
    pub status: String,
    #[serde(rename = "HostConfig")]
    pub host_config: SummaryHostConfig,
    #[serde(rename = "NetworkSettings")]
    pub network_settings: SummaryNetworkSettings,
    #[serde(rename = "Mounts")]
    pub mounts: Vec<Value>,
}

#[derive(Debug, Serialize)]
pub struct SummaryHostConfig {
    #[serde(rename = "NetworkMode")]
    pub network_mode: String,
}

#[derive(Debug, Serialize)]
pub struct SummaryNetworkSettings {
    #[serde(rename = "Networks")]
    pub networks: BTreeMap<String, Value>,
}

/// `GET /containers/{id}/json` response.
#[derive(Debug, Serialize)]
pub struct InspectResponse {
    #[serde(rename = "Id")]
    pub id: String,
    #[serde(rename = "Created")]
    pub created: String,
    #[serde(rename = "Path")]
    pub path: String,
    #[serde(rename = "Args")]
    pub args: Vec<String>,
    #[serde(rename = "State")]
    pub state: InspectState,
    #[serde(rename = "Image")]
    pub image: String,
    #[serde(rename = "ResolvConfPath")]
    pub resolv_conf_path: String,
    #[serde(rename = "HostnamePath")]
    pub hostname_path: String,
    #[serde(rename = "HostsPath")]
    pub hosts_path: String,
    #[serde(rename = "LogPath")]
    pub log_path: String,
    #[serde(rename = "Name")]
    pub name: String,
    #[serde(rename = "RestartCount")]
    pub restart_count: u32,
    #[serde(rename = "Driver")]
    pub driver: String,
    #[serde(rename = "Platform")]
    pub platform: String,
    #[serde(rename = "MountLabel")]
    pub mount_label: String,
    #[serde(rename = "ProcessLabel")]
    pub process_label: String,
    #[serde(rename = "AppArmorProfile")]
    pub app_armor_profile: String,
    #[serde(rename = "ExecIDs")]
    pub exec_ids: Option<Vec<String>>,
    #[serde(rename = "HostConfig")]
    pub host_config: InspectHostConfig,
    #[serde(rename = "GraphDriver")]
    pub graph_driver: GraphDriver,
    #[serde(rename = "Mounts")]
    pub mounts: Vec<Value>,
    #[serde(rename = "Config")]
    pub config: InspectConfig,
    #[serde(rename = "NetworkSettings")]
    pub network_settings: InspectNetworkSettings,
}

#[derive(Debug, Serialize)]
pub struct InspectState {
    #[serde(rename = "Status")]
    pub status: String,
    #[serde(rename = "Running")]
    pub running: bool,
    #[serde(rename = "Paused")]
    pub paused: bool,
    #[serde(rename = "Restarting")]
    pub restarting: bool,
    #[serde(rename = "OOMKilled")]
    pub oom_killed: bool,
    #[serde(rename = "Dead")]
    pub dead: bool,
    #[serde(rename = "Pid")]
    pub pid: i64,
    #[serde(rename = "ExitCode")]
    pub exit_code: i64,
    #[serde(rename = "Error")]
    pub error: String,
    #[serde(rename = "StartedAt")]
    pub started_at: String,
    #[serde(rename = "FinishedAt")]
    pub finished_at: String,
}

#[derive(Debug, Serialize)]
pub struct InspectConfig {
    #[serde(rename = "Hostname")]
    pub hostname: String,
    #[serde(rename = "Domainname")]
    pub domainname: String,
    #[serde(rename = "User")]
    pub user: String,
    #[serde(rename = "AttachStdin")]
    pub attach_stdin: bool,
    #[serde(rename = "AttachStdout")]
    pub attach_stdout: bool,
    #[serde(rename = "AttachStderr")]
    pub attach_stderr: bool,
    #[serde(rename = "ExposedPorts")]
    pub exposed_ports: Option<BTreeMap<String, Value>>,
    #[serde(rename = "Tty")]
    pub tty: bool,
    #[serde(rename = "OpenStdin")]
    pub open_stdin: bool,
    #[serde(rename = "StdinOnce")]
    pub stdin_once: bool,
    #[serde(rename = "Env")]
    pub env: Vec<String>,
    #[serde(rename = "Cmd")]
    pub cmd: Vec<String>,
    #[serde(rename = "Image")]
    pub image: String,
    #[serde(rename = "Volumes")]
    pub volumes: Option<BTreeMap<String, Value>>,
    #[serde(rename = "WorkingDir")]
    pub working_dir: String,
    #[serde(rename = "Entrypoint")]
    pub entrypoint: Vec<String>,
    #[serde(rename = "OnBuild")]
    pub on_build: Option<Vec<String>>,
    #[serde(rename = "Labels")]
    pub labels: BTreeMap<String, String>,
    #[serde(rename = "StopSignal")]
    pub stop_signal: String,
    #[serde(rename = "StopTimeout")]
    pub stop_timeout: u64,
    #[serde(rename = "Shell")]
    pub shell: Option<Vec<String>>,
}

#[derive(Debug, Serialize)]
pub struct InspectHostConfig {
    #[serde(rename = "Binds")]
    pub binds: Option<Vec<String>>,
    #[serde(rename = "ContainerIDFile")]
    pub container_id_file: String,
    #[serde(rename = "LogConfig")]
    pub log_config: LogConfig,
    #[serde(rename = "NetworkMode")]
    pub network_mode: String,
    #[serde(rename = "PortBindings")]
    pub port_bindings: BTreeMap<String, Value>,
    #[serde(rename = "RestartPolicy")]
    pub restart_policy: InspectRestartPolicy,
    #[serde(rename = "AutoRemove")]
    pub auto_remove: bool,
    #[serde(rename = "VolumeDriver")]
    pub volume_driver: String,
    #[serde(rename = "VolumesFrom")]
    pub volumes_from: Option<Vec<String>>,
    #[serde(rename = "ConsoleSize")]
    pub console_size: [u64; 2],
    #[serde(rename = "Annotations")]
    pub annotations: BTreeMap<String, String>,
    #[serde(rename = "CapAdd")]
    pub cap_add: Option<Vec<String>>,
    #[serde(rename = "CapDrop")]
    pub cap_drop: Option<Vec<String>>,
    #[serde(rename = "CgroupnsMode")]
    pub cgroupns_mode: String,
    #[serde(rename = "Dns")]
    pub dns: Option<Vec<String>>,
    #[serde(rename = "DnsOptions")]
    pub dns_options: Option<Vec<String>>,
    #[serde(rename = "DnsSearch")]
    pub dns_search: Option<Vec<String>>,
    #[serde(rename = "ExtraHosts")]
    pub extra_hosts: Option<Vec<String>>,
    #[serde(rename = "GroupAdd")]
    pub group_add: Option<Vec<String>>,
    #[serde(rename = "IpcMode")]
    pub ipc_mode: String,
    #[serde(rename = "Cgroup")]
    pub cgroup: String,
    #[serde(rename = "Links")]
    pub links: Option<Vec<String>>,
    #[serde(rename = "OomScoreAdj")]
    pub oom_score_adj: i64,
    #[serde(rename = "PidMode")]
    pub pid_mode: String,
    #[serde(rename = "Privileged")]
    pub privileged: bool,
    #[serde(rename = "PublishAllPorts")]
    pub publish_all_ports: bool,
    #[serde(rename = "ReadonlyRootfs")]
    pub readonly_rootfs: bool,
    #[serde(rename = "SecurityOpt")]
    pub security_opt: Option<Vec<String>>,
    #[serde(rename = "UTSMode")]
    pub uts_mode: String,
    #[serde(rename = "UsernsMode")]
    pub userns_mode: String,
    #[serde(rename = "ShmSize")]
    pub shm_size: u64,
    #[serde(rename = "Runtime")]
    pub runtime: String,
    #[serde(rename = "Isolation")]
    pub isolation: String,
    #[serde(rename = "CpuShares")]
    pub cpu_shares: u32,
    #[serde(rename = "Memory")]
    pub memory: u64,
    #[serde(rename = "NanoCpus")]
    pub nano_cpus: i64,
    #[serde(rename = "CgroupParent")]
    pub cgroup_parent: String,
    #[serde(rename = "BlkioWeight")]
    pub blkio_weight: u64,
    #[serde(rename = "BlkioWeightDevice")]
    pub blkio_weight_device: Vec<Value>,
    #[serde(rename = "BlkioDeviceReadBps")]
    pub blkio_device_read_bps: Vec<Value>,
    #[serde(rename = "BlkioDeviceWriteBps")]
    pub blkio_device_write_bps: Vec<Value>,
    #[serde(rename = "BlkioDeviceReadIOps")]
    pub blkio_device_read_iops: Vec<Value>,
    #[serde(rename = "BlkioDeviceWriteIOps")]
    pub blkio_device_write_iops: Vec<Value>,
    #[serde(rename = "CpuPeriod")]
    pub cpu_period: i64,
    #[serde(rename = "CpuQuota")]
    pub cpu_quota: i64,
    #[serde(rename = "CpuRealtimePeriod")]
    pub cpu_realtime_period: i64,
    #[serde(rename = "CpuRealtimeRuntime")]
    pub cpu_realtime_runtime: i64,
    #[serde(rename = "CpusetCpus")]
    pub cpuset_cpus: String,
    #[serde(rename = "CpusetMems")]
    pub cpuset_mems: String,
    #[serde(rename = "Devices")]
    pub devices: Vec<Value>,
    #[serde(rename = "DeviceCgroupRules")]
    pub device_cgroup_rules: Option<Vec<String>>,
    #[serde(rename = "DeviceRequests")]
    pub device_requests: Vec<Value>,
    #[serde(rename = "MemoryReservation")]
    pub memory_reservation: i64,
    #[serde(rename = "MemorySwap")]
    pub memory_swap: i64,
    #[serde(rename = "MemorySwappiness")]
    pub memory_swappiness: Option<i64>,
    #[serde(rename = "OomKillDisable")]
    pub oom_kill_disable: Option<bool>,
    #[serde(rename = "PidsLimit")]
    pub pids_limit: Option<i64>,
    #[serde(rename = "Ulimits")]
    pub ulimits: Vec<Value>,
    #[serde(rename = "CpuCount")]
    pub cpu_count: i64,
    #[serde(rename = "CpuPercent")]
    pub cpu_percent: i64,
    #[serde(rename = "IOMaximumIOps")]
    pub io_maximum_iops: u64,
    #[serde(rename = "IOMaximumBandwidth")]
    pub io_maximum_bandwidth: u64,
}

#[derive(Debug, Serialize)]
pub struct InspectRestartPolicy {
    #[serde(rename = "Name")]
    pub name: String,
    #[serde(rename = "MaximumRetryCount")]
    pub maximum_retry_count: u16,
}

#[derive(Debug, Serialize)]
pub struct LogConfig {
    #[serde(rename = "Type")]
    pub r#type: String,
    #[serde(rename = "Config")]
    pub config: BTreeMap<String, String>,
}

#[derive(Debug, Serialize)]
pub struct GraphDriver {
    #[serde(rename = "Name")]
    pub name: String,
    #[serde(rename = "Data")]
    pub data: BTreeMap<String, String>,
}

#[derive(Debug, Serialize)]
pub struct InspectNetworkSettings {
    #[serde(rename = "Bridge")]
    pub bridge: String,
    #[serde(rename = "SandboxID")]
    pub sandbox_id: String,
    #[serde(rename = "HairpinMode")]
    pub hairpin_mode: bool,
    #[serde(rename = "LinkLocalIPv6Address")]
    pub link_local_ipv6_address: String,
    #[serde(rename = "LinkLocalIPv6PrefixLen")]
    pub link_local_ipv6_prefix_len: u64,
    #[serde(rename = "Ports")]
    pub ports: BTreeMap<String, Value>,
    #[serde(rename = "SandboxKey")]
    pub sandbox_key: String,
    #[serde(rename = "SecondaryIPAddresses")]
    pub secondary_ip_addresses: Option<Vec<Value>>,
    #[serde(rename = "SecondaryIPv6Addresses")]
    pub secondary_ipv6_addresses: Option<Vec<Value>>,
    #[serde(rename = "EndpointID")]
    pub endpoint_id: String,
    #[serde(rename = "Gateway")]
    pub gateway: String,
    #[serde(rename = "GlobalIPv6Address")]
    pub global_ipv6_address: String,
    #[serde(rename = "GlobalIPv6PrefixLen")]
    pub global_ipv6_prefix_len: u64,
    #[serde(rename = "IPAddress")]
    pub ip_address: String,
    #[serde(rename = "IPPrefixLen")]
    pub ip_prefix_len: u64,
    #[serde(rename = "IPv6Gateway")]
    pub ipv6_gateway: String,
    #[serde(rename = "MacAddress")]
    pub mac_address: String,
    #[serde(rename = "Networks")]
    pub networks: BTreeMap<String, Value>,
}

/// `GET /version` response.
#[derive(Debug, Serialize)]
pub struct VersionResponse {
    #[serde(rename = "Platform")]
    pub platform: VersionPlatform,
    #[serde(rename = "Components")]
    pub components: Vec<VersionComponent>,
    #[serde(rename = "Version")]
    pub version: String,
    #[serde(rename = "ApiVersion")]
    pub api_version: String,
    #[serde(rename = "MinAPIVersion")]
    pub min_api_version: String,
    #[serde(rename = "GitCommit")]
    pub git_commit: String,
    #[serde(rename = "GoVersion")]
    pub go_version: String,
    #[serde(rename = "Os")]
    pub os: String,
    #[serde(rename = "Arch")]
    pub arch: String,
    #[serde(rename = "KernelVersion")]
    pub kernel_version: String,
    #[serde(rename = "Experimental")]
    pub experimental: bool,
    #[serde(rename = "BuildTime")]
    pub build_time: String,
}

#[derive(Debug, Serialize)]
pub struct VersionPlatform {
    #[serde(rename = "Name")]
    pub name: String,
}

#[derive(Debug, Serialize)]
pub struct VersionComponent {
    #[serde(rename = "Name")]
    pub name: String,
    #[serde(rename = "Version")]
    pub version: String,
    #[serde(rename = "Details")]
    pub details: BTreeMap<String, String>,
}

/// `GET /info` response. Moby's structure is large; fields omitted here are
/// decoded by clients into their zero values. Fields that would be misleading
/// are explicit (synthetic runtime, no image store).
#[derive(Debug, Serialize)]
pub struct InfoResponse {
    #[serde(rename = "ID")]
    pub id: String,
    #[serde(rename = "Containers")]
    pub containers: usize,
    #[serde(rename = "ContainersRunning")]
    pub containers_running: usize,
    #[serde(rename = "ContainersPaused")]
    pub containers_paused: usize,
    #[serde(rename = "ContainersStopped")]
    pub containers_stopped: usize,
    #[serde(rename = "Images")]
    pub images: usize,
    #[serde(rename = "Driver")]
    pub driver: String,
    #[serde(rename = "DriverStatus")]
    pub driver_status: Vec<[String; 2]>,
    #[serde(rename = "Plugins")]
    pub plugins: InfoPlugins,
    #[serde(rename = "MemoryLimit")]
    pub memory_limit: bool,
    #[serde(rename = "SwapLimit")]
    pub swap_limit: bool,
    #[serde(rename = "KernelMemory")]
    pub kernel_memory: bool,
    #[serde(rename = "CpuCfsPeriod")]
    pub cpu_cfs_period: bool,
    #[serde(rename = "CpuCfsQuota")]
    pub cpu_cfs_quota: bool,
    #[serde(rename = "CPUShares")]
    pub cpu_shares: bool,
    #[serde(rename = "CPUSet")]
    pub cpu_set: bool,
    #[serde(rename = "PidsLimit")]
    pub pids_limit: bool,
    #[serde(rename = "IPv4Forwarding")]
    pub ipv4_forwarding: bool,
    #[serde(rename = "Debug")]
    pub debug: bool,
    #[serde(rename = "NFd")]
    pub nfd: i64,
    #[serde(rename = "OomKillDisable")]
    pub oom_kill_disable: bool,
    #[serde(rename = "NGoroutines")]
    pub n_goroutines: i64,
    #[serde(rename = "SystemTime")]
    pub system_time: String,
    #[serde(rename = "LoggingDriver")]
    pub logging_driver: String,
    #[serde(rename = "CgroupDriver")]
    pub cgroup_driver: String,
    #[serde(rename = "CgroupVersion")]
    pub cgroup_version: String,
    #[serde(rename = "NEventsListener")]
    pub n_events_listener: usize,
    #[serde(rename = "KernelVersion")]
    pub kernel_version: String,
    #[serde(rename = "OperatingSystem")]
    pub operating_system: String,
    #[serde(rename = "OSVersion")]
    pub os_version: String,
    #[serde(rename = "OSType")]
    pub os_type: String,
    #[serde(rename = "Architecture")]
    pub architecture: String,
    #[serde(rename = "IndexServerAddress")]
    pub index_server_address: String,
    #[serde(rename = "RegistryConfig")]
    pub registry_config: Value,
    #[serde(rename = "NCPU")]
    pub ncpu: usize,
    #[serde(rename = "MemTotal")]
    pub mem_total: i64,
    #[serde(rename = "GenericResources")]
    pub generic_resources: Vec<Value>,
    #[serde(rename = "DockerRootDir")]
    pub docker_root_dir: String,
    #[serde(rename = "HttpProxy")]
    pub http_proxy: String,
    #[serde(rename = "HttpsProxy")]
    pub https_proxy: String,
    #[serde(rename = "NoProxy")]
    pub no_proxy: String,
    #[serde(rename = "Name")]
    pub name: String,
    #[serde(rename = "Labels")]
    pub labels: Vec<String>,
    #[serde(rename = "ExperimentalBuild")]
    pub experimental_build: bool,
    #[serde(rename = "ServerVersion")]
    pub server_version: String,
    #[serde(rename = "Runtimes")]
    pub runtimes: BTreeMap<String, RuntimeInfo>,
    #[serde(rename = "DefaultRuntime")]
    pub default_runtime: String,
    #[serde(rename = "Swarm")]
    pub swarm: SwarmInfo,
    #[serde(rename = "LiveRestoreEnabled")]
    pub live_restore_enabled: bool,
    #[serde(rename = "Isolation")]
    pub isolation: String,
    #[serde(rename = "InitBinary")]
    pub init_binary: String,
    #[serde(rename = "ContainerdCommit")]
    pub containerd_commit: CommitInfo,
    #[serde(rename = "RuncCommit")]
    pub runc_commit: CommitInfo,
    #[serde(rename = "InitCommit")]
    pub init_commit: CommitInfo,
    #[serde(rename = "SecurityOptions")]
    pub security_options: Vec<String>,
    #[serde(rename = "Warnings")]
    pub warnings: Vec<String>,
}

#[derive(Debug, Serialize)]
pub struct InfoPlugins {
    #[serde(rename = "Volume")]
    pub volume: Vec<String>,
    #[serde(rename = "Network")]
    pub network: Vec<String>,
    #[serde(rename = "Authorization")]
    pub authorization: Option<Vec<String>>,
    #[serde(rename = "Log")]
    pub log: Vec<String>,
}

#[derive(Debug, Serialize)]
pub struct RuntimeInfo {
    #[serde(rename = "path")]
    pub path: String,
    #[serde(rename = "status")]
    pub status: BTreeMap<String, String>,
}

#[derive(Debug, Serialize)]
pub struct SwarmInfo {
    #[serde(rename = "LocalNodeState")]
    pub local_node_state: String,
    #[serde(rename = "ControlAvailable")]
    pub control_available: bool,
    #[serde(rename = "Error")]
    pub error: String,
}

#[derive(Debug, Serialize)]
pub struct CommitInfo {
    #[serde(rename = "ID")]
    pub id: String,
    #[serde(rename = "Expected")]
    pub expected: String,
}

/// `{"message": ...}` is the Engine API's single-error envelope.
#[derive(Debug, Serialize)]
pub struct ErrorResponse<'a> {
    pub message: &'a str,
}

/// `POST /containers/{id}/wait` response.
#[derive(Debug, Serialize)]
pub struct WaitResponse {
    #[serde(rename = "StatusCode")]
    pub status_code: i64,
    #[serde(rename = "Error")]
    pub error: Option<WaitError>,
}

#[derive(Debug, Serialize)]
pub struct WaitError {
    #[serde(rename = "Message")]
    pub message: String,
}

/// Create request decoded from Moby's top-level Config + nested HostConfig.
#[derive(Debug, Clone, Deserialize, Default)]
#[serde(default)]
pub struct CreateRequestDto {
    #[serde(rename = "Hostname")]
    pub hostname: String,
    #[serde(rename = "Domainname")]
    pub domainname: String,
    #[serde(rename = "User")]
    pub user: String,
    #[serde(rename = "AttachStdin")]
    pub attach_stdin: bool,
    #[serde(rename = "AttachStdout")]
    pub attach_stdout: bool,
    #[serde(rename = "AttachStderr")]
    pub attach_stderr: bool,
    #[serde(rename = "ExposedPorts")]
    pub exposed_ports: Option<BTreeMap<String, Value>>,
    #[serde(rename = "Tty")]
    pub tty: bool,
    #[serde(rename = "OpenStdin")]
    pub open_stdin: bool,
    #[serde(rename = "StdinOnce")]
    pub stdin_once: bool,
    #[serde(rename = "Env")]
    pub env: Option<Vec<String>>,
    #[serde(rename = "Cmd")]
    pub cmd: Option<Vec<String>>,
    #[serde(rename = "Healthcheck")]
    pub healthcheck: Option<Value>,
    #[serde(rename = "ArgsEscaped")]
    pub args_escaped: bool,
    #[serde(rename = "Image")]
    pub image: String,
    #[serde(rename = "Volumes")]
    pub volumes: Option<Value>,
    #[serde(rename = "WorkingDir")]
    pub working_dir: String,
    #[serde(rename = "Entrypoint")]
    pub entrypoint: Option<Vec<String>>,
    #[serde(rename = "OnBuild")]
    pub on_build: Option<Vec<String>>,
    #[serde(rename = "Labels")]
    pub labels: Option<BTreeMap<String, String>>,
    #[serde(rename = "StopSignal")]
    pub stop_signal: Option<String>,
    #[serde(rename = "StopTimeout")]
    pub stop_timeout: Option<i64>,
    #[serde(rename = "Shell")]
    pub shell: Option<Vec<String>>,
    #[serde(rename = "HostConfig")]
    pub host_config: Option<HostConfigDto>,
    #[serde(rename = "NetworkingConfig")]
    pub networking_config: Option<Value>,
    #[serde(flatten)]
    pub extra: BTreeMap<String, Value>,
}

/// Subset of Moby's `container.HostConfig` that the daemon model can carry.
#[derive(Debug, Clone, Deserialize, Default)]
#[serde(default)]
pub struct HostConfigDto {
    #[serde(rename = "Memory")]
    pub memory: i64,
    #[serde(rename = "CpuShares")]
    pub cpu_shares: i64,
    #[serde(rename = "PidsLimit")]
    pub pids_limit: Option<i64>,
    /// Docker CLI sends `-1` when `--memory-swappiness` is unset.
    #[serde(rename = "MemorySwappiness")]
    pub memory_swappiness: Option<i64>,
    /// `[height, width]`; Docker CLI marshals the zero array even when no TTY
    /// size was requested.
    #[serde(rename = "ConsoleSize")]
    pub console_size: Option<[u64; 2]>,
    #[serde(rename = "NetworkMode")]
    pub network_mode: String,
    #[serde(rename = "RestartPolicy")]
    pub restart_policy: Option<RestartPolicyDto>,
    #[serde(rename = "AutoRemove")]
    pub auto_remove: bool,
    #[serde(rename = "Privileged")]
    pub privileged: bool,
    #[serde(rename = "ReadonlyRootfs")]
    pub readonly_rootfs: bool,
    #[serde(rename = "ExtraHosts")]
    pub extra_hosts: Option<Vec<String>>,
    #[serde(rename = "IpcMode")]
    pub ipc_mode: String,
    #[serde(rename = "PidMode")]
    pub pid_mode: String,
    #[serde(rename = "UTSMode")]
    pub uts_mode: String,
    #[serde(rename = "UsernsMode")]
    pub userns_mode: String,
    #[serde(rename = "CgroupnsMode")]
    pub cgroupns_mode: String,
    #[serde(rename = "Isolation")]
    pub isolation: String,
    #[serde(rename = "LogConfig")]
    pub log_config: Option<LogConfigDto>,
    #[serde(flatten)]
    pub extra: BTreeMap<String, Value>,
}

#[derive(Debug, Clone, Deserialize, Default)]
#[serde(default)]
pub struct RestartPolicyDto {
    #[serde(rename = "Name")]
    pub name: String,
    #[serde(rename = "MaximumRetryCount")]
    pub maximum_retry_count: u16,
}

#[derive(Debug, Clone, Deserialize, Default)]
#[serde(default)]
pub struct LogConfigDto {
    #[serde(rename = "Type")]
    pub r#type: String,
    #[serde(rename = "Config")]
    pub config: BTreeMap<String, String>,
}

impl CreateRequestDto {
    /// Validate and convert a decoded create request.
    pub fn into_daemon(self, name: Option<String>) -> Result<CreateRequest> {
        self.into_daemon_with_image(name, None)
    }

    /// Validate and convert a decoded create request, applying the execution
    /// defaults that Moby inherits from the resolved image config. Container
    /// fields supplied by the client win; image values fill only unset fields.
    pub fn into_daemon_with_image(
        self,
        name: Option<String>,
        image: Option<&Image>,
    ) -> Result<CreateRequest> {
        if self.image.trim().is_empty() {
            return Err(Error::invalid_argument("invalid image: empty image name"));
        }
        if !self.domainname.is_empty() {
            return Err(Error::unsupported(
                "container domainname is not supported by the current runtime",
            ));
        }
        if self.args_escaped {
            return Err(Error::unsupported(
                "ArgsEscaped is a Windows-only create option and is not supported",
            ));
        }
        if self.tty {
            return Err(Error::unsupported(
                "TTY allocation is not implemented; refusing to silently ignore Tty=true",
            ));
        }
        if !is_empty_json(self.healthcheck.as_ref()) {
            return Err(Error::unsupported(
                "Healthcheck is not implemented; refusing to silently ignore it",
            ));
        }
        if !is_empty_json(self.volumes.as_ref()) {
            return Err(Error::unsupported(
                "anonymous volume declarations are not implemented; use an explicit mount when supported",
            ));
        }
        if !is_empty_json(self.networking_config.as_ref()) {
            return Err(Error::unsupported(
                "NetworkingConfig is not implemented; use HostConfig.NetworkMode",
            ));
        }
        reject_extra("container config", &self.extra)?;

        let image_ref = ImageRef::parse(&self.image)?;
        let image_config = image.map(|image| &image.config);
        let inherited_stop_signal = image_config
            .map(|config| config.stop_signal.trim())
            .filter(|signal| !signal.is_empty());
        let stop_signal = match self
            .stop_signal
            .as_deref()
            .map(str::trim)
            .filter(|signal| !signal.is_empty())
            .or(inherited_stop_signal)
        {
            None => Signal::Sigterm,
            Some(signal) => signal.parse()?,
        };
        let stop_timeout = match self.stop_timeout {
            None => 10,
            Some(value) if value >= 0 => value as u64,
            Some(value) => {
                return Err(Error::invalid_argument(format!(
                    "StopTimeout must be non-negative, got {value}"
                )))
            }
        };
        let mut exposed_ports = parse_exposed_ports(self.exposed_ports)?;
        if let Some(config) = image_config {
            for key in config.exposed_ports.keys() {
                let port: PortKey = key.parse()?;
                if !exposed_ports.contains(&port) {
                    exposed_ports.push(port);
                }
            }
        }
        exposed_ports.sort_by_key(|port| (port.port, port.proto.to_string()));
        exposed_ports.dedup();
        let labels = merge_labels(
            image_config.map(|config| &config.labels),
            self.labels.unwrap_or_default(),
        );
        let working_dir = non_empty(self.working_dir).or_else(|| {
            image_config
                .map(|config| config.working_dir.trim())
                .filter(|dir| !dir.is_empty())
                .map(str::to_string)
        });
        let user = non_empty(self.user).or_else(|| {
            image_config
                .map(|config| config.user.trim())
                .filter(|user| !user.is_empty())
                .map(str::to_string)
        });
        let config = ContainerConfig {
            image: image_ref,
            entrypoint: non_empty_list(self.entrypoint)
                .or_else(|| image_config.map(|config| config.entrypoint.clone()))
                .unwrap_or_default(),
            cmd: non_empty_list(self.cmd)
                .or_else(|| image_config.map(|config| config.cmd.clone()))
                .unwrap_or_default(),
            env: merge_env(
                image_config.map(|config| config.env.as_slice()),
                self.env.unwrap_or_default(),
            ),
            labels,
            working_dir,
            user,
            hostname: non_empty(self.hostname),
            tty: self.tty,
            open_stdin: self.open_stdin,
            stop_signal,
            stop_timeout_secs: stop_timeout,
            exposed_ports,
        };
        let host_config = self
            .host_config
            .map(HostConfigDto::into_model)
            .transpose()?
            .unwrap_or_default();

        let mut request = CreateRequest::new(config).with_host_config(host_config);
        if let Some(name) = name {
            request = request.with_name(name);
        }
        Ok(request)
    }
}

impl HostConfigDto {
    fn into_model(self) -> Result<HostConfig> {
        if self.memory < 0 {
            return Err(Error::invalid_argument("Memory must be non-negative"));
        }
        if self.memory > 0 {
            return Err(Error::unsupported(
                "memory limits are not implemented (cgroups are not wired)",
            ));
        }
        if self.cpu_shares < 0 {
            return Err(Error::invalid_argument("CpuShares must be non-negative"));
        }
        if self.cpu_shares > 0 {
            return Err(Error::unsupported(
                "CPU shares are not implemented (cgroups are not wired)",
            ));
        }
        let pids_limit = match self.pids_limit {
            None | Some(0) | Some(-1) => None,
            Some(value) if value > 0 => {
                return Err(Error::unsupported(
                    "PidsLimit is not implemented (cgroups are not wired)",
                ))
            }
            Some(value) => {
                return Err(Error::invalid_argument(format!(
                    "PidsLimit must be positive or -1, got {value}"
                )))
            }
        };
        if !self.extra_hosts.as_deref().unwrap_or_default().is_empty() {
            return Err(Error::unsupported(
                "ExtraHosts is not implemented by the native runtime",
            ));
        }
        match self.memory_swappiness {
            None | Some(-1) => {}
            Some(value @ 0..=100) => {
                return Err(Error::unsupported(format!(
                    "MemorySwappiness={value} is not implemented (cgroup controls are not wired)"
                )))
            }
            Some(value) => {
                return Err(Error::invalid_argument(format!(
                    "MemorySwappiness must be -1 or 0..=100, got {value}"
                )))
            }
        }
        if self.console_size.is_some_and(|size| size != [0, 0]) {
            return Err(Error::unsupported(
                "ConsoleSize/TTY resize is not implemented",
            ));
        }
        ensure_mode("IpcMode", &self.ipc_mode, &["", "private"])?;
        ensure_mode("PidMode", &self.pid_mode, &["", "private"])?;
        ensure_mode("UTSMode", &self.uts_mode, &["", "private"])?;
        ensure_mode("UsernsMode", &self.userns_mode, &["", "host"])?;
        ensure_mode("CgroupnsMode", &self.cgroupns_mode, &["", "host"])?;
        ensure_mode("Isolation", &self.isolation, &["", "default"])?;
        if let Some(log) = &self.log_config {
            if !log.config.is_empty() {
                return Err(Error::unsupported(
                    "log driver options are not implemented; LogConfig.Config must be empty",
                ));
            }
            if !matches!(log.r#type.as_str(), "" | "json-file") {
                return Err(Error::unsupported(format!(
                    "log driver {:?} is not implemented",
                    log.r#type
                )));
            }
        }
        reject_extra("HostConfig", &self.extra)?;

        let restart = self.restart_policy.unwrap_or_default();
        let (restart_policy, max_retry_count) = match restart.name.as_str() {
            "" | "no" => (RestartPolicy::No, 0),
            "on-failure" => (RestartPolicy::OnFailure, restart.maximum_retry_count),
            "always" => (RestartPolicy::Always, 0),
            "unless-stopped" => (RestartPolicy::UnlessStopped, 0),
            other => {
                return Err(Error::invalid_argument(format!(
                    "invalid restart policy {other:?}"
                )))
            }
        };
        let network_mode = parse_network_mode(&self.network_mode)?;
        if matches!(
            network_mode,
            NetworkMode::Named(_) | NetworkMode::Container(_)
        ) {
            return Err(Error::unsupported(format!(
                "network mode {network_mode:?} is not implemented; only default/bridge, host, and none are accepted"
            )));
        }

        Ok(HostConfig {
            memory_bytes: 0,
            cpu_shares: 0,
            pids_limit,
            network_mode,
            restart_policy,
            max_retry_count,
            auto_remove: self.auto_remove,
            privileged: self.privileged,
            readonly_rootfs: self.readonly_rootfs,
            extra_hosts: self.extra_hosts.unwrap_or_default(),
        })
    }
}

fn non_empty(value: String) -> Option<String> {
    if value.is_empty() {
        None
    } else {
        Some(value)
    }
}

fn non_empty_list(value: Option<Vec<String>>) -> Option<Vec<String>> {
    value.filter(|items| !items.is_empty())
}

/// `docker run -e` semantics: inherited image entries keep their position,
/// while a container entry with the same key replaces the inherited value.
fn merge_env(inherited: Option<&[String]>, overrides: Vec<String>) -> Vec<String> {
    let mut merged = inherited.map(<[String]>::to_vec).unwrap_or_default();
    for entry in overrides {
        let key = entry.split_once('=').map_or(entry.as_str(), |(key, _)| key);
        match merged.iter().position(|existing| {
            existing
                .split_once('=')
                .map_or(existing.as_str(), |(k, _)| k)
                == key
        }) {
            Some(index) => merged[index] = entry,
            None => merged.push(entry),
        }
    }
    merged
}

fn merge_labels(
    inherited: Option<&BTreeMap<String, String>>,
    overrides: BTreeMap<String, String>,
) -> BTreeMap<String, String> {
    let mut merged = inherited.cloned().unwrap_or_default();
    merged.extend(overrides);
    merged
}

fn ensure_mode(field: &str, value: &str, allowed: &[&str]) -> Result<()> {
    if allowed.contains(&value) {
        Ok(())
    } else {
        Err(Error::unsupported(format!(
            "{field}={value:?} is not supported by the current runtime"
        )))
    }
}

fn parse_network_mode(value: &str) -> Result<NetworkMode> {
    let value = value.trim();
    match value {
        "" | "default" | "bridge" => Ok(NetworkMode::Bridge),
        "host" => Ok(NetworkMode::Host),
        "none" => Ok(NetworkMode::None),
        other => {
            if let Some(id) = other.strip_prefix("container:") {
                if id.is_empty() {
                    return Err(Error::invalid_argument(
                        "network mode container: requires a container reference",
                    ));
                }
                Ok(NetworkMode::Container(id.to_string()))
            } else {
                Ok(NetworkMode::Named(other.to_string()))
            }
        }
    }
}

fn parse_exposed_ports(map: Option<BTreeMap<String, Value>>) -> Result<Vec<PortKey>> {
    let Some(map) = map else {
        return Ok(Vec::new());
    };
    let mut ports = Vec::with_capacity(map.len());
    for key in map.keys() {
        ports.push(key.parse::<PortKey>()?);
    }
    ports.sort_by_key(|port| (port.port, port.proto.to_string()));
    ports.dedup();
    Ok(ports)
}

/// Reject unknown fields with non-empty values. Moby's clients marshal many
/// zero-valued fields; those are harmless defaults. A field that asks for
/// behavior we do not implement must fail loudly.
fn reject_extra(scope: &str, extra: &BTreeMap<String, Value>) -> Result<()> {
    let unsupported: Vec<&str> = extra
        .iter()
        .filter(|(_, value)| !is_empty_json(Some(value)))
        .map(|(key, _)| key.as_str())
        .collect();
    if unsupported.is_empty() {
        Ok(())
    } else {
        Err(Error::unsupported(format!(
            "{scope} field(s) not supported by docker-rs: {}",
            unsupported.join(", ")
        )))
    }
}

/// Whether a JSON value represents an unset/zero default.
fn is_empty_json(value: Option<&Value>) -> bool {
    let Some(value) = value else {
        return true;
    };
    match value {
        Value::Null => true,
        Value::Bool(value) => !*value,
        Value::Number(value) => {
            value.as_i64().map(|n| n == 0).unwrap_or(false)
                || value.as_u64().map(|n| n == 0).unwrap_or(false)
                || value.as_f64().map(|n| n == 0.0).unwrap_or(false)
        }
        Value::String(value) => value.is_empty(),
        Value::Array(values) => values.is_empty(),
        Value::Object(map) => map.values().all(|value| is_empty_json(Some(value))),
    }
}

impl From<&ContainerView> for ContainerSummary {
    fn from(view: &ContainerView) -> Self {
        Self {
            id: view.id.as_str().to_string(),
            names: view
                .name
                .as_deref()
                .map(|name| vec![format!("/{}", name.trim_start_matches('/'))])
                .unwrap_or_default(),
            image: view.config.image.to_string(),
            image_id: String::new(),
            command: view.config.effective_argv().join(" "),
            created: view.created_at,
            ports: Vec::new(),
            labels: view.config.labels.clone(),
            state: view.state.status.as_api_status().to_string(),
            status: human_status(view),
            host_config: SummaryHostConfig {
                network_mode: view.host_config.network_mode.to_string(),
            },
            network_settings: SummaryNetworkSettings {
                networks: BTreeMap::new(),
            },
            mounts: Vec::new(),
        }
    }
}

impl From<&ContainerView> for InspectResponse {
    fn from(view: &ContainerView) -> Self {
        let argv = view.config.effective_argv();
        let (path, args) = match argv.split_first() {
            Some((path, args)) => (path.clone(), args.to_vec()),
            None => (String::new(), Vec::new()),
        };
        let config = &view.config;
        let host = &view.host_config;
        Self {
            id: view.id.as_str().to_string(),
            created: rfc3339(view.created_at),
            path,
            args,
            state: inspect_state(&view.state),
            image: config.image.to_string(),
            resolv_conf_path: String::new(),
            hostname_path: String::new(),
            hosts_path: String::new(),
            log_path: String::new(),
            name: view
                .name
                .as_deref()
                .map(|name| format!("/{}", name.trim_start_matches('/')))
                .unwrap_or_default(),
            restart_count: view.restart_count,
            driver: "docker-rs".to_string(),
            platform: std::env::consts::OS.to_string(),
            mount_label: String::new(),
            process_label: String::new(),
            app_armor_profile: String::new(),
            exec_ids: None,
            host_config: InspectHostConfig::from(host),
            graph_driver: GraphDriver {
                name: "docker-rs".to_string(),
                data: BTreeMap::new(),
            },
            mounts: Vec::new(),
            config: InspectConfig {
                hostname: config.hostname.clone().unwrap_or_default(),
                domainname: String::new(),
                user: config.user.clone().unwrap_or_default(),
                attach_stdin: false,
                attach_stdout: false,
                attach_stderr: false,
                exposed_ports: if config.exposed_ports.is_empty() {
                    None
                } else {
                    Some(
                        config
                            .exposed_ports
                            .iter()
                            .map(|port| (port.to_string(), Value::Object(Default::default())))
                            .collect(),
                    )
                },
                tty: config.tty,
                open_stdin: config.open_stdin,
                stdin_once: false,
                env: config.env.clone(),
                cmd: config.cmd.clone(),
                image: config.image.to_string(),
                volumes: None,
                working_dir: config.working_dir.clone().unwrap_or_default(),
                entrypoint: config.entrypoint.clone(),
                on_build: None,
                labels: config.labels.clone(),
                stop_signal: config.stop_signal.as_str().to_string(),
                stop_timeout: config.stop_timeout_secs,
                shell: None,
            },
            network_settings: InspectNetworkSettings {
                bridge: String::new(),
                sandbox_id: String::new(),
                hairpin_mode: false,
                link_local_ipv6_address: String::new(),
                link_local_ipv6_prefix_len: 0,
                ports: BTreeMap::new(),
                sandbox_key: String::new(),
                secondary_ip_addresses: None,
                secondary_ipv6_addresses: None,
                endpoint_id: String::new(),
                gateway: String::new(),
                global_ipv6_address: String::new(),
                global_ipv6_prefix_len: 0,
                ip_address: String::new(),
                ip_prefix_len: 0,
                ipv6_gateway: String::new(),
                mac_address: String::new(),
                networks: BTreeMap::new(),
            },
        }
    }
}

impl From<&HostConfig> for InspectHostConfig {
    fn from(host: &HostConfig) -> Self {
        let (restart_name, restart_count) = match host.restart_policy {
            RestartPolicy::No => ("no", 0),
            RestartPolicy::OnFailure => ("on-failure", host.max_retry_count),
            RestartPolicy::Always => ("always", 0),
            RestartPolicy::UnlessStopped => ("unless-stopped", 0),
        };
        Self {
            binds: None,
            container_id_file: String::new(),
            log_config: LogConfig {
                r#type: "json-file".to_string(),
                config: BTreeMap::new(),
            },
            network_mode: host.network_mode.to_string(),
            port_bindings: BTreeMap::new(),
            restart_policy: InspectRestartPolicy {
                name: restart_name.to_string(),
                maximum_retry_count: restart_count,
            },
            auto_remove: host.auto_remove,
            volume_driver: String::new(),
            volumes_from: None,
            console_size: [0, 0],
            annotations: BTreeMap::new(),
            cap_add: None,
            cap_drop: None,
            cgroupns_mode: String::new(),
            dns: None,
            dns_options: None,
            dns_search: None,
            extra_hosts: if host.extra_hosts.is_empty() {
                None
            } else {
                Some(host.extra_hosts.clone())
            },
            group_add: None,
            ipc_mode: String::new(),
            cgroup: String::new(),
            links: None,
            oom_score_adj: 0,
            pid_mode: String::new(),
            privileged: host.privileged,
            publish_all_ports: false,
            readonly_rootfs: host.readonly_rootfs,
            security_opt: None,
            uts_mode: String::new(),
            userns_mode: String::new(),
            shm_size: 0,
            runtime: "docker-rs".to_string(),
            isolation: String::new(),
            cpu_shares: host.cpu_shares,
            memory: host.memory_bytes,
            nano_cpus: 0,
            cgroup_parent: String::new(),
            blkio_weight: 0,
            blkio_weight_device: Vec::new(),
            blkio_device_read_bps: Vec::new(),
            blkio_device_write_bps: Vec::new(),
            blkio_device_read_iops: Vec::new(),
            blkio_device_write_iops: Vec::new(),
            cpu_period: 0,
            cpu_quota: 0,
            cpu_realtime_period: 0,
            cpu_realtime_runtime: 0,
            cpuset_cpus: String::new(),
            cpuset_mems: String::new(),
            devices: Vec::new(),
            device_cgroup_rules: None,
            device_requests: Vec::new(),
            memory_reservation: 0,
            memory_swap: 0,
            memory_swappiness: Some(-1),
            oom_kill_disable: None,
            pids_limit: host.pids_limit.map(|value| value as i64),
            ulimits: Vec::new(),
            cpu_count: 0,
            cpu_percent: 0,
            io_maximum_iops: 0,
            io_maximum_bandwidth: 0,
        }
    }
}

fn inspect_state(state: &StateSnapshot) -> InspectState {
    let status = state.status;
    InspectState {
        status: status.as_api_status().to_string(),
        running: status.is_alive(),
        paused: status == LifecycleState::Paused,
        restarting: status == LifecycleState::Restarting,
        oom_killed: false,
        dead: status == LifecycleState::Dead,
        pid: 0,
        exit_code: state.exit_code.unwrap_or(0) as i64,
        error: state.error_signal.clone().unwrap_or_default(),
        started_at: state.started_at.map(rfc3339).unwrap_or_else(zero_time),
        finished_at: state.finished_at.map(rfc3339).unwrap_or_else(zero_time),
    }
}

/// Moby's zero time in inspect output.
pub fn zero_time() -> String {
    "0001-01-01T00:00:00Z".to_string()
}

/// RFC3339 with nanosecond precision, matching `time.Time` JSON output.
pub fn rfc3339(seconds: i64) -> String {
    DateTime::<Utc>::from_timestamp(seconds, 0)
        .map(|time| time.to_rfc3339_opts(SecondsFormat::Nanos, true))
        .unwrap_or_else(zero_time)
}

fn human_status(view: &ContainerView) -> String {
    let now = Utc::now().timestamp();
    match view.state.status {
        LifecycleState::Created => "Created".to_string(),
        LifecycleState::Running => match view.state.started_at {
            Some(started) => format!("Up {}", human_duration(now.saturating_sub(started))),
            None => "Up".to_string(),
        },
        LifecycleState::Paused => "Up (Paused)".to_string(),
        LifecycleState::Restarting => "Restarting".to_string(),
        LifecycleState::Removing => "Removing".to_string(),
        LifecycleState::Exited => {
            let code = view.state.exit_code.unwrap_or(0);
            match view.state.finished_at {
                Some(finished) => format!(
                    "Exited ({code}) {} ago",
                    human_duration(now.saturating_sub(finished))
                ),
                None => format!("Exited ({code})"),
            }
        }
        LifecycleState::Dead => "Dead".to_string(),
    }
}

/// Compact Docker-style duration used in `docker ps` status text.
pub fn human_duration(seconds: i64) -> String {
    if seconds < 1 {
        return "Less than a second".to_string();
    }
    let (value, unit) = if seconds < 60 {
        (seconds, "second")
    } else if seconds < 3_600 {
        (seconds / 60, "minute")
    } else if seconds < 86_400 {
        (seconds / 3_600, "hour")
    } else if seconds < 604_800 {
        (seconds / 86_400, "day")
    } else if seconds < 2_592_000 {
        (seconds / 604_800, "week")
    } else if seconds < 31_536_000 {
        (seconds / 2_592_000, "month")
    } else {
        (seconds / 31_536_000, "year")
    };
    if value == 1 {
        format!("{value} {unit}")
    } else {
        format!("{value} {unit}s")
    }
}

/// Translate a daemon filter to the API's list predicate when callers use
/// `ContainerFilter` directly.
pub fn filter_to_predicate(filter: &ContainerFilter) -> impl Fn(&ContainerView) -> bool + '_ {
    move |view| match filter {
        ContainerFilter::IdPrefix(prefix) => view.id.as_str().starts_with(prefix),
        ContainerFilter::Name(name) => view.name.as_deref() == Some(name.as_str()),
        ContainerFilter::Status(status) => view.state.status.as_api_status() == status,
        ContainerFilter::Label(key, value) => view
            .config
            .labels
            .get(key)
            .map(|found| found == value)
            .unwrap_or(value.is_empty()),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn decode(value: Value) -> Result<CreateRequest> {
        let dto: CreateRequestDto = serde_json::from_value(value).unwrap();
        dto.into_daemon(None)
    }

    #[test]
    fn accepts_moby_casing_and_zero_value_host_fields() {
        let request = decode(json!({
            "Image": "alpine:3.20",
            "Cmd": ["/bin/sh", "-c", "echo hi"],
            "Env": ["A=B"],
            "AttachStdout": true,
            "HostConfig": {
                "NetworkMode": "none",
                "RestartPolicy": {"Name": "on-failure", "MaximumRetryCount": 3},
                "Memory": 0,
                "CpuShares": 0,
                "AutoRemove": false,
                "IpcMode": "private",
                "PidMode": "private",
                "UTSMode": "private",
                "UsernsMode": "host",
                "CgroupnsMode": "host",
                "LogConfig": {"Type": "json-file", "Config": {}},
                "Binds": null,
                "PortBindings": {},
                "Mounts": []
            }
        }))
        .unwrap();
        assert_eq!(request.config.image.to_string(), "alpine:3.20");
        assert_eq!(request.config.cmd, ["/bin/sh", "-c", "echo hi"]);
        assert_eq!(request.host_config.network_mode, NetworkMode::None);
        assert_eq!(request.host_config.restart_policy, RestartPolicy::OnFailure);
        assert_eq!(request.host_config.max_retry_count, 3);
    }

    #[test]
    fn rejects_nonempty_unsupported_host_fields() {
        let error = decode(json!({
            "Image": "alpine",
            "HostConfig": {"Binds": ["/host:/container"]}
        }))
        .unwrap_err();
        assert_eq!(error.kind(), docker_errors::ErrorKind::Unsupported);
        assert!(error.message().contains("Binds"));
    }

    #[test]
    fn rejects_privileged_at_model_validation_time() {
        let request = decode(json!({
            "Image": "alpine",
            "HostConfig": {"Privileged": true}
        }))
        .unwrap();
        assert!(request.config.validate().is_ok());
        assert_eq!(
            request.host_config.validate(false).unwrap_err().kind(),
            docker_errors::ErrorKind::Forbidden
        );
    }

    #[test]
    fn default_network_mode_is_bridge() {
        let request =
            decode(json!({"Image":"alpine","HostConfig":{"NetworkMode":"default"}})).unwrap();
        assert_eq!(request.host_config.network_mode, NetworkMode::Bridge);
    }

    #[test]
    fn rejects_runtime_features_that_would_be_silently_ignored() {
        let cases = [
            (
                json!({"Image":"alpine","HostConfig":{"Memory": 1048576}}),
                "memory",
            ),
            (
                json!({"Image":"alpine","HostConfig":{"CpuShares": 512}}),
                "CPU",
            ),
            (
                json!({"Image":"alpine","HostConfig":{"PidsLimit": 100}}),
                "PidsLimit",
            ),
            (
                json!({"Image":"alpine","HostConfig":{"ExtraHosts": ["db:10.0.0.2"]}}),
                "ExtraHosts",
            ),
            (json!({"Image":"alpine","Tty": true}), "TTY"),
            (
                json!({"Image":"alpine","HostConfig":{"NetworkMode":"frontend"}}),
                "network mode",
            ),
            (
                json!({"Image":"alpine","HostConfig":{"NetworkMode":"container:abc"}}),
                "network mode",
            ),
        ];
        for (body, needle) in cases {
            let error = decode(body).unwrap_err();
            assert_eq!(
                error.kind(),
                docker_errors::ErrorKind::Unsupported,
                "{needle}: {error}"
            );
            assert!(
                error.message().contains(needle),
                "{needle}: {}",
                error.message()
            );
        }
    }

    #[test]
    fn exposed_ports_are_parsed_and_deduplicated() {
        let request = decode(json!({
            "Image": "alpine",
            "ExposedPorts": {"80/tcp": {}, "53/udp": {}}
        }))
        .unwrap();
        assert_eq!(request.config.exposed_ports.len(), 2);
    }

    #[test]
    fn empty_object_is_treated_as_zero_value() {
        assert!(is_empty_json(Some(&json!({"Type":"","Config":{}}))));
        assert!(!is_empty_json(Some(&json!({"Type":"json-file"}))));
    }
}
