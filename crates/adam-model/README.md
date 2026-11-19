# adam-model

The model-client seam: the `ModelClient` trait, its request/response types
and a scripted test double.

## Where it sits

The **port** for language models. Agents talk to models only through
`ModelClient`; implementations are separate crates, currently
[`adam-model-openai`](../adam-model-openai/README.md) (any OpenAI-compatible
chat-completions endpoint). [`adam-llm-agent`](../adam-llm-agent/README.md)
consumes the trait. Implementations never retry: `Classify::is_retryable` and
`Classify::retry_after` (both from [`adam-error`](../adam-error/README.md),
re-exported here with `ErrorClass`) tell
[`adam-runtime`](../adam-runtime/README.md) what is worth retrying and how long
the provider asked to wait, and the runtime owns backoff.

## API at a glance

| Item | What |
|---|---|
| `ModelClient` (trait) | `complete(ModelRequest) -> ModelResponse` and `stream(ModelRequest) -> BoxStream<ModelDelta>`; two default methods, `provider() -> Option<&str>` (a label for reports, `openai` for the OpenAI-compatible client) and `context_window(alias) -> Option<u64>` (the window the deployment configured for that alias), both `None` unless the client says |
| `DynModel` | `Arc<dyn ModelClient>` |
| `ModelRequest`, `ModelResponse`, `ModelDelta` | request (`model`, `system`, `messages`, `tools`, `ToolChoice`, `max_output_tokens`, `temperature`, `metadata`, `prompt_cache_key`: a stable key per conversation, which the agent loop sets and a client may send; **new public field, breaks `ModelRequest` struct literals**: use `ModelRequest::new`), response and streamed chunks. `ModelDelta` is `#[non_exhaustive]` since `ToolCallComplete` (below): a `match` over it needs one more arm |
| `Message`, `ContentPart`, `ToolCall`, `ToolSpec`, `Usage`, `FinishReason` | conversation and tool-calling types; all derive `Serialize`/`Deserialize` because a run persists its history. `ToolCall` has `new(id, name, arguments)`, `with_invalid_arguments(id, name, raw)` and `Default`; its `invalid_arguments: Option<String>` holds the raw argument text when it was not JSON (`arguments` is then `{}`). **Breaking:** a `ToolCall { id, name, arguments }` literal no longer compiles (use `ToolCall::new` or `..Default::default()`); the serialized form and the `ModelClient` trait are unchanged, and journals written before the field decode with `None` |
| `ModelError` | `RateLimited { retry_after }`, `Transient`, `ContextLength`, `InvalidRequest`, `Auth`, `Protocol`; `#[non_exhaustive]`, see *Errors* |
| `Classify`, `ErrorClass` | re-exported from `adam-error`: `class()`, `is_retryable()`, `retry_after()` |
| `MockModel`, `RecordedCall` | scripted `ModelClient` that records requests; always compiled, no feature flag; `.with_provider(label)` and `.with_context_window(alias, n)` say what a real client would |

```rust
use adam_model::{Message, MockModel, ModelClient, ModelRequest};

let model = MockModel::new();
model.push_text("hello");

let mut req = ModelRequest::new("any-alias");
req.messages.push(Message::user_text("hi"));
let resp = model.complete(req).await.unwrap();
assert_eq!(resp.message.text(), "hello");
```

### A tool call whose arguments are complete

`ModelDelta::ToolCallComplete(ToolCall)` says that the arguments of a call are whole: the text of them parses as a JSON
**object**, and the call holds what `Finished` will hold for it (same id, same name, same arguments), so a consumer may
start it before the turn ends ([ADR 0049](../../docs/decisions/0049-the-tool-calls-of-a-turn-run-at-the-same-time.md),
amended). A client says it **once per call**: the first time the accumulated text parses as an object, and for every call
it did not say it for, immediately before `Finished`. Empty text is not complete on its own (it parses as `{}`), so a
provider that sends the id and the name first and the arguments after is not announced as `{}`. A call whose text never
parses as JSON is announced with `ToolCall::invalid_arguments` set (and does not run). A made-up id is fixed at the
announcement, and the response keeps it. A model that does not stream says none of this.
**Breaking:** `ModelDelta` is now `#[non_exhaustive]`, so a `match` outside this crate needs a wildcard arm, and every
consumer of the stream may see the new variant.

### Reasoning

