//! Each assistant and tool turn an `LlmAgent` commits carries a turn signature when it has
//! `turn_keys` (ADR 0068, slice T3a-1): it verifies against the run's key set, at its position,
//! over the chain of every message before it; a person's message is chained and never signed;
//! changing an earlier message breaks every later `prv`; without keys nothing is signed; and the
//! member never reaches a model request. Nothing verifies on re-entry yet (T3a-3).
#![allow(clippy::unwrap_used, clippy::expect_used)] // integration tests assert by unwrapping

use std::sync::Arc;
use std::time::{Duration, Instant};

use adam_core::turn::{
    ChainHash, ConversationHash, DynTurnKeys, TurnKeys, TurnPosition, turn_signature, verify_turn,
};
use adam_core::{DynStore, MemoryStore, RunId, RunStatus};
use adam_llm_agent::{LlmAgent, Tool, ToolCtx, ToolError, ToolOutput, user_message};
use adam_model::{DynModel, MockModel, ToolCall, ToolSpec};
use adam_runtime::{CollectingSink, Runtime};
use async_trait::async_trait;
use serde_json::{Value, json};
use tokio::sync::oneshot;

struct Echo;

#[async_trait]
impl Tool for Echo {
    fn spec(&self) -> ToolSpec {
        ToolSpec {
            name: "echo".into(),
            description: "Say what it was given.".into(),
            parameters: json!({"type": "object"}),
        }
    }

    async fn call(&self, _ctx: &ToolCtx, _args: Value) -> Result<ToolOutput, ToolError> {
        Ok(ToolOutput::text("echoed"))
    }
}

/// Run one task to its end on `model`, and give the committed transcript's rows with the run.
async fn run_to_done(
    model: DynModel,
    keys: Option<DynTurnKeys>,
    conversation: Option<&str>,
) -> (RunId, Vec<Value>) {
    let mut builder = LlmAgent::builder("llm", model, "test-model").tool(Echo);
    if let Some(keys) = keys {
        builder = builder.turn_keys(keys);
    }
    let store: DynStore = Arc::new(MemoryStore::new());
    let rt = Runtime::builder(store)
        .agent(builder.build())
        .event_sink(CollectingSink::new())
        .worker_id("w")
        .poll_interval(Duration::from_millis(10))
        .build();
    let run = rt
        .start("llm", user_message("say it"), conversation)
        .await
        .expect("start");
    let (stop, rx) = oneshot::channel::<()>();
    let worker = {
        let rt = rt.clone();
        tokio::spawn(async move {
            rt.run_worker(async {
                let _ = rx.await;
            })
            .await
        })
    };
    let deadline = Instant::now() + Duration::from_secs(20);
    loop {
        let view = rt.view(run).await.unwrap().unwrap();
        if view.status == RunStatus::Done {
            break;
        }
        assert!(view.status != RunStatus::Failed, "failed: {:?}", view.error);
        assert!(Instant::now() < deadline, "timed out: {:?}", view.status);
        tokio::time::sleep(Duration::from_millis(5)).await;
    }
    let _ = stop.send(());
    worker.await.unwrap().unwrap();
    let state = rt.state(run).await.unwrap().expect("state");
    let rows = state["messages"].as_array().expect("rows").clone();
    (run, rows)
}

fn scripted() -> Arc<MockModel> {
    let mock = Arc::new(MockModel::new());
    mock.push_tool_calls(vec![ToolCall {
        id: "c1".into(),
        name: "echo".into(),
        arguments: json!({}),
        ..Default::default()
    }]);
    mock.push_text("all done");
    mock
}

fn position(run: RunId, conversation: Option<&str>, rows: &[Value], idx: usize) -> TurnPosition {
    let cnv = ConversationHash::of(conversation, run);
    TurnPosition {
        run,
        cnv,
        idx: u32::try_from(idx).unwrap(),
        prv: ChainHash::over(run, &cnv, &rows[..idx]),
    }
}

fn role(row: &Value) -> &str {
    row["role"].as_str().unwrap()
}

#[tokio::test]
async fn assistant_and_tool_rows_verify_and_the_user_row_is_unsigned() {
    let keys = <dyn TurnKeys>::ephemeral().unwrap();
    let set = keys.key_set().await.unwrap();
    for conversation in [None, Some("ctx-7")] {
        let mock = scripted();
        let (run, rows) = run_to_done(mock.clone(), Some(keys.clone()), conversation).await;
        let roles: Vec<_> = rows.iter().map(role).collect();
        assert_eq!(roles, ["user", "assistant", "tool", "assistant"]);
        for (idx, row) in rows.iter().enumerate() {
            let at = position(run, conversation, &rows, idx);
            if role(row) == "user" {
                assert!(
                    turn_signature(row).is_none(),
                    "a person's turn is not signed"
                );
                assert!(row.get("extra_content").is_none());
            } else {
                verify_turn(&set, &at, row).unwrap_or_else(|e| panic!("row {idx}: {e}"));
            }
        }
    }
}

