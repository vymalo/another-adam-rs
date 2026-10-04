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
| `OpenAiCompatible` | the client: `OpenAiCompatible::new(config)`, `.with_max_tokens_field(field)` |
| `MaxTokensField` | `MaxTokens` (default) or `MaxCompletionTokens`, for models that want the newer field name |
| `OpenAiConfigError` | invalid configuration: `InvalidBaseUrl`, `InvalidHeader`, `InvalidApiKey`, `Client`; see *Errors* |

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

* `Retry-After` (seconds or HTTP date) surfaces in `ModelError::RateLimited`.
* `timeout` bounds a whole `complete` call; for `stream` it bounds the wait
  for headers and then the silence between chunks.
* The key is a `SecretString`, sent as a sensitive `Authorization: Bearer`
  header, and never appears in `Debug`, errors or logs. An empty key sends no
  header. The gateway's address is not shown either: the `Debug` of `OpenAiConfig` and of
  `OpenAiCompatible` print its scheme, host and port only (`endpoint_for_logs(url)`, public: `<unset>` for a blank
  one, `<set>` for one that is no absolute URL), because a deployment keeps the address in a secret
  (an internal host, a tenant in the path, a key in the query).
* A message whose content has several text parts (a continued conversation merges adjacent
  user messages) goes out as **one string**, the parts joined with a blank line (`\n\n`); the
  parts stay in the stored state. An array of typed parts is for a part that is not text
  (`ContentPart` has only `Text` today), so a chat template that wants a string content always gets one.
* Malformed tool-call argument JSON is `ModelError::Protocol`.
* A transport failure (timeout, connection error) is a `Transient` that keeps
  the `reqwest` error as its `source`; a body that is not JSON is a `Protocol`
  that keeps the parser's error. The message does not repeat the source.

## Errors

Failures map onto `adam_model::ModelError`, which is classified (see
[`adam-model`](../adam-model/README.md#errors) and
[`adam-error`](../adam-error/README.md)). The client never retries.

| Cause | `ModelError` | Class |
|---|---|---|
| HTTP 429 (`Retry-After` kept) | `RateLimited` | `RateLimited` |
| HTTP 408, 5xx; timeout; connection failure; transport error | `Transient` | `Transient` |
| HTTP 401, 403 | `Auth` | `Unauthenticated` |
| HTTP 400, 413, 422 naming the context window | `ContextLength` | `Invalid` |
| any other 4xx; a request that cannot be built | `InvalidRequest` | `Invalid` |
| 1xx/3xx (redirects are not followed); malformed JSON, tool arguments or stream | `Protocol` | `Corrupt` |

An error object inside a `200` or a stream is `RateLimited` for a
`rate_limit` type, `ContextLength` for a context-length code, `InvalidRequest`
for `invalid_request` or `authentication`, and `Transient` for anything else.

`OpenAiConfigError` (from `OpenAiCompatible::new`) is `Invalid` for
`InvalidBaseUrl`, `InvalidHeader` and `InvalidApiKey`, and `Internal` for
`Client` (the HTTP client could not be built; the `reqwest` error is its
`source`). No message carries the URL, the header value or the key.

## Features and environment

No Cargo features. TLS is `rustls` (workspace `reqwest` configuration).

## Tests

* `tests/http.rs`: the client against a `wiremock` server (requests, streaming,
  tool calls, error mapping and classes, timeouts, source chains). Always
  runs, no network.
* Unit tests: `src/errors.rs` (`status_mapping`), `src/lib.rs`
  (`config_error_class_table`, `debug_shows_the_gateway_by_scheme_and_host_only`) and `src/wire.rs` (the request shape, including several text parts
  sent as one string).
* `tests/wiremock_compose.rs`: the client against the `mock-openai` WireMock of `compose.yaml` (text and tool-call answers,
  streamed or not, the error scenarios), and **every scripted model** (`mock-coder`, `mock-assistant`, `mock-researcher`)
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
