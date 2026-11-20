//! The coder's `ask_user` is wrapped twice (the access tier's gate and the redactor): the answer to
//! a form is still read by the tool that asked, through both. A wrapper that forgot to forward
//! `Tool::read_answer` would accept every answer, so this refuses one.
#![allow(clippy::unwrap_used, clippy::expect_used)] // integration tests assert by unwrapping

mod common;

use std::sync::Arc;

use adam_coder::coder_tools;
use adam_llm_agent::{AnswerCheck, ToolCtx, ToolError};
use adam_runtime::CollectingSink;
use common::Fixture;
use serde_json::{Map, Value, json};

const V7: &str = include_str!("../../../crates/adam-ui/tests/fixtures/catalog-v7.json");
const V7_LOCK: &str = include_str!("../../../crates/adam-ui/tests/fixtures/catalog-v7.lock.json");
const V8: &str = include_str!("../../../crates/adam-ui/tests/fixtures/catalog-v8.json");
const V8_LOCK: &str = include_str!("../../../crates/adam-ui/tests/fixtures/catalog-v8.lock.json");
const V5: &str = include_str!("../../../crates/adam-ui/tests/fixtures/catalog-v5.json");
const V5_LOCK: &str = include_str!("../../../crates/adam-ui/tests/fixtures/catalog-v5.lock.json");

fn screen() -> Map<String, Value> {
    screen_of(V7, V7_LOCK)
}

fn screen_of(document: &str, lock: &str) -> Map<String, Value> {
    let lock: Value = serde_json::from_str(lock).unwrap();
    // A screen sends a `ui-catalog/v2` document: the published catalogs before 9 get its `protocolVersion`
    // and the digest is of what is sent.
    let mut document: Value = serde_json::from_str(document).unwrap();
    document["protocolVersion"] = json!("1.0");
    let claimed = json!({
        "catalogId": "https://agents.vymalo.com/a2ui/catalogs/chat",
        "version": lock["version"], "digest": adam_ui::catalog_digest(&document).unwrap()});
    let mut inline = claimed.clone();
    inline["catalog"] = document;
    json!({
        adam_a2a_runtime::CONTEXT_UI_REF: claimed,
        adam_a2a_runtime::CONTEXT_UI_CATALOG: inline,
        adam_a2a_runtime::CONTEXT_A2UI_RENDERER: {
            "catalogIds": ["https://agents.vymalo.com/a2ui/catalogs/chat"]},
    })
    .as_object()
    .cloned()
    .unwrap()
}

#[tokio::test]
async fn the_answer_to_a_form_is_read_by_ask_user_through_the_coders_wrappers() {
    let fx = Fixture::new().await;
    let tools = coder_tools(&fx.env);
    let tool = tools
        .get("ask_user")
        .expect("the coder has ask_user")
        .clone();
    let args = json!({
        "question": "How much?",
        "choices": [{"id": "budget", "question": "What is the budget?", "type": "number",
                     "minValue": "0", "maxValue": "1000", "unit": "EUR"}]
    });
    let ctx = ToolCtx::detached("ask_user", "call-1", Arc::new(CollectingSink::new()))
        .with_state(fx.env.clone())
        .with_context(screen());
    let Err(ToolError::NeedsInput { ui: Some(ui), .. }) = tool.call(&ctx, args.clone()).await
    else {
        panic!("ask_user did not ask with a form");
    };
    let answer = |value: &str| {
        json!({"text": "x", "form": {"surface": "ask-call-1", "context": {
            "answers": [{"id": "budget", "values": [value]}]}}})
    };
    let AnswerCheck::Refuse(refusal) =
        tool.read_answer("call-1", &args, Some(&ui), &answer("1500"))
    else {
        panic!("a budget above its most was accepted: a wrapper does not forward `read_answer`");
    };
    assert_eq!(refusal.rule, "above_max");
    assert!(
        refusal
            .question
            .contains("1500 is above the most, 1000 EUR"),
        "{}",
        refusal.question
    );
    assert_eq!(
        tool.read_answer("call-1", &args, Some(&ui), &answer("500")),
        AnswerCheck::Accept
    );
}

