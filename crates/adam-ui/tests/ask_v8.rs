//! `ask_user` with a question that shows what it asks about (catalog version 8, `show`), against the
//! published catalogs of versions 8, 7, 5 and 2 (byte copies in `tests/fixtures`, see `UPSTREAM`):
//! drawn with the question where the screen's `Choices` takes `show`, above the form where it does
//! not, in words where the screen cannot draw the component, and the same things shown again when a
//! refused answer asks the form again.
#![allow(clippy::unwrap_used, clippy::expect_used)] // integration tests assert by unwrapping

use std::path::PathBuf;
use std::sync::Arc;

use adam_a2a_runtime::{CONTEXT_A2UI_RENDERER, CONTEXT_UI_CATALOG, CONTEXT_UI_REF};
use adam_llm_agent::{AnswerCheck, ToolCtx, ToolError, ToolOutput};
use adam_mcp::McpPolicy;
use adam_runtime::NoopSink;
use adam_ui::{Catalog, Claimed, Ui, catalog_digest};
use serde_json::{Map, Value, json};

const CATALOG_ID: &str = "https://agents.vymalo.com/a2ui/catalogs/chat";
const V8: &str = include_str!("fixtures/catalog-v8.json");
const V8_LOCK: &str = include_str!("fixtures/catalog-v8.lock.json");
const V7: &str = include_str!("fixtures/catalog-v7.json");
const V7_LOCK: &str = include_str!("fixtures/catalog-v7.lock.json");
const V5: &str = include_str!("fixtures/catalog-v5.json");
const V5_LOCK: &str = include_str!("fixtures/catalog-v5.lock.json");
const V2: &str = include_str!("fixtures/catalog-v2.json");
const V2_LOCK: &str = include_str!("fixtures/catalog-v2.lock.json");
const UPSTREAM: &str = include_str!("fixtures/UPSTREAM");
const VECTORS: &str = include_str!("fixtures/choices-v8-vectors.json");

/// The number of cases of `v8-surfaces` the contract page says it holds.
const CASES: usize = 46;

struct Screen {
    document: Value,
    claimed: Claimed,
}

fn screen(document: &str, lock: &str) -> Screen {
    let lock: Value = serde_json::from_str(lock).unwrap();
    // The published catalogs of versions 2 to 8 have no `protocolVersion`; the screen sends them as
    // `ui-catalog/v2` documents (the digest is of what is sent).
    let mut document: Value = serde_json::from_str(document).unwrap();
    document["protocolVersion"] = json!("1.0");
    Screen {
        claimed: Claimed {
            catalog_id: CATALOG_ID.to_owned(),
            version: u32::try_from(lock["version"].as_u64().unwrap()).unwrap(),
            digest: adam_ui::catalog_digest(&document).unwrap(),
        },
        document,
    }
}

fn v8() -> Screen {
    screen(V8, V8_LOCK)
}
fn v7() -> Screen {
    screen(V7, V7_LOCK)
}
fn v5() -> Screen {
    screen(V5, V5_LOCK)
}
fn v2() -> Screen {
    screen(V2, V2_LOCK)
}

fn catalog_of(screen: &Screen) -> Catalog {
    Catalog::from_document(screen.document.clone(), &screen.claimed).unwrap()
}

fn context(screen: &Screen) -> Map<String, Value> {
    let c = &screen.claimed;
    json!({
        CONTEXT_UI_REF: {"catalogId": c.catalog_id, "version": c.version, "digest": c.digest},
        CONTEXT_A2UI_RENDERER: {"catalogIds": [c.catalog_id]},
        CONTEXT_UI_CATALOG: {
            "catalogId": c.catalog_id, "version": c.version, "digest": c.digest,
            "catalog": screen.document},
    })
    .as_object()
    .cloned()
    .unwrap()
}

fn ask_tool() -> adam_llm_agent::DynTool {
    Ui::new(McpPolicy::default())
        .tools()
        .get("ask_user")
        .cloned()
        .unwrap()
}

async fn ask(screen: Option<&Screen>, args: Value) -> Result<ToolOutput, ToolError> {
    let context = screen.map(context).unwrap_or_default();
    ask_tool()
        .call(
            &ToolCtx::detached("ask_user", "call-1", Arc::new(NoopSink)).with_context(context),
            args,
        )
        .await
}

/// The question and the messages of a call that parked with a form.
async fn form(screen: &Screen, args: Value) -> (String, Value) {
    match ask(Some(screen), args).await {
        Err(ToolError::NeedsInput {
            question,
            ui: Some(ui),
        }) => (question, ui),
        other => panic!("expected a question with a form, got {other:?}"),
    }
}

async fn words(screen: Option<&Screen>, args: Value) -> String {
    match ask(screen, args).await {
        Err(ToolError::NeedsInput { question, ui: None }) => question,
        other => panic!("expected a question in words, got {other:?}"),
    }
}

async fn refused(screen: Option<&Screen>, args: Value) -> String {
    match ask(screen, args).await {
        Ok(out) if out.is_error => out.content,
        other => panic!("expected an error result, got {other:?}"),
    }
}

fn components(ui: &Value) -> &Vec<Value> {
    ui[1]["updateComponents"]["components"].as_array().unwrap()
}

fn component<'a>(ui: &'a Value, id: &str) -> &'a Value {
    components(ui)
        .iter()
        .find(|c| c["id"] == id)
        .unwrap_or_else(|| panic!("no component `{id}` in {ui}"))
}

fn choices(ui: &Value) -> &Value {
    components(ui)
        .iter()
        .find(|c| c["component"] == "Choices")
        .unwrap()
}

fn golden(name: &str, got: &Value) {
    let path = PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .join("tests/golden")
        .join(name);
    if std::env::var_os("ADAM_UPDATE_SNAPSHOTS").is_some() {
        std::fs::write(&path, serde_json::to_string_pretty(got).unwrap() + "\n").unwrap();
    }
    let want: Value = serde_json::from_str(&std::fs::read_to_string(&path).unwrap()).unwrap();
    assert_eq!(
        got, &want,
        "{name}: regenerate with ADAM_UPDATE_SNAPSHOTS=1"
    );
}

const PICTURE: &str = "a1a1a1a1a1a1a1a1a1a1a1a1a1a1a1a1a1a1a1a1a1a1a1a1a1a1a1a1a1a1a1a1";
const DIFF: &str =
    "--- a/src/lib.rs\n+++ b/src/lib.rs\n@@ -1,2 +1,2 @@\n-let keep = true;\n+let keep = false;\n";

/// A yes or no that shows the diff it is about: the consent case.
fn consent() -> Value {
    json!({
        "question": "May I apply this change?",
        "choices": [{
            "id": "go", "question": "Apply it?", "type": "confirm",
            "yesLabel": "Apply", "noLabel": "Keep as it is", "default": "no",
            "show": [{"component": "Code", "title": "The change", "language": "diff", "code": DIFF}]
        }]
    })
}

