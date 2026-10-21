//! Query-string parsing and Docker's list/event filter vocabulary.
//!
//! The Engine API encodes filters as a URL-encoded JSON object. Moby accepts
//! both the old `{"key":{"value":true}}` form and the current
//! `{"key":["value"]}` form; both are handled here. Filters we cannot honor
//! are rejected instead of being ignored.

use std::collections::BTreeMap;
use std::fmt;

use chrono::DateTime;
use docker_daemon::ContainerView;
use docker_errors::{Error, Result};
use docker_events::{Event, EventType};
use serde_json::Value;

/// Decoded query string preserving repeated keys.
#[derive(Debug, Clone, Default)]
pub struct Query {
    pairs: Vec<(String, String)>,
}

impl Query {
    /// Parse a raw query string (without the leading `?`).
    pub fn parse(raw: &str) -> Result<Self> {
        let mut pairs = Vec::new();
        for pair in raw.split('&') {
            if pair.is_empty() {
                continue;
            }
            let (key, value) = pair.split_once('=').unwrap_or((pair, ""));
            let key = percent_decode(key)?;
            let value = percent_decode(value)?;
            if key.is_empty() {
                return Err(Error::invalid_argument("query key must not be empty"));
            }
            pairs.push((key, value));
        }
        Ok(Self { pairs })
    }

    /// First value for `key`.
    pub fn get(&self, key: &str) -> Option<&str> {
        self.pairs
            .iter()
            .find(|(name, _)| name == key)
            .map(|(_, value)| value.as_str())
    }

    /// All values for `key`, preserving request order.
    pub fn get_all<'a>(&'a self, key: &'a str) -> impl Iterator<Item = &'a str> + 'a {
        self.pairs
            .iter()
            .filter(move |(name, _)| name == key)
            .map(|(_, value)| value.as_str())
    }

    /// Docker booleans: absent uses `default`; "", "0", "no", "false",
    /// "none" are false; everything else is true.
    pub fn bool_value(&self, key: &str, default: bool) -> bool {
        match self.get(key) {
            None => default,
            Some(value) => parse_docker_bool(value),
        }
    }

    /// Parse a non-negative integer, if present.
    pub fn u64_value(&self, key: &str) -> Result<Option<u64>> {
        match self.get(key) {
            None | Some("") => Ok(None),
            Some(value) => value.parse::<u64>().map(Some).map_err(|_| {
                Error::invalid_argument(format!("invalid value for {key}: {value:?}"))
            }),
        }
    }
}

fn parse_docker_bool(value: &str) -> bool {
    !matches!(
        value.trim().to_ascii_lowercase().as_str(),
        "" | "0" | "no" | "false" | "none"
    )
}

fn percent_decode(value: &str) -> Result<String> {
    let bytes = value.as_bytes();
    let mut out = Vec::with_capacity(bytes.len());
    let mut index = 0;
    while index < bytes.len() {
        match bytes[index] {
            b'+' => {
                out.push(b' ');
                index += 1;
            }
            b'%' => {
                if index + 2 >= bytes.len() {
                    return Err(Error::invalid_argument(
                        "invalid URL escape in query string",
                    ));
                }
                let high = hex(bytes[index + 1])
                    .ok_or_else(|| Error::invalid_argument("invalid URL escape in query string"))?;
                let low = hex(bytes[index + 2])
                    .ok_or_else(|| Error::invalid_argument("invalid URL escape in query string"))?;
                out.push(high << 4 | low);
                index += 3;
            }
            byte => {
                out.push(byte);
                index += 1;
            }
        }
    }
    String::from_utf8(out).map_err(|_| Error::invalid_argument("query string is not valid UTF-8"))
}

fn hex(byte: u8) -> Option<u8> {
    match byte {
        b'0'..=b'9' => Some(byte - b'0'),
        b'a'..=b'f' => Some(byte - b'a' + 10),
        b'A'..=b'F' => Some(byte - b'A' + 10),
        _ => None,
    }
}

/// Decoded filter object.
#[derive(Debug, Clone, Default)]
pub struct FilterSet {
    values: BTreeMap<String, Vec<String>>,
}

impl FilterSet {
    /// Parse the `filters` query parameter. Empty or absent means no filters.
    pub fn parse(raw: Option<&str>) -> Result<Self> {
        let Some(raw) = raw.filter(|raw| !raw.trim().is_empty()) else {
            return Ok(Self::default());
        };
        let value: Value = serde_json::from_str(raw)
            .map_err(|error| Error::invalid_argument(format!("invalid filter JSON: {error}")))?;
        let object = value
            .as_object()
            .ok_or_else(|| Error::invalid_argument("filters must be a JSON object"))?;
        let mut values = BTreeMap::new();
        for (key, value) in object {
            if key.is_empty() {
                return Err(Error::invalid_argument("filter key must not be empty"));
            }
            let entries = filter_entries(value)?;
            if !entries.is_empty() {
                values.insert(key.clone(), entries);
            }
        }
        Ok(Self { values })
    }

