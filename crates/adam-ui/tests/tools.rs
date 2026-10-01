//! `ask_user`, `show` and `ui_catalog` against the web's real catalog (version 2, copied into
//! `tests/fixtures`, digest pinned), with the catalog arriving inline, from the cache, or read
//! again from a fake thread-tools endpoint; and what each does when it cannot have one.
#![allow(clippy::unwrap_used, clippy::expect_used)] // integration tests assert by unwrapping

use std::sync::Arc;

use adam_a2a_runtime::{CONTEXT_THREAD_TOOLS, CONTEXT_UI_CATALOG, CONTEXT_UI_REF};
use adam_llm_agent::{ToolCtx, ToolError, ToolOutput};
use adam_mcp::McpPolicy;
use adam_mcp_testkit::ThreadToolsServer;
use adam_runtime::NoopSink;
use adam_ui::{Catalog, Claimed, Ui, catalog_digest};
use serde_json::{Map, Value, json};

const CATALOG_ID: &str = "https://agents.vymalo.com/a2ui/catalogs/chat";
const FIXTURE: &str = include_str!("fixtures/catalog-v2.json");
const LOCK: &str = include_str!("fixtures/catalog-v2.lock.json");

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

fn reference(c: &Claimed) -> Value {
    json!({"catalogId": c.catalog_id, "version": c.version, "digest": c.digest})
}

/// The context of a message that carried the catalog inline.
fn inline_context(document: &Value, c: &Claimed) -> Map<String, Value> {
    json!({
        CONTEXT_UI_REF: reference(c),
        CONTEXT_UI_CATALOG: {
            "catalogId": c.catalog_id, "version": c.version, "digest": c.digest,
            "catalog": as_doubles(document)},
    })
    .as_object()
    .cloned()
    .unwrap()
}

/// The context of a message that only said which catalog is current, with a grant for the endpoint.
fn ref_context(c: &Claimed, grant: Option<(&str, &str, &str)>) -> Map<String, Value> {
    let mut context = json!({CONTEXT_UI_REF: reference(c)})
        .as_object()
        .cloned()
        .unwrap();
    if let Some((url, token, expires_at)) = grant {
        context.insert(
            CONTEXT_THREAD_TOOLS.into(),
            json!({"url": url, "token": token, "expiresAt": expires_at}),
        );
    }
    context
}

fn ctx(name: &str, call_id: &str, context: Map<String, Value>) -> ToolCtx {
    ToolCtx::detached(name, call_id, Arc::new(NoopSink)).with_context(context)
}

fn ui() -> Ui {
    Ui::new(McpPolicy::default())
}

fn tool(ui: &Ui, name: &str) -> adam_llm_agent::DynTool {
    ui.tools().get(name).cloned().unwrap()
}

/// The three questions of the dev scripts: a database, a login, where it runs.
fn three_questions() -> Value {
    json!({
        "question": "Three quick questions before I start",
        "choices": [
            {"id": "db", "question": "Which database?",
             "options": [{"value": "pg", "label": "Postgres"}, {"value": "sqlite", "label": "SQLite"}]},
            {"id": "auth", "question": "Which login?",
             "options": [{"value": "keycloak", "label": "Keycloak"}, {"value": "none", "label": "No login"}]},
            {"id": "deploy", "question": "Where does it run?",
             "options": [{"value": "k8s", "label": "Kubernetes"}, {"value": "compose", "label": "Docker Compose"}]}
        ]
    })
}

fn future() -> &'static str {
    "2999-01-01T00:00:00Z"
}

fn needs_input(result: Result<ToolOutput, ToolError>) -> (String, Option<Value>) {
    match result {
        Err(ToolError::NeedsInput { question, ui }) => (question, ui),
        other => panic!("expected a question, got {other:?}"),
    }
}

fn text_of(result: Result<ToolOutput, ToolError>) -> ToolOutput {
    result.expect("an output, not an error")
}

// ---- the fixture is the web's catalog ----

