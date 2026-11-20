//! What the renderer offers in the message of this turn (`a2uiRendererCapabilities["1.0"]`, kept by
//! `vymalo_inbound` as `vymalo.a2ui.renderer`) decides whether a tool draws: with no offer, or an offer
//! that does not list the current `catalogId`, `ask_user` asks in words, `show` and `ui_catalog` say to
//! answer in text and `plan_work` draws no surface, whatever a cache holds or an earlier turn offered.
#![allow(clippy::unwrap_used, clippy::expect_used)] // integration tests assert by unwrapping

use std::sync::Arc;

use a2a::{Message, Part, Role};
use adam_a2a::UI_CATALOG_EXTENSION;
use adam_a2a_runtime::{CONTEXT_A2UI_RENDERER, CONTEXT_UI_CATALOG, CONTEXT_UI_REF, vymalo_inbound};
use adam_llm_agent::{ToolCtx, ToolError, ToolOutput};
use adam_mcp::McpPolicy;
use adam_runtime::NoopSink;
use adam_ui::{Ui, catalog_digest};
use serde_json::{Map, Value, json};

const V9: &str = include_str!("fixtures/catalog-v9.json");

fn catalog() -> Value {
    serde_json::from_str(V9).unwrap()
}

fn catalog_id() -> String {
    catalog()["catalogId"].as_str().unwrap().to_owned()
}

fn reference() -> Value {
    json!({"catalogId": catalog_id(), "version": 9, "digest": catalog_digest(&catalog()).unwrap()})
}

/// The context of a message that named the catalog (and carried it, when `inline`) and whose renderer
/// offered `offer` (`None`: no entry at all).
fn context(offer: Option<Value>, inline: bool) -> Map<String, Value> {
    let mut context = json!({CONTEXT_UI_REF: reference()});
    if inline {
        let mut carried = reference();
        carried["catalog"] = catalog();
        context[CONTEXT_UI_CATALOG] = carried;
    }
    if let Some(offer) = offer {
        context[CONTEXT_A2UI_RENDERER] = offer;
    }
    context.as_object().cloned().unwrap()
}

fn offering(ids: &[&str]) -> Option<Value> {
    Some(json!({"catalogIds": ids}))
}

fn ctx(name: &str, context: &Map<String, Value>) -> ToolCtx {
    ToolCtx::detached(name, "c1", Arc::new(NoopSink)).with_context(context.clone())
}

async fn call(
    ui: &Ui,
    name: &str,
    context: &Map<String, Value>,
    args: Value,
) -> Result<ToolOutput, ToolError> {
    ui.tools()
        .get(name)
        .cloned()
        .unwrap()
        .call(&ctx(name, context), args)
        .await
}

fn questions() -> Value {
    json!({
        "question": "Two quick questions",
        "choices": [
            {"id": "db", "question": "Which database?", "options": ["Postgres", "SQLite"]},
            {"id": "deploy", "question": "Where does it run?", "options": ["Kubernetes", "Compose"]}
        ]
    })
}

const IN_WORDS: &str = "Two quick questions\n\
     \n1. Which database?\n   a) Postgres\n   b) SQLite\
     \n2. Where does it run?\n   a) Kubernetes\n   b) Compose";

const NOT_OFFERED: &str = "this screen did not offer a component catalog this turn; answer in text";

async fn drew(ui: &Ui, context: &Map<String, Value>) -> bool {
    match call(ui, "ask_user", context, questions()).await {
        Err(ToolError::NeedsInput { question, ui }) => {
            if ui.is_none() {
                assert_eq!(question, IN_WORDS);
            }
            ui.is_some()
        }
        other => panic!("expected a question, got {other:?}"),
    }
}

/// Every tool of the screen, on a turn whose renderer offered nothing usable.
async fn assert_in_words_everywhere(context: &Map<String, Value>, what: &str) {
    let ui = Ui::new(McpPolicy::default());
    assert!(
        !drew(&ui, context).await,
        "{what}: ask_user must ask in words"
    );
    for name in ["show", "ui_catalog"] {
        let out = call(
            &ui,
            name,
            context,
            json!({"blocks": [{"component": "Text", "text": "x"}]}),
        )
        .await
        .unwrap();
        assert!(
            out.is_error && out.content == NOT_OFFERED && out.artifacts.is_empty(),
            "{what}: {name}: {}",
            out.content
        );
    }
    let plan = call(
        &ui,
        "plan_work",
        context,
        json!({"title": "T", "items": ["a", "b"]}),
    )
    .await
    .unwrap();
    assert!(
        plan.artifacts.is_empty(),
        "{what}: plan_work drew a surface"
    );
    assert!(plan.plan.is_some(), "{what}: the plan is still kept");
}

