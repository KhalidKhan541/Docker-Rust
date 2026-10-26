//! On-disk container records.
//!
//! Each container owns `<data-root>/containers/<id>/`, shared with the native
//! rootfs provider:
//!
//! ```text
//! containers/<id>/container.json   record (config, host config, state)
//! containers/<id>/rootfs/          optional native root filesystem
//! ```
//!
//! Docker-rs owns this document; it is deliberately not Moby's
//! `config.v2.json`/`hostconfig.json` pair, because the field shapes differ and
//! pretending otherwise would invite tools to parse it with the wrong schema.
//!
//! Writes go through `container.json.tmp` plus a rename, so a crash leaves
//! either the previous complete record or the new one. The daemon actor is the
//! only writer of a given container directory, so a fixed temporary name is
//! safe.
//!
//! A container that was running (or paused/restarting) when the daemon died
//! cannot be adopted after restart: the helper process is not tracked anymore.
//! [`ContainerDir::restore`] marks such records exited with [`LOST_EXIT_CODE`]
//! and rewrites them, instead of advertising a process that no longer exists.

use std::fs;
use std::io::Write;
use std::path::{Path, PathBuf};
use std::time::{SystemTime, UNIX_EPOCH};

use docker_common::ContainerId;
use docker_container_model::state::{LifecycleState, StateSnapshot};
use docker_container_model::store::ContainerRecord;
use docker_errors::{Error, Result};
use serde::{Deserialize, Serialize};

const RECORD_FILE: &str = "container.json";
const RECORD_TEMP: &str = "container.json.tmp";
const RECORD_VERSION: u32 = 1;

/// Exit code recorded for a container whose process was lost with the daemon.
/// 255 is runc's "no exit status available" convention, used here rather than
/// inventing a successful exit.
pub const LOST_EXIT_CODE: i32 = 255;

#[derive(Serialize)]
struct RecordRef<'a> {
    version: u32,
    container: &'a ContainerRecord,
}

#[derive(Deserialize)]
struct StoredRecord {
    version: u32,
    container: ContainerRecord,
}

/// Root of the persisted container directories (`<data-root>/containers`).
pub(crate) struct ContainerDir {
    root: PathBuf,
}

impl ContainerDir {
    /// Open (creating if needed) the container directory under `data_root`.
    pub(crate) fn open(data_root: &Path) -> Result<Self> {
        let root = data_root.join("containers");
        fs::create_dir_all(&root).map_err(|error| {
            Error::internal(format!(
                "creating container data directory `{}`: {error}",
                root.display()
            ))
        })?;
        Ok(Self { root })
    }

    pub(crate) fn container_dir(&self, id: &ContainerId) -> PathBuf {
        self.root.join(id.as_str())
    }

    /// Write a complete record, replacing any previous version atomically.
    pub(crate) fn save(&self, record: &ContainerRecord) -> Result<()> {
        let dir = self.container_dir(&record.id);
        fs::create_dir_all(&dir).map_err(|error| {
            Error::internal(format!(
                "creating container directory `{}`: {error}",
                dir.display()
            ))
        })?;
        let path = dir.join(RECORD_FILE);
        let temp = dir.join(RECORD_TEMP);
        let encoded = serde_json::to_vec(&RecordRef {
            version: RECORD_VERSION,
            container: record,
        })
        .map_err(|error| {
            Error::internal(format!("serializing container {}: {error}", record.id))
        })?;

        let mut file = fs::File::create(&temp).map_err(|error| {
            Error::internal(format!(
                "creating container record `{}`: {error}",
                temp.display()
            ))
        })?;
        file.write_all(&encoded).map_err(|error| {
            Error::internal(format!(
                "writing container record `{}`: {error}",
                temp.display()
            ))
        })?;
        file.sync_all().map_err(|error| {
            Error::internal(format!(
                "syncing container record `{}`: {error}",
                temp.display()
            ))
        })?;
        drop(file);
        fs::rename(&temp, &path).map_err(|error| {
            let _ = fs::remove_file(&temp);
            Error::internal(format!(
                "replacing container record `{}`: {error}",
                path.display()
            ))
        })?;
        Ok(())
    }

