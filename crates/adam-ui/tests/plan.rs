//! `plan_work`, `plan_check` and `plan_add` (ADR 0044 decision 5): the exact words of every result and
//! refusal, the limits, and the surface each call draws on the screens that can draw one: a `Checklist`
//! when the screen's catalog validates one, a `Column` of `Text`s when it has those, nothing
//! otherwise. The catalogs are the fixtures in `tests/fixtures` (see its README for the two that
//! are hand-written).
#![allow(clippy::unwrap_used, clippy::expect_used)] // integration tests assert by unwrapping

use std::sync::Arc;

use adam_a2a::A2UI_MEDIA_TYPE;
use adam_a2a_runtime::{CONTEXT_UI_CATALOG, CONTEXT_UI_REF};
use adam_core::RunId;
use adam_llm_agent::{Plan, PlanStatus, ToolCtx, ToolOutput};
use adam_mcp::McpPolicy;
use adam_runtime::NoopSink;
use adam_ui::{Claimed, Ui};
use serde_json::{Map, Value, json};

const CATALOG_ID: &str = "https://agents.vymalo.com/a2ui/catalogs/chat";

/// A catalog fixture and its lock.
struct Fixture {
    document: &'static str,
    lock: &'static str,
}

const V5: Fixture = Fixture {
    document: include_str!("fixtures/catalog-v5.json"),
    lock: include_str!("fixtures/catalog-v5.lock.json"),
};
const V6: Fixture = Fixture {
    document: include_str!("fixtures/catalog-v6.json"),
    lock: include_str!("fixtures/catalog-v6.lock.json"),
};
const V7: Fixture = Fixture {
    document: include_str!("fixtures/catalog-v7.json"),
    lock: include_str!("fixtures/catalog-v7.lock.json"),
};

impl Fixture {
    /// The context of a message that carried this catalog inline.
    fn context(&self) -> Map<String, Value> {
        let lock: Value = serde_json::from_str(self.lock).unwrap();
        let claimed = Claimed {
            catalog_id: CATALOG_ID.to_owned(),
            version: u32::try_from(lock["version"].as_u64().unwrap()).unwrap(),
            digest: lock["digest"].as_str().unwrap().to_owned(),
        };
        let reference = json!({"catalogId": claimed.catalog_id, "version": claimed.version, "digest": claimed.digest});
        json!({
            CONTEXT_UI_REF: reference,
            CONTEXT_UI_CATALOG: {
                "catalogId": claimed.catalog_id, "version": claimed.version, "digest": claimed.digest,
                "catalog": serde_json::from_str::<Value>(self.document).unwrap()},
        })
        .as_object()
        .cloned()
        .unwrap()
    }
}

fn ui() -> Ui {
    Ui::new(McpPolicy::default())
}

/// A call of `name` that starts from `plan`, on a screen with `context`.
async fn call(
    name: &str,
    plan: Option<&Plan>,
    context: Map<String, Value>,
    args: Value,
) -> ToolOutput {
    let tool = ui().tools().get(name).cloned().unwrap();
    let ctx = ToolCtx::detached(name, "c1", Arc::new(NoopSink))
        .with_context(context)
        .with_plan(plan.cloned());
    tool.call(&ctx, args)
        .await
        .expect("an output, not an error")
}

/// The same, on a screen with no catalog.
async fn call_bare(name: &str, plan: Option<&Plan>, args: Value) -> ToolOutput {
    call(name, plan, Map::new(), args).await
}

fn plan_of(output: &ToolOutput) -> Plan {
    *output.plan.clone().expect("the call returned a plan")
}

const ITEMS: [&str; 4] = [
    "Reproduce the failing test",
    "Fix parse_date for time zones",
    "Run the checks",
    "Open the pull request",
];

fn started() -> Value {
    json!({"title": "Fix the date parser", "items": ITEMS})
}

