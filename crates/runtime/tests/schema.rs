//! Validate generated OCI documents against the pinned runtime-spec 1.3.0
//! JSON schemas.
//!
//! The schemas are read from `upstream/runtime-spec/schema/` rather than
//! re-typed here. `upstream/` is intentionally not tracked by git; a checkout
//! produced by `tools/fetch-upstream.ps1` (or the documented equivalent) is
//! required for this test.
//!
//! The validator below implements the draft-04 keyword subset used by these
//! files: `$ref`, `type`, `required`, `properties`, `patternProperties`,
//! `additionalProperties`, `items`, `enum`, `pattern`, `minimum`, `maximum`,
//! `minItems`, `allOf`, and `anyOf`. It is test infrastructure, not a general
//! JSON Schema implementation.

use std::collections::BTreeMap;
use std::fs;
use std::path::{Path, PathBuf};

use docker_common::{ContainerId, ImageRef};
use docker_container_model::config::{ContainerConfig, HostConfig, NetworkMode, Signal};
use docker_runtime::spec::{ContainerState, State, OCI_VERSION};
use docker_runtime::{spec_from_container, Spec};
use regex::Regex;
use serde_json::Value;
use tempfile::TempDir;

fn schema_dir() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR")).join("../../upstream/runtime-spec/schema")
}

struct SchemaSet {
    docs: BTreeMap<String, Value>,
}

impl SchemaSet {
    fn load() -> Self {
        let dir = schema_dir();
        if !dir.is_dir() {
            panic!(
                "pinned runtime-spec schemas are missing at `{}`; populate `upstream/` first",
                dir.display()
            );
        }
        let mut docs = BTreeMap::new();
        for entry in fs::read_dir(&dir).expect("read runtime-spec schema directory") {
            let entry = entry.expect("read schema directory entry");
            let path = entry.path();
            if path.extension().and_then(|value| value.to_str()) != Some("json") {
                continue;
            }
            let name = path
                .file_name()
                .and_then(|value| value.to_str())
                .expect("UTF-8 schema filename")
                .to_string();
            let bytes = fs::read(&path).expect("read schema file");
            let value: Value = serde_json::from_slice(&bytes).expect("parse schema JSON");
            docs.insert(name, value);
        }
        Self { docs }
    }

    fn validate_doc(&self, doc: &str, instance: &Value) -> Vec<String> {
        let schema = self
            .docs
            .get(doc)
            .unwrap_or_else(|| panic!("schema `{doc}` is not present"));
        let mut errors = Vec::new();
        self.validate(doc, schema, instance, "$", &mut errors, 0);
        errors
    }

