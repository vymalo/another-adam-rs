# Concepts: the evidence

For people: [Concepts](../../docs/concepts.md). This page gives, for each term of its glossary, where the code defines it.
Checked 2026-11-26 against `main` at `83cf1ef5`, by reading the files (no build ran for this page).

| Term | Definition | Line |
|---|---|---|
| Run | `RunRecord` | `crates/adam-core/src/store/mod.rs:371` |
| Journal | `JournalEntry` | `crates/adam-core/src/store/mod.rs:484` |
| Step | `Ctx::step` (`step_retrying`, `step_all`, `step_race` beside it) | `crates/adam-runtime/src/ctx.rs:297` |
| Store | `trait Store`; `commit_run` 747, `journal_put` 774 | `crates/adam-core/src/store/mod.rs:636` |
| Lease | `Store::claim_due` 803 and `Store::lease_until` 853 | `crates/adam-core/src/store/mod.rs` |
| Store testkit | `store_conformance!` | `crates/adam-store-testkit/src/lib.rs:49` |
| Notifier | `trait Notifier` | `crates/adam-runtime/src/notify.rs:68` |
| Notifier testkit | `notifier_conformance!` | `crates/adam-notify-testkit/src/lib.rs:66` |
| Role | `enum Role` | `crates/adam-host/src/role.rs:21` |
| Placement | `enum Placement` | `crates/adam-host/src/placement.rs:23` |
| The whole process | `serve` | `crates/adam-service/src/serve.rs:735` |
| Agent folder | `AgentDef` | `crates/adam-assembly/src/def.rs:154` |
| Skill | `Skill` (parsed), `SkillFiles` (run time) | `crates/adam-agent-fs/src/manifest.rs:36`, `crates/adam-assembly/src/skills.rs:50` |
| Subagent | `SubagentTool` | `crates/adam-assembly/src/subagent.rs:82` |
| `#[tool]` | `pub fn tool` (proc-macro attribute) | `crates/adam-macros/src/lib.rs:67` |
| TaskBackend | `trait TaskBackend` | `crates/adam-a2a/src/backend.rs:276` |
| Extension | `ExtensionConfig`; the URIs are constants in `extensions.rs` | `crates/adam-a2a/src/card.rs:190` |

Two terms name no ADR of their own, the lease and `#[tool]`; their glossary rows link the page that specifies them
(`docs/architecture.md`, `docs/reference/agent-files.md`) rather than borrow an ADR that does not decide them. The line
numbers drift: re-check with `grep -n` before relying on one.