    /// Values for one filter key.
    pub fn get(&self, key: &str) -> &[String] {
        self.values.get(key).map(Vec::as_slice).unwrap_or(&[])
    }

    pub fn keys(&self) -> impl Iterator<Item = &str> {
        self.values.keys().map(String::as_str)
    }

    pub fn is_empty(&self) -> bool {
        self.values.is_empty()
    }

    /// Reject every key not in `supported`, and classify known-but-missing
    /// keys with `unsupported`.
    fn check_keys(&self, supported: &[&str], unsupported: &[&str]) -> Result<()> {
        for key in self.values.keys() {
            if supported.contains(&key.as_str()) {
                continue;
            }
            if unsupported.contains(&key.as_str()) {
                return Err(Error::unsupported(format!(
                    "filter {key:?} is not implemented by docker-rs"
                )));
            }
            return Err(Error::invalid_argument(format!("unknown filter {key:?}")));
        }
        Ok(())
    }
}

fn filter_entries(value: &Value) -> Result<Vec<String>> {
    match value {
        Value::Array(values) => values
            .iter()
            .map(|value| {
                value
                    .as_str()
                    .map(str::to_string)
                    .ok_or_else(|| Error::invalid_argument("filter values must be strings"))
            })
            .collect(),
        // Legacy map form: {"value": true}.
        Value::Object(values) => values
            .iter()
            .filter(|(_, enabled)| enabled.as_bool().unwrap_or(true))
            .map(|(value, _)| Ok(value.clone()))
            .collect(),
        Value::String(value) => Ok(vec![value.clone()]),
        Value::Null => Ok(Vec::new()),
        _ => Err(Error::invalid_argument(
            "filter values must be an array, object, or string",
        )),
    }
}

/// `GET /containers/json` options.
#[derive(Debug, Clone, Default)]
pub struct ContainerListOptions {
    pub all: bool,
    pub limit: Option<usize>,
    pub filters: ContainerFilters,
}

impl ContainerListOptions {
    pub fn parse(query: &Query) -> Result<Self> {
        if query.get("size").is_some() && query.bool_value("size", false) {
            return Err(Error::unsupported(
                "container size reporting is not implemented",
            ));
        }
        for key in ["since", "before", "ancestor", "expose", "health"] {
            if query.get(key).is_some() {
                return Err(Error::unsupported(format!(
                    "container list parameter {key:?} is not implemented"
                )));
            }
        }
        let limit = query
            .u64_value("limit")?
            .map(|value| value as usize)
            .filter(|value| *value > 0);
        let filters = ContainerFilters::parse(FilterSet::parse(query.get("filters"))?)?;
        Ok(Self {
            all: query.bool_value("all", false),
            limit,
            filters,
        })
    }
}

/// Container list filters with Docker's OR-within-key, AND-across-keys
/// semantics.
#[derive(Debug, Clone, Default)]
pub struct ContainerFilters {
    statuses: Vec<String>,
    ids: Vec<String>,
    names: Vec<String>,
    labels: Vec<(String, Option<String>)>,
}

impl ContainerFilters {
    fn parse(filters: FilterSet) -> Result<Self> {
        filters.check_keys(
            &["status", "id", "name", "label"],
            &[
                "ancestor",
                "before",
                "since",
                "volume",
                "network",
                "health",
                "isolation",
                "is-task",
                "publish",
                "expose",
                "dangling",
            ],
        )?;
        let mut container = Self::default();
        for status in filters.get("status") {
            let status = status.to_ascii_lowercase();
            // Validate against the state vocabulary so a typo cannot return
            // an accidentally empty list.
            if !matches!(
                status.as_str(),
                "created" | "running" | "paused" | "restarting" | "removing" | "exited" | "dead"
            ) {
                return Err(Error::invalid_argument(format!(
                    "invalid container status filter {status:?}"
                )));
            }
            container.statuses.push(status);
        }
        container.ids = filters.get("id").to_vec();
        container.names = filters.get("name").to_vec();
        for label in filters.get("label") {
            match label.split_once('=') {
                Some((key, value)) if !key.is_empty() => container
                    .labels
                    .push((key.to_string(), Some(value.to_string()))),
                Some(_) => return Err(Error::invalid_argument("label filter has an empty key")),
                None if !label.is_empty() => container.labels.push((label.clone(), None)),
                None => return Err(Error::invalid_argument("label filter is empty")),
            }
        }
        Ok(container)
    }

