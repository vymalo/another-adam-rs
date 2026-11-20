# 0055. A run's state is a head and an append-only transcript

Status: **Accepted** (2026-11-19). The owner's decision: option 2 of the optimisation review's P2 (an
append-only transcript, one row per message, a head-only `state`) plus option 1 (file bytes out of the
state into a content-addressed blob store). Schema version 5. *Amended 2026-11-19 (S1, S2a)*: the blob
half (S1) and the non-PostgreSQL shadow (S2a) are built; the body is append-only **per generation**
(the S2a amendment below); the PostgreSQL head/body half is pending #361. It follows the I/O-layer
pull request, which does not change the state schema.

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
   The agent's transcript is **one row per message** in a body table, in a body generation named by
   the head (append-only per generation, per the S2a amendment).
   File bytes — and only a file artifact's bytes — are **content-addressed blobs** (SHA-256) referenced
   by the head; a JSON artifact's `data` stays in the head.
2. **The store contract grows by defaulted methods only** (rule 2): `keeps_bodies`, `body_list`,
   `load_head`, `keeps_blobs`, `put_blob`, `get_blob`, `purge_blobs`. `RunUpdate` gains `head`, `body`
   and `blobs`; `RunUpdate::new` keeps its signature; `NewRun`, `create_run`, `load_run`, `commit_run`,
   `claim_due` and `purge_finished` keep theirs. A store that keeps neither bodies nor blobs behaves
   exactly as it does today.
3. **The atomic boundary does not move.** The CAS stays on `version`. Body rows are written **before**
   the head that names them; a commit that loses deletes the rows its `writer` inserted (amended in
   S2a). PostgreSQL does it in one data-modifying CTE (rows, refs, GC and the CAS in one transaction).
   MongoDB has no cross-document transaction by default, so it inserts the rows, checks that every row
   was inserted, then CASes; a partial insert aborts before the CAS.
4. **The body is append-only per generation, keyed `(run, generation, idx)`.** A normal commit appends
   the rows after the old head's `len`; a compaction (a transcript that no longer starts with the old
   one) writes the whole transcript under a new generation, and the old one is collected after a grace
   period. The head names `{generation, len}`. `(run, idx)` alone would let a losing writer overwrite
   the winner's row at the same index, which the `writer` cleanup closes (amended in S2a).
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
| **S1 — blobs** | `BlobId`, `keeps_blobs`/`put_blob`/`get_blob`, `purge_blobs`; schema v5 part 1; the three implementations; `blob_conformance!`; the runtime uploads file artifacts and passes `RunUpdate.blobs`; `state` unchanged. The blob sweep and a commit's ref insert are **not one transaction**: an uncommitted ref is invisible to the sweep, which S2a's CTE closes (see the S1 amendment) | blob conformance on Memory/Postgres/Mongo; a 4 MiB file round-trips; the v4 fixture still decodes every `state` |
| **S2a — the shadow** | `Agent`/`AgentStarter::split`/`join`, `Conversation`'s split, the `Head` type, schema v5 part 2, `RunUpdate.head/body`; the step path writes the head and the **delta** of an append-only generation (a compaction starts a new one); **nothing reads the head**; the non-PostgreSQL half only (the PostgreSQL head/body half is the follow-up after #361) | `split/join` conformance; the per-commit reconstruction check; the body conformance (append/compaction/lost CAS/purge); every runtime test green; the v4 fixture green |
| **S2b — the view** | `load_head`, `Stored` (`v1`/`v5`), `Runtime::view` on the head, `RunView.state` = the head, `RunView.artifacts: Vec<ArtifactRef>`, `Runtime::state`/`artifact_bytes`, the A2A blob read and cache | the read-counting test; every A2A test; the write measurement still shows the old cost |
| **S3 — contract** | `state = null` on every commit; `load_run`, `claim_due`, `open_run` and `list_runs` return the head; the step path joins; `deliver`, `cancel_run` and `park_again` write the head they read; `start_continuing`/`on_cancelled` join; the v4 fixture test inverted to assert the documented failure | the 50 × 20 KB test green; `pg_relation_size`/WAL green; the "v4 cannot read a v5 run" test |
| **S4 — later** | drop the v4 `Envelope` read path and the `state` column, after every live run is v5 | — |

*Amended 2026-11-19 (S0)*: S0 lands the measurement **green**, not recorded red — a red test cannot
land. The 50-commit workload is `counting::measure_transcript_write`, a measurement helper and not a
conformance case; each adapter's `measure_the_transcript_write` test prints its line and asserts only
that the workload ran. The `< 50 × 20 KB` and `max < 20 KB` bounds move to S3: S3 rewrites this
workload to commit a head and a body, then asserts the bounds. The baseline S0 recorded, identical on
Memory, PostgreSQL and MongoDB (the state JSON is the same): `commits=50 total_written=13290322
max_commit=521202 first_read=10434 final_read=521202`. On a fresh database: PostgreSQL
`pg_total_relation_size('adam_test_runs')` 13,688,832 B and `pg_stat_wal.wal_bytes` 14,470,163 B;
MongoDB `collStats(adam_runs).size` 514,331 B and the WiredTiger log 13,207,552 B. `pg_relation_size`
alone is the wrong gate: the state is TOASTed, so the heap stays at 8,192 B; `pg_total_relation_size`
is retained size including bloat, and `pg_stat_wal.wal_bytes` is the bytes written.

*Amended 2026-11-19 (S1)*: two constraints S1 leaves for S2a, recorded so they are not lost.

