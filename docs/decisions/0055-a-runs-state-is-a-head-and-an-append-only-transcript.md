# 0055. A run's state is a head and an append-only transcript

Status: **Accepted** (2026-11-19). The owner's decision: option 2 of the optimisation review's P2 (an
append-only transcript, one row per message, a head-only `state`) plus option 1 (file bytes out of the
state into a content-addressed blob store). Schema version 5. Nothing is built yet; it follows the
I/O-layer pull request, which does not change the state schema.

## Context

`RunRecord::state` is the whole runtime envelope (`crates/adam-core/src/store/mod.rs:194-198`), and
every commit rewrites it: one JSONB column on PostgreSQL (`crates/adam-store-postgres/src/lib.rs:561-566`),
one member of the document on MongoDB (`crates/adam-store-mongodb/src/lib.rs:649-656`). The state grows
with the transcript, so the bytes written over a run of `T` turns that add `b` bytes each are
`b·T(T+1)/2`: **20× a delta store at 40 turns, 50× at 100** (the review printed half that; the sum of
`t·b` for `t = 1..T` divided by `b·T` is `(T+1)/2`). File artifacts make it worse: 6 MiB of bytes are
stored as base64 in the state (`crates/adam-runtime/src/events.rs:31-37`), about 8 MiB of text rewritten
on every later commit of that run.

Two constraints decide the shape of the fix, and neither is in the review:

1. **The messages are inside an opaque value.** `Envelope.agent` is a `Value` the runtime never
   interprets (`crates/adam-runtime/src/envelope.rs:22-26`); for the shipped agent it is
   `adam_llm_agent::Conversation`, whose `messages` are the transcript. The runtime cannot split it
   without the agent saying what a message is.
2. **A changed `state` is not old-code safe.** `Envelope::decode` accepts only `v == 1`
   (`envelope.rs:112-116`) and needs a complete `agent`; a v4 worker that claims a run whose state is a
   head commits it `Failed` (`crates/adam-runtime/src/worker.rs:519-527`). There is no flag a v4 process
   reads to learn that it must not step a run.

## Decision

1. **Three homes.** A run's `state` becomes a **head**: the runtime envelope with `agent` reduced to the
   agent's head, the artifact list by reference, and the counters, inbox, output, error and deadlines.
   The agent's transcript is **one row per message** in a body table, in a body set named by the head.
   File bytes — and only a file artifact's bytes — are **content-addressed blobs** (SHA-256) referenced
   by the head; a JSON artifact's `data` stays in the head.
2. **The store contract grows by defaulted methods only** (rule 2): `keeps_bodies`, `body_list`,
   `load_head`, `keeps_blobs`, `put_blob`, `get_blob`, `purge_blobs`. `RunUpdate` gains `head`, `body`
   and `blobs`; `RunUpdate::new` keeps its signature; `NewRun`, `create_run`, `load_run`, `commit_run`,
   `claim_due` and `purge_finished` keep theirs. A store that keeps neither bodies nor blobs behaves
   exactly as it does today.
3. **The atomic boundary does not move.** The CAS stays on `version`. Body rows are written **before**
   the head that names them; rows no head names are ignored and collected. PostgreSQL does it in one
   data-modifying CTE (rows, refs, GC and the CAS in one transaction). MongoDB has no cross-document
   transaction by default, so it inserts the rows, checks that every row was inserted, then CASes; a
   partial insert aborts before the CAS.
4. **A body set is keyed `(run, commit, idx)`, not `(run, seq)`.** A commit's transcript is a set the
   head names, because a later commit may rewrite an early message (compaction); `(run, idx)` alone
   would let a losing writer overwrite the winner's row at the same index.
5. **The split is declared by the agent.** `Agent::split`/`join` and `AgentStarter::split`/`join`,
   defaulted to "no split", with the invariant `join(split(s)) == s` pinned by a conformance test. A
   wrapper that does not delegate them keeps its runs in the old format; the doc says so, as it already
   does for `init_continuing` (`crates/adam-runtime/src/agent.rs:268-271`).
6. **The rollout is expand → contract, two releases.** Expand writes the complete v4 `state` beside the
   head, the body rows and the blobs, and everything that steps a run reads the v4 state; a v4 process
   reads `state` and can step the run, and a v5 reader detects a v4 commit by `head_version != version`
   and falls back. `Runtime::view` may read the head in the same release, because a read cannot write a
   stale state. The contract stops writing `state` (`null`), makes `load_run` return the head, and ships
   only after the expand release has been the only version for a release; from then on a v4 process
   fails a run it touches, which is why it is a separate deploy.
7. **The migration is lazy and per run.** No backfill. A run in the old format is read as it is forever
   if it never commits again; its first commit under the contract release splits it, and a run created
   then starts old-format and splits on its first commit. The old read path stays until a later ADR
   drops it.
