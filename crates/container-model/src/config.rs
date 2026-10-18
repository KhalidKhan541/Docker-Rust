//! Container configuration structures.
//!
//! Subset of the create-container request shape: Config (image, cmd, env,
//! labels, ...) and HostConfig (resources, restart policy, network mode,
//! capabilities). Built against the Engine API contract from the pinned
//! Moby revision — `api/types/container/config.go`, `hostconfig.go`,
//! `resources.go`, plus validation that happens daemon-side before any
//! runtime is touched (`daemon/opts` helpers).

use std::collections::BTreeMap;
use std::str::FromStr;

use docker_common::ImageRef;
use docker_errors::{Error, Result};
use serde::{Deserialize, Serialize};

/// Everything handed to `POST /containers/create` that describes the
/// container's own properties (few fields are outside the HostConfig).
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct ContainerConfig {
    /// Image to run. Required by the daemon (identified at create time).
    pub image: ImageRef,
    /// Entrypoint override, may be empty for the image default.
    pub entrypoint: Vec<String>,
    /// Command override, may be empty for the image default.
    pub cmd: Vec<String>,
    /// Environment in `KEY=value` form (daemon checks the same format).
    pub env: Vec<String>,
    /// Labels — daemon validates label character set.
    pub labels: BTreeMap<String, String>,
    /// Working dir inside the container. Daemon requires absolute paths.
    #[serde(rename = "workingDir", default)]
    pub working_dir: Option<String>,
    /// User (name or UID[:GID]); daemon forwards to the runtime.
    #[serde(default)]
    pub user: Option<String>,
    #[serde(default)]
    pub hostname: Option<String>,
    /// TTY allocation (`tty` in the API). Structured logging caps entry
    /// sizes; a TTY emits non-frame protocol — flag preserved end to end.
    #[serde(default)]
    pub tty: bool,
    /// Keep stdin open (`openStdin`).
    #[serde(default)]
    pub open_stdin: bool,
    /// Signal sent to stop; must parse as a POSIX name or number.
    #[serde(rename = "stopSignal", default = "default_stop_signal")]
    pub stop_signal: Signal,
    /// Grace period, in seconds, before SIGKILL during stop.
    #[serde(rename = "stopTimeout", default = "default_stop_timeout")]
    pub stop_timeout_secs: u64,
    /// Ports exposed by the image (`exposedPorts` map keys). Format
    /// `port/proto` validated below.
    #[serde(rename = "exposedPorts", default)]
    pub exposed_ports: Vec<PortKey>,
}

fn default_stop_signal() -> Signal {
    Signal::Sigterm
}

fn default_stop_timeout() -> u64 {
    10
}

/// Runtime-accepted POSIX signals for stop/kill behavior.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub enum Signal {
    Sighup,
    Sigint,
    Sigquit,
    Sigterm,
    /// Hard termination.
    Sigkill,
    Sigusr1,
    Sigusr2,
    Sigalrm,
    Sigpipe,
}

impl Signal {
    pub fn as_str(self) -> &'static str {
        match self {
            Signal::Sighup => "SIGHUP",
            Signal::Sigint => "SIGINT",
            Signal::Sigquit => "SIGQUIT",
            Signal::Sigterm => "SIGTERM",
            Signal::Sigkill => "SIGKILL",
            Signal::Sigusr1 => "SIGUSR1",
            Signal::Sigusr2 => "SIGUSR2",
            Signal::Sigalrm => "SIGALRM",
            Signal::Sigpipe => "SIGPIPE",
        }
    }
}

impl FromStr for Signal {
    type Err = Error;

    fn from_str(s: &str) -> Result<Self> {
        match s.to_ascii_uppercase().as_str() {
            "SIGHUP" => Ok(Signal::Sighup),
            "SIGINT" => Ok(Signal::Sigint),
            "SIGQUIT" => Ok(Signal::Sigquit),
            "SIGTERM" => Ok(Signal::Sigterm),
            "SIGKILL" => Ok(Signal::Sigkill),
            "SIGUSR1" => Ok(Signal::Sigusr1),
            "SIGUSR2" => Ok(Signal::Sigusr2),
            "SIGALRM" => Ok(Signal::Sigalrm),
            "SIGPIPE" => Ok(Signal::Sigpipe),
            other => Err(Error::invalid_argument(format!(
                "invalid signal `{other}` (accepted POSIX names, case-insensitive)"
            ))),
        }
    }
}

/// Port protocol accepted by the daemon (TCP default, UDP, and SCTP).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum PortProto {
    Tcp,
    Udp,
    Sctp,
}

