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
use std::sync::Arc;

use a2a::{Message, Part, Role};
use adam_a2a::{A2UI_MEDIA_TYPE, UI_CATALOG_EXTENSION};
use adam_a2a_runtime::{CONTEXT_A2UI_RENDERER, CONTEXT_UI_CATALOG, CONTEXT_UI_REF, vymalo_inbound};
use adam_llm_agent::{ToolCtx, ToolError};
use adam_runtime::NoopSink;
use adam_ui::{Ui, canonical_json, catalog_digest};
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
    // The canonical form is 19,413 UTF-8 bytes (S1 corrected the page's count): 19,411 code points,
    // for it holds one "…".
    let canonical = canonical_json(&catalog).unwrap();
    assert_eq!(canonical.len(), 19_413, "UTF-8 bytes");
    assert_eq!(canonical.chars().count(), 19_411, "code points");
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

// ---- what adam writes validates against agent_to_renderer.json

/// The validator of the messages the agent sends, with catalog 9 standing for the catalog in use.
fn agent_to_renderer() -> jsonschema::Validator {
    validator_for(&schema("agent_to_renderer.json"), &catalog_nine())
}

/// Every operation of `surface` (the array of messages a tool wrote) must be valid, say "1.0" and be
/// a `createSurface` or an `updateComponents` (the only two messages adam writes).
fn assert_valid_surface(validator: &jsonschema::Validator, what: &str, surface: &Value) {
    let operations = surface
        .as_array()
        .unwrap_or_else(|| panic!("{what}: not an array"));
    assert!(!operations.is_empty(), "{what}");
    for operation in operations {
        let found = errors(validator, operation);
        assert!(found.is_empty(), "{what}: {operation}\n{found:#?}");
        assert_eq!(operation["version"], json!("1.0"), "{what}");
        let keys: Vec<&str> = operation
            .as_object()
            .unwrap()
            .keys()
            .map(String::as_str)
            .filter(|k| *k != "version")
            .collect();
        assert!(
            matches!(keys.as_slice(), ["createSurface"] | ["updateComponents"]),
            "{what}: {keys:?}"
        );
    }
}

/// Every golden that holds a surface (a list of A2UI operations) validates, and says `"1.0"`. The goldens
/// that are one component (`ask_form_v5.json`, `ask_form_v7.json`) are validated as the component of
/// catalog 9 their `component` names.
#[test]
fn every_golden_validates_against_agent_to_renderer_with_catalog_nine() {
    let validator = agent_to_renderer();
    let catalog = adam_ui::Catalog::from_document(
        catalog_nine(),
        &adam_ui::Claimed {
            catalog_id: catalog_nine()["catalogId"].as_str().unwrap().to_owned(),
            version: 9,
            digest: catalog_digest(&catalog_nine()).unwrap(),
        },
    )
    .unwrap();
    let dir = PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("tests/golden");
    let mut surfaces = 0;
    let mut components = 0;
    for entry in std::fs::read_dir(&dir).unwrap() {
        let path = entry.unwrap().path();
        if path.extension().and_then(|e| e.to_str()) != Some("json") {
            continue;
        }
        let name = path.file_name().unwrap().to_string_lossy().into_owned();
        let golden: Value = serde_json::from_slice(&std::fs::read(&path).unwrap()).unwrap();
        if golden.is_array() {
            assert_valid_surface(&validator, &name, &golden);
            surfaces += 1;
        } else {
            catalog
                .validate(&golden)
                .unwrap_or_else(|why| panic!("{name}: {why}"));
            components += 1;
        }
    }
    assert_eq!((surfaces, components), (5, 2), "the goldens of this crate");
}

fn screen_context() -> serde_json::Map<String, Value> {
    let catalog = catalog_nine();
    let reference = json!({
        "catalogId": catalog["catalogId"], "version": 9, "digest": catalog_digest(&catalog).unwrap()});
    let mut inline = reference.clone();
    inline["catalog"] = catalog;
    json!({
        CONTEXT_UI_REF: reference,
        CONTEXT_UI_CATALOG: inline,
        CONTEXT_A2UI_RENDERER: {"catalogIds": [catalog_nine()["catalogId"]]},
    })
    .as_object()
    .cloned()
    .unwrap()
}

