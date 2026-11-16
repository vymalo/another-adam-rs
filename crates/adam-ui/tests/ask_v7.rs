//! `ask_user` with the five kinds of catalog version 7 (`confirm`, `number`, `scale`, `date`,
//! `rank`), against the published catalogs of versions 7, 6, 5 and 2 (byte copies in
//! `tests/fixtures`, see `UPSTREAM`): drawn natively where the screen's `Choices` takes them, as the
//! nearest older form where it does not, and as words where nothing can be drawn. Every instance the
//! tool makes is checked against the web's own schema.
#![allow(clippy::unwrap_used, clippy::expect_used)] // integration tests assert by unwrapping

use std::path::PathBuf;
use std::sync::Arc;

use adam_a2a_runtime::{CONTEXT_UI_CATALOG, CONTEXT_UI_REF};
use adam_llm_agent::{ToolCtx, ToolError, ToolOutput};
use adam_mcp::McpPolicy;
use adam_runtime::NoopSink;
use adam_ui::{Catalog, Claimed, Ui, catalog_digest};
use serde_json::{Map, Value, json};

const CATALOG_ID: &str = "https://agents.vymalo.com/a2ui/catalogs/chat";
const V7: &str = include_str!("fixtures/catalog-v7.json");
const V7_LOCK: &str = include_str!("fixtures/catalog-v7.lock.json");
const V6: &str = include_str!("fixtures/catalog-v6.json");
const V6_LOCK: &str = include_str!("fixtures/catalog-v6.lock.json");
const V5: &str = include_str!("fixtures/catalog-v5.json");
const V5_LOCK: &str = include_str!("fixtures/catalog-v5.lock.json");
const V2: &str = include_str!("fixtures/catalog-v2.json");
const V2_LOCK: &str = include_str!("fixtures/catalog-v2.lock.json");
const UPSTREAM: &str = include_str!("fixtures/UPSTREAM");
const VECTORS: &str = include_str!("fixtures/choices-v7-vectors.json");

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