    fn validate(
        &self,
        doc: &str,
        schema: &Value,
        instance: &Value,
        path: &str,
        errors: &mut Vec<String>,
        depth: usize,
    ) {
        if depth > 96 {
            errors.push(format!("{path}: schema reference depth exceeded"));
            return;
        }

        if let Some(reference) = schema.get("$ref").and_then(Value::as_str) {
            match self.resolve(doc, reference) {
                Ok((next_doc, next_schema)) => {
                    self.validate(&next_doc, &next_schema, instance, path, errors, depth + 1);
                }
                Err(error) => errors.push(format!("{path}: {error}")),
            }
            return;
        }

        if let Some(expected) = schema.get("type") {
            let matches = match expected {
                Value::String(kind) => type_matches(kind, instance),
                Value::Array(kinds) => kinds
                    .iter()
                    .filter_map(Value::as_str)
                    .any(|kind| type_matches(kind, instance)),
                _ => true,
            };
            if !matches {
                errors.push(format!(
                    "{path}: expected type {}, found {}",
                    expected,
                    json_type(instance)
                ));
                return;
            }
        }

        if let Some(values) = schema.get("enum").and_then(Value::as_array) {
            if !values.iter().any(|value| value == instance) {
                errors.push(format!(
                    "{path}: {instance} is not one of the allowed values"
                ));
            }
        }

        if let Some(pattern) = schema.get("pattern").and_then(Value::as_str) {
            if let Some(text) = instance.as_str() {
                let regex = Regex::new(pattern).expect("valid schema pattern");
                if !regex.is_match(text) {
                    errors.push(format!(
                        "{path}: `{text}` does not match pattern `{pattern}`"
                    ));
                }
            }
        }

        if let Some(minimum) = schema.get("minimum").and_then(Value::as_f64) {
            if let Some(number) = instance.as_f64() {
                if number < minimum {
                    errors.push(format!("{path}: {number} is below minimum {minimum}"));
                }
            }
        }
        if let Some(maximum) = schema.get("maximum").and_then(Value::as_f64) {
            if let Some(number) = instance.as_f64() {
                if number > maximum {
                    errors.push(format!("{path}: {number} exceeds maximum {maximum}"));
                }
            }
        }

        if let Some(all_of) = schema.get("allOf").and_then(Value::as_array) {
            for (index, subschema) in all_of.iter().enumerate() {
                self.validate(
                    doc,
                    subschema,
                    instance,
                    &format!("{path}/allOf/{index}"),
                    errors,
                    depth + 1,
                );
            }
        }

        if let Some(any_of) = schema.get("anyOf").and_then(Value::as_array) {
            let mut matched = false;
            for subschema in any_of {
                let mut candidate = Vec::new();
                self.validate(doc, subschema, instance, path, &mut candidate, depth + 1);
                if candidate.is_empty() {
                    matched = true;
                    break;
                }
            }
            if !matched {
                errors.push(format!("{path}: value does not match any anyOf branch"));
            }
        }

        match instance {
            Value::Object(object) => {
                let properties = schema.get("properties").and_then(Value::as_object);
                let pattern_properties = schema.get("patternProperties").and_then(Value::as_object);

                if let Some(required) = schema.get("required").and_then(Value::as_array) {
                    for key in required.iter().filter_map(Value::as_str) {
                        if !object.contains_key(key) {
                            errors.push(format!("{path}: missing required property `{key}`"));
                        }
                    }
                }

                if let Some(properties) = properties {
                    for (key, value) in object {
                        if let Some(subschema) = properties.get(key) {
                            self.validate(
                                doc,
                                subschema,
                                value,
                                &format!("{path}/{key}"),
                                errors,
                                depth + 1,
                            );
                        }
                    }
                }

                let mut matched_keys = object.keys().cloned().collect::<Vec<_>>();
                if let Some(pattern_properties) = pattern_properties {
                    for (pattern, subschema) in pattern_properties {
                        let regex = Regex::new(pattern).expect("valid patternProperties regex");
                        for (key, value) in object {
                            if regex.is_match(key) {
                                self.validate(
                                    doc,
                                    subschema,
                                    value,
                                    &format!("{path}/{key}"),
                                    errors,
                                    depth + 1,
                                );
                            }
                        }
                    }
                }
                matched_keys.retain(|key| {
                    properties.map(|map| map.contains_key(key)).unwrap_or(false)
                        || pattern_properties
                            .map(|map| {
                                map.keys().any(|pattern| {
                                    Regex::new(pattern)
                                        .expect("valid patternProperties regex")
                                        .is_match(key)
                                })
                            })
                            .unwrap_or(false)
                });

                if let Some(additional) = schema.get("additionalProperties") {
                    for key in object.keys() {
                        if matched_keys.iter().any(|matched| matched == key) {
                            continue;
                        }
                        match additional {
                            Value::Bool(false) => {
                                errors.push(format!("{path}: unexpected property `{key}`"));
                            }
                            Value::Object(_) => self.validate(
                                doc,
                                additional,
                                &object[key],
                                &format!("{path}/{key}"),
                                errors,
                                depth + 1,
                            ),
                            _ => {}
                        }
                    }
                }
            }
            Value::Array(items) => {
                if let Some(minimum) = schema.get("minItems").and_then(Value::as_u64) {
                    if (items.len() as u64) < minimum {
                        errors.push(format!(
                            "{path}: array has {} items, minimum is {minimum}",
                            items.len()
                        ));
                    }
                }
                if let Some(item_schema) = schema.get("items") {
                    if item_schema.is_object() {
                        for (index, item) in items.iter().enumerate() {
                            self.validate(
                                doc,
                                item_schema,
                                item,
                                &format!("{path}/{index}"),
                                errors,
                                depth + 1,
                            );
                        }
                    }
                }
            }
            _ => {}
        }
    }