/// The A2UI components the output's surface draws, by id.
fn components_of(output: &ToolOutput) -> Vec<Value> {
    assert_eq!(output.artifacts.len(), 1, "one surface: {output:?}");
    let artifact = &output.artifacts[0];
    assert_eq!(artifact.name, "plan");
    assert_eq!(artifact.mime_type.as_deref(), Some(A2UI_MEDIA_TYPE));
    artifact.data.as_array().unwrap()[1]["updateComponents"]["components"]
        .as_array()
        .unwrap()
        .clone()
}

fn surface_id(output: &ToolOutput) -> String {
    output.artifacts[0].data[0]["createSurface"]["surfaceId"]
        .as_str()
        .unwrap()
        .to_owned()
}

#[tokio::test]
async fn plan_work_returns_the_exact_text() {
    let out = call_bare("plan_work", None, started()).await;
    assert!(!out.is_error);
    assert_eq!(
        out.content,
        "Plan saved (4 items).\n\
         Plan: Fix the date parser\n\
         1. [ ] Reproduce the failing test\n\
         2. [ ] Fix parse_date for time zones\n\
         3. [ ] Run the checks\n\
         4. [ ] Open the pull request\n\
         0 of 4 done, 0 doing, 4 to do, 0 dropped."
    );
    let plan = plan_of(&out);
    assert_eq!(plan.title.as_deref(), Some("Fix the date parser"));
    assert_eq!(plan.items.len(), 4);
    assert_eq!(plan.items[3].id, 4);

    // The same state gives the same bytes.
    let again = call_bare("plan_work", None, started()).await;
    assert_eq!(again.content, out.content);

    // A second plan_work replaces the plan and starts at 1 again.
    let replaced = call_bare(
        "plan_work",
        Some(&plan),
        json!({"items": ["Only this", "And this"]}),
    )
    .await;
    assert_eq!(
        replaced.content,
        "Plan replaced (2 items).\nPlan\n1. [ ] Only this\n2. [ ] And this\n0 of 2 done, 0 doing, 2 to do, 0 dropped."
    );
    assert_eq!(plan_of(&replaced).items[0].id, 1);
}

#[tokio::test]
async fn plan_check_sets_one_item_and_returns_the_whole_plan() {
    let plan = plan_of(&call_bare("plan_work", None, started()).await);
    let one = call_bare(
        "plan_check",
        Some(&plan),
        json!({"id": 1, "status": "done"}),
    )
    .await;
    let two = call_bare(
        "plan_check",
        Some(&plan_of(&one)),
        json!({"id": 2, "status": "doing", "note": "lexer.rs"}),
    )
    .await;
    let four = call_bare(
        "plan_check",
        Some(&plan_of(&two)),
        json!({"id": 4, "status": "dropped", "note": "the person opens it"}),
    )
    .await;
    assert_eq!(
        two.content,
        "Item 2: doing.\n\
         Plan: Fix the date parser\n\
         1. [x] Reproduce the failing test\n\
         2. [>] Fix parse_date for time zones (note: lexer.rs)\n\
         3. [ ] Run the checks\n\
         4. [ ] Open the pull request\n\
         1 of 4 done, 1 doing, 2 to do, 0 dropped."
    );
    assert_eq!(
        four.content,
        "Item 4: dropped.\n\
         Plan: Fix the date parser\n\
         1. [x] Reproduce the failing test\n\
         2. [>] Fix parse_date for time zones (note: lexer.rs)\n\
         3. [ ] Run the checks\n\
         4. [-] Open the pull request (note: the person opens it)\n\
         1 of 4 done, 1 doing, 1 to do, 1 dropped."
    );
    assert_eq!(plan_of(&four).items[3].status, PlanStatus::Dropped);
}

#[tokio::test]
async fn plan_check_without_a_plan_says_to_start_one() {
    for (name, args) in [
        ("plan_check", json!({"id": 1, "status": "done"})),
        ("plan_add", json!({"items": ["x"]})),
    ] {
        let out = call_bare(name, None, args).await;
        assert!(out.is_error, "{name}");
        assert_eq!(
            out.content,
            "There is no plan yet: start one with plan_work."
        );
        assert!(out.plan.is_none());
        assert!(out.artifacts.is_empty());
    }
}

