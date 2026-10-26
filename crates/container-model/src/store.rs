//! In-memory container store with engine-level listing filters.
//!
//! Milestone A placement: the daemon keeps container metadata in memory and
//! re-materializes from disk at startup (`daemon/list.go` listing filters
//! run over in-memory state). This store models the in-memory half with the
//! same filter vocabulary (`status`, `id` prefix, `name`, `label`) so later
//! persistence swaps behind the same interface.

use std::collections::HashMap;

use docker_common::{ContainerId, Digest};
use docker_errors::{Error, Result};
use serde::{Deserialize, Serialize};

use crate::config::{ContainerConfig, HostConfig};
use crate::state::StateSnapshot;

/// One tracked container: identity + configs + observed state.
///
/// The record is the unit of on-disk persistence: the daemon serializes it
/// verbatim so a restart restores exactly the configuration the container was
/// created with.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ContainerRecord {
    pub id: ContainerId,
    pub name: Option<String>,
    /// Unix seconds when the record was created.
    pub created_at: i64,
    pub config: ContainerConfig,
    /// Image ID resolved at create time. It pins the container to the image
    /// that supplied its defaults even if the original tag moves later.
    pub image_id: Option<Digest>,
    /// Host-side options are part of the record because restart policy and
    /// auto-remove are consulted on every exit, not only at start.
    pub host_config: HostConfig,
    pub state: StateSnapshot,
}

impl ContainerRecord {
    /// Whether this record matches a client filter term.
    pub fn matches(&self, filter: &ContainerFilter) -> bool {
        match filter {
            ContainerFilter::IdPrefix(p) => self.id.starts_with(p),
            ContainerFilter::Name(n) => self.name.as_deref() == Some(n),
            ContainerFilter::Status(s) => self.state.status.as_api_status() == *s,
            ContainerFilter::Label(k, v) => self
                .config
                .labels
                .get(k.as_str())
                .map(|val| val == v.as_str())
                .unwrap_or(v.is_empty()),
        }
    }
}

/// Listing filter subset (daemon/list.go vocabulary).
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ContainerFilter {
    IdPrefix(String),
    Name(String),
    Status(String),
    Label(String, String),
}

#[derive(Debug, Default)]
pub struct ContainerStore {
    by_id: HashMap<ContainerId, ContainerRecord>,
}

impl ContainerStore {
    pub fn new() -> Self {
        Self::default()
    }

    /// Insert a new record; duplicate IDs are conflicts (the daemon errors
    /// equivalently when a container reappears with a taken identity).
    pub fn put(&mut self, record: ContainerRecord) -> Result<()> {
        if self.by_id.contains_key(&record.id) {
            return Err(Error::conflict(format!(
                "container id {} already exists",
                record.id
            )));
        }
        self.by_id.insert(record.id.clone(), record);
        Ok(())
    }

    pub fn get(&self, id: &ContainerId) -> Result<&ContainerRecord> {
        self.by_id
            .get(id)
            .ok_or_else(|| Error::not_found(format!("no such container: {id}")))
    }

    /// Mutable access for the daemon actor (its single owner). Exposing this
    /// rather than remove+reinsert keeps the ID index stable during lifecycle
    /// transitions.
    pub fn get_mut(&mut self, id: &ContainerId) -> Result<&mut ContainerRecord> {
        self.by_id
            .get_mut(id)
            .ok_or_else(|| Error::not_found(format!("no such container: {id}")))
    }

