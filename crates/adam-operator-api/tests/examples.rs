//! The examples: the valid ones are accepted, each invalid one is refused for the reason it
//! states, and every CEL rule has an invalid example.
//!
//! The rules are evaluated with `kube-cel`, a client-side CEL implementation. It is a proxy for an
//! API server, not one: the `kind` job of `.github/workflows/operator.yml` is the real check.

#![allow(clippy::expect_used, clippy::unwrap_used)] // tests may

use std::fs;
use std::path::{Path, PathBuf};

use adam_operator_api::{AgentConfig, AgentService, crds};
use kube::core::cel::Validator;
use serde::Deserialize;
use serde_json::Value;

fn examples_dir() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR")).join("../../deploy/operator/examples")
}

fn yaml_files(dir: &Path) -> Vec<PathBuf> {
    let mut files: Vec<_> = fs::read_dir(dir)
        .unwrap_or_else(|e| panic!("reading {}: {e}", dir.display()))
        .filter_map(Result::ok)
        .map(|e| e.path())
        .filter(|p| p.extension().is_some_and(|x| x == "yaml"))
        // `netcup.values.yaml` is a values file of the operator's chart, not a custom resource.
        .filter(|p| !p.to_string_lossy().ends_with(".values.yaml"))
        .collect();
    files.sort();
    files
}

/// The documents of a (multi-document) YAML file, as JSON.
fn documents(path: &Path) -> Vec<Value> {
    let text =
        fs::read_to_string(path).unwrap_or_else(|e| panic!("reading {}: {e}", path.display()));
    serde_yaml::Deserializer::from_str(&text)
        .map(|d| Value::deserialize(d).unwrap_or_else(|e| panic!("{}: {e}", path.display())))
        .collect()
}

/// Parse a document into its kind, and serialise it back: defaults are filled in as an API server
/// would, a field the types do not know is dropped.
fn through_the_types(doc: &Value) -> Value {
    match doc["kind"].as_str() {
        Some("AgentService") => {
            let typed: AgentService = serde_json::from_value(doc.clone()).expect("an AgentService");
            serde_json::to_value(&typed).expect("serialises")
        }
        Some("AgentConfig") => {
            let typed: AgentConfig = serde_json::from_value(doc.clone()).expect("an AgentConfig");
            serde_json::to_value(&typed).expect("serialises")
        }
        other => panic!("unexpected kind {other:?}"),
    }
}

fn schema_for(kind: &str) -> Value {
    let crd = crds()
        .into_iter()
        .find(|c| c.spec.names.kind == kind)
        .unwrap_or_else(|| panic!("no CRD of kind {kind}"));
    let schema = crd.spec.versions[0]
        .schema
        .as_ref()
        .and_then(|s| s.open_api_v3_schema.as_ref());
    serde_json::to_value(schema.expect("schema")).expect("serialises")
}

/// Every key and value of `a` is in `b`: nothing the author wrote was lost by the types.
fn subset(a: &Value, b: &Value, path: &str) -> Result<(), String> {
    match (a, b) {
        (Value::Object(x), Value::Object(y)) => {
            for (k, v) in x {
                let Some(w) = y.get(k) else {
                    return Err(format!("{path}.{k} is not in the typed form"));
                };
                subset(v, w, &format!("{path}.{k}"))?;
            }
            Ok(())
        }
        (Value::Array(x), Value::Array(y)) if x.len() == y.len() => x
            .iter()
            .zip(y)
            .enumerate()
            .try_for_each(|(i, (v, w))| subset(v, w, &format!("{path}[{i}]"))),
        _ if a == b => Ok(()),
        _ => Err(format!("{path}: {a} became {b}")),
    }
}

fn validate(doc: &Value) -> Result<(), Vec<String>> {
    let typed = through_the_types(doc);
    let kind = doc["kind"].as_str().expect("kind");
    Validator::new()
        .validate(&schema_for(kind), &typed, None)
        .map_err(|errors| errors.iter().map(|e| e.message.clone()).collect())
}

#[test]
fn examples_round_trip_through_the_types_and_lose_nothing() {
    let files = yaml_files(&examples_dir());
    assert!(
        files.iter().any(|f| f.ends_with("coder.yaml"))
            && files.iter().any(|f| f.ends_with("chat.yaml"))
    );
    for file in files {
        for doc in documents(&file) {
            let typed = through_the_types(&doc);
            if let Err(why) = subset(&doc, &typed, "") {
                panic!("{}: {why}", file.display());
            }
            // and the typed form is a fixed point
            assert_eq!(through_the_types(&typed), typed, "{}", file.display());
        }
    }
}

#[test]
fn each_example_is_an_agent_service_and_its_agent_config() {
    for name in ["coder.yaml", "chat.yaml"] {
        let docs = documents(&examples_dir().join(name));
        let kinds: Vec<_> = docs
            .iter()
            .map(|d| d["kind"].as_str().unwrap_or_default())
            .collect();
        assert_eq!(kinds, ["AgentService", "AgentConfig"], "{name}");
        // the service names the config
        assert_eq!(
            docs[0]["spec"]["configRef"]["name"], docs[1]["metadata"]["name"],
            "{name}"
        );
    }
}

#[test]
fn valid_examples_pass_every_cel_rule() {
    for file in yaml_files(&examples_dir()) {
        for doc in documents(&file) {
            if let Err(errors) = validate(&doc) {
                panic!("{} was refused: {errors:#?}", file.display());
            }
        }
    }
}

fn expectation(path: &Path) -> String {
    let text = fs::read_to_string(path).expect("readable");
    text.lines()
        .find_map(|l| l.strip_prefix("# expect: "))
        .unwrap_or_else(|| panic!("{} has no `# expect:` line", path.display()))
        .trim()
        .to_owned()
}

#[test]
fn each_invalid_example_is_refused_for_the_reason_it_states() {
    let files = yaml_files(&examples_dir().join("invalid"));
    assert!(!files.is_empty());
    for file in files {
        let docs = documents(&file);
        assert_eq!(
            docs.len(),
            1,
            "{}: one document, so the whole file is refused",
            file.display()
        );
        let expected = expectation(&file);
        match validate(&docs[0]) {
            Ok(()) => panic!("{} was accepted", file.display()),
            Err(errors) => assert!(
                errors.contains(&expected),
                "{}: expected {expected:?}, got {errors:#?}",
                file.display()
            ),
        }
    }
}

#[test]
fn every_cel_rule_has_an_invalid_example() {
    let expected: Vec<String> = yaml_files(&examples_dir().join("invalid"))
        .iter()
        .map(|f| expectation(f))
        .collect();
    let mut messages = Vec::new();
    for crd in crds() {
        let v = serde_json::to_value(&crd).expect("serialises");
        collect_messages(&v, &mut messages);
    }
    assert!(!messages.is_empty());
    for m in messages {
        assert!(
            expected.contains(&m),
            "no examples/invalid file expects {m:?}"
        );
    }
}

fn collect_messages(v: &Value, out: &mut Vec<String>) {
    match v {
        Value::Object(map) => {
            if let Some(Value::Array(rules)) = map.get("x-kubernetes-validations") {
                out.extend(
                    rules
                        .iter()
                        .filter_map(|r| r["message"].as_str().map(str::to_owned)),
                );
            }
            map.values().for_each(|c| collect_messages(c, out));
        }
        Value::Array(items) => items.iter().for_each(|c| collect_messages(c, out)),
        _ => {}
    }
}
