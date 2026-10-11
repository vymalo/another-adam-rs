//! `ask_user` against the web's catalog of version 5 (copied into `tests/fixtures`, digest pinned):
//! a form with a title, a description, typed questions, pictures and a free box, drawn whole on a
//! screen that has version 5, as the subset that version 2 has on an older one, and as text where
//! nothing can be drawn. Every instance the tool makes is checked against the web's own schema.
#![allow(clippy::unwrap_used, clippy::expect_used)] // integration tests assert by unwrapping

use std::sync::Arc;

use adam_a2a_runtime::{CONTEXT_UI_CATALOG, CONTEXT_UI_REF};
use adam_llm_agent::{ToolCtx, ToolError, ToolOutput};
use adam_mcp::McpPolicy;
use adam_runtime::NoopSink;
use adam_ui::{Catalog, Claimed, Ui, catalog_digest};
use serde_json::{Map, Value, json};

const CATALOG_ID: &str = "https://agents.vymalo.com/a2ui/catalogs/chat";
const V5: &str = include_str!("fixtures/catalog-v5.json");
const V5_LOCK: &str = include_str!("fixtures/catalog-v5.lock.json");
const V2: &str = include_str!("fixtures/catalog-v2.json");
const V2_LOCK: &str = include_str!("fixtures/catalog-v2.lock.json");
const SHA_A: &str = "a1b2c3d4e5f60718293a4b5c6d7e8f90a1b2c3d4e5f60718293a4b5c6d7e8f90";
const SHA_B: &str = "0123456789abcdef0123456789abcdef0123456789abcdef0123456789abcdef";

/// A catalog of the fixtures: its document and what it claims to be.
struct Screen {
    document: Value,
    claimed: Claimed,
}

fn screen(document: &str, lock: &str) -> Screen {
    let lock: Value = serde_json::from_str(lock).unwrap();
    Screen {
        document: serde_json::from_str(document).unwrap(),
        claimed: Claimed {
            catalog_id: CATALOG_ID.to_owned(),
            version: u32::try_from(lock["version"].as_u64().unwrap()).unwrap(),
            digest: lock["digest"].as_str().unwrap().to_owned(),
        },
    }
}

fn v5() -> Screen {
    screen(V5, V5_LOCK)
}

fn v2() -> Screen {
    screen(V2, V2_LOCK)
}

/// Every whole number as a double, as an A2A server hands the metadata of a message over.
fn as_doubles(value: &Value) -> Value {
    match value {
        Value::Number(n) if n.is_i64() || n.is_u64() => json!(n.as_f64().unwrap()),
        Value::Array(items) => Value::Array(items.iter().map(as_doubles).collect()),
        Value::Object(map) => Value::Object(
            map.iter()
                .map(|(k, v)| (k.clone(), as_doubles(v)))
                .collect(),
        ),
        other => other.clone(),
    }
}

/// The context of a message that carried the catalog inline.
fn context(screen: &Screen) -> Map<String, Value> {
    let c = &screen.claimed;
    json!({
        CONTEXT_UI_REF: {"catalogId": c.catalog_id, "version": c.version, "digest": c.digest},
        CONTEXT_UI_CATALOG: {
            "catalogId": c.catalog_id, "version": c.version, "digest": c.digest,
            "catalog": as_doubles(&screen.document)},
    })
    .as_object()
    .cloned()
    .unwrap()
}

async fn ask(screen: Option<&Screen>, args: Value) -> Result<ToolOutput, ToolError> {
    let ui = Ui::new(McpPolicy::default());
    let tool = ui.tools().get("ask_user").cloned().unwrap();
    let context = screen.map(context).unwrap_or_default();
    tool.call(
        &ToolCtx::detached("ask_user", "call-1", Arc::new(NoopSink)).with_context(context),
        args,
    )
    .await
}

/// The question and the Choices component of a call that parked with a form.
async fn form(screen: &Screen, args: Value) -> (String, Value) {
    match ask(Some(screen), args).await {
        Err(ToolError::NeedsInput {
            question,
            ui: Some(ui),
        }) => (question, ui[1]["updateComponents"]["components"][0].clone()),
        other => panic!("expected a question with a form, got {other:?}"),
    }
}

/// The text of a call that parked without a form.
async fn words(screen: Option<&Screen>, args: Value) -> String {
    match ask(screen, args).await {
        Err(ToolError::NeedsInput { question, ui: None }) => question,
        other => panic!("expected a question in words, got {other:?}"),
    }
}

/// The error a call came back with.
async fn refused(screen: Option<&Screen>, args: Value) -> String {
    match ask(screen, args).await {
        Ok(out) if out.is_error => out.content,
        other => panic!("expected an error result, got {other:?}"),
    }
}