#[tokio::test]
async fn changing_an_earlier_message_breaks_every_later_prv() {
    let keys = <dyn TurnKeys>::ephemeral().unwrap();
    let set = keys.key_set().await.unwrap();
    let (run, mut rows) = run_to_done(scripted(), Some(keys), None).await;
    rows[0]["content"][0]["text"] = json!("say something else");
    for idx in 1..rows.len() {
        let at = position(run, None, &rows, idx);
        assert!(verify_turn(&set, &at, &rows[idx]).is_err(), "row {idx}");
    }
}

#[tokio::test]
async fn a_row_moved_to_another_position_or_run_does_not_verify() {
    let keys = <dyn TurnKeys>::ephemeral().unwrap();
    let set = keys.key_set().await.unwrap();
    let (run, rows) = run_to_done(scripted(), Some(keys), None).await;
    // The second assistant row, claimed at the first one's position.
    assert!(verify_turn(&set, &position(run, None, &rows, 1), &rows[3]).is_err());
    // The same row in another run.
    let other = RunId(uuid::Uuid::new_v4());
    assert!(verify_turn(&set, &position(other, None, &rows, 1), &rows[1]).is_err());
}

#[tokio::test]
async fn without_keys_the_transcript_is_exactly_what_it_was() {
    let (_, rows) = run_to_done(scripted(), None, None).await;
    assert_eq!(rows.len(), 4);
    for row in &rows {
        assert!(row.get("extra_content").is_none(), "{row}");
    }
}

/// Through the real client to a mock HTTP server: the second request carries the signed
/// assistant and tool rows, and nothing of adam's member is in any request body.
#[tokio::test]
async fn the_signature_never_reaches_a_model_request() {
    use adam_model_openai::{OpenAiCompatible, OpenAiConfig};
    use wiremock::matchers::{method, path};
    use wiremock::{Mock, MockServer, ResponseTemplate};

    let server = MockServer::start().await;
    let sse = |chunks: Vec<Value>| {
        let mut body = String::new();
        for chunk in chunks {
            body.push_str(&format!("data: {chunk}\n\n"));
        }
        body.push_str("data: [DONE]\n\n");
        ResponseTemplate::new(200).set_body_raw(body, "text/event-stream")
    };
    let chunk = |delta: Value, finish: Value| {
        json!({"id": "r", "object": "chat.completion.chunk", "model": "m",
               "choices": [{"index": 0, "delta": delta, "finish_reason": finish}]})
    };
    let tool_call = sse(vec![
        chunk(
            json!({"role": "assistant", "tool_calls": [{"index": 0, "id": "c1", "type": "function",
                   "function": {"name": "echo", "arguments": "{}"}}]}),
            Value::Null,
        ),
        chunk(json!({}), json!("tool_calls")),
    ]);
    let text = sse(vec![
        chunk(
            json!({"role": "assistant", "content": "all done"}),
            Value::Null,
        ),
        chunk(json!({}), json!("stop")),
    ]);
    Mock::given(method("POST"))
        .and(path("/v1/chat/completions"))
        .respond_with(tool_call)
        .up_to_n_times(1)
        .mount(&server)
        .await;
    Mock::given(method("POST"))
        .and(path("/v1/chat/completions"))
        .respond_with(text)
        .mount(&server)
        .await;
    let client = OpenAiCompatible::new(OpenAiConfig::new(
        format!("{}/v1", server.uri()),
        secrecy::SecretString::from(""),
    ))
    .expect("client")
    .with_credentials(Arc::new(adam_model::InjectedRun::new()));
    let keys = <dyn TurnKeys>::ephemeral().unwrap();
    let (_, rows) = run_to_done(Arc::new(client), Some(keys), None).await;
    assert!(
        rows.iter().skip(1).all(|r| turn_signature(r).is_some()),
        "the committed rows are signed"
    );
    let requests = server.received_requests().await.unwrap();
    assert_eq!(requests.len(), 2);
    let second = String::from_utf8(requests[1].body.clone()).unwrap();
    assert!(
        second.contains("\"tool\""),
        "the tool turn was sent: {second}"
    );
    for request in &requests {
        let body = String::from_utf8(request.body.clone()).unwrap();
        for hidden in ["turn_signature", "extra_content", "adam-turn"] {
            assert!(!body.contains(hidden), "{hidden} reached the model: {body}");
        }
    }
}

