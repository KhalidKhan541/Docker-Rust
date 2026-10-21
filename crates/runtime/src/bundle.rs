//! OCI bundle generation from the daemon's container model.
//!
//! A bundle is a directory containing `config.json`; `root.path` points at a
//! prepared root filesystem supplied by the image/store layer. The mapping
//! follows Moby's `oci.DefaultLinuxSpec` and `CreateDaemonEnvironment` where
//! the Milestone D model has enough information, and returns `Unsupported`
//! for model values that cannot be represented honestly yet.

use std::collections::BTreeMap;
use std::fs;
use std::path::{Path, PathBuf};

use docker_common::ContainerId;
use docker_container_model::config::{ContainerConfig, HostConfig, NetworkMode, Signal};
use docker_errors::{Error, Result};

use crate::spec::{
    Linux, LinuxCpu, LinuxMemory, LinuxNamespace, LinuxNamespaceType, LinuxPids, LinuxResources,
    Mount, Process, Root, Spec, User, OCI_VERSION,
};

/// Default PATH used by Moby on Unix (`moby/oci/defaults.go`).
pub const DEFAULT_PATH_ENV: &str = "/usr/local/sbin:/usr/local/bin:/usr/sbin:/usr/bin:/sbin:/bin";

/// A materialized bundle directory.
#[derive(Debug, Clone, PartialEq)]
pub struct Bundle {
    root: PathBuf,
    spec: Spec,
}

impl Bundle {
    /// Create a new bundle directory and write `config.json`. The directory
    /// must not already exist; an existing path is a conflict rather than
    /// silently reused.
    pub fn create(root: impl Into<PathBuf>, spec: Spec) -> Result<Self> {
        let root = root.into();
        spec.validate()?;
        if root.exists() {
            return Err(Error::conflict(format!(
                "bundle directory `{}` already exists",
                root.display()
            )));
        }
        if let Some(parent) = root.parent() {
            fs::create_dir_all(parent).map_err(|err| {
                Error::internal(format!(
                    "creating bundle parent `{}`: {err}",
                    parent.display()
                ))
            })?;
        }
        fs::create_dir(&root).map_err(|err| {
            Error::internal(format!(
                "creating bundle directory `{}`: {err}",
                root.display()
            ))
        })?;

        let bundle = Self { root, spec };
        if let Err(err) = bundle.write_config() {
            let _ = fs::remove_dir_all(&bundle.root);
            return Err(err);
        }
        Ok(bundle)
    }

    /// Build and materialize a bundle from the container model.
    pub fn from_container(
        root: impl Into<PathBuf>,
        id: &ContainerId,
        config: &ContainerConfig,
        host_config: &HostConfig,
        rootfs: impl AsRef<Path>,
    ) -> Result<Self> {
        let spec = spec_from_container(id, config, host_config, rootfs.as_ref())?;
        Self::create(root, spec)
    }

    /// Open an existing bundle and validate its configuration.
    pub fn open(root: impl Into<PathBuf>) -> Result<Self> {
        let root = root.into();
        let config_path = root.join("config.json");
        let bytes = fs::read(&config_path).map_err(|err| {
            if err.kind() == std::io::ErrorKind::NotFound {
                Error::not_found(format!(
                    "OCI bundle `{}` has no config.json",
                    root.display()
                ))
            } else {
                Error::internal(format!("reading `{}`: {err}", config_path.display()))
            }
        })?;
        let spec = Spec::from_json(&bytes)?;
        Ok(Self { root, spec })
    }

    pub fn root(&self) -> &Path {
        &self.root
    }

    pub fn config_path(&self) -> PathBuf {
        self.root.join("config.json")
    }

    pub fn spec(&self) -> &Spec {
        &self.spec
    }

    pub fn spec_mut(&mut self) -> &mut Spec {
        &mut self.spec
    }

    /// Rewrite `config.json` after mutating the spec.
    pub fn write_config(&self) -> Result<()> {
        let bytes = self.spec.to_json_pretty()?;
        let config_path = self.config_path();
        fs::write(&config_path, bytes).map_err(|err| {
            Error::internal(format!(
                "writing OCI config `{}`: {err}",
                config_path.display()
            ))
        })
    }