8. **Blobs and body rows are collected with the run.** Body rows and blob references cascade with
   `purge_finished`; a blob is deleted when no run references it and it is older than the retention
   cutoff. A deployment that never purges accumulates blobs as it accumulates runs today.

## Rollout slices

Each slice is a pull request, green on its own. S0–S2b are the expand release; S3 is the contract; S4 is
a later cleanup.

| Slice | What lands | The gate that proves it |
|---|---|---|
| **S0 — measure** | `CountingStore` in the testkit; the 50 × 10 KB test in today's shape, asserting `total < 50 × 20 KB` and **recorded red**; `dev/p2-measure.sh` for `pg_relation_size`, `pg_stat_wal` and Mongo `collStats` | the test fails on `main` with the recorded number; the script prints the baseline |
| **S1 — blobs** | `BlobId`, `keeps_blobs`/`put_blob`/`get_blob`, `purge_blobs`; schema v5 part 1; the three implementations; `blob_conformance!`; the runtime uploads file artifacts and passes `RunUpdate.blobs`; `state` unchanged | blob conformance on Memory/Postgres/Mongo; a 4 MiB file round-trips; the v4 fixture still decodes every `state` |
| **S2a — the shadow** | `Agent`/`AgentStarter::split`/`join`, `Conversation`'s split, the `Head` type, schema v5 part 2, `RunUpdate.head/body`; the step path writes the head and rows; **nothing reads the head** | `split/join` conformance; the per-commit reconstruction check; every runtime test green; the v4 fixture green |
| **S2b — the view** | `load_head`, `Stored` (`v1`/`v5`), `Runtime::view` on the head, `RunView.state` = the head, `RunView.artifacts: Vec<ArtifactRef>`, `Runtime::state`/`artifact_bytes`, the A2A blob read and cache | the read-counting test; every A2A test; the write measurement still shows the old cost |
| **S3 — contract** | `state = null` on every commit; `load_run`, `claim_due`, `open_run` and `list_runs` return the head; the step path joins; `deliver`, `cancel_run` and `park_again` write the head they read; `start_continuing`/`on_cancelled` join; the v4 fixture test inverted to assert the documented failure | the 50 × 20 KB test green; `pg_relation_size`/WAL green; the "v4 cannot read a v5 run" test |
| **S4 — later** | drop the v4 `Envelope` read path and the `state` column, after every live run is v5 | — |

## Consequences

* The quadratic write is gone: a commit writes one message plus a head, not the transcript. The
  corrected 20× at 40 turns and 50× at 100 become ~1×. The read of a status becomes one small row.
* **Breaking:** `RunView.artifacts` becomes `Vec<ArtifactRef>` (bytes by reference,
  `Runtime::artifact_bytes` to fetch), and `RunView.state` is the agent's head, not the whole state
  (`Runtime::state(run)` is the joined state). Every in-repo consumer already reads only head members;
  a consumer outside the repository that decodes `RunView.state` as the agent state must switch to
  `Runtime::state`.
* **A release of double writes.** The expand release writes the v4 state and the new representation;
  that is the price of a rollout with no window in which a run cannot be stepped. Measured in the pull
  request, not assumed.
* **MongoDB reads win later than PostgreSQL.** A projected field still reads the whole document from
  WiredTiger, so the Mongo read win arrives with the contract release, when `state` leaves the
  document; the write win is the same release. Stated so nobody measures the expand release on Mongo
  and concludes the design failed.
* A JSON artifact's `data` still lives in the head and is still rewritten per commit; it has no cap
  today. A follow-up either caps it or blobs it.
* An agent with no `KeptStore`/`ImageStore` keeps up to 4 MiB of kept output and images in its head
  (`crates/adam-llm-agent/src/kept.rs:53`); the coder does not. A blob-backed
  `KeptOutputs`/`ImageStore` is the existing seam and a follow-up.
* A third-party `Store` that keeps neither bodies nor blobs keeps working, unoptimised: the runtime
  writes the whole state for it, as today.

## What would make it wrong

* If real runs are short (≤10 turns) the write win is 5.5×, not 50×; still worth it, but the
  measurement must say which workload it quotes.
* If PostgreSQL's TOAST compresses the repetitive JSON well, the disk win is smaller than the row-byte
  win, though the CPU of compress/decompress remains; `pg_stat_wal` and `pg_relation_size` decide it.
* If a caller needs the transcript on every status read, the head-only view is the wrong shape; the
  A2A consumers listed in the plan do not.
* If a v4 process must keep running indefinitely (a pinned old image), the contract release is
  impossible and only the read win lands.
* If the split's `join` is not the exact inverse for some state the agent can produce, the shadow check
  refuses to write the head and the run stays in the old format — safe, but the win never arrives for
  that agent.
