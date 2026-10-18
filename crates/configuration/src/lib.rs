//! Container / engine configuration model.
//!
//! Mirror of the subset of Moby's `daemon.json` structure
//! (moby `daemon/config_unix.go` / `config_windows.go`, plus `pkg/opts`) that
//! docker-rs implements. Values not listed are either planned-but-not-yet
//! or intentionally unsupported; see `docs/API_COMPATIBILITY.md`.
//!
//! Layering (identical resolution order to the original daemon):
//! 1. built-in defaults
//! 2. `docker.json`-style config file
//! 3. environment variable overrides (docker-rs-specific `DRC_*` prefix)
//! 4. future CLI flags layer (Milestone H)

use std::collections::BTreeMap;
use std::path::PathBuf;

use docker_errors::{Error, Result};
use serde::{Deserialize, Serialize};

/// Where container/container-state files live. Mirrors daemon `DataRoot` /
/// `ExecRoot` pair with per-user defaults on Windows (`%ProgramData%`).
/// Serialized with the upstream `daemon.json` key spelling (kebab-case).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub struct DaemonConfig {
    /// `debug` — structured debug logging enabled.
    #[serde(default)]
    pub debug: bool,
    /// `log-level` ∈ {debug, info, warn, error} (opts level parse rules).
    #[serde(default = "default_log_level")]
    pub log_level: String,
    /// Storage driver for image/graph data. Mirrors daemon `StorageDriver`;
    /// docker-rs accepts `vfs` or full `overlay2` only, matching realistic
    // Milestone F scope.
    #[serde(default = "default_storage_driver")]
    pub storage_driver: StorageDriverSetting,
    /// Engine API listen host (tcp or npipe). Mirrors `hosts[]` subset.
    #[serde(default = "default_api_host")]
    pub api_host: String,
    /// Image/graph data root.
    #[serde(default = "default_data_root")]
    pub data_root: PathBuf,
    /// Runtime-provided container state root (defaults under data_root).
    #[serde(default)]
    pub exec_root: Option<PathBuf>,
    /// Experimental features toggle. Mirrors `experimental`.
    #[serde(default)]
    pub experimental: bool,
    /// Base runtime used for containers; mirrors `default-runtime`.
    #[serde(default = "default_runtime")]
    pub runtime: String,
    /// Optional: labels applied to daemon metadata (subset of `labels[]`).
    #[serde(default)]
    pub labels: BTreeMap<String, String>,
    /// DNS servers for embedded resolver; mirrors `dns[][\"ips\"]`.
    #[serde(default)]
    pub dns: Vec<String>,
}

fn default_log_level() -> String {
    "info".into()
}

fn default_storage_driver() -> StorageDriverSetting {
    StorageDriverSetting::Vfs
}

fn default_api_host() -> String {
    "127.0.0.1:2375".into()
}

fn default_data_root() -> PathBuf {
    // Windows default matches Docker service defaults
    // (`C:\ProgramData`).
    #[cfg(windows)]
    return PathBuf::from("C:\\ProgramData\\docker-rs");
    #[cfg(not(windows))]
    return PathBuf::from("/var/lib/docker-rs");
}

fn default_runtime() -> String {
    "rund".into()
}

impl Default for DaemonConfig {
    fn default() -> Self {
        daemon_config_default()
    }
}

fn daemon_config_default() -> DaemonConfig {
    DaemonConfig {
        debug: false,
        log_level: default_log_level(),
        storage_driver: default_storage_driver(),
        api_host: default_api_host(),
        data_root: default_data_root(),
        exec_root: None,
        experimental: false,
        runtime: default_runtime(),
        labels: BTreeMap::new(),
        dns: Vec::new(),
    }
}

/// Storage driver selection. Serialized exactly as configured, even when
/// unsupported (either `Milestone F` later validating it or explicitly
/// rejecting `daemon.json` at init).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum StorageDriverSetting {
    Overlay2,
    Vfs,
}

impl std::fmt::Display for StorageDriverSetting {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            StorageDriverSetting::Overlay2 => f.write_str("overlay2"),
            StorageDriverSetting::Vfs => f.write_str("vfs"),
        }
    }
}