A model in thinking mode writes its reasoning before the answer: it arrives as `ModelDelta::Reasoning`
pieces and, whole, as `ModelResponse.reasoning` (`Option<String>`). It is for people to read and **not part of
the answer**: `Message::text()` never holds it and `Message` does not carry it, with one exception chosen by the
client: `Message::Assistant.reasoning` is `Some` only for a client set to **echo** reasoning (a provider that
requires it back, DeepSeek's thinking mode with tools; [`adam-model-openai`](../adam-model-openai/README.md#reasoning)).
It is written only when present, so a history stored before it existed and one of a client that does not echo are
the same bytes. **Breaking for code that builds these types with struct literals:** `ModelResponse` and
`Message::Assistant` have a new field, and `ModelDelta` a new variant (`match`es over it need an arm).
`MockModel::stream` replays `ModelResponse.reasoning` as a `Reasoning` delta before the text.

### Usage

`Usage` follows AG-UI 1.0's `TokenUsage` accounting: `input_tokens` and `output_tokens` are totals,
and `reasoning_tokens` (part of the output), `cached_input_tokens` and `cache_write_input_tokens`
(disjoint parts of the input) are optional parts, never additions; a part is `None` when the provider
did not say. `Usage::accounted()` adds a part that a provider reported beside a smaller total into that
total (AG-UI's rule for a producer); applying it twice changes nothing. `total_tokens()` and
`saturating_add(other)` sum without overflowing. A part is written to JSON only when it is `Some`, so a
usage recorded before the parts existed reads the same. **Breaking:** `Usage` is `#[non_exhaustive]`
with three new fields, so code outside this crate builds it with `Usage::new(input, output)` and the
`with_*` methods instead of a struct literal. `ModelClient`'s two new methods have defaults: no
implementation has to change.

The persisted JSON shapes of `Message` are shown in the crate docs
(`src/lib.rs`) and are meant to stay stable.

## Images

A message names an image by reference and never holds its bytes (ADR 0044).

| Item | What |
|---|---|
| `ContentPart::Image(ImageRef)` | an image in a user or assistant message. `ImageRef { sha256, media_type, bytes, width, height, name?, origin }`; `ContentPart::as_text` of one is `""` |
| `ImageOrigin` | `Person`, `Workspace`, `Mcp`: **closed** on purpose, so a new origin fails to compile wherever a deployment's allowed sources are checked |
| `Message::Tool { images }` | the images a tool returned (default empty, not serialized while empty: a history with no image is the same bytes as before, and an older journal decodes). `Message::images()`, `tool_result_with_images`, `user_with_images` |
| `ModelRequest::image_data` (`ImageData`) | the bytes of the images `messages` name, by SHA-256, **never serialized**: the agent loop fills it inside the step that calls the model. `Debug` says how many, never the bytes. A reference with no entry is sent as `ImageRef::missing_line()` |
| `ModelClient::input_modalities(model) -> InputModalities` | what the deployment declared the model of that alias can read: text only by default, never guessed from a name (`InputModalities::images: Option<ImageLimits { max_bytes, max_count, max_side, sources }>`, `#[non_exhaustive]`). `Arc<T>` forwards it. `MockModel::seeing_images(limits)` |

**Breaking** for code that builds or matches these types: `ContentPart` gains a variant, `Message::Tool` a field and `ModelRequest` a field (`..Default::default()` or `..` fixes a literal or a pattern); `ModelClient` gains a provided method, so no implementer breaks.

## Errors
## Errors

`ModelError` implements `Classify`; the runtime decides from the class (see
[`adam-error`](../adam-error/README.md)).

| Variant | Class | Retry |
|---|---|---|
| `RateLimited { retry_after }` | `RateLimited` | yes, after `max(backoff, retry_after)` |
| `Transient { message, source }` | `Transient` | yes, with backoff |
| `ContextLength`, `InvalidRequest { message, source }` | `Invalid` | no |
| `Auth` | `Unauthenticated` | no |
| `Protocol { message, source }` | `Corrupt` | no |

`Transient`, `InvalidRequest` and `Protocol` keep the transport's or decoder's
error as their `source`, and their message does not repeat it. Build them with
`ModelError::transient(msg)`, `invalid_request(msg)` and `protocol(msg)`, then
`.with_source(err)`; a variant that has no source ignores `with_source`.
`retry_after()` is set only by `RateLimited`. `is_context_overflow()` is true for
`ContextLength` alone: its class stays `Invalid` (the same request never succeeds), and the
agent loop decides from the predicate that a shorter history is worth one more try. `ModelError` is no longer `Clone`
or `PartialEq`: match on the variant or the class.

## Features and environment

None.

## Tests

Doctests (the crate docs run as tests in CI's `cargo test --doc`) and unit
tests in `src/error.rs` (`class_table`,
`retry_after_comes_from_the_rate_limit_only`,
`a_source_is_kept_and_printed_once`), `src/mock.rs` and `src/types.rs` (the usage accounting:
`a_part_above_its_total_is_added_in_and_one_within_it_is_left`, `usages_add_part_by_part_and_saturate`). No external
service, no environment variables. [`adam-model-openai`](../adam-model-openai/README.md)
tests a real implementation against a mock HTTP server.

## See also

[`adam-model-openai`](../adam-model-openai/README.md),
[`adam-llm-agent`](../adam-llm-agent/README.md),
[`adam-error`](../adam-error/README.md),
[root README](../../README.md).
