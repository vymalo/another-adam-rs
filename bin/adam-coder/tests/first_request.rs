//! S0 of the lean-first-request plan (`plans/2026-11-21-lean-first-request.md`): the coder is
//! **eager by decision**, with two limits that only go down — its tools at most **3,000 tokens**
//! and its prompt may not grow. This test measures what the model is sent (the system prompt and
//! every tool schema as the OpenAI wire wraps it), prints the table row, and refuses growth past
//! the pinned baseline; the distance to the 3,000-token tool budget is printed until S3 makes the
//! coder lean.
//!
//! The plan's numbers are `cl100k` tokens; CI has no tokenizer, so the pins are in **bytes** and
//! the print gives the repository's own estimate (`chars / 4`, about 6 % above `cl100k`). The
//! message is not counted: it is the same "Hi" in every measurement.
#![allow(clippy::unwrap_used, clippy::expect_used)] // integration tests assert by unwrapping

mod common;

use std::sync::Arc;
use std::time::{Duration, Instant};

use adam_coder::{AGENT_NAME, CoderAgent};
use adam_core::{DynStore, MemoryStore, RunStatus};
use adam_llm_agent::user_message;
use adam_model::{DynModel, MockModel, ModelRequest};
use adam_runtime::Runtime;
use common::Fixture;

/// The coder's first request, measured on the commit that pinned it. A kilobyte of growth is
/// allowed (a tool's description may gain a sentence); anything more is a reviewed decision.
// The old pin (30814, measured 2026-11-21 on the merged head b16c076a) plus 1766 bytes: "\n\n" and
// the 1696 bytes of `SessionTools::guide_for(["explorer", "reviewer"])` (ADR 0054 decision 9), plus
// the 68 bytes the `delete` argument of `subagent_close` added to that section (slice 5), plus the
// 11 bytes `command | commands` added to `run_checks`'s line (ANR-243), plus the 271 bytes
// `subagent_edit`'s line adds to it (ANR-216, slice 4). The tools grew by that tool's 2095-byte
// spec (ANR-216), then by the 148 bytes `skills` adds to `subagent_start` and `subagent_edit`
// (ANR-214, slice 7), and the guide by the 10 bytes of "`, `skills`". The deltas are computed, not
// measured: re-pin this from the `system N B` line this test prints on the first green CI run.
const BASELINE_PROMPT_BYTES: usize = 32872; // 32862 + the 10 bytes `skills` adds to the guide's options line
const BASELINE_TOOLS_BYTES: usize = 67143; // 66847 + 148 bytes in each of `subagent_start` and `subagent_edit` (ANR-214)
const GROWTH_ALLOWED: usize = 1024;
/// The owner's budget for the coder's tools once S3 makes it lean (ADR 0066).
const TOOLS_BUDGET_TOKENS: usize = 3000;

/// `(system, tools)` in bytes, the tools wrapped as `{"type":"function","function":{...}}`.
fn request_bytes(req: &ModelRequest) -> (usize, usize) {
    let system = req.system.as_deref().map_or(0, str::len);
    let tools = req
        .tools
        .iter()
        .map(|t| {
            serde_json::to_vec(&serde_json::json!({
                "type": "function",
                "function": {
                    "name": t.name,
                    "description": t.description,
                    "parameters": t.parameters,
                }
            }))
            .expect("a tool schema serialises")
            .len()
        })
        .sum();
    (system, tools)
}

/// One line per tool, sorted by name: the generated part of
/// `plans/2026-11-21-tool-loading-table.md`. A new tool, a renamed one or a changed schema changes
/// this table, so the golden file changes and a reviewer sees it.
fn tool_table(req: &ModelRequest) -> String {
    let mut rows: Vec<(String, usize)> = req
        .tools
        .iter()
        .map(|t| {
            let bytes = serde_json::to_vec(&serde_json::json!({
                "type": "function",
                "function": {
                    "name": t.name,
                    "description": t.description,
                    "parameters": t.parameters,
                }
            }))
            .expect("a tool schema serialises")
            .len();
            (t.name.clone(), bytes)
        })
        .collect();
    rows.sort_by(|a, b| a.0.cmp(&b.0));
    let mut out = String::from("# tool\tbytes\ttokens(chars/4)\n");
    for (name, bytes) in rows {
        out.push_str(&format!("{name}\t{bytes}\t{}\n", bytes / 4));
    }
    out
}

