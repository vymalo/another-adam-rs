//! `show` against version 3 of the web's catalog (`Cards` and `Mermaid` beside `Text`, `Column` and
//! `Choices`), copied into `tests/fixtures` with its lock and digest pinned: what a researcher draws
//! (a few lines, the sources as cards, how they relate as a graph) validates against the schemas the
//! screen validates with, and what breaks them is refused with a reason the model can act on.
#![allow(clippy::unwrap_used, clippy::expect_used)] // integration tests assert by unwrapping

use std::sync::Arc;

use adam_a2a_runtime::{CONTEXT_UI_CATALOG, CONTEXT_UI_REF};
use adam_llm_agent::{ToolCtx, ToolOutput};
use adam_mcp::McpPolicy;
use adam_runtime::NoopSink;
use adam_ui::{Catalog, Claimed, Ui, catalog_digest};
use serde_json::{Map, Value, json};

const CATALOG_ID: &str = "https://agents.vymalo.com/a2ui/catalogs/chat";
const FIXTURE: &str = include_str!("fixtures/catalog-v3.json");
const LOCK: &str = include_str!("fixtures/catalog-v3.lock.json");

/// The digest of version 3 of the web's catalog, pinned here as well as in the lock: a change to the
/// fixture must be a deliberate one, made together with the web's `catalog.lock.json`.
const PINNED_DIGEST: &str =
    "sha256:9f65f9e6ddd424688b1cf61c47634eafee321a3b6736fb7fea8c0f7db1fc7579";

fn document() -> Value {
    serde_json::from_str(FIXTURE).unwrap()
}

fn claimed() -> Claimed {
    let lock: Value = serde_json::from_str(LOCK).unwrap();
    Claimed {
        catalog_id: CATALOG_ID.to_owned(),
        version: u32::try_from(lock["version"].as_u64().unwrap()).unwrap(),
        digest: lock["digest"].as_str().unwrap().to_owned(),
    }
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
fn inline_context() -> Map<String, Value> {
    let c = claimed();
    json!({
        CONTEXT_UI_REF: {"catalogId": c.catalog_id, "version": c.version, "digest": c.digest},
        CONTEXT_UI_CATALOG: {
            "catalogId": c.catalog_id, "version": c.version, "digest": c.digest,
            "catalog": as_doubles(&document())},
    })
    .as_object()
    .cloned()
    .unwrap()
}

fn ctx(name: &str, call_id: &str) -> ToolCtx {
    ToolCtx::detached(name, call_id, Arc::new(NoopSink)).with_context(inline_context())
}

/// One `show` call on the inline version-3 catalog.
async fn show(call_id: &str, args: Value) -> ToolOutput {
    let ui = Ui::new(McpPolicy::default());
    let tool = ui.tools().get("show").cloned().unwrap();
    tool.call(&ctx("show", call_id), args)
        .await
        .expect("an output, not an error")
}

/// What a researcher shows for "what is async Rust?": three sources as cards, and how the ideas relate.
fn researcher_blocks() -> Value {
    json!([
        {"component": "Text", "text": "Three sources agree: async Rust is built on futures and an executor."},
        {"component": "Cards", "title": "Sources", "layout": "list", "cards": [
            {"title": "The Rust language", "subtitle": "example.org",
             "body": "A language empowering everyone to build reliable and efficient software.",
             "url": "https://example.org/mock-search/1", "tags": ["rust", "language"]},
            {"title": "Async in Rust", "subtitle": "example.org",
             "body": "How futures work: a future does nothing until it is polled.",
             "url": "https://example.org/mock-search/2", "tags": ["async"]},
            {"title": "Tokio", "url": "https://example.org/mock-search/3"}
        ]},
        {"component": "Mermaid", "title": "How it fits together",
         "code": "graph TD\n  Future --> Executor\n  Executor --> Waker\n  Waker --> Future",
         "caption": "A future is polled by an executor until it is ready."}
    ])
}

// ---- the model is told what the screen can draw, and that a form is not drawn with `show` ----

/// The specs of the three UI tools, as the agent offers them, then refined by the UI's source for
/// the conversation whose context is `context`.
async fn offered_with(context: Map<String, Value>) -> Vec<adam_model::ToolSpec> {
    use adam_llm_agent::{SourceCtx, ToolSource as _};
    let ui = Ui::new(McpPolicy::default());
    let mut specs: Vec<_> = ui.tools().into_iter().map(|t| t.spec()).collect();
    ui.source()
        .refine(&SourceCtx::detached(context), &mut specs)
        .await;
    specs
}

fn description<'a>(specs: &'a [adam_model::ToolSpec], name: &str) -> &'a str {
    &specs.iter().find(|s| s.name == name).unwrap().description
}

