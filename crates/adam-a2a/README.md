# adam-a2a

Expose an adam-rs agent as an A2A (Agent2Agent) server: an `axum::Router`
over a small backend seam, with bearer authentication that fails closed.

## Where it sits

The **server-side adapter for the A2A protocol**, independent of the agent
runtime. It defines its own port, `TaskBackend`; the durable implementation is
[`adam-a2a-runtime`](../adam-a2a-runtime/README.md), and
[`adam-coder`](../../bin/adam-coder/README.md) mounts the result. This crate holds no
task state. JSON-RPC and HTTP+JSON parsing, ProtoJSON and SSE framing are the official SDK's
(`a2a-lf`, `a2a-server-lf`); this crate implements the SDK's `RequestHandler`
on top of `TaskBackend` instead of using its `DefaultRequestHandler`, whose
resubscribe only works inside one process, and mounts it behind both bindings.

## API at a glance

| Item | What |
|---|---|
| `TaskBackend` (trait), `DynTaskBackend` | `submit(caller, message, task_id, context_id)`, `get`, `cancel`, `subscribe(caller, task_id) -> stream of TaskEvent`, and `list(caller, &TaskQuery) -> TaskPage` (a **default method** answering `UnsupportedOperation`: a backend written before `ListTasks` keeps compiling) |
| `TaskQuery`, `TaskPage`, `PageToken`, `DEFAULT_PAGE_SIZE`, `MAX_PAGE_SIZE` | `ListTasks`: the filters after the handler resolved the defaults, one page, and the cursor token (`PageToken::encode`/`decode`, bound to the caller and the filters) |
| `TEXT_STREAM_KIND_REASONING` | the `kind` (`"reasoning"`) of a `text-stream/v1` chunk that carries the model's reasoning, not its reply ([ADR 0020](../../docs/decisions/0020-reasoning-is-streamed-beside-the-answer-and-never-stored.md)) |
| `TaskEvent`, `BackendError`, `Caller` | events, errors (`#[non_exhaustive]`, see *Errors*), and the caller a request carries: the authenticated `subject` and the `extensions` the request activated (`Caller::new(subject)`, `with_extensions(..)`, `has_extension(uri)`; see *Extensions a request activates*) |
| `AgentCardConfig::extension_uris()` | the URIs the card declares: what a request may activate |
| `A2aServer::router(card, backend, auth)` | the `axum::Router`; `router_with_options(.., ServerOptions)` |
| `ServerOptions` | `with_keepalive_interval(..)` (the SDK sends an SSE comment every `SDK_KEEPALIVE_INTERVAL`, 15 s), `with_push(PushSupport)`, `with_card_signer(CardSigner)`, `with_docs(bool)` (Swagger UI and the OpenAPI document, **on by default**; the field `docs` is new and `Default` sets it `true`) |
| `push` (module) | **push notifications**, off unless a policy allows a webhook: `PushPolicy` (the allow-list and the SSRF rules), `PushStore` (the port the configs and their delivery progress live behind; `InMemoryPushStore` with `test-util`), `PushSupport` (store and policy together), `PushDeliverer` and `PushDeliveryOptions` (the loop), `PushSender` (one request), `PushCursor` (what a webhook has heard), `GuardedResolver` |
| `AgentCardConfig::with_extended_card(ExtendedCardConfig)` | what an authenticated caller sees on top of the public card (`GetExtendedAgentCard`) |
| `CardSigner`, `VerifyingKey`, `canonical_payload`, `canonicalize`, `SigningError`, `VerifyError` | the card's JWS (ES256 or EdDSA over the RFC 8785 canonical card); `generate_signing_key_pem` with `test-util` |
| `AuthConfig` | `BearerTokens(Vec<SecretString>)` (constant-time comparison) or `AllowAnonymous` (logs a warning) |
| `AgentCardConfig`, `SkillConfig`, `ExtensionConfig` | the public agent card |
| `ExtensionConfig::a2ui_v0_9_1()`, `ui_catalog()`, `thread_tools()`, `steps()`, `text_stream()`, `mentions()`, `steer()`, `build(revision, folder_digest)` | the card entries of an agent that draws on a screen, reports its work as steps or streams its reply: A2UI v0.9.1 (with `supportedCatalogIds` and `acceptsInlineCatalogs: true`), `ui-catalog/v1`, `thread-tools/v1`, `steps/v1`, `text-stream/v1`, `mentions/v1`, `steer/v1` and `build/v1` (the last seven optional; only `build/v1` has parameters); `A2UI_EXTENSION_V0_9_1`, `A2UI_BASIC_CATALOG_V0_9_1`, `A2UI_MEDIA_TYPE`, `UI_CATALOG_EXTENSION`, `THREAD_TOOLS_EXTENSION`, `STEPS_EXTENSION`, `TEXT_STREAM_EXTENSION` (and `TEXT_STREAM_KIND_REASONING`), `MENTIONS_EXTENSION`, `STEER_EXTENSION`, `BUILD_EXTENSION` are the URIs and the media type. **`build/v1`** is information only (nothing activates it): its `params` are `{revision, folderDigest}`, the build that answers and the agent files it runs ([ADR 0028](../../docs/decisions/0028-the-card-says-which-build-answers.md)); `build_version(package_version, revision)` is the card's `version` with the revision as semver build metadata (`0.1.0+6478fbc`, `0.1.0+unknown`), and `revision_of` cleans what a build baked in (`UNKNOWN_REVISION`). The contracts are the orchestration layer's (`docs/api/ui-catalog-v1.md`, `docs/api/thread-tools-v1.md`, `docs/api/steps-v1.md`, `docs/api/text-stream-v1.md`, `docs/api/mentions-v1.md`, `docs/api/steer-v1.md` in `vymalo/another-agentic-system`); what an agent does with the messages is [`adam-a2a-runtime`](../adam-a2a-runtime/README.md) (`vymalo_inbound`) and [`adam-ui`](../adam-ui/README.md) |
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