    /// Remove the bundle directory. Callers must ensure the task has exited.
    pub fn remove(&self) -> Result<()> {
        match fs::remove_dir_all(&self.root) {
            Ok(()) => Ok(()),
            Err(err) if err.kind() == std::io::ErrorKind::NotFound => Ok(()),
            Err(err) => Err(Error::internal(format!(
                "removing bundle `{}`: {err}",
                self.root.display()
            ))),
        }
    }
}

/// Translate the daemon's validated container configuration into an OCI
/// 1.3 Linux spec.
pub fn spec_from_container(
    id: &ContainerId,
    config: &ContainerConfig,
    host_config: &HostConfig,
    rootfs: &Path,
) -> Result<Spec> {
    config.validate()?;
    host_config.validate(false)?;

    if !rootfs.is_absolute() {
        return Err(Error::invalid_argument(format!(
            "rootfs path `{}` must be absolute",
            rootfs.display()
        )));
    }
    let metadata = fs::metadata(rootfs).map_err(|err| {
        Error::invalid_argument(format!(
            "rootfs `{}` is not accessible: {err}",
            rootfs.display()
        ))
    })?;
    if !metadata.is_dir() {
        return Err(Error::invalid_argument(format!(
            "rootfs `{}` is not a directory",
            rootfs.display()
        )));
    }
    let rootfs = fs::canonicalize(rootfs).map_err(|err| {
        Error::invalid_argument(format!(
            "rootfs `{}` could not be canonicalized: {err}",
            rootfs.display()
        ))
    })?;
    let rootfs_path = rootfs.to_str().ok_or_else(|| {
        Error::invalid_argument(format!(
            "rootfs path `{}` is not valid UTF-8, which OCI `root.path` requires",
            rootfs.display()
        ))
    })?;

    if !host_config.extra_hosts.is_empty() {
        return Err(Error::unsupported(
            "extra_hosts requires /etc/hosts injection, which the native runtime does not implement yet",
        ));
    }
    if matches!(
        host_config.network_mode,
        NetworkMode::Container(_) | NetworkMode::Named(_)
    ) {
        return Err(Error::unsupported(format!(
            "network mode `{}` requires joining an existing network namespace, which the native runtime does not implement yet",
            host_config.network_mode
        )));
    }

    let args = config.effective_argv();
    if args.is_empty() {
        return Err(Error::invalid_argument(
            "container has no command (entrypoint and cmd are both empty)",
        ));
    }

    let hostname = config
        .hostname
        .clone()
        .filter(|value| !value.is_empty())
        .unwrap_or_else(|| short_id(id));
    validate_hostname(&hostname)?;

    let mut namespaces = vec![
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
    ];
    if host_config.network_mode != NetworkMode::Host {
        namespaces.insert(
            1,
            LinuxNamespace {
                typ: LinuxNamespaceType::Network,
                path: String::new(),
            },
        );
    }

    let mut env_defaults = vec![
        format!("PATH={DEFAULT_PATH_ENV}"),
        format!("HOSTNAME={hostname}"),
    ];
    if config.tty {
        env_defaults.push("TERM=xterm".to_string());
    }
    let env = replace_or_append_env(&env_defaults, &config.env);

    let user = parse_user(config.user.as_deref())?;
    let cwd = config
        .working_dir
        .clone()
        .filter(|value| !value.is_empty())
        .unwrap_or_else(|| "/".to_string());

    let resources = resources_from_host_config(host_config)?;

    let process = Process {
        terminal: config.tty,
        console_size: None,
        user,
        args,
        command_line: String::new(),
        env,
        cwd,
        capabilities: None,
        rlimits: Vec::new(),
        no_new_privileges: false,
        apparmor_profile: String::new(),
        oom_score_adj: None,
        scheduler: None,
        selinux_label: String::new(),
        io_priority: None,
        exec_cpu_affinity: None,
    };

    let mut annotations = BTreeMap::new();
    annotations.insert("io.docker-rs.image".to_string(), config.image.to_string());
    annotations.insert(
        "io.docker-rs.network-mode".to_string(),
        host_config.network_mode.to_string(),
    );

    let spec = Spec {
        version: OCI_VERSION.to_string(),
        process: Some(process),
        root: Some(Root {
            path: rootfs_path.to_string(),
            readonly: host_config.readonly_rootfs,
        }),
        hostname,
        domainname: String::new(),
        mounts: Vec::<Mount>::new(),
        hooks: None,
        annotations,
        linux: Some(Linux {
            namespaces,
            resources,
            ..Linux::default()
        }),
    };
    spec.validate()?;
    Ok(spec)
}