#[test]
fn the_web_catalog_hashes_to_its_lock_and_every_component_compiles() {
    let c = claimed();
    assert_eq!(
        catalog_digest(&document()).unwrap(),
        c.digest,
        "adam's digest is the web's"
    );
    let catalog = Catalog::from_document(document(), &c).unwrap();
    assert_eq!(catalog.names(), "Choices, Column, Text");
    assert_eq!(catalog.version(), 2);
    assert!(
        catalog
            .component("Choices")
            .unwrap()
            .description()
            .contains("Ask the person")
    );
    // Even as doubles, the catalog is the same catalog.
    let doubles = Catalog::from_document(as_doubles(&document()), &c).unwrap();
    assert_eq!(doubles.digest(), c.digest);
    assert_eq!(doubles.document(), catalog.document());
}

// ---- ask_user ----

#[tokio::test]
async fn a_plain_question_parks_with_the_question_and_no_interface() {
    let ui = ui();
    let ask = tool(&ui, "ask_user");
    assert!(ask.asks_user());
    let (question, interface) = needs_input(
        ask.call(
            &ctx("ask_user", "c1", Map::new()),
            json!({"question": " Which city? "}),
        )
        .await,
    );
    assert_eq!((question.as_str(), interface), ("Which city?", None));
    for args in [json!({"question": "  "}), json!({}), Value::Null] {
        let out = text_of(
            ask.call(&ctx("ask_user", "c1", Map::new()), args.clone())
                .await,
        );
        assert!(out.is_error, "{args}");
    }
}

#[tokio::test]
async fn three_questions_become_one_choices_surface_under_the_screens_catalog() {
    let ui = ui();
    let context = inline_context(&document(), &claimed());
    let (question, interface) = needs_input(
        tool(&ui, "ask_user")
            .call(
                &ctx("ask_user", "choices-call-1", context),
                three_questions(),
            )
            .await,
    );
    assert_eq!(question, "Three quick questions before I start");
    let golden: Value = serde_json::from_str(include_str!("golden/ask_choices.json")).unwrap();
    assert_eq!(interface, Some(golden));
    assert_eq!(ui.cache().len(), 1, "the inline catalog is kept");
}

#[tokio::test]
async fn options_given_as_plain_labels_get_slug_values_and_the_answer_names_them() {
    let ui = ui();
    let (_, interface) = needs_input(
        tool(&ui, "ask_user")
            .call(
                &ctx("ask_user", "c9", inline_context(&document(), &claimed())),
                json!({"question": "Pick", "choices": [
                    {"question": "Database?", "options": ["Postgres", "SQLite"], "allowOther": true, "multiple": false}]}),
            )
            .await,
    );
    let root = &interface.unwrap()[1]["updateComponents"]["components"][0];
    assert_eq!(root["questions"][0]["id"], "q1");
    assert_eq!(
        root["questions"][0]["options"],
        json!([{"value": "postgres", "label": "Postgres"}, {"value": "sqlite", "label": "SQLite"}])
    );
    assert_eq!(root["questions"][0]["allowOther"], true);
}

#[tokio::test]
async fn choices_that_break_the_components_schema_come_back_to_the_model_with_the_place() {
    let ui = ui();
    let context = inline_context(&document(), &claimed());
    let long = "x".repeat(400);
    let out = text_of(
        tool(&ui, "ask_user")
            .call(
                &ctx("ask_user", "c1", context.clone()),
                json!({"question": "q", "choices": [{"question": long, "options": ["a", "b"]}]}),
            )
            .await,
    );
    assert!(out.is_error);
    assert!(
        out.content
            .contains("do not fit the screen's Choices component"),
        "{}",
        out.content
    );
    assert!(
        out.content.contains("/questions/0/question"),
        "{}",
        out.content
    );
    // An explicit value that is not an identifier.
    let out = text_of(
        tool(&ui, "ask_user")
            .call(
                &ctx("ask_user", "c1", context),
                json!({"question": "q", "choices": [{"question": "r", "options": [{"value": "has space", "label": "A"}, "B"]}]}),
            )
            .await,
    );
    assert!(
        out.is_error && out.content.contains("/options/0/value"),
        "{}",
        out.content
    );
}