impl DaemonConfig {
    /// Validate the fully merged configuration, before any engine startup.
    ///
    /// Rules (mirroring the original unless noted):
    /// * `log_level` ∈ accepted set (original opts parsing accepts
    ///   debug/info/warn/error; `fatal` is handled at logger init only).
    /// * `storage_driver` ∈ {overlay2, vfs}; overlay2 additionally must not
    ///   be selected on hosts without overlayfs (validated by the storage
    ///   manager, not here).
    /// * `data_root`, when relative, is rejected — original requires
    ///   absolute paths.
    /// * `api_host` must parse as `host:port` with port 1..=65535;
    ///   accepts `unix:`-style paths and Windows named pipe strings as
    ///   `npipe://pipe_name`.
    pub fn validate(&self) -> Result<()> {
        const LEVELS: [&str; 4] = ["debug", "info", "warn", "error"];
        if !LEVELS.contains(&self.log_level.as_str()) {
            return Err(Error::invalid_argument(format!(
                "log-level must be one of debug|info|warn|error, got `{}`",
                self.log_level
            )));
        }
        if self.data_root.is_relative() {
            return Err(Error::invalid_argument(
                "daemon data root must be an absolute path",
            ));
        }
        validate_api_host(&self.api_host)?;
        Ok(())
    }

    /// Layer a parsed JSON file over this config (file layer).
    pub fn overlay_file(mut self, file: FileConfig) -> Result<Self> {
        if let Some(v) = file.debug {
            self.debug = v;
        }
        if let Some(v) = file.log_level {
            if !v.is_empty() {
                self.log_level = v;
            }
        }
        if let Some(v) = file.storage_driver {
            self.storage_driver = v;
        }
        if let Some(v) = file.api_host {
            if !v.is_empty() {
                self.api_host = v;
            }
        }
        if let Some(v) = file.data_root {
            self.data_root = PathBuf::from(v);
        }
        if let Some(v) = file.exec_root {
            self.exec_root = Some(PathBuf::from(v));
        }
        if let Some(v) = file.experimental {
            self.experimental = v;
        }
        if let Some(v) = file.runtime {
            if !v.is_empty() {
                self.runtime = v;
            }
        }
        self.labels.extend(file.labels.clone().unwrap_or_default());
        self.dns.extend(file.dns.unwrap_or_default());
        Ok(self)
    }

    /// Layer environment overrides (`DRC_*` variable family).
    pub fn overlay_env(self, env: impl Fn(&str) -> Option<String>) -> Result<Self> {
        let mut cfg = self;
        if let Some(v) = env("DRC_LOG_LEVEL") {
            if !v.is_empty() {
                cfg.log_level = v;
            }
        }
        if let Some(v) = env("DRC_STORAGE_DRIVER") {
            if !v.is_empty() {
                cfg.storage_driver = parse_storage_driver(&v)?;
            }
        }
        if let Some(v) = env("DRC_API_HOST") {
            if !v.is_empty() {
                cfg.api_host = v;
            }
        }
        if let Some(v) = env("DRC_DATA_ROOT") {
            if !v.is_empty() {
                cfg.data_root = PathBuf::from(v);
            }
        }
        if let Some(v) = env("DRC_DEBUG") {
            if v == "1" || v.eq_ignore_ascii_case("true") {
                cfg.debug = true;
            }
        }
        Ok(cfg)
    }

    /// Effective state storage root (exec_root fallback under data_root).
    pub fn effective_exec_root(&self) -> PathBuf {
        self.exec_root
            .clone()
            .unwrap_or_else(|| self.data_root.join("exec"))
    }
}

fn parse_storage_driver(v: &str) -> Result<StorageDriverSetting> {
    match v {
        "overlay2" => Ok(StorageDriverSetting::Overlay2),
        "vfs" => Ok(StorageDriverSetting::Vfs),
        other => Err(Error::unsupported(format!(
            "storage driver `{other}` not supported by docker-rs (accepted: overlay2, vfs)"
        ))),
    }
}

