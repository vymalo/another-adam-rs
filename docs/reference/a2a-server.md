# The A2A server: what a client sees

How a task looks from outside: the methods, follow-ups, continuing a finished task, push notifications, listing,
the extended card, the card's signature, and the three live streams (steps, text, reasoning). The request path itself is in
[Architecture](../architecture.md#request-in-events-out). Code: `crates/adam-a2a` (server),
`crates/adam-a2a-runtime` (backend over the runtime). Extensions: the `adam-a2a-extensions` skill and
[ADR 0006](../decisions/0006-a2ui-and-the-vymalo-extensions-in-adam-rs.md).

## Methods

All of A2A 1.0's JSON-RPC methods are served (`crates/adam-a2a/src/handler.rs`); what is optional is off
until a deployment turns it on, and says so in the card (*verified 2026-10-07* against
<https://a2a-protocol.org/latest/specification/>, §3.3.4: a capability that is false or absent makes its operations
answer with the matching error).

| Method | Needs | Off or missing |
|---|---|---|
| `SendMessage` (blocking), `SendStreamingMessage`, `GetTask`, `CancelTask`, `SubscribeToTask` | | |
| `ListTasks` | | |
| `CreateTaskPushNotificationConfig`, `GetTaskPushNotificationConfig`, `ListTaskPushNotificationConfigs`, `DeleteTaskPushNotificationConfig`, and `configuration.taskPushNotificationConfig` of a send | `A2A_PUSH_ALLOWED_URLS` | `PushNotificationNotSupported`; `capabilities.pushNotifications` is false |
| `GetExtendedAgentCard` | an extended card (`card.extended` of the agent folder) **and** bearer authentication | `UnsupportedOperation`; `capabilities.extendedAgentCard` is false |
| the card's `signatures` and `GET /.well-known/jwks.json` | `A2A_CARD_SIGNING_KEY_FILE` | no signature, no key set |

## Push notifications

A client names a webhook for a task and is told when the task changes, without holding a stream open. **A notification
is a hint; `GetTask` is the truth** (the specification itself says duplicates may occur and delivery may stop). Design and
reasons: [ADR 0030](../decisions/0030-a2a-push-notifications-list-tasks-extended-card-signatures.md); the delivery sequence and
a config's lifecycle: [Architecture](../architecture.md#push-notification-delivery).

| What | Behaviour |
|---|---|
| **Who may register what** | the deployment's allow-list (`A2A_PUSH_ALLOWED_URLS`: URL prefixes or hosts). A webhook that is not on it, is not `https`, has credentials in its URL or is a private, loopback or link-local address is `InvalidParams`. Delivery judges again and never connects to a private address or follows a redirect |
| **Whose** | a config belongs to its task and its caller: another caller's task is `TaskNotFound` for every method, as for `GetTask`. At most 16 configs per task |
| **Secrets** | `token` and `authentication.credentials` are write-only: create, get and list never return them. The server stores them as given, in clear, in its database |
| **What a webhook receives** | `POST` with `Content-Type: application/a2a+json`, `Authorization: <scheme> <credentials>` when `authentication` was given, `A2A-Notification-Token: <token>` when `token` was; the body is a `StreamResponse`: a `statusUpdate` when the task's status (state or message) changed, an `artifactUpdate` for each new artifact, artifacts first. Answer 2xx to acknowledge |
| **The header of `token`** | the specification does not name one; `A2A-Notification-Token` is what the official Rust SDK sends (*verified 2026-10-07*, `a2a-server-lf` 0.4.4 `src/push/sender.rs`) |
| **Signed notifications** | the specification does not ask for them (no JWT, no JWKS for notifications, *verified 2026-10-07*), so there are none: authenticate a notification by the credentials you chose |
| **Order and loss** | events the deliverer saw are delivered in order and a failed one is sent again before any later one. A state the task passed through between two polls (2 s) or while no replica ran is not delivered: the next event is the state the task is in. A webhook may hear one event twice |
| **Retry and give-up** | a non-2xx answer, a timeout (15 s) or a refused connection is retried with a capped exponential backoff (1 s to 5 min); after `A2A_PUSH_GIVE_UP_AFTER_SECS` (1 hour) of failures delivery to that webhook stops and the reason is recorded (`410 Gone` or a refused address stops it at once). Nothing more arrives after that: poll `GetTask` |
| **In a send** | `configuration.taskPushNotificationConfig` (the 1.0 name). The earlier drafts' `pushNotificationConfig` is refused with `InvalidParams` and starts nothing: the SDK's JSON-RPC layer would drop it silently (*observed 2026-10-07*) and the client would wait for notifications nobody registered. A config the policy refuses (or one past the cap of the task the message continues) fails the call before any task exists. If the store fails after the task was created, the call still returns the task and logs a warning (task id, error class): register the webhook again with `CreateTaskPushNotificationConfig` |
| **A new config** | starts from what the task says when it is created: only later changes are sent, and a config for a task that is already terminal receives nothing. In a `SendMessage` the baseline is the task as the message created it |
| **When it ends** | once the task is terminal and everything was sent. The config stays readable until it is deleted (delete is idempotent) or the run is purged |

```text
POST /hook HTTP/1.1
Content-Type: application/a2a+json
Authorization: Bearer <credentials>
A2A-Notification-Token: <token>

{"statusUpdate":{"taskId":"…","contextId":"…","status":{"state":"TASK_STATE_COMPLETED","timestamp":"…"}}}
```

## ListTasks

The caller's own tasks (never another's, whatever the filters say), most recently updated first, by cursor.