/// Compare the generated table with its golden file, or rewrite it with `UPDATE_GOLDEN=1`.
fn check_golden(generated: &str) {
    let path = concat!(
        env!("CARGO_MANIFEST_DIR"),
        "/tests/fixtures/tool-tokens.txt"
    );
    if std::env::var_os("UPDATE_GOLDEN").is_some() {
        std::fs::write(path, generated).expect("the golden is writable");
        println!("updated {path}");
        return;
    }
    let golden = std::fs::read_to_string(path).unwrap_or_default();
    assert_eq!(
        golden, generated,
        "the tool table changed. If the change is deliberate, regenerate it with \
         `UPDATE_GOLDEN=1 cargo test -p adam-coder --test first_request` and commit the diff; \
         a new tool must never arrive without its row."
    );
}

#[tokio::test]
async fn the_coders_first_request_does_not_grow() {
    let fx = Fixture::new().await.offering_creation();
    let mock = Arc::new(MockModel::new());
    mock.push_text("Hello. What would you like me to work on?");
    let model: DynModel = mock.clone();
    let agent = CoderAgent::new(model, "test-model", fx.env.clone());

    let store: DynStore = Arc::new(MemoryStore::new());
    let rt = Runtime::builder(store)
        .agent(agent)
        .poll_interval(Duration::from_millis(20))
        .build();
    let (stop, rx) = tokio::sync::oneshot::channel::<()>();
    let worker = {
        let rt = rt.clone();
        tokio::spawn(async move {
            rt.run_worker(async {
                let _ = rx.await;
            })
            .await
        })
    };
    let run = rt
        .start(AGENT_NAME, user_message("Hi"), None)
        .await
        .expect("the run starts");
    let deadline = Instant::now() + Duration::from_secs(20);
    loop {
        let view = rt.view(run).await.unwrap().expect("run exists");
        match view.status {
            // A greeting with no repository work behind it is the turn's end, or a park that asks
            // the person: both leave the first request as it was.
            RunStatus::Done | RunStatus::Parked => break,
            RunStatus::Failed => panic!("the run failed: {view:#?}"),
            _ => {}
        }
        assert!(Instant::now() < deadline, "timed out: {view:#?}");
        tokio::time::sleep(Duration::from_millis(5)).await;
    }
    let _ = stop.send(());
    worker.await.unwrap().unwrap();

    let requests = mock.requests();
    assert!(!requests.is_empty(), "the model was asked");
    let first = &requests[0];
    let (system, tools) = request_bytes(first);
    let total = system + tools;
    // Written before the asserts so one `UPDATE_GOLDEN=1` run both prints the numbers and
    // records the table, even while the baseline is still unpinned.
    check_golden(&tool_table(first));
    println!(
        "adam-coder first request: system {system} B (~{} tokens) + tools {tools} B \
         (~{} tokens) = {total} B (~{} tokens at chars/4); \
         budgets: tools {TOOLS_BUDGET_TOKENS} tokens (~{} B), prompt no-growth \
         (baseline {BASELINE_PROMPT_BYTES} B, tools baseline {BASELINE_TOOLS_BYTES} B, \
         allowed +{GROWTH_ALLOWED} B each)",
        system / 4,
        tools / 4,
        total / 4,
        TOOLS_BUDGET_TOKENS * 4
    );
    assert!(
        system <= BASELINE_PROMPT_BYTES + GROWTH_ALLOWED,
        "the coder's prompt grew to {system} B (baseline {BASELINE_PROMPT_BYTES} B, allowed \
         +{GROWTH_ALLOWED} B). A deliberate growth updates the baseline in the same commit; \
         anything else is the regression this test exists for."
    );
    assert!(
        tools <= BASELINE_TOOLS_BYTES + GROWTH_ALLOWED,
        "the coder's tool schemas grew to {tools} B (baseline {BASELINE_TOOLS_BYTES} B, allowed \
         +{GROWTH_ALLOWED} B). A deliberate growth updates the baseline in the same commit; \
         anything else is the regression this test exists for."
    );
}