/// Two questions showing the seven components, four and three.
fn all_seven() -> Value {
    json!({
        "question": "Before I ship.",
        "choices": [
            {"id": "plan", "question": "Which layout?", "options": ["A", "B"],
             "show": [
                {"component": "Text", "text": "Two layouts fit."},
                {"component": "Image", "artifact": PICTURE, "alt": "The page today", "caption": "Today"},
                {"component": "Mermaid", "code": "graph TD; A-->B"},
                {"component": "Code", "code": "let x = 1;", "language": "rust"}]},
            {"id": "risk", "question": "Go on?", "type": "confirm",
             "show": [
                {"component": "Cards", "cards": [{"title": "Option A"}, {"title": "Option B"}]},
                {"component": "Callout", "tone": "warning", "title": "Heads up", "text": "This cannot be undone."},
                {"component": "Chart", "type": "bar", "labels": ["Mon", "Tue"], "series": [{"name": "Runs", "values": [3, 4]}]}]}
        ]
    })
}

// ---- the fixtures are the web's catalog and the contract's vectors ----

#[test]
fn the_catalog_of_version_8_hashes_to_its_lock_and_to_upstream() {
    let digest = catalog_digest(&serde_json::from_str(V8).unwrap()).unwrap();
    let lock: Value = serde_json::from_str(V8_LOCK).unwrap();
    assert_eq!(lock["digest"], digest);
    assert_eq!(lock["version"], 8);
    let line = UPSTREAM
        .lines()
        .find(|l| l.starts_with("catalog-v8.json"))
        .unwrap();
    let mut parts = line.split_whitespace().skip(1);
    assert_eq!(parts.next().unwrap().len(), 40, "a full commit");
    assert_eq!(parts.next().unwrap(), digest);
}

#[test]
fn the_choices_of_version_8_has_show_and_version_7_has_not() {
    for (screen, has) in [(v8(), true), (v7(), false)] {
        let catalog = catalog_of(&screen);
        let question = &catalog.component("Choices").unwrap().schema()["properties"]["questions"]["items"]
            ["properties"];
        assert_eq!(
            question["show"].is_object(),
            has,
            "version {}",
            catalog.version()
        );
    }
    let catalog = catalog_of(&v8());
    let show = &catalog.component("Choices").unwrap().schema()["properties"]["questions"]["items"]
        ["properties"]["show"];
    assert_eq!(
        (show["minItems"].as_u64(), show["maxItems"].as_u64()),
        (Some(1), Some(4))
    );
}

// ---- a screen of version 8 draws what a question shows with it ----

#[tokio::test]
async fn each_shown_component_is_a_valid_instance_of_the_web_schema_and_the_form_names_it() {
    let screen = v8();
    let catalog = catalog_of(&screen);
    let (question, ui) = form(&screen, all_seven()).await;
    assert_eq!(question, "Before I ship.");
    golden("ask_form_v8_show.json", &ui);

    let all = components(&ui);
    // The form, in a Column, and the seven components loose beside it.
    assert_eq!(all.len(), 9);
    assert_eq!(component(&ui, "root")["children"], json!(["ask"]));
    for instance in all {
        catalog
            .validate(instance)
            .unwrap_or_else(|e| panic!("{instance}: {e}"));
    }
    let asked = component(&ui, "ask");
    assert_eq!(
        asked["questions"][0]["show"],
        json!(["q1-s1", "q1-s2", "q1-s3", "q1-s4"])
    );
    assert_eq!(
        asked["questions"][1]["show"],
        json!(["q2-s1", "q2-s2", "q2-s3"])
    );
    let names: Vec<&str> = [
        "q1-s1", "q1-s2", "q1-s3", "q1-s4", "q2-s1", "q2-s2", "q2-s3",
    ]
    .iter()
    .map(|id| component(&ui, id)["component"].as_str().unwrap())
    .collect();
    assert_eq!(
        names,
        [
            "Text", "Image", "Mermaid", "Code", "Cards", "Callout", "Chart"
        ]
    );
    // A shown component is placed nowhere else: not a child of anything.
    for container in all {
        for member in ["children", "child"] {
            let placed = container[member].to_string();
            for id in ["q1-s1", "q1-s4", "q2-s3"] {
                assert!(!placed.contains(id), "{id} placed by {container}");
            }
        }
    }
}

#[tokio::test]
async fn a_confirm_that_shows_a_diff_draws_the_diff_with_the_question() {
    let screen = v8();
    let (_, ui) = form(&screen, consent()).await;
    golden("ask_form_v8_consent.json", &ui);
    assert_eq!(component(&ui, "q1-s1")["code"], DIFF);
    assert_eq!(choices(&ui)["questions"][0]["show"], json!(["q1-s1"]));
    assert_eq!(choices(&ui)["questions"][0]["type"], "confirm");
}

#[tokio::test]
async fn a_hash_in_capitals_is_written_in_lower_case_and_a_form_with_no_show_is_as_before() {
    let screen = v8();
    let mut args = consent();
    args["choices"][0]["show"] =
        json!([{"component": "Image", "artifact": PICTURE.to_uppercase(), "alt": "x"}]);
    let (_, ui) = form(&screen, args).await;
    assert_eq!(component(&ui, "q1-s1")["artifact"], PICTURE);
    // Nothing shown: the root is the form, with no Column and no `show`.
    let mut plain = consent();
    plain["choices"][0].as_object_mut().unwrap().remove("show");
    let (_, ui) = form(&screen, plain).await;
    assert_eq!(components(&ui).len(), 1);
    assert_eq!(components(&ui)[0]["id"], "root");
    assert!(components(&ui)[0]["questions"][0].get("show").is_none());
}

// ---- a screen of version 7 gets the components above the form ----

#[tokio::test]
async fn a_screen_of_version_7_gets_the_question_with_no_show_and_the_components_above_it() {
    let screen = v7();
    let catalog = catalog_of(&screen);
    let (_, ui) = form(&screen, all_seven()).await;
    let asked = component(&ui, "ask");
    for q in asked["questions"].as_array().unwrap() {
        assert!(q.get("show").is_none(), "version 7 refuses `show`: {q}");
    }
    // The Column holds the seven in order, then the form: they are drawn above it.
    assert_eq!(
        component(&ui, "root")["children"],
        json!([
            "q1-cap", "q1-s1", "q1-s2", "q1-s3", "q1-s4", "q2-cap", "q2-s1", "q2-s2", "q2-s3",
            "ask"
        ])
    );
    for instance in components(&ui) {
        catalog
            .validate(instance)
            .unwrap_or_else(|e| panic!("{instance}: {e}"));
    }
    // Each question's blocks come under a caption that names the question and the blocks, and the
    // model's own `help` (there is none here) is not touched.
    assert_eq!(
        component(&ui, "q1-cap")["text"],
        "For question 1 (\"Which layout?\"), blocks 1, 2, 3 and 4 of 4:"
    );
    assert_eq!(
        component(&ui, "q2-cap")["text"],
        "For question 2 (\"Go on?\"), blocks 1, 2 and 3 of 3:"
    );
    for q in asked["questions"].as_array().unwrap() {
        assert!(q.get("help").is_none(), "{q}");
    }
    assert_eq!(component(&ui, "q2-s2")["title"], "Heads up");
}

