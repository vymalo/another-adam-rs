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
| `OpenAiConfigError` | invalid configuration |

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
  header.
* Malformed tool-call argument JSON is `ModelError::Protocol`.

## Features and environment

No Cargo features. TLS is `rustls` (workspace `reqwest` configuration).

## Tests

* `tests/http.rs`: the client against a `wiremock` server (requests, streaming,
  tool calls, error mapping, timeouts). Always runs, no network.
* `tests/live.rs`: optional, against a real endpoint. Passes without doing
  anything unless the variables are set.

| Variable | Meaning |
|---|---|
| `ADAM_TEST_OPENAI_BASE_URL` | endpoint (with `/v1`); with the key, enables `tests/live.rs` |
| `ADAM_TEST_OPENAI_API_KEY` | its key |
| `ADAM_TEST_OPENAI_MODEL` | model alias (default `gpt-4o-mini`) |

Not run in CI. No conformance testkit exists for `ModelClient` yet.

## See also

[`adam-model`](../adam-model/README.md),
[`adam-llm-agent`](../adam-llm-agent/README.md).