#[tokio::test]
async fn show_is_described_with_the_components_of_the_screen_it_draws_on() {
    let specs = offered_with(inline_context()).await;
    let show = description(&specs, "show");
    // Version 3 of the catalog: every component by name, and what it is for.
    for name in ["Cards", "Choices", "Column", "Mermaid", "Text"] {
        assert!(show.contains(&format!("\n- {name}")), "{name}: {show}");
    }
    assert!(show.contains("\n- Cards: "), "{show}");
    // A form is not for `show`, whatever the catalog says of it.
    assert!(
        show.contains("\n- Choices: a form of questions: not for `show`, ask with `ask_user`"),
        "{show}"
    );
    assert!(show.contains("`ui_catalog`"), "{show}");
    assert!(
        show.contains("Never use it to ask the person something"),
        "{show}"
    );
    // The names and the schemas are untouched, and so is every other tool.
    assert_eq!(
        specs.iter().map(|s| s.name.as_str()).collect::<Vec<_>>(),
        ["ask_user", "show", "ui_catalog"]
    );
    let plain = offered_with(Map::new()).await;
    assert_eq!(
        specs.iter().find(|s| s.name == "show").unwrap().parameters,
        plain.iter().find(|s| s.name == "show").unwrap().parameters
    );
    assert_eq!(
        description(&specs, "ui_catalog"),
        description(&plain, "ui_catalog")
    );
    assert_eq!(
        description(&specs, "ask_user"),
        description(&plain, "ask_user")
    );
}

#[tokio::test]
async fn show_keeps_its_own_description_when_the_screen_sent_no_catalog() {
    let specs = offered_with(Map::new()).await;
    let show = description(&specs, "show");
    assert!(
        show.starts_with("Show the person something on their screen"),
        "{show}"
    );
    assert!(!show.contains("The components of this screen"), "{show}");
    assert!(show.contains("Call `ui_catalog` first"), "{show}");
}

#[tokio::test]
async fn the_list_of_components_is_one_short_line_each_and_bounded() {
    // Sixty-four components with long descriptions: each is one line of its first sentence, and the
    // list stops where a screenful would end, saying how many it left out.
    let components: Map<String, Value> = (0..64)
        .map(|n| {
            (
                format!("Component{n:02}"),
                json!({"type": "object", "description": format!(
                    "First sentence of {n}. {}", "More detail that the model does not need here. ".repeat(20))}),
            )
        })
        .collect();
    let document = json!({"catalogId": CATALOG_ID, "components": components});
    let digest = catalog_digest(&document).unwrap();
    let c = Claimed {
        catalog_id: CATALOG_ID.to_owned(),
        version: 9,
        digest,
    };
    let catalog = Catalog::from_document(document.clone(), &c).unwrap();
    let context = json!({
        CONTEXT_UI_REF: {"catalogId": c.catalog_id, "version": c.version, "digest": c.digest},
        CONTEXT_UI_CATALOG: {
            "catalogId": c.catalog_id, "version": c.version, "digest": c.digest,
            "catalog": as_doubles(&document)},
    })
    .as_object()
    .cloned()
    .unwrap();
    assert_eq!(catalog.components().len(), 64);
    let specs = offered_with(context).await;
    let show = description(&specs, "show");
    assert!(
        show.contains("\n- Component00: First sentence of 0."),
        "{show}"
    );
    assert!(!show.contains("More detail"), "{show}");
    assert!(show.contains("more: see `ui_catalog`"), "{show}");
    assert!(show.len() < 3 * 1024, "{} bytes", show.len());
}