#[tokio::test]
async fn what_a_refusal_and_an_ending_say_never_repeats_a_secret_the_redactor_hides() {
    let fx = Fixture::new().await;
    let tools = coder_tools(&fx.env);
    let tool = tools.get("ask_user").expect("ask_user").clone();
    // The model wrote secrets into the words the refusal quotes: the question, the failed question's
    // words, its unit and its help.
    let args = json!({
        "question": format!("Deploy with {}?", common::GITHUB_TOKEN),
        "description": format!("Key {}", common::MODEL_KEY),
        "choices": [{"id": "budget", "question": format!("Budget for {}?", common::DB_PASSWORD),
                     "type": "number", "maxValue": "1000", "unit": common::A2A_TOKEN,
                     "help": format!("About {}", common::DB_PASSWORD)}]
    });
    let ctx = ToolCtx::detached("ask_user", "call-1", Arc::new(CollectingSink::new()))
        .with_state(fx.env.clone())
        .with_context(screen());
    let Err(ToolError::NeedsInput {
        ui: Some(mut ui), ..
    }) = tool.call(&ctx, args.clone()).await
    else {
        panic!("ask_user did not ask with a form");
    };
    let bad = json!({"text": "x", "form": {"surface": "ask-call-1", "context": {
        "answers": [{"id": "budget", "values": ["1500"]}]}}});
    let clean = |what: &str, text: &str| {
        for secret in common::SECRETS {
            assert!(!text.contains(secret), "{what} repeats {secret}: {text}");
        }
    };
    let mut ended = None;
    for attempt in 1..=6 {
        let surface = ui[0]["createSurface"]["surfaceId"]
            .as_str()
            .unwrap()
            .to_owned();
        let mut answer = bad.clone();
        answer["form"]["surface"] = json!(surface);
        match tool.read_answer("call-1", &args, Some(&ui), &answer) {
            AnswerCheck::Refuse(refusal) => {
                assert!(attempt <= 5, "refused a sixth time");
                assert!(
                    refusal.question.contains("above the most, 1000"),
                    "{}",
                    refusal.question
                );
                clean("the question asked again", &refusal.question);
                clean(
                    "the form asked again",
                    &refusal.ui.as_ref().unwrap().to_string(),
                );
                ui = refusal.ui.unwrap();
            }
            AnswerCheck::End(ending) => {
                ended = Some(ending);
                break;
            }
            other => panic!("{other:?}"),
        }
    }
    let ending = ended.expect("the sixth bad answer ends the question");
    assert!(
        ending.question.contains("did not fit the form"),
        "{}",
        ending.question
    );
    clean("the ending", &ending.question);
}

/// What a question shows is the model's content drawn next to a question the person answers (a
/// confirm that shows a diff is the consent case): the redactor scrubs it as it scrubs the question,
/// on a screen that draws it with the form, on one that draws it above, on one that has it in words,
/// and in every form asked again.
#[tokio::test]
async fn a_secret_in_what_a_question_shows_reaches_neither_the_surface_nor_the_question() {
    let fx = Fixture::new().await;
    let tools = coder_tools(&fx.env);
    let tool = tools.get("ask_user").expect("ask_user").clone();
    let args = json!({
        "question": "Apply the change?",
        "choices": [{"id": "budget", "question": "What is the budget?", "type": "number",
                     "maxValue": "1000",
                     "show": [
                        {"component": "Code", "language": "diff",
                         "code": format!("-token = old\n+token = {}\n", common::GITHUB_TOKEN)},
                        {"component": "Text", "text": format!("The key is {}", common::MODEL_KEY)},
                        {"component": "Cards", "cards": [{"title": "Db", "body": common::DB_PASSWORD}]},
                        {"component": "Callout", "title": common::A2A_TOKEN, "text": "ok"}]}]
    });
    let clean = |what: &str, text: &str| {
        for secret in common::SECRETS {
            assert!(!text.contains(secret), "{what} repeats {secret}: {text}");
        }
    };
    for (version, document, lock) in [(8, V8, V8_LOCK), (7, V7, V7_LOCK), (5, V5, V5_LOCK)] {
        let ctx = ToolCtx::detached("ask_user", "call-1", Arc::new(CollectingSink::new()))
            .with_state(fx.env.clone())
            .with_context(screen_of(document, lock));
        let Err(ToolError::NeedsInput {
            question,
            ui: Some(mut ui),
        }) = tool.call(&ctx, args.clone()).await
        else {
            panic!("version {version}: ask_user did not ask with a form");
        };
        clean(&format!("version {version}: the question"), &question);
        clean(&format!("version {version}: the form"), &ui.to_string());
        // The shown content is there, with the secret taken out: the scrub reached it.
        let shown = format!("{question}{ui}");
        assert!(
            shown.contains("token = [redacted]"),
            "version {version}: {shown}"
        );
        assert!(
            shown.contains("The key is [redacted]"),
            "version {version}: {shown}"
        );
        for attempt in 1..=3 {
            let surface = ui[0]["createSurface"]["surfaceId"]
                .as_str()
                .unwrap()
                .to_owned();
            let answer = json!({"text": "x", "form": {"surface": surface, "context": {
                "answers": [{"id": "budget", "values": ["1500"]}]}}});
            let AnswerCheck::Refuse(refusal) =
                tool.read_answer("call-1", &args, Some(&ui), &answer)
            else {
                panic!("version {version}: a budget above its most was accepted");
            };
            clean(
                &format!("version {version}: the question asked again"),
                &refusal.question,
            );
            clean(
                &format!("version {version}: the form asked again"),
                &refusal.ui.as_ref().unwrap().to_string(),
            );
            let again = format!("{}{}", refusal.question, refusal.ui.as_ref().unwrap());
            assert!(
                again.contains("token = [redacted]"),
                "version {version}, attempt {attempt}: the same things are shown again: {again}"
            );
            ui = refusal.ui.unwrap();
        }
    }
}
