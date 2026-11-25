# adam-model-openai

An OpenAI-compatible `adam_model::ModelClient`: chat completions over HTTP,
with streaming and tool calling.

## Where it sits

An **adapter** of the model port in [`adam-model`](../adam-model/README.md).
It speaks `POST {base_url}/chat/completions`, so it works with any server or
gateway that mirrors that protocol. Agents hold an `adam_model::DynModel` and
never see this crate's types. Retries are not done here: failures map onto
`ModelError` and the runtime decides.

## API at a glance

| Item | What |
|---|---|
| `OpenAiConfig` | `base_url`, `api_key: SecretString`, `timeout`, `extra_headers`; `OpenAiConfig::new(base_url, api_key)` |
| `OpenAiCompatible::with_credentials(DynCredentials)` | the headers of every call come from this provider instead of `api_key` ([ADR 0058](../../docs/decisions/0058-the-models-credentials-come-from-a-provider-asked-for-each-call.md)); see *Credentials* below |
| `OpenAiCompatible` | the client: `OpenAiCompatible::new(config)`, `.with_max_tokens_field(field)`, `.with_extra_body(map)` (members merged into every request), `.with_echo_reasoning(Some(field))` (send reasoning back), `.with_prompt_cache_key(true)` (send `ModelRequest::prompt_cache_key` as the body's `prompt_cache_key`; off by default; a `with_extra_body` member of the same name wins; whether every gateway accepts it is *unverified*; OpenAI's reference page could not be fetched on 2026-11-15, so the field name is *unverified* against a primary source too), `.with_context_window(alias, tokens)` (what `ModelClient::context_window` answers for that alias; nothing is sent) |
| `OpenAiCompatible::with_profiles(ModelProfiles)` | the deployment's per-model profiles (`MODEL_ALIASES_FILE`): `ModelClient::profile` answers them, and a request's `reasoning_effort` goes out through the model's `reasoningWire`, after the extra body (*Reasoning effort*) |
| `PROVIDER` | `"openai"`: what `ModelClient::provider` says for this client, whatever gateway is behind it |
| `ReasoningField` | `ReasoningContent` (`reasoning_content`) or `Reasoning` (`reasoning`): the member that carries reasoning when a client echoes it |
| `MaxTokensField` | `MaxTokens` (default) or `MaxCompletionTokens`, for models that want the newer field name |
| `ExtraRoots`, `OpenAiCompatible::with_extra_roots(&roots)` | `MODEL_CA_FILE` ([ADR 0067](../../docs/decisions/0067-a-model-call-through-an-injector-names-its-run-and-a-refusal-parks-the-run.md) decision 9): `ExtraRoots::read(path)` or `from_pem(bytes)` reads a PEM bundle and checks it with the TLS backend (a file that cannot be read, is not PEM, holds no certificate or one the backend refuses is `InvalidCaFile`); `with_extra_roots` builds the client again trusting them **beside** the default roots, for the model endpoint only. `Debug` says how many |
| `OpenAiConfigError` | invalid configuration: `InvalidBaseUrl`, `InvalidHeader`, `InvalidApiKey`, `ReservedBodyKey`, `InvalidCaFile { reason, source }`, `CaFileTooLarge { limit }` (a file over `MAX_CA_FILE_BYTES`, 1 MiB, not read past it), `CredentialHeader` (an `extra_headers` entry named `authorization` or `proxy-authorization`), `Client`; the last four are new and the enum is `#[non_exhaustive]`; see *Errors* |

```rust
use std::sync::Arc;
use adam_model::DynModel;
use adam_model_openai::{OpenAiCompatible, OpenAiConfig};
use secrecy::SecretString;

let model: DynModel = Arc::new(OpenAiCompatible::new(OpenAiConfig::new(
    "https://gateway.example.com/v1",
    SecretString::from("sk-..."),
))?);
```

Behaviour (details in the crate docs, `src/lib.rs`):