| Field | Behaviour |
|---|---|
| `contextId` | only that context |
| `status` | only that state. `completed` is exact in the store; `failed`/`canceled`, `submitted`/`working` and `input-required` are told apart by reading each candidate (at most 500 runs per page: a rare filter returns a short page and a token to continue) |
| `statusTimestampAfter` | the last change at or after it (inclusive) |
| `pageSize` | 50 by default, 1 to 100 (a larger value is clamped, zero and negative mean the default) |
| `pageToken` | the `nextPageToken` of a page of **the same caller and filters**; anything else, forged or not, is `InvalidParams` ("invalid page token") |
| `historyLength`, `includeArtifacts` | as in the specification: `artifacts` is omitted entirely unless asked |
| response | `tasks`, `nextPageToken` (empty on the last page), `pageSize`, `totalSize`: **exact without a status filter and for `completed`, an upper bound for the other states** (counting them exactly would read every run; the in-memory backend of `adam-a2a` counts exactly) |

The token is a cursor (the position of the last task), not an offset, and bound to the caller and the filters by a digest;
it is not signed, and a forged position can only move within the forger's own tasks.

## The extended card

`GetExtendedAgentCard` returns the public card plus what the agent adds for authenticated callers: in an agent folder,
`card.extended` with a `description` (replaces the public one) and `skills` (added; the id of a public skill replaces it);
in code, `AgentCardConfig::with_extended_card` (which can add extensions too, and an extension only the extended card declares
can be activated by an authenticated caller). Only with bearer authentication: an anonymous server has none, the card says
`extendedAgentCard: false`, and the method answers `UnsupportedOperation`. The extended card holds no secret: put nothing
in it that you would not give every holder of a token.

## The card's signature

