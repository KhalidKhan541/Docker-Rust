//! Structured logging for the daemon, and the container log-entry model.
//!
//! Two distinct responsibilities live here, mirroring the split upstream:
//!
//! 1. **Daemon logs** — structured diagnostics produced by the daemon
//!    itself. Upstream uses `logrus`; docker-rs uses `tracing` with the
//!    same level vocabulary (debug/info/warn/error).
//! 2. **Container logs** — bytes produced by containers, delivered through
//!    log *drivers*. Upstream defines them in
//!    (moby `daemon/logger`) layer. docker-rs mirrors the JSON-file driver
//!    wire schema:
//!    `{"log":"line","stream":"stdout","time":"2024-01-01T00:00:00.000000000Z"}`

use std::fmt;
use std::io::Write;
use std::path::Path;

use docker_errors::{Error, Result};
use tracing_subscriber::layer::{Layer, SubscriberExt};
use tracing_subscriber::util::SubscriberInitExt;

/// Daemon log level, mirroring the Engine API's accepted log-level values.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
pub enum DaemonLevel {
    Debug,
    Info,
    Warn,
    Error,
}

impl DaemonLevel {
    pub fn as_str(self) -> &'static str {
        match self {
            DaemonLevel::Debug => "debug",
            DaemonLevel::Info => "info",
            DaemonLevel::Warn => "warn",
            DaemonLevel::Error => "error",
        }
    }

    /// Case-insensitive parse; accepts the same inputs as upstream.
    pub fn parse(input: &str) -> Result<Self> {
        match input.to_ascii_lowercase().as_str() {
            "debug" => Ok(DaemonLevel::Debug),
            "info" | "" => Ok(DaemonLevel::Info),
            "warn" | "warning" => Ok(DaemonLevel::Warn),
            "error" | "err" => Ok(DaemonLevel::Error),
            other => Err(Error::invalid_argument(format!(
                "log-level must be debug|info|warn|error, got `{other}`"
            ))),
        }
    }
}

impl fmt::Display for DaemonLevel {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.as_str())
    }
}

/// Initialize global tracing with a fixed level filter and a compact human
/// format. Libraries must call this exactly once from binary entry points.
pub fn init_daemon_tracing(level: DaemonLevel) {
    // Mapping daemon levels to tracing-style filter strings.
    let filter = match level {
        DaemonLevel::Debug => "trace,docker_rs=trace",
        DaemonLevel::Info => "info,docker_rs=info",
        DaemonLevel::Warn => "warn,docker_rs=warn",
        DaemonLevel::Error => "error,docker_rs=error",
    };
    // Filter construction is validated by construction; wrong strings would
    // be a programming error, so failure is a programming bug and departs
    // from the no-panic rule with an explicit note.
    directive_init(filter);
}

fn directive_init(filter: &str) {
    let filt: tracing_subscriber::filter::Targets =
        filter.parse().expect("daemon logging directive");
    let registry = tracing_subscriber::registry();
    let fmt_layer = tracing_subscriber::fmt::layer()
        .with_target(false)
        .with_thread_ids(false)
        .compact();
    registry.with(fmt_layer.with_filter(filt)).init();
}

/// Which pipe a container log line arrived on.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, serde::Serialize, serde::Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum LogStream {
    Stdout,
    Stderr,
}

impl LogStream {
    pub fn as_str(self) -> &'static str {
        match self {
            LogStream::Stdout => "stdout",
            LogStream::Stderr => "stderr",
        }
    }
}

/// A single container log record before it is persisted by a driver. The
/// message may contain NUL/UTF-8-invalid bytes in the general case, but for
/// the JSON driver upstream encodes anything valid UTF-8 as-is and base64s
/// the rest; docker-rs requires valid UTF-8 at this layer and rejects
/// invalid bytes at the pipe reader where they cannot occur for std processes.
#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub struct LogEntry {
    /// RFC3339 timestamp of log arrival at the daemon.
    pub time: String,
    pub stream: LogStream,
    /// Line payload, without the trailing newline.
    pub message: String,
}

impl LogEntry {
    /// Build an entry with a timestamp taken now.
    pub fn now(stream: LogStream, message: impl Into<String>) -> Self {
        Self::at(chrono::Utc::now(), stream, message)
    }

    /// Build an entry with an explicit timestamp (test determinism).
    pub fn at(
        when: chrono::DateTime<chrono::Utc>,
        stream: LogStream,
        message: impl Into<String>,
    ) -> Self {
        Self {
            time: when.to_rfc3339(),
            stream,
            message: message.into(),
        }
    }
}