fn migration() -> Value {
    json!({
        "question": "A few things I need before I write the migration.",
        "title": "Before I start",
        "description": "The tables depend on the database and on where this runs.",
        "freeInput": {"label": "Anything else I should know?", "placeholder": "A deadline, a constraint…"},
        "submitLabel": "Start",
        "choices": [
            {"id": "db", "question": "Which database?", "help": "The one the service keeps its data in",
             "default": "Postgres",
             "options": [
                {"value": "pg", "label": "Postgres", "description": "Relational, the default", "icon": "git", "recommended": true},
                {"value": "sqlite", "label": "SQLite", "description": "One file, no server"}]},
            {"id": "env", "question": "Which environments?", "type": "multiple", "min": 1, "max": 2,
             "options": [{"label": "Development"}, {"label": "Staging"}, {"label": "Production", "description": "Needs a window"}]},
            {"id": "name", "question": "What is the project called?", "type": "text", "help": "One word"},
            {"id": "story", "question": "Tell me more", "type": "longText", "required": false}
        ]
    })
}

// ---- the fixture is the web's catalog ----

#[test]
fn the_web_catalog_of_version_5_hashes_to_its_lock_and_every_component_compiles() {
    let screen = v5();
    assert_eq!(
        catalog_digest(&screen.document).unwrap(),
        screen.claimed.digest,
        "adam's digest is the web's"
    );
    let catalog = Catalog::from_document(screen.document.clone(), &screen.claimed).unwrap();
    assert_eq!(catalog.version(), 5);
    let choices = catalog.component("Choices").unwrap();
    for member in [
        "title",
        "description",
        "freeInput",
        "questions",
        "submitLabel",
    ] {
        assert!(
            choices.schema()["properties"][member].is_object(),
            "{member}"
        );
    }
    let question = &choices.schema()["properties"]["questions"]["items"]["properties"];
    for member in ["help", "type", "default", "min", "max", "options"] {
        assert!(question[member].is_object(), "{member}");
    }
    let option = &question["options"]["items"]["properties"];
    for member in ["image", "icon", "recommended"] {
        assert!(option[member].is_object(), "{member}");
    }
}

// ---- the whole form on a screen of version 5 ----

#[tokio::test]
async fn a_form_with_a_title_a_description_types_and_a_free_box_is_drawn_whole() {
    let (question, root) = form(&v5(), migration()).await;
    assert_eq!(
        question,
        "A few things I need before I write the migration."
    );
    let golden: Value = serde_json::from_str(include_str!("golden/ask_form_v5.json")).unwrap();
    assert_eq!(root, golden);
}

#[tokio::test]
async fn every_type_is_a_valid_instance_of_the_web_schema() {
    let deck_option = |label: &str, sha: &str| json!({"label": label, "description": "A screen", "image": {"sha256": sha, "alt": "A screenshot"}});
    let fifty: Vec<Value> = (0..50)
        .map(|n| json!({"label": format!("Option {n}")}))
        .collect();
    let calls = [
        json!({"question": "q", "choices": [{"question": "Which?", "options": ["a", "b"]}]}),
        json!({"question": "q", "choices": [{"question": "Which?", "type": "single", "options": ["a", "b"]}]}),
        json!({"question": "q", "choices": [{"question": "Which?", "type": "multiple", "options": ["a", "b", "c"], "min": 1, "max": 3, "default": ["a", "c"]}]}),
        json!({"question": "q", "choices": [{"question": "Which?", "multiple": true, "options": ["a", "b"]}]}),
        json!({"question": "q", "choices": [{"question": "Which?", "type": "select", "options": fifty}]}),
        json!({"question": "q", "choices": [{"question": "Which?", "type": "select", "multiple": true, "max": 2, "options": ["a", "b", "c"]}]}),
        json!({"question": "q", "choices": [{"question": "Which?", "type": "images", "options": [deck_option("A", SHA_A), deck_option("B", SHA_B)]}]}),
        json!({"question": "q", "choices": [{"question": "Which?", "type": "images", "multiple": true, "options": [deck_option("A", SHA_A), deck_option("B", SHA_B)]}]}),
        json!({"question": "q", "choices": [{"question": "Name?", "type": "text", "default": "Ada"}]}),
        json!({"question": "q", "choices": [{"question": "More?", "type": "longText", "required": false}]}),
        json!({"question": "q", "freeInput": true, "choices": [{"question": "Which?", "options": ["a", "b"], "allowOther": true}]}),
    ];
    for call in calls {
        // `form` fails the test when the schema refuses the instance
        let (_, root) = form(&v5(), call.clone()).await;
        assert_eq!(root["component"], "Choices", "{call}");
    }
}

