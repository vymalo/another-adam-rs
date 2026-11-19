# adam-core

The seam every durable-state backend implements: the `Store` trait, the run
and journal types, and an in-memory reference store.

## Where it sits

Ports and adapters: this crate is the **port** for durable state. Adapters
are [`adam-store-postgres`](../adam-store-postgres/README.md) and
[`adam-store-mongodb`](../adam-store-mongodb/README.md); the reference adapter
`MemoryStore` lives here. Every adapter is checked by the shared suite in
[`adam-store-testkit`](../adam-store-testkit/README.md). It depends on no
database driver.

## API at a glance

| Item | What |
|---|---|
| `Store` (trait) | `migrate`, `create_run`, `load_run`, `load_head` (a run as an observer reads it: the head when it is current, else the state; defaults to `load_run`), `run_header` (a run without its state, from one projected read where the adapter can), `commit_run` (compare-and-swap on `version`), `open_run_for_conversation`, `journal_get`/`journal_put`/`journal_list`, `claim_due` (with a `ClaimScope` and the runs the caller is `busy` with), `renew_lease`, `release_lease`, `lease_until` (when a run's lease ends, if it has one), `purge_finished` (also deletes the push configs, body rows and blob references of the runs it purges, and collects unreferenced blobs), `list_runs` and `count_runs` (a caller's runs, newest update first, by keyset: `RunQuery`, `ConversationScope`), the five **push-notification** methods `push_put`, `push_list`, `push_delete`, `push_claim_due`, `push_commit` (see *Push configurations*), the four **blob** methods `keeps_blobs`, `put_blob`, `get_blob`, `purge_blobs` (see *File bytes*), and the three **body** methods `keeps_bodies`, `body_list`, `gc_body` (see *Transcripts*) |
| `DynStore` | `Arc<dyn Store>`, the handle the runtime holds |
| `RunRecord`, `NewRun`, `RunUpdate`, `RunStatus`, `RunId` | a run and how to create or advance one |
| `BodyRow`, `BodyWrite` | one transcript row (`idx`, `message`) and the set a commit writes (`commit`, `rows`), keyed `(run, commit, idx)` in the store (see *Transcripts*) |
| `BlobId` | the content address of a file artifact's bytes: a SHA-256 as 64 lower-case hex, opaque in this crate (see *File bytes*) |
| `RunHeader` | a run without its state: `run_header`'s answer, with `claimed(now)` (a runnable run and a live lease) for a reader that only needs the run's own bookkeeping. `adam_core::claimed(status, lease_until, now)` is that rule, shared with a runtime view |
| `JournalEntry` | the recorded outcome of one step, keyed by `(run, seq)` |
| `ClaimScope` | `Any` (default) or `Pinned`, the scope of a `claim_due`. Closed: no `#[non_exhaustive]` |
| `Lease` | a run claimed by a worker until a deadline |
| `RunQuery`, `ConversationScope` | what `list_runs` and `count_runs` select: always scoped (`Prefix` of an owner's conversations, or `Exact`), optionally by run statuses and last update, one page after a `(updated_at, id)` position |
| `NewPushConfig`, `PushRecord`, `PushProgress`, `PushState` | an A2A push-notification configuration and how far its delivery got; `PushState` (`Active`, `Done`, `GaveUp`) is closed on purpose |
| `StoreError`, `StoreResult` | `AlreadyExists`, `NotFound`, `Conflict`, `ConversationBusy`, `NonDeterminism`, `InvalidInput`, `Corrupt`, `Backend { class, source }`; `#[non_exhaustive]`, see *Errors* |
| `storable` | the one rule every adapter applies before it writes a JSON value: a NUL in a string or a key becomes U+FFFD, so a state or a journal payload is the same value in every backend ([ADR 0051](../../docs/decisions/0051-a-nul-is-stored-as-the-replacement-character.md)) |
| `MemoryStore` | in-memory `Store`, the reference implementation of the suite |
| `testing` (`#[doc(hidden)]`) | `test_env`, `skipped`, `require_db`: gate for database-backed tests, not part of the supported API |

```rust
use adam_core::{MemoryStore, NewRun, RunStatus, RunUpdate, Store};
use serde_json::json;

let store = MemoryStore::new();
store.migrate().await?;
let run = store
    .create_run(NewRun::new("support-bot", json!({"turn": 0})).conversation("chat-42"))
    .await?;
let run = store
    .commit_run(run.id, run.version, RunUpdate::new(RunStatus::Parked, json!({"turn": 1})))
    .await?;
```

`create_run` and `commit_run` return the record **without its state** (`state` is `null`): the caller
supplied it, and the state is the largest member of the record. Read the run for the stored state.
Of the run writes, `journal_put` is the exception: it returns the payload it stored, which is what the
loop decodes the step's outcome from (a store may have replaced a NUL in it,
[ADR 0051](../../docs/decisions/0051-a-nul-is-stored-as-the-replacement-character.md)); the push
methods return the cleaned configuration and cursor too.

