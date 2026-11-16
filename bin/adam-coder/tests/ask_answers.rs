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

fn screen() -> Map<String, Value> {
    let lock: Value = serde_json::from_str(V7_LOCK).unwrap();
    let claimed = json!({
        "catalogId": "https://agents.vymalo.com/a2ui/catalogs/chat",
        "version": lock["version"], "digest": lock["digest"]});
    let mut inline = claimed.clone();
    inline["catalog"] = serde_json::from_str(V7).unwrap();
    json!({
        adam_a2a_runtime::CONTEXT_UI_REF: claimed,
        adam_a2a_runtime::CONTEXT_UI_CATALOG: inline,
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