#[tokio::test]
async fn plan_check_on_an_unknown_id_is_an_error_and_changes_nothing() {
    let plan = plan_of(&call_bare("plan_work", None, started()).await);
    let out = call(
        "plan_check",
        Some(&plan),
        V6.context(),
        json!({"id": 7, "status": "done"}),
    )
    .await;
    assert!(out.is_error);
    assert_eq!(
        out.content,
        "There is no item 7: the items are 1 to 4.\n\
         Plan: Fix the date parser\n\
         1. [ ] Reproduce the failing test\n\
         2. [ ] Fix parse_date for time zones\n\
         3. [ ] Run the checks\n\
         4. [ ] Open the pull request\n\
         0 of 4 done, 0 doing, 4 to do, 0 dropped."
    );
    assert!(out.plan.is_none(), "an error carries no plan");
    assert!(out.artifacts.is_empty(), "and draws nothing");
    let zero = call_bare(
        "plan_check",
        Some(&plan),
        json!({"id": 0, "status": "done"}),
    )
    .await;
    assert!(zero.is_error);
    assert!(
        zero.content.starts_with("There is no item 0: "),
        "{}",
        zero.content
    );
}

#[tokio::test]
async fn plan_add_after_inserts_with_the_next_id() {
    let plan = plan_of(&call_bare("plan_work", None, started()).await);
    let out = call_bare(
        "plan_add",
        Some(&plan),
        json!({"items": ["Add a test for UTC+14", "Update the changelog"], "after": 2}),
    )
    .await;
    assert_eq!(
        out.content,
        "Added 2 items: 5, 6.\n\
         Plan: Fix the date parser\n\
         1. [ ] Reproduce the failing test\n\
         2. [ ] Fix parse_date for time zones\n\
         5. [ ] Add a test for UTC+14\n\
         6. [ ] Update the changelog\n\
         3. [ ] Run the checks\n\
         4. [ ] Open the pull request\n\
         0 of 6 done, 0 doing, 6 to do, 0 dropped."
    );
    // Ids are creation numbers: item 3 is still item 3, and a check by id still finds it.
    let checked = call_bare(
        "plan_check",
        Some(&plan_of(&out)),
        json!({"id": 3, "status": "done"}),
    )
    .await;
    assert!(checked.content.contains("3. [x] Run the checks"));
    // At the end, and with one item.
    let end = call_bare("plan_add", Some(&plan_of(&out)), json!({"items": ["Last"]})).await;
    assert!(
        end.content.starts_with("Added 1 item: 7.\n"),
        "{}",
        end.content
    );
    assert!(
        end.content
            .contains("4. [ ] Open the pull request\n7. [ ] Last\n")
    );
    // An unknown `after` is refused like an unknown id.
    let bad = call_bare("plan_add", Some(&plan), json!({"items": ["x"], "after": 9})).await;
    assert!(bad.is_error);
    assert!(
        bad.content
            .starts_with("There is no item 9: the items are 1 to 4.\nPlan: ")
    );
}

#[tokio::test]
async fn fifty_one_items_are_refused() {
    let many: Vec<String> = (1..=51).map(|n| format!("Step {n}")).collect();
    let out = call_bare("plan_work", None, json!({"items": many})).await;
    assert!(out.is_error);
    assert_eq!(
        out.content,
        "The plan would have 51 items; at most 50. Merge items and call plan_work again."
    );
    assert!(out.plan.is_none());

    let fifty: Vec<String> = (1..=50).map(|n| format!("Step {n}")).collect();
    let full = call_bare("plan_work", None, json!({"items": fifty})).await;
    assert!(!full.is_error, "{}", full.content);
    let over = call_bare(
        "plan_add",
        Some(&plan_of(&full)),
        json!({"items": ["one", "two", "three"]}),
    )
    .await;
    assert!(over.is_error);
    assert_eq!(
        over.content,
        "The plan would have 53 items; at most 50. Drop or merge items, or start again with plan_work."
    );
    assert!(over.plan.is_none());
}

