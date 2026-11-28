---
name: adam-agent-folder
description: "Write or change an adam-rs agent that is only a folder (agent/instructions.md, card, vars, tools, skills/, subagents/, commands/, mcp.json) and serve it with the adam-agent binary from the coder image. Use for 'add an agent', 'change an agent's instructions or MCP servers', 'why does adam-agent exit 78'. Not Docker Agent (cagent) agent.yaml."
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
   * `card:` (`name`, `skills` with `id`, `name`, `description`, `tags`, `examples`), `limits:` (`max_turns`, `max_tool_calls`, `max_output_tokens` — the cap this agent asks for, at least 1;
     absent, its preset's, the model's profile's, then 128,000, clamped to the model's limit and
     window, ADR 0064; a `0` stops the load, naming the key — `max_history_tokens`, and for commands `max_processes`, `command_memory` and `command_cpu`, which only lower the deployment's `MAX_PROCESSES`, `COMMAND_MEMORY_MAX` and `COMMAND_CPU_MAX` and are read by `adam-coder`, and `output` (root only, read by `adam-coder`: `{ run_checks, run, run_command }`, each `on_error` or `full`, the default of a call that gives no `output`; another key or word is exit 78; ADR 0090); see `docs/reference/agent-files.md` at https://github.com/vymalo/another-adam-rs/blob/main/docs/reference/agent-files.md), `model:` (an alias,
     only when it should not be `MODEL`), `model_fallback:` (overrides the `fallback` of the agent's `model_preset` and of
     its model's profile in `MODEL_ALIASES_FILE`: the alias the run continues on, once, when the endpoint refuses
     `model` as locked; not the agent's own alias, else exit 78; a subagent resolves its own, so an `inherit` subagent
     takes its model's profile fallback and needs no key; ADR 0064 amended 2026-11-23),
     `si_memory:` (`off`, `on`, or a map of `recall`, `record`, `preload_kib`, `scopes`: the agent's
     self-improvement memory, ADR 0081; the deployment's `SI_MEMORY=off` beats it, an unknown or secret-looking
     option is a load error, and `memory:` is a different, ignored key),
     `reasoning_effort:` (one of `none`, `minimal`, `low`, `medium`, `high`, `xhigh`, `max`) and
     `model_preset:` (a name in `MODEL_ALIASES_FILE`'s `presets`): the level this agent asks for and
     the preset it starts from. The effort resolves folder, its preset, the model's profile, then the
     profile's base preset (ADR 0063), and the preset also carries the output cap and the fallback
     (ADR 0064). The assembly refuses what it cannot honour, by name: an unknown preset
     (`UnknownModelPreset`), a word that is not one of the seven (`InvalidReasoningEffort`), a level
     the model's profile does not take (`ReasoningEffort`), and a preset whose cap asks for more
     output tokens than the model documents (`OutputCap`). `tools:` (optional selection; `linear__*` takes a
     server's tools; an unknown name is refused with a suggestion), `tools_loading:` (`eager` offers every tool in every request, a
     server's `when` included; `lazy`, the default, starts lean and loads tools in groups; `searchable` behaves as `lazy` until
     searchable groups land; ADR 0066).
   * **A model per message is the deployment's offer, not a folder key.** When `MODEL_ALIASES_FILE` profiles more than one
     model, a served `ROLE=all` agent lists `model-pick/v1` with the aliases it may be bound to, other than its own, each
     as `{"id": "<alias>", "endpoint"?: "<label>"}` — the endpoint label coming from that alias's profile's `endpoint`,
     a label only, absent when the deployment names none — and a message that activates it names one
     (`metadata[URI] = {"model": ..}`); an alias outside the list is refused as
     `model_not_offered` before anything runs (ADR 0064, amended 2026-11-23; the card field is ANR-623; the client side is
     `adam-a2a-extensions`).
   * `vars:` are the `{{placeholders}}` of the body. Every var needs a value in the file. A var
     declared without a value, a var the body does not use, or a placeholder not declared, stops
     the process at startup.
   * Secrets and endpoints never go in the file: the validator rejects `api_key`, `apiKey`,
     `token`, `secret`, `password` and `base_url` (`docs/reference/agent-files.md`, "File formats").
   * `plugins:` names **compiled hooks the deployment registered**, never a program: each entry is a
     name or `{name, options}` of scalars, and it must be one the serving binary or the embedding host
     registered (`AgentDef::plugin`, or `plugin_with` when the entry's options decide how the plugin is
     made). A name nobody registered stops startup (exit 78), a subagent runs its parent's plugins and
     then its own, and an option whose name looks like a secret is refused. **No shipped binary
     registers one yet** (*verified 2026-11-27*), so `plugins:` in a folder `adam-agent` serves is exit
     78: `adam-plugin`'s `Audit` (one JSON line per observed event) and the `adam-hooks/1` program
     adapter (`adam-hook-exec`) are for a host that registers them. `hooks:` stays an ignored key.
3. **Body**: the system prompt. Optional `skills/<name>/SKILL.md` (Agent Skills format, loaded on
   demand by `load_skill` and `read_skill_file`, which take a plain string name and page; a folder with no `skills:` has all of them, `skills: []` none) and `subagents/` (each a child run with only the tools it lists,
   registered as `<name>/<subagent>`). **A subagent is no tool** (ADR 0089 slice C4): an agent with subagents gets the
   six session tools, and the instructions tell the model to use `subagent_start` (`agent`, `description`, `prompt`,
   `wait: true` to have the answer as the result). A `tools:` that names a subagent, the agent's or a mode's, is exit 78
   with a message naming `subagent_start`; a mode that lists `subagent_*` keeps them.
   A subagent file may set `session:` (`mode`: `one_shot`, `sealed` or `open`; `wait`: `true` or `false`): what a
   `subagent_start` that names none gets, and `mode` is also the widest a call may ask (a call only narrows it; asking
   wider is an error result naming both). Another key or word is exit 78; on the root it is a warning and ignored
   (ADR 0089 slice C3; `docs/reference/agent-files.md`).
   A root with subagents also gets the six session tools (`subagent_start` .. `subagent_list`) and a fixed *Child sessions* section at the end of its prompt, after the skills catalog (the tools, the helpers sorted, what `config` takes): do not write a paragraph about them in the instructions.
   A subagent file with `a2a: <agent-card URL>` is a **remote subagent** (another A2A agent): render the URL into
   the file, and give its token through `auth: bearer:VAR` (a variable of the process, from a Secret; unset is exit
   78). Plain `http` to a service of the same cluster needs `A2A_ALLOW_INSECURE_REMOTES=true` on `adam-agent`, and
   then the bearer crosses the pod network in clear text: add a NetworkPolicy or mesh mTLS. A
   remote under a subagent's directory (`subagents/researcher/subagents/browser.md`) is that subagent's (it gets the
   session tools). A file in a remote's answer (a browser's screenshot) is described in a line and dropped; `files: true`
   has no effect since C4 (a warning). A subagent's files never reach the person.
   Optional `commands/<name>.md` are prompt templates the agent serves (`description`, `argument-hint`, a body with `$ARGUMENTS` and `$1` to `$9`; at most 50, `$$` is a literal `$`, never executed, any `!` right before a backtick is refused; a file is `*.md`, regular, at most 40 KiB): the card lists them (`commands/v1`) and a message `/name arguments` or its metadata invokes one; a subagent's `commands/` is ignored.
   Optional `modes/<name>.md` are the modes a person picks **for one message** (`title`, `description`, `default: true` on exactly one, `tools:` — an allowlist over the agent's own, absent: no narrowing — and a body added to the instructions while it is in force; at most 16; a name the agent does not have is left out with a warning, a pattern that matches nothing is exit 78, and a `permission:` key is **refused** until ADR 0039 decision 4 is built). The card lists them (`agent-mode/v1`: `params {modes: [{name, title, description}], default}`) and a message that activates the extension names one (`metadata[URI] = {"mode": ..}`); an unknown one is refused as `mode_not_offered` before anything runs, and the choice holds until another message names another. **A mode only narrows**: the access tier still caps every tool it allows, and a mode can neither raise the tier nor offer a tool the agent does not have. A subagent starts in its parent's mode. A folder with no `modes/` lists no extension and works as before (ADR 0039 decision 3; the client side is `adam-a2a-extensions`).
   `references:` in the frontmatter of the root `instructions.md` (a list of `name`, `url`, optional `branch`, `description`;
   at most 16; no credentials in the `url`) names repositories the **coder** may read beside the one it works on and never
   write; a deployment adds more with `ADAM_REFERENCES_FILE` (a name both declare is exit 78) and `adam-agent` has no
   workspace to read them in. A subagent's `references:` is ignored.
   Formats: `docs/reference/agent-files.md`; the short version is `docs/guides/write-an-agent.md`.
4. **Built-in tools** the agent has without writing any: `ask_user`, `show`, `ui_catalog` and the plan,
   `plan_work`, `plan_check`, `plan_add` (`crates/adam-ui/README.md`; a checklist of at most 50 items kept in the
   conversation's state: tell the agent to plan work of three or more steps; a folder with `tools:` must list `plan_*`
   to keep it; a subagent has no plan). Everything else comes from the folder, mainly MCP. Tell the agent in its
   instructions to offer options with `ask_user` and `choices` (a `title`, a `description` of why it asks,
   options that each have a `label` and a `description`) and never as a list in its reply: a prose list gets
   answers like "yes, add that" that cannot be told apart (`docs/decisions/0035-questions-carry-a-title-a-description-and-typed-options.md`).
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
   * `"when": {"keywords": ["linear", "ticket"]}` on a server: it becomes one **group** of tools, named after
     the server, loaded by one generated tool `load__group__<server>` (its schema is a constant). The server's
     name becomes the group's name, so it is lowercase letters and digits with single underscores, at most 40
     characters: `my-linear` and `MyLinear` are refused at load, naming the server, and nothing is renamed. Its tools are
     **withheld** from a run whose task (the words of the first message) mentions none of the keywords, and the
     server's instructions are the group's note: said in the load's result for an explicit load, and in the first
     prompt for a group the task's keywords load. A successful call
     makes the group's tools available from the **next** model call, **appended** after the tools the model
     already had (never inserted), at the price of one full re-prefill of the prompt; the run's state records
     the group and the turn the tool list grew. A group with no tool the agent's `tools:` allows has **no
     loader**, and a call to a tool of an unloaded group is refused. The loaders reserve the word `load`: a
     server named `load` is refused at start, and a registered tool whose name starts with `load__` is refused
     at bind. Rules: `docs/reference/agent-files.md` ("A server loaded only when the task asks"), code
     `crates/adam-assembly/src/groups.rs`.
   * `"files": true` on a server (a headless browser's screenshots and PDFs): each image, audio clip and blob
     of its results becomes a **file artifact of the run**, the shape the coder's `share_file` gives (one A2A
     `raw` part with `mediaType` and `filename`), named `<tool>-<hash>.<ext>`; the model reads
     `Shared browser_screenshot-3fa2c19b.png (84.0 KiB, image/png).` and, for an image, the Markdown that shows it
     inline by its file name (tell an agent that shows images to use the file's name, never a path) and its
     SHA-256, which names it as the picture of an `ask_user` option and in a `show` `Image` block.
     At most 4 MiB a file, 6 MiB a run; a subagent's
     files stay on its own run. Without the key a file is described and dropped. Rules:
     `crates/adam-mcp/README.md` ("Files"), `docs/decisions/0033-files-from-mcp-results-are-shared-files.md`.
   * Pictures for the model are the **deployment's** word, not a key of `mcp.json`: with
     `MODEL_INPUT_MODALITIES=text,image` and `mcp` in `MODEL_IMAGE_SOURCES` (default `person,workspace`: a server's
     call can carry credentials, and an image is never scrubbed) an image block of a result is also offered to the
     model, within `MODEL_IMAGE_MAX_BYTES`, `_COUNT` and `_SIDE`; with `files: true` it is shared too, with the same
     SHA-256. Otherwise it is `[image not included: image/png]`. An agent folder keeps the bytes in the run's state
     (4 MiB with its long outputs). `docs/reference/agent-files.md` ("Pictures for the model").
   * `"timeoutSecs"` and `"connectTimeoutSecs"` on a server (whole seconds from 1; 60 and 30 without them) are its
     own call and connection timeouts; the deployment caps the effective value, defaults included, with
     `MCP_MAX_CALL_SECS` (900), so a larger one is lowered with a warning, and `0` or a negative number refuses
     the server. `"instructions": false` leaves out
     the text a server returns when it initializes, which is otherwise appended to the agent's instructions once,
     at startup, under a heading that says it comes from the server, as a quoted block. `"prompts": false` stops a server's prompts
     from becoming commands (`<server>__<prompt>`, on the card; the last argument takes the rest of the line; a command of the folder with that name wins; the folder's commands and the prompts are at most 50 together; not in a split front).
     Rules: `docs/reference/agent-files.md` ("Timeouts, instructions and prompts").
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
   operator in vymalo/another-agentic-platform: no manifest to write by hand
   (https://github.com/vymalo/another-agentic-platform/blob/main/deploy/operator/README.md).
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
  `adam-upgrade` (changing the image pin).
* https://github.com/vymalo/another-adam-rs/blob/main/bin/adam-agent/README.md
