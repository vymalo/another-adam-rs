//! A run's model calls as `usage/v1` ([ADR 0032](https://github.com/vymalo/another-adam-rs/blob/main/docs/decisions/0032-usage-per-model-call.md)).
//!
//! A completed model call ([`RunEvent::Usage`](adam_runtime::RunEvent::Usage)) is reported, **to a
//! client whose request activated the extension** (`Caller::extensions`), as a `working` status update
//! with **no message** and the report in the event's `metadata` under the extension's URI:
//!
//! ```json
//! {"https://agents.vymalo.com/a2a/extensions/usage/v1": {
//!   "call": "<run>-c7-1a2b3c4d", "stepId": "tool:call_2", "provider": "openai", "model": "glm-5.3",
//!   "inputTokens": 41250, "outputTokens": 812, "totalTokens": 42062,
//!   "reasoningTokens": 300, "cachedInputTokens": 38000, "contextWindow": 131072}}
//! ```
//!
//! A task that ended or waits (`completed`, `failed`, `canceled`, `input-required`) carries its
//! totals in the **task's** `metadata`, for every reader (a poll, `ListTasks`, a stream's snapshot),
//! from the `usage_totals` its agent keeps in its state:
//!
//! ```json
//! {"https://agents.vymalo.com/a2a/extensions/usage/v1": {"totals": [
//!   {"provider": "openai", "model": "glm-5.3", "inputTokens": 512000, "outputTokens": 9100,
//!    "totalTokens": 521100, "reasoningTokens": 2400, "cachedInputTokens": 470000}]}}
//! ```
//!
//! The counts are AG-UI 1.0's `TokenUsage` (parts never additions), each at most 2^53 - 1, and the
//! labels at most 128 bytes (the contract is the orchestration layer's, `docs/api/usage-v1.md` of
//! `vymalo/another-agentic-system`).

use std::collections::HashMap;

use a2a::{TaskState, TaskStatus, TaskStatusUpdateEvent};
use adam_a2a::{TaskEvent, USAGE_EXTENSION};
use adam_runtime::{
    MAX_USAGE_CALL_BYTES, MAX_USAGE_LABEL_BYTES, MAX_USAGE_TOTALS, RunView, Usage, UsageEvent,
    UsageTotals,
};
use serde_json::{Map, Value, json};

/// The largest count a report carries: 2^53 - 1, the largest integer a JSON number keeps exactly.
pub const MAX_TOKEN_COUNT: u64 = 9_007_199_254_740_991;

/// The `working` status update that reports `call` to a client that activated `usage/v1`: no
/// message, so the task's visible state and text do not change, and the report in the event's
/// metadata.
pub(crate) fn call_report(task_id: &str, context_id: &str, call: &UsageEvent) -> TaskEvent {
    TaskEvent::Status(TaskStatusUpdateEvent {
        task_id: task_id.to_owned(),
        context_id: context_id.to_owned(),
        status: TaskStatus {
            state: TaskState::Working,
            message: None,
            timestamp: Some(chrono::Utc::now()),
        },
        metadata: Some(HashMap::from([(USAGE_EXTENSION.to_owned(), report(call))])),
    })
}

/// The members of a call report: `call`, `stepId` when the call ran under a subagent's step, the
/// `TokenUsage` members, and `contextWindow` when the deployment said one.
pub(crate) fn report(call: &UsageEvent) -> Value {
    let mut report = Map::new();
    report.insert("call".into(), cut(&call.call, MAX_USAGE_CALL_BYTES).into());
    if let Some(step) = &call.step {
        report.insert("stepId".into(), cut(step, MAX_USAGE_CALL_BYTES).into());
    }
    token_usage(
        &mut report,
        call.provider.as_deref(),
        &call.model,
        call.usage,
    );
    if let Some(window) = call.context_window {
        report.insert("contextWindow".into(), window.min(MAX_TOKEN_COUNT).into());
    }
    Value::Object(report)
}

/// The task's `metadata` for the totals its agent keeps (`state.usage_totals`, the shape of
/// [`UsageTotals`]), at most 32 entries; `None` when the state keeps none (an agent that counts
/// nothing, a task that made no model call).
pub(crate) fn totals_metadata(view: &RunView) -> Option<HashMap<String, Value>> {
    let totals: UsageTotals =
        serde_json::from_value(view.state.get("usage_totals")?.clone()).ok()?;
    if totals.is_empty() {
        return None;
    }
    let entries: Vec<Value> = totals
        .entries()
        .iter()
        .take(MAX_USAGE_TOTALS)
        .map(|entry| {
            let mut members = Map::new();
            token_usage(
                &mut members,
                entry.provider.as_deref(),
                &entry.model,
                entry.usage,
            );
            Value::Object(members)
        })
        .collect();
    Some(HashMap::from([(
        USAGE_EXTENSION.to_owned(),
        json!({ "totals": entries }),
    )]))
}

