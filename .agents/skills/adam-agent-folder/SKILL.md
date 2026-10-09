---
name: adam-agent-folder
description: "Write or change an adam-rs agent that is only a folder (agent/instructions.md, card, vars, tools, skills/, subagents/, mcp.json) and serve it with the adam-agent binary from the coder image. Use for 'add an agent', 'change an agent's instructions or MCP servers', 'why does adam-agent exit 78'. Not Docker Agent (cagent) agent.yaml."
---

# Write an adam agent as a folder

An adam agent with no Rust is a folder of files that the `adam-agent` binary reads once at
startup and serves over A2A. The binary ships inside the coder image
(`ghcr.io/vymalo/another-adam-rs/coder`), so a deployment needs the image and the folder, no build.

Every adam-rs path below is in `vymalo/another-adam-rs` at the revision you pin (replace `main` by
that revision). The reference is
https://github.com/vymalo/another-adam-rs/blob/main/bin/adam-agent/README.md; read it at your rev
before you rely on a detail here.

## When to use

* Adding an agent to a stack that already runs adam agents (a chat assistant, a researcher on a
  web-search MCP server, a reviewer).
* Changing an agent's persona, card, tools list, skills, subagents or MCP servers.
* Diagnosing a start that exits 78 or 69.
* Not for an agent that needs its own Rust tools: that is `adam-embed`. Not for Docker Agent
  (cagent) `agent.yaml`: a different product with a different format.

## Procedure

1. **Create `agent/instructions.md`.** The folder you point `ADAM_AGENT_DIR` at holds `agent/`
   (or is `agent/` itself) and exactly one agent: a folder with `agents/` of several is refused.
   Start from `dev/agents/assistant/agent/instructions.md` (a chat agent) or
   `dev/agents/researcher/agent/` (`instructions.md` plus `mcp.json`) in the repository.
2. **Frontmatter**, from the table in `bin/adam-agent/README.md` ("The agent folder"):
   * `name`: the registered name, which is the key of the agent's stored runs. Renaming it
     strands the runs of the old name. Two folders with the same `name` share their runs.
   * `description` (or `card.description`): the A2A card needs one.
   * `card:` (`name`, `skills` with `id`, `name`, `description`, `tags`, `examples`), `limits:`
     (`max_turns`, `max_tool_calls`, `max_output_tokens`, `max_history_tokens`), `model:` (an alias,
     only when it should not be `MODEL`), `tools:` (optional selection; `linear__*` takes a
     server's tools; an unknown name is refused with a suggestion).
   * `vars:` are the `{{placeholders}}` of the body. Every var needs a value in the file. A var
     declared without a value, a var the body does not use, or a placeholder not declared, stops
     the process at startup.
   * Secrets and endpoints never go in the file: the validator rejects `api_key`, `apiKey`,
     `token`, `secret`, `password` and `base_url` (`docs/reference/agent-files.md`, "File formats").
3. **Body**: the system prompt. Optional `skills/<name>/SKILL.md` (Agent Skills format, loaded on
   demand by `load_skill` and `read_skill_file`) and `subagents/` (each one tool of the parent, a
   child run with only the tools it lists and never `ask_user`; registered as `<name>/<subagent>`).
   A subagent file with `a2a: <agent-card URL>` is a **remote subagent** (another A2A agent): render the URL into
   the file, and give its token through `auth: bearer:VAR` (a variable of the process, from a Secret; unset is exit
   78). Plain `http` to a service of the same cluster needs `A2A_ALLOW_INSECURE_REMOTES=true` on `adam-agent`. A
   remote under a subagent's directory (`subagents/researcher/subagents/browser.md`) is that subagent's tool.
   `files: true` in a remote's file shares the file parts of its answer (a browser's screenshot) as files of the
   calling run; without it they are described and dropped. A subagent's files never reach the person.
   Formats: `docs/reference/agent-files.md`; the short version is `docs/guides/write-an-agent.md`.
4. **Built-in tools** the agent has without writing any: `ask_user`, `show`, `ui_catalog`
   (`crates/adam-ui/README.md`). Everything else comes from the folder, mainly MCP.
