//! `docker-rsd`: the runnable Engine API daemon.
//!
//! This binary is deliberately thin: it resolves configuration, constructs a
//! runtime and the single-owner daemon actor, and serves the HTTP API. The
//! current build wires the deterministic synthetic runtime; the native OCI
//! backend still requires a rootfs materializer that is not connected yet, so
//! selecting it is rejected rather than silently substituted.
//!
//! Start it with:
//!
//! ```text
//! cargo run -p docker-rsd -- --host 127.0.0.1:2375 --data-root /var/lib/docker-rs
//! DOCKER_HOST=tcp://127.0.0.1:2375 docker ps
//! ```

use std::env;
use std::path::PathBuf;
use std::process;
use std::sync::Arc;
use std::thread;
use std::time::Duration;

use docker_api::{ApiServer, ServerConfig};
use docker_configuration::{DaemonConfig, FileConfig};
use docker_daemon::{Daemon, SyntheticBehavior, SyntheticRuntime};
use docker_errors::{Error, Result};
use docker_events::EventBroker;
use docker_image_store::ImageStore;
use docker_logging::{init_daemon_tracing, DaemonLevel};

const HELP: &str = "\
docker-rsd — Docker-compatible Engine API daemon

Usage:
  docker-rsd [OPTIONS]

Options:
  --host <ADDR>          Listen address (default: DRC_API_HOST or 127.0.0.1:2375)
  --data-root <DIR>      Image/container data root (must be writable)
  --config <FILE>        daemon.json-style configuration file
  --log-level <LEVEL>    debug | info | warn | error
  --runtime <NAME>       Run container lifecycle with the named runtime
                         (currently: synthetic)
  -h, --help             Print this help
  -V, --version          Print the version

Environment:
  DRC_API_HOST, DRC_LOG_LEVEL, DRC_STORAGE_DRIVER, DRC_DATA_ROOT, DRC_DEBUG
  DOCKER_HOST            Set this on the client to tcp://<host>:<port>
";

fn main() {
    if let Err(error) = run() {
        eprintln!("docker-rsd: {error}");
        process::exit(1);
    }
}

fn run() -> Result<()> {
    let command = parse_args(env::args().skip(1))?;
    let options = match command {
        Command::Help => {
            print!("{HELP}");
            return Ok(());
        }
        Command::Version => {
            println!("docker-rsd {}", env!("CARGO_PKG_VERSION"));
            return Ok(());
        }
        Command::Run(options) => options,
    };

    let mut config = DaemonConfig::default();
    if let Some(path) = options.config {
        let raw = std::fs::read_to_string(&path).map_err(|error| {
            Error::invalid_argument(format!(
                "cannot read config file `{}`: {error}",
                path.display()
            ))
        })?;
        let file: FileConfig = serde_json::from_str(&raw).map_err(|error| {
            Error::invalid_argument(format!(
                "invalid daemon config `{}`: {error}",
                path.display()
            ))
        })?;
        config = config.overlay_file(file)?;
    }
    config = config.overlay_env(|key| env::var(key).ok())?;
    if let Some(level) = options.log_level {
        config.log_level = level;
    }
    if let Some(host) = options.host {
        config.api_host = host;
    }
    if let Some(data_root) = options.data_root {
        config.data_root = data_root;
    }
    config.validate()?;
    init_daemon_tracing(DaemonLevel::parse(&config.log_level)?);
    let image_root = config.data_root.join("images");
    let images = Arc::new(ImageStore::open(image_root.clone()).map_err(|error| {
        Error::invalid_argument(format!(
            "cannot open image store at `{}`: {error}; pass a writable --data-root or DRC_DATA_ROOT",
            image_root.display()
        ))
    })?);

    let runtime_name = options.runtime.unwrap_or_else(|| "synthetic".to_string());
    if runtime_name != "synthetic" {
        return Err(Error::unsupported(format!(
            "runtime {runtime_name:?} is not runnable: the native OCI backend needs a prepared rootfs provider, \
             and no image-rootfs materializer is wired into the daemon yet; use `--runtime synthetic` for the API layer"
        )));
    }

    let listen = parse_listen_address(&config.api_host)?;
    let broker = Arc::new(EventBroker::new());
    let runtime = Arc::new(SyntheticRuntime::with_behavior(
        SyntheticBehavior::running_until_killed(),
    ));
    let daemon = Arc::new(Daemon::spawn(runtime, Arc::clone(&broker))?);
    let server_config = ServerConfig {
        runtime_name: "synthetic".to_string(),
        runtime_path: "synthetic://in-process".to_string(),
        synthetic: true,
        ..ServerConfig::default()
    };
    let api = Arc::new(
        ApiServer::with_config(daemon, Arc::clone(&broker), server_config)?
            .with_image_store(Arc::clone(&images)),
    );
    let server = api.serve(&listen)?;
    tracing::info!(address = %server.addr(), "docker-rsd listening");
    println!("docker-rsd listening on http://{}", server.addr());
    println!("client: DOCKER_HOST=tcp://{} docker version", server.addr());
    println!("image store: {}", images.root().display());
    println!(
        "warning: synthetic runtime is active; container lifecycle is real, but no host process is executed"
    );

    // Keep the process (and the `RunningServer` handle) alive until the OS
    // terminates it. A future milestone adds a control socket for graceful
    // shutdown and reload.
    loop {
        thread::sleep(Duration::from_secs(3600));
    }
}