impl std::fmt::Display for PortProto {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(match self {
            PortProto::Tcp => "tcp",
            PortProto::Udp => "udp",
            PortProto::Sctp => "sctp",
        })
    }
}

/// `port/proto` exposed-ports key form.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub struct PortKey {
    pub port: u16,
    pub proto: PortProto,
}

impl std::fmt::Display for PortKey {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{}/{}", self.port, self.proto)
    }
}

impl FromStr for PortKey {
    type Err = Error;

    fn from_str(s: &str) -> Result<Self> {
        let Some((port_s, proto_s)) = s.split_once('/') else {
            return Err(Error::invalid_argument(format!(
                "invalid port key `{s}`: expected `port/proto`"
            )));
        };
        let port: u16 = port_s
            .parse()
            .map_err(|_| Error::invalid_argument(format!("invalid port in key `{s}`")))?;
        let proto = match proto_s {
            "tcp" => PortProto::Tcp,
            "udp" => PortProto::Udp,
            "sctp" => PortProto::Sctp,
            other => {
                return Err(Error::invalid_argument(format!(
                    "invalid protocol `{other}` in port key `{s}`"
                )))
            }
        };
        Ok(PortKey { port, proto })
    }
}

/// Network mode selection — the daemon StringParser mapping from
/// `daemon/container/hostconfig.go`.
#[derive(Debug, Clone, PartialEq, Eq, Hash, Default, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum NetworkMode {
    /// Host network namespace (no isolation).
    Host,
    /// No network (`none`).
    None,
    /// Default bridge; also the implicit default when the field is absent
    /// from the request JSON, matching daemon behavior (`isDefault`).
    #[default]
    Bridge,
    /// Join another container's network namespace (`container:<id|name>`).
    Container(String),
    /// A named user-defined network.
    Named(String),
}

impl std::fmt::Display for NetworkMode {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            NetworkMode::Host => f.write_str("host"),
            NetworkMode::None => f.write_str("none"),
            NetworkMode::Bridge => f.write_str("bridge"),
            NetworkMode::Container(c) => write!(f, "container:{c}"),
            NetworkMode::Named(n) => f.write_str(n),
        }
    }
}

/// Restart policy semantics (`HostConfig.RestartPolicy.Name`).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Default, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum RestartPolicy {
    /// Never restart (default).
    #[default]
    No,
    /// Restart on non-zero exit, capped by `maximum_retry_count` if set.
    OnFailure,
    /// Always restart regardless of exit status. The daemon автомобилей
    /// restarts and caps nothing.
    Always,
    /// Always except on a stopped-by-user (clean stop) reload.
    UnlessStopped,
}

/// Host-side configuration: resources, restart policy, network, security.
/// Only fields Milestone D implements are represented; everything else is
/// rejected instead of silently dropped — explicit over silent (mirrors
/// upstream behavior where unknown HostConfig fields crash older daemons
/// via strict-validation, while new fields with strict validation are
///   refused by the validation boundary).
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct HostConfig {
    /// Memory limit in bytes (0 = none).
    #[serde(default)]
    pub memory_bytes: u64,
    /// Relative CPU shares (daemon: `--cpu-shares`, 2..=262144).
    #[serde(default)]
    pub cpu_shares: u32,
    /// Max PID count inside the container.
    #[serde(default)]
    pub pids_limit: Option<u64>,
    /// Network the container joins.
    #[serde(rename = "networkMode", default)]
    pub network_mode: NetworkMode,
    /// Restart policy. Engine API nests; here flattened.
    #[serde(default)]
    pub restart_policy: RestartPolicy,
    /// Number of retries for on-failure restarts.
    #[serde(default)]
    pub max_retry_count: u16,
    /// Auto-remove after exit (daemon: `--rm`).
    #[serde(default)]
    pub auto_remove: bool,
    /// Privileged mode — *never* silently accepted; validation blocks the
    /// value unless engine policy allows it (always *false* at Milestone D).
    #[serde(default)]
    pub privileged: bool,
    /// Read-only root filesystem.
    #[serde(rename = "readonlyRootfs", default)]
    pub readonly_rootfs: bool,
    /// Extra hosts injected into /etc/hosts: validated as `host:ip`.
    #[serde(default)]
    pub extra_hosts: Vec<String>,
}

impl Default for HostConfig {
    fn default() -> Self {
        Self {
            memory_bytes: 0,
            cpu_shares: 0,
            pids_limit: None,
            network_mode: NetworkMode::Bridge,
            restart_policy: RestartPolicy::No,
            max_retry_count: 0,
            auto_remove: false,
            privileged: false,
            readonly_rootfs: false,
            extra_hosts: vec![],
        }
    }
}

