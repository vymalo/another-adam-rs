# adam-agent

One binary that serves **any agent folder** over A2A. The agent is files, read when the process starts:
instructions, card, skills, subagents and the MCP servers whose tools it uses. A chat assistant, a
researcher on a web-search server, a reviewer: each is a folder, not a build.

```sh
ADAM_AGENT_DIR=dev/agents/assistant \
DATABASE_URL=postgres://postgres:postgres@127.0.0.1:5432/adam_test \
MODEL_BASE_URL=http://127.0.0.1:8081/v1 MODEL_API_KEY= MODEL=mock-assistant \
A2A_BEARER_TOKENS=dev-token PUBLIC_URL=http://127.0.0.1:8080/ \
  cargo run -p adam-agent
```

It is the second binary over [`adam-service`](../../crates/adam-service/README.md), the first being
[`adam-coder`](../adam-coder/README.md); the process (the store, the roles, the notifications, the exit
codes) is the same code. What is different is the agent: `adam-coder` has an embedded one and nine tools of its own
written in Rust (a worktree, its files, OpenCode, checks, a pull request); `adam-agent` has **none of its own**, and the
only tools it brings are the person's screen: `ask_user`, `show` and `ui_catalog`
([`adam-ui`](../../crates/adam-ui/README.md)). Everything else an agent can do comes from its folder.

## The agent folder

`ADAM_AGENT_DIR` is **required by every role** (there is no default and no embedded copy: a silent persona
would hide a missing mount). It names the directory that holds `agent/`, or `agent/` itself, in the format of
[`docs/reference/agent-files.md`](../../docs/reference/agent-files.md). It holds exactly one agent (`agents/` with several is refused),
and it is read once, at startup ([ADR 0004](../../docs/decisions/0004-agent-folders-at-run-time.md)): an edit
applies at the next start, and a restart is a deploy.