## Routes

| Route | Auth |
|---|---|
| `GET /.well-known/agent-card.json`, `GET /healthz` | public |
| `GET /.well-known/jwks.json` (only when the card is signed) | public |
| `GET /docs` (303 to `docs/`), `GET /docs/` and the files it loads, `GET /openapi.json` (unless `with_docs(false)`) | public |
| `POST /`: JSON-RPC, every A2A 1.0 method | bearer token |
| HTTP+JSON (A2A 1.0 §11): `POST /message:send`, `POST /message:stream`, `GET /tasks/{id}`, `GET /tasks`, `POST /tasks/{id}:cancel`, `POST /tasks/{id}:subscribe`, `POST`/`GET /tasks/{id}/pushNotificationConfigs`, `GET`/`DELETE /tasks/{id}/pushNotificationConfigs/{configId}`, `GET /extendedAgentCard`, and the SDK's aliases of earlier drafts (`src/rest.rs`, `REST_ROUTES`) | bearer token |
| anything else | bearer token, then 404 |

The card lists two interfaces at `AgentCardConfig::url`: `JSONRPC` first, `HTTP+JSON` second (the REST paths are relative
to it; *verified 2026-10-07*, <https://a2a-protocol.org/latest/specification/> §4.4.6, §11.3). Design:
[ADR 0031](../../docs/decisions/0031-swagger-ui-and-the-a2a-rest-binding.md).

**HTTP+JSON** is the SDK's `rest_router` (`a2a-server-lf` 0.4.4) over the same `BackendHandler` as JSON-RPC: the same
caller, extensions (`A2A-Extensions` header and `message.extensions`, echoed in the response), push configs, `ListTasks`,
extended card and error mapping. What the SDK's router does not do is added around it in `src/rest.rs`: the extractors'
plain-text refusals become the binding's `google.rpc.Status` envelope (`PARSE_ERROR` for a body that is not JSON,
`INVALID_PARAMS` with the extractor's text for a query string that does not parse, `INVALID_REQUEST` for a body over the
limit or not declared as JSON), a send that names `configuration.pushNotificationConfig` is refused as on JSON-RPC, and a
401 is `UNAUTHENTICATED` in that envelope (at `POST /` it stays the JSON-RPC `-32000`).