The semantics (CAS commits, first-writer-wins journal, leases, one open run
per conversation) are described in the [architecture](../../docs/architecture.md#the-mental-model).

## Claim scope and the run owner

`Store::claim_due(agents, worker, scope, now, ttl, limit)` takes a closed
`ClaimScope`:

| Scope | Claimable runs | Owner |
|---|---|---|
| `Any` (default) | every due run without a live lease | neither read nor written: claiming works as before owners existed |
| `Pinned` | due runs without a live lease **and** with no owner or with `worker` as owner | the first claim of a run without an owner sets `worker` as the owner |

A run's *owner* is scheduling data next to the lease, not part of `RunRecord` or the run state.
`release_lease` and `commit_run` leave it alone, so a released run comes back only to the same
worker. It is never cleared: a pinned run whose owner is gone is stranded (nothing adopts it;
see [ADR 0002](../../docs/decisions/0002-workspace-placement.md)). `adam-core` does not know
about `Placement`; the host maps `Placement::pins_runs()` to `ClaimScope::Pinned`
([`adam-host`](../adam-host/README.md)). The signature change is breaking for anyone who
implements `Store`; the conformance cases in
[`adam-store-testkit`](../adam-store-testkit/README.md) prove an implementation.

## Listing runs

`Store::list_runs(&RunQuery)` returns one page of runs ordered by `updated_at` descending and then `id` descending (what A2A's
`ListTasks` needs: most recently updated first, made total by the id), starting strictly after `query.after`, and
`Store::count_runs` how many match the filters (ignoring `after` and `limit`). The scope is required, so a listing cannot leak
another owner's runs by leaving a filter out: `ConversationScope::Prefix` for everything of an owner (the A2A server uses
`<subject>:`), `Exact` for one conversation. Every adapter serves a page from an index (Postgres compares the conversation id
in the `"C"` collation so a prefix is a range, MongoDB uses an anchored escaped prefix). Two more **required** `Store` methods:
breaking for implementers, proved by the `list_runs_*` and `count_runs_*` conformance cases.

## Reading a header

`Store::run_header(id)` answers a `RunHeader` — the run's id, agent,
conversation, parent, status, wake time, version, timestamps and lease, **never its state** — or
`None`. A reader that only needs the run's own bookkeeping (a parent check, a
step's cancel watch, a listing that builds its view from the record it already has) reads this
instead of `load_run`: the state is the largest member of a run, and for an `LlmAgent` it is the
whole transcript. `RunHeader::claimed(now)` applies the same rule as `RunView::claimed`: only a
runnable run, and only a lease that has not run out.

The method is **defaulted**, so it breaks no implementer: the default reads `lease_until` and
`load_run` and projects the record (two reads, in that order, for the same interleaving reason
`Runtime::view` reads them in that order). Both shipped adapters override it with one statement that
projects the header columns and the lease, and the conformance case
`run_header_matches_the_record` proves an implementation agrees with `load_run` + `lease_until`.

## File bytes

A file artifact's bytes are content-addressed: `adam_runtime::blob_id(&bytes)` is their SHA-256 as a
`BlobId`, and the runtime uploads them before the commit that names them, so they survive the later
commits of that run and are collected once no run references them. This crate keeps the address and
the four **defaulted** `Store` methods; it has no hash dependency, so it never computes a digest
itself.