fn validate_api_host(host: &str) -> Result<()> {
    if let Some(pipe) = host.strip_prefix("npipe://") {
        if pipe.is_empty() {
            return Err(Error::invalid_argument(
                "api host named pipe requires a pipe name",
            ));
        }
        return Ok(());
    }
    // IPv4/IPv6 host:port form.
    let Some((_, port)) = host.rsplit_once(':') else {
        return Err(Error::invalid_argument(format!(
            "api host `{host}` must be host:port"
        )));
    };
    if !port.bytes().all(|b| b.is_ascii_digit()) {
        return Err(Error::invalid_argument(format!(
            "api host `{host}` has invalid port"
        )));
    }
    match port.parse::<u16>() {
        Ok(0..=0) => Err(Error::invalid_argument(format!(
            "api host `{host}` port 0 not valid"
        ))),
        Ok(_) => Ok(()),
        Err(_) => Err(Error::invalid_argument(format!(
            "api host `{host}` port {port} out of range"
        ))),
    }
}

/// File config overlay parsed from JSON (all fields optional; missing keys
/// keep defaults). Keys use the upstream `daemon.json` kebab-case spelling;
/// unknown keys are rejected exactly like the upstream daemon rejects
/// unknown config-file keys at startup.
#[derive(Debug, Clone, Default, PartialEq, Eq, Deserialize)]
#[serde(deny_unknown_fields, rename_all = "kebab-case")]
pub struct FileConfig {
    pub debug: Option<bool>,
    pub log_level: Option<String>,
    pub storage_driver: Option<StorageDriverSetting>,
    pub api_host: Option<String>,
    pub data_root: Option<String>,
    pub exec_root: Option<String>,
    pub experimental: Option<bool>,
    pub runtime: Option<String>,
    pub labels: Option<BTreeMap<String, String>>,
    pub dns: Option<Vec<String>>,
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn defaults_pass_validation() {
        let c = DaemonConfig::default();
        c.validate()
            .unwrap_or_else(|e| panic!("defaults invalid: {e}"));
    }

    #[test]
    fn rejects_bad_log_level() {
        let c = DaemonConfig {
            log_level: "verbose".into(),
            ..DaemonConfig::default()
        };
        assert_eq!(
            c.validate().unwrap_err().kind(),
            docker_errors::ErrorKind::InvalidArgument
        );
    }

    #[test]
    fn rejects_relative_data_root() {
        let c = DaemonConfig {
            data_root: PathBuf::from("relative/dir"),
            ..DaemonConfig::default()
        };
        assert_eq!(
            c.validate().unwrap_err().kind(),
            docker_errors::ErrorKind::InvalidArgument
        );
    }

    #[test]
    fn file_layer_merge() {
        let base = DaemonConfig::default();
        let file: FileConfig = serde_json::from_value(json!({
            "debug": true,
            "log-level": "warn",
            "storage-driver": "overlay2",
            "data-root": "/tmp/drc"
        }))
        .unwrap();
        let merged = base.overlay_file(file).unwrap();
        assert!(merged.debug);
        assert_eq!(merged.log_level, "warn");
        assert_eq!(merged.storage_driver, StorageDriverSetting::Overlay2);
        assert_eq!(merged.data_root, PathBuf::from("/tmp/drc"));
    }

    #[test]
    fn env_layer_merge() {
        let base = DaemonConfig::default();
        let merged = base
            .overlay_env(|k| match k {
                "DRC_LOG_LEVEL" => Some("error".into()),
                "DRC_DEBUG" => Some("1".into()),
                _ => None,
            })
            .unwrap();
        assert_eq!(merged.log_level, "error");
        assert!(merged.debug);
    }

    #[test]
    fn unknown_file_keys_rejected() {
        let bad = json!({ "not-a-real-key": 1 });
        assert!(serde_json::from_value::<FileConfig>(bad).is_err());
    }

    #[test]
    fn api_host_validation() {
        let mut c = DaemonConfig {
            api_host: "127.0.0.1:2375".into(),
            ..DaemonConfig::default()
        };
        c.validate().unwrap();
        c.api_host = "127.0.0.1:99999".into();
        assert!(c.validate().is_err());
        c.api_host = "npipe://docker_rs_engine".into();
        c.validate().unwrap();
    }
}
