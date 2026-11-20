//! The vendored A2UI v1.0 schemas as the oracle of what adam writes (ADR 0056, decision 8).
//!
//! `tests/a2ui-v1_0/` holds byte copies of six schemas of `a2ui-project/a2ui` at the commit its `UPSTREAM`
//! names; the first test holds each file to its SHA-256. They are read here and never at run time.
//!
//! What is checked: the vendored bytes; catalog 9 (`tests/fixtures/catalog-v9.json`) against
//! `catalog_definition.json`, its digest and its equivalence to catalog 8; the digest vector of
//! `ui-catalog/v2`; and, in the next tests, every golden and fresh tool output against
//! `agent_to_renderer.json` with catalog 9 standing for `catalog.json#/$defs/anyComponent`, the document
//! upstream does not publish.
#![allow(clippy::unwrap_used, clippy::expect_used, missing_docs)]

use std::collections::BTreeMap;
use std::path::PathBuf;

use adam_ui::catalog_digest;
use jsonschema::{Registry, Resource};
use serde_json::{Value, json};
use sha2::{Digest, Sha256};

const V8: &str = include_str!("fixtures/catalog-v8.json");
const V9: &str = include_str!("fixtures/catalog-v9.json");
const V9_LOCK: &str = include_str!("fixtures/catalog-v9.lock.json");
const FIXTURES_UPSTREAM: &str = include_str!("fixtures/UPSTREAM");

const SCHEMAS: [&str; 6] = [
    "agent_to_renderer.json",
    "common_types.json",
    "catalog_definition.json",
    "renderer_capabilities.json",
    "renderer_to_agent.json",
    "agent_capabilities.json",
];

/// The URI `agent_to_renderer.json` reaches a catalog through: `catalog.json#/$defs/anyComponent`.
const CATALOG_SCHEMA_URI: &str = "https://a2ui.org/specification/v1_0/catalog.json";

fn dir() -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("tests/a2ui-v1_0")
}

fn schema(name: &str) -> Value {
    let path = dir().join(name);
    let text = std::fs::read_to_string(&path).unwrap_or_else(|e| panic!("{}: {e}", path.display()));
    serde_json::from_str(&text).unwrap_or_else(|e| panic!("{}: {e}", path.display()))
}

/// `catalog.json`, the document upstream does not publish, as a renderer resolves it for the catalog in use:
/// `anyComponent` is the union of the catalog's components and `anyFunction` matches no call (the screen's
/// catalog declares no function, ADR 0056).
fn catalog_document(catalog: &Value) -> Value {
    let one_of: Vec<Value> = catalog["components"]
        .as_object()
        .expect("components")
        .values()
        .cloned()
        .collect();
    json!({
        "$schema": "https://json-schema.org/draft/2020-12/schema",
        "$id": CATALOG_SCHEMA_URI,
        "$defs": {
            "anyComponent": {"oneOf": one_of},
            "anyFunction": {"not": {}},
        },
    })
}

/// A validator for `document`, offline, with the vendored documents in a registry so that a `$ref` to a
/// sibling file resolves to the vendored copy; `catalog` is the catalog `catalog.json#/$defs/anyComponent`
/// stands for.
fn validator_for(document: &Value, catalog: &Value) -> jsonschema::Validator {
    let mut resources: BTreeMap<String, Value> = SCHEMAS
        .into_iter()
        .map(|name| {
            let document = schema(name);
            let id = document["$id"]
                .as_str()
                .expect("every A2UI schema names itself")
                .to_owned();
            (id, document)
        })
        .collect();
    resources.insert(CATALOG_SCHEMA_URI.to_owned(), catalog_document(catalog));
    let mut registry = Registry::new();
    for (uri, document) in resources {
        registry = registry
            .add(uri.as_str(), Resource::from_contents(document))
            .unwrap_or_else(|e| panic!("{uri}: {e}"));
    }
    let registry = registry
        .prepare()
        .unwrap_or_else(|e| panic!("the registry: {e}"));
    jsonschema::options()
        .with_registry(&registry)
        .offline()
        .build(document)
        .unwrap_or_else(|e| panic!("the schema compiles: {e}"))
}

#[allow(dead_code)]
fn errors(validator: &jsonschema::Validator, instance: &Value) -> Vec<String> {
    validator
        .iter_errors(instance)
        .map(|e| format!("{}: {e}", e.instance_path()))
        .collect()
}

