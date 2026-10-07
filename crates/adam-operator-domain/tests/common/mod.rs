//! Loading the examples and changing them, for the tests of `adam-operator-domain`.
#![allow(dead_code)] // each test file uses part of it

use std::path::{Path, PathBuf};

use adam_operator_api::{AgentConfig, AgentService};
use adam_operator_domain::{ConfigIssue, ResolvedAgent};
use adam_operator_ports::OwnerHandle;
use serde::Deserialize;
use serde_json::Value;

pub fn repo_root() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR")).join("../..")
}

/// The documents of a YAML file, as JSON.
pub fn documents(path: &Path) -> Vec<Value> {
    let text = std::fs::read_to_string(path).unwrap_or_else(|e| panic!("{}: {e}", path.display()));
    serde_yaml::Deserializer::from_str(&text)
        .map(|d| Value::deserialize(d).unwrap_or_else(|e| panic!("{}: {e}", path.display())))
        .collect()
}

/// An example as the two JSON documents it holds: the service, then the config.
pub fn example(name: &str) -> (Value, Value) {
    let docs = documents(
        &repo_root()
            .join("deploy/operator/examples")
            .join(format!("{name}.yaml")),
    );
    let mut service = None;
    let mut config = None;
    for d in docs {
        match d["kind"].as_str() {
            Some("AgentService") => service = Some(d),
            Some("AgentConfig") => config = Some(d),
            other => panic!("unexpected kind {other:?}"),
        }
    }
    (
        service.expect("an AgentService"),
        config.expect("an AgentConfig"),
    )
}

pub fn typed(service: &Value, config: &Value) -> (AgentService, AgentConfig) {
    (
        serde_json::from_value(service.clone()).expect("an AgentService"),
        serde_json::from_value(config.clone()).expect("an AgentConfig"),
    )
}

/// Set the value at a JSON pointer, creating the objects on the way.
pub fn set(doc: &mut Value, pointer: &str, value: Value) {
    let mut at = doc;
    let parts: Vec<&str> = pointer.split('/').skip(1).collect();
    for (i, part) in parts.iter().enumerate() {
        let part = part.replace("~1", "/").replace("~0", "~");
        if i + 1 == parts.len() {
            match at {
                Value::Array(a) => {
                    let i = part.parse::<usize>().unwrap();
                    if i == a.len() {
                        a.push(value);
                    } else {
                        a[i] = value;
                    }
                }
                _ => {
                    at[part.as_str()] = value;
                }
            }
            return;
        }
        if at.get(part.as_str()).is_none() && !at.is_array() {
            at[part.as_str()] = Value::Object(serde_json::Map::new());
        }
        at = match at {
            Value::Array(a) => &mut a[part.parse::<usize>().unwrap()],
            _ => &mut at[part.as_str()],
        };
    }
}

/// Remove the value at a JSON pointer, if it is there.
pub fn remove(doc: &mut Value, pointer: &str) {
    let (parent, last) = pointer.rsplit_once('/').expect("a pointer");
    if let Some(Value::Object(m)) = doc.pointer_mut(parent) {
        m.remove(&last.replace("~1", "/").replace("~0", "~"));
    }
}

/// A resolved example with changes: `change` edits the two documents before they are typed.
pub fn resolved(
    name: &str,
    change: impl FnOnce(&mut Value, &mut Value),
) -> Result<ResolvedAgent, Vec<ConfigIssue>> {
    let (mut service, mut config) = example(name);
    change(&mut service, &mut config);
    let (service, config) = typed(&service, &config);
    adam_operator_domain::resolve(&service, &config, OwnerHandle::new("owner-token"))
}

pub fn must_resolve(name: &str, change: impl FnOnce(&mut Value, &mut Value)) -> ResolvedAgent {
    resolved(name, change).unwrap_or_else(|issues| {
        panic!(
            "expected the example to resolve, got:\n{}",
            issues
                .iter()
                .map(ToString::to_string)
                .collect::<Vec<_>>()
                .join("\n")
        )
    })
}

/// The issues of an example with changes, which must be refused.
pub fn issues(name: &str, change: impl FnOnce(&mut Value, &mut Value)) -> Vec<ConfigIssue> {
    match resolved(name, change) {
        Ok(_) => panic!("expected the example to be refused"),
        Err(issues) => issues,
    }
}

/// Asserts that an issue is about `field_part` and says `message_part`.
pub fn assert_issue(issues: &[ConfigIssue], field_part: &str, message_part: &str) {
    assert!(
        issues
            .iter()
            .any(|i| i.field.contains(field_part) && i.message.contains(message_part)),
        "no issue about {field_part:?} saying {message_part:?} in:\n{}",
        issues
            .iter()
            .map(ToString::to_string)
            .collect::<Vec<_>>()
            .join("\n")
    );
}