#[tokio::test]
async fn a_screen_that_lacks_a_component_gets_its_content_in_words_in_the_question() {
    // Version 5 has no `Code`: the diff is said in full in the question the person reads first, and
    // the form says so.
    let (lead, ui) = form(&v5(), consent()).await;
    assert!(lead.starts_with("May I apply this change?"), "{lead}");
    assert!(lead.contains(DIFF.trim_end()), "{lead}");
    assert!(lead.contains("```diff"), "{lead}");
    assert!(components(&ui).iter().all(|c| c["component"] != "Code"));
    assert!(
        lead.contains(
            "For question 1 (\"Apply it?\"), block 1 of 1:\nBlock 1: Code \"The change\""
        ),
        "{lead}"
    );
    assert!(choices(&ui)["questions"][0].get("help").is_none());
    // A screen that has `Text` and `Column` draws the text above and says the rest.
    let args = json!({"question": "Q", "choices": [{
        "id": "a", "question": "Which?", "options": ["A", "B"],
        "show": [{"component": "Text", "text": "Some words"}, {"component": "Callout", "tone": "error", "title": "No", "text": "Careful"}]}]});
    let (lead, ui) = form(&v2(), args).await;
    assert!(
        lead.contains("For question 1 (\"Which?\"), block 2 of 2:\nBlock 2: error - No: Careful"),
        "{lead}"
    );
    assert!(!lead.contains("Some words"), "{lead}");
    assert_eq!(
        component(&ui, "root")["children"],
        json!(["q1-cap", "q1-s1", "ask"])
    );
    assert_eq!(
        component(&ui, "q1-cap")["text"],
        "For question 1 (\"Which?\"), block 1 of 2:"
    );
    assert_eq!(component(&ui, "q1-s1")["text"], "Some words");
}

#[tokio::test]
async fn a_screen_with_no_catalog_gets_every_shown_thing_in_words() {
    let text = words(None, all_seven()).await;
    for wanted in [
        "Two layouts fit.",
        "A picture: The page today (Today) - a file shared in this conversation",
        "```mermaid\ngraph TD; A-->B\n```",
        "let x = 1;",
        "- Option A",
        "warning - Heads up: This cannot be undone.",
        "A bar chart:",
        "- Runs: Mon 3, Tue 4",
    ] {
        assert!(text.contains(wanted), "{wanted:?} not in {text}");
    }
    let text = words(None, consent()).await;
    assert!(text.contains(DIFF.trim_end()), "{text}");
}

// ---- what is refused before a form is drawn ----

#[tokio::test]
async fn what_the_contract_refuses_is_refused_before_drawing_and_names_the_member() {
    let one = |show: Value| json!({"question": "Q", "choices": [{"id": "a", "question": "Which?", "options": ["A", "B"], "show": show}]});
    let text = |n: usize| {
        (0..n)
            .map(|i| json!({"component": "Text", "text": format!("t{i}")}))
            .collect::<Vec<_>>()
    };
    let nine: Vec<Value> = [4, 4, 1]
        .iter()
        .enumerate()
        .map(|(i, n)| {
            json!({"id": format!("q{i}"), "question": "Which?", "options": ["A", "B"], "show": text(*n)})
        })
        .collect();
    let cases: Vec<(&str, Value, &str)> = vec![
        (
            "five in a question",
            one(json!(text(5))),
            "`show` of question `a` has 5 blocks",
        ),
        ("empty", one(json!([])), "has 0 blocks"),
        (
            "not a list",
            one(json!("t1")),
            "`show` of question `a` is not a list",
        ),
        (
            "not an object",
            one(json!([7])),
            "block 1 of `show` of question `a` is not an object",
        ),
        (
            "no component",
            one(json!([{"text": "x"}])),
            "block 1 of `show` of question `a` has no `component`",
        ),
        (
            "a Choices",
            one(json!([{"component": "Choices", "questions": []}])),
            "is a `Choices`, which a question cannot show",
        ),
        (
            "a Table",
            one(json!([{"component": "Table", "columns": [], "rows": []}])),
            "is a `Table`, which a question cannot show",
        ),
        (
            "a Column",
            one(json!([{"component": "Column", "children": ["x"]}])),
            "is a `Column`",
        ),
        ("a Diff", one(json!([{"component": "Diff"}])), "is a `Diff`"),
        (
            "an Image with no alt",
            one(json!([{"component": "Image", "artifact": PICTURE}])),
            "an `Image` with no `alt`",
        ),
        (
            "an Image of a blank alt",
            one(json!([{"component": "Image", "artifact": PICTURE, "alt": " "}])),
            "an `Image` with no `alt`",
        ),
        (
            "an Image by URL",
            one(
                json!([{"component": "Image", "artifact": "https://example.com/a.png", "alt": "x"}]),
            ),
            "never a URL or a path",
        ),
        (
            "an Image by a short hash",
            one(json!([{"component": "Image", "artifact": "abc123", "alt": "x"}])),
            "sha256 (64 hexadecimal digits)",
        ),
        (
            "a hidden character",
            one(json!([{"component": "Code", "code": "let ok = true;\u{202e}"}])),
            "(U+202E) in `code`",
        ),
        (
            "a zero-width character in a card",
            one(json!([{"component": "Cards", "cards": [{"title": "a\u{200b}b"}]}])),
            "(U+200B) in `cards[1].title`",
        ),
        (
            "nine in a form",
            json!({"question": "Q", "choices": nine}),
            "the form shows 9 blocks in all",
        ),
    ];
    for (because, args, wanted) in cases {
        for screen in [Some(v8()), Some(v7()), None] {
            let out = refused(screen.as_ref(), args.clone()).await;
            assert!(out.contains(wanted), "{because}: {out}");
        }
    }
}

#[tokio::test]
async fn a_block_that_breaks_its_components_own_schema_names_the_block_and_the_problem() {
    let args = json!({"question": "Q", "choices": [{"id": "a", "question": "Which?", "options": ["A", "B"],
        "show": [{"component": "Text", "text": "ok"}, {"component": "Code", "code": ""}]}]});
    let out = refused(Some(&v8()), args).await;
    assert!(
        out.contains("block 2 of `show` of question `a` (Code):"),
        "{out}"
    );
    // Another member of a component is refused too: the web's schemas close their members.
    let args = json!({"question": "Q", "choices": [{"id": "a", "question": "Which?", "options": ["A", "B"],
        "show": [{"component": "Text", "text": "ok", "color": "red"}]}]});
    assert!(
        refused(Some(&v8()), args)
            .await
            .contains("block 1 of `show`")
    );
}

#[tokio::test]
async fn a_model_id_is_replaced_and_what_is_shown_cannot_be_shown_twice() {
    let args = json!({"question": "Q", "choices": [
        {"id": "a", "question": "Which?", "options": ["A", "B"], "show": [{"id": "root", "component": "Text", "text": "x"}]},
        {"id": "b", "question": "Which?", "options": ["A", "B"], "show": [{"id": "root", "component": "Text", "text": "x"}]}]});
    let (_, ui) = form(&v8(), args).await;
    let ids: Vec<&str> = components(&ui)
        .iter()
        .map(|c| c["id"].as_str().unwrap())
        .collect();
    assert_eq!(ids, ["root", "ask", "q1-s1", "q2-s1"]);
    assert_eq!(component(&ui, "root")["component"], "Column");
}

#[tokio::test]
async fn a_surface_over_the_size_a_screen_reads_is_refused() {
    let code = |n: usize| json!({"component": "Code", "code": "x".repeat(n)});
    let args = json!({"question": "Q", "choices": [
        {"id": "a", "question": "Which?", "options": ["A", "B"], "show": [code(20000), code(20000), code(20000), code(20000)]}]});
    let out = refused(Some(&v8()), args).await;
    assert!(out.contains("too large"), "{out}");
    assert!(out.contains("65536"), "{out}");
}