#[tokio::test]
async fn on_a_screen_that_cannot_draw_it_the_options_go_in_the_text() {
    let ui = ui();
    let ask = tool(&ui, "ask_user");
    let expected = "Three quick questions before I start\n\
         \n1. Which database?\n   a) Postgres\n   b) SQLite\
         \n2. Which login?\n   a) Keycloak\n   b) No login\
         \n3. Where does it run?\n   a) Kubernetes\n   b) Docker Compose";
    // No catalog at all.
    let (question, interface) = needs_input(
        ask.call(&ctx("ask_user", "c1", Map::new()), three_questions())
            .await,
    );
    assert_eq!((question.as_str(), interface), (expected, None));
    // A catalog without a Choices component (an older screen).
    let text_only =
        json!({"catalogId": CATALOG_ID, "components": {"Text": document()["components"]["Text"]}});
    let c = Claimed {
        catalog_id: CATALOG_ID.into(),
        version: 1,
        digest: catalog_digest(&text_only).unwrap(),
    };
    let (question, interface) = needs_input(
        ask.call(
            &ctx("ask_user", "c1", inline_context(&text_only, &c)),
            three_questions(),
        )
        .await,
    );
    assert_eq!((question.as_str(), interface), (expected, None));
    // A catalog that cannot be read now: the current one is not held and there is no grant.
    let (question, interface) = needs_input(
        ask.call(
            &ctx("ask_user", "c1", ref_context(&claimed(), None)),
            three_questions(),
        )
        .await,
    );
    assert_eq!((question.as_str(), interface), (expected, None));
}

// ---- refetch ----

async fn endpoint_with_catalog(document: &Value, c: &Claimed) -> ThreadToolsServer {
    let server = ThreadToolsServer::start(&["tok"]).await;
    server.set_catalog(Some((
        &c.catalog_id,
        u64::from(c.version),
        &c.digest,
        document.clone(),
    )));
    server
}

#[tokio::test]
async fn a_stale_digest_is_read_again_once_and_then_kept() {
    let c = claimed();
    let server = endpoint_with_catalog(&document(), &c).await;
    let ui = ui();
    let ask = tool(&ui, "ask_user");
    let context = ref_context(&c, Some((&server.url("t1"), "tok", future())));

    for call in ["c1", "c2"] {
        let (_, interface) = needs_input(
            ask.call(&ctx("ask_user", call, context.clone()), three_questions())
                .await,
        );
        let interface = interface.expect("the catalog was read, so there is a form");
        assert_eq!(interface[0]["createSurface"]["catalogId"], CATALOG_ID);
        assert_eq!(
            interface[0]["createSurface"]["surfaceId"],
            format!("ask-{call}")
        );
    }
    assert_eq!(
        server.catalog_requests(),
        [None],
        "one get_ui_catalog, with nothing to say it is unchanged; the second call used the cache"
    );
    assert_eq!(ui.cache().len(), 1);
    // The token went in the header and nowhere else.
    assert!(server.authorizations().iter().all(|a| a == "Bearer tok"));
}

