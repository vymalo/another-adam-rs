# Write an agent

An adam agent is **Markdown for what the model reads, Rust for what runs**. Most agents are only a folder: the
`adam-agent` binary serves it, no build. When an agent needs tools of its own, add `#[tool]` functions and embed
the folder in your own binary ([Embed adam](embed-adam.md)). The formats follow Claude Code and GitHub Copilot
custom agents, Agent Skills and `mcp.json`; the layout is [eve](https://eve.dev)'s.

## The smallest agent

`my-agent/agent/instructions.md`:

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
exactly that with `ask_user`. When the answer is one of a few options, pass `choices` (a title, a
description of why you ask, options with a label and a description) and never list the options in your reply.
```

Serve it:

```sh
ADAM_AGENT_DIR=my-agent DATABASE_URL=postgres://... MODEL_BASE_URL=... MODEL_API_KEY=... MODEL=... \
A2A_BEARER_TOKENS=dev-token PUBLIC_URL=http://127.0.0.1:8080/ cargo run -p adam-agent
```

Or mount it into the image (`ADAM_AGENT_DIR=/etc/adam/agent`, entrypoint `adam-agent`), or add a service to
`compose.yaml`: copy `agent`, change the folder, port and token. `dev/agents/assistant/agent` is a working
example and `dev/agents/researcher/agent` a second one with an `mcp.json`.

## What goes in the folder

```text
agent/
├── instructions.md          frontmatter + the system prompt
├── instructions/            optional: more .md files, appended in filename order
├── skills/<name>/SKILL.md   Agent Skills (or skills/<name>.md); loaded on demand
├── subagents/<name>.md      one tool of the parent; a child run (or <name>/instructions.md)
├── commands/<name>.md       a prompt template a message invokes by name: `/review acme/app`
└── mcp.json                 MCP servers whose tools the agent gets
```

| Frontmatter | What it does |
|---|---|
| `name` | the registered name and **the key of the stored runs**: renaming strands the old runs |
| `description` or `card.description` | the A2A card needs one |
| `card:` | the card: `name`, `skills` (`id`, `name`, `description`, `tags`, `examples`); the URL is `PUBLIC_URL` |
| `limits:` | `max_upload_bytes` (root only: the largest file the card lists for `uploads/v1`, 1 to 4 GiB), `max_turns`, `max_tool_calls`, `max_output_tokens` (the output cap this agent asks for, at least 1; absent, its preset's, the model's profile's, then 128,000, clamped to the model's limit and window, ADR 0064), `max_history_tokens`, `max_parallel` (tool calls of one turn that run at once, 1 to 32; it lowers the deployment's `MAX_PARALLEL_TOOL_CALLS` for this agent), `max_open_sessions`, `max_working_sessions`, `max_session_rounds` (root only: with a subagent, the agent also offers sessions to a person over A2A, and these lower the card's `maxOpen`; lower the deployment's `SESSION_MAX_OPEN`, `SESSION_MAX_WORKING`, `SESSION_MAX_ROUNDS` for the sessions this agent starts, never raise them) |
| `model:` | a gateway alias, when it should not be `MODEL`; `inherit` takes the parent's |
| `reasoning_effort:` | the reasoning level the agent's model calls ask for by default: `none`, `minimal`, `low`, `medium`, `high`, `xhigh` or `max`, one the model's profile declares (ADR 0063); a message's `reasoning/v1` activation still wins per turn |
| `model_preset:` | a named preset of the deployment's `MODEL_ALIASES_FILE` (`presets:`) the agent's model calls start from (ADR 0064) |
| `model_fallback:` | overrides the preset's and the profile's `fallback`: the alias the run continues on, once, when the endpoint refuses the agent's model as locked; not the agent's own alias (ADR 0064, amended 2026-11-23) |
| `vars:` | defaults for `{{placeholders}}` in the body; an undeclared or unused one stops startup |
| `tools:` | optional selection among the agent's tools; `linear__*` takes a server's tools; an unknown name is refused with a suggestion |
| `tools_loading:` | `eager` puts every tool in every request, as before the key existed, including a server whose `mcp.json` entry has a `when`; `lazy` (the default) starts lean and lets a run load tools in groups (ADR 0066). `searchable` is accepted and behaves as `lazy` until searchable groups land |
| `skills:`, `preload_skills:` | which skills the model sees, and which are put in the prompt whole |

Rules worth knowing:

* **Secrets and endpoints never go in files.** `api_key`, `token`, `secret`, `password`, `base_url` are errors;
  `model` is an alias. `${VAR}` exists only in `mcp.json`.
* **A subagent inherits nothing**, and without `tools:` it has none. A tool that asks the person may be in its
  `tools:`, but the question only reaches the person in a kept-open session whose client shows child sessions
  (ADR 0045 decision 9); in any other child run the model is told that nobody can answer and goes on. A file
  copied from `.claude/agents/` parses unchanged, but its `tools` must name adam's tools.
* **A remote subagent** is a file with `a2a: <agent-card URL>`: another A2A agent, called like a subagent. Its token
  is the environment's (`auth: bearer:VAR`), plain `http` to a service of the cluster needs
  `A2A_ALLOW_INSECURE_REMOTES=true`, and one declared in a subagent's directory (`subagents/researcher/subagents/`)
  is that subagent's tool; `files: true` in its file passes the files of its answer (a screenshot) on, and they
  reach the person only from the root's own remotes: a subagent's stay on its run
  ([Remote subagents](../reference/agent-files.md#remote-subagents-a2a)).
* **References** (`references:` in the root's frontmatter) are repositories the **coder** reads beside the one it works
  on and never writes: a `name`, a `url`, an optional `branch` and a one-line `description` each. `adam-agent` does not
  serve them (it logs one warning), so use them in a coder's folder
  ([format](../reference/agent-files.md#references-references), [how they are read](../reference/workspace-and-environments.md#references)).
* **A subagent of Adam shares its root run's workspace**: `bin/adam-coder/agent/subagents/` has two read-only ones
  (`explorer`, `reviewer`) that read the worktree the calling run prepared.
* **The agent already has** `ask_user`, `show` and `ui_catalog` (the person's screen), `plan_work`, `plan_check` and
  `plan_add` (a plan: a checklist the person sees; tell the agent in its instructions when to plan, for example for
  work of three or more steps; with a `tools:` list, name `plan_*` to keep them), the tools of its MCP
  servers (`<server>__<tool>`), one `load__group__<server>` per MCP server with a `when` (the group's tools load when the task holds one of its keywords or the model calls the loader), `load_skill` and `read_skill_file` when it has skills (a folder with no `skills:` has every skill it holds; `skills: []` turns them off; the name is a plain string and a long skill is read page by page), and one tool per subagent.
* **MCP**: credentials go in `headers` as `${VAR}`; `tools` is an allow-list; `"optional": true` lets a server be
  down; `"files": true` hands the images and documents its tools return (a browser's screenshot and PDF) to the
  person as files of the run, the model reading one line each. Which kinds of server are allowed is the
  **deployment's** decision (`MCP_ALLOW_STDIO`, `MCP_ALLOW_INSECURE`, `MCP_ALLOW_URL_VARS`), not the file's.
  `ADAM_EXTRA_MCP_FILE` adds servers without copying the folder. `"when": {"keywords": ["…"]}` makes a server
  one group, loaded by `load__group__<server>`; the server's name becomes the group's name, so it is lowercase
  letters and digits with single underscores, at most 40 characters. A server entry can also set `timeoutSecs` and
  `connectTimeoutSecs` (capped by the deployment), `"instructions": false` (what the server says about itself is
  otherwise added to the agent's instructions, quoted) and `"prompts": false` (its prompts are otherwise commands
  `/<server>__<prompt>`); see [the reference](../reference/agent-files.md#timeouts-instructions-and-prompts).
* **Commands** are templates for the messages people send often. `commands/review.md` with a `description`,
  an optional `argument-hint` and a body that uses `$ARGUMENTS` or `$1` to `$9` is listed on the card, and a
  client (or a person typing `/review acme/app`) invokes it; the agent expands it before the run starts. A
  `/word` that is not one of yours stays plain text, and nothing in a template is ever executed
  ([format and limits](../reference/agent-files.md#commands-commandsnamemd)).
* **Edits apply at the next start.** The folder is read once; restart, no rebuild.

## When it does not start

Exit **78** is the folder or the configuration: every finding is one line, `path:line: error: ...`, in the
`adam-agent failed` log line. Exit **69** is a dependency that is down (Postgres, an MCP server). A startup log
line `agent files` names the path and digest that run.

## Tools in Rust

```rust
use adam::prelude::*;

/// Ask the person who gave you the task a question and wait for the answer.
#[tool]
pub async fn ask_user(
    /// What you need to know
    question: String,
) -> Result<ToolOutput, ToolError> { /* ... */ }

let tools = tools![AskUser];
```

The doc comment is the tool's description for the model, parameter doc comments are the property descriptions,
the arguments struct and its JSON Schema are generated, and the call runs inside the journaled step, so write
tools that are **safe to repeat**. The calls of one model turn run at the same time (up to 10 by default); a tool that
shares something with other calls says so, `#[tool(parallel = "read", path = "path")]`, `"write"`, `"independent"`, or
`access = my_fn`, and a tool that says nothing runs by itself, after the calls before it
([ADR 0049](../decisions/0049-the-tool-calls-of-a-turn-run-at-the-same-time.md)). A tool that fails with
`ToolError::Transient` (or `Retryable`, a rate limit or a conflict) is retried under the runtime's policy, or its
own: `#[tool(retry(attempts = 3, backoff = "1s", on = ["rate-limited"]))]`
([ADR 0061](../decisions/0061-a-step-names-its-retry-policy.md)); a folder sets it per tool in its frontmatter,
`retry: { web_search: { attempts: 5, backoff: 2s, on: [rate-limited] } }`, which wins over the tool's. Embed the folder with `adam_agent_fs::build("agent").emit()` in `build.rs` and
`adam::include_agent!()`; bind it with `AgentDef::from_manifest(AGENT)?.bind(tools)?.state(env).model(model, alias)?`.
Files for the common case, a Rust wrapper around the assembled agent for policy (the coder does this for its
completion rule). The full contract: [Agent files and `#[tool]`](../reference/agent-files.md#the-tool-contract).

## Where to read more

| | |
|---|---|
| Formats, validation, skills, subagents, MCP, dev reload | [Agent files](../reference/agent-files.md) |
| What a binary does with a folder | [`bin/adam-agent`](../../bin/adam-agent/README.md) |
| Every variable | [Environment](../reference/environment.md) |
| The screen's tools (`ask_user` with `choices`: a title, why, typed questions, pictures; `show`) | [`adam-ui`](../../crates/adam-ui/README.md) |
| Deploying a folder agent | [Deploy the coder](deploy-the-coder.md#a-folder-agent-instead-of-the-coder) |
| For an AI assistant working on this | the `adam-agent-folder` skill |