| Method | What |
|---|---|
| `keeps_blobs()` | whether the store keeps file bytes out of a run's state. `false` (the default) means the bytes live in the state, as before |
| `put_blob(id, bytes)` | store `bytes` under `id`; idempotent, and the bytes behind an id never change. A store that keeps none answers `InvalidInput` |
| `get_blob(id)` | the bytes, or `None` when none are stored (never written, or collected). A store that keeps none answers `InvalidInput`, as `put_blob` does |
| `purge_blobs(before)` | delete the blobs written before `before` that no run references; returns how many |

`RunUpdate` gained a public `blobs: Vec<BlobId>` field: **the ids the commit adds**, a delta the
store unions with the ids it already recorded, so a commit that names none keeps the earlier ones.
**A hand-built `RunUpdate { .. }` literal stops compiling**: add `blobs: Vec::new()` or use
`RunUpdate::new(..)` / `.blobs(..)`, which keep their meaning. `RunUpdate::new` is unchanged and
sets it empty, so every existing caller compiles.

`purge_finished` keeps its signature and extends its contract: a run's references go with it, and
the blobs no surviving run references and that were written before its `before` are deleted in the
same sweep. A digest shared by several runs survives until the last one is purged.

## Transcripts: the head and the body

The expand release of [ADR 0055](../../docs/decisions/0055-a-runs-state-is-a-head-and-an-append-only-transcript.md)
adds the types and the **defaulted** methods a store needs to keep a run's transcript apart from its
head; `state` is still the whole envelope, and the runtime reads neither in this slice.

| Method | What |
|---|---|
| `keeps_bodies()` | whether the store keeps a transcript beside the head (`body_list`, `RunUpdate::body`). `false` (the default) means `RunUpdate::state` is the whole run, exactly as before |
| `body_list(run, generation)` | the rows of one generation, ordered by `idx`; empty when it does not exist. A store that keeps none answers empty |
| `gc_body(run, keep_generation, before)` | delete the superseded generations of `run` written before `before`; returns how many rows. The generation a head names is never collected. A store that keeps none answers `0` |
| `load_head(id)` | the run as an **observer** reads it: its `state` is the v5 head when the store has a current one (`head_version == version`), and the whole state otherwise. Defaults to `load_run`, so a store that keeps no head answers exactly as it does today. Nothing in the runtime calls it in the expand release: the tests do, and the read path (S2b) will |

`BodyRow { idx, message }` is one row; `BodyWrite { generation, rows, writer }` is what a commit
writes. A generation is **append-only**: `generation: None` appends `rows[old_len..]` to the current
head's generation (or starts one when there is no head), and `Some(new)` is a compaction that writes
the whole transcript under `new`. `writer` is the attempt's id: a commit that loses the CAS deletes
the rows (and references) that writer added, so a loser cannot poison the next append. The head names
`{generation, len}`; the store resolves the generation and patches the head.

`RunUpdate` gained two public fields: `head: Option<Value>` (the v5 head this commit writes; `None`
leaves the stored head and its version alone, so a delivery or a cancel keeps the format it read) and
`body: Option<BodyWrite>` (the rows this commit writes; `None` writes none). **A hand-built
`RunUpdate { .. }` literal stops compiling**: add `head: None, body: None` or use
`RunUpdate::new(..)`, which keeps its signature and sets both, and the `.head(..)` / `.body(..)`
builders. A commit that writes rows writes them **before** the CAS, and an adapter checks that all
of them landed: a head must never name a generation that is missing a row.

`purge_finished` keeps its signature and extends its contract: a run's body rows go with it.

## Push configurations

A2A push notifications must survive a restart and a second replica, so the webhook a client
registered for a task, and how far its delivery got, are kept in the store beside the run
([ADR 0030](../../docs/decisions/0030-a2a-push-notifications-list-tasks-extended-card-signatures.md)).
The store knows nothing of A2A: `config` and `cursor` are opaque JSON, and what it owns is the
scheduling, as for runs.