#[tokio::test]
async fn an_inline_catalog_that_is_not_the_current_one_triggers_the_refetch() {
    let c = claimed();
    // The thread moved on: version 3 has one more component, and the message only carries the old
    // catalog inline (an agent that restarted holds none).
    let mut newer = document();
    newer["components"]["Note"] = json!({
        "type": "object", "description": "A note.",
        "properties": {"id": {"type": "string"}, "component": {"const": "Note"}, "text": {"type": "string"}},
        "required": ["id", "component", "text"], "additionalProperties": false});
    let newer_claim = Claimed {
        catalog_id: CATALOG_ID.into(),
        version: 3,
        digest: catalog_digest(&newer).unwrap(),
    };
    let server = endpoint_with_catalog(&newer, &newer_claim).await;
    let mut context = inline_context(&document(), &c);
    context.insert(CONTEXT_UI_REF.into(), reference(&newer_claim));
    context.insert(
        CONTEXT_THREAD_TOOLS.into(),
        json!({"url": server.url("t1"), "token": "tok", "expiresAt": future()}),
    );
    let ui = ui();
    let out = text_of(
        tool(&ui, "ui_catalog")
            .call(&ctx("ui_catalog", "c1", context), json!({}))
            .await,
    );
    let described: Value = serde_json::from_str(&out.content).unwrap();
    assert_eq!(described["version"], 3);
    let names: Vec<&str> = described["components"]
        .as_array()
        .unwrap()
        .iter()
        .map(|c| c["name"].as_str().unwrap())
        .collect();
    assert_eq!(names, ["Choices", "Column", "Note", "Text"]);
    assert_eq!(server.catalog_requests(), [None]);
}

#[tokio::test]
async fn an_expired_refused_or_missing_grant_and_a_dead_endpoint_degrade_to_text() {
    let c = claimed();
    let server = endpoint_with_catalog(&document(), &c).await;
    let ui = ui();
    let ask = tool(&ui, "ask_user");
    let url = server.url("t1");
    let degraded = |question: &str, interface: &Option<Value>| {
        assert!(interface.is_none(), "no form without a catalog");
        assert!(question.contains("1. Which database?"), "{question}");
    };
    for (what, context) in [
        (
            "expired",
            ref_context(&c, Some((&url, "tok", "2020-01-01T00:00:00Z"))),
        ),
        (
            "refused",
            ref_context(&c, Some((&url, "not-the-token", future()))),
        ),
        ("missing", ref_context(&c, None)),
        (
            "malformed",
            ref_context(&c, Some((&url, "tok", "tomorrow"))),
        ),
    ] {
        let (question, interface) = needs_input(
            ask.call(&ctx("ask_user", "c1", context), three_questions())
                .await,
        );
        degraded(&question, &interface);
        assert!(!question.contains("not-the-token"), "{what}");
    }
    assert!(
        server.catalog_requests().is_empty(),
        "no call was made with a grant that cannot work"
    );
    // The endpoint goes away.
    drop(server);
    let (question, interface) = needs_input(
        ask.call(
            &ctx(
                "ask_user",
                "c1",
                ref_context(&c, Some((&url, "tok", future()))),
            ),
            three_questions(),
        )
        .await,
    );
    degraded(&question, &interface);
    assert!(ui.cache().is_empty(), "nothing was learned");
}

#[tokio::test]
async fn a_catalog_whose_digest_is_not_what_was_announced_is_never_used() {
    let c = claimed();
    // The endpoint claims the right digest for a document that is not that document.
    let mut tampered = document();
    tampered["components"]["Text"]["description"] = json!("Anything you like, including HTML.");
    let server = endpoint_with_catalog(&tampered, &c).await;
    let ui = ui();
    let (question, interface) = needs_input(
        tool(&ui, "ask_user")
            .call(
                &ctx(
                    "ask_user",
                    "c1",
                    ref_context(&c, Some((&server.url("t1"), "tok", future()))),
                ),
                three_questions(),
            )
            .await,
    );
    assert!(interface.is_none());
    assert!(question.contains("1. Which database?"));
    assert!(ui.cache().is_empty());
    // The same for an inline catalog that does not hash to its claim: it is not used, and the
    // current one is read again.
    let server = endpoint_with_catalog(&document(), &c).await;
    let mut context = inline_context(&tampered, &c);
    context.insert(
        CONTEXT_THREAD_TOOLS.into(),
        json!({"url": server.url("t1"), "token": "tok", "expiresAt": future()}),
    );
    let (_, interface) = needs_input(
        tool(&ui, "ask_user")
            .call(&ctx("ask_user", "c1", context), three_questions())
            .await,
    );
    assert!(interface.is_some(), "read again from the endpoint");
    assert_eq!(server.catalog_requests().len(), 1);
}