async fn call(name: &str, args: Value) -> Result<adam_llm_agent::ToolOutput, ToolError> {
    let ui = Ui::embedded();
    let tool = ui.tools().get(name).cloned().unwrap();
    tool.call(
        &ToolCtx::detached(name, "call-1", Arc::new(NoopSink)).with_context(screen_context()),
        args,
    )
    .await
}

/// Fresh output of the three tools that draw — `ask_user`, `show` and `plan_work` — on a screen with
/// catalog 9 validates, so a change to a writer that the goldens do not cover is caught too.
#[tokio::test]
async fn fresh_ask_show_and_plan_output_validates_against_agent_to_renderer() {
    let validator = agent_to_renderer();

    let questions = json!({
        "question": "Before I ship.",
        "choices": [
            {"id": "plan", "question": "Which layout?", "options": ["A", "B"],
             "show": [
                {"component": "Text", "text": "Two layouts fit."},
                {"component": "Mermaid", "code": "graph TD; A-->B"},
                {"component": "Code", "code": "let x = 1;", "language": "rust"}]},
            {"id": "risk", "question": "Go on?", "type": "confirm",
             "show": [
                {"component": "Cards", "cards": [{"title": "Option A"}, {"title": "Option B"}]},
                {"component": "Callout", "tone": "warning", "title": "Heads up", "text": "This cannot be undone."},
                {"component": "Chart", "type": "bar", "labels": ["Mon", "Tue"], "series": [{"name": "Runs", "values": [3, 4]}]}]},
            {"id": "n", "question": "How many?", "type": "number"},
            {"id": "when", "question": "When?", "type": "date"}
        ]
    });
    match call("ask_user", questions).await {
        Err(ToolError::NeedsInput { ui: Some(ui), .. }) => {
            assert_valid_surface(&validator, "ask_user", &ui);
        }
        other => panic!("ask_user should park with a form: {other:?}"),
    }

    let shown = call(
        "show",
        json!({"blocks": [
            {"component": "Text", "text": "Three sources agree."},
            {"component": "Cards", "title": "Sources", "layout": "list",
             "cards": [{"title": "Tokio", "body": "The runtime."}]},
            {"component": "Mermaid", "code": "graph TD; A-->B"}]}),
    )
    .await
    .unwrap();
    assert_eq!(shown.artifacts.len(), 1);
    assert_eq!(
        shown.artifacts[0].mime_type.as_deref(),
        Some(A2UI_MEDIA_TYPE)
    );
    assert_valid_surface(&validator, "show", &shown.artifacts[0].data);

    let plan = call(
        "plan_work",
        json!({"title": "Fix the date parser", "items": ["Reproduce it", "Fix it", "Open the pull request"]}),
    )
    .await
    .unwrap();
    assert_eq!(plan.artifacts.len(), 1);
    assert_valid_surface(&validator, "plan_work", &plan.artifacts[0].data);
}

/// The capabilities the orchestrator sends (catalog 9 inline, under `"1.0"`) are what
/// `renderer_capabilities.json` describes, and `vymalo_inbound` turns them into the ref, the inline
/// catalog and the offer, the only things a tool reads.
#[test]
fn the_capabilities_the_orchestrator_sends_validate_and_become_the_ref_the_catalog_and_the_offer() {
    let catalog = catalog_nine();
    let id = catalog["catalogId"].clone();
    let capabilities = json!({"1.0": {"supportedCatalogIds": [id], "inlineCatalogs": [catalog]}});
    let validator = validator_for(&schema("renderer_capabilities.json"), &catalog);
    let found = errors(&validator, &capabilities);
    assert!(found.is_empty(), "{found:#?}");
    // the validator is not vacuous
    assert!(!errors(&validator, &json!({"1.0": {"inlineCatalogs": []}})).is_empty());
    assert!(!errors(&validator, &json!({"v1.0": {"supportedCatalogIds": []}})).is_empty());

    let digest = catalog_digest(&catalog).unwrap();
    let mut message = Message::new(Role::User, vec![Part::text("hello")]);
    message.metadata = Some(
        json!({
            UI_CATALOG_EXTENSION: {"catalogId": id, "version": 9.0, "digest": digest, "inline": true},
            "a2uiRendererCapabilities": capabilities,
        })
        .as_object()
        .unwrap()
        .iter()
        .map(|(k, v)| (k.clone(), v.clone()))
        .collect(),
    );
    let context = vymalo_inbound(&message).unwrap().payload["context"].clone();
    assert_eq!(
        context[CONTEXT_UI_REF],
        json!({"catalogId": id, "version": 9, "digest": digest})
    );
    assert_eq!(context[CONTEXT_UI_CATALOG]["catalog"], catalog);
    assert_eq!(context[CONTEXT_A2UI_RENDERER], json!({"catalogIds": [id]}));
}