**The docs** (`src/openapi.rs`, `src/docs.rs`): one OpenAPI 3.1 document of both bindings, **written by hand** (the SDK's
types derive no schema) and held to the code by the tests below. JSON-RPC is `POST /` with a `oneOf` of one schema per
method and a named example per method; streaming operations are `text/event-stream` and their descriptions give the
`curl` (Swagger UI cannot show a stream as it arrives). The bearer scheme is declared exactly when the server requires
one. The document is built once from the public card and the switches it shows: the same bytes for every caller, no
token, nothing of the extended card. Swagger UI 5.32.6 (Apache-2.0) is bundled in the binary by `utoipa-swagger-ui` 10.0.1
with `vendored` (*verified 2026-10-07* by reading its `build.rs`: with that feature the zip comes from the
`utoipa-swagger-ui-vendored` 0.2.0 crate, which ships it in `res/`, and nothing is downloaded; `minified` leaves the source
maps out). Only the eight files `index.html` loads are served, under
`Content-Security-Policy: default-src 'none'; script-src 'self'; style-src 'self' 'unsafe-inline'; img-src 'self' data:; …`;
the page reads `../openapi.json` (relative, so a nested router works), the document's server is `.` (Try it out calls the
address the page came from: Swagger UI resolves it against the document's URL, *verified 2026-10-07* in the bundled
`swagger-ui-bundle.js` and in headless Chromium), the validator badge and `?url=` are off and the token is not persisted.