| Method | What |
|---|---|
| `push_put(NewPushConfig)` | create or replace `(run, id)`; the run must exist (`NotFound`). New: `Active`, version 1, due at once. Replacing resets state, attempts and error, drops the lease and bumps the version |
| `push_list(run)` | the run's configs, by id |
| `push_delete(run, id)` | idempotent; whether it existed |
| `push_claim_due(agents, worker, now, ttl, limit)` | lease active configs that are due, earliest `next_attempt_at` first; exclusive while the lease lives |
| `push_commit(run, id, expected_version, PushProgress)` | compare-and-swap on the version, drops the lease; `Conflict` on a stale version, `NotFound` when deleted |

A config goes with its run (`purge_finished`). **The store holds what the client gave it, webhook
credentials included, and does not encrypt them**: protect the database like the runs. The five
methods are new **required** methods of `Store`: breaking for anyone who implements it (the
conformance cases `push_*` in [`adam-store-testkit`](../adam-store-testkit/README.md) prove an
implementation).

## Runs the caller is stepping

`Store::claim_due(.., busy, ..)` never returns a run listed in `busy`, whatever its lease says, and
such a run takes no slot of `limit`. The runtime passes the runs it is stepping. A step can outlive
its lease (a renewal that failed, a clock that jumped), and a claim of that run by the worker that
still holds it would lease it a second time: the claim returns a snapshot that the running step is
about to make stale, so a second step could start on it, and the release at the end of the first
step (it matches the worker, not the claim) would clear the new lease. Another worker is not
affected: it claims the run once the lease has expired, as before. The conformance case
`claim_skips_busy_runs` in [`adam-store-testkit`](../adam-store-testkit/README.md) proves an
implementation, for both scopes. The signature change is breaking for anyone who implements
`Store`.

## Errors

`StoreError` implements `adam_error::Classify`; callers decide from the class,
never from the variant (see [`adam-error`](../adam-error/README.md)).

| Variant | Class |
|---|---|
| `AlreadyExists`, `ConversationBusy` | `Rejected` |
| `NotFound` | `NotFound` |
| `Conflict` | `Conflict` |
| `NonDeterminism`, `Corrupt` | `Corrupt` |
| `InvalidInput` | `Invalid` |
| `Backend { class, source }` | the `class` the adapter chose |

`InvalidInput` is data the store cannot hold (an integer above `i64::MAX`, a
bad table prefix); `Corrupt` is data read back that
breaks an invariant (an unknown status, a negative version, a vanished journal
entry). `Backend` keeps the driver's error as its `source` (a `BoxError`, so no
driver type is in this crate) and its message does not repeat it. An adapter
builds it with `StoreError::unavailable(e)` (`Transient`),
`StoreError::internal(e)` (`Internal`) or `StoreError::corrupt_source(e)`
(`Corrupt`). Only `Conflict` and a transient `Backend` are retryable.

## Features and environment

No Cargo features. The `testing` helpers read `ADAM_TEST_REQUIRE_DB`
(`1` or `true`): a database-gated test whose variable is unset then fails
instead of skipping.

## Tests

`MemoryStore` (which implements the push methods too) is run through the conformance suite by
`crates/adam-store-testkit/tests/memory.rs`. Behavioural tests of the types
live next to the code; those in `src/store/mod.rs` (`class_table`,
`retryable_is_derived_from_the_class`,
`backend_display_does_not_repeat_its_source`) pin the class of every
`StoreError` variant with an exhaustive `match`. `the_defaults_keep_no_bodies_and_serve_the_whole_state`
pins the three transcript defaults on a store that overrides none of them, and
`a_head_round_trips_and_a_headless_commit_makes_it_stale` and
`the_body_gc_keeps_the_current_set_and_collects_an_old_superseded_one` pin `MemoryStore`'s head and
its GC with an explicit cutoff. Database gating is described in the
[testing guide](../../docs/guides/testing.md).

## See also

[`adam-store-testkit`](../adam-store-testkit/README.md),
[`adam-runtime`](../adam-runtime/README.md) (the consumer of `Store`),
[`adam-error`](../adam-error/README.md).