#[tokio::test]
async fn a_choices_block_is_refused_because_a_form_drawn_by_show_would_be_dead() {
    let refused = show(
        "c1",
        json!({"blocks": [
            {"component": "Text", "text": "Pick one:"},
            {"component": "Choices", "questions": [
                {"id": "db", "question": "Which database?",
                 "options": [{"value": "pg", "label": "Postgres"}, {"value": "my", "label": "MySQL"}]}],
             "action": {"event": {"name": "choices.answer"}}}]}),
    )
    .await;
    assert!(refused.is_error, "{}", refused.content);
    assert!(
        refused.content.contains("block 2 is a `Choices` form"),
        "{}",
        refused.content
    );
    assert!(refused.content.contains("dead form"), "{}", refused.content);
    assert!(
        refused.content.contains("`ask_user`"),
        "{}",
        refused.content
    );
    assert!(refused.artifacts.is_empty(), "nothing was drawn");
    // Alone, it is refused as well; the other components are drawn as before.
    let alone = show(
        "c2",
        json!({"blocks": [{"component": "Choices", "questions": [], "action": {"event": {"name": "x"}}}]}),
    )
    .await;
    assert!(
        alone.is_error && alone.content.contains("`Choices` form"),
        "{}",
        alone.content
    );
    let fine = show(
        "c3",
        json!({"blocks": [{"component": "Text", "text": "hi"}]}),
    )
    .await;
    assert!(!fine.is_error, "{}", fine.content);
}

// ---- the fixture is the web's catalog, version 3 ----

#[test]
fn version_three_of_the_web_catalog_hashes_to_its_lock_and_every_component_compiles() {
    let c = claimed();
    assert_eq!(c.version, 3);
    assert_eq!(c.digest, PINNED_DIGEST, "the lock is the pinned digest");
    assert_eq!(
        catalog_digest(&document()).unwrap(),
        PINNED_DIGEST,
        "adam's digest of the fixture is the web's"
    );
    let catalog = Catalog::from_document(document(), &c).unwrap();
    assert_eq!(catalog.names(), "Cards, Choices, Column, Mermaid, Text");
    assert_eq!(catalog.version(), 3);
    // The doubles of an A2A message's metadata do not change what the catalog is.
    let doubles = Catalog::from_document(as_doubles(&document()), &c).unwrap();
    assert_eq!(doubles.digest(), PINNED_DIGEST);
    assert_eq!(doubles.document(), catalog.document());
}

#[test]
fn version_three_adds_cards_and_mermaid_to_what_version_two_had() {
    let v2: Value = serde_json::from_str(include_str!("fixtures/catalog-v2.json")).unwrap();
    let v3 = document();
    for name in ["Text", "Column", "Choices"] {
        assert_eq!(
            v3["components"][name], v2["components"][name],
            "{name} is the same schema in both versions"
        );
    }
    assert!(v2["components"].get("Cards").is_none());
    assert!(v3["components"]["Cards"].is_object() && v3["components"]["Mermaid"].is_object());
}

// ---- show: cards and a graph ----