Protocol notes (details in the crate docs, `src/lib.rs`; the SDK's `RequestHandler` is implemented in `src/handler.rs`):

* Serves A2A **1.0** method names: `SendMessage`, `SendStreamingMessage`,
  `GetTask`, `ListTasks`, `CancelTask`, `SubscribeToTask`, the four push-notification
  methods and `GetExtendedAgentCard`; task states on the wire are `TASK_STATE_*`.
  What is optional is **off by default and the card says so**: push notifications answer
  `PushNotificationNotSupported` unless `ServerOptions::with_push` was given a policy that allows a
  webhook, and `GetExtendedAgentCard` answers `UnsupportedOperation` without an extended card and
  bearer authentication (*verified 2026-10-07*,
  <https://a2a-protocol.org/latest/specification/> §3.3.4).
* `A2aServer::health_router()` is the `/healthz` route alone (200, `ok`), for a process that
  answers probes but serves no A2A, such as a worker.
* Every route except the agent card, `/healthz`, the docs and (when signed) the key set answers 401 without a valid
  token. The middleware strips any client-sent identity header and injects the
  trusted `Caller`.
* A body sent to `POST /` that is not a JSON-RPC request gets HTTP 200 and a JSON-RPC error
  object with a null id, never the SDK extractor's plain-text 400/415/422/413:
  `-32700` when it is not JSON, `-32600` when it is JSON but not a request, is
  not declared as `application/json`, or is over the SDK's size limit. The
  layer sits inside authentication, so an anonymous caller still gets 401.
* **Extensions a request activates.** A client names the extensions it wants in the `A2A-Extensions` request header (a
  comma-separated list) and, in a message it sends, in `message.extensions`. The handler gives the backend the ones
  the **card declares**, each once, in the order they were named (the header first), in `Caller::extensions`; a URI
  the card does not declare (or names with another version, a trailing slash or another case) is not activated, so a
  client cannot switch on what the agent never advertised. Every method does this for its own request: a poll, a
  cancel and a resubscribe carry their own header. The response lists what was activated in its `A2A-Extensions`
  header, and has none when nothing was (*verified* 2026-10-01,
  <https://a2a-protocol.org/latest/topics/extensions/>: "the response SHOULD include the `A2A-Extensions` header,
  listing all extensions that were successfully activated for that request"; the page does not mention
  `Message.extensions`, which the orchestration layer sends as well, so both count). Ownership never looks at
  `extensions`: a task belongs to a `subject`.
* **Push notifications** ([ADR 0030](../../docs/decisions/0030-a2a-push-notifications-list-tasks-extended-card-signatures.md),
  [what a client sees](../../docs/reference/a2a-server.md#push-notifications), the sequence and the config's lifecycle:
  [architecture](../../docs/architecture.md#push-notification-delivery)). A **notification is a hint; `GetTask` is
  the truth.** The handler checks a webhook against the `PushPolicy` at create time (`InvalidParams`), reads the task as the
  caller first (another caller's task is `TaskNotFound`), keeps at most 16 configs per task and returns configs
  **without** `token` and `credentials` (write-only; the store holds them as given, in clear). `PushDeliverer::run` is
  what delivers: it claims due configs from the `PushStore` (a lease), reads the task as its owner, sends the first event the
  webhook has not heard (`statusUpdate`/`artifactUpdate`, `Content-Type: application/a2a+json`,
  `Authorization: <scheme> <credentials>`, `A2A-Notification-Token: <token>`), and records the progress (compare-and-swap);
  a failed event is kept whole and sent again with capped exponential backoff until `give_up_after`, then the config is
  `GaveUp` with the reason. At least once and in order for what it saw; a state between two polls is not sent. The
  client resolves names itself and never connects to a private address, never follows a redirect and ignores
  `HTTP(S)_PROXY`. The specification asks for no signature on notifications (*verified 2026-10-07*), so there is none.
  Run one `PushDeliverer` beside the server (`PushSupport::deliverer`); several replicas share the work by leases. The
  durable store is `adam_a2a_runtime::StorePushStore`.
* A send whose configuration names the push config the way earlier drafts did (`pushNotificationConfig`) is refused with
  `InvalidParams` before any task starts: the SDK reads requests as proto3 JSON, which has no such member, so the config
  would be dropped silently (*observed 2026-10-07*). The 1.0 name is `taskPushNotificationConfig`.
* **A config in a send** is checked before the message is submitted (policy, header values, the cap of configs of the
  task the message continues): a refused one fails the call and creates no task. It is stored after the task exists, and
  if the store then fails the call **still returns the task** (a failure would make the client send the message again):
  a `warn` with the task id and the error class is logged, and the client registers the webhook with
  `CreateTaskPushNotificationConfig`.
* **ListTasks**: `TaskBackend::list` returns the caller's own tasks, most recently updated first, by cursor
  (`PageToken`, never an offset; bound by a digest to the caller and the filters; another caller's or a tampered token is
  `InvalidParams` "invalid page token"). The handler resolves `pageSize` (50, 1 to 100), omits `artifacts` unless
  `includeArtifacts`, applies `historyLength`, and always sends `nextPageToken` (empty on the last page). `totalSize` is
  whatever the backend counts: `InMemoryBackend` counts exactly; `RuntimeTaskBackend` is exact without a `status`
  filter and for `completed`, and an upper bound for the other states (see its README).
* **GetExtendedAgentCard**: the public card plus `ExtendedCardConfig` (a description, skills, extensions); only when
  configured **and** the server authenticates with bearer tokens (`AllowAnonymous` has no "authenticated": it is off and a
  warning is logged).
* **Card signatures** (`signing.rs`): with `ServerOptions::with_card_signer` the public and the extended card carry a JWS
  and `GET /.well-known/jwks.json` (public, only then) serves the key set. See the module docs for the exact canonicalization
  and what is *unverified* (agreement with other SDKs' payloads).
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
| `UnsupportedOperation` | `Rejected` | `-32004` unsupported operation: a message to a task that has finished (A2A's error for it) |
| `InvalidParams` | `Invalid` | `-32602` invalid params, with the message |
| `Unavailable { message, source }` | `Transient` | `-32603` "backend temporarily unavailable" |
| `Internal { message, source }` | `Internal` | `-32603` "internal error" |

`push::PushStoreError` (`NotFound`, `Conflict`, `Unavailable`, `Internal`; classes `NotFound`, `Conflict`, `Transient`,
`Internal`) is what the push store reports: the handler turns it into `TaskNotFound` or a `BackendError`
(`push_error` in `src/handler.rs`), and the deliverer decides from the class.

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
| `test-util` | no | ships `InMemoryBackend`, `InMemoryPushStore` and `generate_signing_key_pem` (pulls in `tokio-stream`); also useful to other repositories' tests |

No environment variables (the binaries read `A2A_PUSH_*`, `A2A_CARD_SIGNING_*` and `A2A_DOCS` in
[`adam-service`](../adam-service/README.md) and pass them in `ServerOptions`).

## Tests

`tests/rest.rs` (HTTP+JSON: the card's two interfaces in order; the official client over REST: send, get, list, cancel,
streaming, resubscribe; a task private to its caller on both bindings; errors as status and `ErrorInfo` reason; malformed
requests in the binding's envelope and never before authentication; extensions activated and echoed; push configs
created, read, listed and deleted with secrets write-only; the extended card; an anonymous server), and
`http_json_gives_the_backend_the_same_caller_and_extensions` in `tests/round_trip.rs`. `tests/docs.rs` (the page, its files
and the document are public under the CSP while calls are 401; off, they are closed like any unknown route; the document
validates against the official OpenAPI 3.1 JSON Schema, vendored in `tests/fixtures` (see `third-party-notices.md`), every
`$ref` resolves, operation ids are unique and path parameters declared; every JSON-RPC method the handler dispatches is in
the `oneOf` and the reverse, and each example reaches its method; every REST route is documented and routed and no other
method is served on its paths; the examples and real responses, stream events and errors of both bindings validate against
the document's schemas; the document is the same for every caller and names no token and nothing of the extended card; the
bearer scheme only when bearer is on). `method_tripwire` fails when `Cargo.lock` moves `a2a-server-lf` off 0.4.4: axum
cannot list a router's routes, so `RPC_METHODS` and `REST_ROUTES` mirror the SDK's source and must be re-read then.

`tests/push.rs` (the official client and a local webhook: off by default and card flags, create/get/list/delete, secrets write-only,
another caller refused, disallowed URLs refused, each state change delivered in order with the token and the credentials,
a config in `SendMessage`, retry after 500s, **delivery resumes after a restart on the same store without loss**, give-up
after the bound, a redirect never followed, a private address refused at delivery), `tests/list_tasks.rs` (order, pages by
cursor, filters, isolation, bad and foreign tokens), `tests/card.rs` (the extended card needs authentication and shows the
extra entries; the signature verifies with the key set and fails when the card is altered; the extended card is signed too)
and `tests/round_trip.rs`: a real A2A client (`a2a-client-lf`) against the router
over TCP, using `InMemoryBackend` (card, send, streaming, get, cancel,
resubscribe, `input-required` follow-ups, a message to a finished task as `-32004`, error mapping, malformed bodies
(`malformed_json_with_a_valid_token_is_rejected_cleanly`,
`wrong_content_type_is_rejected_cleanly`,
`an_oversized_body_is_an_invalid_request`), authentication on every route,
keepalive frames, caller isolation). Unit tests in `src/backend.rs`
(`class_table`, `the_source_is_kept_and_not_repeated_in_the_message`,
`a2a_errors_do_not_leak_the_cause`, the extension entries of `src/extensions.rs`, `steer/v1` included, the send paths and
the legacy push config of `src/rest.rs`, and which paths are public in `src/docs.rs` and `src/auth.rs`). The crate's own
dev-dependency turns on `test-util`. Offline, no environment variables.

## See also

[`adam-a2a-runtime`](../adam-a2a-runtime/README.md),
[`adam-coder`](../../bin/adam-coder/README.md),
[`adam-error`](../adam-error/README.md).
