# adam-agent-fs

Parse and validate agent directories. An agent is a directory of Markdown and JSON
(`agent/instructions.md`, `skills/`, `subagents/`, `mcp.json`, `schedules/`); this crate reads
it into an owned `AgentManifest` and reports every mistake as a `Diagnostic` with the file and
the line. The layout, the file formats and the reasons are in
[`docs/authoring.md`](../../docs/authoring.md).

## Where it sits

The **files half of the authoring layer** (slice S4). It is a leaf: `serde`, `serde_json`,
[`serde-saphyr`](https://crates.io/crates/serde-saphyr) (YAML 1.2, deserialize only), `url`,
`thiserror` and [`adam-error`](../adam-error/README.md). No async, no `tokio`, no adam runtime
crate, and it never expands `${VAR}` or reads a secret. The `build.rs` codegen (S5), the binding
to `LlmAgent` (S6) and the MCP client (S11) build on it and are not here yet.

`ManifestSource` is the seam between where the files are and what they mean. `Dir` reads a
directory; an embedded source (a generated static manifest) and a remote one can sit next to it.
Its signature has only manifest types and diagnostics: no `walkdir`, `notify` or `std::fs` type.
Directory walking uses `std::fs`, so there is no `walkdir` dependency either.

## API at a glance

| Item | What |
|---|---|
| `ManifestSource` (trait), `Dir` | `load() -> Result<Report, Error>`. `Dir::new(root)` reads `root/agent/` (one agent) or `root/agents/<name>/` (several); `.optional()` accepts neither, `.default_name(n)` names a root agent whose frontmatter has no `name` (a build script passes `CARGO_PKG_NAME`) |
| `Report`, `Package`, `Layout` | `report.package.agents`, `report.diagnostics`, `errors()`, `warnings()`, `is_ok()`, `into_package(Strictness)` |
| `Diagnostic`, `Severity` | `{ severity: Error \| Warning, path, line: Option<u32>, message }`; `Display` is `path:line: severity: message` |
| `Strictness` | `Lenient` (only errors fail) or `Strict` (warnings fail too) |
| `AgentManifest` | `name`, `path`, `frontmatter`, `instructions` (`body`, `parts`, `prompt()`), `skills`, `subagents`, `mcp`, `schedules` |
| `Subagent` | `Local(Box<AgentManifest>)` or `Remote(RemoteAgent)` (`a2a:` URL, `RemoteAuth::Bearer { env }`) |
| `Skill`, `SkillLayout` | `name`, `description`, `license`, `compatibility`, `metadata`, `allowed_tools`, `body`, `resources` (paths, not contents) |
| `Schedule` | `name` (`a/b` from the path), `cron`, `timezone`, `agent`, `prompt` |
| `AgentFrontmatter`, `Limits`, `Card`, `ToolList`, `ModelRef`, `SkillSelection` | the shared agent and subagent schema; unknown keys are kept in `extra` |
| `SkillFrontmatter`, `ScheduleFrontmatter` | the other two YAML schemas |
| `McpConfig`, `McpServer`, `RemoteKind`, `EnvRef` | `mcp.json`; `env_references()` lists the `${VAR}` names, never values |
| `split(text)` | the frontmatter splitter: `Split { frontmatter, body, .. }` or `SplitError::Unterminated` |
| `parse_skill`, `parse_mcp` | the pure text-to-value parsers, for callers that hold text and not a directory |
| `is_agent_name`, `is_skill_name`, `is_tool_name`, `is_env_name` | the name patterns |
| `Error` | `Io { path, source }` (the source could not be read) and `Invalid { diagnostics }` (`into_package` refused); implements `adam_error::Classify` (`NotFound`, `Internal`, `Invalid`) |

```rust
use adam_agent_fs::{Dir, ManifestSource, Strictness};

let report = Dir::new(env!("CARGO_MANIFEST_DIR")).default_name(env!("CARGO_PKG_NAME")).load()?;
for finding in &report.diagnostics {
    eprintln!("{finding}"); // agent/skills/pdf/SKILL.md:2: warning: `name: pdf-tools` does not match ...
}
let package = report.into_package(Strictness::Lenient)?; // Err when there is an error
```

A problem in the files is a diagnostic, not an `Err`: one load reports every mistake. An item
with an error is left out of the package (a skill without a description, a subagent that does not
parse), so a package next to errors is what *could* be read, not something to run.

## What is checked

| File | Errors (the item is skipped or the build fails) | Warnings (kept) |
|---|---|---|
| directory | `agent/` and `agents/` together; neither (unless `.optional()`); no `instructions.md`; `agents/<name>` whose `name:` differs; two subagents or skills with one name; an empty prompt; a file that is not UTF-8 | an entry that is not part of an agent directory (`tools/`, ...); `agents/<x>` without `instructions.md`; `schedules/` in a subagent |
| frontmatter | a `---` first line with no closing `---`; YAML that does not parse (line reported); `api_key`, `apiKey`, `token`, `secret`, `password`, `base_url` ("secrets and endpoints belong in the environment"); a `model` that is a URL; an invalid `vars` key; `a2a` on the root agent, a bad `a2a` URL or `auth`; a subagent with no `description` or no body | unknown keys; Claude Code and Copilot keys adam ignores (`color`, `permissionMode`, `target`, ...); `mcpServers` / `mcp-servers` (use `mcp.json`); a `name` that is not a valid identifier (the file name is used, lower-cased when needed); tool names adam cannot bind (`Read`); `maxTurns` disagreeing with `limits.max_turns`; a prompt over Copilot's 30,000 characters |
| `SKILL.md` | no `description`; unparseable YAML or an unterminated frontmatter | `name` different from the directory, missing, or breaking the name rule; a description over 1024 or `compatibility` over 500 characters; a flat skill without frontmatter (its first line becomes the description) |
| `mcp.json` | invalid JSON; a `url` without `type`; an unsupported `type`; `command` and `url` together; a key such as `apiKey`; a server or allow-listed tool name that cannot become `<server>__<tool>` within 64 characters | unknown keys; a header, environment value or URL that carries a literal credential (`Bearer sk-...`, `X-Api-Key: ...`, `user:pw@`); a `${` that is not a reference |
| schedule | no `cron`, not five fields, a bad time zone, no body, `agent:` naming another agent | unknown keys |

The Agent Skills rules follow the spec's client guide (lenient: a name mismatch or an over-long
field warns; a missing description or bad YAML skips the skill). Skipping is an *error* here
because these files are our own source, not a third-party install.

Formats accepted unchanged: a Claude Code agent (`.claude/agents/x.md`) and a GitHub Copilot
custom agent (`.github/agents/x.agent.md`) read as subagents when copied into `subagents/`. The
`.agent.md` suffix is dropped from the name. Claude's `maxTurns` is read as
`limits.max_turns`; `tools` is a list or a comma-separated string.

## Tests

`cargo test -p adam-agent-fs`:

* `tests/rules.rs`: one directory per rule (75), each producing exactly one diagnostic of
  the stated severity; the valid fixture (`tests/fixtures/valid`) with none; ordering, ignore
  rules, symbolic links, non-UTF-8 files.
* `tests/conformance.rs`: all 75 vendored `.agents/skills/*/SKILL.md` of this repository parse
  with no error and no warning; a Claude Code agent and two Copilot agents
  (`tests/fixtures/{claude,copilot}-agents`) parse unchanged as subagents.
* `tests/splitter.rs` (proptest): the splitter and the parsers never panic on arbitrary text; an
  unclosed `---` is always an error; the split loses no byte.
* Unit tests next to the code: the splitter, name patterns, `${VAR}` scanning, cron.