fn resources_from_host_config(host_config: &HostConfig) -> Result<Option<LinuxResources>> {
    let mut resources = LinuxResources::default();

    if host_config.memory_bytes > 0 {
        let limit = i64::try_from(host_config.memory_bytes)
            .map_err(|_| Error::invalid_argument("memory limit exceeds the OCI int64 range"))?;
        resources.memory = Some(LinuxMemory {
            limit: Some(limit),
            ..LinuxMemory::default()
        });
    }
    if host_config.cpu_shares > 0 {
        resources.cpu = Some(LinuxCpu {
            shares: Some(u64::from(host_config.cpu_shares)),
            ..LinuxCpu::default()
        });
    }
    if let Some(limit) = host_config.pids_limit {
        let limit = i64::try_from(limit)
            .map_err(|_| Error::invalid_argument("pids limit exceeds the OCI int64 range"))?;
        resources.pids = Some(LinuxPids { limit: Some(limit) });
    }

    if resources.is_empty() {
        Ok(None)
    } else {
        Ok(Some(resources))
    }
}

fn parse_user(value: Option<&str>) -> Result<User> {
    let Some(value) = value.map(str::trim).filter(|value| !value.is_empty()) else {
        return Ok(User::default());
    };

    let (uid_text, gid_text) = match value.split_once(':') {
        Some((uid, gid)) => (uid, Some(gid)),
        None => (value, None),
    };

    if uid_text.is_empty() {
        return Err(Error::invalid_argument(
            "container user has an empty UID component",
        ));
    }
    let Some(uid) = parse_id(uid_text) else {
        return Err(Error::unsupported(format!(
            "resolving user `{value}` requires a rootfs /etc/passwd lookup, which the native runtime does not implement yet; use numeric UID[:GID]"
        )));
    };
    let gid = match gid_text {
        Some(gid_text) => {
            if gid_text.is_empty() {
                return Err(Error::invalid_argument(
                    "container user has an empty GID component",
                ));
            }
            Some(parse_id(gid_text).ok_or_else(|| {
                Error::unsupported(format!(
                    "resolving group `{gid_text}` requires a rootfs /etc/group lookup, which the native runtime does not implement yet; use numeric UID:GID"
                ))
            })?)
        }
        None => None,
    };

    Ok(User {
        uid,
        gid: gid.unwrap_or(0),
        ..User::default()
    })
}

fn parse_id(value: &str) -> Option<u32> {
    value.parse::<u32>().ok()
}

fn short_id(id: &ContainerId) -> String {
    id.as_str().chars().take(12).collect()
}

fn validate_hostname(hostname: &str) -> Result<()> {
    if hostname.len() > 64 {
        return Err(Error::invalid_argument(
            "hostname exceeds the 64-byte kernel limit",
        ));
    }
    if hostname
        .bytes()
        .any(|byte| byte.is_ascii_whitespace() || byte.is_ascii_control())
    {
        return Err(Error::invalid_argument(
            "hostname must not contain whitespace or control characters",
        ));
    }
    Ok(())
}