With `A2A_CARD_SIGNING_KEY_FILE` the public and the extended card carry one JWS in `signatures` (RFC 7515 over the RFC 8785
canonical card, *verified 2026-10-07*, specification §8.4): `alg` `ES256` (a P-256 key) or `EdDSA` (Ed25519), `typ: JOSE`,
`kid` (the key's RFC 7638 thumbprint unless `A2A_CARD_SIGNING_KEY_ID`), `jku` only if `A2A_CARD_SIGNING_JKU`. The public key
set is served at `GET /.well-known/jwks.json`. To verify: take the card as you received it (either JSON form parses to the
same card), remove `signatures`, remove `null`s and empty arrays and objects except `capabilities`, `defaultInputModes`,
`defaultOutputModes`, `skills`, `supportedInterfaces` and a skill's `tags`, remove an extension's `required: false`, leave
`securityRequirements` and extension `params` as they are, canonicalize with RFC 8785, and verify
`BASE64URL(protected) + "." + BASE64URL(payload)`. In Rust: `adam_a2a::VerifyingKey::from_jwk(&jwks, kid)` and
`verify_card(&card)`. *Unverified:* that the payload equals another SDK's byte for byte (the specification has no test
vector); generate a key with `openssl genpkey -algorithm ed25519` (or `-algorithm EC -pkeyopt ec_paramgen_curve:P-256`).

## Follow-ups

| The message | What happens |
|---|---|
| carries a `taskId`, the task is `input-required` | delivered to the run (`Runtime::deliver`) |
| carries a `taskId`, the task has finished | `-32004` (`UnsupportedOperationError`) |
| carries a `taskId`, the task is `submitted` or `working` | `-32602`, **unless the request activated `steer/v1`**: then it goes to the run's inbox, is read at its next step, and the answer is the task, still working ([ADR 0016](../decisions/0016-a-message-sent-to-a-working-task-is-steered-into-it.md)) |
| has a `contextId`, no `taskId` | delivered to the context's open task, or starts a new one |
| has `referenceTaskIds` | starts from the conversation of one of them, below |

## A new task that continues a finished one

A refinement or a rework is a new task in the same `contextId`, because a finished task accepts nothing.
`Message.referenceTaskIds` names what it builds on ([ADR 0003](../decisions/0003-a-new-task-continues-the-task-it-references.md)).

```mermaid
sequenceDiagram
    autonumber
    participant C as A2A client
    participant B as RuntimeTaskBackend<br/>adam-a2a-runtime
    participant R as Runtime<br/>adam-runtime
    participant DB as Store<br/>Postgres
    participant A as AgentStarter or Agent<br/>LlmStarter, LlmAgent

    C->>B: submit(caller, message with referenceTaskIds [t1], no taskId, contextId c1)
    loop each reference, at most MAX_REFERENCES (8), in order (none at all for the anonymous caller)
        B->>DB: load_run(reference), the raw record
        B->>B: this agent's, this caller's, in c1, terminal?<br/>if not: debug log and next reference
        B->>R: view(reference), only for one that passed
        Note over B,R: a state that does not decode: warn, next reference
    end
    Note over B: a fresh task started though some were given: one info line with counts
    alt a reference qualifies (t1)
        B->>R: start_with_id_continuing(task_id_for(...), agent, inbound, conversation, t1)
        R->>DB: load_run(run id): a repeat answers false here
        R->>DB: load_run(t1), and the envelope's agent state
        R->>A: init_continuing(inbound, prior state, t1)
        Note over A: LlmAgent: carry the history, answer a tool call<br/>that never got its result as stopped, reset the counters,<br/>cap the history at 256 KiB (tool output first),<br/>keep the roles alternating
        A-->>R: the new run's state
        R->>DB: create_run (Runnable, version 1)
    else none, or the conversation has an open task
        B->>R: start_with_id (a fresh start), or deliver to the open task
    end
    B-->>C: Task t2 (a new task id), same contextId
```

* **The reference is checked from the raw record**, before any state is decoded: this agent's, this
  caller's, in the same context, terminal. Anything else is skipped, and the client gets the fresh task it
  would for an id that never existed, so it learns nothing about other callers' tasks. At most 8 references.
  A message without a `contextId` continues nothing; the backend never guesses "the latest task".
* **"Caller" is the authenticated subject** (`token-<index>`); reordering the tokens hands a history to
  whoever holds that index next. The anonymous caller is every client at once, so it continues nothing.
