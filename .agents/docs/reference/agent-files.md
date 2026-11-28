# Agent files: the decisions to keep

For people: [Agent files and `#[tool]`](../../../docs/reference/agent-files.md). This page holds the owner's decisions
the format rests on. A change to the folder format, its validation or `#[tool]` is checked against them; changing one
is an ADR.

## Decisions (accepted by the owner, 2026-09-29)

The owner accepted D1 to D6 as recommended, and added one rule: **keep the structure of eve.dev, GitHub
Copilot and Claude Code for agents and subagents.** The layout is eve's; the file format is the Claude
Code / Copilot custom agent; `.agent.md` is accepted and the name is the stem without `.agent`; unknown
Claude or Copilot keys are accepted with a warning.

| | Decision | Rejected alternative |
|---|---|---|
| D1 | Configuration is YAML frontmatter, not `agent.toml`: one schema shared with Claude Code, Copilot, OpenCode and Agent Skills | TOML: Rust-native, but no agent convention |
| D2 | Tools live in `src/` with an explicit `tools![]`, not `agent/tools/*.rs` auto-discovery | included files are invisible to `cargo fmt` |
| D3 | A subagent that omits `tools:` gets none | inherit all: Claude-compatible, surprising for a durable backend |
| D4 | Unknown frontmatter keys and Agent Skills violations warn (`.strict()` opts in to errors); a missing `description` always fails | fail the build on any |
| D5 | One tool per subagent, named after it. *Amended 2026-11-28* ([ADR 0089](../../../docs/decisions/0089-every-child-is-a-session-and-the-one-shot-subagent-tool-goes.md) slice C4): no tool is named after a subagent; every agent with subagents gets the six session tools, and the one-shot call is `subagent_start` with `mode: one_shot`, `wait: true`; a `tools:` that names a subagent is refused at load (`names_a_subagent`, `crates/adam-agent-fs/src/source.rs`) | a single `delegate { agent, message }` tool |
| D6 | The `dev` feature ships in the facade and is off by default | on by default |

Also confirmed: adam-rs domain types are **closed enums** (no `#[non_exhaustive]` unless an existing
type already sets the pattern).

## `plugins:`: what to check

The key is read (`crates/adam-agent-fs/src/load/agent.rs`, `check_plugins`: a name of `a-z`, `0-9`, `-`,
`_` and at most 64 characters, no duplicates, scalar options only, and no option name that looks like a
secret) and bound by the assembly (`AgentDef::plugin` / `plugin_with`,
`crates/adam-assembly/src/def.rs`; a name nobody registered is `Error::UnknownPlugin`, exit 78 in a
binary). A subagent's plugins are its parent's and then its own (ADR 0050 decision 8), and `hooks:` stays
an ignored key: an agent file never names a program.

**Check before documenting a `plugins:` example:** does the binary you are writing about register the
name? None of `bin/` does today (*verified 2026-11-27*: no `Plugin`, `HookSet` or `.plugin(` anywhere in
`bin/`), so a folder served by `adam-agent` or `adam-coder` exits 78 on any entry, and the only
in-tree plugin type, `adam-plugin`'s `Audit`, is registered by a host. When a binary starts registering
one, this page, `docs/reference/agent-files.md`, `docs/guides/write-an-agent.md`, the
`adam-agent-folder` skill and `docs/reference/parity.md` row 28 all change together.
