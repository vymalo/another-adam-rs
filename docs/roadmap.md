# Roadmap

> The product roadmap to launch, with its milestones and issues, is in
> [`ROADMAP.md` of another-agentic-system](https://github.com/vymalo/another-agentic-system/blob/main/ROADMAP.md).
> The table below is this library's own roadmap.

| # | Item | State |
|---|---|---|
| 1 | `Store` trait and its adapters (`adam-core`, PostgreSQL, MongoDB) | built |
| 2 | Run state machine and `ctx.step` journaling (`adam-runtime`) | built |
| 3 | `#[tool]` macro (`adam-macros`, through the `adam` facade) | built: [Agent files](reference/agent-files.md#the-tool-contract) |
| 4 | `build.rs` discovery of `agent/` (instructions, skills, subagents, `mcp.json`) | built: parser and validator (`adam-agent-fs`), binding to `LlmAgent`s (`adam-assembly`), subagents as child runs (local and remote A2A), dev reload, `mcp.json` tools, run-time folders (`ADAM_AGENT_DIR`) |
| 5 | Parking, approvals, schedules | partly: parking and `ask_user` are built; approvals (`approval:`) and running schedules are not (schedule files are read and warned about) |
| 6 | Dev TUI (`cargo adam dev`) | not started |
| 7 | Host adapters (axum/tower), channels, sandboxes | not started |
| 8 | The A2A 1.0 server, complete | built, [ADR 0030](decisions/0030-a2a-push-notifications-list-tasks-extended-card-signatures.md): push notifications (durable, at least once, off unless a webhook allow-list is set), `ListTasks`, `GetExtendedAgentCard`, signed cards. Not done: encrypted webhook credentials, signed notifications (the specification asks for none), per-caller extended cards |
| 9 | A coder as good as OpenCode, and above: paged tools, a stable prompt prefix, a plan, skills, commands and project context ([ADR 0044](decisions/0044-paged-tools-a-stable-prompt-prefix-a-plan-skills-commands-and-project-context.md), 16 slices) | partly, **10 of 16 on 2026-11-15**: built 1 (the loop survives bad model output), 2 (paged reads), 3 (command output kept whole and read back by page, a timeout per command, large MCP results paged), 4 (shortenings recorded once in the state, source tools in name order, the prefix test, `MODEL_PROMPT_CACHE_KEY`), 5 (OpenCode removed), 6 (`plan_work`, `plan_check`, `plan_add`), 10 (the repeated-call rule, time limits, a cancel that reaches children), 8 (skills on by default: `load_skill` and `read_skill_file` take a string name and page, `RunSkills`, a repository's skills listed by `prepare_workspace`, the coder's `discover-checks` and `pull-request-text`), 7 (project instructions: `AGENTS.md`, else `CLAUDE.md`, else `README.md` in the result of `prepare_workspace`, and a nested `AGENTS.md` or `CLAUDE.md` before the first read below it), 12 (commands: `commands/<name>.md`, the `commands/v1` extension, `/name` or metadata expanded by the agent; the coder's `review`, `fix-checks` and `explain`). Not started: 9 (compaction), 11 (more kinds of question), 13 (references), 14 (MCP timeouts, instructions and prompts), 15 (images in and out), 16 (commands that keep running) |
| 10 | A subagent in the background, as a session ([ADR 0045](decisions/0045-a-subagent-can-run-in-the-background-as-a-session.md), 8 slices) | partly, **2 of 8 on 2026-11-15**: built 1 (the runtime: a continuing child, the report notice, the session listing) and 2 (the five `subagent_*` tools, the turn-end wait, tokens counted once). Not started: 3 (reports during a turn), 4 (limits), 5 (remote sessions), 6 (the `child-sessions/v1` extension), 7 (the coder's own sessions), 8 (asking from a child) |

Known gaps recorded in the docs: a pinned run whose worker never returns is stranded
([Architecture](architecture.md#where-a-runs-files-and-processes-live)); subagent calls of one turn run one after the other
([Child runs](reference/child-runs.md)); a cancel does not reach a remote A2A task a run started
([Child runs](reference/child-runs.md#cancel-cascades-and-time-limits)); the
chart has not been applied with more than one worker ([Deploy](guides/deploy-the-coder.md#known-risks)).
