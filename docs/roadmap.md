# Roadmap

| # | Item | State |
|---|---|---|
| 1 | `Store` trait and its adapters (`adam-core`, PostgreSQL, MongoDB) | built |
| 2 | Run state machine and `ctx.step` journaling (`adam-runtime`) | built |
| 3 | `#[tool]` macro (`adam-macros`, through the `adam` facade) | built: [Agent files](reference/agent-files.md#the-tool-contract) |
| 4 | `build.rs` discovery of `agent/` (instructions, skills, subagents, `mcp.json`) | built: parser and validator (`adam-agent-fs`), binding to `LlmAgent`s (`adam-assembly`), subagents as child runs (local and remote A2A), dev reload, `mcp.json` tools, run-time folders (`ADAM_AGENT_DIR`) |
| 5 | Parking, approvals, schedules | partly: parking and `ask_user` are built; approvals (`approval:`) and running schedules are not (schedule files are read and warned about) |
| 6 | Dev TUI (`cargo adam dev`) | not started |
| 7 | Host adapters (axum/tower), channels, sandboxes | not started |

Known gaps recorded in the docs: a pinned run whose worker never returns is stranded
([Architecture](architecture.md#where-a-runs-files-and-processes-live)); cancelling a parent does not cancel its
children and subagent calls of one turn run one after the other ([Child runs](reference/child-runs.md)); the
chart has not been applied with more than one worker ([Deploy](guides/deploy-the-coder.md#known-risks)).