// ---- the schema is one value ----

#[test]
fn the_spec_is_the_same_whatever_the_screen_and_names_show() {
    let spec =
        |ui: &Ui| serde_json::to_string(&ui.tools().get("ask_user").unwrap().spec()).unwrap();
    let a = spec(&Ui::new(McpPolicy::default()));
    let b = spec(&Ui::new(McpPolicy::default()));
    assert_eq!(a, b);
    let spec = ask_tool().spec();
    let show = &spec.parameters["properties"]["choices"]["items"]["properties"]["show"];
    assert_eq!(show["maxItems"], 4);
    assert_eq!(show["minItems"], 1);
    assert_eq!(
        show["items"]["properties"]["component"]["enum"],
        json!([
            "Text", "Image", "Mermaid", "Code", "Cards", "Callout", "Chart"
        ])
    );
    assert!(spec.description.contains("`show`"));
}

// ---- a refused answer asks the same form again, showing the same things ----

fn budget_answer(surface: &str, value: &str) -> Value {
    json!({"text": "x", "form": {"surface": surface, "context": {
        "answers": [{"id": "budget", "values": [value]}]}}})
}

fn budget_args() -> Value {
    json!({"question": "How much?", "choices": [{
        "id": "budget", "question": "What is the budget?", "type": "number",
        "minValue": "0", "maxValue": "1000",
        "show": [{"component": "Callout", "tone": "info", "title": "Cap", "text": "The cap is 1000."},
                 {"component": "Chart", "type": "bar", "labels": ["a"], "series": [{"name": "s", "values": [1]}]}]}]})
}

#[tokio::test]
async fn a_refused_answer_asks_the_form_again_showing_the_same_things() {
    for screen in [v8(), v7()] {
        let catalog = catalog_of(&screen);
        let args = budget_args();
        let (_, ui) = form(&screen, args.clone()).await;
        let AnswerCheck::Refuse(refusal) = ask_tool().read_answer(
            "call-1",
            &args,
            Some(&ui),
            &budget_answer("ask-call-1", "1500"),
        ) else {
            panic!("a budget above its most was accepted");
        };
        let again = refusal.ui.unwrap();
        assert_eq!(again[0]["createSurface"]["surfaceId"], "ask-call-1.r1");
        assert_eq!(again[1]["updateComponents"]["surfaceId"], "ask-call-1.r1");
        // The same components, each valid for the screen it is drawn on, apart from the surface.
        assert_eq!(components(&again), components(&ui));
        for instance in components(&again) {
            catalog
                .validate(instance)
                .unwrap_or_else(|e| panic!("{instance}: {e}"));
        }
        assert_eq!(component(&again, "q1-s1")["title"], "Cap");
        assert_eq!(component(&again, "q1-s2")["component"], "Chart");
        // The Choices is found wherever it is: the form is asked with the valid answers as defaults.
        assert!(
            refusal.question.contains("above the most, 1000"),
            "{}",
            refusal.question
        );
    }
}

#[tokio::test]
async fn a_form_whose_things_were_said_in_words_says_them_again_when_it_is_asked_again() {
    // Version 5 cannot draw a Callout or a Chart: they are in the question, and in the new one.
    let screen = v5();
    let args = budget_args();
    let (lead, ui) = form(&screen, args.clone()).await;
    assert!(lead.contains("The cap is 1000."), "{lead}");
    let AnswerCheck::Refuse(refusal) = ask_tool().read_answer(
        "call-1",
        &args,
        Some(&ui),
        &budget_answer("ask-call-1", "1500"),
    ) else {
        panic!("a budget above its most was accepted");
    };
    assert!(
        refusal.question.contains("The cap is 1000."),
        "{}",
        refusal.question
    );
    assert!(
        refusal.question.contains("A bar chart:"),
        "{}",
        refusal.question
    );
    // A thing a screen did draw is not said a second time.
    let (lead, ui) = form(&v2(), json!({"question": "Q", "choices": [{
        "id": "budget", "question": "What is the budget?", "options": ["A", "B"],
        "show": [{"component": "Text", "text": "Plain words"}, {"component": "Callout", "title": "T", "text": "Said"}]}]})).await;
    assert!(
        lead.contains("Said") && !lead.contains("Plain words"),
        "{lead}"
    );
    let args = json!({"question": "Q", "choices": [{
        "id": "budget", "question": "What is the budget?", "options": ["A", "B"],
        "show": [{"component": "Text", "text": "Plain words"}, {"component": "Callout", "title": "T", "text": "Said"}]}]});
    let answer = json!({"text": "x", "form": {"surface": "ask-call-1", "context": {
        "answers": [{"id": "budget", "values": ["A"]}], "note": "n"}, "actions": 2}});
    let AnswerCheck::Refuse(refusal) = ask_tool().read_answer("call-1", &args, Some(&ui), &answer)
    else {
        panic!("two answers in one message were accepted");
    };
    assert!(refusal.question.contains("Said") && !refusal.question.contains("Plain words"));
}

// ---- every block is checked the same way on every screen ----

/// The screens a block is asked on: the version 8, 7, 5 and 2 catalogs and none.
fn every_screen() -> Vec<(&'static str, Option<Screen>)> {
    vec![
        ("v8", Some(v8())),
        ("v7", Some(v7())),
        ("v5", Some(v5())),
        ("v2", Some(v2())),
        ("none", None),
    ]
}

fn one_block(block: Value) -> Value {
    json!({"question": "Q", "choices": [{"id": "a", "question": "Which?", "options": ["A", "B"], "show": [block]}]})
}