* **It works on a front that holds only the starter**: the prior state is decoded as the starter's `State`.
  `init_continuing` defaults to `init`; a state that does not decode falls back to `init` with a warning.
  An agent that wraps another must forward it.
* **The history is bounded.** `Conversation::continued` shortens old tool outputs first, then drops the
  oldest whole turns beyond `MAX_CARRIED_BYTES` (256 KiB), saying so in a marker text and `omitted_turns`.
  The first user message and the newest prior turn are kept; adjacent user messages are merged so roles alternate.
* **The new run is an ordinary run**: new id, own journal, limits and worktree.
* **The coder carries the work on**: a rework can check out the branch an earlier task pushed, and
  `open_pull_request` then updates the same pull request ([coder](coder-agent.md#continuing-a-task)).

## Steps

A tool call, the work it hands to another agent and the commands that agent runs are **steps**
(`RunEvent::Step`, [ADR 0007](../decisions/0007-progress-as-steps-and-streamed-text.md)). The agent
reports `tool:<call id>` before and after every call; a tool says more with `ToolCtx::emit_progress` and
`report_step`. A client that **activated `steps/v1`** gets each as a `working` status whose metadata
carries the report; any other client reads a line of text. A tool call's report carries its `input` and
`output`, redacted by the agent and cut to 4 KiB and 8 KiB ([ADR 0011](../decisions/0011-a-tool-calls-step-carries-its-input-and-output.md));
a step too big for a `NOTIFY` crosses between processes without them.

A step's **label** is a title a person reads ("Edit a file", "Run the checks"), never the tool's name: the tool says it with
`Tool::step_style` (`#[tool(label = "...")]`), an MCP tool with its `title`, a tool of a source with `ToolNote::label`, a subagent
with its own name capitalised. The model keeps calling the tool by its name. A client that did not activate `steps/v1` reads
the same label in its plain line (`Edit a file: done`) ([ADR 0027](../decisions/0027-every-tool-has-a-title-for-its-step.md)).

```mermaid
sequenceDiagram
    participant C as Client
    participant S as A2A server
    participant B as Subscription
    participant K as BroadcastSink
    participant A as LlmAgent
    C->>S: SendStreamingMessage, A2A-Extensions: steps/v1
    S->>S: Caller.extensions = named and declared by the card
    S->>B: subscribe(caller, task)
    S-->>C: header A2A-Extensions: steps/v1
    A->>K: Step tool:c1 running
    K-->>B: RunEvent::Step
    alt the caller activated steps/v1
        B->>B: admit: the first report of a state, a change of state, or an end
        B-->>C: working, one text part + the report in metadata
    else it did not
        B-->>C: working, one line of text
    end
```

```mermaid
stateDiagram-v2
    [*] --> Reported: the first report of a state of a step goes out
    Reported --> Held: the same state again within a second (only for an activated client)
    Held --> Reported: a second has passed
    Reported --> Reported: a change of state goes out at once
    Reported --> [*]: an end goes out, and the step is forgotten
```

Events are live and best effort. A streaming send submits the run and subscribes afterwards, so
`BroadcastSink` keeps a small replay of each run's recent events (the newest 64, none older than 30 s, no
`Status` or `Artifact`, dropped at every status change) and `subscribe_run` delivers it before the live ones,
with no gap and no duplicate: a step that started before the subscriber attached keeps its `input`. A
resubscribe within 30 s may repeat a step (a snapshot by id) or a text piece (it carries its `offset`). A
process with no event sink replays nothing and sees only the durable record
([ADR 0007](../decisions/0007-progress-as-steps-and-streamed-text.md), `crates/adam-runtime/src/events.rs`).

## Which build answers

The card says which build of the agent answers, so that a thread export can be tied to a commit and to the agent's files
([ADR 0028](../decisions/0028-the-card-says-which-build-answers.md)):

* `version` is `<crate version>+<first 7 characters of the commit>` (`0.1.0+6478fbc`, `0.1.0+unknown` for a build that was
  given no revision): semver build metadata, which does not take part in precedence. The commit is the build argument
  `ADAM_BUILD_REVISION` of `docker/coder/Dockerfile` (CI passes `github.sha`), read with `option_env!` by `adam-coder` and
  `adam-agent`.
* `capabilities.extensions` lists `https://agents.vymalo.com/a2a/extensions/build/v1` with `params`
  `{"revision": <the commit, whole, or "unknown">, "folderDigest": "sha256:..."}`. The digest is
  `adam-agent-fs`'s over the agent's files (the one the `agent files` line of the startup log shows), so a prompt mounted
  over the embedded copy changes it. The extension is optional and informational: `required: false`, nothing to
  activate, ignored by a client that does not know it. `adam_a2a::ExtensionConfig::build(revision, folder_digest)` declares it,
  `adam_agent::card_of_folder` and `adam_coder::agent_card_from` add it.

## Streamed text

A model turn streams by default (`LlmAgentBuilder::stream_text`): the journaled step `model:<turn>` calls
`ModelClient::stream` and sends what the model has written as `RunEvent::TextDelta` pieces. A client that
**activated `text-stream/v1`** gets each as an artifact chunk with its byte offset. The whole text is stated
once, by a `working` status for words before a tool call or by the status that ends the turn (`completed`
with `output.stream`, or `input-required` for a reply turned into a question).

```mermaid
sequenceDiagram
    participant C as Client
    participant B as Subscription
    participant K as BroadcastSink
    participant A as LlmAgent (step model:N)
    participant M as Model
    C->>B: SendStreamingMessage, A2A-Extensions: text-stream/v1
    A->>M: stream(request)
    M-->>A: Text deltas
    A->>A: coalesce: 200 bytes or 100 ms, at most 1024 bytes a piece
    A->>K: TextDelta(stream, offset, text)
    K-->>B: RunEvent::TextDelta
    alt the caller activated text-stream/v1
        B-->>C: artifact update: the piece, its offset, append, lastChunk
    else it did not
        B->>B: nothing: the whole reply comes with the turn
    end
    M-->>A: Finished(response)
    A->>K: TextDelta(last)
    A->>A: journal the response and the stream id, then Done with output.stream
    B-->>C: completed: the whole text, metadata {streamId}
```

```mermaid
stateDiagram-v2
    [*] --> Streaming: the first word that is not blank
    Streaming --> Streaming: a piece (200 bytes, or 100 ms since the last)
    Streaming --> Ended: the model finished: the last piece
    Streaming --> Abandoned: the model failed: the last piece, abandoned
    Ended --> Stated: output.stream (the answer), or agent_text with the stream (words before a tool call)
    Stated --> [*]
    Abandoned --> [*]: the run fails or retries as another stream
```

* Pieces are live and meant to be lost; a replay of a recorded step sends none and states the same
  words whole under the recorded stream id.
* A failure in the middle is the call's failure (`ModelFailure`): the same retry, the same failed run.
* A chunk is at most 1024 bytes so the event fits a `NOTIFY` payload.
* A client that did not activate the extension, and a blocking `message/send`, get the whole reply
  with the turn.

## Reasoning

A model in thinking mode writes its reasoning before its answer, on the same streamed step
([ADR 0020](../decisions/0020-reasoning-is-streamed-beside-the-answer-and-never-stored.md)).
`ModelDelta::Reasoning` (read from `reasoning_content` or `reasoning`, `crates/adam-model-openai/src/wire.rs`)
becomes `RunEvent::ReasoningDelta` in a stream of its own that ends before the words begin, and a
`text-stream/v1` chunk marked `"kind": "reasoning"`.

It is **never stored**: dropped before the journal, in no output, `turn_output` or step, and a replay
sends none. The model is sent its earlier reasoning only if its client is set to
(`MODEL_ECHO_REASONING`, which DeepSeek's thinking mode with tools needs). `MODEL_EXTRA_BODY` is how a
deployment asks a model that needs a flag to think.
