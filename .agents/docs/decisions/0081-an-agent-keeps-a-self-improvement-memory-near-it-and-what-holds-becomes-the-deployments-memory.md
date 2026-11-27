# 0081: the evidence

For people: [0081. An agent keeps a self-improvement memory near it](../../../docs/decisions/0081-an-agent-keeps-a-self-improvement-memory-near-it-and-what-holds-becomes-the-deployments-memory.md),
which states the decision. This page holds what it was checked against (*verified 2026-11-27 at `5d2ffc00`*), the
paper, the details the page for people leaves out, and how to check a slice.

## The paper

*Self-Improvement Memory*, arXiv:2611.18856v1, 13 November 2026 (*verified 2026-11-27* by reading it).

* A **harness optimizer** proposes and selects edits to another agent's harness while the task model is frozen. SIM
  groups the selected edits by a **root-cause hypothesis**; each attempt keeps the edit and rationale, the tasks it was
  **predicted** to fix (written before evaluation), what was **observed**, and provenance (domain, benchmark, model,
  round) (§3.2, App. D.3).
* The whole record is read in the optimizer's prompt every round, with no retrieval (§3.2). Only the selected edit is
  written, failures included. The optimizer decides which entry a diagnosis matches (App. D.1).
* Carried across a benchmark or model change it beat carrying the harness alone in all ten settings, 2.36 points on
  average in a paired coding study of ten runs (p = 0.003) (§4.2 to §4.4).
* Its limits: one run per condition outside coding, compatible interfaces only, the procedure evaluated as a whole, an
  LLM judge in one domain (Limitations, Ethics). It is memory for the optimizer, not the task-solving agent (App. A.3).
  So task-time lessons (M1 to M10) are an adaptation, unmeasured until M10; M11 is the paper's own setting.

## adam-rs facts the decision stands on