#[tokio::test]
async fn a_block_the_web_would_refuse_is_refused_on_every_screen_naming_the_block_and_the_member() {
    let long = |n: usize| "x".repeat(n);
    let cases: Vec<(&str, Value, &str)> = vec![
        (
            "Code with no code",
            json!({"component": "Code"}),
            "`code` is missing",
        ),
        (
            "Code with a number",
            json!({"component": "Code", "code": 5}),
            "`code` is not text",
        ),
        (
            "Code of 20001",
            json!({"component": "Code", "code": long(20001)}),
            "`code` has 20001 characters",
        ),
        (
            "Code of no characters",
            json!({"component": "Code", "code": ""}),
            "`code` has 0 characters",
        ),
        (
            "Code title of 121",
            json!({"component": "Code", "code": "x", "title": long(121)}),
            "`title` has 121 characters",
        ),
        (
            "Callout with no text",
            json!({"component": "Callout", "title": "T"}),
            "`text` is missing",
        ),
        (
            "Callout of another tone",
            json!({"component": "Callout", "title": "T", "text": "x", "tone": "loud"}),
            "`tone` is not one of",
        ),
        (
            "Cards with no cards",
            json!({"component": "Cards"}),
            "`cards` is missing",
        ),
        (
            "Cards of none",
            json!({"component": "Cards", "cards": []}),
            "`cards` has 0 items",
        ),
        (
            "a card with no title",
            json!({"component": "Cards", "cards": [{"subtitle": "s"}]}),
            "`cards[1].title` is missing",
        ),
        (
            "a card link that is no http link",
            json!({"component": "Cards", "cards": [{"title": "t", "url": "javascript:x"}]}),
            "`cards[1].url` must start with http",
        ),
        (
            "nine tags",
            json!({"component": "Cards", "cards": [{"title": "t", "tags": ["a","b","c","d","e","f","g","h","i"]}]}),
            "`cards[1].tags` has 9 items",
        ),
        (
            "Text of 4001",
            json!({"component": "Text", "text": long(4001)}),
            "`text` has 4001 characters",
        ),
        (
            "Text of another variant",
            json!({"component": "Text", "text": "x", "variant": "h9"}),
            "`variant` is not one of",
        ),
        (
            "Mermaid with no code",
            json!({"component": "Mermaid", "title": "t"}),
            "`code` is missing",
        ),
        (
            "Chart with no labels",
            json!({"component": "Chart", "type": "bar", "series": [{"name": "s", "values": [1]}]}),
            "`labels` is missing",
        ),
        (
            "Chart of another type",
            json!({"component": "Chart", "type": "radar", "labels": ["a"], "series": [{"name": "s", "values": [1]}]}),
            "`type` is not one of",
        ),
        (
            "Chart with a value that is no number",
            json!({"component": "Chart", "type": "bar", "labels": ["a"], "series": [{"name": "s", "values": ["1"]}]}),
            "`series[1].values[1]` is not a number",
        ),
        (
            "Chart with five series",
            json!({"component": "Chart", "type": "bar", "labels": ["a"], "series": [
            {"name": "1", "values": [1]}, {"name": "2", "values": [1]}, {"name": "3", "values": [1]},
            {"name": "4", "values": [1]}, {"name": "5", "values": [1]}]}),
            "`series` has 5 items",
        ),
        (
            "Chart with a series of too few values",
            json!({"component": "Chart", "type": "bar", "labels": ["a", "b"], "series": [
            {"name": "ok", "values": [1, 2]}, {"name": "short", "values": [1]}]}),
            "`series[2]` (\"short\") has 1 values for 2 labels",
        ),
        (
            "Chart with a series of too many values",
            json!({"component": "Chart", "type": "bar", "labels": ["a"], "series": [
            {"name": "long", "values": [1, 2]}]}),
            "`series[1]` (\"long\") has 2 values for 1 labels",
        ),
        (
            "Image alt of 301",
            json!({"component": "Image", "artifact": PICTURE, "alt": long(301)}),
            "`alt` has 301 characters",
        ),
        (
            "a member nobody has",
            json!({"component": "Text", "text": "x", "color": "red"}),
            "`color` is not a member it has",
        ),
    ];
    for (because, block, wanted) in cases {
        let component = block["component"].as_str().unwrap().to_owned();
        for (name, screen) in every_screen() {
            let out = refused(screen.as_ref(), one_block(block.clone())).await;
            assert!(
                out.contains(&format!(
                    "block 1 of `show` of question `a` ({component}): "
                )),
                "{because} on {name}: {out}"
            );
            assert!(out.contains(wanted), "{because} on {name}: {out}");
        }
    }
}

#[tokio::test]
async fn the_limits_of_a_component_are_met_exactly_on_every_screen() {
    for (name, screen) in every_screen() {
        let block = json!({"component": "Code", "code": "x".repeat(20000)});
        let result = ask(screen.as_ref(), one_block(block)).await;
        assert!(
            matches!(result, Err(ToolError::NeedsInput { .. })),
            "20000 characters of code on {name}: {result:?}"
        );
    }
}

#[tokio::test]
async fn an_image_hash_that_is_not_64_lower_case_hexadecimal_digits_is_refused_everywhere() {
    let bad = [
        PICTURE[1..].to_owned(),
        format!("{PICTURE}a"),
        format!("g{}", &PICTURE[1..]),
        "z".repeat(64),
        format!("{} ", &PICTURE[1..]),
        "../a.png".to_owned(),
    ];
    for hash in bad {
        for (name, screen) in every_screen() {
            let block = json!({"component": "Image", "artifact": hash, "alt": "x"});
            let out = refused(screen.as_ref(), one_block(block)).await;
            assert!(
                out.contains("sha256 (64 hexadecimal digits)"),
                "{hash:?} on {name}: {out}"
            );
        }
    }
    // The hash is written in lower case, and a number is no hash.
    let block = json!({"component": "Image", "artifact": 5, "alt": "x"});
    assert!(
        refused(None, one_block(block))
            .await
            .contains("sha256 (64 hexadecimal digits)")
    );
}

#[tokio::test]
async fn the_words_of_a_screen_are_bounded_like_its_surface() {
    // 4 blocks of 20000 three-byte characters are 240000 bytes written out: none is read.
    let code = json!({"component": "Code", "code": "€".repeat(20000)});
    let args = json!({"question": "Q", "choices": [{"id": "a", "question": "Which?",
        "options": ["A", "B"], "show": [code.clone(), code.clone(), code.clone(), code]}]});
    for screen in [None, Some(v5())] {
        let out = refused(screen.as_ref(), args.clone()).await;
        assert!(out.contains("too large"), "{out}");
        assert!(out.contains("65536"), "{out}");
    }
}

/// A screen whose catalog is `base` with `change` applied to its document, digest and all.
fn screen_with(base: &Screen, change: impl FnOnce(&mut Value)) -> Screen {
    let mut document = base.document.clone();
    change(&mut document);
    Screen {
        claimed: Claimed {
            digest: catalog_digest(&document).unwrap(),
            ..base.claimed.clone()
        },
        document,
    }
}

#[tokio::test]
async fn a_screen_whose_component_is_stricter_than_the_table_refuses_the_block_naming_it() {
    // The table is the catalog of version 8; a screen's own schema still has the last word.
    let strict = screen_with(&v8(), |document| {
        document["components"]["Code"]["properties"]["code"]["maxLength"] = json!(100);
    });
    let out = refused(
        Some(&strict),
        one_block(json!({"component": "Code", "code": "x".repeat(101)})),
    )
    .await;
    assert!(
        out.contains("block 1 of `show` of question `a` (Code): "),
        "{out}"
    );
    // Within its limit the block is drawn.
    form(
        &strict,
        one_block(json!({"component": "Code", "code": "x".repeat(100)})),
    )
    .await;
}

// ---- a question's blocks are never detached from it ----

fn two_confirms() -> Value {
    json!({"question": "Two things.", "choices": [
        {"id": "wipe", "question": "Delete the database?", "type": "confirm", "help": "This cannot be undone and takes the backups too.",
         "show": [{"component": "Code", "title": "Plan A", "code": "DROP DATABASE app;"}]},
        {"id": "push", "question": "Push to main?", "type": "confirm",
         "show": [{"component": "Code", "title": "Plan B", "code": "git push origin main"}]}]})
}