    pub fn matches(&self, view: &ContainerView) -> bool {
        if !self.statuses.is_empty()
            && !self
                .statuses
                .iter()
                .any(|status| status == view.state.status.as_api_status())
        {
            return false;
        }
        if !self.ids.is_empty()
            && !self
                .ids
                .iter()
                .any(|prefix| view.id.as_str().starts_with(prefix))
        {
            return false;
        }
        if !self.names.is_empty() {
            let Some(name) = &view.name else {
                return false;
            };
            let needle = self.names.iter().any(|needle| {
                name.to_ascii_lowercase()
                    .contains(&needle.to_ascii_lowercase())
            });
            if !needle {
                return false;
            }
        }
        for (key, value) in &self.labels {
            match view.config.labels.get(key) {
                None => return false,
                Some(found) => {
                    if value.as_ref().is_some_and(|value| value != found) {
                        return false;
                    }
                }
            }
        }
        true
    }
}

/// `GET /events` options.
#[derive(Debug, Clone, Default)]
pub struct EventOptions {
    pub since_nano: Option<i64>,
    pub until_nano: Option<i64>,
    pub filters: EventFilters,
}

impl EventOptions {
    pub fn parse(query: &Query) -> Result<Self> {
        let since = parse_timestamp(query.get("since"))?;
        let until = parse_timestamp(query.get("until"))?;
        if let (Some(since), Some(until)) = (since, until) {
            if until < since {
                return Err(Error::invalid_argument(format!(
                    "`since` time ({}) cannot be after `until` time ({})",
                    query.get("since").unwrap_or(""),
                    query.get("until").unwrap_or("")
                )));
            }
        }
        Ok(Self {
            since_nano: since,
            until_nano: until,
            filters: EventFilters::parse(FilterSet::parse(query.get("filters"))?)?,
        })
    }
}

/// Event filters evaluated before serialization.
#[derive(Debug, Clone, Default)]
pub struct EventFilters {
    types: Vec<String>,
    actions: Vec<String>,
    containers: Vec<String>,
    images: Vec<String>,
    labels: Vec<(String, Option<String>)>,
    scopes: Vec<String>,
}

impl EventFilters {
    fn parse(filters: FilterSet) -> Result<Self> {
        filters.check_keys(
            &["type", "event", "container", "image", "label", "scope"],
            &[
                "daemon", "network", "volume", "plugin", "builder", "node", "service", "secret",
                "config",
            ],
        )?;
        let mut event = Self::default();
        for r#type in filters.get("type") {
            event.types.push(r#type.to_ascii_lowercase());
        }
        for action in filters.get("event") {
            event.actions.push(action.to_ascii_lowercase());
        }
        event.containers = filters.get("container").to_vec();
        event.images = filters.get("image").to_vec();
        event.scopes = filters.get("scope").to_vec();
        for label in filters.get("label") {
            match label.split_once('=') {
                Some((key, value)) if !key.is_empty() => event
                    .labels
                    .push((key.to_string(), Some(value.to_string()))),
                Some(_) => return Err(Error::invalid_argument("label filter has an empty key")),
                None if !label.is_empty() => event.labels.push((label.clone(), None)),
                None => return Err(Error::invalid_argument("label filter is empty")),
            }
        }
        Ok(event)
    }