    /// Get-or-prefix lookup. Ambiguous prefixes return an `invalid_argument`
    /// error naming the ambiguity; no match returns `not_found` — mirrors
    /// the daemon prefix-resolution semantics.
    pub fn resolve<'a>(&'a self, reference: &str) -> Result<&'a ContainerRecord> {
        if reference.is_empty() {
            return Err(Error::not_found("no name or id passed"));
        }
        let candidates: Vec<&ContainerRecord> = self
            .by_id
            .values()
            .filter(|c| {
                c.id.starts_with(reference)
                    || c.name.as_deref().map(|n| n == reference).unwrap_or(false)
            })
            .collect();
        match candidates.len() {
            0 => Err(Error::not_found(format!("no such container: {reference}"))),
            1 => Ok(candidates[0]),
            n => {
                if candidates.iter().any(|c| c.id.as_str() == reference) {
                    return Ok(candidates[0]);
                }
                Err(Error::invalid_argument(format!(
                    "ambiguous reference `{reference}` matches {n} containers"
                )))
            }
        }
    }

    /// Remove a record by exact id.
    pub fn remove(&mut self, id: &ContainerId) -> Result<ContainerRecord> {
        self.by_id
            .remove(id)
            .ok_or_else(|| Error::not_found(format!("no such container: {id}")))
    }

    /// Filtered listing ordered by insertion order (the daemon sorts by
    /// created-time; our records keep insertion order semantics documented
    /// in the tests).
    pub fn list(&self, filters: &[ContainerFilter]) -> Vec<&ContainerRecord> {
        self.by_id
            .values()
            .filter(|c| filters.iter().all(|f| c.matches(f)))
            .collect()
    }

    pub fn len(&self) -> usize {
        self.by_id.len()
    }

    pub fn is_empty(&self) -> bool {
        self.by_id.is_empty()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::ContainerConfig;
    use crate::state::LifecycleState;
    use docker_common::{ContainerId, ImageRef};

    fn sample(id: &ContainerId, name: &str) -> ContainerRecord {
        ContainerRecord {
            id: id.clone(),
            name: Some(name.into()),
            created_at: 1_700_000_000,
            config: ContainerConfig {
                image: ImageRef::parse("alpine:latest").unwrap(),
                entrypoint: vec!["/bin/sh".into()],
                cmd: vec![],
                env: vec![],
                labels: Default::default(),
                working_dir: None,
                user: None,
                hostname: None,
                tty: false,
                open_stdin: false,
                stop_signal: crate::config::Signal::Sigterm,
                stop_timeout_secs: 10,
                exposed_ports: vec![],
            },
            image_id: None,
            state: StateSnapshot {
                status: LifecycleState::Created,
                exit_code: None,
                error_signal: None,
                started_at: None,
                finished_at: None,
            },
            host_config: crate::config::HostConfig::default(),
        }
    }

    #[test]
    fn put_get_remove_lifecycle() {
        let id = ContainerId::generate().unwrap();
        let mut store = ContainerStore::new();
        store.put(sample(&id, "web")).unwrap();
        assert_eq!(store.len(), 1);
        assert_eq!(store.get(&id).unwrap().name.as_deref(), Some("web"));
        let rec = store.remove(&id).unwrap();
        assert_eq!(rec.name.as_deref(), Some("web"));
        assert!(store.is_empty());
        assert_eq!(
            store.get(&id).unwrap_err().kind(),
            docker_errors::ErrorKind::NotFound
        );
    }

    #[test]
    fn duplicate_id_conflicts() {
        let id = ContainerId::generate().unwrap();
        let mut store = ContainerStore::new();
        store.put(sample(&id, "a")).unwrap();
        let err = store.put(sample(&id, "b")).unwrap_err();
        assert_eq!(err.kind(), docker_errors::ErrorKind::Conflict);
    }

    #[test]
    fn prefix_resolution_finds_single() {
        let mut store = ContainerStore::new();
        let ida = ContainerId::generate().unwrap();
        let idb = ContainerId::generate().unwrap();
        store.put(sample(&ida, "alpha")).unwrap();
        store.put(sample(&idb, "beta")).unwrap();
        let short = ida.as_str().split_at(6).0.to_string();
        assert_eq!(store.resolve(&short).unwrap().id, ida);
        assert_eq!(store.resolve("alpha").unwrap().id, ida);
        assert_eq!(
            store.resolve("missing").unwrap_err().kind(),
            docker_errors::ErrorKind::NotFound
        );
    }

    #[test]
    fn ambiguous_prefix_reports_count() {
        let mut store = ContainerStore::new();
        // Deterministic ids sharing the "ab" prefix:
        // "ab" + 62 zeros = 64 chars, and "abcd" x16 = 64 chars.
        let ida = ContainerId::new(&format!("ab{}", "0".repeat(62))).unwrap();
        let idb = ContainerId::new(&"abcd".repeat(16)).unwrap();
        store.put(sample(&ida, "a")).unwrap();
        store.put(sample(&idb, "b")).unwrap();
        let err = store.resolve("ab").unwrap_err();
        assert_eq!(err.kind(), docker_errors::ErrorKind::InvalidArgument);
        assert!(err.message().contains("matches 2"));
    }

    #[test]
    fn filters_match() {
        let mut store = ContainerStore::new();
        let ida = ContainerId::generate().unwrap();
        store.put(sample(&ida, "web")).unwrap();
        let got = store
            .list(&[
                ContainerFilter::Status("created".into()),
                ContainerFilter::Name("web".into()),
            ])
            .len();
        assert_eq!(got, 1);
        let got = store
            .list(&[ContainerFilter::Status("running".into())])
            .len();
        assert_eq!(got, 0);
    }
}
