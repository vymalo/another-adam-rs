# adam-rs

A Rust framework for durable AI agents, in the spirit of [eve](https://eve.dev):
filesystem-first authoring, macros where eve uses file conventions, and every
piece of infrastructure behind a trait so each developer picks their own.

**Docs:** [`docs/`](docs/README.md), starting with the
[architecture](docs/architecture.md) (crate map, ports, the path of a task, the
run lifecycle, the error tree, the coder agent), with Mermaid diagrams.

This repository currently contains the **durable-state layer**: the `Store`
trait, a shared conformance suite, and two production adapters.

Libraries live in `crates/`, binaries (the agents you can run) in `bin/`; the
table below links each README, wherever it is.

| Crate | What it is |
|---|---|
| [`adam-error`](crates/adam-error/README.md) | The error model every crate shares: `ErrorClass`, the `Classify` trait, `BoxError` and `report()`. Pure: no I/O, no async |
| [`adam-host`](crates/adam-host/README.md) | The contract with a host app: the closed process `Role` (`all`, `control-plane`, `worker`) and a small role-aware supervisor that starts the matching components, stops them in order (control plane, then workers) and names the one that failed. Std-only `Role`; the supervisor is feature `supervisor` (tokio) |
| [`adam-core`](crates/adam-core/README.md) | `Store` trait, run/journal/lease types, in-memory reference store |
| [`adam-store-testkit`](crates/adam-store-testkit/README.md) | Conformance suite every store must pass (`store_conformance!`) |
| [`adam-store-postgres`](crates/adam-store-postgres/README.md) | PostgreSQL 12+ via `sqlx` 0.9 |
| [`adam-store-mongodb`](crates/adam-store-mongodb/README.md) | MongoDB 5.0+ via the official driver; standalone `mongod` is enough. No cross-process signals: workers poll |
| [`adam-model`](crates/adam-model/README.md) | `ModelClient` trait (`complete` + streaming, tool calling), request/response types, `MockModel` test double |
| [`adam-model-openai`](crates/adam-model-openai/README.md) | `OpenAiCompatible`: any OpenAI-compatible chat-completions endpoint (gateway or provider) via `reqwest` + rustls |
| [`adam-workspace`](crates/adam-workspace/README.md) | Per-run git worktrees over a shared mirror, commit and push, and pull requests (`CodeHost`, GitHub). The token is passed per `git` invocation and never stored |
| [`adam-runtime`](crates/adam-runtime/README.md) | Durable agent loop: `Agent` trait, run state machine, `ctx.step` journaling, workers, retries, event sinks |
| [`adam-notify-postgres`](crates/adam-notify-postgres/README.md) | Cross-process run events (`PgEventSink`) and wake-up/cancel signals (`PgNotifier`) over PostgreSQL `LISTEN`/`NOTIFY`: a latency optimisation next to polling, never the truth |
| [`adam-notify-testkit`](crates/adam-notify-testkit/README.md) | Conformance suite every `Notifier` (and its event transport) must pass (`notifier_conformance!`) |
| [`adam-a2a`](crates/adam-a2a/README.md) | Expose an agent as an A2A 1.0 server (axum): `TaskBackend` seam, bearer auth (fail closed), `InMemoryBackend` under feature `test-util` |
| [`adam-devcontainer`](crates/adam-devcontainer/README.md) | A run's commands in its repository's devcontainer, on a rootless Podman service, through the official devcontainer CLI: the container-backed `Environment` of `adam-workspace` ([ADR 0010](docs/decisions/0010-a-run-works-in-its-repositorys-devcontainer.md)); the stack is `dev/compose.devcontainer.yaml` |
| [`adam-env-kubernetes`](crates/adam-env-kubernetes/README.md) | A run's commands in a Kubernetes pod of its own, from a pod template the chart mounts, through `pods/exec` and the client `adam-kube-exec`: the cluster-backed `Environment` of `adam-workspace` ([ADR 0019](docs/decisions/0019-a-runs-processes-in-a-pod-of-their-own.md)); the chart's `runPods` block makes the objects it needs |
| [`adam-acp`](crates/adam-acp/README.md) | ACP client that drives a coding agent (`opencode acp`) over stdio; ships a scripted fake agent for tests |
| [`adam-llm-agent`](crates/adam-llm-agent/README.md) | `LlmAgent`: the durable model/tool-calling loop (`Tool` trait and typed tool helpers, `NeedsInput` parking, limits, history truncation) on top of `adam-runtime` |
| [`adam-macros`](crates/adam-macros/README.md) | The `#[tool]` attribute macro: an `async fn` becomes a `Tool` (name, description and argument schema from the function and its doc comments; `State<T>` and `&ToolCtx` parameters). A proc-macro crate over `syn`; the expansion is a pure, unit-tested function |
| [`adam-agent-fs`](crates/adam-agent-fs/README.md) | Parses and validates agent directories (`agent/instructions.md`, skills in the Agent Skills format, subagents in the Claude Code / Copilot format, `mcp.json`, schedules) into an `AgentManifest`, with file-and-line diagnostics; feature `build` embeds a directory in the binary from `build.rs` (`build("agent").emit()`). A leaf: YAML through `serde-saphyr`, no async, no runtime dependency |
| [`adam-assembly`](crates/adam-assembly/README.md) | Binds an agent manifest to `LlmAgent`s (`AgentDef::from_manifest(AGENT)?.bind(tools![..])?.state(env).model(model, alias)?`): `{{var}}` templating of the prompt, `tools:` checked against the `ToolSet` with "did you mean" errors, model aliases, one `LlmAgent` for the root and one per local subagent definition, a tool per remote (A2A) subagent with bearer auth from the environment, and the root's A2A card (feature `a2a`). The same code path for the embedded manifest and one read from a directory; feature `dev` reloads that directory while the process runs (`LiveAssembly`, `notify`), keeping the last good version on an invalid edit; feature `mcp` gives each agent the tools of its own `mcp.json` (`AgentDef::connect_mcp`, `<server>__<tool>`, checked at `bind`) |
| [`adam-mcp`](crates/adam-mcp/README.md) | MCP client over `rmcp` 3.5 for the servers of an `mcp.json` (streamable HTTP and stdio; no SSE): `McpServers::connect(&config, &Env, &McpPolicy)` gives tools named `<server>__<tool>`; `${VAR}` expansion, a tool allow-list, secrets redacted from every message and log, one reconnect per broken session, stdio only when the deployment opts in, fail closed at startup |
| [`adam-mcp-testkit`](crates/adam-mcp-testkit/README.md) | Test kit, not published: a scriptable MCP server over stdio (`adam-mcp-test-server`) and streamable HTTP (`TestHttpServer`), and the stdio tests of `adam-mcp` |
| [`adam`](crates/adam/README.md) | The facade for writing an agent: `use adam::prelude::*` gives `#[tool]`, `tools!`, `Tool`, `State`, `LlmAgent`, ... (feature `macros`, on by default), `adam::include_agent!()` for the agent directory embedded by `build.rs`, `AgentDef` to bind it (feature `a2a` for the card, feature `dev` for dev reload), and re-exports the model, runtime, core, error, agent-fs and assembly crates |
| [`adam-agent-fixture`](crates/adam-agent-fixture/README.md) | Test fixture, not published: a crate whose `build.rs` embeds an agent directory and whose tests compare the embedded manifest with the directory |
| [`adam-a2a-runtime`](crates/adam-a2a-runtime/README.md) | `RuntimeTaskBackend`: the A2A `TaskBackend` over `adam-runtime` (task = run, ownership per caller, `input-required` from parked runs); subscriptions are rebuilt from the store, so they survive restarts. Reusable by any agent |
| [`adam-ui`](crates/adam-ui/README.md) | The screen's UI catalog as model tools: `ask_user` with `choices` (one Choices form, answers back as the result), `show` (blocks of the screen's components, validated against the catalog's JSON Schema), `ui_catalog`, and `ThreadTools`, a `ToolSource` that offers every tool of the conversation's MCP endpoint each model turn; the catalog is checked against its digest and refetched over thread tools when stale; `card_extensions()` announces it. Every adam agent can use it |
| [`adam-service`](crates/adam-service/README.md) | The A2A service every agent binary shares: `serve(&ServiceConfig, Agents, shutdown)` connects Postgres, builds the runtime, the A2A router and the `LISTEN`/`NOTIFY` signals and runs the components of a `ROLE`; the configuration the binaries have in common (`ServiceConfig`, `ModelConfig`, `McpSettings`) and the exit codes. A binary hands it the agent's name, card and a registration closure |
| [`adam-agent`](bin/adam-agent/README.md) | One binary that serves **any agent folder** over A2A: `ADAM_AGENT_DIR` names a folder of files (instructions, card, skills, subagents, `mcp.json` tools), read at startup; `ask_user` is its only tool of its own. No embedded agent, one agent per process, any number of services over one database. Library and the `adam-agent` binary (`ROLE`), over `adam-service`; ships inside the coder image |
| [`adam-coder`](bin/adam-coder/README.md) | The coder agent: a coding task to a verified pull request over A2A (worktree, OpenCode over ACP, bounded check cycles, commit, push, PR). Library and the `adam-coder` binary, which runs the A2A server, the workers or both (`ROLE`); image in `docker/coder`, chart in `deploy/coder` |

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
A worker never claims a run it is stepping itself, even once the lease on it has
lapsed (`claim_due` takes those runs as `busy`): a second lease on the run would
come with a snapshot that its own step is about to make stale.
A claim can also be **pinned**: a run then has an *owner*, the worker that first
claimed it, and only that worker claims it again (`ClaimScope::Pinned`, used by the
`affinity` and `isolated` workspace placements; see
[ADR 0002](docs/decisions/0002-workspace-placement.md)).

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
| Busy runs (`claim_due(.., busy, ..)`) never claimed | `AND id <> ALL($busy)` in the claiming statement | `_id: { $nin: busy }` in the candidate filter |
| Pinned claiming (`owner`, schema version 2) | `AND (owner IS NULL OR owner = $w)` and `owner = COALESCE(owner, $w)` in the claiming `UPDATE` | the same condition in both the candidate and the `updateMany` filter (a missing field is `null`), `$set` of `owner` in the `updateMany` |
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
docker compose up -d --wait        # postgres, mongodb, mock-openai, mock-github, mock-github-mcp, git-server
docker compose --profile app up -d --build --wait   # ... plus the coder and the general agent, wired to the mocks
docker compose down -v             # stop and forget all state (volumes included)
```

| Service | Host address | What it is |
|---|---|---|
| `postgres` | `127.0.0.1:5432` | PostgreSQL 16, database `adam_test`, user and password `postgres` |
| `mongodb` | `127.0.0.1:27017` | MongoDB 7, standalone |
| `mock-openai` | `http://127.0.0.1:8081/v1` | WireMock: OpenAI-compatible chat completions (`/v1/chat/completions` and `/chat/completions`, plus `/v1/models`); the models `mock-coder`, `mock-opencode`, `mock-assistant` and `mock-researcher` are scripted (see "Scripted models") |
| `mock-github` | `http://127.0.0.1:8082` | WireMock: the GitHub REST subset `adam-workspace` uses (list and open pull requests, create a repository, the owner's kind, the login) and the GitHub App token trade (`POST /app/installations/{id}/access_tokens`: a JWT for an installation token that lasts four minutes) |
| `mock-github-mcp` | `http://127.0.0.1:8085/mcp` | WireMock: the GitHub MCP server's streamable HTTP endpoint, as the coder reads GitHub through it: behind a bearer (`401` without), `initialize`, `tools/list` (the twelve read tools of the coder's allow-list) and `tools/call` of `get_me` and `list_branches`; any other tool is an error result "not scripted". The coder's `mcp.json` in this stack is `dev/coder-agent/mcp.json`, mounted over the folder's (production starts the real `github-mcp-server` as a child process) |
| `git-server` | `http://127.0.0.1:8083/local/sandbox.git` | bare repositories over smart HTTP (nginx + git-http-backend), seeded with `local/sandbox.git` and creating an empty repository on first use for the owners of `AUTO_CREATE_OWNERS` (`scratch` in the compose file); no authentication |
| `coder` (profile `app`) | `http://127.0.0.1:8080/` | the coder agent built from `docker/coder/Dockerfile`, bearer token `dev-token`; its agent files are the folder `bin/adam-coder/agent` mounted read-only at `/etc/adam/agent` (`ADAM_AGENT_DIR`, see "Changing what the coder says") |
| `github-mcp` (profile `app`) | none (the coder's network, `127.0.0.1:8082`) | the real GitHub MCP server (`github-mcp-server http --read-only`, the coder's image) as the sidecar of the `coder` service, holding no credential; idle in this stack, whose coder reads `mock-github-mcp` |
| `agent` (profile `app`) | `http://127.0.0.1:8084/` | the general agent: `adam-agent` from the **coder's image** (`entrypoint: ["tini", "--", "adam-agent"]`, so there is no second image), bearer token `dev-token`, serving the folder `dev/agents/assistant/agent` mounted read-only at `/etc/adam/agent` (`ADAM_AGENT_DIR`); model `mock-assistant`; shares the coder's database (runs are scoped by the agent's name). See "A general agent from a folder" |

Host ports can be moved with `POSTGRES_PORT`, `MONGODB_PORT`, `MOCK_OPENAI_PORT`,
`MOCK_GITHUB_PORT`, `MOCK_GITHUB_MCP_PORT`, `GIT_SERVER_PORT`, `CODER_PORT` and `AGENT_PORT` (for example in a `.env`
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
# optional: ADAM_AGENT_DIR=bin/adam-coder/agent reads the prompt and the card from that folder at startup
# instead of the copy embedded in the binary (see the crate README, "Where the prompt and the card live").
# optional: ROLE=control-plane or ROLE=worker instead of the default `all`. Run one of each
# over the same DATABASE_URL (different LISTEN_ADDR) to split the halves; a control plane ignores
# the model, GitHub and workspace variables. See the crate README.
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

The scenarios above apply to every model except `mock-coder`, `mock-opencode` and `mock-assistant`,
which follow their scripts (see "Scripted models" below). The error scenarios
apply to streaming and non-streaming requests alike and persist as long as the
header or keyword is sent.

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

`dev/wiremock/mock-github/mappings/repos.json` is what the coder's `create_repository` needs
([ADR 0009](docs/decisions/0009-github-per-installation-read-through-mcp.md), decision 9): `GET /users/{owner}` says
`local` and `scratch` are organisations and any other owner a user, `GET /user` is `dev-user` and, for an
installation token (`Bearer ghs_...`), `403 Resource not accessible by integration` (what GitHub says: a GitHub App
has no user), and `POST /orgs/{owner}/repos` and `POST /user/repos` answer `201` with the repository as GitHub
describes a new empty one, whose `clone_url` is `http://git-server:8080/<owner>/<name>.git` (git-server makes an
empty repository for the owners of `AUTO_CREATE_OWNERS` on first use). The scenario `already-exists` (the keyword
`[mock:already-exists]` in the body, or the header) answers `422 name already exists on this account`, and
`Bearer bad-token` is `401`. The stack's coder has `CREATE_REPO_OWNERS: scratch`.

### `mock-github-mcp`

The coder reads GitHub through the official GitHub MCP server, read-only ([ADR
0009](docs/decisions/0009-github-per-installation-read-through-mcp.md) and [ADR
0017](docs/decisions/0017-a-github-app-works-on-every-account-it-is-installed-on.md); "GitHub over MCP" in
[`bin/adam-coder`](bin/adam-coder/README.md#github-over-mcp-read-only)). Production runs the real binary in `http`
mode as a sidecar of the coder's pod (the `github-mcp` service of this stack is the same, in the coder's network, and
idle: nothing here has a GitHub for it); the stack has no GitHub to talk to, so the mock stands in for it over the
streamable HTTP transport and the coder's `mcp.json` is `dev/coder-agent/mcp.json`, mounted over the folder's own
(the folder in `CODER_AGENT_DIR` must therefore have an `mcp.json`: a copy of `bin/adam-coder/agent` does), with
`GITHUB_MCP_URL=http://mock-github-mcp:8080` so that the coder binds its credentials to that origin. The coder is
given `MCP_ALLOW_INSECURE=true` for it (plain `http` to another container; development only).

Every `POST /mcp` needs `Authorization: Bearer <anything>` or gets `401`. The dev file holds none: the coder sends the
credentials of each call (its token, or the installation token of its App) and a placeholder
(`ghs_adam_listing_only`) to list the tools at startup. The mock answers JSON (no session id, no standalone stream, `GET` and `DELETE` are `405`), which is
what `rmcp`, the client of `adam-mcp`, accepts (*verified* by `cargo test -p adam-mcp --test wiremock_compose`,
which CI runs against this service). Its scripted answers: `get_me` is `{"login":"dev-user"}`, `list_branches`
is `[{"name":"main"}]`; the other ten tools are listed with their schemas and answer an error result.
`dev/coder-e2e.sh` reads the mock's journal, see below.

### `git-server`

```sh
git clone http://127.0.0.1:8083/local/sandbox.git      # README.md, check.sh, justfile
```

Inside the compose network the same repository is
`http://git-server:8080/local/sandbox.git`, which is what a coder task should
name. It has no authentication (any credentials are accepted) and its
repositories live in the `git-data` volume. The layout is
`/<owner>/<repo>.git`, the shape `adam-workspace` and the mock GitHub expect.

It is seeded with every directory `dev/git-server/seed/<owner>/<name>/` (today `local/sandbox`; `local/library`, a second repository with a `greeting.txt`, for the `second-repo` scenario below; `local/devbox`, whose own devcontainer has the tool `devbox-tool`, and `local/devbox-broken`, whose devcontainer file asks for what is not allowed, for the work-environment scenarios below), each
once. And it behaves like a place where repositories can be **created**: a request for
`/<owner>/<name>.git` of an owner in `AUTO_CREATE_OWNERS` (a comma or space list; `scratch` in
`compose.yaml`, empty means none) makes the bare repository first, empty, on branch `main`, with pushes
enabled (`dev/git-server/cgi.sh`, in front of `git-http-backend`), so it is what a repository that was
just created on GitHub is: it exists, and it has no ref. Any other missing repository is a 404.
`GET /__repos/` (and `/__repos/<owner>/`) lists what is there as JSON, read-only. The `scratch`
scenario below publishes a project to `http://git-server:8080/scratch/fib-<id>.git`.

### Running the coder against the mocks

```sh
docker compose --profile app up -d --build --wait
curl -N http://127.0.0.1:8080/ \
  -H 'Authorization: Bearer dev-token' -H 'Content-Type: application/json' \
  -d '{"jsonrpc":"2.0","id":"1","method":"SendStreamingMessage","params":{"message":{
        "messageId":"m1","role":"ROLE_USER","parts":[{"text":
        "In http://git-server:8080/local/sandbox.git (base branch main), add hello.txt containing hello."}]}}}'
```

This exercises the A2A endpoint, authentication, the store, the agent loop,
OpenCode and the pull request path, and **the run ends in a pull request**: the
coder's models are scripted (see "Scripted models" below), so the same task
always takes the same steps. `dev/coder-e2e.sh` runs that task and checks the
result; CI runs it on the image it has just built (`.github/workflows/coder.yml`,
step "Compose e2e"):

```sh
docker compose --profile app up -d --build --wait postgres mock-openai mock-github mock-github-mcp git-server coder
sh dev/coder-e2e.sh                 # OpenCode makes the change
NO_OPENCODE=1 sh dev/coder-e2e.sh   # the check command makes it, OpenCode is not started
SCENARIO=files sh dev/coder-e2e.sh  # the coder reads and writes the file itself (read_file, write_file)
SCENARIO=scratch sh dev/coder-e2e.sh  # no repository is named: a scratch project, published to one named later
SCENARIO=second-repo sh dev/coder-e2e.sh              # another repository joins the workspace, the person says yes
SCENARIO=second-repo ANSWER=no sh dev/coder-e2e.sh    # ... and with a no it does not
SCENARIO=create-repo sh dev/coder-e2e.sh              # a repository is created for the scratch project, the person says yes
SCENARIO=create-repo ANSWER=no sh dev/coder-e2e.sh    # ... and with a no it is not
```

The script sends the task with `SendStreamingMessage`, waits for
`TASK_STATE_COMPLETED`, and checks that the `checks` (the last one passed, bound to the pushed commit), `branch` and `pull_request`
artifacts are there, that `mock-github` saw exactly one
`POST /repos/local/sandbox/pulls` (head = the branch, base = `main`), and that
`git-server` has the branch with `hello.txt` containing `hello`. It also checks the GitHub MCP side: the journal of
`mock-github-mcp` (not reset: the coder connects the server when it starts, before the script) holds `initialize` and
`tools/list`, and the default scenario (the one with OpenCode, whose script reads `github__list_branches` right after
`prepare_workspace`) added exactly one `tools/call` of `list_branches`, and the model was given
its answer (the `mock-openai` journal has a request whose history holds the tool message that names `main`); every
`tools/call` carries the coder's own credentials (the dummy token, or the installation token) and every `tools/list` the
startup placeholder, since the dev file holds no credential; every other scenario adds no call. `TIMEOUT`, `CODER_URL`, `CODER_TOKEN`, `MOCK_GITHUB_URL`, `MOCK_GITHUB_MCP_URL`,
`MOCK_OPENAI_URL` and `GIT_SERVER_URL` override the defaults (see the script's header).

`SCENARIO=scratch` is two messages. The task names no repository ("Write a fib.sh that prints the first 7
Fibonacci numbers. I'll give you the repo later."), so the coder builds `fib.sh` and its check in a scratch
slot and asks which repository to publish it to: the script checks that the task waits
(`TASK_STATE_INPUT_REQUIRED`) with that question, that no pull request was opened and that `git-server`
has not been asked for the repository (its `/__repos/scratch/` listing). It then sends the answer to the
task, `Publish it to http://git-server:8080/scratch/fib-<id>.git`, an empty repository that git-server makes
on first use, and checks everything above for that repository (the pull request, the branch, the artifacts),
plus that `main` is the one empty commit the coder gave it, that `fib.sh` is on the branch, and that the
**tree** the checks ran on in the scratch project is the tree of the pushed commit: what was checked is what
was pushed. `<id>` is new on every run, so a rerun on one stack never meets the last one's repository.

`SCENARIO=second-repo` is two messages as well, and is about consent. The task names `local/sandbox` and asks for
"our shared greeting", which lives in `local/library`. The coder prepares the sandbox, then calls
`request_repository` for the library with a reason, and the task waits (`TASK_STATE_INPUT_REQUIRED`) on a question
the tool wrote: `May I add the repository local/library ...`, with the reason quoted and the options `Yes, add
local/library` and `No` (the script sends no screen, so they are in the text). The script checks that, that no pull
request exists and that git-server has **not** been asked for `local/library` since the script began (the
repository is seeded, so only git-server's access log tells: `GIT_SERVER_LOGS` is the command that prints it, by
default `docker compose logs --no-color git-server`; the check is skipped, and says so, where that is not
readable). Then it sends the answer, `ANSWER` (`yes` by default). With `yes` the task completes: the pull request
is for the sandbox, `hello.txt` on its branch holds the library's `hello from library`, and git-server was asked
for `local/library` after the answer. With `no` the task waits again (the coder says it could not add the
library), git-server was never asked for `local/library` and no pull request was opened.

`SCENARIO=create-repo` is three messages. Like `scratch`, the task names no repository: the coder builds `fib.sh` in a
scratch project and says it can create a repository for it, and the task waits. The second message ("Create
scratch/fib-<id> and put it there") makes it call `create_repository`: the task waits again, on a question the tool
wrote (`May I create the repository scratch/fib-<id> on git-server? It will be private and empty.`), and the
script checks that `mock-github` has seen no `POST /orgs/scratch/repos` and git-server does not know the repository.
The third message is the answer, `ANSWER` (`yes` by default). With `yes` mock-github saw **exactly one** `POST
/orgs/scratch/repos`, for that name, `private: true` and `auto_init: false`, and the task completes as `scratch`
does: the project is published to the new repository (empty, until then), `main` is the one empty commit and the pull
request is for it. With `no` the creation is never made, git-server never hears of the repository, no pull
request is opened and the task waits. (`CREATE_REPO_OWNERS=scratch` is in `compose.yaml`; the App-mode run creates in
the organisation too, since an installation token has no user.)

#### The work environment: the repository's devcontainer (slice 7b)

`SCENARIO=devcontainer`, `default-env`, `broken-env` and `no-runtime` run against the stack **with a rootless Podman service beside the
coder** ([ADR 0010](docs/decisions/0010-a-run-works-in-its-repositorys-devcontainer.md); `dev/podman/README.md` says what the service
is given and why: no `privileged`, no `cap_add`, no `devices`, three `security_opt`):

```sh
# Ubuntu 24.04 hosts (CI runners, desktops) stop unprivileged user namespaces under AppArmor, which a rootless Podman needs:
sudo sysctl -w kernel.apparmor_restrict_unprivileged_userns=0      # only where the key exists
docker compose -f compose.yaml -f dev/compose.devcontainer.yaml --profile app up -d --build --wait
SCENARIO=devcontainer sh dev/coder-e2e.sh    # the repository's own devcontainer (local/devbox: devbox-tool is only there)
SCENARIO=default-env  sh dev/coder-e2e.sh    # a repository without one: the default image, not the coder's own
SCENARIO=broken-env   sh dev/coder-e2e.sh    # local/devbox-broken: refused, the person decides, nothing in the file ran
SCENARIO=no-runtime   sh dev/coder-e2e.sh    # the service stopped: the coder's own container, with a step that says so
```

The first image pull and build take minutes (the 367 MiB `devcontainers/base` image of the default environment, and `devbox`'s own build on
it); where the containers have no direct internet, `PRELOAD_FROM_DOCKER=1` loads the default image into the service from the host's
Docker (*unverified*). `COMPOSE_CMD` says how the script reaches `docker compose` for the stack. Each scenario asserts durable things (what the
model's journal says a tool returned, the `checks` artifact's `environment`, the pushed `tool.txt`, what Podman lists with the run's label) and the
steps of the environment as lines of the stream: `devcontainer` also that `env` inside has no `GITHUB_TOKEN`, `DATABASE_URL` or
`A2A_BEARER_TOKENS`, that Podman lists a container while the run lasts and none within a minute of its end (the janitor, `WORKSPACE_SWEEP_SECS=10`),
and that the Podman service has no `privileged`, `cap_add` or `devices` and no service mounts a Docker socket; `broken-env` that
`/work/INIT-RAN` (the fixture's `initializeCommand`) does not exist in the coder. The coder image in the stack needs the devcontainer
CLI and `podman-remote` (`docker/coder/Dockerfile`).

#### The coder as a GitHub App installation

An installation is a token or a GitHub App, never both
([ADR 0009](docs/decisions/0009-github-per-installation-read-through-mcp.md)). The compose file runs the
token. The override `dev/compose.github-app.yaml` runs the same coder as an App: an init service makes a
throwaway RSA key into a volume (`openssl genrsa`; no key is committed), `GITHUB_TOKEN` is turned off, and the
coder gets `GITHUB_APP_ID`, `GITHUB_APP_OWNERS` (`local,scratch,other-org`: **no** `GITHUB_APP_INSTALLATION_ID`, so
the App is not pinned) and `GITHUB_APP_PRIVATE_KEY_PATH`. It signs a JWT, finds the installation of each repository's
owner at `mock-github` (`GET /orgs/{owner}/installation`, then `/users/{owner}/installation`: every owner is on
installation 67890 and `other-org` on 67891), trades the JWT for an installation token and gives that to `git`, to the
REST calls and to the GitHub MCP calls. An owner that is not on the list is refused before anything is looked up.
A pinned App (`GITHUB_APP_INSTALLATION_ID`) is covered by the tests of the crates and of the binary.

```sh
docker compose -f compose.yaml -f dev/compose.github-app.yaml --profile app up -d --build --wait \
  postgres mock-openai mock-github mock-github-mcp git-server coder
GITHUB_AUTH=app sh dev/coder-e2e.sh                  # likewise NO_OPENCODE=1, SCENARIO=files, SCENARIO=scratch
```

With `GITHUB_AUTH=app` the script asserts, besides everything above, that `mock-github` saw at least one
`POST /app/installations/67890/access_tokens` (and, with `EXPECT_INSTALLATION_LOOKUP=1`, for the first run after the
coder started, which keeps what it found, at least one installation lookup with a JWT, for an owner on the list) and that **every** call to `/repos/...` (the pull request's
included) carried `Bearer ghs_mockinstallationtoken...` and never the JWT, and that every `tools/call` to
`mock-github-mcp` carried it too (the coder sends the credentials of each MCP call itself, and a placeholder to list
the tools). With the default `token` it asserts that every such call carried the dummy token. WireMock cannot check an RS256 signature, so the mock accepts any
bearer that looks like a JWT; the signature, `iss` and lifetime are checked by
`cargo test -p adam-workspace --test github_app` against a key made for the test. CI runs the four scenarios a
second time this way (`.github/workflows/coder.yml`, step "Compose e2e").

#### Saying hello, and the coder's name

The coder introduces itself (adam-rs#55): it says its name (`Coder`, the `display_name` var of its
instructions and the name on its card), answers "hi" with a short greeting that says what it does in one
sentence and asks which repository and what to change, and answers "what can you do?" in plain words, with the
tool names only when asked for detail. On the mocks the greeting is built from the persona lines of the prompt
(see "Scripted models"); `dev/greeting-e2e.sh` checks the whole chain through the stack (a greeting that ends
`TASK_STATE_INPUT_REQUIRED`, the same task going on to a pull request, and a restart on an edited folder):

```sh
docker compose --profile app up -d --build --wait postgres mock-openai mock-github git-server coder
sh dev/greeting-e2e.sh                   # NO_RESTART=1 skips the step that restarts the coder on an edited copy
```

How a *live* model behaves with these instructions is *unverified*: the mocks prove what the model is sent, not
what it says.

#### Changing what the coder says

The coder reads its agent files (the prompt, the card, the skills) at startup from
`ADAM_AGENT_DIR`; `compose.yaml` mounts `bin/adam-coder/agent` there, read-only.
Edit `bin/adam-coder/agent/instructions.md` (or copy the folder, edit the copy and set
`CODER_AGENT_DIR=<copy>`) and restart the service, with no rebuild:

```sh
docker compose --profile app up -d coder        # the container is recreated and reads the folder again
```

The startup log has one `agent files` line (`source=folder`, the path, the digest, the agent,
the number of warnings); a folder with a mistake stops the container with exit code 78 and every
finding as `path:line: error: ...`. The folder must be readable by uid 10001 (`chmod -R a+rX`).
Remove the variable and the mount and the copy embedded in the image is used. The tests that prove
this are in [`bin/adam-coder/README.md`](bin/adam-coder/README.md#tests).

To run a prebuilt image instead of building one, set `CODER_IMAGE` (default
`adam-rs/coder:dev`) and pass `--no-build`. `CODER_MODEL=mock-model` brings back
the canned answers: text only, no tool call, so no pull request.

#### A general agent from a folder

The service `agent` runs `adam-agent` ([`bin/adam-agent/README.md`](bin/adam-agent/README.md)) from the
same image: the `coder` image carries both binaries, its entrypoint stays `adam-coder`, and the service
overrides it. The agent is a folder of files, `dev/agents/assistant/agent` (a chat persona, no tools of its own
but the screen's: `ask_user`, `show`, `ui_catalog`), read at startup from `ADAM_AGENT_DIR` (`/etc/adam/agent`); `AGENT_FOLDER=<copy>` mounts another one.
Its model `mock-assistant` answers every request in role: the greeting is built from the two persona lines of the
system prompt (see "Scripted models"), so the mocked answer follows the folder. There is no workspace, no
GitHub and no worktree, and the same database holds the coder's runs and the agent's, each under its own name.

```sh
docker compose --profile app up -d --build --wait postgres mock-openai agent
sh dev/agent-e2e.sh        # NO_RESTART=1 skips the step that restarts the agent on an edited copy of the folder
```

`dev/agent-e2e.sh` checks the card of the folder, that "hi" ends `TASK_STATE_COMPLETED` (a chat agent answers, it
does not wait) with the name and the one-sentence summary of the folder, and that a copy of the folder with
another name and summary, mounted in its place, changes the card and the answer after a restart, with no rebuild.
A fourth agent is a folder and about twelve lines of `compose.yaml` (copy the `agent` service, change the
folder, the port and the token). How a folder declares MCP servers for its tools (a researcher on a web-search
server) is in [`bin/adam-agent/README.md`](bin/adam-agent/README.md#mcp-servers-from-the-folder). How a *live*
model behaves with the folder is *unverified*, as for the coder.

#### Asking with choices

The coder (and `adam-agent`) asks several questions at once as one form when the screen can draw it
([`adam-ui`](crates/adam-ui/README.md), [ADR 0006](docs/decisions/0006-a2ui-and-the-vymalo-extensions-in-adam-rs.md)):
the card lists A2UI v0.9.1, `ui-catalog/v1`, `thread-tools/v1`, `mentions/v1` and `steer/v1`, a message from the orchestration layer carries the
screen's catalog (or its digest and a grant for the conversation's MCP endpoint), `ask_user` takes `choices`, and the
person's answers come back as one A2UI action. The task that proves it, on the mocks: three questions as radio lists
(a database, a login, where it runs), the answers, and the coder going on (`dev/coder-choices-e2e.sh`; no repository,
GitHub or OpenCode is needed):

```sh
docker compose --profile app up -d --build --wait postgres mock-openai mock-github git-server coder
sh dev/coder-choices-e2e.sh
```

The script checks the card's extensions; that a task whose text carries `[mock:choices]` and the web's catalog inline
ends `TASK_STATE_INPUT_REQUIRED` with the question as text and an `application/a2ui+json` part (a `createSurface` under
the screen's `catalogId` and one Choices of three questions); that an A2UI action with the answers (`db=pg`,
`auth=keycloak`, `deploy=compose`) is read as the person's answer and the coder's next words quote it; and that a screen
the coder cannot read (a message that only names the catalog) gets the options as text and no A2UI part. The catalog is
a copy of the web's (`crates/adam-ui/tests/fixtures/catalog-v2.json`, with its lock); `CATALOG_FILE` and `CATALOG_LOCK`
point at another. How a *live* model uses `choices` is *unverified*.

#### A researcher that shows its sources

The same service serves any folder, so the researcher is `AGENT_FOLDER=<copy of dev/agents/researcher/agent>`: a persona
that searches the web through the MCP server its `mcp.json` names, answers with the sources as links, and, **when the
screen has the components**, shows them as cards (and, for a question about how things relate, a mermaid graph) with
`show` ([`adam-ui`](crates/adam-ui/README.md)). The folder's `mcp.json` names the web-search mock of the orchestration
layer's stack (`mock-mcp-search`), which this stack does not have, so the scenario serves a copy without it (the
scripted question does not search):

```sh
docker compose --profile app up -d --build --wait postgres mock-openai agent
sh dev/agent-cards-e2e.sh    # restarts `agent` on the researcher folder and `mock-researcher`, and puts the default back
```

The script checks the card's extensions; that a question carrying `[mock:cards]` and **version 3** of the web's catalog
inline ends `TASK_STATE_COMPLETED` with one `ui` artifact (a `createSurface` under the screen's `catalogId`, and a Column
of a Text, a Cards of three cards each with an https link, and a Mermaid `graph TD`) and the three links in the words;
and that the same question from a screen on **version 2** (no `Cards`) or with no catalog gets the words and no
surface. The catalogs are copies of the web's (`crates/adam-ui/tests/fixtures/catalog-v3.json` and `catalog-v2.json`,
each with its lock; `CATALOG_FILE`, `CATALOG_LOCK`, `OLD_CATALOG_FILE` and `OLD_CATALOG_LOCK` point at others).
`NO_RESTART=1` skips the restart, for an agent you started yourself on the researcher folder. How a *live* model uses
`show` is *unverified*.

### Scripted models

The coder's model is `mock-coder` and OpenCode's is `mock-opencode` (`MODEL` and
`OPENCODE_MODEL` in `compose.yaml`, moved with `CODER_MODEL` and
`CODER_OPENCODE_MODEL`); the general agent's is `mock-assistant` (`AGENT_MODEL`, which `dev/agent-cards-e2e.sh` sets to `mock-researcher`). All four are in `mock-openai`, selected by the `model` of the
request, and both are **stateless**: the answer is chosen by which scripted
tool-call ids the request's history already holds, so a retried or replayed
request gets the same answer and the script cannot drift out of step.

| Model | Mapping | Script |
|---|---|---|
| `mock-coder` | `mappings/coder-script.json` | `prepare_workspace` (`http://git-server:8080/local/sandbox.git`, `main`, id `coder-call-1`), `github__list_branches` (`local/sandbox`, read through `mock-github-mcp`, id `coder-gh-1`), `delegate_to_opencode` (create `hello.txt` containing `hello`, `coder-call-2`), `run_checks` (`sh ./check.sh`, `coder-call-3`), `commit_and_push` (`coder-call-4`), `open_pull_request` (`coder-call-5`), then a final text (`stop`). Also as a stream (see below). |
| `mock-coder`, the person's first message is a greeting (`hi`, `hello` or `hey`, then anything) | same file | a text answer (`stop`): `Hi! I'm <name>. <summary>. Which repository should I work on, and what should I change?`, **built from the first two lines of the system prompt** (`messages[0]`: `Your name is <name>.` and `In one sentence: <summary>.`, the persona lines the coder's `agent/instructions.md` opens with), so editing the instructions, or mounting another folder, changes the mocked answer. The run then waits for the person (`input-required`); the answer to it (the synthetic `ask_user` call `stop0000N` is in the history) continues with `prepare_workspace` (`coder-call-1`) and the script above. A greeting needs a system message first: a request with the user message alone is not one. |
| `mock-coder`, task text contains `[mock:no-opencode]` | same file | `prepare_workspace` (`nc-call-1`), `run_checks` with `echo hello > hello.txt && sh ./check.sh` (the check command makes the change, `nc-call-2`), `commit_and_push`, `open_pull_request`, final text. OpenCode is never started: deterministic where OpenCode's own behaviour is not the subject. |
| `mock-coder`, task text contains `[mock:files]` | same file | the coder edits the files itself, ids `fl-call-N`: `prepare_workspace` (`fl-call-1`), `read_file` `README.md` (`fl-call-2`), `write_file` `hello.txt` with `hello` (`fl-call-3`), `run_checks` (`sh ./check.sh`), `commit_and_push`, `open_pull_request`, final text. OpenCode is never started. `dev/coder-e2e.sh` runs it with `SCENARIO=files`. |
| `mock-coder`, task text contains `[mock:second-repo]` | same file | ids `sr-call-N`: `prepare_workspace` (`local/sandbox`), `request_repository` (`http://git-server:8080/local/library.git`, a reason), which parks the run on the tool's question; once the answer is in the history, `prepare_workspace` (`local/library`), and then by what that said. **Added** (its `slot: library` is in the result): `read_file` (`greeting.txt`, `repo: library`), `write_file` (`hello.txt` in the sandbox, `hello from library`), `run_checks`, `commit_and_push` and `open_pull_request` (all `repo: sandbox`), final text. **Refused** (the refusal text is in the result): a final text that says the library could not be added, which parks the run. `dev/coder-e2e.sh` runs it with `SCENARIO=second-repo` and `ANSWER=yes` or `no`, and `wiremock_compose` plays both ways, in both forms. |
| `mock-coder`, task text contains `[mock:create-repo] fib-<hex>` | same file | ids `cr-call-N`: `start_scratch`, two `write_file`, `run_checks`, a **text question** (it can create a repository), which parks the run. Once the person's answer holds `Create scratch/`: `create_repository` (`scratch`, `fib-<hex>`, a description; the name taken from the task text with `regexExtract`), which parks the run on the tool's question; once its answer is in the history, `create_repository` again. Then by what the tool said. **Created** (`(private, empty` is in the result): `publish_scratch` (`http://git-server:8080/scratch/fib-<hex>.git`), `commit_and_push` and `open_pull_request` (`repo: fib-<hex>`), final text. **Declined** (`declined` is in the result): a final text that says the repository was not created, which parks the run. `dev/coder-e2e.sh` runs it with `SCENARIO=create-repo` and `ANSWER=yes` or `no`, and `wiremock_compose` plays both ways, in both forms. |
| `mock-coder`, task text contains `[mock:devcontainer]` | same file | the repository's own devcontainer is the environment (slice 7b), ids `dc-call-N`: `prepare_workspace` (`local/devbox`), `run_command` `devbox-tool --version` (`dc-call-2`; the tool only the devcontainer has), `run_command` `env` (`dc-call-3`; the e2e asserts it shows no secret), `delegate_to_opencode` (`dc-call-4`, with `[mock:oc-devbox]` in the instructions: `mock-opencode` then calls `bash` with `devbox-tool --version > tool.txt`, ids `oc-dc-1`, and says it is done), `run_checks` (`sh ./check.sh`, which passes only where `devbox-tool` is the devcontainer's), `commit_and_push`, `open_pull_request`, final text. `dev/coder-e2e.sh` runs it with `SCENARIO=devcontainer`, on the stack with `dev/compose.devcontainer.yaml`, and `wiremock_compose` plays all four scripts below both ways. |
| `mock-coder`, task text contains `[mock:default-env]` | same file | a repository without a devcontainer, ids `de-call-N`: `prepare_workspace` (`local/sandbox`), `run_command` `test -d /opt/flutter && echo coder-env || echo devcontainer-env` (only the coder's own image has `/opt/flutter`: the output says which environment ran it), `run_checks` (the check command makes the change, as `[mock:no-opencode]`), `commit_and_push`, `open_pull_request`, final text. `SCENARIO=default-env`. |
| `mock-coder`, task text contains `[mock:broken-env]` | same file | `local/devbox-broken`, whose file asks for `privileged`, ids `be-call-N`: `prepare_workspace`, `run_command` `true` (the result is the broken environment's error, which names the file and says to ask the person), then a final **question** (wait for the fix, or go on in the default environment), which parks the run. `SCENARIO=broken-env`. |
| `mock-coder`, task text contains `[mock:no-runtime]` | same file | `local/devbox` with the Podman service stopped, ids `nr-call-N`: `prepare_workspace`, `run_command` `devbox-tool --version` (reported as a missing tool), then a final question, which parks the run. `SCENARIO=no-runtime` stops the service first and starts it again at the end. |
| `mock-coder`, task text contains `[mock:scratch] fib-<hex>` | same file | no repository is named, ids `sc-call-N`: `start_scratch` (`fib`), `write_file` `fib.sh` and `check.sh`, `run_checks` (`repo: fib`, `sh ./check.sh`), then a **text question** (which repository should I publish it to), which parks the run. Once the person's answer holds `Publish it to` (and the stop's `ask_user` call is in the history, which the greeting's second step also reads: that step excludes this switch): `publish_scratch` (`http://git-server:8080/scratch/fib-<hex>.git`, the repository named in the task text with `regexExtract`, so reruns on one stack never collide), `commit_and_push` and `open_pull_request` (both `repo: fib-<hex>`, the slot of the new repository), final text. OpenCode is never started. `dev/coder-e2e.sh` runs it with `SCENARIO=scratch`, and `wiremock_compose` plays it, in both forms, with the answer in the shape the coder gives it. |
| `mock-coder`, task text contains `[mock:choices]` | `mappings/coder-choices.json` | `ask_user` (`choices-call-1`) with the question `Three quick questions before I start` and three `choices`: `db` (`pg`, `sqlite`), `auth` (`keycloak`, `none`), `deploy` (`k8s`, `compose`), priority 1; once its result holds `db: pg` (how the person's answers read to the model) the text `Going with Postgres, Keycloak and Compose.` (`stop`), priority 1; any other answers, `Thanks, I have your answers.`, priority 2. No workspace or repository is touched. `dev/coder-choices-e2e.sh` runs it through the stack. |
| `mock-opencode` | `mappings/opencode-script.json`, `__files/opencode-*.sse` | streamed: a `bash` tool call `oc-call-1` with `echo hello > hello.txt`, then, once its result is in the history, a final text. Any other request of that model (for example OpenCode's title generation) gets the canned text of the default scenario. |
| `mock-assistant` | `mappings/agent-script.json` | for the general agent (`adam-agent`), stateless: a request that holds a tool result (`role: tool`) gets a fixed text (`I looked into it with the tool you gave me. ...`), priority 1; any other request gets `Hi! I'm <name>. <summary>.`, **built from the first two lines of the system prompt** (`Your name is <name>.` and `In one sentence: <summary>.`), priority 2. Also as a stream (see below). It answers in role whatever is asked: it proves that the folder reaches the model, not what a model does with it. `dev/agent-e2e.sh` runs it through the stack. |
| `mock-researcher`, question text contains `[mock:cards]` | `mappings/researcher-cards.json` | `show` (`cards-call-1`) with three blocks: a `Text`, a `Cards` of three sources (`https://example.org/mock-search/1` to `/3`, each with a subtitle, a body and tags) and a `Mermaid` `graph TD`, priority 1; once the history holds `cards-call-1` (whatever the result was: drawn, or refused by a screen without `Cards`) the text `Here are the three sources I found: ...` with the three links (`stop`), priority 1. Any other request of that model gets the canned text of the default scenario (the orchestration layer's own `mock-researcher`, which searches, is a different mock, in its repository). |

**Streamed answers.** The agents call their model with `"stream": true` (`LlmAgentBuilder::stream_text` is on, [ADR 0007](docs/decisions/0007-progress-as-steps-and-streamed-text.md)),
so every scripted answer above also has an SSE twin, in `mappings/coder-script-stream.json`, `coder-choices-stream.json`,
`agent-script-stream.json` and `researcher-cards-stream.json`: the same request matchers plus `$.stream == true`, **one priority
above the original** (so priority 0 for the ones that were 1), and the same answer as a stream: a text in about eight content
deltas (the greeting and the other answers dribbled over half a second, the coder's last answer over about two seconds, so a
screen has something to show growing), a tool call in a few argument deltas, the usage chunk and `data: [DONE]`. The off-script
404 and the error scenarios are plain HTTP errors, which a stream request gets as well. `cargo test -p adam-model-openai --test
wiremock_compose` (CI's `compose` job, `ADAM_TEST_MOCK_OPENAI_URL`) plays every script from the first request to the final
answer **both ways** and requires the same response (text, tool calls, finish reason, usage), so a twin cannot drift from
its original: **change a script in both files**.

The steps mirror the reference script of `bin/adam-coder/tests/binary.rs`. The greeting mapping has priority 1 and
the first step of the script (`prepare_workspace` on a first request) priority 2, so a greeting is never taken for a
task; the 404 is priority 3. `dev/greeting-e2e.sh` runs the greeting through the stack (see below).
A request of `mock-coder` that is not on the script (an id out of order, a
history the script does not know) is answered with **404** `off_script` on
purpose, so a run that leaves the script fails loudly instead of wandering on
the canned answers. The task text only has to name the repository and base
branch; the script does not read it, apart from the `[mock:no-opencode]` switch.

OpenCode's tool name and argument (`bash`, `command`) are *verified 2026-09-29*:
the published `opencode-ai` 1.18.33 binary (the version pinned in
`vymalo/another-agentic-images`' workspace image at the time) ran through the
real `adam-coder` against these mappings, and all three stubs matched (the
title-generation fallback, the `bash` call, the final text). Which OpenCode
version a given coder image ships is *unverified*; a newer one that renames the
tool needs the mapping updated, and `NO_OPENCODE=1` is the variant that does not
depend on it.

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

Two more suites need a service of their own and are gated apart, so that `ADAM_TEST_REQUIRE_DB=1` does not turn them on: the devcontainer
test (`ADAM_TEST_DEVCONTAINER=1`, with `ADAM_TEST_REQUIRE_DEVCONTAINER=1` to fail instead of skip) and the **cluster test** of
`adam-env-kubernetes` (`ADAM_TEST_KUBECONFIG`, the kubeconfig of the coder's ServiceAccount, with `ADAM_TEST_REQUIRE_KUBERNETES=1` to fail
instead of skip; its other variables are in the header of `crates/adam-env-kubernetes/tests/cluster.rs`). CI's `run-pods` job sets up a `kind`
cluster for the second with `deploy/coder/tests/kind-run-pods.sh`, which a developer with `kind` can run too.

The compile tests of `#[tool]` (`cargo test -p adam --test ui`, trybuild) split in two: the
errors the macro produces itself always run, and the ones rustc words itself run only with
`ADAM_TRYBUILD=1`, in the CI job `ui` pinned to one toolchain (see the
[`adam` README](crates/adam/README.md#tests)).

The suite (27 cases) covers: exact JSON roundtrip (unicode, i64 bounds,
floats, special keys), CAS conflicts, 16-way concurrent commits with a single
winner, journal ordering, first-writer-wins and 16-way races,
non-determinism detection, due rules, agent filtering and limits, busy runs left unclaimed, 8 workers
claiming 60 runs with no double lease, lease expiry and takeover, renew and
release, pinned claims (an owned run never goes to another worker, the owner is set by the
first pinned claim and only by it, 4 workers racing), one-open-run-per-conversation including a 16-way race, and purging.

To add a backend (SQLite, Redis, FoundationDB, ...), implement `Store` and add
one line: `adam_store_testkit::store_conformance!(make_store);`. Likewise a new
`Notifier` (a Redis or NATS transport, say) runs
`adam_notify_testkit::notifier_conformance!(make_pair);`; the Postgres one needs
`ADAM_TEST_POSTGRES_URL` (a superuser, for its reconnect test).

## Development

Every crate has a `README.md` next to its `Cargo.toml` (what it is for, its
public API at a glance, features and environment variables, how it is tested),
and `readme = "README.md"` in its manifest. Update the README in the same
change as any change to the crate's public API, environment variables or
tests. CI (the `lint` job) fails when a `crates/*/Cargo.toml` or
`bin/*/Cargo.toml` has no sibling `README.md`; it cannot check that the README
is still accurate, so review does. The crate table above links each README.

Docs live in [`docs/`](docs/README.md). Every process there is a Mermaid
diagram, and CI (the `docs` job) parses each diagram and resolves each relative
link and `#heading` in the repository's Markdown:

```sh
npm --prefix tools/docs-check ci          # once per clone
node tools/docs-check/check-docs.mjs
```

The same check covers the repository's own agent skills (below): their frontmatter, their mirrors and
every repository path they cite.

### Agent context and skills

[`CLAUDE.md`](CLAUDE.md) (`AGENTS.md` is a symlink to it) is the guide for an agent working in this
repository: layout, rules, commands, and which skill to use when. Skills live in `.agents/skills/`,
mirrored by symlinks in `.claude/skills`, `.goose/skills` and `.kiro/skills`. The vendored ones are
pinned in `skills-lock.json` (licences in [`third-party-notices.md`](third-party-notices.md)) and
updated with the skills CLI, never by hand.

**Skills this repository provides.** Six first-party skills are for the repositories that integrate
adam-rs; each says to read adam-rs files at the revision you pin:

| Skill | For |
|---|---|
| `adam-agent-folder` | an agent that is only a folder, served by `adam-agent` |
| `adam-embed` | hosting adam agents in your own Rust process |
| `adam-store-adapter` | implementing or updating a `Store` or `Notifier` |
| `adam-a2a-extensions` | the A2A extensions an adam agent declares, and how a client activates them |
| `adam-coder-deploy` | the coder image and its Helm chart |
| `adam-upgrade` | moving a consumer from one adam-rs revision to another |

```sh
npx skills add vymalo/another-adam-rs --list                          # these six, and only these
npx skills add vymalo/another-adam-rs --skill adam-agent-folder --skill adam-upgrade -a claude-code -y
npx skills update -p -y                                               # later: take their newer versions
```

The consumer's `skills-lock.json` pins what it installed. `update-vendored-skills` is internal and
is not listed.

## Roadmap

1. ~~Store trait and adapters~~ (this repo)
2. ~~Run state machine and `ctx.step` journaling~~ (`adam-runtime`)
3. ~~`#[tool]` macro (schemars)~~ (`adam-macros`, through the `adam` facade; [`docs/authoring.md`](docs/authoring.md))
4. ~~`build.rs` discovery of `agent/` (instructions, skills, subagents, `mcp.json`)~~ (`adam-agent-fs` parses and validates, `build("agent").emit()` embeds, `adam::include_agent!()` includes; [`docs/authoring.md`](docs/authoring.md)). `adam-assembly` binds it to `LlmAgent`s (templating, tool binding, models, the skills catalog with `load_skill` and `read_skill_file`, the A2A card); the runtime side of subagents as child runs is built (`Runtime::start_child`, the `adam.run.finished` message, `ToolError::AwaitRun`), subagents are tools of their parent (`adam-assembly`'s `SubagentTool`, one per subagent, with least-privilege tools, name checks and no asking tools); remote (A2A) subagents are tools of the same shape (`a2a:` and `auth: bearer:VAR` in the file; a journaled `SendMessage`, then `GetTask` on the wait timer); dev reload behind the feature `dev` (`LiveAssembly`: the directory is read at run time and swapped at step boundaries, the last good version stays on an invalid edit); `mcp.json` tools behind the feature `mcp` (`adam-mcp`: each agent's servers connected at startup, `<server>__<tool>`, allow-listed, fail closed, at-least-once)
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
