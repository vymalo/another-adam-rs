//! The edges of which catalog is current and whether the renderer offered it: the inline catalog's own
//! claim when no reference came, a claim that is not the reference, a version below 1, empty text, and
//! a catalog read again whose id this turn did not offer. Each is a place where a tool could draw
//! with a catalog the screen did not offer or did not name.
#![allow(clippy::unwrap_used, clippy::expect_used)] // integration tests assert by unwrapping

use std::sync::Arc;

use adam_a2a_runtime::{
    CONTEXT_A2UI_RENDERER, CONTEXT_THREAD_TOOLS, CONTEXT_UI_CATALOG, CONTEXT_UI_REF,
};
use adam_llm_agent::{ToolCtx, ToolError, ToolOutput};
use adam_mcp::McpPolicy;
use adam_mcp_testkit::ThreadToolsServer;
use adam_runtime::NoopSink;
use adam_ui::{Ui, catalog_digest};
use serde_json::{Map, Value, json};

const V9: &str = include_str!("fixtures/catalog-v9.json");

const NOT_OFFERED: &str = "this screen did not offer a component catalog this turn; answer in text";
const NO_CATALOG: &str = "this screen has no component catalog; answer in text";

fn catalog() -> Value {
    serde_json::from_str(V9).unwrap()
}

fn catalog_id() -> String {
    catalog()["catalogId"].as_str().unwrap().to_owned()
}

fn digest() -> String {
    catalog_digest(&catalog()).unwrap()
}

fn claim(version: u64) -> Value {
    json!({"catalogId": catalog_id(), "version": version, "digest": digest()})
}

fn carried(version: u64) -> Value {
    let mut entry = claim(version);
    entry["catalog"] = catalog();
    entry
}

fn offering(ids: &[String]) -> Value {
    json!({"catalogIds": ids})
}

fn object(value: Value) -> Map<String, Value> {
    value.as_object().cloned().unwrap()
}

async fn call(
    ui: &Ui,
    name: &str,
    context: &Map<String, Value>,
    args: Value,
) -> Result<ToolOutput, ToolError> {
    let ctx = ToolCtx::detached(name, "c1", Arc::new(NoopSink)).with_context(context.clone());
    ui.tools()
        .get(name)
        .cloned()
        .unwrap()
        .call(&ctx, args)
        .await
}

/// Whether `ask_user` drew a form on this context.
async fn drew(context: &Map<String, Value>) -> bool {
    let args = json!({"question": "Which?", "choices": [
        {"id": "db", "question": "Which database?", "options": ["Postgres", "SQLite"]}]});
    match call(&Ui::new(McpPolicy::default()), "ask_user", context, args).await {
        Err(ToolError::NeedsInput { ui, .. }) => ui.is_some(),
        other => panic!("expected a question, got {other:?}"),
    }
}

/// What `show` says on this context.
async fn show_says(ui: &Ui, context: &Map<String, Value>) -> String {
    let out = call(
        ui,
        "show",
        context,
        json!({"blocks": [{"component": "Text", "text": "x"}]}),
    )
    .await
    .unwrap();
    assert!(out.is_error, "{}", out.content);
    out.content
}

/// A message with no reference: the catalog it carries says which catalog it is.
#[tokio::test]
async fn an_inline_catalog_with_no_reference_is_the_current_one() {
    let context = object(json!({
        CONTEXT_A2UI_RENDERER: offering(&[catalog_id()]),
        CONTEXT_UI_CATALOG: carried(9),
    }));
    assert!(drew(&context).await);
}

/// The inline catalog must be the one the reference names: a claim of another version is not it, even
/// when its document hashes to the reference's digest.
#[tokio::test]
async fn an_inline_catalog_whose_claim_is_not_the_reference_is_not_used() {
    let same = object(json!({
        CONTEXT_A2UI_RENDERER: offering(&[catalog_id()]),
        CONTEXT_UI_REF: claim(9),
        CONTEXT_UI_CATALOG: carried(9),
    }));
    assert!(drew(&same).await, "the control: the same claim is used");

    let other_version = object(json!({
        CONTEXT_A2UI_RENDERER: offering(&[catalog_id()]),
        CONTEXT_UI_REF: claim(9),
        CONTEXT_UI_CATALOG: carried(8),
    }));
    assert!(
        !drew(&other_version).await,
        "no grant to read it again, so words"
    );
}

/// A catalog version starts at 1: a reference or a claim with 0 names no catalog.
#[tokio::test]
async fn version_zero_names_no_catalog() {
    let context = object(json!({
        CONTEXT_A2UI_RENDERER: offering(&[catalog_id()]),
        CONTEXT_UI_REF: claim(0),
        CONTEXT_UI_CATALOG: carried(0),
    }));
    let ui = Ui::new(McpPolicy::default());
    assert_eq!(show_says(&ui, &context).await, NO_CATALOG);
    assert!(!drew(&context).await);
}

/// An empty `catalogId` or digest names no catalog, even when the renderer offers the empty id.
#[tokio::test]
async fn an_empty_catalog_id_or_digest_names_no_catalog() {
    let ui = Ui::new(McpPolicy::default());
    for reference in [
        json!({"catalogId": "", "version": 9, "digest": digest()}),
        json!({"catalogId": catalog_id(), "version": 9, "digest": ""}),
    ] {
        let context = object(json!({
            CONTEXT_A2UI_RENDERER: offering(&[String::new(), catalog_id()]),
            CONTEXT_UI_REF: reference,
        }));
        assert_eq!(show_says(&ui, &context).await, NO_CATALOG, "{context:?}");
    }
}

/// The renderer's offer decides, not the endpoint: a catalog read again under an id this turn did not
/// offer is `NotOffered` (and never drawn with), not an unreadable catalog.
#[tokio::test]
async fn a_catalog_read_again_under_an_id_that_was_not_offered_is_not_offered() {
    let server = ThreadToolsServer::start(&["tok"]).await;
    server.set_catalog(Some((
        "https://other.example.test/catalog",
        9,
        &digest(),
        catalog(),
    )));
    let context = object(json!({
        CONTEXT_A2UI_RENDERER: offering(&[catalog_id()]),
        CONTEXT_UI_REF: claim(9),
        CONTEXT_THREAD_TOOLS: {"url": server.url("t1"), "token": "tok", "expiresAt": "2999-01-01T00:00:00Z"},
    }));
    let ui = Ui::new(McpPolicy::default());
    assert_eq!(show_says(&ui, &context).await, NOT_OFFERED);
    assert_eq!(
        server.catalog_requests().len(),
        1,
        "it was asked, and refused what it got"
    );
    assert_eq!(ui.cache().len(), 0, "nothing of it is kept");
}