#[tokio::test]
async fn with_no_renderer_entry_every_tool_answers_in_words() {
    assert_in_words_everywhere(&context(None, true), "inline, no entry").await;
    assert_in_words_everywhere(&context(None, false), "named, no entry").await;
    // A null entry is the same as none: it is what a message without capabilities sets.
    assert_in_words_everywhere(&context(Some(Value::Null), true), "null entry").await;
}

#[tokio::test]
async fn an_entry_that_lists_another_id_is_the_same_as_no_entry() {
    assert_in_words_everywhere(
        &context(offering(&["https://a2ui.org/basic"]), true),
        "other id",
    )
    .await;
    assert_in_words_everywhere(&context(offering(&[]), true), "empty").await;
}

#[tokio::test]
async fn an_offer_decides_before_a_cache_that_holds_the_catalog() {
    let ui = Ui::new(McpPolicy::default());
    let id = catalog_id();
    assert!(drew(&ui, &context(offering(&[id.as_str()]), true)).await);
    assert_eq!(ui.cache().len(), 1, "the catalog is held now");
    // The same digest, held, but this turn offers nothing.
    assert!(!drew(&ui, &context(None, false)).await);
    assert!(!drew(&ui, &context(offering(&["other"]), false)).await);
}

#[tokio::test]
async fn an_id_only_in_the_supported_ids_draws_with_the_catalog_cached() {
    let ui = Ui::new(McpPolicy::default());
    let id = catalog_id();
    // The first turn carried the catalog and cached it; this one names it and offers it.
    assert!(drew(&ui, &context(offering(&[id.as_str()]), true)).await);
    assert!(drew(&ui, &context(offering(&["x", id.as_str()]), false)).await);
}

fn message(text: &str, capabilities: Option<Value>) -> Message {
    let mut metadata = json!({UI_CATALOG_EXTENSION: {
        "catalogId": catalog_id(), "version": 9.0, "digest": catalog_digest(&catalog()).unwrap(),
        "inline": true}});
    if let Some(capabilities) = capabilities {
        metadata["a2uiRendererCapabilities"] = capabilities;
    }
    let mut message = Message::new(Role::User, vec![Part::text(text)]);
    message.metadata = Some(
        metadata
            .as_object()
            .unwrap()
            .iter()
            .map(|(k, v)| (k.clone(), v.clone()))
            .collect(),
    );
    message
}

fn context_of(message: &Message) -> Map<String, Value> {
    vymalo_inbound(message).unwrap().payload["context"]
        .as_object()
        .cloned()
        .unwrap()
}

#[tokio::test]
async fn an_id_only_in_the_inline_catalogs_draws() {
    let ui = Ui::new(McpPolicy::default());
    let message = message(
        "hi",
        Some(json!({"1.0": {"supportedCatalogIds": [], "inlineCatalogs": [catalog()]}})),
    );
    assert!(drew(&ui, &context_of(&message)).await);
}

#[tokio::test]
async fn a_second_turn_with_no_capabilities_is_in_words() {
    let ui = Ui::new(McpPolicy::default());
    let id = catalog_id();
    let first = message(
        "one",
        Some(json!({"1.0": {"supportedCatalogIds": [id], "inlineCatalogs": [catalog()]}})),
    );
    assert!(drew(&ui, &context_of(&first)).await);
    // The second message names the catalog and has no capabilities (the old keys do not count).
    let second = message("two", None);
    assert!(!drew(&ui, &context_of(&second)).await);
    let old = message(
        "three",
        Some(json!({"v0.9.1": {"supportedCatalogIds": [catalog_id()]}})),
    );
    let mut context = context_of(&old);
    assert_eq!(context[CONTEXT_A2UI_RENDERER], Value::Null);
    assert!(!drew(&ui, &context).await);
    // Merged as the conversation does it, a null deletes the key: the same.
    context.remove(CONTEXT_A2UI_RENDERER);
    assert!(!drew(&ui, &context).await);
}