#[tokio::test]
async fn text_cards_and_a_graph_are_one_surface_under_the_screens_catalog() {
    let out = show("research-call-1", json!({"blocks": researcher_blocks()})).await;
    assert!(!out.is_error, "{}", out.content);
    assert_eq!(out.content, "Shown to the person.");
    assert_eq!(out.artifacts.len(), 1);
    let artifact = &out.artifacts[0];
    assert_eq!(artifact.name, "ui");
    assert_eq!(artifact.mime_type.as_deref(), Some("application/a2ui+json"));
    let golden: Value =
        serde_json::from_str(include_str!("golden/show_cards_mermaid.json")).unwrap();
    assert_eq!(artifact.data, golden);

    // The artifact is what the screen validates: every component of the surface passes the schema
    // of version 3 (the same document the screen holds, by digest), and the surface is under its id.
    let catalog = Catalog::from_document(document(), &claimed()).unwrap();
    let messages = artifact.data.as_array().unwrap();
    assert_eq!(messages[0]["createSurface"]["catalogId"], CATALOG_ID);
    let components = messages[1]["updateComponents"]["components"]
        .as_array()
        .unwrap();
    let kinds: Vec<&str> = components
        .iter()
        .map(|c| c["component"].as_str().unwrap())
        .collect();
    assert_eq!(kinds, ["Column", "Text", "Cards", "Mermaid"]);
    for component in components {
        catalog
            .validate(component)
            .unwrap_or_else(|problem| panic!("{component}: {problem}"));
    }
    // The column holds the three blocks in order, and the surface id is stable across a replay.
    assert_eq!(components[0]["children"], json!(["b1", "b2", "b3"]));
    let again = show("research-call-1", json!({"blocks": researcher_blocks()})).await;
    assert_eq!(again.artifacts[0], *artifact);
}

#[tokio::test]
async fn a_single_card_list_or_graph_is_the_root_itself() {
    let cards = show(
        "c1",
        json!({"blocks": [{"component": "Cards", "layout": "grid",
                           "cards": [{"title": "One", "url": "http://example.org/one"}]}]}),
    )
    .await;
    assert!(!cards.is_error, "{}", cards.content);
    let root = &cards.artifacts[0].data[1]["updateComponents"]["components"][0];
    assert_eq!(
        (root["id"].as_str(), root["component"].as_str()),
        (Some("root"), Some("Cards"))
    );

    let graph = show(
        "c2",
        json!({"title": "Flow", "blocks": [{"component": "Mermaid", "code": "graph TD; A-->B"}]}),
    )
    .await;
    assert!(!graph.is_error, "{}", graph.content);
    let components = graph.artifacts[0].data[1]["updateComponents"]["components"]
        .as_array()
        .unwrap();
    let kinds: Vec<&str> = components
        .iter()
        .map(|c| c["component"].as_str().unwrap())
        .collect();
    assert_eq!(
        kinds,
        ["Column", "Text", "Mermaid"],
        "a title makes a column"
    );
}

#[tokio::test]
async fn the_limits_of_cards_and_mermaid_hold_at_the_edge_and_are_refused_beyond_it() {
    let card = |n: usize| json!({"title": format!("Source {n}")});
    // At the limits: 24 cards, 8 tags, a 20000-byte graph, the longest title.
    let at: Vec<Value> = (0..24).map(card).collect();
    let tags: Vec<String> = (0..8).map(|n| format!("t{n}")).collect();
    let longest = "x".repeat(200);
    for blocks in [
        json!([{"component": "Cards", "cards": at}]),
        json!([{"component": "Cards", "cards": [{"title": longest, "tags": tags}]}]),
        json!([{"component": "Mermaid", "code": format!("graph TD; {}", "A".repeat(19_986))}]),
    ] {
        let out = show("edge", json!({"blocks": blocks})).await;
        assert!(!out.is_error, "{}", out.content);
    }
    // Over them.
    let over: Vec<Value> = (0..25).map(card).collect();
    let nine_tags: Vec<String> = (0..9).map(|n| format!("t{n}")).collect();
    for (what, blocks, want) in [
        (
            "25 cards",
            json!([{"component": "Cards", "cards": over}]),
            "(at /cards)",
        ),
        (
            "no cards",
            json!([{"component": "Cards", "cards": []}]),
            "(at /cards)",
        ),
        (
            "9 tags",
            json!([{"component": "Cards", "cards": [{"title": "t", "tags": nine_tags}]}]),
            "(at /cards/0/tags)",
        ),
        (
            "a 201-byte title",
            json!([{"component": "Cards", "cards": [{"title": "x".repeat(201)}]}]),
            "(at /cards/0/title)",
        ),
        (
            "an empty graph",
            json!([{"component": "Mermaid", "code": ""}]),
            "(at /code)",
        ),
        (
            "a graph over 20000 bytes",
            json!([{"component": "Mermaid", "code": "A".repeat(20_001)}]),
            "(at /code)",
        ),
    ] {
        let out = show("over", json!({"blocks": blocks})).await;
        assert!(out.is_error && out.artifacts.is_empty(), "{what} was drawn");
        assert!(
            out.content.contains("block 1") && out.content.contains(want),
            "{what}: {}",
            out.content
        );
    }
}