#[tokio::test]
async fn blocks_above_the_form_come_under_a_caption_that_names_their_question() {
    let (_, ui) = form(&v7(), two_confirms()).await;
    assert_eq!(
        component(&ui, "root")["children"],
        json!(["q1-cap", "q1-s1", "q2-cap", "q2-s1", "ask"])
    );
    assert_eq!(
        component(&ui, "q1-cap")["text"],
        "For question 1 (\"Delete the database?\"), block 1 of 1:"
    );
    assert_eq!(
        component(&ui, "q2-cap")["text"],
        "For question 2 (\"Push to main?\"), block 1 of 1:"
    );
    assert_eq!(component(&ui, "q1-s1")["title"], "Plan A");
    // The model's help is what the model wrote.
    let asked = component(&ui, "ask");
    assert_eq!(
        asked["questions"][0]["help"],
        "This cannot be undone and takes the backups too."
    );
    assert!(asked["questions"][1].get("help").is_none());
}

#[tokio::test]
async fn blocks_in_words_come_under_a_caption_that_names_their_question() {
    for screen in [None, Some(v5())] {
        let (lead, _) = match &screen {
            Some(screen) => form(screen, two_confirms()).await,
            None => (words(None, two_confirms()).await, Value::Null),
        };
        let at = |needle: &str| {
            lead.find(needle)
                .unwrap_or_else(|| panic!("{needle:?} in {lead}"))
        };
        assert!(at("Plan A") < at("DROP DATABASE") && at("DROP DATABASE") < at("Plan B"));
        assert!(at("Plan B") < at("git push origin main"));
        if screen.is_some() {
            assert!(
                at("For question 1 (\"Delete the database?\"), block 1 of 1:")
                    < at("DROP DATABASE")
            );
            assert!(
                at("For question 2 (\"Push to main?\"), block 1 of 1:")
                    < at("git push origin main")
            );
        } else {
            assert!(at("1. Delete the database?") < at("Block 1 of 1: Code \"Plan A\""));
            assert!(at("2. Push to main?") < at("Block 1 of 1: Code \"Plan B\""));
        }
    }
    // A help of the model's own length is left whole.
    let mut args = two_confirms();
    let help = "h".repeat(300);
    args["choices"][0]["help"] = json!(help);
    let (_, ui) = form(&v5(), args).await;
    assert_eq!(choices(&ui)["questions"][0]["help"], json!(help));
}

#[tokio::test]
async fn the_number_of_a_block_is_said_wherever_it_lands() {
    // Version 5 has no Code and no Callout: blocks 1 and 3 are written, 2 and 4 drawn above.
    let args = json!({"question": "Q", "choices": [{"id": "a", "question": "Which?", "options": ["A", "B"], "show": [
        {"component": "Code", "code": "first"},
        {"component": "Mermaid", "code": "graph TD; A-->B"},
        {"component": "Callout", "title": "T", "text": "third"},
        {"component": "Text", "text": "fourth"}]}]});
    let (lead, ui) = form(&v5(), args).await;
    assert_eq!(
        component(&ui, "q1-cap")["text"],
        "For question 1 (\"Which?\"), blocks 2 and 4 of 4:"
    );
    assert_eq!(
        component(&ui, "root")["children"],
        json!(["q1-cap", "q1-s2", "q1-s4", "ask"])
    );
    let at = |needle: &str| {
        lead.find(needle)
            .unwrap_or_else(|| panic!("{needle:?} in {lead}"))
    };
    assert!(
        lead.contains("For question 1 (\"Which?\"), blocks 1 and 3 of 4:"),
        "{lead}"
    );
    assert!(at("Block 1: Code") < at("first") && at("first") < at("Block 3: info - T: third"));
    assert!(
        !lead.contains("fourth") && !lead.contains("graph TD"),
        "{lead}"
    );
}

#[tokio::test]
async fn a_screen_with_no_column_gets_everything_it_shows_in_words() {
    for (screen, what) in [(v8(), "version 8"), (v7(), "version 7")] {
        let screen = screen_with(&screen, |document| {
            document["components"]
                .as_object_mut()
                .unwrap()
                .remove("Column")
                .unwrap();
        });
        let (lead, ui) = form(&screen, consent()).await;
        assert!(lead.contains(DIFF.trim_end()), "{what}: {lead}");
        assert!(
            lead.contains("Block 1: Code \"The change\""),
            "{what}: {lead}"
        );
        // Nothing is half placed: the Choices alone, naming nothing it does not hold.
        assert_eq!(components(&ui).len(), 1, "{what}: {ui}");
        assert!(
            choices(&ui)["questions"][0].get("show").is_none(),
            "{what}: {ui}"
        );
    }
}

// ---- the words say every member that is content ----

fn every_member() -> Value {
    json!({"question": "Before I ship.", "choices": [
        {"id": "c1", "question": "Which change?", "options": ["A", "B"], "show": [
            {"component": "Code", "title": "The build", "filename": "Makefile", "language": "make", "code": "all:\n\tcc a.c\n"},
            {"component": "Mermaid", "title": "The flow", "code": "graph TD; A-->B", "caption": "How it runs", "weight": 2},
            {"component": "Callout", "tone": "warning", "title": "Heads up", "text": "This cannot be undone."},
            {"component": "Text", "text": "Plain words", "variant": "h2", "weight": 1}]},
        {"id": "c2", "question": "Go on?", "type": "confirm", "show": [
            {"component": "Cards", "title": "Options", "layout": "grid", "weight": 1, "cards": [
                {"title": "Card A", "subtitle": "first", "body": "the body", "url": "https://example.com/a", "tags": ["x", "y"]},
                {"title": "Card B"}]},
            {"component": "Chart", "title": "Latency", "caption": "Last week", "type": "line", "unit": "ms",
             "labels": ["Mon", "Tue"], "series": [{"name": "p50", "values": [3, 4]}, {"name": "p99", "values": [30, 40.5]}]},
            {"component": "Image", "artifact": PICTURE, "alt": "The page", "caption": "Today", "weight": 1}]}]})
}

fn golden_text(name: &str, got: &str) {
    let path = PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .join("tests/golden")
        .join(name);
    if std::env::var_os("ADAM_UPDATE_SNAPSHOTS").is_some() {
        std::fs::write(&path, got).unwrap();
    }
    assert_eq!(
        got,
        std::fs::read_to_string(&path).unwrap(),
        "{name}: regenerate with ADAM_UPDATE_SNAPSHOTS=1"
    );
}

#[tokio::test]
async fn the_words_of_each_component_say_every_member_that_is_content() {
    let text = words(None, every_member()).await;
    golden_text("ask_show_words.txt", &text);
    // What only says how to draw is not said.
    for drawn_only in ["h2", "grid", "weight"] {
        assert!(!text.contains(drawn_only), "{drawn_only:?} in {text}");
    }
}

#[tokio::test]
async fn a_code_fence_is_longer_than_any_run_of_backticks_in_the_code() {
    for (code, fence) in [
        ("```\nnot a close\n```", "````"),
        ("a `` b", "```"),
        ("`````x", "``````"),
    ] {
        let text = words(None, one_block(json!({"component": "Code", "code": code}))).await;
        assert!(
            text.contains(&format!("{fence}\n{code}\n{fence}")),
            "{text}"
        );
    }
}

#[tokio::test]
async fn a_tab_and_a_line_break_in_code_are_text() {
    let make = "all:\r\n\tcc -o a a.c\n\t./a\n";
    let args = one_block(json!({"component": "Code", "language": "make", "code": make}));
    let (_, ui) = form(&v8(), args.clone()).await;
    assert_eq!(component(&ui, "q1-s1")["code"], make);
    assert!(words(None, args).await.contains(make));
}