    fn resolve(&self, current_doc: &str, reference: &str) -> Result<(String, Value), String> {
        let (file, pointer) = match reference.split_once('#') {
            Some((file, pointer)) => (file, pointer),
            None => (reference, ""),
        };
        let doc_name = if file.is_empty() {
            current_doc.to_string()
        } else {
            file.to_string()
        };
        let mut node = self
            .docs
            .get(&doc_name)
            .ok_or_else(|| format!("referenced schema `{doc_name}` was not loaded"))?;
        for token in pointer.split('/').skip(1) {
            let token = token.replace("~1", "/").replace("~0", "~");
            node = node
                .get(&token)
                .ok_or_else(|| format!("reference `{reference}` does not resolve"))?;
        }
        Ok((doc_name, node.clone()))
    }
}

fn type_matches(kind: &str, value: &Value) -> bool {
    match kind {
        "object" => value.is_object(),
        "array" => value.is_array(),
        "string" => value.is_string(),
        "boolean" => value.is_boolean(),
        "null" => value.is_null(),
        "number" => value.is_number(),
        "integer" => {
            value.as_i64().is_some()
                || value.as_u64().is_some()
                || value
                    .as_f64()
                    .map(|number| number.fract() == 0.0)
                    .unwrap_or(false)
        }
        _ => true,
    }
}

fn json_type(value: &Value) -> &'static str {
    match value {
        Value::Null => "null",
        Value::Bool(_) => "boolean",
        Value::Number(_) => "number",
        Value::String(_) => "string",
        Value::Array(_) => "array",
        Value::Object(_) => "object",
    }
}

fn container_config() -> ContainerConfig {
    ContainerConfig {
        image: ImageRef::parse("alpine:3.19").unwrap(),
        entrypoint: vec!["/bin/sh".into()],
        cmd: vec!["-c".into(), "echo schema".into()],
        env: vec!["FOO=bar".into()],
        labels: BTreeMap::new(),
        working_dir: Some("/work".into()),
        user: Some("1000:1001".into()),
        hostname: Some("schema-test".into()),
        tty: false,
        open_stdin: false,
        stop_signal: Signal::Sigterm,
        stop_timeout_secs: 10,
        exposed_ports: Vec::new(),
    }
}

fn generated_spec(rootfs: &Path) -> Spec {
    let id = ContainerId::from_generated("b".repeat(64)).unwrap();
    spec_from_container(
        &id,
        &container_config(),
        &HostConfig {
            network_mode: NetworkMode::None,
            ..HostConfig::default()
        },
        rootfs,
    )
    .unwrap()
}

#[test]
fn generated_bundle_matches_pinned_config_schema() {
    let rootfs = TempDir::new().unwrap();
    let spec = generated_spec(rootfs.path());
    let instance = serde_json::to_value(&spec).unwrap();
    let schemas = SchemaSet::load();
    let errors = schemas.validate_doc("config-schema.json", &instance);
    assert!(errors.is_empty(), "schema violations:\n{errors:#?}");
}

#[test]
fn generated_state_matches_pinned_state_schema() {
    let state = State {
        version: OCI_VERSION.to_string(),
        id: "abc123".into(),
        status: ContainerState::Running,
        pid: 42,
        bundle: "/var/lib/docker-rs/bundles/abc123".into(),
        annotations: BTreeMap::new(),
    };
    let instance = serde_json::to_value(&state).unwrap();
    let schemas = SchemaSet::load();
    let errors = schemas.validate_doc("state-schema.json", &instance);
    assert!(errors.is_empty(), "schema violations:\n{errors:#?}");
}

#[test]
fn schema_validator_rejects_missing_required_field() {
    let schemas = SchemaSet::load();
    let instance = serde_json::json!({ "root": { "path": "/tmp/rootfs" } });
    let errors = schemas.validate_doc("config-schema.json", &instance);
    assert!(errors.iter().any(|error| error.contains("ociVersion")));
}