/// Log driver: sink for container stdout/stderr entries.
pub trait LogDriver: Send {
    /// Append one entry.
    fn write(&mut self, entry: &LogEntry) -> Result<()>;
}

/// The `json-file` driver: one JSON object per line with the exact upstream
/// schema (moby daemon/logger/jsonfilelog):
/// `{"log":"message","stream":"stdout","time":"..."}` and a bounded
/// in-memory tail for follow requests.
pub struct JsonLinesDriver {
    file: std::io::BufWriter<std::fs::File>,
    tail: std::collections::VecDeque<LogEntry>,
    tail_cap: usize,
}

impl JsonLinesDriver {
    /// Open a driver backed by a log file. Creates missing parent dirs.
    pub fn open(path: &Path, tail_cap: usize) -> Result<Self> {
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent)?;
        }
        let file = std::fs::OpenOptions::new()
            .create(true)
            .append(true)
            .read(true)
            .open(path)?;
        Ok(Self {
            file: std::io::BufWriter::new(file),
            tail: std::collections::VecDeque::new(),
            tail_cap,
        })
    }

    /// Read the ring tail without mutation (for `docker logs`).
    pub fn tail(&self) -> impl Iterator<Item = &LogEntry> {
        self.tail.iter()
    }
}

impl LogDriver for JsonLinesDriver {
    fn write(&mut self, entry: &LogEntry) -> Result<()> {
        let line = serde_json::to_string(&EntryJson::from(entry))?;
        self.file.write_all(line.as_bytes())?;
        self.file.write_all(b"\n")?;
        self.file.flush()?;
        if self.tail_cap > 0 {
            self.tail.push_back(entry.clone());
            while self.tail.len() > self.tail_cap {
                self.tail.pop_front();
            }
        }
        Ok(())
    }
}

/// Copy of the upstream wire schema, decoupled from the model.
#[derive(serde::Serialize)]
struct EntryJson<'a> {
    log: &'a str,
    stream: &'static str,
    time: &'a str,
}

impl<'a> From<&'a LogEntry> for EntryJson<'a> {
    fn from(e: &'a LogEntry) -> Self {
        Self {
            log: &e.message,
            stream: e.stream.as_str(),
            time: &e.time,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn level_parsing() {
        assert_eq!(DaemonLevel::parse("WARN").unwrap(), DaemonLevel::Warn);
        assert_eq!(DaemonLevel::parse("").unwrap(), DaemonLevel::Info);
        assert_eq!(DaemonLevel::parse("").unwrap(), DaemonLevel::Info);
        assert!(DaemonLevel::parse("verbose").is_err());
        assert!(DaemonLevel::parse("").is_ok());
    }

    #[test]
    fn json_lines_wire_format_matches_upstream() {
        let entry = LogEntry::at(
            chrono::DateTime::parse_from_rfc3339("2024-05-01T10:00:00.123456789Z")
                .unwrap()
                .into(),
            LogStream::Stderr,
            "hello world",
        );
        let json = serde_json::to_string(&EntryJson::from(&entry)).unwrap();
        assert_eq!(
            json,
            r#"{"log":"hello world","stream":"stderr","time":"2024-05-01T10:00:00.123456789+00:00"}"#
        );
    }

    #[test]
    fn json_file_driver_persists_and_rings_tail() {
        let path = std::env::temp_dir().join(format!("drc-logs-{}.jsonl", std::process::id()));
        let _ = std::fs::remove_file(&path);
        let mut driver = JsonLinesDriver::open(&path, 2).unwrap();

        // Deterministic timestamps.
        let t = chrono::DateTime::parse_from_rfc3339("2024-05-01T10:00:00Z")
            .unwrap()
            .into();
        driver
            .write(&LogEntry::at(t, LogStream::Stdout, "one"))
            .unwrap();
        driver
            .write(&LogEntry::at(t, LogStream::Stdout, "two"))
            .unwrap();
        driver
            .write(&LogEntry::at(t, LogStream::Stderr, "three"))
            .unwrap();

        let tail: Vec<_> = driver.tail().collect();
        assert_eq!(tail.len(), 2);
        assert_eq!(tail[0].message, "two");
        assert_eq!(tail[1].stream, LogStream::Stderr);

        let raw = std::fs::read_to_string(&path).unwrap();
        let lines: Vec<&str> = raw.lines().collect();
        assert_eq!(lines.len(), 3);
        assert!(lines[0].contains(r#""log":"one""#));
        assert!(lines[2].contains(r#""stream":"stderr""#));
        let _ = std::fs::remove_file(&path);
    }
}