    /// Remove everything for one container: record, logs, and native rootfs.
    /// A missing directory is a no-op so removal stays idempotent.
    pub(crate) fn remove(&self, id: &ContainerId) -> Result<()> {
        let dir = self.container_dir(id);
        match fs::remove_dir_all(&dir) {
            Ok(()) => Ok(()),
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(()),
            Err(error) => Err(Error::internal(format!(
                "removing container directory `{}`: {error}",
                dir.display()
            ))),
        }
    }

    /// Load every persisted record in stable directory order.
    ///
    /// Containers that were alive at the last shutdown are normalized to
    /// `exited` with [`LOST_EXIT_CODE`] and written back. A corrupt record or
    /// an ID/directory mismatch fails startup: silently dropping a container
    /// would be worse than refusing to start with an actionable error.
    pub(crate) fn restore(&self) -> Result<Vec<ContainerRecord>> {
        let entries = fs::read_dir(&self.root).map_err(|error| {
            Error::internal(format!(
                "reading container directory `{}`: {error}",
                self.root.display()
            ))
        })?;
        let mut dirs = Vec::new();
        for entry in entries {
            let entry = entry.map_err(|error| {
                Error::internal(format!(
                    "reading container directory `{}`: {error}",
                    self.root.display()
                ))
            })?;
            let file_type = entry.file_type().map_err(|error| {
                Error::internal(format!(
                    "inspecting container path `{}`: {error}",
                    entry.path().display()
                ))
            })?;
            if file_type.is_dir() {
                dirs.push(entry.path());
            }
        }
        dirs.sort();

        let mut records = Vec::with_capacity(dirs.len());
        for dir in dirs {
            let name = match dir.file_name().and_then(|name| name.to_str()) {
                Some(name) => name.to_string(),
                None => {
                    tracing::warn!(
                        directory = %dir.display(),
                        "ignoring container directory with a non-UTF-8 name"
                    );
                    continue;
                }
            };
            let path = dir.join(RECORD_FILE);
            if !path.is_file() {
                tracing::warn!(
                    directory = %dir.display(),
                    "container directory has no container record; ignoring it"
                );
                continue;
            }
            let raw = fs::read(&path).map_err(|error| {
                Error::internal(format!(
                    "reading container record `{}`: {error}",
                    path.display()
                ))
            })?;
            let stored: StoredRecord = serde_json::from_slice(&raw).map_err(|error| {
                Error::internal(format!(
                    "corrupt container record `{}`: {error}; move the directory aside to start the daemon without it",
                    path.display()
                ))
            })?;
            if stored.version != RECORD_VERSION {
                return Err(Error::unsupported(format!(
                    "container record `{}` has version {}; this daemon reads version {RECORD_VERSION}",
                    path.display(),
                    stored.version
                )));
            }
            let mut record = stored.container;
            if record.id.as_str() != name {
                return Err(Error::internal(format!(
                    "container record `{}` names id {} but lives in directory `{name}`",
                    path.display(),
                    record.id
                )));
            }
            if record.state.status.is_alive() {
                let previous = record.state.status.as_api_status();
                record.state = StateSnapshot {
                    status: LifecycleState::Exited,
                    exit_code: Some(LOST_EXIT_CODE),
                    error_signal: None,
                    started_at: record.state.started_at,
                    finished_at: Some(unix_now()),
                };
                self.save(&record)?;
                tracing::warn!(
                    container = %record.id.as_str(),
                    previous,
                    "container process was lost with the daemon; marking it exited"
                );
            }
            records.push(record);
        }
        Ok(records)
    }
}

fn unix_now() -> i64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|duration| duration.as_secs() as i64)
        .unwrap_or_default()
}

#[cfg(test)]
mod tests {
    use super::*;
    use docker_common::ImageRef;
    use docker_container_model::config::{ContainerConfig, HostConfig, Signal};

    fn record(status: LifecycleState) -> ContainerRecord {
        ContainerRecord {
            id: ContainerId::generate().unwrap(),
            name: Some("web".into()),
            created_at: 1_700_000_000,
            config: ContainerConfig {
                image: ImageRef::parse("alpine:latest").unwrap(),
                entrypoint: vec!["/bin/sh".into()],
                cmd: vec!["-c".into(), "sleep 100".into()],
                env: vec!["A=B".into()],
                labels: Default::default(),
                working_dir: None,
                user: None,
                hostname: None,
                tty: false,
                open_stdin: false,
                stop_signal: Signal::Sigterm,
                stop_timeout_secs: 10,
                exposed_ports: vec![],
            },
            image_id: None,
            host_config: HostConfig::default(),
            state: StateSnapshot {
                status,
                exit_code: None,
                error_signal: None,
                started_at: None,
                finished_at: None,
            },
        }
    }

