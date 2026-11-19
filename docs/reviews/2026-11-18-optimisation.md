# Optimisation review — `another-adam-rs` at `9187cdc`

Read-only review of the Rust workspace (~310k lines) plus `docker/coder`, `deploy/coder`, `compose.yaml`
and `bench/`. No source, manifest, chart or test was modified; this file is the only write.

> **Base.** The review was taken at `9187cdc`. The branch that carries it starts at `origin/main`
> (`2c0bd5e`); the mechanisms were re-checked against that base and the citations refreshed where a
> symbol moved. Numbers tagged MEASURED were measured on the review machine and are not re-measured by CI.

**How to read the numbers.**

| Tag | Meaning |
|---|---|
| **MEASURED** | I ran it. The command and the result are in [Measurements I actually ran](#5-measurements-i-actually-ran). |
| **COUNTED** | An exact count read off the source: queries per operation, a constant, a buffer size. The `file:line` is given, so it can be re-checked in a minute. |
| **ESTIMATED** | Arithmetic over COUNTED/MEASURED numbers. The basis is written out; treat the factor as an order of magnitude, not a promise. |

Nothing in this report was produced by running the agent, a store or a model. Every CPU/RAM/IO factor is
ESTIMATED unless it says otherwise.

---

## 1. Executive summary

The framework's cost is dominated by one structural decision: **a run's durable state is the whole agent
state — for `LlmAgent`, the entire transcript plus every artifact and kept output — and it is
re-materialised several times per commit and re-read in full several times per second.** Everything else
in this report is either a consequence of that or a smaller, independent leak.

The eight changes that carry most of the reduction, in order:

| # | Change | Expected | Confidence |
|---|---|---|---|
| 1 | **P1** Stop copying the state up to 8× per commit and decoding it twice per transition | 3–5× CPU, 2–4× peak RAM on the runtime path | High — eight distinct deep copies, all avoidable, all cited |
| 2 | **P2** Stop rewriting the whole transcript per commit (append-only state, or a small head + a body table) | 10–25× write bytes and WAL on a 40–100-turn run; grows linearly with run length | High on mechanism, medium on the schema change (ADR + `Store` trait) |
| 3 | **P3** Read a *status/lease projection* instead of the whole state for observation | 3–8× less DB read traffic; the single largest I/O win because it is per-second, not per-commit | High |
| 4 | **P4** Poll the durable record at 1–2 s instead of 250 ms per subscriber and per in-flight step | 4–20× fewer `view` calls and wakeups | High; latency trade-off is bounded and already event-assisted |
| 5 | **P5** Compute an artifact's content id once, not on every poll | removes an O(artifact bytes) SHA-256 + `serde_json::to_vec` per artifact per 250 ms per subscriber | High — `artifact_slot` is called in the poll loop |
| 6 | **P6** Stop re-reading records the caller already holds (`ListTasks` reads a page of full states, then 2 queries + a full state per row; sessions listings scan all rounds) | 2× queries and 2× bytes on `ListTasks`; 5–10× on the `child-sessions/v1` listing | High |
| 7 | **P7/P8** Mongo: 4 round trips per journal entry and 2 full-document reads per commit; Postgres NOTIFY: one round trip per event | 3–5× fewer round trips per step; 5–10× fewer on the event path | High |
| 8 | **P12** A tuned release profile (`lto`, `codegen-units=1`, `strip`) | −25% binary size (MEASURED with `strip` alone); 5–15% CPU (ESTIMATED) | High on size, medium on CPU |

### The 5× targets, honestly

| Target | Verdict |
|---|---|
| **RAM ≥5×** | **Reachable for the state-related peak**, and only there: P1 + P2 + P10 remove up to 8 copies of the transcript, the base64 doubling of file artifacts in state, and the 16 MiB-per-command capture that the run can only keep 4 MiB of. On an artifact- or file-heavy coder run (the shipped agent's actual shape) 5× peak RSS is plausible. On a text-only run the state is ~200–400 KB and the *total* process RSS is dominated by things this report does not shrink much (the 50 MB replay buffer, 16 pooled connections, tokio stacks), so total RSS there is more like 2–3×. An idle process is already small; nothing here makes it 5× smaller. |
| **CPU ≥5×** | **Reachable on the framework's own CPU** (serde, hashing, redaction, polling): P1 + P3 + P4 + P5 + P9 + P10 all sit on that path. **Not reachable on wall-clock or on CPU as a fraction of a busy run**, because most of a run's wall time is a model call that uses no local CPU; and if the deployment is one worker with one run and no subscribers, the CPU is already low and the ratio is meaningless. The honest statement is: *CPU-seconds per run on the framework side can drop 3–5×; the share of that in total wall time depends on the model.* |
| **I/O ≥2×** | **Comfortably reachable; 5–20× on DB bytes for long runs.** P2 removes the quadratic write, P3/P4/P6 remove most reads, P7 removes the Mongo amplification, P8 the notify amplification. 2× is a floor, not a target. |

Two things this review does **not** claim: that the image can be made much smaller (the 2.92 GB workspace
base dominates; see P12/P24), and that the model cost drops (nothing here changes token counts except P14,
which removes one listing per turn, not tokens).

---

## 2. Ranked findings

Effort: S ≤ half a day, M ≤ 2 days, L ≥ 3 days or an ADR. Risk is the risk of the *change*, not of leaving it.

| id | area | symptom | mechanism | expected saving | effort | risk | evidence |
|---|---|---|---|---|---|---|---|
| P1 | runtime | every commit copies the transcript up to 8×, decodes it twice per transition | `Envelope::decode` clones the `Value`; `commit` clones `RunRecord`+`Envelope` for nothing; `advance` and `transition` both decode; `build_update` clones artifacts/agent/output; `encode` builds a second `Value` tree | CPU 3–5×, RAM 2–4× on the runtime path | M | Low | `envelope.rs:113`, `worker.rs:428,499,564,841,847,865,912,913` — COUNTED |
| P2 | store | state written in full every commit; state grows with turns → O(turns²) bytes per run | `RunUpdate.state` is the whole envelope; both stores rewrite the row/document | IO 10–25× on write for 40–100 turns | L | Medium (schema/ADR) | `worker.rs:865`, `store-postgres:356`, `store-mongodb:536` — COUNTED + ESTIMATED |
| P3 | runtime/store | `view()` reads the whole state (twice, with `lease_until`) for a status snapshot | `Runtime::view` = `lease_until` + `load_run` + `Envelope::decode` | IO 3–8× on observation, CPU 2–4× | S | Low | `runtime.rs:1350,1354` — COUNTED |
| P4 | runtime/a2a | the durable record is re-read every 250 ms per SSE subscriber and per in-flight step | `ticker` = `backend.poll` (250 ms) → `view`; cancel watch sleeps `poll_interval` → `load_run` | IO 4–20×, wakeups 4–20× | S | Low–Medium (latency) | `subscribe.rs:397,425`, `worker.rs:365,377`, `runtime.rs:649`, `backend.rs:27` — COUNTED |
| P5 | a2a-runtime | every poll re-serialises and SHA-256s every artifact of the run | `Tracker::sync` → `artifact_slot` → `derived_id` → `to_vec` + `Sha256::digest` (over file bytes too) | CPU: an O(artifact) hash 4×/s per subscriber | S | Low | `subscribe.rs:113,146,379`, `convert.rs:352,364,371` — COUNTED |
| P6 | a2a-runtime | `ListTasks` reads a page of full states, then 2 queries + a full state per row; sessions listing scans every round | `scan` calls `runtime.view(rec.id)` although `rec.state` is in hand; `list_sessions` pages full `RunRecord`s and counts twice | IO 2× queries / 2× bytes on ListTasks; 5–10× on sessions | M | Low | `list.rs:164,172`, `runtime.rs:1114–1151`, `session_events.rs:283,345–352` — COUNTED |
| P7 | store-mongodb | 4 round trips per journal entry; 2 full-document reads per commit; full document for `lease_until` | `journal_put` = find + `run_exists` + insert + `run_exists`; `commit_run` = `load_live` then `find_one_and_update` returning the document | IO 3–5× on the write path | S | Low | `store-mongodb:545,563,611–650,794` — COUNTED |
| P8 | notify-postgres | one `pg_notify` round trip per event, one event at a time | `publish_loop` sends `SELECT pg_notify($1,$2)` per item | IO 5–10× on the event path | S | Low | `notify-postgres:349–359,309–327` — COUNTED |
| P9 | adam-coder | every tool output is scanned once per registered secret (up to ~650 needles) | `scrub` loops `needles` doing `contains` + `replace` | CPU 2–5× on tool-output handling; more with big outputs | S | Low | `redact.rs:282–290`, `redact.rs:54` — COUNTED + ESTIMATED |
| P10 | adam-coder | a command captures 16 MiB but the run may keep 4 MiB; ×10 parallel calls | `KEPT_OUTPUT_MAX` (16 MiB) vs `MAX_RUN_KEPT_BYTES` (4 MiB) | RAM up to 4× on command-heavy runs; CPU 2–4× (scrub + copy of the wasted 12 MiB) | S | Low | `tools/shell.rs:37,191`, `kept.rs:53` — COUNTED |
| P11 | adam-coder | `read_file` streams the whole file (up to 512 MiB) to page 2000 lines | `page_lines` counts lines by reading from the start | IO: one full file read per call | S | Low | `tools/files.rs:322,390,428` — COUNTED |
| P12 | build | no `[profile.release]`: no LTO, no strip, `panic=unwind`, `codegen-units=16` | `Cargo.toml` has no profile section; the Dockerfile runs plain `cargo build --release` | CPU 5–15% (ESTIMATED); binary −25% (MEASURED, strip only) | S | Low | `Cargo.toml:1–114`, `docker/coder/Dockerfile` build stage — MEASURED |
| P13 | runtime/events | every subscriber receives and copies every run's events; replay can hold ~50 MB | one global `broadcast::Sender`; `RunSubscription::recv` filters by run after the clone; `REPLAY_MAX_RUNS × REPLAY_EVENTS_PER_RUN` | RAM up to 50 MB (documented worst case); CPU N× copies | M | Low–Medium | `events.rs:445–455,629–641,658–672` — COUNTED |
| P14 | adam-ui/adam-mcp | the thread-tools list is fetched over a fresh MCP session on every model turn | `ThreadTools::listing` → `Endpoint::list_tools` → `dial()` (initialize + list + close) | latency: 2 RTT per turn; IO: 1 session per turn | S | Low | `thread_tools.rs:472–481`, `once.rs:235–252` — COUNTED |
| P15 | llm-agent | the whole transcript is cloned once per model turn | `for_sending` = `messages.to_vec()` | CPU: one O(transcript) copy per turn | S | Low | `images.rs:853–863`, `agent.rs:1382` — COUNTED |
| P16 | model-openai | the SSE parser consumes bytes one at a time | `for &b in bytes { self.line.push(b) }` | CPU 3–10× on the parser (a small share of a call) | S | Low | `sse.rs:25–36` — COUNTED + ESTIMATED |
| P17 | adam-coder | the janitor loads full states to read a status, and re-reads runs it already checked | `standing` → `load_run`; `sweep_outputs`/`release_orphans` call `is_over` again | IO: N full-state reads per sweep (N = workspaces on the volume) | S | Low | `janitor.rs:546–555,340–365,459–498` — COUNTED |
| P18 | runtime/store | a journaled step is 2 round trips (Postgres) but 5 on Mongo | `Ctx::step` → `journal_get`; `settle` → `journal_put`; Mongo's `journal_put` adds 3 more reads | IO: Mongo 5 → 2 per step | S | Low | `ctx.rs:283,362–375,595`, `store-mongodb:611–650` — COUNTED |
| P19 | a2a-runtime | one `SendMessage`/`GetTask` costs 4–6 full-state reads | `owned` → `view`; `start_with_id_continuing` → `load_run`; `deliver` → `load_run`; `view_of` → `view` | IO 3–5× per request, latency 4–6 RTT | M | Medium | `backend.rs:314–329,717–723`, `runtime.rs:832,1350` — COUNTED |
| P20 | store-postgres | pool size is hardcoded at 16; the notify listener holds one | `PgPoolOptions::new().max_connections(16)` | capacity only (no env knob) | S | Low | `store-postgres:80–84` — COUNTED |
| P21 | runtime | `deliver`/`cancel`/`park_again` clone the state on every retry iteration | `env.inbox.push` then `env.encode()`; `rec.state.clone()` | CPU 1 copy per retry (usually 1) | S | Low | `runtime.rs:354–359,390–398`, `worker.rs:792` — COUNTED |
| P22 | a2a-runtime | each status poll rebuilds the whole task, artifacts included | `task_from_view` → `artifact_of` per artifact per poll | CPU: O(artifact bytes) per poll (compounds P5) | S | Low | `convert.rs:241–258,280–326` — COUNTED |
| P23 | ops | JSON logs at `info`, with an instrumented span per `view`/`deliver`/`cancel`/step | `#[tracing::instrument]` on hot methods; `fmt().json()` | log volume: 1 span per observed call | S | Low | `runtime.rs:691,774,1303,1335,1341`, `worker.rs:47,404`, `main.rs:22` — COUNTED |
| P24 | build | any change to any crate invalidates the Docker `COPY crates crates` layer | one `COPY` for the whole workspace before `cargo build` | build time only (registry + target cache mounts already help) | M | Low | `docker/coder/Dockerfile` build stage — COUNTED |
| P25 | deploy | one replica, 4 concurrent runs, 250 ms poll, 6 GiB limit | `values.yaml` + defaults | none on its own; it is the baseline the factors apply to | — | — | `values.yaml` (`replicaCount`, `config.workers`, `resources`), `runtime.rs:646–653` — COUNTED |

---

## 3. Per-finding detail

### P1 — The state is copied up to eight times per commit and decoded twice per transition

**Current behaviour.** A run's `RunRecord.state` is a JSONB/BSON document holding the runtime envelope,
whose `agent` member is the agent's own state. For `LlmAgent` that is `Conversation`: `messages`
(the transcript), `artifacts`, `kept_outputs`, `images`, … (`crates/adam-llm-agent/src/conversation.rs:328`).
One transition (`worker.rs:405` `advance` → `worker.rs:489` `transition` → `worker.rs:904` `commit`) does:

1. `advance`: `Envelope::decode(run, &lease.run.state)` (`worker.rs:428`) — `envelope.rs:113` does
   `serde_json::from_value(state.clone())`, i.e. **a deep clone of the whole state** plus a deserialise.
2. `transition`: `Envelope::decode(run, &rec.state)` (`worker.rs:499`) — **the same work again**.
3. `agent.step(&mut ctx, env.agent.clone())` (`worker.rs:564`) — **a third deep copy** of the transcript.
4. `build_update`: `cur.artifacts.clone()` (`worker.rs:841`) — **every artifact, including file bytes** —
   plus `next.agent.clone()`, `next.output.clone()`, `next.error.clone()` (`worker.rs:845–864`).
5. `Envelope::encode` (`worker.rs:865` → `envelope.rs:120`) — `serde_json::to_value(self)`, which **walks the
   agent `Value` and copies every string** into a second `Value` tree.
6. `commit`: `let mut cur_rec = claimed.clone(); let mut cur_env = base.clone();` (`worker.rs:912–913`) —
   **two more full deep copies, made before the loop**. On the common path (`attempt == 0`) they are used
   only for `cur_rec.version` and `&cur_env`, so both copies are pure waste; on a retry they are
   overwritten before use. The clone is needed only for the retry path, which can clone lazily.

Then the store serialises that `Value` a third time (Postgres: JSONB text; Mongo: `json_to_bson`).
So a 400 KB state costs ~6 × 400 KB of copying, 2 full serialisations and 1–2 more in the driver, **per
commit**, and a coder run commits once per turn.

**Proposed change.**
* `Envelope::decode` should deserialise from `&Value` (`T::deserialize(state)`; `&Value` is a
  `Deserializer`), removing the clone. One line, no behaviour change.
* `commit` should take `&RunRecord` and `&Envelope` and only clone on `attempt > 0` (where it actually
  reassigns).
* `advance` should keep the decoded envelope and pass it to `transition` (or read `deadline`/`limit_secs`
  from a cheap projection — see P3), so the state is decoded once.
* `agent.step` should take the `Value` by move (`env.agent` is only needed afterwards for `base.attempt`,
  `base.deadline`, `base.remaining_secs` and the `failed`/`retrying` paths, which can borrow those
  scalars instead of the whole envelope).
* `build_update` should consume `Next` (move the artifacts, the agent value, the output) and build the
  envelope `Value` **by moving** `agent` into a `serde_json::Map` rather than `to_value(&self)`.

**Why it is safe.** Every step is a pure ownership change; the wire format, the CAS semantics, the replay
journal and the events are untouched. `serde_json::from_value(v)` and `T::deserialize(&v)` accept the same
documents.

**How to verify.** A unit test in `adam-runtime` that counts allocations with a counting global allocator
around one `commit`, or simply a `dhat`/`heaptrack` run of `cargo test -p adam-runtime --test runtime` on a
run with a 200 KB state: the peak and the total allocated bytes should drop by ~5×. A behavioural test
already exists (`tests/runtime.rs`), and it must stay green. A sharper probe: assert
`Envelope::decode` does not clone by giving the state a `Value` with a huge string and checking
`Arc::strong_count`… not possible for `Value`; use the allocator counter.

**What would make it wrong.** If `serde_json::from_value` on a borrowed value were not available for the
envelope's shape (it is: `&Value` implements `Deserializer` for any `Deserialize` type), or if some caller
relied on `commit` cloning `claimed` before an `await` (it does not: the clones are local).

---

### P2 — The whole transcript is rewritten on every commit

> **Amended 2026-11-19.** The ratio below is half the real one: the bytes written over `T` turns are
> `b·T(T+1)/2` against `b·T` for a delta store, so the ratio is `(T+1)/2` — **20× at 40 turns, 50× at
> 100**, not 10× and 25×. The fix is decided in
> [ADR 0055](../decisions/0055-a-runs-state-is-a-head-and-an-append-only-transcript.md): a head plus
> an append-only transcript and content-addressed file blobs, schema 5, expand then contract across two
> releases. It follows the I/O-layer pull request.

**Current behaviour.** `RunUpdate.state` is the complete envelope (`adam-core/src/store/mod.rs:190`).
Postgres writes it as one JSONB column (`store-postgres:356–362`); Mongo replaces the `state` member of the
document (`store-mongodb:555–575`). The state grows with every turn: an assistant message plus its tool
results, and the artifacts.

Arithmetic (ESTIMATED): with `T` turns adding `b` bytes each, the state after turn `t` is `≈ t·b`, so the
bytes written over a run are `Σ t·b = b·T(T+1)/2`. A delta store writes `b` per turn. The ratio is
`(T+1)/4`: **10× at 40 turns, 25× at 100 turns**, and it grows with run length. Postgres adds WAL on top
(≈ the row size again, less TOAST compression), so the disk traffic is ~2× the row bytes.

`MAX_RUN_FILE_BYTES` is 6 MiB of file artifacts, stored **base64 in the state** (`events.rs:31–37`), i.e.
8 MiB of text that is rewritten on every subsequent commit of that run, and `MAX_RUN_KEPT_BYTES` is another
4 MiB (`kept.rs:53`). A run that has emitted a 4 MiB screenshot therefore rewrites ~5 MiB per turn for the
rest of its life.

**Proposed change.** Two options, in increasing order of work:

1. **Cheap (S–M):** move file artifacts out of the state first. They are the largest single item (6 MiB of
   bytes as 8 MiB of base64, rewritten on every later commit) and the only one with a natural external home:
   a content-addressed blob (a table/collection, or the object store the deployment already has) plus a
   reference in the state. This needs one new `Store` method pair for blobs and no change to the envelope's
   meaning; the A2A layer already serves a file as a `raw` part from `Artifact::file`.
2. **Proper (L, ADR):** an append-only body table/collection with one row per message, and a `state` that
   keeps only the head (counters, pending wait, the artifact list by reference). A commit appends the
   messages it added and CASes the head; a `view` reads the head only (which is P3's projection). This is
   the change that removes the quadratic write for text-only runs too.

Both keep the CAS on the run's `version`, and both need an ADR (rule 8) plus a *defaulted* `Store` method
(rule 2), so no implementer breaks.

**Why it is safe.** The CAS stays on the run's `version`. Body rows are written before the head is CASed
and are keyed by `(run, seq)`; a crash between the two leaves rows that the committed head does not count,
so a replay ignores them — the same at-least-once discipline the journal already has (`ctx.rs:595` writes
the journal entry before the commit). Old code keeps reading a state whose head still carries the same
envelope version.

**How to verify.** Instrument `PgStore::commit_run` to log `octet_length(state::text)`; run the bench's
`dry-run` task set with a scripted model and sum the bytes. Before: quadratic in turns; after: linear.
`SELECT pg_relation_size('adam_runs') + pg_relation_size('adam_journal')` before/after a 50-turn run is the
cheapest end-to-end probe. A test that commits 50 times with a 10 KB message each and asserts the total
bytes written is below `50 × 20 KB` would go red today and green after.

**What would make it wrong.** If the real workload's runs are short (≤10 turns) the quadratic term is
small and the win is only ~3×; if the store's TOAST compression is very effective on repetitive JSON the
*disk* win is smaller than the row-byte win (the CPU of compress/decompress remains).

---

### P3 — `view()` reads the transcript to answer "what is the status?"

**Current behaviour.** `Runtime::view` (`runtime.rs:1342`) does `store.lease_until(run)` then
`store.load_run(run)` then `Envelope::decode` (which clones, P1) and builds a `RunView` that carries the
whole agent state (`runtime.rs:193`). On Postgres `lease_until` is a projection (`store-postgres:389`) but
`load_run` returns `RUN_COLUMNS`, which includes `state` (`store-postgres:67`). On Mongo `lease_until`
reads the **whole document** (`store-mongodb:794–801`), so a `view` reads the state twice.

`view` is the observation primitive: SSE polls (P4), `GetTask`, every `owned()` check (P19), the sessions
tracker (P6), the cancel watch (P4), the janitor (P17).

**Proposed change.** Add a defaulted `Store` method `run_header(id) -> RunHeader` (status, version,
wake_at, updated_at, parent, agent, conversation, lease_until) whose default implementation calls
`load_run` and projects, and implement it in both stores with an explicit column projection / Mongo
`projection()`. `Runtime::view` then needs one query for a caller that only wants the status, and
`RunView::state` becomes lazily loaded (`Option<Value>`) for the few callers that need it (the prompt
function, `status_ui`, the sessions tracker) — or `view` keeps a `view_with_state` variant.

**Why it is safe.** A new defaulted trait method does not break implementers (`CLAUDE.md` rule 2). The
callers that need the state ask for it explicitly; the ones that do not get a smaller read. `lease_until`
already exists as a separate call, so no semantics change.

**How to verify.** `EXPLAIN (ANALYZE, BUFFERS)` on the two statements: today `load_run` shows
`Buffers: shared hit=… ` proportional to the row size; after, the header query touches one heap page.
A test that asserts `view` performs ≤1 store call for a terminal run (a counting store wrapper — the
testkit already has `fault.rs`, which counts methods) goes red today (2 calls) and green after.

**What would make it wrong.** If the A2A layer really needs the state on every poll — it does not:
`status_of` reads `pending_wait`, `output` and `error`, all of which can live in the header or be fetched
only when the status is `input-required`/`completed`.

---

### P4 — 250 ms polling of the durable record, per subscriber and per in-flight step

**Current behaviour.** Two independent loops read the full run every `poll_interval` (250 ms,
`runtime.rs:649`, `backend.rs:27`, `service.rs:40`):

* the SSE pump: `ticker.tick()` → `backend.runtime.view(run)` (`subscribe.rs:397,404,425`);
* the cancel watch of every step in flight: `sleep(every)` → `store.load_run(run)` (`worker.rs:365,377`),
  plus a parent read at most once a second for a child run (`worker.rs:379–390`).

Arithmetic (COUNTED × ESTIMATED): one SSE subscriber = 4 `view`/s = **8 queries/s and 4 full-state reads/s**.
One in-flight run = **4 full-state reads/s** from its cancel watch. A run being watched while it steps
therefore costs ~8 full-state reads/s; at a 300 KB state that is ~2.4 MB/s from the database, per run. Ten
such runs: ~24 MB/s and 120 queries/s for observation alone, on one replica.

Both loops are *already* event-assisted: a commit emits a `Status` event in-process and, with
`adam-notify-postgres`, to every other process (`runtime.rs:454–464`, `worker.rs:959–977`), and a cancel
publishes `Signal::Finished` (`runtime.rs:330–335`). The 250 ms poll is the fallback, not the mechanism.

**Proposed change.** Keep polling (the docs promise it is the truth) but make the fallback slower and
event-driven:
* SSE pump: poll at 2 s (a new `A2A_VIEW_POLL_SECS`/`RuntimeTaskBackend::with_poll_interval` default), and
  poll immediately on a `Status` event (already done) and on a live `Artifact`/`Step` event if the status
  key could have changed. A run whose worker is in another process already gets its statuses over NOTIFY.
* cancel watch: poll at 1 s (a separate `CANCEL_POLL_SECS`), and fire on `Signal::Finished` (already wired
  through `Inner::fire_cancel`) and on `Delivery::Resync`.
* Make both configurable and document the latency bound they imply.

**Why it is safe.** Correctness never depends on the notification (`notify.rs`, `CLAUDE.md` rule 5). The
only change is the worst-case latency of a state change that no event announced: from 250 ms to 1–2 s. For
a run being *watched*, the events are emitted by the same process that commits (or NOTIFY'd), so the
practical latency is unchanged.

**How to verify.** A counter on `Store::load_run` (the testkit's fault wrapper can do it) during a
`SendStreamingMessage` of a scripted 5-turn run: today ~40 loads for the stream plus 4/s per step; after,
≤10. End-to-end: `pg_stat_statements` calls for the `SELECT … FROM adam_runs WHERE id = $1` statement per
minute before/after.

**What would make it wrong.** If the deployment runs the control plane and the worker without
`adam-notify-postgres` (a `LiveSignals::local()` composition, e.g. an embedded host), the slower poll is the
only path and the latency really does become 2 s. That is why the knob must exist and the default should be
justified per role: `serve()` uses the notifier, an embedded composition may not.

---

### P5 — Artifacts are re-hashed and re-serialised on every poll

**Current behaviour.** `Tracker::sync` iterates `view.artifacts` on every poll and calls `admit`, which
calls `artifact_slot` → `derived_id` → `serde_json::to_vec(&(&name, &mime_type, &data))` + `Sha256::digest`,
or, for a file, `Sha256::digest(&file.bytes)` (`subscribe.rs:113,146`, `convert.rs:352–387`). The same
happens once at subscribe (`subscribe.rs:379`). So a run that emitted a 4 MiB image artifact re-reads and
hashes 4 MiB **four times a second per subscriber**, and re-serialises every JSON artifact's data too.

**Proposed change.** Compute the content id when the artifact is created/committed (in `Artifact` or in the
runtime's commit path) and store it (a new `Option<String>` member with `skip_serializing_if`, or an
`Arc<str>`), so `artifact_slot` is a lookup. Alternatively keep a per-subscriber `HashMap<artifact index,
(slot, content)>` keyed by the artifact's position in `view.artifacts`, which is stable across polls.

**Why it is safe.** The id is a pure function of the artifact; caching it changes no wire value. A member
added with `#[serde(default)]` reads old state.

**How to verify.** A test that emits a 1 MiB artifact and polls a `Tracker` 10 times, asserting the number
of bytes hashed (a counting hasher, or just wall time on a debug build) does not grow with the polls. Today
it grows 10×.

**What would make it wrong.** If `view.artifacts` were reordered between polls the index-keyed cache would
miss; the content id cache is immune. If artifacts were mutated in place with the same id (they are
replaced, not mutated — `add_artifact`, `worker.rs:875`), the cache would be stale; the runtime replaces by
id, so a content change means a new id.

---

### P6 — `ListTasks` and the sessions listing re-read what they already hold

**Current behaviour.**
* `list_tasks::scan` (`list.rs:161–199`) pages `list_runs` (a batch of 50–200 full `RunRecord`s, state
  included), then for **each** record calls `self.runtime.view(rec.id)` (`list.rs:172`) — two more queries
  and another full state read, for a record whose `state` is already in `rec`. A 50-task page therefore
  costs 1 count + 1 list (51 states) + 50 × (2 queries + 1 state) ≈ **100 full-state reads and ~100 queries**
  where ~51 reads and 3 queries would do; a filtered page that scans to `MAX_SCAN` costs 500 × 2 = **1 000
  queries**.
* `Runtime::list_sessions` (`runtime.rs:1114–1151`) pages full `RunRecord`s of every round (state included)
  and calls `count_runs` twice per agent per attempt, retrying up to 3 times when the counts differ — which
  they do while a parent is committing.
* `SessionsTracker` calls that listing at least every second (200 ms when the parent's version changed,
  `session_events.rs:27–38`) and then `runtime.view` per open session (`session_events.rs:345–352`), i.e.
  2 more full-state reads per open session per listing.

**Proposed change.** `scan` should decode `rec.state` itself (`Envelope::decode(rec.id, &rec.state)`), and
read the lease only when `task_state` needs `claimed` (runnable, version ≤ 1) — one extra query instead of
two per row, and no second state read. `list_sessions` should select the columns it uses (agent,
conversation, status, version, updated_at — not `state`) through a projection, or use the P3 header; the
sessions tracker should cache `Seen` by `(session, version)` and skip the `view` when the version is
unchanged.

**Why it is safe.** `rec.state` is exactly what `view` would return (the record was read after the lease, so
the only difference is `claimed`, which the header read covers). The listing's documented contract
(ordered by `updated_at`, keyset pages) is unchanged.

**How to verify.** A counting `Store` wrapper around `list_tasks` for a 50-task context: queries drop from
~100 to ~52 and state bytes from ~100× to ~51×. For the sessions tracker, count `view` calls over a 10 s
stream of a parent with 3 open sessions: 30 today, ≤10 after.

**What would make it wrong.** If `list_runs` were changed to not return `state` (it must, for the trait's
other callers), or if `claimed` were needed for every row (it is not: a terminal or parked run is never
`claimed`, `runtime.rs:1355`).

---

### P7 — MongoDB: 4 round trips per journal entry, 2 full-document reads per commit

**Current behaviour.**
* `journal_put` (`store-mongodb:611–650`): `find_journal` (1) → `run_exists` (2) → `insert_one` (3) →
  `run_exists` again (4). With `Ctx::step`'s `journal_get` before it (`ctx.rs:362–375`), **a journaled step
  is 5 network round trips** in the common case where nothing is recorded yet.
* `commit_run` (`store-mongodb:536–592`): `load_live` (a full document, 1) then `find_one_and_update` with
  `ReturnDocument::After` (the full document again, 2), plus the write. The state is transferred 3× per
  commit.
* `lease_until` (`store-mongodb:794–801`) fetches the whole document to read one field; `view` calls it
  (P3), so every SSE poll transfers the state twice from Mongo.

**Proposed change.**
* `journal_put`: try `insert_one` first and treat a duplicate key as "someone won, read theirs" (the
  duplicate path already exists); drop the pre-insert `run_exists` and rely on the post-insert check only
  when the insert succeeded (as it does today) — or better, keep a tombstone check in the insert's filter
  by making the journal write a two-statement transaction on replica sets, or accept the FK-less check at
  purge time. 4 → 1–2 round trips.
* `commit_run`: `load_live` is only needed for `agent` and `conversation_id` (for `open_key`). Project those
  two fields (`projection(doc!{"agent":1,"conversation_id":1})`), and project the document
  `find_one_and_update` returns down to the fields the caller reads, filling `state` from the update just
  written (it is the same bytes) instead of transferring the stored copy back. 3 state transfers → 1.
* `lease_until`: `projection(doc!{"lease_until":1})`.

**Why it is safe.** Same atomicity (single-document `find_one_and_update`), same CAS. The projections only
remove fields the code does not read.

**How to verify.** The Mongo conformance suite (`adam-store-testkit`) must stay green. Count round trips
with the driver's command monitoring (`mongodb`'s `event` handlers) around one `journal_put` + `commit_run`:
11 → 3. `mongostat`/`mongotop` on a bench run shows the drop in `query`/`getmore` ops.

**What would make it wrong.** If `run_exists` before the insert is load-bearing for a *concurrent purge*
(the comment says it is: a purge between the check and the insert would leave an orphan). The insert-then-
check variant already deletes the orphan, so the pre-check is redundant — but this needs the purge race
tested (the testkit has one).

---

### P8 — One `pg_notify` round trip per event

**Current behaviour.** `PgEventSink::emit` encodes the event and `enqueue`s it; `publish_loop` sends each
item with `SELECT pg_notify($1, $2)` on the pool, **one at a time** (`notify-postgres:309–327,349–359`).
A streaming model turn emits a text delta every 200 bytes or 100 ms (`text_stream.rs:24–27`), so a turn is
~10 NOTIFY round trips/s; every step event, status, usage and artifact is another. Each NOTIFY is then
delivered to every listening process, parsed and re-emitted into its local `BroadcastSink`.

**Proposed change.**
* Batch: drain the queue into `SELECT pg_notify(channel, payload) FROM unnest($1::text[], $2::text[])`
  (one statement per ≤N items, N ≈ 32), preserving order.
* Do not send text/reasoning deltas across processes at all: they are pure latency sugar and the whole text
  is committed with the turn (`events.rs:259–261` says so). Keep them local.
* Consider `NOTIFY` only for `Status` and `Signal`, which is what the latency actually needs; steps and
  progress are cosmetic and already re-read on the next poll.

**Why it is safe.** Notifications are explicitly best-effort (`notify-postgres` module docs); dropping
deltas changes only how smoothly a remote client sees text grow, and the durable text still arrives with the
turn. Batching preserves per-channel order if the batch is built in order.

**How to verify.** `pg_stat_statements` calls for `SELECT pg_notify` per run: today ≈ events; after ≈
events/32. A test asserting the number of `pg_notify` statements for 100 emitted events is < 10.

**What would make it wrong.** If a client relied on cross-process text streaming for a run stepped by
another replica and *not* on the committed text — the A2A contract says the whole text arrives with the
turn, so it should not.

---

### P9 — The redactor scans every output once per registered secret

**Current behaviour.** `Redactor::scrub` loops over `needles` and does `out.contains(needle)` then
`out.replace(...)` for each (`redact.rs:282–290`). `needles` holds 5 forms per secret (raw, two base64s, two
basic-auth base64s) over `fixed` plus up to `MAX_ADDED = 2 × MAX_CACHED_INSTALLATIONS = 128` rotating
tokens (`redact.rs:54`, `github_app.rs:125`) — **up to ~650 needles** for a GitHub App deployment with many
owners. Every tool output, every failure text and every JSON string of a structured result goes through it.

Arithmetic (ESTIMATED): a 1 MiB output is scanned 650× = 650 MiB of `memmem` per tool call (~30–60 ms at
10–20 GB/s); a 16 MiB MCP result (`once.rs:302–305` caps at `MAX_KEPT_BYTES + SCRUB_OVERLAP`) is ~10 GB,
i.e. ~1 s of CPU, per call.

**Proposed change.** Build an Aho-Corasick automaton (or a first-byte-indexed bucket list) on `rebuild` and
scan the text once. `rebuild` already exists and is called only when a secret is added.

**Why it is safe.** Same needles, same replacement; only the search strategy changes. Ordering ("longest
first, so a secret containing another is replaced whole") is preserved by Aho-Corasick's leftmost-longest
match.

**How to verify.** A unit test with 128 added tokens and a 1 MiB text that contains none: assert the elapsed
time is within a small bound (or count `memchr` calls with a custom searcher). Today it is 650 passes; after,
one.

**What would make it wrong.** If most deployments use a single token (not an App), `needles` is ~10 and the
win is ~10× on a path that is already fast — the finding then matters only for the App deployments it
targets.

---

### P10 — A command captures 16 MiB but the run can keep only 4 MiB

**Current behaviour.** `Tail` captures the first `KEPT_OUTPUT_MAX + margin` bytes of a command's output
(`tools/shell.rs:37,191`), where `KEPT_OUTPUT_MAX = 16 MiB`. The keep step accepts a result only if the
run's `kept_bytes() + text.len() <= MAX_RUN_KEPT_BYTES` = 4 MiB (`conversation.rs:1026–1032`,
`kept.rs:53`). So up to 12 MiB per command is captured, copied, scrubbed (P9) and then dropped. With
`max_parallel` 10 (`agent.rs:185`) that is up to **160 MiB of capture buffers per run**, and with
`concurrency` 4, up to 640 MiB.

**Proposed change.** Capture at most `MAX_RUN_KEPT_BYTES + margin`, or pass the run's remaining room into
the tool (the tool already receives `kept_left`, `conversation.rs:1023–1025`) and capture `min(room,
MAX_KEPT_BYTES)`. Keep the tail (what the model is shown) as it is.

**Why it is safe.** A result that would not be kept is already refused; capturing less only changes *when*
the refusal happens, not whether. The model still sees the same tail.

**How to verify.** A test that runs a command producing 20 MiB with `kept_left = 4 MiB` and asserts the peak
allocation (a counting allocator) is < 5 MiB, and that the tool's answer still says the output was kept or
not. Today the capture alone is 16 MiB.

**What would make it wrong.** If some tool returned the captured `all` to a *different* consumer that is not
bounded by `MAX_RUN_KEPT_BYTES` (the checks' findings?), that consumer would see less. Grep shows `all` is
used for keeping only.

---

### P11 — `read_file` reads the whole file to page it

**Current behaviour.** `read_in_until` opens the file and `paging::page_lines` counts lines by reading from
the start; `READ_MAX_FILE` is 512 MiB (`tools/files.rs:322,390,428`). A model that pages through a large
lockfile or a minified bundle reads the file once per call.

**Proposed change.** For `offset > 1` and a page that does not need the total, seek to the start and skip
lines with a buffered reader (still O(offset) but no full read), or lower `READ_MAX_FILE` to a few MiB and
point at `search_files` (the error message already does).

**Why it is safe.** The page content is identical; only the total-line footer may be omitted for a middle
page (it is already only shown at the end).

**How to verify.** `strace -c -e read` around one `read_file` of a 100 MiB file at offset 1 000 000: today
100 MiB read; after, ~the page.

**What would make it wrong.** If the footer (total lines) is part of the tool's contract for every page, the
count must be kept — then the fix is only the lower cap.

---

### P12 — No release profile (MEASURED)

`Cargo.toml` has no `[profile.release]`, so the shipped binaries are built with
`opt-level=3, lto=false, codegen-units=16, panic=unwind, strip=none` (`docker/coder/Dockerfile` runs plain
`cargo build --release`). Adding:

```toml
[profile.release]
lto = "thin"
codegen-units = 1
strip = "symbols"
```

costs build time and gives 5–15% CPU on serde/JSON-heavy code (ESTIMATED) and a much smaller binary
(MEASURED — see the measurements section). `panic = "abort"` interacts with the runtime's
`catch_unwind` around `agent.step` (`worker.rs:564–566`): with `panic=abort` a panicking agent kills the
process instead of failing the run. **That is a behaviour change**: either keep `panic = "unwind"` (lose
little) or record the decision. I recommend `lto`, `codegen-units`, `strip` only.

**How to verify.** `cargo build --release` before/after: binary size, and `cargo bench`/the bench harness
wall time on a scripted model (the model call dominates; measure CPU-seconds with
`/usr/bin/time -v`, "User time").

**What would make it wrong.** If CI's build time budget cannot absorb `codegen-units=1` + thin LTO (it
roughly doubles link/compile time), use `lto = "off"` + `codegen-units = 1` or keep 16 and take only
`strip`.

---

### P13 — One global broadcast; every subscriber copies every run's events

`BroadcastSink::emit` sends every event of every run into one `broadcast::Sender` (`events.rs:629–641`); a
`RunSubscription` receives all of them and filters by run *after* tokio clones the `SinkEvent`
(`events.rs:658–672`). With S subscribers and E events/s across all runs, that is S×E deep clones of
`SinkEvent` (including artifact payloads). The replay buffer is bounded at 64 runs × 64 events
(`events.rs:445–455`), and the code itself notes the worst case is "near 50 MiB" — a real RAM floor per
process.

**Proposed change.** Key the broadcast by run (a `Mutex<HashMap<RunId, broadcast::Sender<RunEvent>>>` with
a refcounted cleanup, no new dependency), or send `Arc<SinkEvent>` so a subscriber clones a pointer, not a
payload. Drop the `Step` input/output from the *replay* copy (the live event keeps them) — the replay is a
fidelity aid, not the record.

**Why it is safe.** Same delivery semantics; an `Arc` clone is invisible to the API if `SinkEvent` is
returned by value (clone-on-return, but only for the matching run).

**How to verify.** A test with 10 subscribers on 10 runs emitting 1 000 events: count allocations (or
`Arc::strong_count`) — today 10 000 payload clones, after 1 000.

---

### P14 — The thread-tools list is fetched per model turn, over a new MCP session

`ThreadTools::listing` calls `Endpoint::list_tools()` (`thread_tools.rs:472–481`), which dials a **new**
MCP session (`once.rs:235–252`: initialize + list + close), and `apply_sources` runs inside every journaled
model step (`agent.rs:1443`). So every turn pays 2 extra round trips to the orchestrator before the model
call, and the list is static for a run.

**Proposed change.** Cache the listing per `(url, token digest)` for the life of the run (or 60 s) in the
`Endpoint`/`ThreadTools` state, invalidated by a failed call (`tools/list` is idempotent). The per-call
session is justified by the short-lived token; the per-turn *listing* is not.

**Why it is safe.** A stale list only means a tool the orchestrator added mid-run is not offered until the
TTL; the call path itself still dials per call.

**How to verify.** Count `initialize` requests the mock MCP server sees for a 10-turn scripted run: 10
today, 1 after.

---

### P15 — The transcript is cloned per model turn

`for_sending(&state.messages, …)` = `messages.to_vec()` (`images.rs:857`), called once per model call
(`agent.rs:1382`). One O(transcript) deep copy per turn; at 400 KB it is 400 KB × turns per run. Cheap to
avoid: `for_sending` could take `&[Message]` and only clone the messages it rewrites (the image ones), or
return `Cow`.

### P16 — SSE parser consumes bytes one at a time

`SseParser::push` loops `for &b in bytes { self.line.push(b) }` (`sse.rs:25–36`). For a 1 MB stream that is
1 M iterations with a per-byte capacity check; `memchr` + `extend_from_slice` between newlines is a few
lines. Small share of a model call, but free.

### P17 — The janitor reads full states for a status

`standing` calls `store.load_run(id)` (`janitor.rs:546–555`) for every workspace directory, every
`WORKSPACE_SWEEP_SECS` (300 s default); `sweep_outputs` and `release_orphans` call `is_over` again for runs
the main loop already judged. With N workspaces that is N full-state reads per sweep. Use the P3 header
projection and remember the verdict within a sweep.

### P18 — Two round trips per journaled step are inherent; Mongo adds three more

`Ctx::step` does `journal_get` before (`ctx.rs:283,362–375`) and `journal_put` after (`ctx.rs:595`). The
read cannot be skipped: after a crash, a step at `seq >= committed_seq` may still have been recorded by the
attempt that died before its commit, so only a read tells you. Postgres is already minimal (the write is one
`INSERT … ON CONFLICT DO NOTHING RETURNING`). **Mongo is not** (P7): 5 round trips for the same step. The
action is P7, not a new mechanism here; a `Store::journal_put_many` would help a turn that journals several
tool calls at once (`step_all`, `ctx.rs:338–359`) but is a smaller win.

### P19 — A2A request fan-out

A new `SendMessage`: `start_with_id_continuing` → `load_run(run_id)` (a miss) + `create_run` + `view_of`
(2 reads) = 4 round trips. A follow-up: `owned` (2) + `deliver` (1 read + 1 write) + `view_of` (2) = 6.
Each read is a full state. P3's header read and P6's reuse cut this to ~2–3; caching the created record in
`start_with_id` (it is returned by `create_run`!) removes one. **With `child-sessions/v1` activated** every
`get`, `submit` and `cancel` also calls `with_sessions` → `sessions_object` → the full sessions listing
(P6), so one `GetTask` can be a scan of every round of the context on top of its own two reads.

### P20–P25 — capacity, ops, build

* **P20**: `PgPool::max_connections(16)` is hardcoded (`store-postgres:80–84`); the notify listener holds
  one for the process lifetime. Make it configurable (`DATABASE_POOL_SIZE`) and document that 16 is per
  process.
* **P21**: `deliver`/`cancel_run` clone the envelope per retry iteration (`runtime.rs:354–359,390–398`) and
  `park_again` clones `rec.state` (`worker.rs:792`) — one avoidable copy each on the common path.
* **P22**: `task_from_view` maps every artifact per poll (`convert.rs:241–258`), compounding P5; the task's
  artifacts could be built once per version.
* **P23**: `#[tracing::instrument]` on `view`, `deliver`, `cancel`, `start*`, `step`, `advance`,
  `model.complete` (`runtime.rs:691,774,1309,1335,1341`; `worker.rs:47,404`) creates a registry span per
  call; with JSON logging and `info`, a busy control plane pays for it. Cheap to demote `view`/`deliver` to
  `debug` or to drop the attribute (the span carries no useful field for `view`).
* **P24**: the Docker build `COPY crates crates` + `COPY bin bin` invalidates the compile layer on any
  source change; the registry and target cache mounts (`--mount=type=cache`) already keep it reasonable.
  A dependency-only layer (copy the manifests, build a dummy main, then copy the sources) would make the
  common "one crate changed" build reuse the dependency graph — at the cost of a second `cargo build` pass.
  Not worth it unless build time is a complaint.
* **P25**: `deploy/coder/values.yaml` runs one replica, `config.workers: 4`, requests 500m/1Gi, limit 6Gi,
  probes at 5/15 s. With P4's 250 ms polls and 4 concurrent runs, the idle load is ~4 claim queries/s and,
  per active run, ~8 full-state reads/s. Nothing in the chart is wrong; it is the baseline the factors
  above multiply.

---

## 4. Proposed but rejected

| Proposal | Why rejected |
|---|---|
| **Compress the state in the application before storing it** (zstd) | It reduces bytes but *adds* CPU on a path already dominated by serialisation, and Postgres TOAST already compresses JSONB (pglz) and WiredTiger compresses collections (snappy). Do P2 instead: less data beats better compression. |
| **Cache `RunView` in the process and serve SSE from it** | Breaks the documented contract that `Runtime::view` is the durable truth read after every status event (`events.rs:1–8`, `subscribe.rs:162–163`) and that a subscriber in another process sees a worker's commit. The in-process `BroadcastSink` already is that cache, for the live part. |
| **Drop polling entirely and rely on NOTIFY** | Explicitly forbidden by `CLAUDE.md` rule 5 and the `Notifier` contract ("a notification is never the truth"). P4 keeps polling, only slower. |
| **`panic = "abort"` in the release profile** | Changes behaviour: `advance` catches an agent panic and fails the run (`worker.rs:564–571`). With abort, one agent panic kills the worker and every other run on it. Rejected; keep unwind. |
| **Shrink the workspace base image** | The 2.92 GB compressed base is `vymalo/another-agentic-images/workspace`, another repository, and the coder image adds only a few MB on top. Out of scope here; only P12's `strip` touches it. |
| **Batch `commit_run` across runs** | The CAS is per run and the runtime advances runs independently; batching would serialise unrelated runs behind one statement and change the lease/commit interleaving the tests pin. |
| **Replace `serde_json::Value` with a typed envelope everywhere** | The envelope must stay queryable from SQL/`mongosh` (`store-postgres` module docs) and the agent state is genuinely open (`Value`). The win comes from not copying it, not from typing it. |
| **Make `Store::commit_run` take bytes instead of `Value`** | This is the *right* long-term shape (one serialisation), but it is a breaking trait change for every implementer and needs an ADR (rule 2, rule 8). Recorded as the follow-up to P1/P2, not proposed as a quick fix. |
| **Reduce `concurrency` or raise `poll_interval` in the chart** | Treats the symptom and changes the product's throughput/latency. P3/P4 fix the cost per poll; the knobs should stay where the operator can choose them. |
| **Cache the MCP `tools/list` forever** | The token is short-lived and the endpoint may legitimately change tools mid-conversation; a 60 s TTL is the compromise in P14. |
| **Remove the `text-stream/v1` extension** | It is a product feature with a contract; P8 keeps it local and stops paying DB round trips for it. |

---

## 5. Measurements I actually ran

**1. Release build of `adam-coder` (no source changes).**

```
systemd-run --user --scope --quiet -p MemoryMax=32G -p MemoryHigh=28G \
  env CARGO_TARGET_DIR=/tmp/opencode/adam-target CARGO_BUILD_JOBS=4 \
  /usr/bin/time -v cargo build --release --locked -p adam-coder --bin adam-coder
```

Observed (MEASURED), on this machine's toolchain (rustc 1.98.0, while the image builds with 1.94 — the
workspace MSRV). The wall clock was taken while two other sessions' builds were running, so the
contention-independent number is **user CPU (1361 s)**; the size and RSS figures are unaffected.

| | baseline | `strip --strip-all` |
|---|---|---|
| wall clock | **6:20.78** | — (a post-step) |
| user CPU | **1361.49 s** | — |
| system CPU | **64.60 s** | — |
| peak RSS of the build | **1,417,800 KiB (1.35 GiB)** | — |
| `target/release/adam-coder` | **89,416,360 B (85.3 MiB)** | **67,000,480 B (63.9 MiB)** |

`strip --strip-all` alone removes **22,415,880 bytes (21.4 MiB, −25.1%)** from the shipped binary. The image
carries six binaries built the same way (`adam-coder`, `adam-agent`, `adam-kube-exec`, `adam-egress`,
`adam-git-sandbox`, `adam-git`), so a `strip = "symbols"` in the release profile is roughly 30–50 MiB off
the coder image's own layer, at zero runtime risk. A full-workspace release build is ~1.5 GB of target and
6 minutes at 4 jobs; that is the baseline P24 argues about, not a problem in itself.

The LTO/`codegen-units=1` build was started as a second measurement
(`CARGO_PROFILE_RELEASE_LTO=thin CARGO_PROFILE_RELEASE_CODEGEN_UNITS=1 CARGO_PROFILE_RELEASE_STRIP=symbols`)
and then stopped: three other builds were running on this machine (two of them other sessions' gates), and a
slot was worth more than the number. **The LTO size gain is unmeasured**; the `strip` figure above is the
measured one.

**2. Nothing else.** I did not run the agent, a store, a model, the bench harness or `docker compose`; no
allocation counter, `EXPLAIN`, `pg_stat_statements` or `mongostat` was run. Every CPU/RAM/IO factor in this
report is ESTIMATED from the COUNTED code facts and the arithmetic shown next to it.

**How to turn the estimates into measurements** (in the order I would do it):

1. `pg_stat_statements` + `pg_stat_user_tables` on the coder's database for one bench run: `calls` and
   `shared_blks_read` per statement name. The `SELECT … WHERE id = $1` (view) and `UPDATE … SET state`
   (commit) statements should dominate; that alone confirms P1–P4.
2. `EXPLAIN (ANALYZE, BUFFERS)` on the commit and the view: buffers per call ∝ state size.
3. `/usr/bin/time -v` around the worker process, plus `memory.peak` from its cgroup: peak RSS before/after
   P1/P2/P10.
4. The bench harness records cost and tokens but **not** resources; adding `docker stats --no-stream` (or
   the cgroup `memory.peak`/`cpu.stat`) sampled per arm would make every claim above falsifiable in the
   existing report. That is a small addition to `bench/lib/runner.mjs`.

---

## 6. Issues ready to file

**1. `perf(runtime): the run state is deep-copied up to eight times per commit`** — `perf` `ram` `cpu`
`Envelope::decode` clones the whole state (`envelope.rs:113`), `commit` clones `RunRecord`+`Envelope` for
nothing (`worker.rs:912–913`), `advance` and `transition` decode twice, and `build_update` clones artifacts
and the agent value before serialising a second `Value` tree. Deserialise from `&Value`, take references in
`commit`, move instead of clone in `build_update`. Expect 3–5× less CPU and 2–4× less peak RSS on the
runtime path; the existing `tests/runtime.rs` suite is the guard.

**2. `perf(store): the whole transcript is rewritten on every commit`** — `io` `ram`
`RunUpdate.state` is the entire envelope, so a run's writes are O(turns²) (10× at 40 turns, 25× at 100) and
a 4 MiB file artifact is rewritten (base64) on every later commit. Put file artifacts out of line first, then
store the transcript in an append-only body table with a small head. Needs an ADR and a defaulted
`Store` method so implementers do not break.

**3. `perf(runtime): view() reads the whole state, twice, to answer a status question`** — `io` `cpu`
`Runtime::view` = `lease_until` + `load_run` + a full decode; on Mongo `lease_until` reads the whole
document. Add a defaulted `Store::run_header` projection and make `RunView::state` opt-in. It is the
primitive behind SSE polling, `GetTask`, `owned()`, the sessions tracker and the janitor.

**4. `perf(a2a): poll the durable run every 250 ms per subscriber and per in-flight step`** — `io` `cpu`
`subscribe.rs:397,425` and `worker.rs:365,377` re-read the full record 4×/s each. Keep polling as the truth
but at 1–2 s, and fire on the `Status`/`Signal::Finished` events that already exist. Make the interval a
documented setting per role.

**5. `perf(a2a): artifacts are re-hashed and re-serialised on every poll`** — `cpu`
`Tracker::sync` → `artifact_slot` → `derived_id` runs `serde_json::to_vec` + SHA-256 over every artifact
(including file bytes) four times a second per subscriber. Compute the content id once when the artifact is
committed and cache it.

**6. `perf(a2a): ListTasks reads every record twice`** — `io`
`list.rs:172` calls `runtime.view(rec.id)` although `rec.state` is already in hand: a 50-task page is ~100
full-state reads instead of ~51. Decode the record it has; read the lease only when `claimed` matters.

**7. `perf(store-mongodb): 5 round trips per journaled step, 3 state transfers per commit`** — `io`
`journal_put` does find + `run_exists` + insert + `run_exists`; `commit_run` reads the whole document and
returns it whole; `lease_until` fetches the whole document. Insert first and read the winner on duplicate,
project the two fields `commit_run` needs, project `lease_until`.

**8. `perf(notify): one pg_notify round trip per event`** — `io`
`publish_loop` sends `SELECT pg_notify($1,$2)` per item, one at a time; a streaming turn is ~10/s. Batch
with `unnest`, and stop sending text/reasoning deltas across processes (the whole text is committed with the
turn).

**9. `perf(coder): the redactor scans every output once per secret (up to ~650)`** — `cpu`
`Redactor::scrub` loops needles with `contains` + `replace`; with a GitHub App's 128 rotating tokens that is
650 passes over every tool output. Build an Aho-Corasick automaton on `rebuild`.

**10. `perf(coder): a command captures 16 MiB but the run can keep 4 MiB`** — `ram` `cpu`
`KEPT_OUTPUT_MAX` (16 MiB, `tools/shell.rs:37`) against `MAX_RUN_KEPT_BYTES` (4 MiB, `kept.rs:53`), ×10
parallel calls = up to 160 MiB of capture buffers per run. Capture `min(room, MAX_KEPT_BYTES)`.

**11. `perf(build): add a tuned release profile`** — `cpu`
No `[profile.release]`: no LTO, no strip, `codegen-units=16`. `lto="thin"`, `codegen-units=1`,
`strip="symbols"` (not `panic=abort`: the runtime catches agent panics). Measured size and build-time
numbers are in the review.

**12. `perf(runtime): one global broadcast copies every run's events to every subscriber`** — `ram` `cpu`
`events.rs:629–641` sends all runs into one channel and `RunSubscription` filters after the clone; the
replay is bounded at ~50 MiB by the code's own estimate. Send `Arc<SinkEvent>` or key the channel by run, and
keep step input/output out of the replay copy.
