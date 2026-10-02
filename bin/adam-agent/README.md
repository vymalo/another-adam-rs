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
[`docs/authoring.md`](../../docs/authoring.md). It holds exactly one agent (`agents/` with several is refused),
and it is read once, at startup ([ADR 0004](../../docs/decisions/0004-agent-folders-at-run-time.md)): an edit
applies at the next start, and a restart is a deploy.

| In the folder | What it does here |
|---|---|
| `agent/instructions.md` frontmatter `name` | the registered name of the agent, which is the key of its stored runs: **renaming strands the runs of the old name**. Required |
| `description` or `card.description` | the A2A card needs one of them (exit 78 for a role that serves A2A otherwise) |
| `card:` | the card: `name`, `skills` (`id`, `name`, `description`, `tags`, `examples`). The URL is `PUBLIC_URL`, the version is this binary's |
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
  (`get_ui_catalog` today; the relayed tools of attached servers later), through a `ToolSource` that `assemble`
  gives every agent. The URL is an MCP server's: **`MCP_ALLOW_INSECURE=true`** lets it be plain `http` on another
  host (a compose stack); https and loopback need nothing. No grant, or an expired one, offers nothing.

The card lists A2UI v0.9.1 (with `acceptsInlineCatalogs: true`), `ui-catalog/v1`, `thread-tools/v1`, `steps/v1` and
`text-stream/v1` (`card_of`), and the service reads A2A messages as ones from a screen (`vymalo_inbound`, set in `agents`); an agent
whose messages carry none of that is not affected. **Every tool call is a step** (`tool:<call id>`, labelled with the
tool's name, running, then completed, failed or waiting for the person) to a client that activates `steps/v1` (the
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
graph) with `show` after reading the screen with `ui_catalog`. Its files are the same as the orchestration layer's
copy of the folder (`another-agentic-system`, `dev/agents/researcher/agent`) and are kept in step by hand. A folder
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
[MCP tools at run time](../../docs/authoring.md#mcp-tools-at-run-time-built-feature-mcp); this is how a folder
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
| `ROLE` | what this process runs: `all`, `control-plane` or `worker` (see [Roles](#roles)) | `all` |
| `DATABASE_URL` | Postgres for the run store | required |
| `A2A_BEARER_TOKENS` | comma-separated accepted tokens (fail closed: none = no server) | required by `all` and `control-plane` |
| `PUBLIC_URL` | where clients reach the JSON-RPC endpoint (agent card) | required by `all` and `control-plane` |
| `LISTEN_ADDR` | bind address: the A2A server, or a worker's `/healthz` listener | `0.0.0.0:8080` |
| `WORKERS` | runs advanced concurrently | `4` |
| `WORKER_ID` | lease identity of this worker: 1 to 128 of letters, digits, `.`, `_`, `-`, not starting with `.` | random per process |
| `MODEL_BASE_URL`, `MODEL_API_KEY` | OpenAI-compatible gateway (with `/v1`) and its key | required by `all` and `worker` (key may be empty) |
| `MODEL` | model alias of the agent | required by `all` and `worker` |
| `MCP_ALLOW_STDIO`, `MCP_ALLOW_INSECURE`, `MCP_ALLOW_URL_VARS` | what the folder's MCP servers may be (see above) | `false` each |
| `RUST_LOG` | log filter (JSON logs on stdout) | `info` |

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
| `card_of(&def, &public_url)` | the A2A card the files declare |
| `assemble(def, model, alias, &policy)` | connect the MCP servers, bind `ask_user`, `show` and `ui_catalog` and the thread-tools source (the `policy` is also the one for the thread-tools URL), give the root and each subagent the model: the `Assembly` |
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
"A general agent from a folder" in the [root README](../../README.md#a-general-agent-from-a-folder). CI
(`.github/workflows/coder.yml`) builds the image once and smoke-tests both binaries in it:
`docker/coder/test/container-smoke.sh` for `adam-coder` and `docker/coder/test/agent-smoke.sh` for `adam-agent`
(no folder: exit 78; the example folder mounted: the card, `401` without a token, a completed task against a stub
model, `tini` as PID 1, SIGTERM exits 0), then the compose scenarios.

## Tests

* `src/config.rs`: `ADAM_AGENT_DIR` required by every role and an existing directory, every problem at once,
  a control plane that needs no model, secrets hidden from `Debug`. 
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