    #[test]
    fn round_trips_and_replaces_records() {
        let temp = tempfile::tempdir().unwrap();
        let dir = ContainerDir::open(temp.path()).unwrap();
        let mut original = record(LifecycleState::Created);
        dir.save(&original).unwrap();

        let loaded = dir.restore().unwrap();
        assert_eq!(loaded.len(), 1);
        assert_eq!(loaded[0].id, original.id);
        assert_eq!(loaded[0].config, original.config);
        assert_eq!(loaded[0].host_config, original.host_config);
        assert_eq!(loaded[0].state, original.state);

        original.name = Some("renamed".into());
        original.state.exit_code = Some(7);
        dir.save(&original).unwrap();
        let loaded = dir.restore().unwrap();
        assert_eq!(loaded.len(), 1);
        assert_eq!(loaded[0].name.as_deref(), Some("renamed"));
        assert_eq!(loaded[0].state.exit_code, Some(7));
    }

    #[test]
    fn restores_a_lost_running_container_as_exited() {
        let temp = tempfile::tempdir().unwrap();
        let dir = ContainerDir::open(temp.path()).unwrap();
        let mut original = record(LifecycleState::Running);
        original.state.started_at = Some(1_700_000_100);
        dir.save(&original).unwrap();

        let loaded = dir.restore().unwrap();
        assert_eq!(loaded.len(), 1);
        assert_eq!(loaded[0].state.status, LifecycleState::Exited);
        assert_eq!(loaded[0].state.exit_code, Some(LOST_EXIT_CODE));
        assert_eq!(loaded[0].state.started_at, Some(1_700_000_100));
        assert!(loaded[0].state.finished_at.is_some());

        // The normalized state is durable: a second restore does not rewrite
        // the exit code or finished-at timestamp.
        let again = dir.restore().unwrap();
        assert_eq!(again[0].state, loaded[0].state);
    }

    #[test]
    fn rejects_a_corrupt_record() {
        let temp = tempfile::tempdir().unwrap();
        let dir = ContainerDir::open(temp.path()).unwrap();
        let id = ContainerId::generate().unwrap();
        let path = dir.container_dir(&id);
        fs::create_dir_all(&path).unwrap();
        fs::write(path.join(RECORD_FILE), b"{not json").unwrap();

        let error = dir.restore().unwrap_err();
        assert_eq!(error.kind(), docker_errors::ErrorKind::Internal);
        assert!(error.to_string().contains("corrupt container record"));
    }

    #[test]
    fn rejects_a_record_whose_id_does_not_match_its_directory() {
        let temp = tempfile::tempdir().unwrap();
        let dir = ContainerDir::open(temp.path()).unwrap();
        let record = record(LifecycleState::Created);
        dir.save(&record).unwrap();
        let other = ContainerId::generate().unwrap();
        fs::rename(dir.container_dir(&record.id), dir.container_dir(&other)).unwrap();

        let error = dir.restore().unwrap_err();
        assert!(error.to_string().contains("lives in directory"));
    }

    #[test]
    fn rejects_an_unknown_record_version() {
        let temp = tempfile::tempdir().unwrap();
        let dir = ContainerDir::open(temp.path()).unwrap();
        let record = record(LifecycleState::Created);
        let path = dir.container_dir(&record.id);
        fs::create_dir_all(&path).unwrap();
        let document = serde_json::json!({
            "version": RECORD_VERSION + 1,
            "container": record,
        });
        fs::write(
            path.join(RECORD_FILE),
            serde_json::to_vec(&document).unwrap(),
        )
        .unwrap();

        let error = dir.restore().unwrap_err();
        assert_eq!(error.kind(), docker_errors::ErrorKind::Unsupported);
    }

    #[test]
    fn ignores_directories_without_a_record_and_removes_idempotently() {
        let temp = tempfile::tempdir().unwrap();
        let dir = ContainerDir::open(temp.path()).unwrap();
        let id = ContainerId::generate().unwrap();
        fs::create_dir_all(dir.container_dir(&id)).unwrap();

        assert!(dir.restore().unwrap().is_empty());
        dir.remove(&id).unwrap();
        dir.remove(&id).unwrap();
        assert!(!dir.container_dir(&id).exists());
    }
}