/// `tests/a2ui-v1_0/UPSTREAM` names a commit and each file's SHA-256; the bytes here are those bytes.
#[test]
fn the_vendored_schemas_are_the_ones_the_record_pins() {
    let text = std::fs::read_to_string(dir().join("UPSTREAM")).expect("UPSTREAM");
    let mut pinned = BTreeMap::new();
    let mut commit = None;
    let mut repo = None;
    for line in text.lines().map(str::trim) {
        if let Some(value) = line.strip_prefix("commit=") {
            commit = Some(value.to_owned());
        } else if let Some(value) = line.strip_prefix("repo=") {
            repo = Some(value.to_owned());
        } else if !line.starts_with('#') {
            let mut parts = line.split_whitespace();
            if let (Some(name), Some(hash), Some(path)) = (parts.next(), parts.next(), parts.next())
            {
                assert_eq!(path, format!("specification/v1_0/json/{name}"));
                pinned.insert(name.to_owned(), hash.to_owned());
            }
        }
    }
    assert_eq!(repo.as_deref(), Some("a2ui-project/a2ui"));
    assert_eq!(
        commit.as_deref(),
        Some("b4f91c26fcf62abc0720c21a65840220941d0a3f"),
        "ADR 0056, decision 8 pins this commit"
    );
    let mut names: Vec<&str> = pinned.keys().map(String::as_str).collect();
    names.sort_unstable();
    let mut want = SCHEMAS.to_vec();
    want.sort_unstable();
    assert_eq!(names, want, "exactly the six files are vendored");
    for (name, want) in pinned {
        let bytes = std::fs::read(dir().join(&name)).unwrap_or_else(|e| panic!("{name}: {e}"));
        assert_eq!(
            format!("{:x}", Sha256::digest(&bytes)),
            want,
            "{name} is not the file UPSTREAM pins"
        );
        let document: Value = serde_json::from_slice(&bytes).unwrap();
        assert_eq!(
            document["$schema"],
            json!("https://json-schema.org/draft/2020-12/schema"),
            "{name}"
        );
    }
}

fn catalog_nine() -> Value {
    serde_json::from_str(V9).unwrap()
}

/// Catalog 9 is the published file: its bytes, its digest and its lock are the ones the contract
/// (`ui-catalog-v2.md`, sections 1 and 2) pins; it is catalog 8 with `protocolVersion`; and it validates
/// against `catalog_definition.json`.
#[test]
fn catalog_nine_is_the_published_catalog_and_validates_against_catalog_definition() {
    assert_eq!(V9.len(), 35_143, "the bytes of the page's block");
    assert_eq!(
        format!("{:x}", Sha256::digest(V9.as_bytes())),
        "f6ee69d833ab968ff25ece454f5ad163858bbfa0f8f0d6468f7cb2a0011ebbed"
    );
    let catalog = catalog_nine();
    let digest = catalog_digest(&catalog).unwrap();
    assert_eq!(
        digest,
        "sha256:5f174ee91c50759d59a0d7e91e3a1dc2e7c110b82ee91d858f4a23a944ed72b2"
    );
    let lock: Value = serde_json::from_str(V9_LOCK).unwrap();
    assert_eq!(lock, json!({"version": 9, "digest": digest}));
    let line = FIXTURES_UPSTREAM
        .lines()
        .find(|l| l.starts_with("catalog-v9.json"))
        .expect("fixtures/UPSTREAM names catalog 9");
    assert!(line.contains(&digest), "{line}");

    let validator = validator_for(&schema("catalog_definition.json"), &catalog);
    let found = errors(&validator, &catalog);
    assert!(found.is_empty(), "{found:#?}");
    assert_eq!(catalog["protocolVersion"], json!("1.0"));
    // the validator is not vacuous: a catalog with a member the definition does not
    // have is refused
    let mut broken = catalog.clone();
    broken["surprise"] = json!(true);
    assert!(!errors(&validator, &broken).is_empty());

    // Taking `protocolVersion` out gives back catalog 8, member for member and digest for digest.
    let mut without = catalog;
    without.as_object_mut().unwrap().remove("protocolVersion");
    assert_eq!(without, serde_json::from_str::<Value>(V8).unwrap());
    assert_eq!(
        catalog_digest(&without).unwrap(),
        "sha256:22c333102bed676791dc9d47f21fc0a39be6e5062ef2c52f5a599ecca1b8341d"
    );
}

/// The digest vector of `ui-catalog/v2` (`ui-catalog-v2.md`, section 2): v1's vector with
/// `protocolVersion`. Every implementation pins it.
#[test]
fn the_digest_vector_of_ui_catalog_v2_is_pinned() {
    let input: Value = serde_json::from_str(
        r#"{"catalogId":"https://agents.vymalo.com/a2ui/catalogs/test","protocolVersion":"1.0","components":{"Note":{"type":"object","properties":{"component":{"const":"Note"},"text":{"type":"string","maxLength":10}},"required":["component","text"]}}}"#,
    )
    .unwrap();
    assert_eq!(
        catalog_digest(&input).unwrap(),
        "sha256:9fdab0c5c2094871cfb01d15046f206f3460341960ddcee4767e2bd33ab8e8f8"
    );
}