impl ContainerConfig {
    /// Validate rules the daemon enforces before admitting a container.
    pub fn validate(&self) -> Result<()> {
        validate_env(&self.env)?;
        for l in self.labels.keys() {
            validate_label_key(l)?;
        }
        if let Some(w) = &self.working_dir {
            if !w.starts_with('/') {
                return Err(Error::invalid_argument(format!(
                    "working_dir `{w}` must be an absolute path (the daemon requires an absolute path)"
                )));
            }
        }
        Ok(())
    }

    /// Full executable command vector (entrypoint ++ cmd), the shape the
    /// OCI runtime receives.
    pub fn effective_argv(&self) -> Vec<String> {
        let mut argv = self.entrypoint.clone();
        argv.extend(self.cmd.iter().cloned());
        argv
    }
}

fn validate_env(entries: &[String]) -> Result<()> {
    for e in entries {
        let Some((key, _)) = e.split_once('=') else {
            return Err(Error::invalid_argument(format!(
                "invalid env `{e}`: expected `KEY=value`"
            )));
        };
        if key.is_empty() {
            return Err(Error::invalid_argument(
                "invalid env: empty key is not accepted",
            ));
        }
        // Uppercase keys are normal (`FOO=bar`); only whitespace and
        // control characters are rejected — the spoofing risks documented
        // in the daemon-side env checks.
        if key.contains(char::is_whitespace) {
            return Err(Error::invalid_argument(format!(
                "invalid env key `{key}`: whitespace not allowed"
            )));
        }
        if key.bytes().any(|b| b < 0x20 || b == 0x7f) {
            return Err(Error::invalid_argument(format!(
                "invalid env key `{key}`: control characters not allowed"
            )));
        }
    }
    Ok(())
}

/// Label key validation mirrors daemon `daemon/opts` label processing: the
/// whole key may contain any characters except control and whitespace?
/// In Moby the shortcut is permissive; we restrict to keep log/event
/// attributes always JSON-safe. Documented in API_COMPATIBILITY.md.
pub fn validate_label_key(key: &str) -> Result<()> {
    if key.is_empty() {
        return Err(Error::invalid_argument("label key must not be empty"));
    }
    if key.len() > 255 {
        return Err(Error::invalid_argument(format!(
            "label key `{}` exceeds 255 bytes",
            &key[..20]
        )));
    }
    if key
        .bytes()
        .any(|b| b < 0x20 || b == b'\"' || b == b'\\' || b > 0x7f)
    {
        return Err(Error::invalid_argument(format!(
            "label key `{key}` must be ASCII printable without `\"` or backslash"
        )));
    }
    Ok(())
}

