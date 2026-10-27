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
//! A task that ended (`completed`, `failed`, `canceled`, `rejected`) or waits (`input-required`,
//! `auth-required`) carries its totals in the **task's** `metadata`, for every reader (a poll,
//! `ListTasks`, a stream's snapshot), from the `usage_totals` its agent keeps in its state:
//!
//! ```json
//! {"https://agents.vymalo.com/a2a/extensions/usage/v1": {"totals": [
//!   {"provider": "openai", "model": "glm-5.3", "inputTokens": 512000, "outputTokens": 9100,
//!    "totalTokens": 521100, "reasoningTokens": 2400, "cachedInputTokens": 470000}]}}
//! ```
//!
//! The same object carries a `sandbox` member beside `totals` when the run's tools reported the time
//! their commands ran in a sandbox (`state.sandbox_usage`, [`SandboxUsage`]; ADR 0043), or only the
//! environment they ran in when that is not measured:
//!
//! ```json
//! {"https://agents.vymalo.com/a2a/extensions/usage/v1": {"totals": [],
//!   "sandbox": {"environment": "kubernetes", "usage": [
//!     {"cpuMillis": 250, "memoryBytes": 536870912, "commands": 12, "milliseconds": 81234}]}}}
//! ```
//!
//! `{"environment": "local"}` and `{"environment": "devcontainer"}` have no `usage`: *not measured*,
//! which is not zero. `"usage": []` is a pod that was asked for and ran no command: a measured zero.
//! `totals` is always there when the object is, `[]` for a run that made no model call.
//!
//! The counts are AG-UI 1.0's `TokenUsage` (parts never additions), each at most 2^53 - 1, and the
//! labels at most 128 bytes (the contract is the orchestration layer's, `docs/api/usage-v1.md` of
//! `vymalo/another-agentic-system`).

use std::collections::HashMap;

