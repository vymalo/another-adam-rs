# adam-llm-agent

`LlmAgent`: a reusable, durable model/tool-calling loop on top of
`adam-runtime`.

## Where it sits

An `adam_runtime::Agent` implementation, written against the two ports it
needs: [`adam-model`](../adam-model/README.md) (`DynModel`) and
[`adam-runtime`](../adam-runtime/README.md). Every model call and every tool
call is a journaled `Ctx::step`, so a restarted worker replays what already
happened instead of repeating a side effect. An adam-rs agent is then
instructions + a model + a toolset. It is served over A2A by
[`adam-a2a-runtime`](../adam-a2a-runtime/README.md); the coder agent
([`adam-coder`](../adam-coder/README.md)) is built on it.

## API at a glance

| Item | What |
|---|---|
| `LlmAgent`, `LlmAgentBuilder` | `LlmAgent::builder(name, model, model_alias)` then `.instructions(..)`, `.tool(..)`, `.dyn_tool(..)`, `.limits(..)`, `.build()` |
| `Limits` | `max_turns`, `max_tool_calls`, `max_output_tokens`, `max_history_tokens`; a tripped limit fails the run with a message naming it (except history, which shortens old tool output) |
| `Tool` (trait), `DynTool` | `spec() -> ToolSpec` and `async call(&ToolCtx, Value) -> Result<ToolOutput, ToolError>` |
| `ToolOutput` | `text`, `error`, `with_artifact` |
| `ToolCtx` | run id, conversation id, attempt, call id, `emit_progress`, `cancelled` / `cancel_token` |
| `ToolError` | `Transient`, `Permanent`, `NeedsInput { question }` (parks the run; A2A reports `input-required`) |
| `Conversation`, `PendingQuestion`, `ArtifactRef` | what `Runtime::view(run).state` deserializes into |
| `user_message(text)`, `MESSAGE_KIND` | build the `Inbound` that starts or continues a run |
| `TRUNCATION_MARKER_PREFIX` | prefix of the marker left where history truncation shortened a tool output |

```rust
use std::sync::Arc;
use adam_llm_agent::{Limits, LlmAgent};
use adam_model::MockModel;

let agent = LlmAgent::builder("assistant", Arc::new(MockModel::new()), "my-model")
    .instructions("Be brief.")
    .limits(Limits { max_turns: 10, ..Limits::default() })
    .build();
// register it: Runtime::builder(store).agent(agent).build()
// start a run: runtime.start("assistant", user_message("hello"), None)
```

A complete `Tool` implementation is in the crate docs (`src/lib.rs`).

## Features and environment

None.

## Tests

`tests/llm_agent.rs` is a behavioural suite over a scripted `MockModel` and
`MemoryStore` (tool loop, retries and rate limits, limits, replay after a
crash, `NeedsInput` parking, cancellation, history truncation). Property tests
of the truncation are in `src/history.rs`. Offline, no environment variables.

## See also

[`adam-runtime`](../adam-runtime/README.md),
[`adam-model`](../adam-model/README.md),
[`adam-coder`](../adam-coder/README.md).
