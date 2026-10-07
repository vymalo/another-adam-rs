//! Loading the examples and changing them, for the tests of `adam-operator-runtime-kubernetes`.
#![allow(dead_code)] // each test file uses part of it

use std::path::{Path, PathBuf};

use adam_operator_api::{AgentConfig, AgentService};
use adam_operator_domain::ResolvedAgent;
use adam_operator_runtime_kubernetes::owner_handle;
use serde::Deserialize;
use serde_json::Value;

pub fn repo_root() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR")).join("../..")
}

/// An example as the two JSON documents it holds: the service, then the config.
pub fn example(name: &str) -> (Value, Value) {
    let path = repo_root()
        .join("deploy/operator/examples")
        .join(format!("{name}.yaml"));
    let text = std::fs::read_to_string(&path).unwrap_or_else(|e| panic!("{}: {e}", path.display()));
    let mut service = None;
    let mut config = None;
    for d in serde_yaml::Deserializer::from_str(&text) {
        let d = Value::deserialize(d).unwrap();
        match d["kind"].as_str() {
            Some("AgentService") => service = Some(d),
            Some("AgentConfig") => config = Some(d),
            other => panic!("unexpected kind {other:?}"),
        }
    }
    (service.unwrap(), config.unwrap())
}

/// Set the value at a JSON pointer (`~1` is `/`), creating the objects on the way.
pub fn set(doc: &mut Value, pointer: &str, value: Value) {
    let mut at = doc;
    let parts: Vec<String> = pointer
        .split('/')
        .skip(1)
        .map(|p| p.replace("~1", "/").replace("~0", "~"))
        .collect();
    for (i, part) in parts.iter().enumerate() {
        let last = i + 1 == parts.len();
        if let Value::Array(items) = at {
            let index: usize = part.parse().unwrap();
            if last {
                items[index] = value;
                return;
            }
            at = &mut items[index];
            continue;
        }
        if last {
            at[part.as_str()] = value;
            return;
        }
        if at.get(part.as_str()).is_none() {
            at[part.as_str()] = Value::Object(serde_json::Map::new());
        }
        at = &mut at[part.as_str()];
    }
}

/// An example with changes, resolved with an owner that has a uid.
pub fn resolved(name: &str, change: impl FnOnce(&mut Value, &mut Value)) -> ResolvedAgent {
    let (mut service, mut config) = example(name);
    change(&mut service, &mut config);
    let service: AgentService = serde_json::from_value(service).unwrap();
    let config: AgentConfig = serde_json::from_value(config).unwrap();
    let owner = owner_handle(
        "agents.vymalo.com/v1alpha1",
        "AgentService",
        service.metadata.name.clone().unwrap(),
        "0b1f3c5e-1111-4222-8333-444455556666",
    );
    adam_operator_domain::resolve(&service, &config, owner).unwrap_or_else(|issues| {
        panic!(
            "the example does not resolve: {}",
            issues
                .iter()
                .map(ToString::to_string)
                .collect::<Vec<_>>()
                .join("; ")
        )
    })
}