    pub fn matches(&self, event: &Event) -> bool {
        if !self.types.is_empty()
            && !self
                .types
                .iter()
                .any(|value| value == event_type_name(event.r#type))
        {
            return false;
        }
        if !self.actions.is_empty() {
            let action = event.action.to_string().to_ascii_lowercase();
            if !self.actions.iter().any(|value| value == &action) {
                return false;
            }
        }
        if !self.containers.is_empty()
            && !self.containers.iter().any(|needle| {
                event.actor.id.starts_with(needle)
                    || event
                        .actor
                        .attributes
                        .get("name")
                        .is_some_and(|name| name == needle.trim_start_matches('/'))
            })
        {
            return false;
        }
        if !self.images.is_empty()
            && !self.images.iter().any(|needle| {
                event
                    .actor
                    .attributes
                    .get("image")
                    .is_some_and(|image| image == needle)
                    || event.actor.id == *needle
            })
        {
            return false;
        }
        if !self.scopes.is_empty() && !self.scopes.iter().any(|scope| scope == event_scope(event)) {
            return false;
        }
        for (key, expected) in &self.labels {
            let Some(actual) = event.actor.attributes.get(key) else {
                return false;
            };
            if expected.as_ref().is_some_and(|expected| expected != actual) {
                return false;
            }
        }
        true
    }
}

fn event_type_name(r#type: EventType) -> &'static str {
    r#type.as_str()
}

fn event_scope(event: &Event) -> &'static str {
    match event.scope {
        docker_events::EventScope::Local => "local",
        docker_events::EventScope::Swarm => "swarm",
    }
}

/// Parse Docker event time: RFC3339Nano, Unix seconds, or Unix seconds with a
/// fractional part. Returns nanoseconds since the epoch.
pub fn parse_timestamp(raw: Option<&str>) -> Result<Option<i64>> {
    let Some(raw) = raw.filter(|raw| !raw.trim().is_empty()) else {
        return Ok(None);
    };
    if let Ok(seconds) = raw.parse::<i64>() {
        return seconds
            .checked_mul(1_000_000_000)
            .map(Some)
            .ok_or_else(|| Error::invalid_argument(format!("timestamp out of range: {raw}")));
    }
    if let Some((seconds, fraction)) = raw.split_once('.') {
        if let (Ok(seconds), true) = (
            seconds.parse::<i64>(),
            !fraction.is_empty() && fraction.bytes().all(|byte| byte.is_ascii_digit()),
        ) {
            let mut nanos = 0i64;
            for (index, byte) in fraction.bytes().take(9).enumerate() {
                nanos += i64::from(byte - b'0') * 10i64.pow(8 - index as u32);
            }
            let total = seconds
                .checked_mul(1_000_000_000)
                .and_then(|seconds| seconds.checked_add(nanos))
                .ok_or_else(|| Error::invalid_argument(format!("timestamp out of range: {raw}")))?;
            return Ok(Some(total));
        }
    }
    let parsed = DateTime::parse_from_rfc3339(raw)
        .map_err(|error| Error::invalid_argument(format!("invalid timestamp {raw:?}: {error}")))?;
    parsed
        .timestamp_nanos_opt()
        .map(Some)
        .ok_or_else(|| Error::invalid_argument(format!("timestamp out of range: {raw}")))
}

impl fmt::Display for FilterSet {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str("{")?;
        for (index, (key, values)) in self.values.iter().enumerate() {
            if index > 0 {
                f.write_str(", ")?;
            }
            write!(f, "{key}={values:?}")?;
        }
        f.write_str("}")
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_and_decodes_query_pairs() {
        let query = Query::parse("all=1&filters=%7B%22status%22%3A%5B%22running%22%5D%7D&name=a+b")
            .unwrap();
        assert!(query.bool_value("all", false));
        assert_eq!(query.get("name"), Some("a b"));
        assert_eq!(query.get("filters"), Some("{\"status\":[\"running\"]}"));
    }

    #[test]
    fn rejects_bad_percent_escape() {
        assert!(Query::parse("x=%zz").is_err());
    }

    #[test]
    fn accepts_current_and_legacy_filter_shapes() {
        let current = FilterSet::parse(Some(r#"{"status":["running","paused"]}"#)).unwrap();
        assert_eq!(current.get("status"), ["running", "paused"]);
        let legacy = FilterSet::parse(Some(r#"{"status":{"running":true}}"#)).unwrap();
        assert_eq!(legacy.get("status"), ["running"]);
    }

    #[test]
    fn rejects_unknown_and_unimplemented_filters() {
        let unknown = FilterSet::parse(Some(r#"{"widget":["x"]}"#)).unwrap();
        assert_eq!(
            unknown.check_keys(&["status"], &[]).unwrap_err().kind(),
            docker_errors::ErrorKind::InvalidArgument
        );
        let unimplemented = FilterSet::parse(Some(r#"{"ancestor":["alpine"]}"#)).unwrap();
        assert_eq!(
            unimplemented
                .check_keys(&["status"], &["ancestor"])
                .unwrap_err()
                .kind(),
            docker_errors::ErrorKind::Unsupported
        );
    }

    #[test]
    fn parses_timestamps() {
        assert_eq!(parse_timestamp(Some("0")).unwrap(), Some(0));
        assert_eq!(parse_timestamp(Some("1.5")).unwrap(), Some(1_500_000_000));
        assert_eq!(
            parse_timestamp(Some("2024-01-01T00:00:00Z")).unwrap(),
            Some(1_704_067_200_000_000_000)
        );
        assert!(parse_timestamp(Some("not-a-time")).is_err());
        assert_eq!(parse_timestamp(None).unwrap(), None);
    }

    #[test]
    fn event_filter_matches_actions_and_labels() {
        let filters = EventFilters::parse(
            FilterSet::parse(Some(
                r#"{"type":["container"],"event":["start"],"label":["role=web"]}"#,
            ))
            .unwrap(),
        )
        .unwrap();
        let mut event = docker_events::event_now(
            EventType::Container,
            docker_events::EventAction::Known(docker_events::KnownAction::Start),
            docker_events::attrs::container("abc", "alpine", "web"),
        );
        event.actor.attributes.insert("role".into(), "web".into());
        assert!(filters.matches(&event));
        event.action = docker_events::EventAction::Known(docker_events::KnownAction::Die);
        assert!(!filters.matches(&event));
    }
}
