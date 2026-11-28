# Concepts: the evidence

For people: [Concepts](../../docs/concepts.md). This page gives, for each term of its glossary, where the code defines it.
Checked 2026-11-28 against `main` at `024ba0f5` plus this branch, by reading the files (no build ran for this page).

| Term | Definition | Line |
|---|---|---|
| Run | `RunRecord` | `crates/adam-core/src/store/mod.rs:371` |
| Journal | `JournalEntry` (`signature` is the runtime's `adam-step/v1` signature) | `crates/adam-core/src/store/mod.rs:484` |
| Step | `Ctx::step` (`step_retrying`, `step_all`, `step_race` beside it) | `crates/adam-runtime/src/ctx.rs:328` |
| Store | `trait Store`; `commit_run` 757, `journal_put` 785 | `crates/adam-core/src/store/mod.rs:646` |
| Lease | `Store::claim_due` 814 and `Store::lease_until` 864 | `crates/adam-core/src/store/mod.rs` |
| Store testkit | `store_conformance!` | `crates/adam-store-testkit/src/lib.rs:50` |
| Notifier | `trait Notifier` | `crates/adam-runtime/src/notify.rs:68` |
| Notifier testkit | `notifier_conformance!` | `crates/adam-notify-testkit/src/lib.rs:66` |
| Role | `enum Role` | `crates/adam-host/src/role.rs:21` |
| Placement | `enum Placement` | `crates/adam-host/src/placement.rs:23` |
| The whole process | `serve` | `crates/adam-service/src/serve.rs:797` |
| Agent folder | `AgentDef` | `crates/adam-assembly/src/def.rs:189` |
| Skill | `Skill` (parsed), `SkillFiles` (run time) | `crates/adam-agent-fs/src/manifest.rs:43`, `crates/adam-assembly/src/skills.rs:50` |
| Subagent | `SubagentTool` | `crates/adam-assembly/src/subagent.rs:87` |
| `#[tool]` | `pub fn tool` (proc-macro attribute) | `crates/adam-macros/src/lib.rs:67` |
| TaskBackend | `trait TaskBackend` | `crates/adam-a2a/src/backend.rs:276` |
| Extension | `ExtensionConfig`; the URIs are constants in `extensions.rs` | `crates/adam-a2a/src/card.rs:190` |

Two terms name no ADR of their own, the lease and `#[tool]`; their glossary rows link the page that specifies them
(`docs/architecture.md`, `docs/reference/agent-files.md`) rather than borrow an ADR that does not decide them. The line
numbers drift: re-check with `grep -n` before relying on one.
