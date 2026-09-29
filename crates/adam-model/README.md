# adam-model

The model-client seam: the `ModelClient` trait, its request/response types
and a scripted test double.

## Where it sits

The **port** for language models. Agents talk to models only through
`ModelClient`; implementations are separate crates, currently
[`adam-model-openai`](../adam-model-openai/README.md) (any OpenAI-compatible
chat-completions endpoint). [`adam-llm-agent`](../adam-llm-agent/README.md)
consumes the trait. Implementations never retry: `ModelError::is_retryable`
tells [`adam-runtime`](../adam-runtime/README.md) what is worth retrying, and
the runtime owns backoff.

## API at a glance

| Item | What |
|---|---|
| `ModelClient` (trait) | `complete(ModelRequest) -> ModelResponse` and `stream(ModelRequest) -> BoxStream<ModelDelta>` |
| `DynModel` | `Arc<dyn ModelClient>` |
| `ModelRequest`, `ModelResponse`, `ModelDelta` | request (`model`, `system`, `messages`, `tools`, `ToolChoice`, `max_output_tokens`, `temperature`, `metadata`), response and streamed chunks |
| `Message`, `ContentPart`, `ToolCall`, `ToolSpec`, `Usage`, `FinishReason` | conversation and tool-calling types; all derive `Serialize`/`Deserialize` because a run persists its history |
| `ModelError` | `RateLimited { retry_after }`, `Transient`, `ContextLength`, `InvalidRequest`, `Auth`, `Protocol`; `is_retryable()` |
| `MockModel`, `RecordedCall` | scripted `ModelClient` that records requests; always compiled, no feature flag |

```rust
use adam_model::{Message, MockModel, ModelClient, ModelRequest};

let model = MockModel::new();
model.push_text("hello");

let mut req = ModelRequest::new("any-alias");
req.messages.push(Message::user_text("hi"));
let resp = model.complete(req).await.unwrap();
assert_eq!(resp.message.text(), "hello");
```

The persisted JSON shapes of `Message` are shown in the crate docs
(`src/lib.rs`) and are meant to stay stable.

## Features and environment

None.

## Tests

Doctests (the crate docs run as tests in CI's `cargo test --doc`) and unit
tests in `src/error.rs`, `src/mock.rs` and `src/types.rs`. No external
service, no environment variables. [`adam-model-openai`](../adam-model-openai/README.md)
tests a real implementation against a mock HTTP server.

## See also

[`adam-model-openai`](../adam-model-openai/README.md),
[`adam-llm-agent`](../adam-llm-agent/README.md),
[root README](../../README.md).