#[tokio::test]
async fn an_icon_the_catalog_does_not_have_comes_back_with_the_place() {
    let out = refused(
        Some(&v5()),
        json!({"question": "q", "choices": [{"question": "Which?", "options": [{"label": "A", "icon": "rocket"}, "B"]}]}),
    )
    .await;
    assert!(
        out.contains("do not fit the screen's Choices component"),
        "{out}"
    );
    assert!(out.contains("/options/0/icon"), "{out}");
}

#[tokio::test]
async fn the_models_mistakes_in_a_form_come_back_before_the_catalog_is_asked() {
    let q = |extra: Value| {
        let mut base = json!({"question": "Which?", "options": ["a", "b"]});
        base.as_object_mut()
            .unwrap()
            .extend(extra.as_object().unwrap().clone());
        json!({"question": "q", "choices": [base]})
    };
    let screen = v5();
    let cases = [
        (q(json!({"options": ["only"]})), "needs 2 to 12"),
        (
            q(
                json!({"type": "select", "options": (0..51).map(|n| n.to_string()).collect::<Vec<_>>()}),
            ),
            "2 to 50",
        ),
        (q(json!({"type": "images"})), "has no image"),
        (q(json!({"type": "text"})), "takes no options"),
        (q(json!({"default": "Oracle"})), "not one of its options"),
        (q(json!({"min": 1})), "apply when several may be chosen"),
        (
            q(json!({"type": "single", "multiple": true})),
            "\"single\" but has `multiple: true`",
        ),
        (q(json!({"type": "slider"})), "unknown variant `slider`"),
    ];
    for (call, expected) in cases {
        let out = refused(Some(&screen), call.clone()).await;
        assert!(out.contains(expected), "{expected}: {out}");
    }
    // A picture is a file of the conversation by its hash, never a URL.
    let url = refused(
        Some(&screen),
        json!({"question": "q", "choices": [{"question": "Which?", "type": "images", "options": [
            {"label": "A", "image": {"sha256": "https://evil.example/a.png", "alt": "x"}},
            {"label": "B", "image": {"sha256": SHA_B, "alt": "x"}}]}]}),
    )
    .await;
    assert!(url.contains("never a URL"), "{url}");
}

// ---- an older screen ----

#[tokio::test]
async fn a_screen_of_version_2_gets_the_subset_it_has_and_the_description_in_the_text() {
    let call = json!({
        "question": "Which database?",
        "title": "Before I start",
        "description": "The tables depend on it.",
        "freeInput": true,
        "choices": [{"id": "db", "question": "Which one?", "help": "Where the data lives",
            "options": [{"label": "Postgres", "recommended": true, "icon": "git", "description": "default"}, "SQLite"],
            "default": "Postgres"}]
    });
    let (question, root) = form(&v2(), call).await;
    assert_eq!(question, "Which database?\nThe tables depend on it.");
    assert_eq!(
        root,
        json!({
            "id": "root", "component": "Choices", "title": "Before I start",
            "questions": [{"id": "db", "question": "Which one? (Where the data lives)",
                "options": [{"value": "postgres", "label": "Postgres (recommended)", "description": "default"},
                            {"value": "sqlite", "label": "SQLite"}]}],
            "action": {"event": {"name": "answer"}}
        })
    );
}

#[tokio::test]
async fn a_screen_of_version_2_cannot_take_a_typed_answer_so_the_form_is_words() {
    let text = words(Some(&v2()), migration()).await;
    assert!(
        text.starts_with(
            "A few things I need before I write the migration.\nThe tables depend on the database and on where this runs.\n"
        ),
        "{text}"
    );
    assert!(
        text.contains("\n3. What is the project called? (One word) (answer in a few words)"),
        "{text}"
    );
    assert!(
        text.contains("\n   a) Postgres - Relational, the default (recommended) (default)"),
        "{text}"
    );
    assert!(
        text.ends_with("Anything else I should know? (optional: add it to your reply)"),
        "{text}"
    );
}

#[tokio::test]
async fn a_screen_with_no_catalog_gets_the_form_as_words_too() {
    let text = words(None, migration()).await;
    assert!(
        text.contains("2. Which environments? (any number of these)"),
        "{text}"
    );
    assert!(
        text.contains("\n   c) Production - Needs a window"),
        "{text}"
    );
}

#[tokio::test]
async fn a_plain_question_ignores_a_title_and_a_description() {
    let text = words(
        Some(&v5()),
        json!({"question": " Which city? ", "title": "T", "description": "D"}),
    )
    .await;
    assert_eq!(text, "Which city?");
}
