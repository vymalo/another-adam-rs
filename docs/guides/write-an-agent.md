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
exactly that with `ask_user`.
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
└── mcp.json                 MCP servers whose tools the agent gets
```

| Frontmatter | What it does |
|---|---|
| `name` | the registered name and **the key of the stored runs**: renaming strands the old runs |
| `description` or `card.description` | the A2A card needs one |
| `card:` | the card: `name`, `skills` (`id`, `name`, `description`, `tags`, `examples`); the URL is `PUBLIC_URL` |
| `limits:` | `max_turns`, `max_tool_calls`, `max_output_tokens`, `max_history_tokens` |
| `model:` | a gateway alias, when it should not be `MODEL`; `inherit` takes the parent's |
| `vars:` | defaults for `{{placeholders}}` in the body; an undeclared or unused one stops startup |
| `tools:` | optional selection among the agent's tools; `linear__*` takes a server's tools; an unknown name is refused with a suggestion |
| `skills:`, `preload_skills:` | which skills the model sees, and which are put in the prompt whole |

Rules worth knowing:

* **Secrets and endpoints never go in files.** `api_key`, `token`, `secret`, `password`, `base_url` are errors;
  `model` is an alias. `${VAR}` exists only in `mcp.json`.
* **A subagent inherits nothing**, and without `tools:` it has none. It cannot use a tool that asks the person.
  A file copied from `.claude/agents/` parses unchanged, but its `tools` must name adam's tools.
* **A remote subagent** is a file with `a2a: <agent-card URL>`: another A2A agent, called like a subagent. Its token
  is the environment's (`auth: bearer:VAR`), plain `http` to a service of the cluster needs
  `A2A_ALLOW_INSECURE_REMOTES=true`, and one declared in a subagent's directory (`subagents/researcher/subagents/`)
  is that subagent's tool; `files: true` in its file passes the files of its answer (a screenshot) on, and they
  reach the person only from the root's own remotes: a subagent's stay on its run
  ([Remote subagents](../reference/agent-files.md#remote-subagents-a2a)).
* **A subagent of Adam shares its root run's workspace**: `bin/adam-coder/agent/subagents/` has two read-only ones
  (`explorer`, `reviewer`) that read the worktree the calling run prepared.
* **The agent already has** `ask_user`, `show` and `ui_catalog` (the person's screen), the tools of its MCP
  servers (`<server>__<tool>`), `load_skill` and `read_skill_file` when it has skills, and one tool per subagent.
* **MCP**: credentials go in `headers` as `${VAR}`; `tools` is an allow-list; `"optional": true` lets a server be
  down; `"files": true` hands the images and documents its tools return (a browser's screenshot and PDF) to the
  person as files of the run, the model reading one line each. Which kinds of server are allowed is the
  **deployment's** decision (`MCP_ALLOW_STDIO`, `MCP_ALLOW_INSECURE`, `MCP_ALLOW_URL_VARS`), not the file's.
  `ADAM_EXTRA_MCP_FILE` adds servers without copying the folder.
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
tools that are **safe to repeat**. Embed the folder with `adam_agent_fs::build("agent").emit()` in `build.rs` and
`adam::include_agent!()`; bind it with `AgentDef::from_manifest(AGENT)?.bind(tools)?.state(env).model(model, alias)?`.
Files for the common case, a Rust wrapper around the assembled agent for policy (the coder does this for its
completion rule). The full contract: [Agent files and `#[tool]`](../reference/agent-files.md#the-tool-contract).

## Where to read more

| | |
|---|---|
| Formats, validation, skills, subagents, MCP, dev reload | [Agent files](../reference/agent-files.md) |
| What a binary does with a folder | [`bin/adam-agent`](../../bin/adam-agent/README.md) |
| Every variable | [Environment](../reference/environment.md) |
| The screen's tools (`ask_user` with `choices`, `show`) | [`adam-ui`](../../crates/adam-ui/README.md) |
| Deploying a folder agent | [Deploy the coder](deploy-the-coder.md#a-folder-agent-instead-of-the-coder) |
| For an AI assistant working on this | the `adam-agent-folder` skill |