#[tokio::test]
async fn an_item_over_200_characters_is_refused() {
    let long = "é".repeat(240);
    let out = call_bare(
        "plan_work",
        None,
        json!({"items": ["fine", "fine too", long]}),
    )
    .await;
    assert!(out.is_error);
    assert_eq!(out.content, "Item 3 is 240 characters; at most 200.");
    assert!(out.plan.is_none());
    // 200 exactly passes, and the count is of characters, not bytes.
    let edge = call_bare("plan_work", None, json!({"items": ["é".repeat(200)]})).await;
    assert!(!edge.is_error, "{}", edge.content);

    let plan = plan_of(&call_bare("plan_work", None, started()).await);
    let note = call_bare(
        "plan_check",
        Some(&plan),
        json!({"id": 1, "status": "done", "note": "n".repeat(201)}),
    )
    .await;
    assert!(note.is_error);
    assert_eq!(note.content, "The note is 201 characters; at most 200.");
    let title = call_bare(
        "plan_work",
        None,
        json!({"title": "t".repeat(81), "items": ["x"]}),
    )
    .await;
    assert!(title.is_error);
    assert_eq!(title.content, "The title is 81 characters; at most 80.");
    let empty = call_bare("plan_work", None, json!({"items": ["ok", "  \n "]})).await;
    assert!(empty.is_error);
    assert_eq!(empty.content, "Item 2 is empty.");
    let none = call_bare("plan_work", None, json!({"items": []})).await;
    assert!(none.is_error);
    assert_eq!(none.content, "`items` needs 1 to 50 entries; it has 0.");
}

#[tokio::test]
async fn newlines_and_control_characters_become_one_space() {
    let out = call_bare(
        "plan_work",
        None,
        json!({"title": "  A\ntitle ", "items": ["  one\n\ntwo\tthree\u{0}four\u{200b}five "]}),
    )
    .await;
    assert!(!out.is_error);
    let plan = plan_of(&out);
    assert_eq!(plan.title.as_deref(), Some("A title"));
    assert_eq!(plan.items[0].text, "one two three four five");
}

#[tokio::test]
async fn an_id_as_a_string_is_read() {
    let plan = plan_of(&call_bare("plan_work", None, started()).await);
    let out = call_bare(
        "plan_check",
        Some(&plan),
        json!({"id": "3", "status": "doing"}),
    )
    .await;
    assert!(!out.is_error, "{}", out.content);
    assert_eq!(plan_of(&out).items[2].status, PlanStatus::Doing);
    let added = call_bare(
        "plan_add",
        Some(&plan),
        json!({"items": ["x"], "after": "1"}),
    )
    .await;
    assert!(!added.is_error, "{}", added.content);
    assert_eq!(plan_of(&added).items[1].text, "x");
    // Not a number at all is the model's mistake, told in words.
    let bad = call_bare(
        "plan_check",
        Some(&plan),
        json!({"id": "three", "status": "done"}),
    )
    .await;
    assert!(bad.is_error);
    assert!(
        bad.content
            .starts_with("invalid arguments for `plan_check`: "),
        "{}",
        bad.content
    );
    let status = call_bare(
        "plan_check",
        Some(&plan),
        json!({"id": 1, "status": "pending"}),
    )
    .await;
    assert!(status.is_error);
}

#[tokio::test]
async fn a_subagent_has_no_plan() {
    let tool = ui().tools().get("plan_work").cloned().unwrap();
    let ctx = ToolCtx::detached("plan_work", "c1", Arc::new(NoopSink)).with_root_run(RunId::new());
    let out = tool.call(&ctx, started()).await.unwrap();
    assert!(out.is_error);
    assert_eq!(
        out.content,
        "A subagent keeps no plan: put what is left in your answer."
    );
    assert!(out.plan.is_none());
    assert!(out.artifacts.is_empty());
}