impl HostConfig {
    /// Validate host config amounts + policy pairings the daemon checks.
    pub fn validate(&self, allow_privileged: bool) -> Result<()> {
        if self.memory_bytes > 0 && self.memory_bytes < 1024 {
            return Err(Error::invalid_argument(
                "memory limit below 1KiB not reasonable",
            ));
        }
        if self.cpu_shares != 0 && !(2..=262_144).contains(&self.cpu_shares) {
            return Err(Error::invalid_argument(format!(
                "cpu-shares must be 2..=262144, got {}",
                self.cpu_shares
            )));
        }
        if let Some(pid) = self.pids_limit {
            if pid == 0 {
                return Err(Error::invalid_argument(
                    "pids-limit 0 is invalid; omit for unlimited",
                ));
            }
        }
        if self.restart_policy != RestartPolicy::OnFailure && self.max_retry_count != 0 {
            return Err(Error::invalid_argument(
                "max_retry_count is only valid with on-failure restart policy",
            ));
        }
        if self.privileged && !allow_privileged {
            return Err(Error::forbidden(
                "privileged containers are not allowed on this engine install",
            ));
        }
        for xh in &self.extra_hosts {
            let Some((host, ip)) = xh.split_once(':') else {
                return Err(Error::invalid_argument(format!(
                    "extra_hosts entry `{xh}`: expected `host:ip`"
                )));
            };
            if host.is_empty() || ip.is_empty() {
                return Err(Error::invalid_argument(format!(
                    "extra_hosts entry `{xh}`: empty host or ip"
                )));
            }
            if !host
                .bytes()
                .all(|b| b.is_ascii_alphanumeric() || b == b'.' || b == b'-' || b == b'_')
            {
                return Err(Error::invalid_argument(format!(
                    "extra_hosts host `{host}`: characters outside [a-zA-Z0-9._-]"
                )));
            }
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use docker_common::ImageRef;

    fn base_config() -> ContainerConfig {
        ContainerConfig {
            image: ImageRef::parse("alpine:3.19").unwrap(),
            entrypoint: vec!["/bin/sh".into()],
            cmd: vec!["-c".into(), "echo hi".into()],
            env: vec!["FOO=bar".into(), "EMPTY=".into()],
            labels: BTreeMap::new(),
            working_dir: Some("/work".into()),
            user: None,
            hostname: Some("web-1".into()),
            tty: false,
            open_stdin: false,
            stop_signal: Signal::Sigterm,
            stop_timeout_secs: 5,
            exposed_ports: vec![PortKey {
                port: 8080,
                proto: PortProto::Tcp,
            }],
        }
    }

    #[test]
    fn valid_config_passes() {
        let c = base_config();
        c.validate()
            .unwrap_or_else(|e| panic!("config invalid: {e}"));
        let host = HostConfig::default();
        host.validate(false).unwrap();
        assert_eq!(
            c.effective_argv(),
            vec![
                "/bin/sh".to_string(),
                "-c".to_string(),
                "echo hi".to_string()
            ]
        );
    }

    #[test]
    fn env_validation_rejects_missing_equals() {
        let mut c = base_config();
        c.env = vec!["BARE".into()];
        assert_eq!(
            c.validate().unwrap_err().kind(),
            docker_errors::ErrorKind::InvalidArgument
        );
        c.env = vec!["=novalue".into()];
        assert!(c.validate().is_err());
        c.env = vec!["K EY=values".into()];
        assert!(c.validate().is_err());
    }

    #[test]
    fn working_dir_must_be_absolute() {
        let mut c = base_config();
        c.working_dir = Some("rel/dir".into());
        assert!(c.validate().is_err());
    }

    #[test]
    fn port_key_parsing() {
        assert_eq!(
            "8080/tcp".parse::<PortKey>().unwrap(),
            PortKey {
                port: 8080,
                proto: PortProto::Tcp
            }
        );
        assert_eq!("53/udp".parse::<PortKey>().unwrap().proto, PortProto::Udp);
        assert!("x/tcp".parse::<PortKey>().is_err());
        assert!("80/icmp".parse::<PortKey>().is_err());
        assert!("99999/tcp".parse::<PortKey>().is_err());
    }

    #[test]
    fn host_config_rules() {
        let mut h = HostConfig {
            cpu_shares: 1,
            ..HostConfig::default()
        };
        assert!(h.validate(false).is_err());
        h.cpu_shares = 512;
        h.validate(false).unwrap();
        h.memory_bytes = 500;
        assert!(h.validate(false).is_err());
        h.memory_bytes = 134217728;
        h.validate(false).unwrap();
        h.privileged = true;
        assert_eq!(
            h.validate(false).unwrap_err().kind(),
            docker_errors::ErrorKind::Forbidden
        );
        h.validate(true).unwrap();
    }

    #[test]
    fn restart_policy_retry_pairing() {
        let mut h = HostConfig {
            restart_policy: RestartPolicy::Always,
            max_retry_count: 3,
            ..HostConfig::default()
        };
        assert!(h.validate(true).is_err());
        h.restart_policy = RestartPolicy::OnFailure;
        h.validate(true).unwrap();
    }

    #[test]
    fn extra_hosts_validation() {
        let mut h = HostConfig {
            extra_hosts: vec!["mydb.internal:10.10.10.1".into()],
            ..HostConfig::default()
        };
        h.validate(true).unwrap();
        h.extra_hosts = vec!["broken-entry".into()];
        assert!(h.validate(true).is_err());
    }

    #[test]
    fn label_keys_validated() {
        assert!(validate_label_key("").is_err());
        assert!(validate_label_key("my.label-1_ok").is_ok());
        assert!(validate_label_key("with\"quote").is_err());
        assert!(validate_label_key("with backslash\\").is_err());
        assert!(validate_label_key("trait\u{E9}").is_err());
    }

    #[test]
    fn signal_parsing() {
        assert_eq!("sigteRM".parse::<Signal>().unwrap(), Signal::Sigterm);
        assert_eq!("SIGKILL".parse::<Signal>().unwrap(), Signal::Sigkill);
        assert!("SIGIMAGINARY".parse::<Signal>().is_err());
    }

    #[test]
    fn network_mode_display() {
        assert_eq!(NetworkMode::Host.to_string(), "host");
        assert_eq!(NetworkMode::Bridge.to_string(), "bridge");
        assert_eq!(
            NetworkMode::Container("abc".into()).to_string(),
            "container:abc"
        );
        assert_eq!(NetworkMode::Named("mynet".into()).to_string(), "mynet");
    }
}