// ---- the card and the actions (A2)

/// The `params` of the A2UI entry of the cards of `adam-agent` and `adam-coder` are what
/// `agent_capabilities.json#/properties/1.0` describes, and the entry names the v1.0 extension and
/// the v1.0 basic catalog.
#[test]
fn the_cards_a2ui_params_validate_against_agent_capabilities() {
    let validator = validator_for(
        &json!({
            "$schema": "https://json-schema.org/draft/2020-12/schema",
            "$ref": "https://a2ui.org/specification/v1_0/agent_capabilities.json#/properties/1.0",
        }),
        &catalog_nine(),
    );
    let entry = adam_ui::card_extensions()
        .into_iter()
        .find(|e| e.uri == adam_a2a::A2UI_EXTENSION_V1_0)
        .expect("the card lists the A2UI v1.0 extension");
    assert_eq!(
        entry.uri, "https://a2ui.org/a2a-extension/a2ui/v1.0",
        "the URI of the sys contract"
    );
    let params = Value::Object(entry.params);
    let found = errors(&validator, &params);
    assert!(found.is_empty(), "{found:#?}");
    assert_eq!(
        params["supportedCatalogIds"],
        json!([adam_a2a::A2UI_BASIC_CATALOG_V1_0])
    );
    assert_eq!(
        adam_a2a::A2UI_BASIC_CATALOG_V1_0,
        "https://a2ui.org/specification/v1_0/catalogs/basic/catalog.json"
    );
    // The same entry of the coder's golden card.
    let card: Value = serde_json::from_str(include_str!(
        "../../../bin/adam-coder/tests/fixtures/agent/card.json"
    ))
    .unwrap();
    let golden = card["extensions"]
        .as_array()
        .unwrap()
        .iter()
        .find(|e| e["uri"] == adam_a2a::A2UI_EXTENSION_V1_0)
        .expect("the coder's card lists it");
    let found = errors(&validator, &golden["params"]);
    assert!(found.is_empty(), "{found:#?}");
    assert_eq!(golden["params"], params);
    // the validator is not vacuous
    assert!(!errors(&validator, &json!({"acceptsInlineCatalogs": "yes"})).is_empty());
    assert!(!errors(&validator, &json!({"supportedCatalogIds": "basic"})).is_empty());
}

/// The actions adam reads (the form's submit as the orchestrator sends it, and any other action)
/// are what `renderer_to_agent.json` describes, with `"version": "1.0"`; the same message under
/// the earlier version is not.
#[test]
fn the_actions_adam_reads_validate_against_renderer_to_agent() {
    let validator = validator_for(&schema("renderer_to_agent.json"), &catalog_nine());
    let submit = json!({"version": "1.0", "action": {
        "name": "answer", "surfaceId": "ask-c1", "sourceComponentId": "submit",
        "timestamp": "2026-11-20T10:00:00Z",
        "context": {"answers": [{"id": "q1", "values": ["a"]}], "note": "ok"}}});
    let other = json!({"version": "1.0", "action": {
        "name": "open", "surfaceId": "s", "sourceComponentId": "b",
        "timestamp": "2026-11-20T10:00:00Z", "context": {}}});
    for action in [&submit, &other] {
        let found = errors(&validator, action);
        assert!(found.is_empty(), "{found:#?}");
        // and vymalo_inbound reads it
        let part = Part::data(json!([action])).with_media_type(A2UI_MEDIA_TYPE);
        let message = Message::new(Role::User, vec![part]);
        let text = vymalo_inbound(&message).unwrap().payload["text"].clone();
        assert!(text.as_str().unwrap().starts_with("The person "), "{text}");
    }
    for old in ["v0.9.1", "v0.9", "0.9"] {
        let mut stale = submit.clone();
        stale["version"] = json!(old);
        assert!(!errors(&validator, &stale).is_empty(), "{old}");
    }
}