use a2a::{TaskState, TaskStatus, TaskStatusUpdateEvent};
use adam_a2a::{TaskEvent, USAGE_EXTENSION};
use adam_runtime::{
    MAX_SANDBOX_ENTRIES, MAX_USAGE_CALL_BYTES, MAX_USAGE_LABEL_BYTES, MAX_USAGE_TOTALS, RunView,
    SandboxUsage, Usage, UsageEvent, UsageTotals,
};
use serde_json::{Map, Value};

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
/// [`UsageTotals`]), at most 32 entries, and for the sandbox usage it keeps (`state.sandbox_usage`),
/// as `sandbox`; `None` when the state keeps neither (an agent that counts nothing, a task that made
/// no model call and asked for no environment). `totals` is written whenever the object is.
pub(crate) fn totals_metadata(view: &RunView) -> Option<HashMap<String, Value>> {
    let totals: UsageTotals = view
        .state
        .get("usage_totals")
        .and_then(|totals| serde_json::from_value(totals.clone()).ok())
        .unwrap_or_default();
    let sandbox = view
        .state
        .get("sandbox_usage")
        .and_then(|usage| serde_json::from_value::<SandboxUsage>(usage.clone()).ok())
        .and_then(|usage| sandbox_report(&usage));
    if totals.is_empty() && sandbox.is_none() {
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
    let mut members = Map::new();
    members.insert("totals".into(), entries.into());
    if let Some(sandbox) = sandbox {
        members.insert("sandbox".into(), sandbox);
    }
    Some(HashMap::from([(
        USAGE_EXTENSION.to_owned(),
        Value::Object(members),
    )]))
}

/// The `sandbox` member: the environment the run's commands ran in and, when that environment
/// measures them, one entry per shape (at most 8) with the shape's requests (a request the template
/// does not make is left out), the commands and their milliseconds. `None` while no environment was
/// asked for.
fn sandbox_report(usage: &SandboxUsage) -> Option<Value> {
    let environment = usage.environment()?;
    let mut report = Map::new();
    report.insert("environment".into(), environment.as_str().into());
    if environment.is_measured() {
        let entries: Vec<Value> = usage
            .entries()
            .iter()
            .take(MAX_SANDBOX_ENTRIES)
            .map(|entry| {
                let mut members = Map::new();
                if let Some(cpu) = entry.cpu_millis {
                    members.insert("cpuMillis".into(), cpu.min(MAX_TOKEN_COUNT).into());
                }
                if let Some(memory) = entry.memory_bytes {
                    members.insert("memoryBytes".into(), memory.min(MAX_TOKEN_COUNT).into());
                }
                members.insert(
                    "commands".into(),
                    entry.commands.min(MAX_TOKEN_COUNT).into(),
                );
                members.insert(
                    "milliseconds".into(),
                    entry.milliseconds.min(MAX_TOKEN_COUNT).into(),
                );
                Value::Object(members)
            })
            .collect();
        report.insert("usage".into(), entries.into());
    }
    Some(Value::Object(report))
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
    use serde_json::json;

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

    fn view_with(state: Value) -> RunView {
        let now = chrono::Utc::now();
        RunView {
            id: adam_core::RunId::new(),
            agent: "a".into(),
            conversation_id: None,
            status: adam_core::RunStatus::Done,
            wake_at: None,
            waiting: false,
            claimed: false,
            output: None,
            error: None,
            attempt: 0,
            pending_inbox: 0,
            artifacts: Vec::new(),
            state,
            version: 2,
            created_at: now,
            updated_at: now,
        }
    }

    fn written(state: Value) -> Option<Value> {
        totals_metadata(&view_with(state)).map(|mut m| m.remove(USAGE_EXTENSION).unwrap())
    }

    const TOKENS: fn() -> Value = || {
        json!([{"provider": "openai", "model": "m",
                "usage": {"input_tokens": 10, "output_tokens": 1}}])
    };

    #[test]
    fn tokens_only_is_the_object_it_always_was() {
        // `json!({"totals": entries})`, which is what this wrote before the sandbox member existed.
        let object = written(json!({"usage_totals": TOKENS()})).unwrap();
        assert_eq!(
            object,
            json!({"totals": [{"provider": "openai", "model": "m", "inputTokens": 10,
                               "outputTokens": 1, "totalTokens": 11}]})
        );
        assert_eq!(
            object.as_object().unwrap().len(),
            1,
            "no other member: {object}"
        );
    }

    #[test]
    fn a_run_that_kept_neither_writes_nothing() {
        assert_eq!(written(json!({})), None);
        assert_eq!(written(json!({"usage_totals": []})), None);
        assert_eq!(
            written(json!({"usage_totals": [], "sandbox_usage": {}})),
            None
        );
    }

    #[test]
    fn sandbox_only_is_written_with_totals_empty() {
        let object = written(json!({
            "sandbox_usage": {"environment": "kubernetes", "usage": [
                {"cpu_millis": 250, "memory_bytes": 536_870_912, "commands": 12, "milliseconds": 81_234}]}
        }))
        .unwrap();
        assert_eq!(
            object,
            json!({"totals": [],
                   "sandbox": {"environment": "kubernetes", "usage": [
                       {"cpuMillis": 250, "memoryBytes": 536_870_912,
                        "commands": 12, "milliseconds": 81_234}]}})
        );
    }

    #[test]
    fn tokens_and_sandbox_are_two_members_of_one_object() {
        let object = written(json!({
            "usage_totals": TOKENS(),
            "sandbox_usage": {"environment": "kubernetes", "usage": [
                {"cpu_millis": 250, "commands": 1, "milliseconds": 5}]}
        }))
        .unwrap();
        assert_eq!(object["totals"][0]["totalTokens"], json!(11));
        assert_eq!(
            object["sandbox"]["usage"],
            json!([{"cpuMillis": 250, "commands": 1, "milliseconds": 5}]),
            "a request the template does not make is absent, not zero"
        );
    }

    #[test]
    fn an_environment_that_is_not_measured_says_where_and_no_usage() {
        for environment in ["local", "devcontainer"] {
            let object = written(json!({"sandbox_usage": {"environment": environment}})).unwrap();
            assert_eq!(
                object,
                json!({"totals": [], "sandbox": {"environment": environment}}),
                "not measured is not zero: there is no `usage`"
            );
        }
        // Whatever the state holds, an environment that is not measured has no entries.
        let object = written(json!({
            "sandbox_usage": {"environment": "local", "usage": [
                {"commands": 1, "milliseconds": 5}]}
        }))
        .unwrap();
        assert!(object["sandbox"].get("usage").is_none(), "{object}");
    }

    #[test]
    fn a_pod_asked_for_and_never_used_is_a_measured_zero() {
        let object = written(json!({"sandbox_usage": {"environment": "kubernetes"}})).unwrap();
        assert_eq!(
            object,
            json!({"totals": [], "sandbox": {"environment": "kubernetes", "usage": []}})
        );
    }

    #[test]
    fn at_most_eight_entries_and_counts_within_2_to_the_53() {
        let entries: Vec<Value> = (0..12)
            .map(|n| json!({"cpu_millis": n, "commands": 1, "milliseconds": 1}))
            .collect();
        let object = written(json!({
            "sandbox_usage": {"environment": "kubernetes", "usage": entries}
        }))
        .unwrap();
        assert_eq!(object["sandbox"]["usage"].as_array().unwrap().len(), 8);

        let object = written(json!({
            "sandbox_usage": {"environment": "kubernetes", "usage": [
                {"cpu_millis": u64::MAX, "memory_bytes": u64::MAX,
                 "commands": u64::MAX, "milliseconds": u64::MAX}]}
        }))
        .unwrap();
        for key in ["cpuMillis", "memoryBytes", "commands", "milliseconds"] {
            assert_eq!(
                object["sandbox"]["usage"][0][key].as_u64(),
                Some(MAX_TOKEN_COUNT),
                "{key}"
            );
        }
    }

    #[test]
    fn a_sandbox_member_that_cannot_be_read_does_not_spoil_the_tokens() {
        let object =
            written(json!({"usage_totals": TOKENS(), "sandbox_usage": {"environment": 7}}))
                .unwrap();
        assert_eq!(object["totals"].as_array().unwrap().len(), 1);
        assert!(object.get("sandbox").is_none());
    }
}