| In the folder | What it does here |
|---|---|
| `agent/instructions.md` frontmatter `name` | the registered name of the agent, which is the key of its stored runs: **renaming strands the runs of the old name**. Required |
| `description` or `card.description` | the A2A card needs one of them (exit 78 for a role that serves A2A otherwise) |
| `card:` | the card: `name`, `skills` (`id`, `name`, `description`, `tags`, `examples`) and `extended` (`description`, `skills`: what an authenticated caller sees on top, `GetExtendedAgentCard`). The URL is `PUBLIC_URL`, the version is this binary's with the build's revision as build metadata (`0.1.0+6478fbc`, `+unknown` without one: the build argument `ADAM_BUILD_REVISION` of the image) |
| `limits:` | `max_turns`, `max_tool_calls`, `max_output_tokens`, `max_history_tokens` |
| `model:` | the model alias of the agent, when it should not be `MODEL` |
| `vars:` | `{{placeholders}}` of the prompt. **Every var needs a value in the file**: nothing in this binary supplies one, so a var declared without a value (or one the prompt does not use, or a placeholder `vars` does not declare) stops the process at startup, naming it |
| `tools:` | optional; selects among the tools the agent has (`ask_user`, `show`, `ui_catalog`, the MCP tools `<server>__<tool>`, the skills' tools, the subagents). `linear__*` takes a server's tools. A name that is none of them is refused with a suggestion |
| the body | the system prompt |
| `skills/` | Agent Skills: a catalog the model loads from on demand (`load_skill`, `read_skill_file`) |
| `subagents/` | assembled and **registered beside the agent** (`<name>/<subagent>`): one tool per subagent, a child run with its own prompt and tools. A subagent gets only the tools it lists, and not `ask_user` (nobody would answer it) |
| `mcp.json` | MCP servers whose tools the agent gets, see [below](#mcp-servers-from-the-folder). Connected by the **workers** |
| `schedules/` | read, not run: a warning says so |

The built-in tools are the person's screen ([`adam-ui`](../../crates/adam-ui/README.md)), and the same ones
`adam-coder` has, with descriptions that fit any agent:

* `ask_user { question, choices? }`: the run parks, A2A reports `input-required` with the question, and the person's
  answer resumes it. With `choices` (up to 8 questions of 2 to 8 options) and a screen that can draw a form, the
  question carries one Choices surface and the answers come back as the result (`- db: pg`); otherwise the options
  are listed in the question's text. `asks_user` is `true`, so a subagent never has it.
* `show { blocks, title? }` and `ui_catalog {}`: draw blocks of the components the person's screen has (cards, a
  diagram), and list them. They answer "answer in text" when the screen sent no catalog. A folder that does not want
  them lists the tools it does want in `tools:`.
* **The conversation's tools.** A message from the orchestration layer's chat announces one MCP endpoint for the
  conversation (`thread-tools/v1`); whatever it lists is offered to the model at every turn under its listed name
  (the relayed tools of attached servers; not `get_ui_catalog`, which the model has as `ui_catalog`), through a
  `ToolSource` that `assemble` gives every agent, and that also describes `show` with the components of the
  conversation's screen. `show` refuses a `Choices` form (a dead form: ask with `ask_user` and `choices`). The URL
  is an MCP server's: **`MCP_ALLOW_INSECURE=true`** lets it be plain `http` on another host (a compose stack); https
  and loopback need nothing. No grant, or an expired one, offers nothing. A tool may say how long it can take
  (`_meta["thread-tools/v1"].timeoutSecs`): the call waits that long, at most **`THREAD_TOOLS_MAX_CALL_SECS`** (default 3600;
  60 s for a tool that says nothing), carries a `callId` that a retried step repeats, and a tool the orchestrator reports as
  a step (`reportsStep`) gets no step of the agent's. When the person mentioned agents, the instructions gain a "Mentioned
  agents" block (only then); see [`adam-ui`](../../crates/adam-ui/README.md) and
  [ADR 0015](../../docs/decisions/0015-tools-the-orchestrator-reports-long-calls-and-mentioned-agents.md).

**Sending while it works** (`steer/v1`, on the card): when a request activates the extension, a message that names the running task
is delivered to it and read at its next step, and a final answer written while one is unread takes another model turn instead of
ending the run, so a message sent during the last model call is answered; a finished task answers `-32004`, and without the
activation the message is refused as before ([`adam-a2a-runtime`](../../crates/adam-a2a-runtime/README.md#steering-a-running-task),
[ADR 0016](../../docs/decisions/0016-a-message-sent-to-a-working-task-is-steered-into-it.md)).

The card lists A2UI v0.9.1 (with `acceptsInlineCatalogs: true`), `ui-catalog/v1`, `thread-tools/v1`, `mentions/v1`, `steer/v1`, `steps/v1` and
`text-stream/v1` (`card_of`), and `build/v1` (`card_of_folder`: the build's revision and the folder's digest, [ADR 0028](../../docs/decisions/0028-the-card-says-which-build-answers.md)), and the service reads A2A messages as ones from a screen (`vymalo_inbound`, set in `agents`); an agent
whose messages carry none of that is not affected. **Every tool call is a step** (`tool:<call id>`, labelled with a
title a person reads (`Ask you`, an MCP tool's own `title`, a subagent's name capitalised, [ADR 0027](../../docs/decisions/0027-every-tool-has-a-title-for-its-step.md)); running, then completed, failed or waiting for the person; with the
call's arguments as `input` and its result as `output`, cut to 4 KiB and 8 KiB and **scrubbed of this process's secrets first**: the model's
key, the A2A tokens, the password of `DATABASE_URL` and the value of every environment variable whose name says it is a secret, which is
where the `${VAR}` values of an `mcp.json` come from; `redact::step_io`, [ADR 0011](../../docs/decisions/0011-a-tool-calls-step-carries-its-input-and-output.md))
to a client that activates `steps/v1` (the
orchestration layer's chat does when the card lists it), and a line of text to one that does not
([ADR 0007](../../docs/decisions/0007-progress-as-steps-and-streamed-text.md)): the MCP tools of the folder, `show`,
`ask_user`, and its subagents, which are `subagent` steps. **The model's answer is streamed** (`stream_text` is on, so every model
call is a stream) to a client that activates `text-stream/v1`: chunks while the model writes, then the whole text under the
stream's id, and the whole reply with the turn to one that does not (the same ADR; `adam-a2a-runtime`'s README says how).

A folder that follows the **persona convention** (the body opens with `Your name is {{display_name}}.` and a line
`In one sentence: <summary>.`, the summary without `"` and ending at its first period) is greeted by the mock
models of this repository and of `another-agentic-system` in role: they build `Hi! I'm <name>. <summary>.` from
those two lines, so editing them changes the mocked answer. A live model follows the whole prompt; what it does
with it is not tested here.

`dev/agents/assistant/agent/` is a complete example, and the folder the stack's `agent` service mounts.
`dev/agents/researcher/agent/` is a second one: **a researcher** whose `mcp.json` names a web-search MCP server (the
mock of the orchestration layer's stack) and whose instructions tell the model to search, to cite every source as a
link, and, when the screen has the components, to show the sources as `Cards` (and how they relate as a `Mermaid`
graph) with `show` after reading the screen with `ui_catalog`. Both folders end with a section, **What the person
sees**: the words before a tool call are working notes, shown in the activity panel and not in the conversation;
the reply that ends the turn is the only text in it, so it is complete on its own and puts the result first; and
replies render as Markdown. (The orchestration layer's copy of the folders, `another-agentic-system`,
`dev/agents/*/agent`, is kept in step by hand, and has not taken this section yet.) A folder
of a few lines is enough:

```markdown
---
name: chat
description: A chat assistant.
card:
  name: Chat
---
Your name is Chat.
In one sentence: I talk things through with you.

Answer in short, plain sentences. If you cannot answer without something from the person, ask for
exactly that with `ask_user`.
```

## MCP servers from the folder

An `agent/mcp.json` (and one next to each subagent's file) names the MCP servers whose tools the agent gets,
named `<server>__<tool>`. Every worker connects them once at startup, before it serves. The format and the
rules are [`adam-mcp`](../../crates/adam-mcp/README.md)'s and
[MCP tools at run time](../../docs/reference/agent-files.md#mcp-tools-at-run-time-feature-mcp); this is how a folder
declares a web-search server for a researcher:

```json
{
  "mcpServers": {
    "search": {
      "type": "http",
      "url": "https://search.example.com/mcp",
      "headers": { "Authorization": "Bearer ${SEARCH_TOKEN}" },
      "tools": ["web_search"]
    }
  }
}
```

The agent then has the tool `search__web_search` (the allow-list `tools` keeps only what is listed; without it
every tool the server offers whose name fits is taken), and `tools: ['search__*']` in the instructions would
select them. A **researcher** is exactly this: a persona, a skill (`web-research`) and this `mcp.json`. The client
works against a *stateless* server too (`POST /mcp` answered with JSON, no `Mcp-Session-Id`, `405` for `GET` and
`DELETE`: the shape a small mock has), which `tests/` runs against (*verified 2026-10-01*, see
[Tests](#tests)); a server that streams its answers or keeps sessions works as well (`adam-mcp`'s own tests).

* **The process is non-dumpable.** At startup `adam-agent` calls `adam_service::harden::make_non_dumpable` (`prctl(PR_SET_DUMPABLE, 0)`,
  Linux), as `adam-coder` does: the command of a `stdio` MCP server (when the deployment allows one) is a process of the same user, and
  without it `cat /proc/<agent pid>/environ` would give it the model key, the database URL and every variable of the process. A failure is a
  warning in the log and not fatal.
* **Environment variables in `headers`**: `${SEARCH_TOKEN}` and `${VAR:-default}` read the process environment.
  Put credentials there. Unset and no default: exit 78 naming the variable, never its value.
* **Environment variables in a `url`** (`"url": "${SEARCH_URL}"`, so that one folder serves a laptop and a
  cluster) are refused unless the deployment sets `MCP_ALLOW_URL_VARS=true`: the MCP client library logs the URL
  it dials, so a secret in a URL would reach the logs. Use it for URLs that are not secrets.
* **Which kinds of server are allowed** is the deployment's, not the file's: a local process (`command`) needs
  `MCP_ALLOW_STDIO=true`, plain `http` to another machine needs `MCP_ALLOW_INSECURE=true` (development only);
  `https` and loopback need nothing. `type: sse` is not supported.
* **A server that is down** at startup stops the process with exit 69, so a supervisor restarts it until the
  server is up; a mistake in the files or in the policy is 78. A tool call that fails later is an error result
  the model reads, not a failed run.
* A **control plane** serves the card and starts runs, which needs no tools, so it connects no server and
  validates no MCP variable; only `all` and `worker` do.

## Configuration

Environment variables (`src/config.rs` is the reference for this binary's, and
[`adam-service`](../../crates/adam-service/README.md#environment) for the ones every agent binary reads the
same way; every problem is reported at once at startup):

| Variable | Meaning | Default |
|---|---|---|
| `ADAM_AGENT_DIR` | the agent folder; **required by every role**, and an existing directory (exit 78 naming the variable otherwise) | required |
| `ADAM_EXTRA_MCP_FILE` | roles that run workers: a file of extra MCP servers in the shape of `mcp.json`, added to the folder's own before they connect (an existing file, else exit 78; a name the folder already has is refused, exit 78). Same parser, `${VAR}`, `tools`, `optional` and policy as `mcp.json`; the variables it names are scrubbed from the tool-call steps like the folder's own (`redact::step_io_named`). See [`adam-assembly`](../../crates/adam-assembly/README.md#extra-mcp-servers-adam_extra_mcp_file) | unset: only the folder's servers |
| `ROLE` | what this process runs: `all`, `control-plane` or `worker` (see [Roles](#roles)) | `all` |
| `DATABASE_URL` | Postgres for the run store | required |
| `A2A_BEARER_TOKENS` | comma-separated accepted tokens (fail closed: none = no server) | required by `all` and `control-plane` |
| `PUBLIC_URL` | where clients reach the JSON-RPC endpoint (agent card) | required by `all` and `control-plane` |
| `A2A_PUSH_ALLOWED_URLS` | turns A2A push notifications **on** and names the webhooks they may reach: comma-separated URL prefixes or hosts ([`adam-service`](../../crates/adam-service/README.md#environment)); a client's webhook that matches none, is not `https` or is a private address is refused | unset: off, the card says so |
| `A2A_PUSH_ALLOW_PRIVATE` | also allow loopback, private and link-local webhooks and `http` to loopback: development only | `false` |
| `A2A_PUSH_GIVE_UP_AFTER_SECS`, `A2A_PUSH_REQUEST_TIMEOUT_SECS` | how long a notification may keep failing before delivery to that webhook is abandoned (1 to 604800), and how long one request may take (1 to 120) | `3600`, `15` |
| `A2A_CARD_SIGNING_KEY_FILE`, `A2A_CARD_SIGNING_KEY_ID`, `A2A_CARD_SIGNING_JKU` | a PKCS#8 PEM key (ECDSA P-256 or Ed25519) that signs the agent card, its `kid` (default: the key's thumbprint) and its `jku` ; the server serves the key set at `/.well-known/jwks.json` | unset: unsigned |
| `A2A_DOCS` | Swagger UI at `/docs` and the OpenAPI document at `/openapi.json`, public; `false` turns them off ([`adam-service`](../../crates/adam-service/README.md#environment)) | `true` |
| `LISTEN_ADDR` | bind address: the A2A server, or a worker's `/healthz` listener | `0.0.0.0:8080` |
| `WORKERS` | runs advanced concurrently | `4` |
| `WORKER_ID` | lease identity of this worker: 1 to 128 of letters, digits, `.`, `_`, `-`, not starting with `.` | random per process |
| `MODEL_BASE_URL`, `MODEL_API_KEY` | OpenAI-compatible gateway (with `/v1`) and its key | required by `all` and `worker` (key may be empty) |
| `MODEL` | model alias of the agent | required by `all` and `worker` |
| `MODEL_EXTRA_BODY` | a JSON object merged into every chat-completions request of the agent's model, for a flag that makes a gateway or model emit its reasoning: `{"reasoning_effort":"medium"}`, `{"thinking":{"type":"enabled"}}`, `{"chat_template_kwargs":{"enable_thinking":true}}`. **Not a secret** (it shows in the pod's environment). Not JSON, not an object, or a member the runtime owns (`model`, `messages`, `tools`, `tool_choice`, `stream`): exit 78 at startup, the message names the variable and never repeats the value | unset: nothing is added |
| `MODEL_ECHO_REASONING` | `reasoning_content` or `reasoning`: keep the model's reasoning in the run's history and send it back under that member name. For a provider that requires it (DeepSeek's thinking mode with tools answers a request without it with a 400); `false` or unset sends none, which is what almost every model wants. Anything else: exit 78 | unset: never sent |
| `MODEL_CONTEXT_WINDOW` | the context window of the model `MODEL` names, in tokens (1 to 9007199254740991): what each `usage/v1` call report of a call on that alias says as `contextWindow`; a subagent whose `model:` names another alias reports none. Anything else: exit 78 | unset: reports carry no window |
| `MCP_ALLOW_STDIO`, `MCP_ALLOW_INSECURE`, `MCP_ALLOW_URL_VARS` | what the folder's MCP servers may be (see above) | `false` each |
| `THREAD_TOOLS_MAX_CALL_SECS` | the longest a call to a tool of the thread's tools endpoint is waited for, whatever time the tool says it may take (1 to 86400); a tool that says nothing is waited for 60 s | `3600` |
| `RUST_LOG` | log filter (JSON logs on stdout); when set it replaces the default whole, so `RUST_LOG=info` shows `rmcp` again | `info,rmcp=warn` (the MCP client library's per-connection lines are quiet; `adam_service::logging`) |

**`optional` and `tools:`**: a server marked `optional: true` that is skipped (down, no key, a missing allow-listed tool) has no tools, so a `tools:` entry of the agent that names one of them (`search__web_search`, `search__*`) makes the folder exit 78 at startup. Leave an optional server's tools out of `tools:`, or make the server required.

There is no `GITHUB_TOKEN`, no workspace and no placement: the agent has no worktree. Runs are not pinned to a
worker, so several workers share one database and any of them steps any run.

### Roles

`ROLE` is parsed with `adam_host::Role`. The components are `adam-service`'s (`all`: the A2A server and the
workers in one process; `control-plane`: the A2A server over a runtime that knows the agent as a starter only;
`worker`: the workers and a `/healthz` listener). Every role reads the folder: the control plane serves its card
and the workers run its prompt. Only the workers assemble the agent, so a mistake in `tools:`, a var or a
subagent shows on a worker (exit 78), and a control plane that has none still starts.

**Several agents, one database.** Runs are scoped by the agent's name: a service serves one agent, and a worker
claims only the runs of the agents it registered. So any number of `adam-agent` processes with different folders
can share one Postgres database. Two folders with the same `name` share their runs: that is what replicas of one
agent are.

## The process

```mermaid
sequenceDiagram
  participant M as main
  participant S as serve
  participant F as AgentFolder
  participant A as agents / assemble
  participant V as adam_service::serve
  M->>S: serve(Config, shutdown)
  S->>F: load(ADAM_AGENT_DIR), log `agent files`
  alt a role that runs workers
    S->>A: connect_mcp(policy), bind(ask_user, show, ui_catalog) and the thread-tools source, model(client, alias)
    A-->>S: Agents { the whole agent }
  else control plane
    S->>A: the start-only half (name and init)
    A-->>S: Agents { the starter }
  end
  S->>V: serve(ServiceConfig, Agents + card, shutdown)
  V-->>M: Ok on a clean shutdown, else the error
```

```mermaid
stateDiagram-v2
  [*] --> ReadingFolder
  ReadingFolder --> Refused: no ADAM_AGENT_DIR, errors in the files, two agents (78)
  ReadingFolder --> Assembling: one agent, warnings logged
  Assembling --> Refused: an MCP server is down (69), the policy or the files refuse (78)
  Assembling --> Serving: assembled (a control plane skips this step)
  Serving --> Draining: SIGTERM
  Serving --> Failed: a component stops, Postgres is gone
  Draining --> [*]: exit 0
  Refused --> [*]
  Failed --> [*]
```

Startup logs one `agent files` line (`source=folder`, `path`: the directory that holds `agent/`, `digest`:
`sha256:...`, `agent`, `warnings`) and each warning as `path:line: warning: ...`; the same line `adam-coder`
logs. A mistake in the files stops the process before it connects to anything, exit code 78, with every finding
as `path:line: error: ...` in the one `adam-agent failed` line.

Exit codes are `adam-service`'s: 0 after a clean shutdown, 78 configuration (including the folder and the files),
69 a dependency is unreachable (Postgres, an MCP server), 71 the OS refused something (a port), 70 internal (a
component stopped), 1 anything else. A failure is one structured log line with the whole cause chain, and
nothing on stderr.

## Library

The binary is `main.rs` over a small library, so everything it does is testable without a process:

| Item | What |
|---|---|
| `Config::from_env()`, `from_lookup` | the environment as above; `Config { service: ServiceConfig, agent_dir, worker: Option<WorkerConfig { model, mcp }> }` |
| `serve(config, shutdown)` | the process: folder, card, assembly, then `adam_service::serve` |
| `folder::load(path)`, `folder::log(&folder)` | read the folder (every diagnostic in the error), say which files run |
| `card_of(&def, &public_url)`, `card_of_folder(&folder, &public_url)` | the A2A card the files declare; the second also declares `build/v1` (the revision and the folder's digest). `build_version()`, `BUILD_REVISION`, `VERSION` |
| `assemble(def, model, alias, &policy)` | connect the MCP servers, bind `ask_user`, `show` and `ui_catalog` and the thread-tools source (the `policy` is also the one for the thread-tools URL), give the root and each subagent the model: the `Assembly`. `assemble_with(.., step_io)` also says how the tool-call steps report their input and output |
| `redact::step_io_named(&config, vars, &names)`, `redact::secret_values_named` | the same, and also the variables in `names` whatever their names look like (`AgentDef::mcp_env_references`: what the folder's and the extra file's servers read as `${VAR}`); `serve` uses it |
| `redact::step_io(&config, vars)`, `redact::secret_values` | the `StepIo` that scrubs the secrets of the configuration and of the environment variables `vars` (`redact::process_vars()` in `serve`: the process's, without what is not text) from the input and output of every tool-call step; `WorkerParts::step_io` carries it |
| `agents(def, card, workers)` | the `Agents` for `adam_service::serve` or a composition of your own: the whole agent with `workers: Some(WorkerParts)`, its starter with `None` |
| `AgentError`, `exit_code(&err)` | why a step failed, and the exit code of a chain of causes |

## Image and compose

`adam-agent` ships **inside the existing `coder` image** (`ghcr.io/vymalo/another-adam-rs/coder`): its
[Dockerfile](../../docker/coder/Dockerfile) builds and installs both binaries, and the entrypoint stays
`adam-coder`. A service that serves a folder runs the same image with the entrypoint overridden and the folder
mounted (read-only, readable by uid 10001):

```sh
docker run --rm --entrypoint tini \
  -v "$PWD/dev/agents/assistant/agent:/etc/adam/agent:ro" -e ADAM_AGENT_DIR=/etc/adam/agent \
  -e DATABASE_URL=... -e MODEL_BASE_URL=... -e MODEL_API_KEY=... -e MODEL=... \
  -e A2A_BEARER_TOKENS=... -e PUBLIC_URL=... -p 8080:8080 \
  ghcr.io/vymalo/another-adam-rs/coder:sha-<7> -- adam-agent
```

There is no second package: a new GHCR package is private until its owner makes it public, and a lean image of its
own (the coder's image carries the workspace toolchains, which a chat agent does not use) can come later. Stdio MCP
servers that need `node` or `python` need an image that has them (`MCP_ALLOW_STDIO=true` allows the kind; the image
brings the program). The coder image sets **no** `MCP_ALLOW_STDIO`: it belongs to the coder's own deployment, so an
`adam-agent` run from the image refuses local-process servers unless its deployment sets it.

`compose.yaml` has the service `agent` (profile `app`): the example folder `dev/agents/assistant/agent` mounted
at `/etc/adam/agent`, the model `mock-assistant` of the WireMock mock, the coder's database (runs are scoped by the
agent's name), port 8084 (`AGENT_PORT`), `AGENT_FOLDER` to mount another folder. A fourth agent is a folder and the
same dozen lines. `dev/agent-e2e.sh` runs "hi" through it and restarts it on an edited copy of the folder. See
[Run it locally](../../docs/guides/run-locally.md). CI
(`.github/workflows/coder.yml`) builds the image once and smoke-tests both binaries in it:
`docker/coder/test/container-smoke.sh` for `adam-coder` and `docker/coder/test/agent-smoke.sh` for `adam-agent`
(no folder: exit 78; the example folder mounted: the card, `401` without a token, a completed task against a stub
model, `tini` as PID 1, SIGTERM exits 0), then the compose scenarios.

## Tests

* `src/config.rs`: `ADAM_AGENT_DIR` required by every role and an existing directory, every problem at once,
  a control plane that needs no model, `MODEL_CONTEXT_WINDOW` read by the workers (a bad value is exit 78), secrets
  hidden from `Debug`.
* `tests/agent.rs` (in-process, over the in-memory store, scripted models): the card is the folder's, and lists the screen's three extensions, `steps/v1` and `text-stream/v1`; **a chat
  folder answers "hi" in role over A2A** (the task completes with the greeting its two persona lines give, the
  model is sent the folder's rendered prompt and the screen's three tools only); an edited folder says the edited words; `ask_user`
  parks the run as `input-required` and the answer resumes it; a control plane starts a run that a worker over the
  same store completes; the tools of an `mcp.json` server are offered and a call reaches the server with the
  token from the environment; `${VAR}` in a URL is refused unless allowed; the exit code of a server that is down
  (69), a refused policy and an unset variable (78); a local subagent runs as a child run; an unknown tool, a var
  without a value and a bad alias are refused at assembly; every diagnostic of a broken folder, two agents in one
  folder and a warning. **A researcher** on a stateless web-search MCP server (`tests/common`: `SearchServer`,
  `POST /mcp` with JSON responses, no session id, `405` for `GET`/`DELETE`, one tool `web_search`) gets
  `search__web_search` with the server's own schema, carries the token from the environment, and the model names
  the source from the results; an empty search and a failing one are results the model reads. **The researcher
  the repository ships** (`dev/agents/researcher/agent`, its search server pointed at the test's) loads without a
  warning and, scripted the way a good model follows its instructions (search, `ui_catalog`, `show`), **answers
  with its sources as cards and a graph on a screen that draws them**: one `ui` artifact (`application/a2ui+json`)
  under the screen's `catalogId` whose components all validate against version 3 of the web's catalog (a copy in
  `crates/adam-ui/tests/fixtures`), the prompt carries the instruction to show, and the model read the screen's
  five components; on a screen of catalog version 2 (no `Cards`) or with no catalog, `show` is refused to the
  model as an error result, the run goes on in words, and there is no artifact.
* `tests/binary.rs` (the binary as a process; the cases that need a database use `ADAM_TEST_POSTGRES_URL` and are
  skipped without it): no `ADAM_AGENT_DIR` (exit 78, every other problem listed with it), a missing, invalid or
  two-agent folder (78 with `path:line`, before anything connects), Postgres unreachable (69, no password in
  the output), the card and `/healthz` served and a clean SIGTERM, **a chat folder answering "hi" in role through
  the official A2A client against a model scripted by the prompt, and the same database after an edit and a
  restart saying the edited words**, a researcher answering with its source through a stateless MCP server, a control plane and a worker in two processes completing a task over one
  database, and the MCP cases of the section above as a process.
* The mock models `mock-assistant` (`dev/wiremock/mock-openai/mappings/agent-script.json`) and `mock-researcher`
  (`researcher-cards.json`, the script of `[mock:cards]`) are probed by the `compose` job of `ci.yml`; the container
  smoke test and the compose scenarios (`dev/agent-e2e.sh`, and `dev/agent-cards-e2e.sh`, which serves the researcher
  folder on `mock-researcher` and checks the cards and the graph, and the words alone on a screen without them) run
  in `coder.yml` (see above).