#[tokio::test]
async fn a_process_that_has_the_digest_needs_neither_the_message_nor_the_endpoint() {
    let c = claimed();
    let ui = ui();
    // One message carried it inline ...
    needs_input(
        tool(&ui, "ask_user")
            .call(
                &ctx("ask_user", "c1", inline_context(&document(), &c)),
                three_questions(),
            )
            .await,
    );
    // ... a later one, handled by this process again, only names it, and there is no grant.
    let (_, interface) = needs_input(
        tool(&ui, "ask_user")
            .call(
                &ctx("ask_user", "c2", ref_context(&c, None)),
                three_questions(),
            )
            .await,
    );
    assert!(interface.is_some());
}

// ---- show ----

/// One `show` call on a fresh context.
async fn run(
    show: &adam_llm_agent::DynTool,
    context: &Map<String, Value>,
    args: Value,
) -> ToolOutput {
    text_of(show.call(&ctx("show", "c1", context.clone()), args).await)
}

#[tokio::test]
async fn show_draws_blocks_in_a_column_under_a_title_and_emits_the_surface_as_an_artifact() {
    let ui = ui();
    let show = tool(&ui, "show");
    let context = inline_context(&document(), &claimed());
    let args = json!({
        "title": "Greeting",
        "blocks": [{"component": "Text", "text": "Hello"},
                   {"component": "Text", "text": "World", "variant": "caption"}]
    });
    let out = text_of(
        show.call(&ctx("show", "show-call-1", context.clone()), args.clone())
            .await,
    );
    assert!(!out.is_error);
    assert_eq!(out.content, "Shown to the person.");
    assert_eq!(out.artifacts.len(), 1);
    let artifact = &out.artifacts[0];
    assert_eq!(artifact.name, "ui");
    assert_eq!(artifact.mime_type.as_deref(), Some("application/a2ui+json"));
    let golden: Value = serde_json::from_str(include_str!("golden/show_blocks.json")).unwrap();
    assert_eq!(artifact.data, golden);
    // The same call again (a replay of the step) emits the very same surface.
    let again = text_of(show.call(&ctx("show", "show-call-1", context), args).await);
    assert_eq!(again.artifacts[0], *artifact);
}

#[tokio::test]
async fn one_block_and_no_title_is_the_root_itself() {
    let ui = ui();
    let out = text_of(
        tool(&ui, "show")
            .call(
                &ctx("show", "c1", inline_context(&document(), &claimed())),
                json!({"blocks": [{"component": "Text", "text": "Only"}]}),
            )
            .await,
    );
    let components = &out.artifacts[0].data[1]["updateComponents"]["components"];
    assert_eq!(
        components,
        &json!([{"id": "root", "component": "Text", "text": "Only"}])
    );
}

#[tokio::test]
async fn a_block_the_catalog_refuses_is_explained_to_the_model() {
    let ui = ui();
    let show = tool(&ui, "show");
    let context = inline_context(&document(), &claimed());

    let unknown = run(
        &show,
        &context,
        json!({"blocks": [{"component": "Cards", "cards": []}]}),
    )
    .await;
    assert!(unknown.is_error && unknown.artifacts.is_empty());
    assert!(
        unknown
            .content
            .contains("`Cards` is not a component of this screen")
            && unknown.content.contains("Choices, Column, Text"),
        "{}",
        unknown.content
    );
    let broken = run(
        &show,
        &context,
        json!({"blocks": [{"component": "Text", "text": "ok"}, {"component": "Text"}]}),
    )
    .await;
    assert!(
        broken.is_error && broken.content.starts_with("block 2 (Text):"),
        "{}",
        broken.content
    );
    let extra = run(
        &show,
        &context,
        json!({"blocks": [{"component": "Text", "text": "ok", "color": "red"}]}),
    )
    .await;
    assert!(
        extra.is_error && extra.content.contains("block 1 (Text)"),
        "{}",
        extra.content
    );
    let none = run(
        &show,
        &context,
        json!({"blocks": [{"text": "no component"}]}),
    )
    .await;
    assert!(
        none.content.contains("block 1 has no `component`"),
        "{}",
        none.content
    );
    let not_object = run(&show, &context, json!({"blocks": ["text"]})).await;
    assert!(not_object.content.contains("block 1 is not an object"));
    let empty = run(&show, &context, json!({"blocks": []})).await;
    assert!(empty.content.contains("needs 1 to 16"));
    let many: Vec<Value> = (0..17)
        .map(|_| json!({"component": "Text", "text": "x"}))
        .collect();
    assert!(
        run(&show, &context, json!({"blocks": many}))
            .await
            .content
            .contains("17 entries")
    );
    let bad_args = run(&show, &context, json!({"title": "x"})).await;
    assert!(bad_args.is_error && bad_args.content.contains("invalid arguments for `show`"));
}

