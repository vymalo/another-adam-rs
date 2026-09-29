# adam-a2a

Expose an adam-rs agent as an A2A (Agent2Agent) server: an `axum::Router`
over a small backend seam, with bearer authentication that fails closed.

## Where it sits

The **server-side adapter for the A2A protocol**, independent of the agent
runtime. It defines its own port, `TaskBackend`; the durable implementation is
[`adam-a2a-runtime`](../adam-a2a-runtime/README.md), and
[`adam-coder`](../adam-coder/README.md) mounts the result. This crate holds no
task state. JSON-RPC parsing, ProtoJSON and SSE framing are the official SDK's
(`a2a-lf`, `a2a-server-lf`); this crate implements the SDK's `RequestHandler`
on top of `TaskBackend` instead of using its `DefaultRequestHandler`, whose
resubscribe only works inside one process.

## API at a glance

| Item | What |
|---|---|
| `TaskBackend` (trait), `DynTaskBackend` | `submit(caller, message, task_id, context_id)`, `get`, `cancel`, `subscribe(caller, task_id) -> stream of TaskEvent` |
| `TaskEvent`, `BackendError`, `Caller` | events, errors (`#[non_exhaustive]`, see *Errors*), and the authenticated subject requests carry |
| `A2aServer::router(card, backend, auth)` | the `axum::Router`; `router_with_options(.., ServerOptions)` |
| `ServerOptions` | `with_keepalive_interval(..)` (the SDK sends an SSE comment every `SDK_KEEPALIVE_INTERVAL`, 15 s) |
| `AuthConfig` | `BearerTokens(Vec<SecretString>)` (constant-time comparison) or `AllowAnonymous` (logs a warning) |
| `AgentCardConfig`, `SkillConfig`, `ExtensionConfig` | the public agent card |
| `InMemoryBackend`, `InMemoryConfig` | reference backend, **only with feature `test-util`** |

```rust
use std::sync::Arc;
use adam_a2a::{A2aServer, AgentCardConfig, AuthConfig, InMemoryBackend}; // needs `test-util`
use secrecy::SecretString;

let card = AgentCardConfig::new(
    "echo", "Echoes what it is told", "http://127.0.0.1:8080/".parse().unwrap(), "0.1.0",
);
let auth = AuthConfig::BearerTokens(vec![SecretString::from("s3cret")]);
let app = A2aServer::router(card, Arc::new(InMemoryBackend::new()), auth);
let listener = tokio::net::TcpListener::bind("127.0.0.1:8080").await?;
axum::serve(listener, app).await?;
```

Protocol notes (details in the crate docs, `src/lib.rs`):

* Serves A2A **1.0** method names: `SendMessage`, `SendStreamingMessage`,
  `GetTask`, `CancelTask`, `SubscribeToTask`; task states on the wire are
  `TASK_STATE_*`. `ListTasks` is unsupported and push-notification methods
  return `PushNotificationNotSupported`.
* Every route except the agent card and `/healthz` answers 401 without a valid
  token. The middleware strips any client-sent identity header and injects the
  trusted `Caller`.
* A body that is not a JSON-RPC request gets HTTP 200 and a JSON-RPC error
  object with a null id, never the SDK extractor's plain-text 400/415/422/413:
  `-32700` when it is not JSON, `-32600` when it is JSON but not a request, is
  not declared as `application/json`, or is over the SDK's size limit. The
  layer sits inside authentication, so an anonymous caller still gets 401.
* Payload numbers come back as floats (ProtoJSON): send exact integers and
  money as strings.

*Unverified (2026-09-29):* the claim that the SDK speaks A2A 1.0 only is
recorded in the crate docs from the SDK versions `a2a-lf` 0.3 and
`a2a-server-lf` 0.4 (versions verified 2026-09-29 in the workspace
`Cargo.toml`); it was not re-checked against the SDK source for this README.

## Errors

`BackendError` implements `adam_error::Classify` (see
[`adam-error`](../adam-error/README.md)), and converts to one A2A error each.

| Variant | Class | A2A error the client sees |
|---|---|---|
| `TaskNotFound` (also another caller's task) | `NotFound` | `-32001` task not found |
| `NotCancelable` | `Rejected` | `-32002` task not cancelable |
| `InvalidParams` | `Invalid` | `-32602` invalid params, with the message |
| `Unavailable { message, source }` | `Transient` | `-32603` "backend temporarily unavailable" |
| `Internal { message, source }` | `Internal` | `-32603` "internal error" |

The server is the trust boundary: for `Unavailable` and `Internal` the client
gets only the fixed message, and the log gets the whole chain
(`adam_error::report`) under `backend unavailable` or `backend internal
error`. Build them with `BackendError::unavailable(msg)` and
`BackendError::internal(msg)`, then `.with_source(err)`. `is_retryable()` comes
from `Classify` and is true only for `Unavailable`. Malformed request bodies are
answered as described under *Protocol notes*, not through `BackendError`.

## Features

| Feature | Default | Effect |
|---|---|---|
| `test-util` | no | ships `InMemoryBackend` (pulls in `tokio-stream`); also useful to other repositories' tests |

No environment variables.

## Tests

`tests/round_trip.rs`: a real A2A client (`a2a-client-lf`) against the router
over TCP, using `InMemoryBackend` (card, send, streaming, get, cancel,
resubscribe, `input-required` follow-ups, error mapping, malformed bodies
(`malformed_json_with_a_valid_token_is_rejected_cleanly`,
`wrong_content_type_is_rejected_cleanly`,
`an_oversized_body_is_an_invalid_request`), authentication on every route,
keepalive frames, caller isolation). Unit tests in `src/backend.rs`
(`class_table`, `the_source_is_kept_and_not_repeated_in_the_message`,
`a2a_errors_do_not_leak_the_cause`). The crate's own
dev-dependency turns on `test-util`. Offline, no environment variables.

## See also

[`adam-a2a-runtime`](../adam-a2a-runtime/README.md),
[`adam-coder`](../adam-coder/README.md),
[`adam-error`](../adam-error/README.md).