/// Moby `ReplaceOrAppendEnvValues`: defaults come first; matching keys in
/// `overrides` replace in place, new keys append preserving request order.
pub fn replace_or_append_env(defaults: &[String], overrides: &[String]) -> Vec<String> {
    let mut result = defaults.to_vec();
    for entry in overrides {
        let Some((key, _)) = entry.split_once('=') else {
            // The model validates this before bundle generation; preserve a
            // malformed entry only if a caller invokes this helper directly.
            result.push(entry.clone());
            continue;
        };
        let prefix = format!("{key}=");
        match result
            .iter_mut()
            .find(|existing| existing.starts_with(&prefix))
        {
            Some(existing) => *existing = entry.clone(),
            None => result.push(entry.clone()),
        }
    }
    result
}

/// The OCI stop signal is not part of `config.json`; the daemon keeps it.
/// This helper keeps the translation explicit for future callers.
pub fn stop_signal_number(signal: Signal) -> i32 {
    signal.number()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::oci::ensure_supported;
    use crate::spec::LinuxNamespaceType;
    use docker_common::ImageRef;
    use docker_container_model::config::{PortKey, PortProto};
    use tempfile::TempDir;

    fn id() -> ContainerId {
        ContainerId::from_generated("a".repeat(64)).unwrap()
    }

    fn config() -> ContainerConfig {
        ContainerConfig {
            image: ImageRef::parse("alpine:3.19").unwrap(),
            entrypoint: vec!["/bin/sh".into()],
            cmd: vec!["-c".into(), "echo hi".into()],
            env: vec!["FOO=bar".into(), "PATH=/custom".into()],
            labels: BTreeMap::new(),
            working_dir: Some("/work".into()),
            user: Some("1000:1001".into()),
            hostname: Some("web-1".into()),
            tty: false,
            open_stdin: false,
            stop_signal: Signal::Sigterm,
            stop_timeout_secs: 10,
            exposed_ports: vec![PortKey {
                port: 8080,
                proto: PortProto::Tcp,
            }],
        }
    }

    fn host_config() -> HostConfig {
        HostConfig {
            network_mode: NetworkMode::None,
            ..HostConfig::default()
        }
    }

    #[test]
    fn env_replacement_matches_moby_order() {
        let defaults = vec!["PATH=/default".to_string(), "HOSTNAME=abc".to_string()];
        let overrides = vec!["PATH=/custom".to_string(), "NEW=1".to_string()];
        assert_eq!(
            replace_or_append_env(&defaults, &overrides),
            vec!["PATH=/custom", "HOSTNAME=abc", "NEW=1"]
        );
    }

    #[test]
    fn translates_container_model_to_oci_spec() {
        let rootfs = TempDir::new().unwrap();
        let spec = spec_from_container(&id(), &config(), &host_config(), rootfs.path()).unwrap();
        assert_eq!(spec.version, OCI_VERSION);
        assert_eq!(spec.hostname, "web-1");
        let process = spec.process.as_ref().unwrap();
        assert_eq!(process.args, ["/bin/sh", "-c", "echo hi"]);
        assert_eq!(process.cwd, "/work");
        assert_eq!(process.user.uid, 1000);
        assert_eq!(process.user.gid, 1001);
        assert!(process.env.contains(&"PATH=/custom".to_string()));
        assert!(process.env.contains(&"HOSTNAME=web-1".to_string()));
        assert!(process.env.contains(&"FOO=bar".to_string()));
        let linux = spec.linux.as_ref().unwrap();
        assert!(linux
            .namespaces
            .iter()
            .any(|namespace| namespace.typ == LinuxNamespaceType::Pid));
        assert!(linux
            .namespaces
            .iter()
            .any(|namespace| namespace.typ == LinuxNamespaceType::Network));
        ensure_supported(&spec).expect("the generated `none`-network bundle should be executable");
    }

    #[test]
    fn default_hostname_uses_short_id() {
        let rootfs = TempDir::new().unwrap();
        let mut cfg = config();
        cfg.hostname = None;
        let spec = spec_from_container(&id(), &cfg, &host_config(), rootfs.path()).unwrap();
        assert_eq!(spec.hostname, "a".repeat(12));
    }

    #[test]
    fn host_network_omits_network_namespace() {
        let rootfs = TempDir::new().unwrap();
        let mut host = host_config();
        host.network_mode = NetworkMode::Host;
        let spec = spec_from_container(&id(), &config(), &host, rootfs.path()).unwrap();
        let linux = spec.linux.unwrap();
        assert!(!linux
            .namespaces
            .iter()
            .any(|namespace| namespace.typ == LinuxNamespaceType::Network));
    }

    #[test]
    fn resources_follow_host_config() {
        let rootfs = TempDir::new().unwrap();
        let mut host = host_config();
        host.memory_bytes = 16 * 1024 * 1024;
        host.cpu_shares = 512;
        host.pids_limit = Some(64);
        let spec = spec_from_container(&id(), &config(), &host, rootfs.path()).unwrap();
        let resources = spec.linux.unwrap().resources.unwrap();
        assert_eq!(resources.memory.unwrap().limit, Some(16 * 1024 * 1024));
        assert_eq!(resources.cpu.unwrap().shares, Some(512));
        assert_eq!(resources.pids.unwrap().limit, Some(64));
    }

    #[test]
    fn bridge_mode_is_recorded_not_dropped() {
        let rootfs = TempDir::new().unwrap();
        let mut host = host_config();
        host.network_mode = NetworkMode::Bridge;
        let spec = spec_from_container(&id(), &config(), &host, rootfs.path()).unwrap();
        assert_eq!(
            spec.annotations
                .get("io.docker-rs.network-mode")
                .map(String::as_str),
            Some("bridge")
        );
        let err = ensure_supported(&spec).unwrap_err();
        assert_eq!(err.kind(), docker_errors::ErrorKind::Unsupported);
        assert!(err.to_string().contains("bridge"));
    }

    #[test]
    fn named_network_is_explicitly_unsupported() {
        let rootfs = TempDir::new().unwrap();
        let mut host = host_config();
        host.network_mode = NetworkMode::Named("frontend".into());
        let err = spec_from_container(&id(), &config(), &host, rootfs.path()).unwrap_err();
        assert_eq!(err.kind(), docker_errors::ErrorKind::Unsupported);
    }

    #[test]
    fn extra_hosts_are_explicitly_unsupported() {
        let rootfs = TempDir::new().unwrap();
        let mut host = host_config();
        host.extra_hosts = vec!["db:10.0.0.2".into()];
        let err = spec_from_container(&id(), &config(), &host, rootfs.path()).unwrap_err();
        assert_eq!(err.kind(), docker_errors::ErrorKind::Unsupported);
    }

    #[test]
    fn username_is_explicitly_unsupported() {
        let rootfs = TempDir::new().unwrap();
        let mut cfg = config();
        cfg.user = Some("nobody".into());
        let err = spec_from_container(&id(), &cfg, &host_config(), rootfs.path()).unwrap_err();
        assert_eq!(err.kind(), docker_errors::ErrorKind::Unsupported);
        assert!(err.to_string().contains("passwd"));
    }

    #[test]
    fn missing_rootfs_is_invalid() {
        let rootfs = TempDir::new().unwrap();
        let missing = rootfs.path().join("does-not-exist");
        let err = spec_from_container(&id(), &config(), &host_config(), &missing).unwrap_err();
        assert_eq!(err.kind(), docker_errors::ErrorKind::InvalidArgument);
    }

    #[test]
    fn bundle_round_trip_and_conflict() {
        let state = TempDir::new().unwrap();
        let rootfs = TempDir::new().unwrap();
        let spec = spec_from_container(&id(), &config(), &host_config(), rootfs.path()).unwrap();
        let bundle = Bundle::create(state.path().join("bundle"), spec.clone()).unwrap();
        assert!(bundle.config_path().is_file());
        let loaded = Bundle::open(bundle.root()).unwrap();
        assert_eq!(loaded.spec(), &spec);
        assert_eq!(
            Bundle::create(state.path().join("bundle"), spec)
                .unwrap_err()
                .kind(),
            docker_errors::ErrorKind::Conflict
        );
        loaded.remove().unwrap();
        assert!(!bundle.root().exists());
    }
}
