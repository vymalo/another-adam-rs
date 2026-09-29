# adam-rs

A Rust framework for durable AI agents, in the spirit of [eve](https://eve.dev):
filesystem-first authoring, macros where eve uses file conventions, and every
piece of infrastructure behind a trait so each developer picks their own.

This repository currently contains the **durable-state layer**: the `Store`
trait, a shared conformance suite, and two production adapters.

| Crate | What it is |
|---|---|
| [`adam-error`](crates/adam-error/README.md) | The error model every crate shares: `ErrorClass`, the `Classify` trait, `BoxError` and `report()`. Pure: no I/O, no async |
| [`adam-core`](crates/adam-core/README.md) | `Store` trait, run/journal/lease types, in-memory reference store |
| [`adam-store-testkit`](crates/adam-store-testkit/README.md) | Conformance suite every store must pass (`store_conformance!`) |
| [`adam-store-postgres`](crates/adam-store-postgres/README.md) | PostgreSQL 12+ via `sqlx` 0.9 |
| [`adam-store-mongodb`](crates/adam-store-mongodb/README.md) | MongoDB 5.0+ via the official driver; standalone `mongod` is enough |
| [`adam-model`](crates/adam-model/README.md) | `ModelClient` trait (`complete` + streaming, tool calling), request/response types, `MockModel` test double |
| [`adam-model-openai`](crates/adam-model-openai/README.md) | `OpenAiCompatible`: any OpenAI-compatible chat-completions endpoint (gateway or provider) via `reqwest` + rustls |
| [`adam-workspace`](crates/adam-workspace/README.md) | Per-run git worktrees over a shared mirror, commit and push, and pull requests (`CodeHost`, GitHub). The token is passed per `git` invocation and never stored |
| [`adam-runtime`](crates/adam-runtime/README.md) | Durable agent loop: `Agent` trait, run state machine, `ctx.step` journaling, workers, retries, event sinks |
| [`adam-a2a`](crates/adam-a2a/README.md) | Expose an agent as an A2A 1.0 server (axum): `TaskBackend` seam, bearer auth (fail closed), `InMemoryBackend` under feature `test-util` |
| [`adam-acp`](crates/adam-acp/README.md) | ACP client that drives a coding agent (`opencode acp`) over stdio; ships a scripted fake agent for tests |
| [`adam-llm-agent`](crates/adam-llm-agent/README.md) | `LlmAgent`: the durable model/tool-calling loop (`Tool` trait, `NeedsInput` parking, limits, history truncation) on top of `adam-runtime` |
| [`adam-a2a-runtime`](crates/adam-a2a-runtime/README.md) | `RuntimeTaskBackend`: the A2A `TaskBackend` over `adam-runtime` (task = run, ownership per caller, `input-required` from parked runs); subscriptions are rebuilt from the store, so they survive restarts. Reusable by any agent |
| [`adam-coder`](crates/adam-coder/README.md) | The coder agent: a coding task to a verified pull request over A2A (worktree, OpenCode over ACP, bounded check cycles, commit, push, PR). Library and the `adam-coder` binary; image in `docker/coder`, chart in `deploy/coder` |

## The model

A **run** is one execution of an agent. The runtime owns the agent loop as an
explicit state machine, serializes it into `RunRecord::state` (JSON), and
commits every transition with a compare-and-swap on `version`. A worker that
dies loses nothing: another worker picks the run up from the last commit.

The **journal** records the outcome of every side effect a tool performs
through `ctx.step(..)`, keyed by `(run, seq)`. On replay, the recorded outcome
is returned instead of running the side effect again. The first writer wins,
and a replay that asks for a different step name at the same `seq` fails with
`NonDeterminism` instead of silently doing the wrong thing.

**Leases** stop two workers from advancing the same run at once. They are an
efficiency mechanism; the version CAS is what guarantees correctness, so a
worker whose lease expired mid-step still cannot overwrite newer state.

**Scheduling** is one indexed range scan: each run stores a derived `sched_at`
and is due when `sched_at <= now`.

| Status | `wake_at` | Due |
|---|---|---|
| runnable | none | immediately |
| runnable | set | at `wake_at` (retry backoff) |
| parked | set | at `wake_at` (timers, `ctx.sleep`) |
| parked | none | never; resumed by committing it back to runnable (approval, inbound message) |
| done / failed | – | never |

**Conversations**: at most one open (runnable or parked) run per
`(agent, conversation_id)`, enforced by a unique index. Two inbound messages
racing on one conversation cannot start two runs; the loser gets
`ConversationBusy` and resumes the open run instead. Creating a run with a
deterministic id gives idempotent "fire once" semantics, e.g. one run per cron
tick across all replicas.

## Using a store

```rust
use std::sync::Arc;
use adam_core::{DynStore, NewRun, RunStatus, RunUpdate};
use serde_json::json;

// Pick one. Both take your existing pool/database handle too.
let store: DynStore = Arc::new(adam_store_postgres::PgStore::connect(&pg_url).await?);
let store: DynStore = Arc::new(adam_store_mongodb::MongoStore::connect(&mongo_uri, "myapp").await?);

store.migrate().await?; // idempotent, safe to run from every replica at boot

let run = store.create_run(NewRun::new("support-bot", json!({"turn": 0})).conversation("chat-42")).await?;
let run = store.commit_run(run.id, run.version, RunUpdate::new(RunStatus::Parked, json!({"turn": 1}))).await?;
```

## How each adapter guarantees the contract

| Guarantee | PostgreSQL | MongoDB |
|---|---|---|
| Commit CAS | `UPDATE .. WHERE version = $n RETURNING` | `findOneAndUpdate({_id, version: n}, {$inc: {version: 1}})` |
| Journal first-writer-wins | `INSERT .. ON CONFLICT DO NOTHING`, read winner | `insertOne` with `_id = "<run>:<seq>"`, duplicate key → read winner |
| Exclusive claiming | `FOR UPDATE SKIP LOCKED` in one statement | read candidates, `updateMany` re-checking due/lease in the filter with a claim token, read back by token |
| One open run per conversation | partial unique index | plain unique index on `open_key`; closed runs get `~<run id>`, so no partial/sparse index is needed |
| Journal deleted with run | `ON DELETE CASCADE` | journal deleted first, then runs, in batches |
| State storage | `JSONB` (queryable with SQL) | real BSON document (queryable with dot paths) |
| Transactions needed | none held open | none (works on a standalone `mongod`) |

### Data caveats

* **MongoDB keys.** Agent state often contains JSON Schema keys like `$ref` and
  `$defs`, dotted keys, and empty keys. These are escaped reversibly (`%`
  prefix plus percent-encoding of `%`, `.`, `$`, NUL); ordinary keys are stored
  as-is so `state.messages.role` still works in queries. See
  `adam-store-mongodb/src/codec.rs`.
* **MongoDB integers** above `i64::MAX` are rejected with `InvalidInput`.
* **PostgreSQL NUL.** `JSONB` cannot hold `\u0000`; such state is rejected with
  `InvalidInput` instead of a raw driver error.
* **Time** is truncated to milliseconds in every store (BSON dates are
  millisecond precision), so all backends compare timestamps identically.
  Lease expiry uses the `now` the caller passes in; keep worker clocks in sync
  (NTP) and leave lease TTLs well above expected clock skew.

## Local development

`compose.yaml` (Compose Spec) starts the databases, WireMock mocks of the
external systems the code talks to, and a local git remote. Ports bind to
`127.0.0.1` only and every credential in the file is a dummy.

```sh
docker compose up -d --wait        # postgres, mongodb, mock-openai, mock-github, git-server
docker compose --profile app up -d --build --wait   # ... plus the coder, wired to the mocks
docker compose down -v             # stop and forget all state (volumes included)
```

| Service | Host address | What it is |
|---|---|---|
| `postgres` | `127.0.0.1:5432` | PostgreSQL 16, database `adam_test`, user and password `postgres` |
| `mongodb` | `127.0.0.1:27017` | MongoDB 7, standalone |
| `mock-openai` | `http://127.0.0.1:8081/v1` | WireMock: OpenAI-compatible chat completions (`/v1/chat/completions` and `/chat/completions`, plus `/v1/models`) |
| `mock-github` | `http://127.0.0.1:8082` | WireMock: the GitHub REST subset `adam-workspace` uses (list and open pull requests) |
| `git-server` | `http://127.0.0.1:8083/local/sandbox.git` | bare repositories over smart HTTP (nginx + git-http-backend), seeded with `local/sandbox.git`; no authentication |
| `coder` (profile `app`) | `http://127.0.0.1:8080/` | the coder agent built from `docker/coder/Dockerfile`, bearer token `dev-token` |

Host ports can be moved with `POSTGRES_PORT`, `MONGODB_PORT`, `MOCK_OPENAI_PORT`,
`MOCK_GITHUB_PORT`, `GIT_SERVER_PORT` and `CODER_PORT` (for example in a `.env`
file next to `compose.yaml`).

### Pointing the code at the mocks

```sh
export ADAM_TEST_POSTGRES_URL=postgres://postgres:postgres@127.0.0.1:5432/adam_test
export ADAM_TEST_MONGODB_URI=mongodb://127.0.0.1:27017
export ADAM_TEST_MOCK_OPENAI_URL=http://127.0.0.1:8081       # root of the mock, no /v1
export ADAM_TEST_MOCK_GITHUB_URL=http://127.0.0.1:8082

# a locally run adam-coder (cargo run -p adam-coder):
export DATABASE_URL=$ADAM_TEST_POSTGRES_URL
export MODEL_BASE_URL=http://127.0.0.1:8081/v1 MODEL_API_KEY=mock-api-key MODEL=mock-model
export GITHUB_API_URL=http://127.0.0.1:8082 GITHUB_TOKEN=dev-github-token
export A2A_BEARER_TOKENS=dev-token PUBLIC_URL=http://127.0.0.1:8080/
```

OpenCode (which the coder runs) reaches the mock model through the same
`MODEL_BASE_URL`: its `@ai-sdk/openai-compatible` provider speaks the same
chat-completions wire format the mock serves.

### `mock-openai` scenarios

Selected by the request header `X-Mock-Scenario: <name>`, or by putting
`[mock:<name>]` anywhere in the request body (for example in the prompt). The
mappings are in `dev/wiremock/mock-openai/`.

| Scenario | Answer |
|---|---|
| (none) | a text answer; with `"stream": true` an SSE stream: role chunk, text chunks, finish chunk, a usage chunk (empty `choices`), `data: [DONE]` |
| `tool-call` | a call of the **first tool the request declares** with arguments `{}` (`finish_reason: tool_calls`), streamed or not. Once the history holds a `tool` message the mock answers in text instead, so an agent loop ends |
| `rate-limit` | `429` with `Retry-After: 2` |
| `server-error` | `500` |
| `unauthorized` | `401` `invalid_api_key` |
| `context-length` | `400` `context_length_exceeded` |

The error scenarios apply to streaming and non-streaming requests alike and
persist as long as the header or keyword is sent.

### `mock-github` scenarios

The mock answers `GET /repos/{owner}/{repo}/pulls` (no open pull requests) and
`POST /repos/{owner}/{repo}/pulls` (`201`, with number, `html_url` and `head.ref`
derived from the request). Same switches: header `X-Mock-Scenario`, or
`[mock:<name>]` in the request body of a `POST` (the pull request title or
body); `GET` requests can only use the header.

| Switch | Answer |
|---|---|
| `Authorization: Bearer bad-token`, or scenario `unauthorized` | `401` "Bad credentials" |
| `rate-limit` | `403` with `x-ratelimit-remaining: 0` |
| `server-error` | `500` |
| `already-exists` (on the `POST`) | `422` "A pull request already exists", and the **next** `GET` returns a pull request (#42) once, then the mock is back to normal: a lost race with another opener |
| a `head` query containing `already-open` | the list returns pull request #7, so opening is idempotent |

### `git-server`

```sh
git clone http://127.0.0.1:8083/local/sandbox.git      # README.md, check.sh, justfile
```

Inside the compose network the same repository is
`http://git-server:8080/local/sandbox.git`, which is what a coder task should
name. It has no authentication (any credentials are accepted) and its
repositories live in the `git-data` volume. The layout is
`/<owner>/<repo>.git`, the shape `adam-workspace` and the mock GitHub expect.

### Running the coder against the mocks

```sh
docker compose --profile app up -d --build --wait
curl -N http://127.0.0.1:8080/ \
  -H 'Authorization: Bearer dev-token' -H 'Content-Type: application/json' \
  -d '{"jsonrpc":"2.0","id":"1","method":"SendStreamingMessage","params":{"message":{
        "messageId":"m1","role":"ROLE_USER","parts":[{"text":
        "In http://git-server:8080/local/sandbox.git (base branch main), add hello.txt containing hello."}]}}}'
```

This exercises the A2A endpoint, authentication, the store and the agent loop
against the mock model. The mock model is canned: it answers in text, or calls
the first declared tool with `{}`, so it cannot drive OpenCode through a real
change and the run does not end in a pull request. A complete run needs a model
that can call tools (see the live smoke test in `crates/adam-coder/README.md`);
the git remote and the pull request API of that run can still be `git-server`
and `mock-github`. (The `app` profile was validated with `docker compose config`
only when this was written: no container runtime was available.)

## Errors

Every library error enum implements `adam_error::Classify`: a variant says
**what happened**, its `ErrorClass` says **what to do**. Retry loops, the A2A
error a client sees and the process exit code all decide from the class, never
from a variant, so a new variant only needs a class decision.

| Class | Meaning | Retry | Alert | A2A error (`adam-a2a`) | Exit code (`adam-coder`) |
|---|---|---|---|---|---|
| `Transient` | may succeed later: network, 5xx, timeout, pool, crashed child | yes, with backoff | no | `-32603` "backend temporarily unavailable" | 69 |
| `RateLimited` | slow down; honour `retry_after()` | yes, after `max(backoff, retry_after)` | no | `-32603` "backend temporarily unavailable" | 69 |
| `Conflict` | lost an optimistic-concurrency race | yes, at once (bounded) | no | `-32603` "backend temporarily unavailable" | 69 |
| `Invalid` | the input is wrong; it never succeeds | no | no | `-32602` invalid params | 78 |
| `NotFound` | absent, or invisible to this caller | no | no | `-32001` task not found | 1 |
| `Rejected` | valid, but the target's state forbids it (finished, busy, exists) | no | no | `-32602` invalid params (`-32002` for a cancel) | 1 |
| `Unauthenticated` | credentials missing or refused | no | no | `-32603` "internal error" | 1 |
| `Unsupported` | the peer does not offer this | no | no | `-32603` "internal error" | 1 |
| `Corrupt` | stored or received data breaks an invariant | no | **yes** | `-32603` "internal error" | 70 |
| `Internal` | a bug, or unclassified | no | **yes** | `-32603` "internal error" | 70 |

The A2A server answers every JSON-RPC error with HTTP 200 and an error object,
so there is no HTTP status to map; a body that is not a request gets `-32700`
(not JSON) or `-32600` (JSON, but not a request), with a null id.

**Retry.** The runtime retries a step that fails with a `Transient` or
`RateLimited` `AgentError` with exponential backoff (`RetryPolicy`), waiting
at least the error's `retry_after()` when it has one (a provider's
`Retry-After`, capped at 24 hours). Any other class fails the run. A store
error while stepping is decided by class: `Corrupt` and `Invalid` fail the run
(a row that can never be read must not be re-leased for ever); everything else
leaves the run to its lease and is logged with its class.

**Exit codes** of `adam-coder` (sysexits.h values, *unverified*: from memory)
are found by walking the `anyhow` chain from the outside in: 78 configuration
(`ConfigError`, an invalid `OpenAiConfigError`, or any other `Invalid` error; a
`Client` build failure is `Internal`, so 70), 69 a dependency
that is unreachable at boot (Postgres), 71 an OS error (a port that cannot
bind), 70 a half of the process that stopped, a panic or an internal error,
1 anything else, 0 after a clean SIGTERM. The failure is one structured JSON
log line, `adam-coder failed`, with the whole cause chain and none of the
process's secrets.

**Rules for an error enum** (details in `crates/adam-error/README.md`):
`thiserror` in libraries and `anyhow` only in binaries, `#[non_exhaustive]`,
an `impl Classify` with an exhaustive `match` in its test, a `#[source]` for
every wrapped error (`BoxError` for a driver or SDK type, so none appears in a
trait signature), and a message that describes its own layer only. Nothing
interpolates its source: `adam_error::report(&e)` prints the chain
(`a: b: c`) once, and is used only where an error is flattened, at a trust or
persistence boundary: the journal, a response to a client, a log line.

## Testing

```sh
docker compose up -d --wait
export ADAM_TEST_POSTGRES_URL=postgres://postgres:postgres@localhost:5432/adam_test
export ADAM_TEST_MONGODB_URI=mongodb://localhost:27017
export ADAM_TEST_MOCK_OPENAI_URL=http://localhost:8081
export ADAM_TEST_MOCK_GITHUB_URL=http://localhost:8082
cargo test --workspace
```

Each database suite is skipped when its variable is unset, so `cargo test`
works with no databases (only the in-memory store runs). The same holds for
the two tests that check the real clients against the mocks of `compose.yaml`
(`adam-model-openai/tests/wiremock_compose.rs` and
`adam-workspace/tests/wiremock_compose.rs`). The suites isolate
cases by agent name, so they run in parallel on one shared database with no
cleanup between runs.

CI must not pass by skipping: set `ADAM_TEST_REQUIRE_DB=1` and a suite whose
variable is unset **fails** instead of skipping (CI sets it in every job that
provides the databases). Gate new database tests with
`adam_core::testing::test_env("ADAM_TEST_...")`, which honours the flag.
CI runs the tests with [cargo-nextest](https://nexte.st) (`cargo nextest run
--workspace`, profile `ci` in `.config/nextest.toml`) plus `cargo test --doc`,
and gates line coverage (`cargo llvm-cov nextest --workspace`).

The suite (22 cases) covers: exact JSON roundtrip (unicode, i64 bounds,
floats, special keys), CAS conflicts, 16-way concurrent commits with a single
winner, journal ordering, first-writer-wins and 16-way races,
non-determinism detection, due rules, agent filtering and limits, 8 workers
claiming 60 runs with no double lease, lease expiry and takeover, renew and
release, one-open-run-per-conversation including a 16-way race, and purging.

To add a backend (SQLite, Redis, FoundationDB, ...), implement `Store` and add
one line: `adam_store_testkit::store_conformance!(make_store);`.

## Development

Every crate has a `README.md` next to its `Cargo.toml` (what it is for, its
public API at a glance, features and environment variables, how it is tested),
and `readme = "README.md"` in its manifest. Update the README in the same
change as any change to the crate's public API, environment variables or
tests. CI (the `lint` job) fails when a `crates/*/Cargo.toml` has no sibling
`README.md`; it cannot check that the README is still accurate, so review
does. The crate table above links each README.

## Roadmap

1. ~~Store trait and adapters~~ (this repo)
2. ~~Run state machine and `ctx.step` journaling~~ (`adam-runtime`)
3. `#[tool]` macro (schemars)
4. `build.rs` discovery of `agent/` (instructions, tools, skills, subagents)
5. Parking, approvals, schedules
6. Dev TUI (`cargo adam dev`)
7. Host adapters (axum/tower), channels, sandboxes

## License

Licensed under either of [Apache License, Version 2.0](LICENSE-APACHE) or
[MIT license](LICENSE-MIT) at your option. Vendored agent skills keep their own
licenses; see [third-party-notices.md](third-party-notices.md).

Unless you explicitly state otherwise, any contribution intentionally submitted
for inclusion in this work by you, as defined in the Apache-2.0 license, shall
be dual licensed as above, without any additional terms or conditions.