#[derive(Debug, PartialEq, Eq)]
enum Command {
    Run(Options),
    Help,
    Version,
}

#[derive(Debug, Default, PartialEq, Eq)]
struct Options {
    host: Option<String>,
    data_root: Option<PathBuf>,
    config: Option<PathBuf>,
    log_level: Option<String>,
    runtime: Option<String>,
}

fn parse_args(args: impl Iterator<Item = String>) -> Result<Command> {
    let mut options = Options::default();
    let mut args = args.peekable();
    while let Some(argument) = args.next() {
        let (flag, inline_value) = match argument.split_once('=') {
            Some((flag, value)) => (flag, Some(value.to_string())),
            None => (argument.as_str(), None),
        };
        let mut value = |name: &str| -> Result<String> {
            if let Some(value) = inline_value.clone() {
                return Ok(value);
            }
            args.next()
                .ok_or_else(|| Error::invalid_argument(format!("{name} requires a value")))
        };
        match flag {
            "-h" | "--help" => {
                if inline_value.is_some() {
                    return Err(Error::invalid_argument("--help does not take a value"));
                }
                return Ok(Command::Help);
            }
            "-V" | "--version" => {
                if inline_value.is_some() {
                    return Err(Error::invalid_argument("--version does not take a value"));
                }
                return Ok(Command::Version);
            }
            "--host" => options.host = Some(value("--host")?),
            "--data-root" => options.data_root = Some(PathBuf::from(value("--data-root")?)),
            "--config" => options.config = Some(PathBuf::from(value("--config")?)),
            "--log-level" => options.log_level = Some(value("--log-level")?),
            "--runtime" => options.runtime = Some(value("--runtime")?),
            other => {
                return Err(Error::invalid_argument(format!(
                    "unknown option `{other}`; run docker-rsd --help"
                )))
            }
        }
    }
    Ok(Command::Run(options))
}

/// Convert a Docker-style host value into the TCP address tiny_http accepts.
/// Named pipes and Unix sockets are not silently treated as TCP.
fn parse_listen_address(raw: &str) -> Result<String> {
    let address = raw
        .strip_prefix("tcp://")
        .or_else(|| raw.strip_prefix("http://"))
        .unwrap_or(raw);
    if address.is_empty() {
        return Err(Error::invalid_argument("listen address must not be empty"));
    }
    if address.contains("://") || address.starts_with("npipe:") {
        return Err(Error::unsupported(format!(
            "listen address {raw:?} is not supported; docker-rsd currently supports TCP host:port only"
        )));
    }
    if !address.contains(':') {
        return Err(Error::invalid_argument(format!(
            "listen address `{address}` must be host:port"
        )));
    }
    Ok(address.to_string())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn args(values: &[&str]) -> std::vec::IntoIter<String> {
        values
            .iter()
            .map(|value| (*value).to_string())
            .collect::<Vec<_>>()
            .into_iter()
    }

    #[test]
    fn parses_flags_and_inline_values() {
        let command = parse_args(args(&[
            "--host=127.0.0.1:2375",
            "--log-level",
            "debug",
            "--config",
            "daemon.json",
            "--runtime=synthetic",
        ]))
        .unwrap();
        assert_eq!(
            command,
            Command::Run(Options {
                host: Some("127.0.0.1:2375".into()),
                data_root: None,
                config: Some(PathBuf::from("daemon.json")),
                log_level: Some("debug".into()),
                runtime: Some("synthetic".into()),
            })
        );
    }

    #[test]
    fn help_and_version_are_recognized() {
        assert_eq!(parse_args(args(&["--help"])).unwrap(), Command::Help);
        assert_eq!(parse_args(args(&["-V"])).unwrap(), Command::Version);
    }

    #[test]
    fn rejects_unknown_or_incomplete_flags() {
        assert_eq!(
            parse_args(args(&["--wat"])).unwrap_err().kind(),
            docker_errors::ErrorKind::InvalidArgument
        );
        assert_eq!(
            parse_args(args(&["--host"])).unwrap_err().kind(),
            docker_errors::ErrorKind::InvalidArgument
        );
    }

    #[test]
    fn listen_address_is_tcp_only() {
        assert_eq!(
            parse_listen_address("tcp://127.0.0.1:2375").unwrap(),
            "127.0.0.1:2375"
        );
        assert_eq!(
            parse_listen_address("127.0.0.1:2375").unwrap(),
            "127.0.0.1:2375"
        );
        assert_eq!(
            parse_listen_address("npipe://./pipe/docker_engine")
                .unwrap_err()
                .kind(),
            docker_errors::ErrorKind::Unsupported
        );
    }
}