fn v7() -> Screen {
    screen(V7, V7_LOCK)
}
fn v6() -> Screen {
    screen(V6, V6_LOCK)
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

/// The five questions of the contract's own examples.
fn examples() -> Vec<Value> {
    let vectors: Value = serde_json::from_str(VECTORS).unwrap();
    vectors["questions"].as_array().unwrap().clone()
}

fn five() -> Value {
    json!({
        "question": "Before I ship it.",
        "title": "Before I ship",
        "description": "Five things I cannot decide for you.",
        "choices": examples(),
    })
}

fn one(question: Value) -> Value {
    json!({"question": "Q", "choices": [question]})
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

// ---- the fixtures are the web's catalogs ----

#[test]
fn every_catalog_fixture_hashes_to_its_lock_and_to_upstream() {
    let upstream: Vec<(&str, &str, &str)> = UPSTREAM
        .lines()
        .filter(|l| l.starts_with("catalog-v"))
        .map(|l| {
            let mut parts = l.split_whitespace();
            (
                parts.next().unwrap(),
                parts.next().unwrap(),
                parts.next().unwrap(),
            )
        })
        .collect();
    assert_eq!(upstream.len(), 5);
    for (file, document, lock) in [
        ("catalog-v2.json", V2, V2_LOCK),
        ("catalog-v5.json", V5, V5_LOCK),
        ("catalog-v6.json", V6, V6_LOCK),
        ("catalog-v7.json", V7, V7_LOCK),
    ] {
        let digest = catalog_digest(&serde_json::from_str(document).unwrap()).unwrap();
        let lock: Value = serde_json::from_str(lock).unwrap();
        assert_eq!(lock["digest"], digest, "{file}: the lock is the digest");
        let (_, commit, named) = upstream.iter().find(|(f, _, _)| *f == file).unwrap();
        assert_eq!(*named, digest, "{file}: UPSTREAM names the digest");
        assert_eq!(commit.len(), 40, "{file}: UPSTREAM names a full commit");
    }
}

#[test]
fn the_catalog_of_version_7_has_the_five_kinds_and_their_members() {
    let screen = v7();
    let catalog = Catalog::from_document(screen.document.clone(), &screen.claimed).unwrap();
    assert_eq!(catalog.version(), 7);
    let question = &catalog.component("Choices").unwrap().schema()["properties"]["questions"]["items"]
        ["properties"];
    assert_eq!(
        question["type"]["enum"],
        json!([
            "single", "multiple", "select", "images", "text", "longText", "confirm", "number",
            "scale", "date", "rank"
        ])
    );
    for member in [
        "yesLabel",
        "noLabel",
        "minValue",
        "maxValue",
        "step",
        "unit",
        "from",
        "to",
        "fromLabel",
        "toLabel",
        "minDate",
        "maxDate",
    ] {
        assert!(question[member].is_object(), "{member}");
    }
}

// ---- the whole form on a screen of version 7 ----

#[tokio::test]
async fn the_five_kinds_are_drawn_whole_and_are_valid_instances_of_the_web_schema() {
    // `form` fails the test when the schema refuses the instance.
    let (question, root) = form(&v7(), five()).await;
    assert_eq!(question, "Before I ship it.");
    golden("ask_form_v7.json", &root);
    let types: Vec<&str> = root["questions"]
        .as_array()
        .unwrap()
        .iter()
        .map(|q| q["type"].as_str().unwrap())
        .collect();
    assert_eq!(types, ["confirm", "number", "scale", "date", "rank"]);
    // A decimal is a string, never a JSON number, in what the screen is sent.
    assert_eq!(root["questions"][1]["maxValue"], "1000");
    assert_eq!(root["questions"][2]["to"], 10);
}

#[tokio::test]
async fn each_kind_alone_and_with_numbers_written_as_json_numbers_is_a_valid_instance() {
    let calls = [
        one(json!({"id": "a", "question": "Sure?", "type": "confirm"})),
        one(
            json!({"id": "a", "question": "Sure?", "type": "confirm", "yesLabel": "Yes, ship", "noLabel": "Not yet", "default": "no", "required": false}),
        ),
        // a label of the button names the safe answer too
        one(
            json!({"id": "a", "question": "Sure?", "type": "confirm", "yesLabel": "Delete it", "default": "Delete it"}),
        ),
        one(json!({"id": "a", "question": "How many?", "type": "number"})),
        one(
            json!({"id": "a", "question": "How many?", "type": "number", "minValue": 0, "maxValue": 1000.5, "step": 0.5, "unit": "kg", "default": 250}),
        ),
        one(
            json!({"id": "a", "question": "How many?", "type": "number", "minValue": "-5", "maxValue": "5", "step": "2.5", "default": "-2.5"}),
        ),
        one(json!({"id": "a", "question": "Rate", "type": "scale", "to": 5})),
        one(
            json!({"id": "a", "question": "Rate", "type": "scale", "from": 0, "to": 10, "fromLabel": "Low", "toLabel": "High", "default": 7}),
        ),
        one(json!({"id": "a", "question": "Day?", "type": "date"})),
        one(
            json!({"id": "a", "question": "Day?", "type": "date", "minDate": "2026-12-01", "maxDate": "2026-12-31", "default": "2026-12-15"}),
        ),
        one(json!({"id": "a", "question": "Order", "type": "rank", "options": ["A", "B"]})),
        one(
            json!({"id": "a", "question": "Order", "type": "rank", "options": [{"value": "x", "label": "X", "icon": "git", "recommended": true}, "Y", "Z"], "default": ["z", "x", "y"]}),
        ),
    ];
    for call in calls {
        let (_, root) = form(&v7(), call.clone()).await;
        assert_eq!(root["component"], "Choices", "{call}");
    }
    // The model's JSON numbers come out as the decimal strings the catalog needs.
    let (_, root) = form(
        &v7(),
        one(json!({"id": "a", "question": "How many?", "type": "number", "minValue": 0, "maxValue": 1000.5, "step": 0.5, "default": 250})),
    )
    .await;
    let q = &root["questions"][0];
    assert_eq!(
        (
            q["minValue"].clone(),
            q["maxValue"].clone(),
            q["step"].clone(),
            q["default"].clone()
        ),
        (json!("0"), json!("1000.5"), json!("0.5"), json!("250"))
    );
}

#[tokio::test]
async fn the_models_mistakes_with_the_five_kinds_come_back_before_the_catalog_is_asked() {
    let q = |extra: Value| {
        let mut base = json!({"id": "a", "question": "Q?"});
        base.as_object_mut()
            .unwrap()
            .extend(extra.as_object().unwrap().clone());
        one(base)
    };
    let cases = [
        (
            q(json!({"type": "number", "minValue": "1e3"})),
            "not a decimal",
        ),
        (
            q(json!({"type": "number", "minValue": 1e21})),
            "not a decimal",
        ),
        (
            q(json!({"type": "number", "minValue": "10", "maxValue": "5"})),
            "`minValue` of question `a` is above",
        ),
        (q(json!({"type": "number", "step": 0})), "above zero"),
        (
            q(json!({"type": "number", "default": "x"})),
            "not a decimal",
        ),
        (
            q(json!({"type": "number", "maxValue": "10", "default": "11"})),
            "not an answer the question accepts",
        ),
        (
            q(json!({"type": "number", "yesLabel": "Go"})),
            "`yesLabel`, which belongs to type \"confirm\"",
        ),
        (
            q(json!({"type": "single", "options": ["a", "b"], "step": "1"})),
            "belongs to type \"number\"",
        ),
        (
            q(json!({"type": "text", "unit": "kg"})),
            "belongs to type \"number\"",
        ),
        (
            q(json!({"type": "number", "multiple": true})),
            "takes no `multiple`",
        ),
        (
            q(json!({"type": "confirm", "options": ["a", "b"]})),
            "takes no options",
        ),
        (
            q(json!({"type": "confirm", "default": "maybe"})),
            "use \"yes\" or \"no\"",
        ),
        (q(json!({"type": "confirm", "default": ["yes"]})), "a list"),
        (q(json!({"type": "scale"})), "`to` is required"),
        (q(json!({"type": "scale", "to": 11})), "2 to 10"),
        (q(json!({"type": "scale", "from": 2, "to": 5})), "0 or 1"),
        (q(json!({"type": "scale", "from": 1, "to": 1})), "2 to 10"),
        (
            q(json!({"type": "scale", "to": 5, "default": "6"})),
            "above the last point",
        ),
        (
            q(json!({"type": "date", "minDate": "2026-02-30"})),
            "not a day",
        ),
        (
            q(json!({"type": "date", "minDate": "2026-12-31", "maxDate": "2026-12-01"})),
            "after its `maxDate`",
        ),
        (
            q(json!({"type": "date", "maxDate": "2026-12-01", "default": "2026-12-02"})),
            "after the last day",
        ),
        (q(json!({"type": "rank"})), "needs 2 to 12"),
        (
            q(json!({"type": "rank", "options": ["a"]})),
            "needs 2 to 12",
        ),
        (
            q(json!({"type": "rank", "options": ["a", "b"], "default": "a"})),
            "a list of every option once",
        ),
        (
            q(json!({"type": "rank", "options": ["a", "b"], "default": ["a"]})),
            "not an answer the question accepts",
        ),
        (
            q(
                json!({"type": "rank", "options": [{"label": "A", "image": {"sha256": "a".repeat(64), "alt": "x"}}, "B"]}),
            ),
            "does not draw",
        ),
    ];
    for (call, expected) in cases {
        let out = refused(Some(&v7()), call.clone()).await;
        assert!(out.contains(expected), "{expected}: {out}\n{call}");
    }
}

// ---- a screen that lacks the kinds ----

fn texts(root: &Value) -> Vec<(String, String)> {
    root["questions"]
        .as_array()
        .unwrap()
        .iter()
        .map(|q| {
            (
                q["type"].as_str().unwrap_or("-").to_owned(),
                q["id"].as_str().unwrap().to_owned(),
            )
        })
        .collect()
}

#[tokio::test]
async fn a_screen_of_version_6_gets_the_nearest_form_and_no_new_type() {
    for screen in [v6(), v5()] {
        let (_, root) = form(&screen, five()).await;
        let questions = root["questions"].as_array().unwrap();
        // No type that screen does not have, and the instance is valid for its own schema.
        for q in questions {
            assert!(
                ["single", "text"].contains(&q["type"].as_str().unwrap()),
                "{q}"
            );
        }
        assert_eq!(
            texts(&root)
                .iter()
                .map(|(t, _)| t.as_str())
                .collect::<Vec<_>>(),
            ["single", "text", "single", "text", "text"]
        );
        // confirm: radio buttons yes and no, the safe one chosen
        assert_eq!(
            questions[0]["options"],
            json!([{"value": "yes", "label": "Delete it"}, {"value": "no", "label": "Keep it"}])
        );
        assert_eq!(questions[0]["default"], "no");
        // number and date: words, said in the help
        assert_eq!(
            questions[1]["help"],
            "A number from 0 to 1000 EUR, in steps of 0.5."
        );
        // the box starts with the default, as a typed question does
        assert_eq!(questions[1]["default"], "250");
        assert_eq!(
            questions[3]["help"],
            "A day, YYYY-MM-DD, from 2026-12-01 to 2026-12-31."
        );
        // scale: one option per point, the ends carrying the words; optional stays optional
        let points: Vec<&str> = questions[2]["options"]
            .as_array()
            .unwrap()
            .iter()
            .map(|o| o["label"].as_str().unwrap())
            .collect();
        assert_eq!(points.len(), 11);
        assert_eq!(points[0], "0 - Not at all");
        assert_eq!(points[10], "10 - Certainly");
        assert_eq!(points[5], "5");
        assert_eq!(questions[2]["required"], false);
        // rank: the help lists the options with a number each and asks for the numbers in order
        let help = questions[4]["help"].as_str().unwrap();
        assert!(
            help.starts_with("Put the options in order, the best first"),
            "{help}"
        );
        assert!(
            help.ends_with("1) Speed 2) Cost 3) Risk The most important first."),
            "{help}"
        );
    }
}

#[tokio::test]
async fn a_screen_of_version_2_asks_what_fits_and_otherwise_the_form_is_words() {
    // A yes or no and a short scale are radio buttons, which that screen has.
    let (_, root) = form(
        &v2(),
        json!({"question": "Q", "choices": [
            {"id": "ship", "question": "Delete it?", "type": "confirm", "default": "no"},
            {"id": "nps", "question": "How good?", "type": "scale", "to": 5}]}),
    )
    .await;
    assert_eq!(root["questions"][0]["options"][0]["value"], "yes");
    assert_eq!(root["questions"][1]["options"].as_array().unwrap().len(), 5);
    assert!(root["questions"][0].get("type").is_none());
    // A scale of 11 points is more options than that screen takes, and a number is a typed answer:
    // both go as words.
    let text = words(
        Some(&v2()),
        json!({"question": "Q", "choices": [{"id": "n", "question": "How good?", "type": "scale", "to": 10, "from": 0}]}),
    )
    .await;
    assert!(
        text.contains("1. How good? (a whole number from 0 to 10)"),
        "{text}"
    );
    let text = words(Some(&v2()), five()).await;
    assert!(text.contains("2. What is the monthly budget?"), "{text}");
}

#[tokio::test]
async fn a_rank_whose_options_do_not_fit_a_help_line_is_words_on_a_screen_without_it() {
    let long = "x".repeat(110);
    let options: Vec<Value> = (0..4).map(|n| json!(format!("{long} {n}"))).collect();
    let text = words(
        Some(&v6()),
        one(json!({"id": "r", "question": "Order", "type": "rank", "options": options})),
    )
    .await;
    assert!(
        text.contains("1. Order (put the options in order"),
        "{text}"
    );
    assert!(text.contains("\n   4) "), "{text}");
    // On a screen that has the kind the same call is a form.
    let (_, root) = form(
        &v7(),
        one(json!({"id": "r", "question": "Order", "type": "rank", "options": (0..4).map(|n| json!(format!("{long} {n}"))).collect::<Vec<_>>()})),
    )
    .await;
    assert_eq!(root["questions"][0]["type"], "rank");
}

// ---- words ----

#[tokio::test]
async fn a_screen_with_no_catalog_gets_each_kind_in_words() {
    let text = words(None, five()).await;
    for expected in [
        "1. Delete the old branch? (It cannot be restored.) (yes: \"Delete it\" / no: \"Keep it\"; the safe answer is no)",
        "2. What is the monthly budget? (a number from 0 to 1000 EUR, in steps of 0.5)",
        "3. How likely are you to recommend it? (a whole number from 0 \"Not at all\" to 10 \"Certainly\")",
        "4. When should it ship? (a day, YYYY-MM-DD, from 2026-12-01 to 2026-12-31)",
        "5. Put these in order of importance (The most important first.) (put the options in order, the best first: answer with their numbers in order, like 2, 1, 3)",
        "\n   1) Speed",
        "\n   2) Cost",
        "\n   3) Risk - What can go wrong",
    ] {
        assert!(text.contains(expected), "{expected}\n---\n{text}");
    }
}

#[tokio::test]
async fn the_words_of_a_number_and_a_day_with_one_bound_or_none() {
    let said = |q: Value| async move { words(None, one(q)).await };
    let text =
        said(json!({"id": "a", "question": "How much?", "type": "number", "unit": "EUR"})).await;
    assert!(text.contains("(a number in EUR)"), "{text}");
    let text =
        said(json!({"id": "a", "question": "How much?", "type": "number", "minValue": "1"})).await;
    assert!(text.contains("(a number of at least 1)"), "{text}");
    let text = said(json!({"id": "a", "question": "How much?", "type": "number", "maxValue": "9", "unit": "kg"})).await;
    assert!(text.contains("(a number of at most 9 kg)"), "{text}");
    let text =
        said(json!({"id": "a", "question": "When?", "type": "date", "minDate": "2026-01-01"}))
            .await;
    assert!(
        text.contains("(a day, YYYY-MM-DD, on or after 2026-01-01)"),
        "{text}"
    );
    let text = said(json!({"id": "a", "question": "Sure?", "type": "confirm"})).await;
    assert!(text.contains("(yes: \"Yes\" / no: \"No\")"), "{text}");
}

// ---- the schema the model reads is one value ----

#[test]
fn the_spec_is_the_same_whatever_the_screen_and_names_the_five_kinds() {
    let a = Ui::new(McpPolicy::default())
        .tools()
        .get("ask_user")
        .cloned()
        .unwrap()
        .spec();
    let b = Ui::new(McpPolicy::default())
        .tools()
        .get("ask_user")
        .cloned()
        .unwrap()
        .spec();
    assert_eq!(
        serde_json::to_string(&a).unwrap(),
        serde_json::to_string(&b).unwrap()
    );
    for kind in ["confirm", "number", "scale", "date", "rank"] {
        assert!(a.description.contains(&format!("`{kind}`")), "{kind}");
    }
}
