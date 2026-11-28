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
| `ModelClient` (trait) | `complete(ModelRequest) -> ModelResponse` and `stream(ModelRequest) -> BoxStream<ModelDelta>`; four default methods, `provider() -> Option<&str>` (a label for reports, `openai` for the OpenAI-compatible client), `context_window(alias) -> Option<u64>` (the window the deployment configured for that alias; by default the `contextWindow` of the alias's `profile`, ADR 0086), `input_modalities(alias) -> InputModalities` (text only unless the deployment declared images) and `profile(alias) -> Option<&ModelProfile>` (the deployment's profile of that model, `None` when it declared none); each `None`/text-only unless the client says |
| `ModelClient::profile(model) -> Option<&ModelProfile>` | the deployment's profile of the model that alias names (`MODEL_ALIASES_FILE`), `None` by default: nothing of a profile is applied. `Arc<T>` forwards it. Hosts will read it for the cap resolved per call once the model is chosen at run time (ADR 0064 slice 5, not built; the cap itself is built per agent, slice 2); the OpenAI-compatible client sends a request's level through its wire |
| `DynModel` | `Arc<dyn ModelClient>` |
| `ModelRequest`, `ModelResponse`, `ModelDelta` | request (`model`, `system`, `messages`, `tools`, `ToolChoice`, `max_output_tokens`, `temperature`, `top_p`, `headers` (a credential-like name is refused: `is_credential_header`), `extra_body` (merged over the body; `RESERVED_BODY_KEYS` refused), `reasoning_effort`: the level this call asks for, absent sends nothing (ADR 0063), `metadata`, `prompt_cache_key`: a stable key per conversation, which the agent loop sets and a client may send; `run`: the root run the call belongs to, which the agent loop sets from in-process state and an injector reads ([ADR 0067](../../docs/decisions/0067-a-model-call-through-an-injector-names-its-run-and-a-refusal-parks-the-run.md)), skipped in JSON when `None`; **new public fields, break `ModelRequest` struct literals**: use `ModelRequest::new`). `ModelRequest`'s `PartialEq` and `Serialize` include `run`: two requests of different runs are not equal, and a test that compares requests of two runs clears it first. Response and streamed chunks. `ModelDelta` is `#[non_exhaustive]` since `ToolCallComplete` (below): a `match` over it needs one more arm |
| `Message`, `ContentPart`, `ToolCall`, `ToolSpec`, `Usage`, `FinishReason` | conversation and tool-calling types; all derive `Serialize`/`Deserialize` because a run persists its history. `ToolCall` has `new(id, name, arguments)`, `with_invalid_arguments(id, name, raw)` and `Default`; its `invalid_arguments: Option<String>` holds the raw argument text when it was not JSON (`arguments` is then `{}`). `Message::Assistant.provider` and `ToolCall.opaque` carry a provider's opaque members, bound to the model alias and endpoint that made them (*A provider's opaque members*, below); both are `None` when absent and are not serialized then, so an older journal decodes and a message without them is the same bytes. **Breaking:** a `ToolCall { id, name, arguments }` literal and a `Message::Assistant { content, tool_calls, reasoning }` literal no longer compile (use `ToolCall::new` or `..Default::default()` for a call; add `opaque: None` and `provider: None`, or `..` in a pattern); the serialized form and the `ModelClient` trait are unchanged, and journals written before the fields decode with `None` |
| `ModelError` | `RateLimited { retry_after }`, `Transient`, `ContextLength`, `InvalidRequest`, `Auth`, `Protocol`, `Refused { status, slug, retry_after, park }`, `ModelLocked { model }`, `Unreachable`; `#[non_exhaustive]`, see *Errors* |
| `ReasoningEffort`, `ReasoningWire`, `ModelProfile`, `Preset`, `ModelProfiles` | the per-model profiles of `MODEL_ALIASES_FILE` (ADR 0063, ADR 0064), parsed and validated by `ModelProfiles::parse_json`; pure data, no file I/O (see [Per-model profiles](#per-model-profiles)) |
| `Classify`, `ErrorClass` | re-exported from `adam-error`: `class()`, `is_retryable()`, `retry_after()` |
| `ModelCredentials` (trait), `DynCredentials` | where the credentials of a model call come from, **asked for every call** ([ADR 0058](../../docs/decisions/0058-the-models-credentials-come-from-a-provider-asked-for-each-call.md)): `headers(CredentialRequest { method, url, attempt, run, prompt_statement, body }) -> CredentialHeaders` (`run` is the request's `ModelRequest::run`, `body` the exact bytes sent; **new fields: a `CredentialRequest` literal outside the workspace breaks**; its `Debug` shows the body's length, never the body) and `rejected(call, &Rejection { status, www_authenticate, dpop_nonce }) -> AfterRejection` (`Retry` or `GiveUp`, the default), and the provided `parks(&Refusal) -> Park` (`Park::No` by default, so no implementer breaks: see *Refusals*). A host mints, renews or proves a credential at the moment of use (a token renewed from the person's session, a DPoP proof over the method and URL), or gives a header with no key at all (an injector's run tag). Its errors are its choice of class: `Auth` when the person must act, `Transient` when the issuer did not answer. `Arc<T>` forwards it, `parks` included: **a decorator of a provider must forward `parks` too**, or it answers the default `Park::No` and its provider's refusals stop parking |
| `CredentialHeaders` | the headers of one call: `none()`, `bearer(&token)`, `.with(name, value)` (a later one of the same name replaces it, case-insensitive); values are `SecretString`, `Debug` shows the names only |
| `StaticBearer` | a fixed bearer, the same for every call (an empty one sends no header): what `MODEL_API_KEY` gives. It gives up on a rejection |
| `InjectedRun`, `RUN_HEADER` | `MODEL_AUTH=injector` ([ADR 0067](../../docs/decisions/0067-a-model-call-through-an-injector-names-its-run-and-a-refusal-parks-the-run.md)): no key, exactly one header, `X-Run-Id: <run>`, and no `Authorization`, for an injector in front of the model that adds the run's credential. A call with no run (or an empty one) is refused with `InvalidRequest` ("a model call through the injector names no run"), so nothing is sent. It gives up on a rejection |
| `PromptStatement`, `NotAPromptStatement`, `NoPromptId`, `PROMPT_STATEMENT_HEADER`, `PROMPT_USER_SIGNATURE_HEADER`, `MAX_PROMPT_JWS_BYTES` | `prompt-statement/v1` ([ADR 0077](../../docs/decisions/0077-a-model-call-carries-the-prompt-statement-and-the-persons-envelope.md)): the prompt statement and the person's envelope of a run, each a compact JWS of at most 8 KiB (checked, never verified), kept as they came; `new(statement, user)`, `envelope(user)` for a device's envelope alone, `from_parts`; **`statement()` is an `Option` since the device path (ADR 0077 amended 2026-11-25)**; `prompt_jti()` reads the statement's `jti`, or the envelope's `nonce` without a statement (the one thing read, for the request signature). `Debug` holds no value and an error names the member, never its content. `ModelRequest::prompt_statement` (not serialized, never journaled) and `CredentialRequest::prompt_statement` carry it to the provider; `InjectedRun` and `SignedRequests` send `Prompt-Statement` and `Prompt-User-Signature` (each when its value is there), byte for byte, beside `X-Run-Id`; no other provider sends them |
| `SignedRequests`, `TURN_SIGNATURE_HEADER`, `request_target` | every model request signed ([ADR 0068](../../docs/decisions/0068-adam-signs-its-own-history.md), *Amended 2026-11-25 (the request signature)*): `SignedRequests::new(inner, turn_keys)` wraps any provider and adds to every call `X-Run-Id`, the prompt headers and `Turn-Signature`, adam's attached JWS (`adam-request/v1`) over the run, the method, the request target, the SHA-256 of the exact body, the prompt's id (`null` without one), `iat` and a nonce, signed anew for each attempt (`adam_core::turn::sign_request`). A call it cannot sign is not sent: `InvalidRequest` (no run, no target, an unreadable prompt id), `Transient` (keys unreachable), `Auth` (keys refuse). It forwards `rejected` and `parks`. `request_target(url)` is the path and query an HTTP client sends. Depends on `adam-core` for the keys |
| `Refusal`, `ProblemType`, `Park`, `FIRST_PARK`, `MAX_PARK`, `MODEL_LOCKED_SLUG` | what a client asks `parks` about: `Refusal { status, problem, retry_after }` (no body, no header; `#[non_exhaustive]`, built with `Refusal::new(status)`, `.with_slug(..)`, `.with_problem(..)`, `.with_retry_after(..)`, read with `.slug()`), whose `problem` is `ProblemType::Absent` (no problem document, no `type`, `about:blank`), `Slug(word)` or `Unreadable` (a `type` that is there and is no word); the answer, `Park::No`, `OnTimer(Duration)` or `UntilWoken` (closed on purpose); `FIRST_PARK` is 30 s, the first step of the agent's backoff, and `MAX_PARK` 24 h, the longest timer whatever `Retry-After` says; `MODEL_LOCKED_SLUG` is `model_locked` and `OVER_LIMIT_SLUG` is `over_limit`, the slug a `402` reads as (see *Refusals*) |
| `RenewingCredentials` | a test double: `token-1`, `token-2`, ... moved on by each rejection, which it asks to retry; `issued()`, `rejections()` |
| `MockModel`, `RecordedCall` | scripted `ModelClient` that records requests; always compiled, no feature flag; `.with_provider(label)`, `.with_context_window(alias, n)` and `.with_profiles(ModelProfiles)` say what a real client would; its window is resolved as the real client's, the profile's first |

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
provider that sends the id and the name first and the arguments after is not announced as `{}`; a call with no name yet
waits for its name (it is not a call until it has one). A call whose text never
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

### A provider's opaque members

A provider may attach members this client does not model to an assistant message or to one of its
tool calls — Gemini 3's `extra_content.google.thought_signature` is the one that forced this
(adam-rs#322: without it, every second call is a 400). They are part of the next request's bytes,
so they are kept **value for value** in the conversation and sent back unchanged:

| Item | What |
|---|---|
| `ProviderTurn` | `model` (the request's model alias), `endpoint` (the client's stable id of the endpoint: a fingerprint of the base URL, not a secret), `members: BTreeMap<String, Value>` (the message's unmodelled members), `reasoning: Option<Value>` (a reasoning block the provider sent as an object or array instead of text). `ProviderTurn::new(model, endpoint)` and `is_bound_to(model, endpoint)` |
| `Message::Assistant.provider` | `Option<ProviderTurn>`; `None` when the provider attached nothing, and then not serialized |
| `ToolCall.opaque` | `Option<BTreeMap<String, Value>>`, the call object's unmodelled members. Its provenance is the enclosing message's `provider`: a call never duplicates the alias or the endpoint |
| `Message::Assistant.extra_content`, `Message::Tool.extra_content` | `Option<BTreeMap<String, Value>>`: members of the message object itself, one entry per maker (`{"adam": {…}, "google": {…}}`). `None` when absent, and then not serialized, so an older journal decodes and an unsigned history is the same bytes. adam's own maker, `ADAM_MAKER` (`"adam"`), is for adam to read back ([ADR 0068](../../docs/decisions/0068-adam-signs-its-own-history.md)): a request never carries it and a model's reply never keeps it (`strip_adam_member` removes only the `adam` key, and the member only when that emptied it). The carrier is adam's namespace only and is never sent to a model: another maker's members belong to `ProviderTurn.members`, which is bound to its model, and a maker set in the carrier does not reach the wire. **Breaking:** a `Message::Assistant { .. }` or `Message::Tool { .. }` literal needs `extra_content: None` |

**Bound to the model that made it.** A client sends a turn's members back only when the turn's
`model` and `endpoint` both match the request's model alias and endpoint. A run that switches
model (the frontier fallback, a person's choice) drops the old model's members: they never reach
another model. A turn that is not stamped matches nothing, so a client that does not know the
endpoint sends nothing — fail closed.

The carrier rides the conversation's JSON with `serde(default, skip_serializing_if)`: a journal
written before it decodes with `None`, and a message without it is the same bytes as before. No
store schema changes.

**Limit.** `reasoning` is the raw value only: this crate has no block type, so a reasoning block
is carried as an opaque `Value`, not as a structured turn with its own members.

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
implementation has to change. The default `context_window` now answers the profile's `contextWindow`
(ADR 0086): a client that overrides `profile` and not `context_window` says its profiles' windows.

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

**Breaking** for code that builds or matches these types: `ContentPart` gains a variant, `Message::Tool` a field and `ModelRequest` fields (`..Default::default()` or `..` fixes a literal or a pattern); `ModelClient` gains a provided method, so no implementer breaks.

## Per-model profiles

`MODEL_ALIASES_FILE` (ADR 0063, ADR 0064) declares, for each model a deployment can call, whether it takes a
reasoning setting, which levels it accepts, how they are sent, its output cap, its context window
([ADR 0086](../../docs/decisions/0086-the-context-window-follows-the-model-and-the-history-budget-is-200k.md))
and the members merged into its requests only. The types live here; the file is read by the binary (`adam-service`'s `ModelConfig`), and nothing
in this crate touches the file system.

| Item | What |
|---|---|
| `ReasoningEffort` | `None`, `Minimal`, `Low`, `Medium`, `High`, `XHigh`, `Max`, spelled `none`, `minimal`, `low`, `medium`, `high`, `xhigh`, `max`. Closed on purpose, `ALL` lowest first, exhaustive `as_str`/`FromStr`, serde lower case, `Ord` in that order. `none` is a real level (some models reason unless it is sent); **absent** sends nothing |
| `ReasoningWire` | `ReasoningEffort` (`{"reasoning_effort": ...}`, the only one verified on the deployment's gateway), `ReasoningObject` (`{"reasoning": {"effort": ...}}`), `ThinkingBudget` (`{"thinking": {"type": "enabled", "budget_tokens": n}}`); serde snake case |
| `ModelProfile` | serde camelCase, `Default`, unknown members ignored: `reasoning_efforts`, `reasoning_wire`, `reasoning_budgets`, `reasoning_effort`, `max_output_tokens`, `max_output_tokens_limit`, `context_window` (`contextWindow`, the model's window in tokens, 1 to `MAX_CONTEXT_WINDOW`; profile only, no preset sets it), `extra_body`, `preset`, `fallback` (the model a run continues on, once, when this one is locked: another profile of the document), `endpoint` (the deployment's own **endpoint label** for the model: a label only, never a provider, a host, a URL or a key; what a `model-pick/v1` choice carries beside its alias, ANR-623, read by `endpoint_of`). A profile with only a cap or only an extra body is valid. **Breaking:** `ModelProfile::fallback`, `Preset::fallback`, `ModelProfile::endpoint` and `ModelProfile::context_window` are new public fields of structs that are not `#[non_exhaustive]`: a literal needs them (or `..Default::default()`) |
| `Preset`, `ModelProfiles` | a named bundle (`reasoning_effort`, `max_output_tokens`) and the document (`profiles` by model id, `presets` by name). `profile(id)`, `preset(name)`, `preset_of(id)`, `endpoint_of(id)` |
| `ModelProfiles::parse_json(text)` | parse and validate, **JSON only for now** (the workspace has no YAML parser). `Err(Vec<String>)` lists every problem, each naming the profile (or preset) and the key, but not the file: the caller adds the path. A file with any problem is refused whole |
| `MAX_CONTEXT_WINDOW`, `resolve_context_window(profile, configured)` | 2^53 − 1, the largest window (a profile's and `DEFAULT_MODEL_CONTEXT_WINDOW`'s); and **the one rule** of a model's window (ADR 0086): the profile's `contextWindow`, else the window configured for that alias outside the profiles, else `None`. `ModelClient::context_window`'s default, `MockModel` and `OpenAiCompatible` answer through it, so the history budget, the output cap and each `usage/v1` report read the same window for an alias, a subagent's included |
| `DEFAULT_MAX_OUTPUT_TOKENS` | 128,000: the cap a call asks for when its folder, its preset and its model's profile set none (ADR 0064). The assembly resolves it per model and clamps it to the model's limit and window; the loop's `Limits` default is the same value |

The rules, all of them refusals at load: a level outside the vocabulary; an unknown wire; a duplicate level (the
list is sorted lowest first); a budget for an undeclared level, a declared level with no budget, or a budget of
`0`; `none` declared with the `thinking_budget` wire, whose shape has no disabled form (a level that means "do not
reason" would send `thinking: {type: enabled, ...}`: leave `none` out or use another wire); an effort outside the
model's levels (a profile's own, or a preset's); an unknown preset; efforts without a
wire, a wire with no efforts, or budgets without `thinking_budget`; a cap of `0`, or above
`maxOutputTokensLimit` (a preset's too); a `contextWindow` of `0` or above 2^53 − 1; a `fallback` that names itself or a model with no profile; an `extra_body` member the runtime owns (`RESERVED_EXTRA_BODY_KEYS`:
`model`, `messages`, `tools`, `tool_choice`, `stream`, `reasoning_effort`, `max_tokens`,
`max_completion_tokens`). `reasoningEfforts: []` says the model takes no reasoning setting.
`endpoint` is the one member that never refuses the document: a blank, non-string or over-long label
(over `MAX_ENDPOINT_LABEL_LEN`, 128 bytes) is *omitted*, so a card lists no endpoint rather than a
guessed one (ANR-623).

`ModelRequest::reasoning_effort` carries the level a call asks for (ADR 0063); the agent loop sets it
from the run's protected context key `vymalo.reasoningEffort`, else the agent's default. The
OpenAI-compatible client sends it through the model profile's `reasoningWire`, after the extra body,
which it wins over where the two would duplicate a member (`crates/adam-model-openai/README.md`,
*Reasoning effort*); a model with no profile or no wire sends nothing. The profile's `extra_body` is
ADR 0064 slice 3.

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
| `Refused { status, slug, retry_after, park }` | by its status: `401`, `403` `Unauthenticated`; `402` `Rejected`; `429` `RateLimited`; `503` `Transient`; any other `Rejected` | `429` and `503` only (`retry_after()` is its `retry_after` for a `429`) |
| `ModelLocked { model }` | `Rejected` | no |
| `Unreachable { source }` | `Transient` | yes, with backoff |

`Transient`, `InvalidRequest` and `Protocol` keep the transport's or decoder's
error as their `source`, and their message does not repeat it. Build them with
`ModelError::transient(msg)`, `invalid_request(msg)` and `protocol(msg)`, then
`.with_source(err)`; a variant that has no source ignores `with_source`.
`retry_after()` is set only by `RateLimited` and a `Refused` of status `429`. `Refused` holds no body
and no header: its `Display` is `the model endpoint refused the call: HTTP <status>[ <slug>]`, and it has
no source. `Unreachable` (`ModelError::unreachable()`) is a connection that could not be made, never a
timeout nor a stream cut off. `is_context_overflow()` is true for
`ContextLength` alone: its class stays `Invalid` (the same request never succeeds), and the
agent loop decides from the predicate that a shorter history is worth one more try. `ModelError` is no longer `Clone`
or `PartialEq`: match on the variant or the class.

## Refusals

[ADR 0067](../../docs/decisions/0067-a-model-call-through-an-injector-names-its-run-and-a-refusal-parks-the-run.md)
decision 3. A client that gets an answer of status 400 or above builds a `Refusal` and asks its provider
`parks(&refusal)`. `Park::No` keeps the error the call always had (`StaticBearer`, the device grant and
`RenewingCredentials` answer it, so a wrong key fails as before); any other answer is `Refused { .., park }`.
The slug `model_locked` on a status other than `402` is `ModelLocked` for every provider. A `402` is `Refused`
with the slug `over_limit` whatever the gateway's problem document called it, so the person reads one event: the run
tries another model once and parks until it is woken when none is left (ADR 0067 decision 3, amended
2026-11-25). Nothing parks a run in this crate: the LLM agent's park is its own slice.

`InjectedRun` answers by the slug, whatever the status, and a `402` says the person's usage limit is
reached whatever the slug:

| Answer | Park |
|---|---|
| `unauthenticated`, `run_not_bound`, `lease_expired` | `OnTimer(FIRST_PARK)` |
| any `402` (`over_limit`, `model_locked`, or no problem document) | `UntilWoken` |
| `rate_limited`, `unavailable` | `OnTimer` of `Retry-After`, clamped to `FIRST_PARK ..= MAX_PARK` |
| none (`ProblemType::Absent`), on a `401` or `403` | `OnTimer(FIRST_PARK)`, as `unauthenticated` (*unverified* that an edge's denial has no problem body) |
| `model_locked` on another status, another slug, a `type` that cannot be read (`ProblemType::Unreadable`), none on another status | `No` |

## Features and environment

None.

## Tests

Doctests (the crate docs run as tests in CI's `cargo test --doc`) and unit
tests in `src/error.rs` (`class_table`,
`retry_after_comes_from_the_rate_limit_only`,
`a_source_is_kept_and_printed_once`), `src/credentials.rs` (a fixed key and an empty one, giving up, `Debug` without values, a header replaced by name, the double's renewal; the injector's one header on both attempts, its refusal of a call with no run, and its giving up; `the_injector_parks_by_the_table` (with the clamp to `MAX_PARK`), `an_unreadable_problem_type_parks_nothing`, `the_other_providers_never_park`, and `the_injector_sends_the_prompt_statement_and_the_envelope_when_the_run_has_them`), `src/prompt.rs` (a compact JWS kept as it came, every malformed value refused without its content, the 8 KiB bound, `Debug` without values, the serde round trip), `src/signed.rs` (the request target; a cloud call signed over its run, body and statement's `jti`, anew each time; a device call that names its run and binds the envelope's `nonce`; no prompt signed `null`; no run and an unreadable prompt id refused; keys that cannot sign send nothing, with their class), `src/error.rs` also `a_refusal_is_classed_by_its_status`, `a_refusals_retry_after_is_said_for_a_rate_limit_only` and `a_refusal_shows_its_status_and_slug_only`, `src/mock.rs` and `src/types.rs` (the usage accounting:
`a_part_above_its_total_is_added_in_and_one_within_it_is_left`, `usages_add_part_by_part_and_saturate`), and
`src/profiles.rs` (the seven levels and the three wires, the ADR 0064 example, and every refusal: an unknown level or
wire, a duplicate, the budgets, the caps, the preset references, the reserved extra-body members;
`a_context_window_is_one_to_two_to_the_53_minus_one_tokens` and
`the_profiles_window_wins_over_the_configured_one`), `src/client.rs`'s `the_default_window_is_the_profiles`
and `src/mock.rs`'s `a_profiles_window_wins_over_the_told_one`. No external
service, no environment variables. [`adam-model-openai`](../adam-model-openai/README.md)
tests a real implementation against a mock HTTP server.

## See also

[`adam-model-openai`](../adam-model-openai/README.md),
[`adam-llm-agent`](../adam-llm-agent/README.md),
[`adam-error`](../adam-error/README.md),
[root README](../../README.md).