#[tokio::test]
async fn with_no_catalog_the_plan_succeeds_as_text() {
    let out = call_bare("plan_work", None, started()).await;
    assert!(!out.is_error);
    assert!(out.artifacts.is_empty(), "no screen, no surface");
    assert!(out.content.starts_with("Plan saved (4 items).\n"));
    assert!(out.plan.is_some());
}

#[tokio::test]
async fn a_screen_with_column_and_text_gets_a_plan_surface() {
    // Version 5 has Text and Column and no Checklist.
    let out = call("plan_work", None, V5.context(), started()).await;
    assert!(!out.is_error, "{}", out.content);
    let id = surface_id(&out);
    assert!(id.starts_with("plan-"), "{id}");
    let components = components_of(&out);
    assert_eq!(components[0]["id"], "root");
    assert_eq!(components[0]["component"], "Column");
    let texts: Vec<&str> = components[1..]
        .iter()
        .map(|c| {
            assert_eq!(c["component"], "Text");
            c["text"].as_str().unwrap()
        })
        .collect();
    // One Text per line of the result block.
    let block: Vec<&str> = out.content.lines().skip(1).collect();
    assert_eq!(texts, block);
    assert_eq!(
        components[0]["children"].as_array().unwrap().len(),
        block.len()
    );
    // The surface is made for this run: the same id on every call of the run, so each call replaces
    // it; another run has another one.
    let tool = ui().tools().get("plan_check").cloned().unwrap();
    let run = ToolCtx::detached("plan_check", "c9", Arc::new(NoopSink)).with_context(V5.context());
    let args = json!({"id": 1, "status": "done"});
    let first = tool
        .call(&run.clone().with_plan(plan_of(&out)), args.clone())
        .await
        .unwrap();
    let second = tool
        .call(
            &run.clone().with_plan(plan_of(&first)),
            json!({"id": 2, "status": "done"}),
        )
        .await
        .unwrap();
    assert_eq!(surface_id(&first), surface_id(&second));
    assert_eq!(surface_id(&first), format!("plan-{}", run.run_id()));
    assert_ne!(surface_id(&first), id);
}

#[tokio::test]
async fn a_screen_with_checklist_gets_a_checklist_surface() {
    let out = call("plan_work", None, V6.context(), started()).await;
    assert!(!out.is_error, "{}", out.content);
    let step1 = call(
        "plan_check",
        Some(&plan_of(&out)),
        V6.context(),
        json!({"id": 1, "status": "done"}),
    )
    .await;
    let step2 = call(
        "plan_check",
        Some(&plan_of(&step1)),
        V6.context(),
        json!({"id": 2, "status": "doing"}),
    )
    .await;
    let components = components_of(&step2);
    assert_eq!(components.len(), 1, "one Checklist, the root");
    assert_eq!(
        components[0],
        json!({
            "id": "root",
            "component": "Checklist",
            "title": "Fix the date parser",
            "items": [
                {"label": "Reproduce the failing test", "state": "done"},
                {"label": "Fix parse_date for time zones", "state": "doing"},
                {"label": "Run the checks", "state": "todo"},
                {"label": "Open the pull request", "state": "todo"},
            ]
        })
    );
    assert!(surface_id(&step2).starts_with("plan-"));
    // A plan with no title is called "Plan"; items follow the display order, not the numbers.
    let added = call(
        "plan_add",
        Some(&plan_of(
            &call(
                "plan_work",
                None,
                V6.context(),
                json!({"items": ["a", "b"]}),
            )
            .await,
        )),
        V6.context(),
        json!({"items": ["c"], "after": 1}),
    )
    .await;
    let components = components_of(&added);
    assert_eq!(components[0]["title"], "Plan");
    let labels: Vec<&str> = components[0]["items"]
        .as_array()
        .unwrap()
        .iter()
        .map(|i| i["label"].as_str().unwrap())
        .collect();
    assert_eq!(labels, ["a", "c", "b"]);
}