/// Turn keys that cannot sign: `current` fails as `error` says.
struct Failing(fn() -> adam_core::turn::TurnKeyError);

#[async_trait]
impl TurnKeys for Failing {
    async fn current(&self) -> Result<adam_core::turn::CurrentKey, adam_core::turn::TurnKeyError> {
        Err((self.0)())
    }

    async fn sign(
        &self,
        _kid: &adam_core::turn::KeyId,
        _input: &[u8],
    ) -> Result<adam_core::turn::Signature, adam_core::turn::TurnKeyError> {
        Err((self.0)())
    }

    async fn key_set(&self) -> Result<Arc<adam_core::turn::KeySet>, adam_core::turn::TurnKeyError> {
        Err((self.0)())
    }
}

/// A run whose turn keys fail ends `Failed`, and nothing of what its steps produced is committed:
/// the transcript the store holds is the person's message alone, never an unsigned assistant or
/// tool row (ADR 0068, slice T3a-1 decision 4). A refusal is permanent and fails at once; an
/// unavailable KMS is retried and fails when the retries are spent.
#[tokio::test]
async fn failing_keys_fail_the_run_and_commit_no_unsigned_turn() {
    use adam_core::turn::TurnKeyError;
    use adam_runtime::RetryPolicy;
    type Make = fn() -> TurnKeyError;
    let cases: [(&str, Make); 2] = [
        ("refused", || TurnKeyError::Refused("locked".into())),
        ("unavailable", || {
            TurnKeyError::Unavailable("kms down".into())
        }),
    ];
    for (name, error) in cases {
        let keys: DynTurnKeys = Arc::new(Failing(error));
        let store: DynStore = Arc::new(MemoryStore::new());
        let agent = LlmAgent::builder("llm", scripted(), "test-model")
            .tool(Echo)
            .turn_keys(keys)
            .build();
        let rt = Runtime::builder(store)
            .agent(agent)
            .event_sink(CollectingSink::new())
            .worker_id("w")
            .poll_interval(Duration::from_millis(10))
            .retry(
                RetryPolicy::new(2)
                    .with_backoff(Duration::from_millis(5), Duration::from_millis(5))
                    .with_multiplier(1.0),
            )
            .build();
        let run = rt
            .start("llm", user_message("say it"), None)
            .await
            .expect("start");
        let (stop, rx) = oneshot::channel::<()>();
        let worker = {
            let rt = rt.clone();
            tokio::spawn(async move {
                rt.run_worker(async {
                    let _ = rx.await;
                })
                .await
            })
        };
        let deadline = Instant::now() + Duration::from_secs(20);
        let view = loop {
            let view = rt.view(run).await.unwrap().unwrap();
            if matches!(view.status, RunStatus::Done | RunStatus::Failed) {
                break view;
            }
            assert!(
                Instant::now() < deadline,
                "{name}: timed out {:?}",
                view.status
            );
            tokio::time::sleep(Duration::from_millis(5)).await;
        };
        let _ = stop.send(());
        worker.await.unwrap().unwrap();
        assert_eq!(view.status, RunStatus::Failed, "{name}: {view:?}");
        let state = rt.state(run).await.unwrap().expect("state");
        let rows = state["messages"].as_array().expect("rows");
        let roles: Vec<_> = rows.iter().map(role).collect();
        assert_eq!(roles, ["user"], "{name}: no unsigned turn was committed");
    }
}

/// A NUL in an assistant's text and in a tool call's arguments (a string and a key) is replaced by
/// the store (ADR 0051). The signature and the chain cover the row **as stored**, so every row
/// still verifies when it is read back.
#[tokio::test]
async fn rows_with_a_nul_verify_as_the_store_keeps_them() {
    let keys = <dyn TurnKeys>::ephemeral().unwrap();
    let set = keys.key_set().await.unwrap();
    let mock = Arc::new(MockModel::new());
    mock.push_tool_calls(vec![ToolCall {
        id: "c1".into(),
        name: "echo".into(),
        arguments: json!({"cmd": "a\u{0}b", "k\u{0}ey": ["x\u{0}"]}),
        ..Default::default()
    }]);
    mock.push_text("nul\u{0}in the answer");
    let (run, rows) = run_to_done(mock, Some(keys), None).await;
    let text = serde_json::to_string(&rows).unwrap();
    assert!(
        !text.contains("\\u0000"),
        "the store replaced every NUL: {text}"
    );
    assert!(
        text.contains("a\u{FFFD}b") && text.contains("nul\u{FFFD}in"),
        "{text}"
    );
    for (idx, row) in rows.iter().enumerate().skip(1) {
        verify_turn(&set, &position(run, None, &rows, idx), row)
            .unwrap_or_else(|e| panic!("row {idx}: {e}"));
    }
}
