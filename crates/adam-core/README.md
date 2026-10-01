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
| `Store` (trait) | `migrate`, `create_run`, `load_run`, `commit_run` (compare-and-swap on `version`), `open_run_for_conversation`, `journal_get`/`journal_put`/`journal_list`, `claim_due` (with a `ClaimScope` and the runs the caller is `busy` with), `renew_lease`, `release_lease`, `purge_finished` |
| `DynStore` | `Arc<dyn Store>`, the handle the runtime holds |
| `RunRecord`, `NewRun`, `RunUpdate`, `RunStatus`, `RunId` | a run and how to create or advance one |
| `JournalEntry` | the recorded outcome of one step, keyed by `(run, seq)` |
| `ClaimScope` | `Any` (default) or `Pinned`, the scope of a `claim_due`. Closed: no `#[non_exhaustive]` |
| `Lease` | a run claimed by a worker until a deadline |
| `StoreError`, `StoreResult` | `AlreadyExists`, `NotFound`, `Conflict`, `ConversationBusy`, `NonDeterminism`, `InvalidInput`, `Corrupt`, `Backend { class, source }`; `#[non_exhaustive]`, see *Errors* |
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

The semantics (CAS commits, first-writer-wins journal, leases, one open run
per conversation) are described in the [root README](../../README.md#the-model).

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

`InvalidInput` is data the store cannot hold (a NUL in a `JSONB` string, an
integer above `i64::MAX`, a bad table prefix); `Corrupt` is data read back that
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

`MemoryStore` is run through the conformance suite by
`crates/adam-store-testkit/tests/memory.rs`. Behavioural tests of the types
live next to the code; those in `src/store/mod.rs` (`class_table`,
`retryable_is_derived_from_the_class`,
`backend_display_does_not_repeat_its_source`) pin the class of every
`StoreError` variant with an exhaustive `match`. Database gating is described in the
[root README](../../README.md#testing).

## See also

[`adam-store-testkit`](../adam-store-testkit/README.md),
[`adam-runtime`](../adam-runtime/README.md) (the consumer of `Store`),
[`adam-error`](../adam-error/README.md).
