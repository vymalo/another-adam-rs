//! `ask_user` with a question that shows what it asks about (catalog version 8, `show`), against the
//! published catalogs of versions 8, 7, 5 and 2 (byte copies in `tests/fixtures`, see `UPSTREAM`):
//! drawn with the question where the screen's `Choices` takes `show`, above the form where it does
//! not, in words where the screen cannot draw the component, and the same things shown again when a
//! refused answer asks the form again.
#![allow(clippy::unwrap_used, clippy::expect_used)] // integration tests assert by unwrapping

use std::path::PathBuf;
use std::sync::Arc;

use adam_a2a_runtime::{CONTEXT_UI_CATALOG, CONTEXT_UI_REF};
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
    Screen {
        document: serde_json::from_str(document).unwrap(),
        claimed: Claimed {
            catalog_id: CATALOG_ID.to_owned(),
            version: u32::try_from(lock["version"].as_u64().unwrap()).unwrap(),
            digest: lock["digest"].as_str().unwrap().to_owned(),
        },
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

#[tokio::test]
async fn a_replay_makes_the_same_surface() {
    let first = form(&v8(), all_seven()).await;
    let second = form(&v8(), all_seven()).await;
    assert_eq!(first, second);
    let first = form(&v7(), all_seven()).await;
    let second = form(&v7(), all_seven()).await;
    assert_eq!(first, second);
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
            "q1-s1", "q1-s2", "q1-s3", "q1-s4", "q2-s1", "q2-s2", "q2-s3", "ask"
        ])
    );
    for instance in components(&ui) {
        catalog
            .validate(instance)
            .unwrap_or_else(|e| panic!("{instance}: {e}"));
    }
    // The question says where what it is about is.
    for q in asked["questions"].as_array().unwrap() {
        assert!(
            q["help"].as_str().unwrap().contains("shown above the form"),
            "{q}"
        );
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
    let help = choices(&ui)["questions"][0]["help"].as_str().unwrap();
    assert!(help.contains("written above the form"), "{help}");
    // A screen that has `Text` and `Column` draws the text above and says the rest.
    let args = json!({"question": "Q", "choices": [{
        "id": "a", "question": "Which?", "options": ["A", "B"],
        "show": [{"component": "Text", "text": "Some words"}, {"component": "Callout", "tone": "error", "title": "No", "text": "Careful"}]}]});
    let (lead, ui) = form(&v2(), args).await;
    assert!(lead.contains("error - No: Careful"), "{lead}");
    assert!(!lead.contains("Some words"), "{lead}");
    assert_eq!(component(&ui, "root")["children"], json!(["q1-s1", "ask"]));
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

// ---- the contract's vectors, run through the tool ----

/// How a vector of the contract's `v8-surfaces` is held to what `ask_user` makes.
#[derive(Debug, PartialEq, Eq, Clone, Copy)]
enum Held {
    /// The contract's result is the tool's: drawn, or refused before drawing.
    Same,
    /// The tool gives each shown component an id of its own and places it only by `show`, so the
    /// case cannot be made: the call is accepted and the surface holds what the contract wants.
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
                let drawn = matches!(out, Err(ToolError::NeedsInput { ui: Some(_), .. }));
                match result {
                    "drawn" => {
                        assert!(drawn, "{because}: {out:?}");
                        Held::Same
                    }
                    "refused:schema" => {
                        // An id twice, or shown by two questions or two forms, cannot be written:
                        // each block is given an id of its own.
                        let by_construction = [
                            "the same id twice in one question",
                            "one id shown by two questions of one form",
                            "one id shown by two forms",
                        ]
                        .contains(&because);
                        if by_construction {
                            assert!(drawn, "{because}: {out:?}");
                            Held::ByConstruction
                        } else {
                            assert!(!drawn, "{because}: {out:?}");
                            Held::Same
                        }
                    }
                    "refused:expansion" | "pending" => {
                        assert!(drawn, "{because}: {out:?}");
                        Held::ByConstruction
                    }
                    "refused:artifact" => {
                        assert!(drawn, "{because}: {out:?}");
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