/// The members of AG-UI's `TokenUsage` into `members`: the labels cut to 128 bytes and the counts
/// within 2^53 - 1 **and** consistent: `totalTokens` is the sum of the two totals, and each part is
/// within its total (the counts are made to follow the accounting first, [`Usage::accounted`]).
fn token_usage(
    members: &mut Map<String, Value>,
    provider: Option<&str>,
    model: &str,
    usage: Usage,
) {
    if let Some(provider) = provider {
        members.insert(
            "provider".into(),
            cut(provider, MAX_USAGE_LABEL_BYTES).into(),
        );
    }
    members.insert("model".into(), cut(model, MAX_USAGE_LABEL_BYTES).into());
    let usage = usage.accounted();
    let input = usage.input_tokens.min(MAX_TOKEN_COUNT);
    let output = usage.output_tokens.min(MAX_TOKEN_COUNT - input);
    members.insert("inputTokens".into(), input.into());
    members.insert("outputTokens".into(), output.into());
    members.insert("totalTokens".into(), (input + output).into());
    if let Some(reasoning) = usage.reasoning_tokens {
        members.insert("reasoningTokens".into(), reasoning.min(output).into());
    }
    let cached = usage.cached_input_tokens.map(|cached| cached.min(input));
    if let Some(cached) = cached {
        members.insert("cachedInputTokens".into(), cached.into());
    }
    if let Some(written) = usage.cache_write_input_tokens {
        let room = input - cached.unwrap_or(0);
        members.insert("cacheWriteInputTokens".into(), written.min(room).into());
    }
}

/// `text` cut to at most `max` bytes, on a character boundary.
fn cut(text: &str, max: usize) -> String {
    let mut end = text.len().min(max);
    while !text.is_char_boundary(end) {
        end -= 1;
    }
    text[..end].to_owned()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_report_is_the_contracts_members_and_only_what_is_known() {
        let call = UsageEvent::new(
            "c7",
            "glm-5.3",
            Usage::new(41_250, 812)
                .with_reasoning_tokens(300)
                .with_cached_input_tokens(38_000),
        )
        .under("tool:call_2")
        .with_provider("openai")
        .with_context_window(131_072);
        assert_eq!(
            report(&call),
            json!({"call": "c7", "stepId": "tool:call_2", "provider": "openai", "model": "glm-5.3",
                   "inputTokens": 41250, "outputTokens": 812, "totalTokens": 42062,
                   "reasoningTokens": 300, "cachedInputTokens": 38000, "contextWindow": 131072})
        );
        // The agent's own call, a provider that says nothing, no window: none of those members.
        assert_eq!(
            report(&UsageEvent::new("c1", "m", Usage::default())),
            json!({"call": "c1", "model": "m", "inputTokens": 0, "outputTokens": 0, "totalTokens": 0})
        );
    }

    #[test]
    fn the_report_rides_on_a_working_status_with_no_message() {
        let event = call_report("t1", "ctx", &UsageEvent::new("c1", "m", Usage::new(1, 2)));
        let TaskEvent::Status(update) = event else {
            panic!("a status update");
        };
        assert_eq!(update.status.state, TaskState::Working);
        assert!(update.status.message.is_none());
        assert_eq!(
            update.metadata.unwrap()[USAGE_EXTENSION]["totalTokens"],
            json!(3)
        );
    }

    #[test]
    fn counts_saturate_and_stay_consistent_and_labels_are_cut() {
        let call = UsageEvent::new(
            "c",
            "m",
            Usage::new(u64::MAX, u64::MAX)
                .with_reasoning_tokens(u64::MAX)
                .with_cached_input_tokens(u64::MAX)
                .with_cache_write_input_tokens(u64::MAX),
        )
        .with_context_window(u64::MAX);
        let r = report(&call);
        let n = |k: &str| r[k].as_u64().unwrap();
        assert_eq!(n("inputTokens"), MAX_TOKEN_COUNT);
        assert_eq!(n("totalTokens"), n("inputTokens") + n("outputTokens"));
        assert!(n("totalTokens") <= MAX_TOKEN_COUNT);
        assert!(n("reasoningTokens") <= n("outputTokens"));
        assert!(n("cachedInputTokens") + n("cacheWriteInputTokens") <= n("inputTokens"));
        assert_eq!(n("contextWindow"), MAX_TOKEN_COUNT);

        // A part reported beside a smaller total is added in.
        let beside = report(&UsageEvent::new(
            "c",
            "m",
            Usage::new(10, 5).with_reasoning_tokens(300),
        ));
        assert_eq!(beside["outputTokens"], json!(305));
        assert_eq!(beside["totalTokens"], json!(315));

        let mut members = Map::new();
        token_usage(
            &mut members,
            Some(&"é".repeat(100)),
            &"x".repeat(300),
            Usage::default(),
        );
        assert_eq!(members["provider"].as_str().unwrap().len(), 128);
        assert_eq!(members["model"].as_str().unwrap().len(), 128);
    }
}