#[tokio::test]
async fn every_bidirectional_control_and_hidden_character_in_a_block_is_refused_and_says_what_to_do()
 {
    let controls: Vec<u32> = (0x202A..=0x202E)
        .chain(0x2066..=0x2069)
        .chain([
            0x200E, 0x200F, 0x061C, 0x200B, 0x200C, 0x200D, 0x2060, 0xFEFF, 0x00AD, 0x0007,
        ])
        .collect();
    for code in controls {
        let c = char::from_u32(code).unwrap();
        for (name, screen) in [("v8", Some(v8())), ("v5", Some(v5())), ("none", None)] {
            let out = refused(
                screen.as_ref(),
                one_block(json!({"component": "Code", "code": format!("let a{c}b;")})),
            )
            .await;
            assert!(
                out.contains("block 1 of `show` of question `a`"),
                "{code:X} {name}: {out}"
            );
            assert!(
                out.contains(&format!("(U+{code:04X}) in `code`")),
                "{code:X} {name}: {out}"
            );
            assert!(
                out.contains(&format!("\\u{{{code:04X}}}")) && out.contains("say it in words"),
                "{code:X} {name}: {out}"
            );
        }
    }
}

// ---- the size of what is sent ----

#[tokio::test]
async fn the_size_is_measured_in_utf8_bytes_of_what_is_sent() {
    // 3 blocks of 6000 three-byte characters are about 54 KB, and a fourth makes 72 KB: 24000
    // characters, over the bound in bytes only.
    let code = |n: usize| json!({"component": "Code", "code": "€".repeat(n)});
    let ok = json!({"question": "Q", "choices": [{"id": "a", "question": "Which?", "options": ["A", "B"],
        "show": [code(6000), code(6000), code(6000)]}]});
    form(&v8(), ok).await;
    let over = json!({"question": "Q", "choices": [{"id": "a", "question": "Which?", "options": ["A", "B"],
        "show": [code(6000), code(6000), code(6000), code(6000)]}]});
    let out = refused(Some(&v8()), over).await;
    assert!(out.contains("too large") && out.contains("65536"), "{out}");
}

#[tokio::test]
async fn defaults_carried_to_a_form_asked_again_never_make_it_larger_than_a_screen_reads() {
    let code = |n: usize| json!({"component": "Code", "code": "x".repeat(n)});
    let args = |n: usize| {
        json!({"question": "Q", "choices": [
            {"id": "budget", "question": "What is the budget?", "type": "number", "minValue": "0", "maxValue": "1000",
             "show": [code(n), code(n), code(n), code(n)]},
            {"id": "notes", "question": "Anything else?", "type": "longText"},
            {"id": "pick", "question": "Which?", "options": ["A", "B"]}]})
    };
    let size = |ui: &Value| ui.to_string().len();
    // Find the code length that leaves the first surface just under the bound.
    let (_, probe) = form(&v8(), args(10000)).await;
    let n = 10000 + (65536 - 400 - size(&probe)) / 4;
    let (_, ui) = form(&v8(), args(n)).await;
    assert!(
        size(&ui) <= 65536 && size(&ui) > 65536 - 600,
        "{}",
        size(&ui)
    );
    let answer = json!({"text": "x", "form": {"surface": "ask-call-1", "context": {"answers": [
        {"id": "budget", "values": ["1500"]},
        {"id": "notes", "values": [], "text": "y".repeat(1800)},
        {"id": "pick", "values": ["a"]}]}}});
    let AnswerCheck::Refuse(refusal) =
        ask_tool().read_answer("call-1", &args(n), Some(&ui), &answer)
    else {
        panic!("a budget above its most was accepted");
    };
    let again = refusal.ui.expect("the form is asked again on a surface");
    assert!(size(&again) <= 65536, "{}", size(&again));
    let questions = choices(&again)["questions"].as_array().unwrap().clone();
    assert!(
        questions[1].get("default").is_none(),
        "the text was kept: {}",
        questions[1]
    );
    assert_eq!(
        questions[2]["default"], "a",
        "a default that fits was dropped"
    );
    assert_eq!(component(&again, "q1-s4")["component"], "Code");
}

// ---- a refused answer asks again from the surface that was journaled ----

#[tokio::test]
async fn a_surface_journaled_with_the_run_is_what_a_re_ask_reads_back_when_the_run_is_loaded_again()
{
    let args = budget_args();
    for screen in [v8(), v7(), v5()] {
        let (_, ui) = form(&screen, args.clone()).await;
        // The journal holds the surface as JSON text; a run loaded again reads it back.
        let loaded: Value = serde_json::from_str(&serde_json::to_string(&ui).unwrap()).unwrap();
        let answer = budget_answer("ask-call-1", "1500");
        let read = |ui: &Value| match ask_tool().read_answer("call-1", &args, Some(ui), &answer) {
            AnswerCheck::Refuse(refusal) => (refusal.question, refusal.ui),
            other => panic!("a budget above its most was accepted: {other:?}"),
        };
        let (question, again) = read(&loaded);
        assert_eq!(
            (question.clone(), again.clone()),
            read(&ui),
            "version {}",
            screen.claimed.version
        );
        let again = again.unwrap();
        assert_eq!(components(&again), components(&loaded));
        assert!(question.contains("above the most, 1000"), "{question}");
        // What the screen could not draw is said again, once, in the same words.
        let first = form(&screen, args.clone()).await.0;
        let said: Vec<&str> = first.lines().filter(|l| l.starts_with("Block ")).collect();
        for line in &said {
            assert_eq!(question.matches(line).count(), 1, "{line:?} in {question}");
        }
    }
}

// ---- the contract's vectors, run through the tool ----

/// How a vector of the contract's `v8-surfaces` is held to what `ask_user` makes.
#[derive(Debug, PartialEq, Eq, Clone, Copy)]
enum Held {
    /// The contract's result is the tool's: drawn with the vector's components in its order, or
    /// refused before drawing with a message that names the block.
    Same,
    /// The tool gives each shown component an id of its own and places it only by `show`, so the
    /// case cannot be made: the surface it draws is walked for the rule instead.
    ByConstruction,
    /// Only the web knows (the thread's files): the tool draws it and the web refuses it.
    WebOnly,
    /// Not about a question that shows (a plan, an update of a surface, another catalog).
    NotAboutAsking,
}

/// A vector's surface as the arguments of a call, or `None` when it is not about a form.
fn call_of(case: &Value) -> Option<Value> {
    let all = case["components"].as_array()?;
    let form = all.iter().find(|c| c["component"] == "Choices")?;
    let by_id = |id: &Value| all.iter().find(|c| c["id"] == *id).cloned();
    let questions: Vec<Value> = form["questions"]
        .as_array()?
        .iter()
        .map(|q| {
            let mut q = q.clone();
            let ids = q.get("show").and_then(Value::as_array).cloned();
            if let Some(ids) = ids.filter(|ids| ids.iter().all(Value::is_string)) {
                let blocks: Vec<Value> = ids
                    .iter()
                    .map(|id| {
                        let mut block = by_id(id).unwrap_or_else(
                            || json!({"component": "Text", "text": "an id of nothing yet"}),
                        );
                        block.as_object_mut().unwrap().remove("id");
                        block
                    })
                    .collect();
                q["show"] = Value::Array(blocks);
            }
            q
        })
        .collect();
    Some(json!({"question": "Q", "choices": questions}))
}