* A tool call the server gave **no id** (or an empty one) gets one made up: `call_` and 16 random hexadecimal digits, new for every
  call of every response and the same in the stream and in the assembled message. It used to be `call_0`, `call_1`... in every
  turn, and an id is what ties a person's answer to the call that asked. The agent journals the response with its ids, so a
  resumed run reads the ones that were made (`adam-llm-agent`'s `call_ids_a_client_made_up_are_journaled_and_a_resumed_run_keeps_them`).
* `Retry-After` (seconds or HTTP date) surfaces in `ModelError::RateLimited`.
* `timeout` bounds a whole `complete` call; for `stream` it bounds the wait
  for headers and then the silence between chunks.
* Response size caps, all constants: a body over 16 MiB, an SSE line over 1 MiB,
  one event's `data:` over 4 MiB, or a stream over 64 MiB is refused with a
  `Protocol` error (class `Corrupt`) before the buffer grows past the cap. A
  gateway that streams one endless line can no longer grow the process without bound.
* The key is a `SecretString`, sent as a sensitive `Authorization: Bearer`
  header, and never appears in `Debug`, errors or logs. An empty key sends no
  header. It is the default credentials provider (`StaticBearer`), asked for each call like any other. The gateway's address is not shown either: the `Debug` of `OpenAiConfig` and of
  `OpenAiCompatible` print its scheme, host and port only (`endpoint_for_logs(url)`, public: `<unset>` for a blank
  one, `<set>` for one that is no absolute URL), because a deployment keeps the address in a secret
  (an internal host, a tenant in the path, a key in the query).
* A message whose content has several text parts (a continued conversation merges adjacent
  user messages) goes out as **one string**, the parts joined with a blank line (`\n\n`); the
  parts stay in the stored state. An array of typed parts is for a part that is not text
  (`ContentPart` has only `Text` today), so a chat template that wants a string content always gets one.
* Tool-call arguments that are not JSON do not fail the call: the response carries the call with `ToolCall::invalid_arguments` set to the raw text (cut at 8 KiB, with a marker saying how much) and `arguments` the JSON string `INVALID_ARGUMENTS_MARKER`, and the agent answers it with an error. A call like that is sent back to the provider with `{}` as its arguments. A call with no name is still `Protocol`.
* A tool call whose arguments are complete is announced while the stream goes on: `ModelDelta::ToolCallComplete` with
  the call the assembled response holds (the same id, name and arguments), once per call, the first time the accumulated
  argument text parses as a JSON object, and for a call whose text never does (empty, or not JSON) immediately before
  `Finished` ([ADR 0049](../../docs/decisions/0049-the-tool-calls-of-a-turn-run-at-the-same-time.md), amended). A
  provider that sends the id and the name in one chunk and the arguments in later ones is announced when the arguments
  close, never as `{}`; a provider that sends a whole call in one chunk is announced in that chunk, after its
  `ToolCallStarted`; and a provider that sends the arguments before the name is announced when the name arrives (a call
  with no name is not a call: `finish` refuses it), never skipped.
* A transport failure is a `Transient` that keeps the `reqwest` error as its `source`, except a connection that could
  not be made (refused, no route, a name that does not resolve), which is `Unreachable`, of class `Transient` too. A
  timeout, the connect timeout included, stays `Transient`, and so does a TLS handshake that failed (a `rustls::Error`
  anywhere in the chain, `io::Error`s opened with `get_ref`): `transient model error: TLS handshake failed`, since
  waiting cures neither a CA not trusted nor a name that does not match.
  **Visible change:** a refused connection's message was `transient model error: connection failed` and is now
  `the model endpoint could not be reached`; a body that is not JSON is a `Protocol`
  that keeps the parser's error. The message does not repeat the source.
* A response that stops short is `Transient`, so the runtime retries the turn: a stream (or `[DONE]`)
  that ended with no `finish_reason`, a last chunk cut off mid-JSON, a body cut off mid-JSON. A *complete*
  chunk that is not JSON, and a complete non-streamed body whose choice has no `finish_reason` (absent or
  `null`), stay `Protocol`: a server that never sends it fails fast, not after every retry.

## TLS provider

The feature `aws-lc` (default) links reqwest and rustls on aws-lc-rs, as the coder does. A host with its own
provider (ring, the platform's) depends on the crate with `default-features = false` and installs it before
the first request: `rustls::crypto::ring::default_provider().install_default()`. `adam-model-oauth` has the
same feature. `tools/tls-provider/check.sh` (CI job `tls-provider`) fails when aws-lc-rs is in that tree.

## Images

`OpenAiCompatible::with_input_modalities(alias, modalities)` is what `ModelClient::input_modalities` answers for that alias, and text only for any other (a subagent on another alias fails closed); `adam-service` sets it from `MODEL_INPUT_MODALITIES` for the alias `MODEL`. On the wire (*unverified* on every provider: the OpenAI API reference answered 403 on 2026-11-15, so the shapes are from memory):

* a user message that holds an image is an **array** `[{"type":"text","text":...},{"type":"image_url","image_url":{"url":"data:image/png;base64,..."}}]`, no `detail`; one with no image stays a string, so every text-only body is the bytes it was;
* the images of a **run of tool messages** follow it in one user message, `Images from tool results: c1 shot.png; c2 b.png`, then the parts: the tool messages stay text (a tool-role message that holds an image is *unverified*), and each body's `messages` starts with the one before it;
* an image with no bytes in `ModelRequest::image_data` is its stand-in line. A chat template that insists on string content rejects the array: the setting is off by default for that reason.

## A provider's opaque members

Gemini 3, behind an OpenAI-compatible endpoint, attaches an opaque
`extra_content.google.thought_signature` to the assistant message and to each tool call it returns,
and refuses the next request with a 400 when a tool call comes back without it (adam-rs#322: 60 of
60 bench runs failed on their second call). This client keeps **every** member it does not model,
on the message and on each call, and sends it back value for value:

| Direction | What |
|---|---|
| parse, completion | the unknown members of `choices[0].message` and of each `tool_calls[i]` (`extra_content`, and siblings) go into `Message::Assistant.provider.members` and `ToolCall.opaque`. The modelled members are not opaque: `role`, `content`, `tool_calls`, `reasoning_content`, `reasoning`, and the output-only `refusal`, `audio`, `function_call`, `annotations` on the message; `id`, `type`, `function` on the call |
| parse, stream | a delta's own unknown members go to the message (the same filter); its unknown members on a tool call are accumulated per call (last value wins per key) and reach the finished call. A final chunk shaped as a whole `message` instead of a `delta` contributes its unknown members too — no provider we test sends one, so that path is defensive |
| build | the members go out on the assistant message and on each call **only when the turn is bound to the request's model alias and endpoint**; the modelled keys win on a collision |
| provenance | a parse does not know either, so it leaves the turn unstamped; the client stamps `req.model` and its endpoint id in `complete` and in the streamed `Finished` |

The **output-only** members (`refusal`, `audio`, `function_call`, `annotations`) are consumed and
never stored: they are response-only, a provider never needs them back, and keeping one would change
every request that carries it. A signature is not one of them. `tool_call_id` is not filtered — it
cannot collide with an assistant message's fields and replaying it is harmless — and both modelled
sets name the same keys.

The endpoint id is `openai:` and the first 16 hexadecimal digits of the SHA-256 of the client's
normalised `base_url`, computed once when the client is built. It is not a secret, but a fingerprint
of the base URL: it confirms a guess. **It cannot separate two clients on one base URL** — two
tenants behind one gateway URL, told apart by an `extra_headers` value or a credential, are one
endpoint to this carrier, and a signature from one would be sent to the other. A deployment that
needs that separation must give each tenant a distinct base URL (a distinct path or host); a
deployment-set endpoint id is a follow-up. A run that changes model or endpoint drops the old turn's
members: they never reach another model. A message with no carrier is the bytes it always was.

A **reasoning block** (a `reasoning` or `reasoning_content` value that is an object or array, not
a string) is carried as `provider.reasoning`, raw. When the client is set to echo reasoning, that
value goes back under the configured field **in place of** the text; when it is not, nothing of it
is sent and the client clears it before the history holds the response (ADR 0020).
`ModelResponse.reasoning` stays the text-only view.

**Limits.** The block is a raw JSON value, not a typed turn with its own members: a provider whose
block needs a shape of its own needs a new type, not this carrier. A member **inside** a tool call's
`function` object (rather than beside it) is not carried: `function` is modelled (`name`,
`arguments`). A `message`-shaped chunk contributes only its unknown members and reasoning block: its
text and calls are not read (the stream's deltas carry them).

## Reasoning

A model in thinking mode sends its reasoning beside its answer, and this client reads it:

| Where | Member | Becomes |
|---|---|---|
| stream, `choices[0].delta` | `reasoning_content` or `reasoning` (a string; the first that is not empty) | `ModelDelta::Reasoning`, then `ModelResponse.reasoning` |
| completion, `choices[0].message` | `reasoning_content` or `reasoning` | `ModelResponse.reasoning` |

A value that is not a string (a structured `reasoning` some gateways add) is not reasoning text; when the client echoes
reasoning it is kept raw in the carrier (*A provider's opaque members*) and sent back under the configured field. Off (the
default) nothing of it is stored. The chunk it came in still counts for its content and calls. The reasoning is **never** the
answer's text (`Message::text()`); the **text view is never in the history and never sent back** by default. A structured
block lives only in the carrier, only when the client echoes reasoning (then it goes back bound to the model and endpoint);
off, it is cleared before the response reaches the history. `ModelResponse.reasoning` is for people to read. `with_extra_body(map)` merges a JSON object into every request body at its top level
(it wins over what the client wrote, except the members a typed reasoning level's wire duplicates: the level wins, below;
`model`, `messages`, `tools`, `tool_choice` and `stream` are refused with
`OpenAiConfigError::ReservedBodyKey`), which is how a deployment turns reasoning on where the model needs a flag
(`MODEL_EXTRA_BODY` of `adam-service`).

**Sending it back is the exception, and it is the provider's rule, not ours** (*verified 2026-10-05*,
<https://api-docs.deepseek.com/guides/thinking_mode>): DeepSeek, for a request that carries `tools`, "the `reasoning_content` must be
fully passed back to the API in all subsequent requests... If your code does not correctly pass back `reasoning_content`, the API will return
a 400 error", and for a request without `tools` it "does not need to be passed back; even if passed to the API, it will be ignored". So
`with_echo_reasoning(Some(field))` keeps the reasoning in `Message::Assistant.reasoning` (the history, so a run's stored state grows with it) and
sends it on the assistant message under `field`'s name; `None`, the default, sends none, whatever the message holds.

Provider facts the parsing rests on, each *verified 2026-10-05* from the page named unless it says *unverified*:

| Provider | Response field | Request flag to turn it on | Source |
|---|---|---|---|
| DeepSeek (API) | `reasoning_content`, a delta in a stream, beside `content` in a completion | `{"thinking": {"type": "enabled"}}` (on by default for the V4 models: "Thinking mode is enabled by default, with the default effort being `high`"); `{"thinking": {"type": "disabled"}}` turns it off | <https://api-docs.deepseek.com/guides/thinking_mode> |
| GLM (Z.ai) | `reasoning_content` in streaming deltas | `{"thinking": {"type": "enabled"}}` or `"disabled"`; GLM-4.7 and later think by default, and the GLM-5.3 line cannot turn it off; `"clear_thinking": false` keeps the reasoning of earlier turns ("preserved thinking": it asks for the complete `reasoning_content` back on the assistant message) | <https://docs.z.ai/guides/capabilities/thinking-mode> |
| LiteLLM | `reasoning_content` in a message and as a stream delta, for DeepSeek, Anthropic, Bedrock, Vertex AI, OpenRouter, xAI, Google AI Studio, Perplexity, Mistral, Groq (and `thinking_blocks` for Anthropic only, which this client does not read) | `reasoning_effort` (`low`, `medium`, `high`, across providers); `{"type": "enabled", "budget_tokens": n}` as `thinking` for Anthropic models | <https://docs.litellm.ai/docs/reasoning_content> |
| vLLM | `reasoning` (current: "the primary field... previously `reasoning_content`, now deprecated"), a delta in a stream | model-dependent `chat_template_kwargs`: `{"thinking": true}` (DeepSeek-V3.1, Granite 3.2), `{"enable_thinking": false}` to turn off what is on by default (Qwen3); `reasoning_effort` for Gemma 4 | <https://docs.vllm.ai/en/latest/features/reasoning_outputs.html> |
| OpenRouter | `reasoning` (a string) and `reasoning_details` (a structured array) in a message; in a stream's delta the page names `reasoning_details` (`delta.reasoning` as a string is *unverified* from the page, read as the same member) | `{"reasoning": {"effort": "low".."high" or "max_tokens": n or "exclude": true}}` | <https://openrouter.ai/docs/use-cases/reasoning-tokens> |
| Ollama | the OpenAI-compatible endpoint takes `reasoning_effort` and `reasoning.effort`; the **name of its response field** there is *unverified* (the page says `message.thinking` for the native API only) | `reasoning_effort` | <https://docs.ollama.com/api/openai-compatibility> |

## Reasoning effort

A call may name a reasoning level (`ModelRequest::reasoning_effort`, ADR 0063). The client sends it **only**
when the deployment's profiles give the request's model a `reasoningWire` (`with_profiles`;
`MODEL_ALIASES_FILE` in `adam-service`). No profile, no wire or no level sends nothing: the body is byte for
byte the one without the feature.

| `reasoningWire` | Member written |
|---|---|
| `reasoning_effort` | `{"reasoning_effort": "<level>"}` — **verified 2026-11-21** on the deployment's gateway (every family accepts it; Gemini rejects `max` and `xhigh`) |
| `reasoning_object` | `{"reasoning": {"effort": "<level>"}}`, merged into the `reasoning` object the extra body already has; its other members stay. A `reasoning` member that is not an object is replaced, since the name collides: the typed level wins. **Unverified here** (a 400 on Gemini; ADR 0063) |
| `thinking_budget` | `{"thinking": {"type": "enabled", "budget_tokens": <n>}}`, `<n>` from the profile's `reasoningBudgets` for that level. **Unverified here** (`thinking` is ignored or rejected by this gateway; ADR 0063) |

The typed level is applied **after** the extra body (`MODEL_EXTRA_BODY` until ADR 0064 slice 3 replaces it with
the profile's `extraBody`), and it removes only the members its wire would duplicate: `reasoning_effort` and
`reasoning.effort` for the first two wires (a `reasoning` object that held the effort and empties is removed;
one the extra body wrote empty stays), `thinking` for the third. Every other member stays, so a flag the model
needs is not dropped — including, for `thinking_budget`, the extra body's `reasoning_effort` and
`reasoning.effort`, which the ADR's letter leaves in place. `none` is a real level and is sent as `"none"`; an
**absent** level sends nothing. A level the profile does not declare is the assembly's to refuse, not the
client's: the client is not a second validator.

**Limits.** `thinking` is removed wholesale, as the ADR says: an extra body's other `thinking` members
(`clear_thinking`, a provider's own) are lost, and merging them is a follow-up (adam-rs#330). The wires are
asymmetric on a non-object `reasoning`: `reasoning_object` replaces it (the member name collides), while
`reasoning_effort` leaves it (only `reasoning.effort` is its duplicate, and a non-object holds none). A
hand-built profile with the `thinking_budget` wire and no budget for the level is
`ModelError::InvalidRequest`, naming the model and the level; the profiles loader refuses such a file at
startup, so only a profile built in code reaches it. The loader also refuses `none` with `thinking_budget`: the
shape has no disabled form, so that level would send thinking **enabled**.

## Usage

The `usage` of a completion, and of the last chunk of a stream (`stream_options.include_usage`: empty
`choices`, the usage of the whole request; the chunks before it say `"usage": null`), becomes
`adam_model::Usage`:

| Member | `Usage` |
|---|---|
| `prompt_tokens`, `completion_tokens` | `input_tokens`, `output_tokens` |
| `completion_tokens_details.reasoning_tokens` | `reasoning_tokens` |
| `prompt_tokens_details.cached_tokens` | `cached_input_tokens` |
| `prompt_tokens_details.cache_write_tokens` | `cache_write_input_tokens` |

The names and the streaming rule are *verified 2026-10-08* in OpenAI's API reference,
<https://developers.openai.com/api/reference/resources/chat/subresources/completions/methods/create>
("reasoning tokens ... are still counted in the total completion tokens"). A detail that is absent,
`null` or not a whole number leaves its part `None`. A gateway that reports a part beside a smaller
total gets it added in (`Usage::accounted`). Which OpenAI-compatible gateways send the details, and
whether `cache_write_tokens` is disjoint from `cached_tokens` everywhere, is *unverified*.

## Errors

Failures map onto `adam_model::ModelError`, which is classified (see
[`adam-model`](../adam-model/README.md#errors) and
[`adam-error`](../adam-error/README.md)). The client never retries.

| Cause | `ModelError` | Class |
|---|---|---|
| HTTP 429 (`Retry-After` kept) | `RateLimited` | `RateLimited` |
| a refusal the provider parks (below) | `Refused { status, slug, retry_after, park }` | by its status |
| a refusal whose problem type is `model_locked`, for every provider | `ModelLocked { model }` | `Rejected` |
| a connection that could not be made (`reqwest::Error::is_connect`, not a timeout, not a TLS failure) | `Unreachable` | `Transient` |
| HTTP 408, 5xx; timeout; a failed TLS handshake; transport error; a stream or body that ended before the model said why it stopped | `Transient` | `Transient` |
| HTTP 401, 403 | `Auth` | `Unauthenticated` |
| HTTP 400, 413, 422 naming the context window | `ContextLength` | `Invalid` |
| any other 4xx; a request that cannot be built | `InvalidRequest` | `Invalid` |
| 1xx/3xx (redirects are not followed); malformed (complete) JSON, wrong shape, a tool call with no name | `Protocol` | `Corrupt` |

`ModelError::is_context_overflow()` is true for exactly these two shapes (the code
`context_length_exceeded` and the words "maximum context length"); the agent loop compacts its
history once on it. The bodies of other providers (Anthropic-style, Gemini, llama.cpp, Ollama) are
*unverified* and none is mapped without a captured body.

**Refusals** ([ADR 0067](../../docs/decisions/0067-a-model-call-through-an-injector-names-its-run-and-a-refusal-parks-the-run.md)
decision 3). For an answer of status 400 or above, before the table's own rows, the client builds an
`adam_model::Refusal`: the status, the `Retry-After` (seconds or an HTTP date; a number too large for a `u64` is the
longest wait, not none) and the problem type, read only from an `application/problem+json` body (any parameters, any
case). It is `Absent` for no body, no `type` member or `about:blank`; a `Slug` when the `type`'s last path segment, or
the member as is when it holds no `/`, is a word of at most 64 letters, digits, `_`, `-` and `.`; and `Unreadable` for
any other `type` (a `urn:`, a query, a fragment, a trailing `/`, not a string) or a body that is not a JSON object,
which `InjectedRun` never parks on. The slug `model_locked` is `ModelLocked`; otherwise the provider is asked `parks`: `No`
keeps the table's error exactly (a key of one's own fails as before), any other answer is `Refused`, which holds no body
and no header. `InjectedRun`'s answers are in [`adam-model`](../adam-model/README.md#refusals). A gateway refuses before a
response streams, so an error object inside a stream is never a refusal.

An error object inside a `200` or a stream is `RateLimited` for a
`rate_limit` type, `ContextLength` for a context-length code, `InvalidRequest`
for `invalid_request` or `authentication`, and `Transient` for anything else.

`OpenAiConfigError` (from `OpenAiCompatible::new`, `with_extra_body`, `with_extra_roots` and `ExtraRoots`) is `Invalid` for
`InvalidBaseUrl`, `InvalidHeader`, `InvalidApiKey`, `ReservedBodyKey`, `InvalidCaFile` (its reader's or TLS backend's
error is its `source`), `CaFileTooLarge` and `CredentialHeader`, and `Internal` for
`Client` (the HTTP client could not be built; the `reqwest` error is its
`source`). No message carries the URL, the header value or the key.

## Credentials

Every call asks the client's `ModelCredentials` for its headers ([ADR 0058](../../docs/decisions/0058-the-models-credentials-come-from-a-provider-asked-for-each-call.md)): the key of the
`OpenAiConfig` by default, or the provider given with `with_credentials`, so a host can mint or renew a token at the moment
of use, prove it over the request, or send a run's tag with no key.

* The provider is told the method (`POST`), the full URL, the attempt and the request's `run` (`ModelRequest::run`), on the
  first try and on the retry after a 401, for a completion and a stream alike. With `adam_model::InjectedRun` the call
  carries `X-Run-Id` and no `Authorization`: the key of the `OpenAiConfig` is replaced, and a call with no run sends nothing. A request's `prompt_statement` is given to the provider as `CredentialRequest::prompt_statement`, so the injector also sends `Prompt-Statement` and `Prompt-User-Signature` (ADR 0077); it is never in the body. The
  provider is also given the body's exact bytes (`CredentialRequest::body`), and the URL is kept as the URL parser writes
  it, so `adam_model::SignedRequests` signs the request target and the body that are sent (ADR 0068 amended 2026-11-25).
  No `extra_headers` entry may carry a credential: `authorization` and `proxy-authorization` (any case) are refused
  when the client is built (`CredentialHeader`), so nothing but the provider sends one. Its headers are sent as sensitive values and
  replace any of the same name; `content-type`, `content-length`, `accept`, `host` and `transfer-encoding` are refused, as is
  a value that is no valid header, with `InvalidRequest` (the value is never in the error) and nothing sent.
* A provider's error is the call's, before anything is sent: its class decides whether the runtime retries.
* On a **401** the provider gets the `Rejection` (status, `WWW-Authenticate`, `DPoP-Nonce`). `Retry` asks for new headers
  and sends the same body once more (`bytes::Bytes`, no copy); a second 401, or `GiveUp` (a fixed key's answer), is the
  call's `Auth` error. A 403 is never retried. Streams and completions behave the same.

## Features and environment

No Cargo features. TLS is `rustls` (workspace `reqwest` configuration), verified by the platform verifier; `with_extra_roots`
adds roots to it. The crate reads no variable itself: `MODEL_CA_FILE` is read by `adam-service`.

## Tests

* `tests/http.rs`: the client against a `wiremock` server (requests, streaming,
  tool calls, error mapping and classes, timeouts, source chains, reasoning in a stream and a completion, the extra body on every request, reasoning sent back only when echoed). Always
  runs, no network.
* The carrier cases by name: `tests/http.rs`'s `a_tool_calls_extra_content_survives_the_next_request` (the
  Gemini repro), `a_message_level_extra_content_survives_the_next_request`,
  `a_delta_message_level_extra_content_survives_the_next_request`, `a_message_shaped_chunk_keeps_its_opaque_members`,
  `an_output_only_delta_member_is_not_carried`,
  `streamed_tool_call_extra_content_survives_the_next_request`,
  `a_reasoning_block_is_echoed_raw_by_a_client_that_echoes_reasoning`,
  `a_reasoning_block_alone_is_not_stored_without_echo`, `a_reasoning_block_is_cleared_but_members_stay_without_echo`,
  `opaque_members_are_dropped_when_the_run_changes_model`, `opaque_members_are_dropped_at_another_endpoint` and
  `the_client_stamps_the_requests_own_model_alias`; `src/wire.rs`'s `a_bound_turns_members_go_back_and_the_modelled_keys_win`,
  `an_unbound_turn_sends_none_of_its_members` and `a_response_without_opaque_members_parses_as_before`.
* The reasoning-wire cases by name: `src/wire.rs`'s `a_typed_level_wins_over_the_extra_body_and_keeps_its_other_members`
  (the ADR's pinned order), `each_wire_writes_its_own_member` (all three wires, each with an extra body carrying all three
  members), `an_emptied_reasoning_object_is_removed_and_its_other_members_stay`,
  `an_empty_reasoning_object_the_extra_body_wrote_stays`, `the_reasoning_object_wire_keeps_the_objects_other_members`,
  `the_thinking_budget_wire_replaces_thinking_wholesale`, `none_is_a_level_that_is_sent_and_an_absent_one_is_not`,
  `without_a_profile_a_wire_or_a_level_the_body_is_byte_identical`,
  `a_thinking_budget_with_no_budget_names_the_model_and_the_level` and
  `a_hand_built_thinking_budget_profile_refuses_none`; `src/lib.rs`'s
  `the_profiles_are_answered_for_their_models_and_debug_shows_ids_only`; `crates/adam-model/src/profiles.rs`'s
  `the_thinking_budget_wire_refuses_the_level_none`; and `tests/model_wire.rs` in `adam-service` for the end-to-end from
  `MODEL_ALIASES_FILE` through `ModelConfig::client()`.
* Unit tests: `src/errors.rs` (`status_mapping`, `a_problem_type_is_read_by_its_last_segment`,
  `a_problem_document_is_told_by_its_content_type`, `retry_after_seconds_and_date`), `src/lib.rs` also
  `a_rustls_error_is_found_however_it_is_wrapped`, `src/lib.rs`
  (`config_error_class_table`, `bad_headers_and_keys_are_rejected`, which refuses a credential in `extra_headers`,
  `debug_shows_the_gateway_by_scheme_and_host_only`,
  `the_provider_is_openai_and_a_window_is_said_for_its_alias_only`) and `src/wire.rs` (the request shape, including several text parts
  sent as one string; the usage parts of a completion and of a stream's last chunk, absent ones, and reasoning beside a smaller
  completion count; made-up ids for calls with none: random, distinct, the same in the stream and the message).
* `tests/credentials.rs`: the provider against a `wiremock` server: asked for every call, a 401 renewed once with the same
  body, a second 401 as the answer, the configured key never retried, a stream renewed, a proof told the method and URL, a
  provider with no key, a provider's error sent nowhere (its class kept), and the framing headers refused. Always runs.
* `tests/injector.rs`: `InjectedRun` against a `wiremock` server: every completion and stream carries `X-Run-Id` and no
  `Authorization` (a key in the configuration is never sent), the run is in no body, the retry after a 401 names the run
  too, a call with no run sends no request, and a request with a prompt statement carries `Prompt-Statement` and
  `Prompt-User-Signature` (the statement alone without an envelope, neither without one), never in the body. Always runs.
* `tests/request_signature.rs`: `SignedRequests` over the injector against a `wiremock` server: every completion and
  stream carries a `Turn-Signature` that verifies over the run, method, path and exact body the server received and the
  statement's `jti`, each with its own nonce; the retry after a 401 is signed anew; a call with no run sends nothing.
  Always runs.
* `tests/refusals.rs`: refusals against a `wiremock` server: under `InjectedRun` each slug of the table, completed and
  streamed, is `Refused` with its park, status, slug and `Retry-After` (120 s is a 120 s timer, 5 s a 30 s one) in one
  request, and its class follows its status; a `Retry-After` in the year 9999 or too large for a `u64` parks for 24 h
  and is kept as said, and an HTTP date two minutes ahead parks for about two minutes; a problem `type` that cannot be
  read (`urn:`, a query, a fragment, a trailing `/`, 65 characters) parks nothing on a 401, 403 or 402, while no `type`
  or `about:blank` on a 403 parks as `unauthenticated`; a 401 or a 403 with no body, a plain body, a JSON body that is no problem
  document or only a `Retry-After` parks for 30 s; `model_locked` is `ModelLocked` under the injector and a fixed key; an
  unknown slug, a slugless 500 and a 402 whose `type` is in plain JSON are today's errors; under `StaticBearer` every answer
  is today's error, against `Display` goldens of what `errors::from_http` gave at `9f40a3b`; no `Refused` nor `ModelLocked` holds the body's
  words or a secret in its `Display`, `Debug` or source chain; an error object inside a stream is not a refusal; a port with
  nothing listening is `Unreachable` (class `Transient`) for a completion and a stream, and a timeout is `Transient`, not
  `Unreachable`. Always runs.
* `tests/ca_file.rs`: a TLS server on the loopback (rustls, a test CA and a server certificate for `localhost` and
  `127.0.0.1` in `tests/fixtures/`, valid until 2126, signing nothing else) is refused with the default roots (`Transient`, "TLS handshake failed", never `Unreachable`), nothing
  reaching it, and answers once the client trusts the CA with `with_extra_roots`; a missing file, an empty one, a key, bad
  base64 and a block that is no certificate are each `InvalidCaFile`, with the cause kept as the source; a file over 1 MiB
  is `CaFileTooLarge` and one of exactly 1 MiB is read. Always runs. The fixtures are test only: `tests/fixtures/README.md`
  says how to make them again.
* `tests/wiremock_compose.rs`: the client against the `mock-openai` WireMock of `compose.yaml` (text and tool-call answers,
  streamed or not, the error scenarios), and **every scripted model** (`mock-coder`, `mock-assistant`, `mock-researcher`, `mock-painter`)
  played from its first request to its final answer both as a completion and as a stream, which must say the same, with the
  coder's last answer arriving over time: the guard that the SSE twins of `dev/wiremock/mock-openai` do not drift from the
  scripts they copy. Passes without doing anything unless `ADAM_TEST_MOCK_OPENAI_URL` is set (CI's `compose` job sets it).
* `tests/live.rs`: optional, against a real endpoint. Passes without doing
  anything unless the variables are set.

| Variable | Meaning |
|---|---|
| `ADAM_TEST_MOCK_OPENAI_URL` | the root of `mock-openai` (without `/v1`, for example `http://127.0.0.1:8081`); enables `tests/wiremock_compose.rs` |
| `ADAM_TEST_OPENAI_BASE_URL` | endpoint (with `/v1`); with the key, enables `tests/live.rs` |
| `ADAM_TEST_OPENAI_API_KEY` | its key |
| `ADAM_TEST_OPENAI_MODEL` | model alias (default `gpt-4o-mini`) |

`tests/live.rs` is not run in CI. No conformance testkit exists for `ModelClient` yet.

## See also

[`adam-model`](../adam-model/README.md),
[`adam-llm-agent`](../adam-llm-agent/README.md).