#[tokio::test]
async fn a_dropped_item_is_left_out_and_counted_when_the_catalog_has_no_dropped_state() {
    let made = call("plan_work", None, V6.context(), started()).await;
    let dropped = call(
        "plan_check",
        Some(&plan_of(&made)),
        V6.context(),
        json!({"id": 3, "status": "dropped", "note": "not needed"}),
    )
    .await;
    // The model's own result still lists the dropped item with its mark.
    assert!(
        dropped
            .content
            .contains("3. [-] Run the checks (note: not needed)")
    );
    let components = components_of(&dropped);
    assert_eq!(components[0]["title"], "Fix the date parser (1 dropped)");
    let labels: Vec<&str> = components[0]["items"]
        .as_array()
        .unwrap()
        .iter()
        .map(|i| i["label"].as_str().unwrap())
        .collect();
    assert_eq!(
        labels,
        [
            "Reproduce the failing test",
            "Fix parse_date for time zones",
            "Open the pull request"
        ]
    );
    // Every item dropped: a Checklist cannot be empty, so the screen gets the Column and Texts.
    let plan = plan_of(&call("plan_work", None, V6.context(), json!({"items": ["only"]})).await);
    let all = call(
        "plan_check",
        Some(&plan),
        V6.context(),
        json!({"id": 1, "status": "dropped"}),
    )
    .await;
    let components = components_of(&all);
    assert_eq!(components[0]["component"], "Column");
}

#[tokio::test]
async fn a_catalog_that_allows_dropped_and_note_gets_them() {
    let made = call("plan_work", None, V7.context(), started()).await;
    let doing = call(
        "plan_check",
        Some(&plan_of(&made)),
        V7.context(),
        json!({"id": 2, "status": "doing", "note": "lexer.rs"}),
    )
    .await;
    let dropped = call(
        "plan_check",
        Some(&plan_of(&doing)),
        V7.context(),
        json!({"id": 4, "status": "dropped", "note": "the person opens it"}),
    )
    .await;
    let components = components_of(&dropped);
    assert_eq!(
        components[0],
        json!({
            "id": "root",
            "component": "Checklist",
            "title": "Fix the date parser",
            "items": [
                {"label": "Reproduce the failing test", "state": "todo"},
                {"label": "Fix parse_date for time zones", "state": "doing", "note": "lexer.rs"},
                {"label": "Run the checks", "state": "todo"},
                {"label": "Open the pull request", "state": "dropped", "note": "the person opens it"},
            ]
        })
    );
}

#[tokio::test]
async fn a_long_note_is_cut_inside_the_label_limit() {
    // Version 6 has no `note` member: the note goes on the label, and the label has 200 characters.
    let text = "x".repeat(190);
    let made = call(
        "plan_work",
        None,
        V6.context(),
        json!({"items": [text.clone(), "short"]}),
    )
    .await;
    let noted = call(
        "plan_check",
        Some(&plan_of(&made)),
        V6.context(),
        json!({"id": 1, "status": "doing", "note": "n".repeat(60)}),
    )
    .await;
    assert!(!noted.is_error, "{}", noted.content);
    let components = components_of(&noted);
    let labels: Vec<&str> = components[0]["items"]
        .as_array()
        .unwrap()
        .iter()
        .map(|i| i["label"].as_str().unwrap())
        .collect();
    assert_eq!(labels[0].chars().count(), 200, "cut to the limit");
    assert!(
        labels[0].starts_with(&format!("{text} (note: n")),
        "{}",
        labels[0]
    );
    assert!(labels[0].ends_with('…'));
    assert_eq!(labels[1], "short");
    // A short note is appended whole, with no cut.
    let short = call(
        "plan_check",
        Some(&plan_of(&made)),
        V6.context(),
        json!({"id": 2, "status": "done", "note": "ok"}),
    )
    .await;
    let components = components_of(&short);
    assert_eq!(components[0]["items"][1]["label"], "short (note: ok)");
}