1. **The sweep is not atomic with a commit.** `purge_blobs`, and the sweep `purge_finished` runs in the
   same call, delete a blob no `adam_run_blob` row names. Under PostgreSQL's `READ COMMITTED` (and in
   MongoDB's find-then-delete) a reference another commit is inserting at that moment is invisible, so
   the sweep can collect a blob that commit is about to name. In S1 this only costs the copy in the
   expand release, because the run's `state` still carries the bytes; the PostgreSQL follow-up after
   #361 writes the refs, the GC and the CAS in one statement and closes the window.
2. **A run seeded from a prior run's state has no reference until its first commit.** `create_run`
   takes no blobs, and the runtime's upload writes the blob, not a reference. A session round started
   from the previous round's state therefore names artifacts no `adam_run_blob` row holds; if the prior
   round is purged before the new run's first commit, the sweep can collect a digest the new run's
   state still carries. S2a either writes the references at create or makes the sweep read the state;
   S1 does not widen `create_run`.

*Amended 2026-11-19 (S2a)*: the body is **append-only per generation**, and the earlier "one set per
commit" language is superseded.

1. **A normal commit writes the delta.** `Conversation::split` returns the whole transcript, but a
   commit whose rows start with the previous transcript passes `BodyWrite { generation: None }`; the
   store appends only `rows[old_len..]` to the current generation. The head names
   `{generation, len}`. The **body's** write win therefore holds in the expand release across a run
   of splitting commits: commit `t` appends `b` bytes, not `t·b`. It does not hold across a headless
   commit or a v4 commit — the next split compacts (point 5) — and every commit still carries the
   full `state` until S3. The first design (a new set per commit) would have written the whole
   transcript every commit, and S3's `< 50 × 20 KB` gate would have been unreachable.
2. **A compaction bumps the generation.** When the new transcript does not start with the previous one
   (a shortening, a dropped turn), the runtime passes `BodyWrite { generation: Some(new) }`; the store
   writes all rows under `new` and collects the old generation after a grace period (`gc_body`). A
   first split of a v4 run also starts a generation.
3. **A loser cleans up after itself.** The runtime fixes one `writer` UUID per step attempt. The store
   inserts the rows with it before the CAS, and a commit that loses deletes the rows and references
   that `writer` added (never a row or digest the winner's head names), so a loser cannot poison the
   next append.
4. **The PostgreSQL upgrade gate.** The PostgreSQL half must gate on `stored < 6` (or check
   `to_regclass('<prefix>body')`), not on the blob step's `stored == Some(4)`: a database stamped 5 by
   S1/S2a has the blob tables but no head or body. The follow-up bumps `SCHEMA_VERSION` to 6.
5. **The delta win holds across splitting commits, not across headless ones.** A commit that writes a
   head appends only the delta. A **headless** commit (`deliver`, `cancel`, `park_again`, a failed
   transition) writes no head, so the next split sees `head_version != version` and starts a new
   generation with the whole transcript. The `head_version` gate stays: resolving the generation from
   a stale head would be unsafe for a v4 step that changed the transcript. The same holds for a v4
   commit. S3 makes those paths write the head they read, and the commit's win (no full `state`)
   arrives there. Measured with a delivery-shaped commit between every step:
   `p2-headless: steps=50 rows=1275 bytes=13288347 generations=50` (12.7 MB) against ~0.5 MB of
   deltas for consecutive splits.
6. **A duplicate slot is resolved by content, then by age.** Two attempts at the same expected
   version can insert different messages at the same `(run, generation, idx)`. On a duplicate key
   MongoDB reads the foreign rows at the attempted slots. If every one has the message this attempt
   would write, it is **adopted** (the completeness check passes and the CAS decides whose head names
   it), so a deterministic re-run makes progress. Otherwise, if every foreign row is older than the
   **orphan grace** (5 minutes, far longer than the insert→CAS gap of a live attempt — milliseconds;
   the default lease is 30 s), the attempt that wrote it died before its CAS, so the rows are deleted
   and the insert retried once. Otherwise a live attempt owns the slots and the commit is a
   `Conflict`. A loser deletes its own rows and refs, and never a row below the winner's head's
   `len`; the PostgreSQL follow-up does the same inside its transaction. A live concurrent attempt's
   rows stay a `Conflict` — the runtime drops the result and the next claim steps the run again —
   but rows from an attempt that died in the insert→CAS window are adopted when identical or removed
   after the grace, so they no longer wedge the run. The grace is not free: after such a crash the
   run is re-stepped and each attempt's result is dropped until the grace expires — minutes of
   wasted store round trips; the journaled steps replay, so no model call or tool side effect is
   repeated. A live attempt whose insert stalls for more than the grace (or a clock skew larger
   than it) can have its rows collected while it is alive: the writer-keyed rows
   (`(run, generation, writer, idx)`, the head naming its writer) in S2b/S3 remove the shared slot
   and this adoption heuristic. An older binary that still wedges one is cancelled by an operator.

## Consequences

* The **body's** quadratic write is gone across splitting commits: a split writes one message plus a
  head, not the transcript (amended S2a: the append-only generation is what makes this true in the
  expand release too). A headless commit or a v4 commit still compacts the transcript until S3, and
  every commit still carries the full `state`, so the commit's full win (no `state`) arrives at S3.
  Across splitting commits the corrected 20× at 40 turns and 50× at 100 become ~1× for the body. The
  read of a status becomes one small row.
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