5. **MCP servers**: `agent/mcp.json` with `{"mcpServers": {...}}`; tools are named
   `<server>__<tool>`, `tools` in a server entry is the allow-list. Credentials go in `headers`
   as `${VAR}` (unset without default: exit 78 naming the variable). What kinds of server are
   allowed is the deployment's, not the file's:
   * `MCP_ALLOW_STDIO=true` for a local process (`command`); the coder image sets none.
   * `MCP_ALLOW_INSECURE=true` for plain `http` to another machine (development only).
   * `MCP_ALLOW_URL_VARS=true` to allow `${VAR}` inside a `url`.
   * `type: sse` is not supported.
   * `"optional": true` on a server: if it is down, refuses its credentials, lacks an allow-listed tool or its
     `${VAR}` has no value, it is skipped with a warning and the process starts without it (a required server
     that is down is exit 69). A header whose `${VAR}` is empty is an error (a skip when optional).
     Naming a skipped optional server's tools in the agent's `tools:` (`search__*`) makes the folder exit 78,
     so leave an optional server's tools out of `tools:` or make the server required.
   * `"files": true` on a server (a headless browser's screenshots and PDFs): each image, audio clip and blob
     of its results becomes a **file artifact of the run**, the shape the coder's `share_file` gives (one A2A
     `raw` part with `mediaType` and `filename`), named `<tool>-<n>.<ext>`; the model reads
     `Shared browser_screenshot-1.png (84.0 KiB, image/png).` and, for an image, the Markdown that shows it
     inline by its file name (tell an agent that shows images to use the file's name, never a path).
     At most 4 MiB a file, 6 MiB a run; a subagent's
     files stay on its own run. Without the key a file is described and dropped. Rules:
     `crates/adam-mcp/README.md` ("Files"), `docs/decisions/0033-files-from-mcp-results-are-shared-files.md`.
   * **More servers without copying the folder**: `ADAM_EXTRA_MCP_FILE=/path/mcp.json`, a file in the same
     shape, is added to the folder's own `mcp.json` at startup by the roles that run workers (`adam-agent` and
     `adam-coder`, which has the shipped `github` server in its embedded copy). A name the folder already has
     is exit 78 and nothing is replaced. Every `${VAR}` a `mcp.json` names is treated as a secret of the
     process: `adam-coder` hides it from the processes of runs and redacts its value, `adam-agent` scrubs it
     from the steps.
6. **Make it readable by uid 10001** (the image's user) and mount it read-only. Run the image with
   the entrypoint overridden (the coder's entrypoint is `adam-coder`):

   ```sh
   docker run --rm --entrypoint tini \
     -v "$PWD/my-agent/agent:/etc/adam/agent:ro" -e ADAM_AGENT_DIR=/etc/adam/agent \
     -e DATABASE_URL=... -e MODEL_BASE_URL=... -e MODEL_API_KEY=... -e MODEL=... \
     -e A2A_BEARER_TOKENS=... -e PUBLIC_URL=... -p 8080:8080 \
     ghcr.io/vymalo/another-adam-rs/coder:sha-<7> -- adam-agent
   ```

   Pin the image by `sha-<7>` tag and digest (`adam-upgrade`). `A2A_BEARER_TOKENS` and `PUBLIC_URL`
   are required by roles that serve A2A (fail closed: no token, no server); `MODEL_*` by
   roles that run workers. All variables: "Configuration" in `bin/adam-agent/README.md`.
7. **In a compose stack**, copy the service `agent` of `compose.yaml` in the repository (profile
   `app`: the folder mounted at `/etc/adam/agent`, `AGENT_FOLDER` to choose it, port from
   `AGENT_PORT`, the coder's database). A new agent is a folder and about a dozen lines of
   that service.
   On Kubernetes the folder can be the `agent.folder` of an `AgentConfig`, served by an `AgentService` of the
   adam-rs operator: no manifest to write by hand (`adam-operator`).
8. **Edits apply at the next start** (ADR 0004,
   `docs/decisions/0004-agent-folders-at-run-time.md`): restart the service, no rebuild. There
   is no hot reload in release images.

## Verify

* **Exit codes**: 78 is configuration (a missing `ADAM_AGENT_DIR`, a bad file, a `vars` mistake,
  an unset `${VAR}`, a refused MCP policy); every finding is listed as `path:line: error: ...`
  in the one `adam-agent failed` log line. 69 is a dependency that is down (Postgres, an MCP
  server): a supervisor can restart until it is up.
* **Startup logs** one `agent files` line (`path`, `digest`, `agent`, `warnings`) and each
  warning as `path:line: warning: ...`: read it to see which files run.
* **The card** is public at the agent's URL and is the folder's; a call without a bearer token
  gets 401.
* **End to end in the repository's stack**: `sh dev/agent-e2e.sh` (the card, "hi" answered in
  role, then a restart on an edited copy of the folder). The container smoke test is
  `sh docker/coder/test/agent-smoke.sh <image>` (`AGENT_FOLDER` names an `agent/` folder).

## Pitfalls

* `ADAM_AGENT_DIR` has no default and the binary has no embedded agent: unset is exit 78.
* The folder's `name` is the key of the runs: a rename is a new agent with no history.
* A subagent inherits nothing and, without `tools:`, has no tools (`docs/reference/agent-files.md`).
* `${VAR}` in agent frontmatter does not exist; it is only for `mcp.json`.
* One agent per process. Several agents share one database by running several processes with
  different folders (runs are scoped by the agent's name).
* The coder image sets no `MCP_ALLOW_STDIO`, and has no extra interpreters for stdio MCP
  servers that need `node` or `python`.
* A folder that follows the persona convention (the body opens with `Your name is {{display_name}}.`
  and a line `In one sentence: <summary>.`) is greeted by the repository's mock models in role.
  A live model follows the whole prompt.

## See also

* `bin/adam-agent/README.md`, `docs/guides/write-an-agent.md`, `docs/reference/agent-files.md`, `crates/adam-mcp/README.md` (MCP rules),
  `crates/adam-ui/README.md` (the built-in tools).
* `adam-embed` (agents with Rust tools), `adam-a2a-extensions` (what the card announces),
  `adam-upgrade` (changing the image pin), `adam-operator` (serving the folder on Kubernetes).
* https://github.com/vymalo/another-adam-rs/blob/main/bin/adam-agent/README.md