| Fact | Where |
|---|---|
| No agent memory exists; `memory:` in an agent file is an ignored key (Claude Code's) | `crates/adam-agent-fs/src/load/agent.rs:34` |
| `Plugin` is built: deciding points, `events`, `observe`; no actions and no `tool_source` yet; no binary registers a plugin | `crates/adam-plugin/src/lib.rs:226` |
| `plugins:` in a folder names a registered plugin, inherited by subagents | `crates/adam-agent-fs/src/schema/agent.rs:550`, `crates/adam-assembly/src/def.rs:557` |
| A journaled step returns its record on replay | `crates/adam-runtime/src/ctx.rs:297` |
| Tools come through `ToolSource`, in groups loaded on demand | `crates/adam-llm-agent/src/source.rs:318`; ADR 0066 |
| The coder appends a slot's `AGENTS.md` inside `prepare_workspace`'s step | `bin/adam-coder/src/tools/project.rs:4` |
| The coder's checks are machine-observed outcomes | `bin/adam-coder/src/tools/notes.rs:149`, `bin/adam-coder/src/tools/checks.rs:578` |
| One marker for every redactor; the assembly's scrubber | `crates/adam-llm-agent/src/redact.rs:11`, `crates/adam-assembly/src/scrub.rs:14` |
| A caller is a bearer index, not a person; a person's identity is proposed, not built | `crates/adam-a2a/src/backend.rs:41`; ADR 0072 |
| adam-rs has no notion of a chat agent: a chat is a folder served by `adam-agent` | `docs/architecture.md:1639` |

The orchestration layer's person memory is its ADR 0091 (accepted 2026-11-24, *verified 2026-11-27*): tools only, the
person's own notes, off until turned on per agent. Its decision 8 leaves an adam agent's own memory to adam.

## Details the decision leaves to the slices

These are the draft's values; each is a deployment setting or a slice's to confirm, not decided beyond the owner's
answers recorded on the page for people.

| Item | Value |
|---|---|
| Ports and crates | `SiMemory` (L1), `Memory` (L2); `adam-memory`, `adam-memory-testkit` with `si_memory_conformance!`, `memory_conformance!`, `promotion_conformance!`; in-process references `InProcessSiMemory`, `InProcessMemory`; adapters as features of their own crates, on the store's pool and table prefix where one exists |
| Bounds (decision 5) | per L1 scope 64 problems, 16 attempts each, 1 KiB a field, a root cause at most 200 characters; per L2 audience 128 lessons; attempts older than 90 days go unless they evidence an active lesson |
| L1 keys | deployment, caller, agent, kind `agent` or `repository:<host>/<owner>/<name>`; L2: deployment and audience (agent names or `*`) |
| Tools | `si_recall` is paged and notes to weigh an entry less when its model differs; a write is keyed `<run>/tool:<call id>`; the coder appends a slot's repository-scoped problems (at most 2 KiB) after its `AGENTS.md` inside `prepare_workspace`'s step |
| Observer | stamps from `tool.completed`, `tool.failed`, `run.ended`, keyed by the event's key; outside the journal (ADR 0050 decision 1) |
| Preload | `recall: preload` reads in a step `si:load` every run of an agent built with the crate takes, so configuration never moves a seq (ADR 0050 decision 3); waits for a journal epoch (ADR 0076) |
| Promotion rule | within 30 days: 5 attempts observed as successes, from 5 runs and 3 conversations, in 2 L1 scopes, a Wilson lower bound of 0.6, none of the last 3 attempts failed |
| Promotion job | a built-in agent `adam.memory-promote` on a schedule (ADR 0060, daily); `promote(evidence, lessons, policy)` is pure; upsert by `promotion_id`, a hash of the normalised root cause and the audience; the watermark moves after the upsert |
| Reviewer | `adam.memory-review`: a folder, a model alias and limits from the deployment's settings; it gets a candidate and its evidence and returns a verdict and a reason, journaled; `MEMORY_PROMOTION=off` stops the pass |
| Settings | `SI_MEMORY` (`on` for `adam-coder` and `adam-agent`, `off` for `adam-engine`) is the ceiling; a folder's `si_memory:` may name `recall`, `preload_kib`, `record`, `scopes`; `MEMORY_URL` from a Secret enables L2; chart values `config.siMemory` and `config.memory` empty by default, rendering nothing |
| Exit 78 at startup | an unknown `si_memory` option, `on` over a store with no adapter, promotion with no L2 store, a secret-looking option |

## Slices: the proof of each

| Slice | Proof, seen failing first | Depends on |
|---|---|---|
| M1 | a double that answers another scope or writes twice for one key fails `si_memory_conformance!`; 16 concurrent writers | M0 |
| M2 | the suite on PostgreSQL, MongoDB, Redis; it fails under `ADAM_TEST_REQUIRE_DB=1` with a URL unset (Rule 7) | M1 |
| M3 + M4 | a folder `off` is off; `SI_MEMORY=off` beats a folder `on`; an unknown option exits 78; `memory:` stays ignored; a replay past `journal_put` asks the store nothing; a secret in a field is refused; a repository entry is not served without its slot | M2 |
| M5 + M6 | a passing check stamps a success, a failing one a failure; a panicking observer leaves the run unchanged; the coder's first request is byte-identical but for one loader line; `deploy/coder` renders unchanged by default (the chart is read by another repository: say so in the pull request) | M4 |
| M7 + M8 | an active lesson is not served outside its audience; a candidate no one approved is served to nobody; the same evidence twice is one lesson; a text naming a repository is never promoted | M2, M5 |
| M9 + M10 | a lesson whose evidence turns is retired and no longer served; the study runs with and without memory from one build | M8 |
| M11 | its own ADR before code | M10 |

New adapters follow `adam-store-adapter`. A new required trait method on `SiMemory` or `Memory` breaks implementers:
say so in the pull request and the crate README (Rule 2).

## Comparison with a code index (the draft's rejected alternative)

An open-source code index, `lci-mcp` (a tree-sitter graph and embeddings in SQLite, MCP over stdio; seven tools, shallow
clone at `beff555`, *verified 2026-11-27*), answers "where is this code and who calls it", rebuilt per revision into a new
generation swapped in atomically. L1 answers "what did we try for this problem and did it work": outcomes of runs, not
derivable from the code. The two are complementary: the coder keeps reaching code intelligence as an MCP server; an
attempt's code anchors are data, and memory never calls the index. Its generation swap is a good precedent for a
retention sweep that never serves a half-written state.

## Checks for this page

```sh
npm --prefix tools/docs-check ci
node tools/docs-check/check-docs.mjs
git ls-tree --name-only origin/main docs/decisions/ | tail -3   # the number is free
```
