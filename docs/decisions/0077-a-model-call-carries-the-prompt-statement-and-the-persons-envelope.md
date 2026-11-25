# 0077. A model call carries the prompt statement and the person's envelope

Status: **Accepted** (2026-11-25), as this repository's side of the shared prompt-signature contract (its decisions 3
and 6). The extension's URI is *proposed* there; a change to its shape is that contract's decision, not this ADR's.

## Context

A client that signs what a person asked wants the gateway in front of the model to see, on every model call a task
makes, two values: the **prompt statement** (a JWS over the prompt) and, when it has one, the **person's envelope**
(a JWS the person's device made). The gateway verifies them; adam only carries them. Two facts shape how:

- **Every call of the task, not only the first.** A task's model calls are the root run's, a local subagent's and a
  session round's, each made by a different run of the same tree ([ADR 0067](0067-a-model-call-through-an-injector-names-its-run-and-a-refusal-parks-the-run.md)
  already makes all of them name the root in `X-Run-Id`). A value that only the root's calls carried would leave the
  others unsigned.
- **A message brings them, and a task takes several messages.** A steer ([ADR 0016](0016-a-message-sent-to-a-working-task-is-steered-into-it.md))
  or an answer is a later message of the same task, and it may bring a new statement.

adam carries per-message host conveniences as optional extensions the card lists and a request activates
([ADR 0006](0006-a2ui-and-the-vymalo-extensions-in-adam-rs.md)); `locale/v1` ([ADR 0069](0069-the-interface-language-is-a-hint-and-a-hint-is-all-it-is.md))
is the nearest.

## Decision

### 1. `prompt-statement/v1`

The URI is `https://agents.vymalo.com/a2a/extensions/prompt-statement/v1`. The card's entry is optional and has no
`params`. A request that **activates** it may carry, on the message,
`metadata[<URI>] = {"statement": "<JWS>", "user": "<JWS>"}`, `user` absent (or `null`) when there is no envelope.

- **Read only when activated** (`adam_a2a::statement_of_request`): a message whose request did not name the extension
  carries nothing, whatever its metadata says, and the inbound payload key a sender might spell is removed.
- **Checked, not verified.** Each value must be a compact JWS (RFC 7515 §7.1: three base64url segments, header and
  signature not empty) of at most 8 KiB (`adam_model::MAX_PROMPT_JWS_BYTES`). That is what makes a value safe as a
  header; whether the signature is good is the gateway's to decide.
- **A malformed entry refuses the message** (`InvalidParams`), and nothing is written. Unlike a locale it cannot be
  dropped: a message that brings no values keeps the old ones (decision 2), so a broken entry taken as "none" would
  sign the new prompt with the old statement.

### 2. The values belong to the run, and the newest message that brings them wins

The values are kept with the run in its state, `Conversation::prompt_statement`, **not** in its context, so no tool
reads them. The message a run starts with sets them; a **later message of the task** (a steer, an answer) that brings
new ones **replaces** both (a new statement without an envelope leaves no envelope); a later message that brings none
**keeps** them. A later task in the same conversation starts with none of the earlier task's: its message brings its
own.

A local subagent and a session's round start with the values of the run that started them (the first message's
payload key `prompt_statement`, written by `ToolCtx::start_child` and the session tools only). A child that is already
running when a steer arrives keeps what it started with; one started after it gets the new ones.

The state is durable, so a restart or another worker sends the same values; the model request's field is not
serialized, so no journal holds them.

### 3. The injector sends them, as two headers, on every call

`adam_model::InjectedRun` (`MODEL_AUTH=injector`) adds, beside `X-Run-Id`:

| Header | Value |
|---|---|
| `Prompt-Statement` | the statement, byte for byte |
| `Prompt-User-Signature` | the person's envelope, byte for byte; absent when there is none |

A run with no values sends neither. A key or a device grant sends neither either: those talk to the model's own
endpoint, which has no use for a person's signature. The turn signature of [ADR 0068](0068-adam-signs-its-own-history.md)
is not part of this extension.

### 4. The card lists it exactly where the headers are sent

`adam-coder` and `adam-agent` list the entry when `MODEL_AUTH=injector` (`ServiceConfig::prompt_statement`, read by
every role as the word only); with a key or a device grant they list nothing, and no message is read for it. No new
variable.

### 5. Never in a log or an error

Neither value is logged, at any level. `PromptStatement`'s `Debug` holds no value, the credential headers' `Debug`
holds names only, and every refusal names the member that is wrong and never its content.

## Consequences

- **Built:** `adam-model` (`PromptStatement`, the headers, `ModelRequest::prompt_statement`,
  `CredentialRequest::prompt_statement`, `InjectedRun`); `adam-model-openai` passes the request's values to its
  provider; `adam-a2a` (the URI, `ExtensionConfig::prompt_statement`, `statement_of_request`); `adam-a2a-runtime` (the
  payload key on `submit` and on a person's message to a session, `PAYLOAD_PROMPT_STATEMENT`); `adam-llm-agent` (the
  state, the steer, the children, both model calls of a turn and of a summary); `adam-service` (the card). The test
  `crates/adam-a2a-runtime/tests/prompt_statement.rs` drives a task through the real client to a mock endpoint: every
  call of the root, of a local subagent and of a session's round carries the headers, a steer replaces them, one with
  none keeps them, a request that did not activate the extension sends neither, and no value is in any log line.
- **`CredentialRequest` gained a public field.** Code that builds one with a struct literal adds
  `prompt_statement: None`; no trait changed.
- **Not built here:** the client half and the gateway that verifies (the shared prompt-signature contract's other
  sides). In a split deployment the chart renders `MODEL_AUTH` on the workers only, so the control plane's card does
  not list the extension; that is a gap in `docs/reference/parity.md`.
- **The engine** (`adam-engine`) has no card; a host that builds its client with `InjectedRun` sets nothing that fills
  the values, which is a gap listed beside it.

## Amended 2026-11-25 (the device's envelope alone, and the request signature)

The shared prompt-signature contract's device path has no orchestrator: a device sends the person's envelope alone. So
an activated entry may now be `{"user": "<JWS>"}` with no `statement` (`PromptStatement::envelope`,
`PromptStatement::statement` is an `Option`); an entry with neither, or a member that is not a string, is still
refused. Such a run sends `Prompt-User-Signature` and no `Prompt-Statement`. An engine's host gives the values with
`Say::with_prompt_statement`, which closes this ADR's engine gap. adam now reads one thing of the values: the prompt's
id, for its request signature ([ADR 0068](0068-adam-signs-its-own-history.md), *Amended 2026-11-25 (the request
signature)*): the statement's `jti`, or the envelope's `nonce`. With `SignedRequests` the values are sent whatever the
inner credentials (not only by `InjectedRun`), because the signature binds them.