#[tokio::test]
async fn a_card_without_a_title_or_with_a_link_that_is_not_http_is_refused_with_its_place() {
    let missing = show(
        "c1",
        json!({"blocks": [{"component": "Text", "text": "Sources:"},
                          {"component": "Cards", "cards": [{"title": "ok"}, {"subtitle": "no title"}]}]}),
    )
    .await;
    assert!(missing.is_error && missing.artifacts.is_empty());
    assert!(
        missing.content.starts_with("block 2 (Cards):")
            && missing.content.contains("\"title\" is a required property")
            && missing.content.contains("(at /cards/1)"),
        "{}",
        missing.content
    );

    for url in [
        "javascript:alert(1)",
        "ftp://example.org/file",
        "//example.org/protocol-relative",
        "example.org/no-scheme",
        "data:text/html,hi",
    ] {
        let out = show(
            "c2",
            json!({"blocks": [{"component": "Cards", "cards": [{"title": "t", "url": url}]}]}),
        )
        .await;
        assert!(out.is_error && out.artifacts.is_empty(), "{url} was drawn");
        assert!(
            out.content.starts_with("block 1 (Cards):")
                && out.content.contains("(at /cards/0/url)"),
            "{url}: {}",
            out.content
        );
    }
    // https is as good as http.
    let ok = show(
        "c3",
        json!({"blocks": [{"component": "Cards",
                           "cards": [{"title": "t", "url": "https://example.org/a?b=c#d"}]}]}),
    )
    .await;
    assert!(!ok.is_error, "{}", ok.content);
}

#[tokio::test]
async fn properties_the_components_do_not_have_are_refused_so_the_model_corrects_them() {
    for (blocks, mention) in [
        // `links` instead of `url`, a card's `image`, a graph's `theme`: not in the schema.
        (
            json!([{"component": "Cards", "cards": [{"title": "t", "image": "https://example.org/i.png"}]}]),
            "image",
        ),
        (
            json!([{"component": "Mermaid", "code": "graph TD; A-->B", "theme": "dark"}]),
            "theme",
        ),
        (
            json!([{"component": "Cards", "cards": [{"title": "t"}], "layout": "carousel"}]),
            "carousel",
        ),
    ] {
        let out = show("c1", json!({"blocks": blocks})).await;
        assert!(out.is_error, "drawn: {blocks}");
        assert!(out.content.contains(mention), "{}", out.content);
    }
}

#[tokio::test]
async fn ui_catalog_lists_cards_and_mermaid_with_what_they_are_for() {
    let ui = Ui::new(McpPolicy::default());
    let tool = ui.tools().get("ui_catalog").cloned().unwrap();
    let out = tool
        .call(&ctx("ui_catalog", "c1"), json!({}))
        .await
        .expect("an output");
    assert!(!out.is_error);
    let described: Value = serde_json::from_str(&out.content).unwrap();
    assert_eq!(described["version"], 3);
    assert_eq!(described["digest"], PINNED_DIGEST);
    let by_name = |name: &str| {
        described["components"]
            .as_array()
            .unwrap()
            .iter()
            .find(|c| c["name"] == name)
            .unwrap_or_else(|| panic!("{name} is not listed"))
            .clone()
    };
    let cards = by_name("Cards");
    assert!(
        cards["description"]
            .as_str()
            .unwrap()
            .contains("up to 24 cards")
    );
    assert_eq!(cards["schema"]["properties"]["cards"]["maxItems"], 24);
    let mermaid = by_name("Mermaid");
    assert!(
        mermaid["description"]
            .as_str()
            .unwrap()
            .contains("mermaid diagram")
    );
    assert_eq!(mermaid["schema"]["properties"]["code"]["maxLength"], 20_000);
}