#[tokio::test]
async fn the_specs_are_in_the_fixed_order_with_the_exact_schemas() {
    let tools = ui().tools();
    assert_eq!(
        tools.names(),
        [
            "ask_user",
            "show",
            "ui_catalog",
            "plan_work",
            "plan_check",
            "plan_add"
        ]
    );
    let spec = |name: &str| tools.get(name).unwrap().spec();
    assert_eq!(
        spec("plan_work").parameters,
        json!({
            "type": "object",
            "properties": {
                "title": {"type": "string", "maxLength": 80, "description": "A short name for the plan"},
                "items": {
                    "type": "array", "minItems": 1, "maxItems": 50,
                    "description": "The items, in order, one action each",
                    "items": {"type": "string", "minLength": 1, "maxLength": 200}
                }
            },
            "required": ["items"],
            "additionalProperties": false
        })
    );
    assert_eq!(
        spec("plan_check").parameters,
        json!({
            "type": "object",
            "properties": {
                "id": {"type": "integer", "minimum": 1, "description": "The number shown before the item"},
                "status": {"enum": ["doing", "done", "dropped"]},
                "note": {"type": "string", "maxLength": 200, "description": "Why, or where the work is"}
            },
            "required": ["id", "status"],
            "additionalProperties": false
        })
    );
    assert_eq!(
        spec("plan_add").parameters,
        json!({
            "type": "object",
            "properties": {
                "items": {
                    "type": "array", "minItems": 1, "maxItems": 50,
                    "description": "The items to add, in order, one action each",
                    "items": {"type": "string", "minLength": 1, "maxLength": 200}
                },
                "after": {"type": "integer", "minimum": 1, "description": "Show the new items after this item; at the end when left out"}
            },
            "required": ["items"],
            "additionalProperties": false
        })
    );
    assert_eq!(
        spec("plan_check").description,
        "Set one item of the plan: `doing` when you start it, `done` when it is finished, `dropped` when it is no longer needed (say why in `note`). `id` is the number shown before the item. Returns the whole plan."
    );
    assert_eq!(
        spec("plan_add").description,
        "Add items you found on the way, at the end or after item `after`; they get the next numbers. Returns the whole plan."
    );
    assert_eq!(
        spec("plan_work").description,
        "Write the plan for this task as a short checklist the person sees, replacing any earlier plan. Use it when the work has three or more steps or the person asked for several things; not for a question or a one-step change. One action per item, in order, at most 50, each under 200 characters. Then mark progress with plan_check (one item `doing` at a time, `done` as soon as it is) and add what you discover with plan_add. Every call returns the whole plan. Example: {\"title\":\"Fix the date parser\",\"items\":[\"Reproduce the failing test\",\"Fix parse_date for time zones\",\"Run the checks\",\"Open the pull request\"]}"
    );
}

/// The biggest plan a model can write is far under the loop's backstop, so it is never cut.
#[tokio::test]
async fn the_largest_plan_is_under_the_backstop_and_is_not_cut() {
    use adam_llm_agent::paging::{MAX_TOOL_RESULT_BYTES, backstop};
    let items: Vec<String> = (1..=50)
        .map(|n| format!("{n:02}{}", "é".repeat(198)))
        .collect();
    let out = call_bare("plan_work", None, json!({"title": "t", "items": items})).await;
    assert!(!out.is_error, "{}", out.content);
    let mut plan = plan_of(&out);
    for item in plan.items.iter_mut() {
        item.note = Some("n".repeat(200));
    }
    let checked = call_bare(
        "plan_check",
        Some(&plan),
        json!({"id": 1, "status": "doing", "note": "n".repeat(200)}),
    )
    .await;
    assert!(!checked.is_error, "{}", checked.content);
    assert!(checked.content.len() < MAX_TOOL_RESULT_BYTES);
    assert_eq!(
        backstop(&checked.content, "plan_check", false),
        checked.content.as_str()
    );
}