#[tokio::test]
async fn show_and_ui_catalog_say_to_answer_in_text_when_there_is_no_catalog_to_read() {
    let ui = ui();
    for (what, context, expected) in [
        (
            "none",
            Map::new(),
            "this screen has no component catalog; answer in text",
        ),
        (
            "unreadable",
            ref_context(&claimed(), None),
            "the screen's components could not be read; answer in text",
        ),
    ] {
        let shown = text_of(
            tool(&ui, "show")
                .call(
                    &ctx("show", "c1", context.clone()),
                    json!({"blocks": [{"component": "Text", "text": "x"}]}),
                )
                .await,
        );
        assert!(
            shown.is_error && shown.content == expected,
            "{what}: {}",
            shown.content
        );
        assert!(shown.artifacts.is_empty());
        let listed = text_of(
            tool(&ui, "ui_catalog")
                .call(&ctx("ui_catalog", "c1", context), json!({}))
                .await,
        );
        assert!(
            listed.is_error && listed.content == expected,
            "{what}: {}",
            listed.content
        );
    }
}

// ---- ui_catalog ----

#[tokio::test]
async fn ui_catalog_gives_each_component_with_its_description_and_schema() {
    let ui = ui();
    let out = text_of(
        tool(&ui, "ui_catalog")
            .call(
                &ctx("ui_catalog", "c1", inline_context(&document(), &claimed())),
                json!({}),
            )
            .await,
    );
    assert!(!out.is_error);
    let described: Value = serde_json::from_str(&out.content).unwrap();
    assert_eq!(described["catalogId"], CATALOG_ID);
    assert_eq!(described["digest"], claimed().digest);
    let choices = &described["components"][0];
    assert_eq!(choices["name"], "Choices");
    assert_eq!(
        choices["schema"]["properties"]["component"]["const"],
        "Choices"
    );
    assert!(
        choices["description"]
            .as_str()
            .unwrap()
            .contains("up to 8 questions")
    );
}

#[test]
fn the_tool_set_and_the_card_entries_are_the_documented_ones() {
    let ui = ui();
    assert_eq!(ui.tools().names(), ["ask_user", "show", "ui_catalog"]);
    let uris: Vec<String> = adam_ui::card_extensions()
        .into_iter()
        .map(|e| e.uri)
        .collect();
    assert_eq!(
        uris,
        [
            "https://a2ui.org/a2a-extension/a2ui/v0.9.1",
            "https://agents.vymalo.com/a2a/extensions/ui-catalog/v1",
            "https://agents.vymalo.com/a2a/extensions/thread-tools/v1",
        ]
    );
    let card = adam_ui::with_card_extensions(adam_a2a::AgentCardConfig::new(
        "a",
        "d",
        "http://localhost/".parse().unwrap(),
        "1",
    ));
    assert_eq!(card.extensions.len(), 3);
    assert!(format!("{ui:?}").contains("cached_catalogs"));
}
