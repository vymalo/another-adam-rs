//! `Ui::embedded` and `Ui::screen` (ADR 0047 slice 1): the screen's tools with no thread-tools
//! client. A catalog a message carries is used; one that would have to be read again is not, and no
//! endpoint is ever asked, even when the message names one; the screen's source adds the mentioned
//! agents and describes `show`, with no tool of its own.
#![allow(clippy::unwrap_used, clippy::expect_used)] // integration tests assert by unwrapping

use std::sync::Arc;

use adam_a2a_runtime::{
    CONTEXT_MENTIONS, CONTEXT_THREAD_TOOLS, CONTEXT_UI_CATALOG, CONTEXT_UI_REF,
};
use adam_llm_agent::{SourceCtx, ToolCtx, ToolSource as _};
use adam_mcp_testkit::ThreadToolsServer;
use adam_runtime::NoopSink;
use adam_ui::{Claimed, Ui, catalog_digest};
use serde_json::{Map, Value, json};

const CATALOG_ID: &str = "https://agents.vymalo.com/a2ui/catalogs/chat";
const FIXTURE: &str = include_str!("fixtures/catalog-v2.json");
const LOCK: &str = include_str!("fixtures/catalog-v2.lock.json");

/// The catalog as a screen sends it: the published version 2 with the `ui-catalog/v2` `protocolVersion`.
fn document() -> Value {
    let mut document: Value = serde_json::from_str(FIXTURE).unwrap();
    document["protocolVersion"] = json!("1.0");
    document
}

fn claimed() -> Claimed {
    let lock: Value = serde_json::from_str(LOCK).unwrap();
    Claimed {
        catalog_id: CATALOG_ID.to_owned(),
        version: u32::try_from(lock["version"].as_u64().unwrap()).unwrap(),
        digest: catalog_digest(&document()).unwrap(),
    }
}

fn reference(c: &Claimed) -> Value {
    json!({"catalogId": c.catalog_id, "version": c.version, "digest": c.digest})
}

fn object(value: Value) -> Map<String, Value> {
    value.as_object().cloned().unwrap()
}

fn inline_context(c: &Claimed) -> Map<String, Value> {
    let document = document();
    object(json!({
        CONTEXT_UI_REF: reference(c),
        CONTEXT_UI_CATALOG: {
            "catalogId": c.catalog_id, "version": c.version, "digest": c.digest, "catalog": document},
    }))
}

/// A message that only names the current catalog, with a grant for an endpoint that has it.
fn ref_context(c: &Claimed, url: &str) -> Map<String, Value> {
    object(json!({
        CONTEXT_UI_REF: reference(c),
        CONTEXT_THREAD_TOOLS: {"url": url, "token": "tok", "expiresAt": "2999-01-01T00:00:00Z"},
    }))
}

fn ctx(name: &str, context: Map<String, Value>) -> ToolCtx {
    ToolCtx::detached(name, "c1", Arc::new(NoopSink)).with_context(context)
}

fn show_description(specs: &[adam_model::ToolSpec]) -> String {
    specs
        .iter()
        .find(|s| s.name == "show")
        .unwrap()
        .description
        .clone()
}

async fn endpoint_with_catalog(c: &Claimed) -> ThreadToolsServer {
    let document = document();
    let server = ThreadToolsServer::start(&["tok"]).await;
    server.set_catalog(Some((
        &c.catalog_id,
        u64::from(c.version),
        &c.digest,
        document,
    )));
    server
}

#[tokio::test]
async fn an_embedded_ui_uses_the_catalog_a_message_carries() {
    let c = claimed();
    let ui = Ui::embedded();
    let tool = ui.tools().get("ui_catalog").cloned().unwrap();

    let out = tool
        .call(&ctx("ui_catalog", inline_context(&c)), json!({}))
        .await
        .unwrap();

    assert!(!out.is_error, "{}", out.content);
    assert!(out.content.contains("Choices"), "{}", out.content);
    assert_eq!(ui.cache().len(), 1);
}

#[tokio::test]
async fn an_embedded_ui_never_asks_an_endpoint_even_when_a_message_names_one() {
    let c = claimed();
    let server = endpoint_with_catalog(&c).await;
    let ui = Ui::embedded();
    let tool = ui.tools().get("ui_catalog").cloned().unwrap();

    let out = tool
        .call(
            &ctx("ui_catalog", ref_context(&c, &server.url("t1"))),
            json!({}),
        )
        .await
        .unwrap();

    assert!(
        out.is_error,
        "no catalog can be read again: {}",
        out.content
    );
    assert!(
        out.content.contains("could not be read; answer in text"),
        "the unreadable case, not the no-catalog one: {}",
        out.content
    );
    assert!(
        server.catalog_requests().is_empty(),
        "the endpoint was asked"
    );
    // Its thread-tools source, which a build with thread tools still has, offers and calls nothing.
    #[cfg(feature = "thread-tools")]
    {
        let source = ui.source();
        let sctx = SourceCtx::detached(ref_context(&c, &server.url("t1")));
        assert!(source.specs(&sctx).await.is_empty());
        assert!(
            source
                .call(
                    &ctx("anything", ref_context(&c, &server.url("t1"))),
                    "anything",
                    json!({})
                )
                .await
                .is_none()
        );
    }
    assert!(server.catalog_requests().is_empty());
    assert!(
        server.requests().is_empty(),
        "nothing at all reached the endpoint"
    );
}

#[tokio::test]
async fn the_screen_source_describes_show_once_the_catalog_is_held_and_offers_no_tool() {
    let c = claimed();
    let ui = Ui::embedded();
    let screen = ui.screen();
    // A later message names the catalog without carrying it: only a catalog this process holds can
    // describe `show`, and nobody is asked for one.
    let named_only = SourceCtx::detached(object(json!({CONTEXT_UI_REF: reference(&c)})));
    let fresh = || ui.tools().into_iter().map(|t| t.spec()).collect::<Vec<_>>();

    assert!(screen.specs(&named_only).await.is_empty());
    assert!(
        screen
            .call(&ctx("show", inline_context(&c)), "show", json!({}))
            .await
            .is_none()
    );

    let mut before = fresh();
    screen.refine(&named_only, &mut before).await;
    assert!(!show_description(&before).contains("The components of this screen"));

    // A message that carried it inline is read by a tool; from then on the process holds it.
    ui.tools()
        .get("ui_catalog")
        .cloned()
        .unwrap()
        .call(&ctx("ui_catalog", inline_context(&c)), json!({}))
        .await
        .unwrap();
    let mut after = fresh();
    screen.refine(&named_only, &mut after).await;
    let described = show_description(&after);
    assert!(
        described.contains("The components of this screen:\n- "),
        "{described}"
    );
    assert!(
        described.contains("\n- Choices: a form of questions"),
        "{described}"
    );
}

#[tokio::test]
async fn the_screen_source_names_the_mentioned_agents() {
    let screen = Ui::embedded().screen();
    let none = SourceCtx::detached(Map::new());
    assert!(screen.instructions(&none).await.is_none());

    let mentioned = SourceCtx::detached(object(json!({
        CONTEXT_MENTIONS: {"mentions": [{"agentId": "researcher", "label": "@researcher", "name": "Researcher"}]},
    })));
    let block = screen.instructions(&mentioned).await.unwrap();

    assert!(block.contains("agentId \"researcher\""), "{block}");
    assert!(block.contains("named \"Researcher\""), "{block}");
}