/// The components a form shows, per question, without their ids: from a surface (`show` names ids)
/// of the contract's, or of the tool's.
fn shown_per_question(components: &[Value]) -> Vec<Vec<Value>> {
    let form = components
        .iter()
        .find(|c| c["component"] == "Choices")
        .unwrap();
    form["questions"]
        .as_array()
        .unwrap()
        .iter()
        .map(|q| {
            q["show"]
                .as_array()
                .into_iter()
                .flatten()
                .filter_map(|id| components.iter().find(|c| c["id"] == *id))
                .map(|c| {
                    let mut c = c.clone();
                    c.as_object_mut().unwrap().remove("id");
                    c
                })
                .collect()
        })
        .collect()
}

/// The ids that a container of the surface places: the `children` of a `Column` or `Stack`, the
/// `child` of a `Button` or a card, the `child` of a `Tabs` entry or an `Accordion` item.
fn placed_by_containers(components: &[Value]) -> Vec<String> {
    let mut placed = Vec::new();
    for c in components {
        for id in c["children"].as_array().into_iter().flatten() {
            placed.extend(id.as_str().map(str::to_owned));
        }
        placed.extend(c["child"].as_str().map(str::to_owned));
        for member in ["tabs", "items"] {
            for entry in c[member].as_array().into_iter().flatten() {
                placed.extend(entry["child"].as_str().map(str::to_owned));
            }
        }
    }
    placed
}

/// What a refused vector says in the tool's words: each of these is in the message.
fn refusal_of(because: &str) -> &'static [&'static str] {
    match because {
        b if b.starts_with("a question shows") => &[
            "block 1 of `show` of question",
            "which a question cannot show",
        ],
        "an empty list" => &["`show` of question", "has 0 blocks"],
        "five ids in one question" => &["`show` of question", "has 5 blocks"],
        b if b.starts_with("nine ids") => &["the form shows 9 blocks in all"],
        "show is a string, not a list" => &["`show` of question", "is not a list of blocks"],
        "show holds a number" => &["block 1 of `show` of question", "is not an object"],
        "an image with no alt text" => {
            &["block 1 of `show` of question", "an `Image` with no `alt`"]
        }
        other => panic!("{other}: a refusal the test does not know"),
    }
}

#[tokio::test]
async fn the_vectors_of_the_contract_are_held_by_the_tool() {
    let document: Value = serde_json::from_str(VECTORS).unwrap();
    let cases = document["v8Surfaces"]["cases"].as_array().unwrap();
    assert_eq!(
        cases.len(),
        CASES,
        "a case was deleted, or the page holds more"
    );
    let screen = v8();
    let mut tally = std::collections::BTreeMap::<String, usize>::new();
    for case in cases {
        let result = case["result"].as_str().unwrap();
        let because = case["because"].as_str().unwrap();
        let plan = case["surfaceId"]
            .as_str()
            .is_some_and(|id| id.starts_with("plan-") && id.len() > 5);
        let held = match call_of(case) {
            Some(_)
                if case["catalog"].is_string() || case.get("operationsAfter").is_some() || plan =>
            {
                Held::NotAboutAsking
            }
            None => Held::NotAboutAsking,
            Some(args) => {
                let out = ask(Some(&screen), args.clone()).await;
                let ui = match &out {
                    Err(ToolError::NeedsInput { ui: Some(ui), .. }) => Some(ui),
                    _ => None,
                };
                // Whatever is drawn names each shown component once and places it only by `show`.
                if let Some(ui) = ui {
                    let drawn = components(ui);
                    let shown: Vec<&str> = drawn
                        .iter()
                        .filter(|c| c["component"] != "Choices" && c["id"] != "root")
                        .map(|c| c["id"].as_str().unwrap())
                        .collect();
                    let named: Vec<&str> = choices(ui)["questions"]
                        .as_array()
                        .unwrap()
                        .iter()
                        .flat_map(|q| q["show"].as_array().into_iter().flatten())
                        .map(|id| id.as_str().unwrap())
                        .collect();
                    assert_eq!(
                        shown, named,
                        "{because}: every shown id is in the surface and in one `show`"
                    );
                    let placed = placed_by_containers(drawn);
                    assert!(
                        named.iter().all(|id| !placed.iter().any(|p| p == id)),
                        "{because}: a shown component is placed by a container"
                    );
                }
                match result {
                    "drawn" => {
                        let ui = ui.unwrap_or_else(|| panic!("{because}: {out:?}"));
                        let ours = shown_per_question(components(ui));
                        let theirs = shown_per_question(case["components"].as_array().unwrap());
                        assert_eq!(
                            ours, theirs,
                            "{because}: the components, in the vector's order"
                        );
                        for (n, q) in ours.iter().enumerate() {
                            for j in 1..=q.len() {
                                component(ui, &format!("q{}-s{j}", n + 1));
                            }
                        }
                        Held::Same
                    }
                    "refused:schema" => {
                        // An id twice, or shown by two questions or two forms, cannot be written:
                        // each block is given an id of its own, and each is drawn.
                        let by_construction = [
                            "the same id twice in one question",
                            "one id shown by two questions of one form",
                            "one id shown by two forms",
                        ]
                        .contains(&because);
                        if by_construction {
                            assert!(ui.is_some(), "{because}: {out:?}");
                            Held::ByConstruction
                        } else {
                            let Ok(refusal) = &out else {
                                panic!("{because}: expected a refusal, got {out:?}");
                            };
                            assert!(refusal.is_error, "{because}: {out:?}");
                            for wanted in refusal_of(because) {
                                assert!(
                                    refusal.content.contains(wanted),
                                    "{because}: {wanted:?} not in {}",
                                    refusal.content
                                );
                            }
                            Held::Same
                        }
                    }
                    // The tool draws a surface whole, every id of it present: nothing is "not there
                    // yet" (pending); and each id is placed by `show` alone, never by a container
                    // (expansion): the walk above holds both for what it drew.
                    "refused:expansion" | "pending" => {
                        assert!(ui.is_some(), "{because}: {out:?}");
                        Held::ByConstruction
                    }
                    // The tool does not know the thread's files: it draws the image with the hash,
                    // and the web refuses a file the thread does not hold.
                    "refused:artifact" => {
                        let ui = ui.unwrap_or_else(|| panic!("{because}: {out:?}"));
                        let hash = case["components"]
                            .as_array()
                            .unwrap()
                            .iter()
                            .find(|c| c["component"] == "Image")
                            .unwrap()["artifact"]
                            .clone();
                        assert_eq!(component(ui, "q1-s1")["artifact"], hash, "{because}");
                        Held::WebOnly
                    }
                    other => panic!("{because}: a result the test does not know: {other}"),
                }
            }
        };
        *tally.entry(format!("{held:?}")).or_default() += 1;
    }
    // Every case is in one class: a case that moves from one to another is a decision to review.
    assert_eq!(tally.values().sum::<usize>(), CASES);
    assert_eq!(
        tally,
        [
            ("ByConstruction", 7),
            ("NotAboutAsking", 12),
            ("Same", 25),
            ("WebOnly", 2),
        ]
        .into_iter()
        .map(|(k, v)| (k.to_owned(), v))
        .collect()
    );
}
