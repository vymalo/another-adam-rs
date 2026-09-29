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
| `Store` (trait) | `migrate`, `create_run`, `load_run`, `commit_run` (compare-and-swap on `version`), `open_run_for_conversation`, `journal_get`/`journal_put`/`journal_list`, `claim_due`, `renew_lease`, `release_lease`, `purge_finished` |
| `DynStore` | `Arc<dyn Store>`, the handle the runtime holds |
| `RunRecord`, `NewRun`, `RunUpdate`, `RunStatus`, `RunId` | a run and how to create or advance one |
| `JournalEntry` | the recorded outcome of one step, keyed by `(run, seq)` |
| `Lease` | a run claimed by a worker until a deadline |
| `StoreError`, `StoreResult` | `Conflict`, `NotFound`, `AlreadyExists`, `ConversationBusy`, `NonDeterminism`, `InvalidData`, `Backend` |
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

## Features and environment

No Cargo features. The `testing` helpers read `ADAM_TEST_REQUIRE_DB`
(`1` or `true`): a database-gated test whose variable is unset then fails
instead of skipping.

## Tests

`MemoryStore` is run through the conformance suite by
`crates/adam-store-testkit/tests/memory.rs`. Behavioural tests of the types
live next to the code. Database gating is described in the
[root README](../../README.md#testing).

## See also

[`adam-store-testkit`](../adam-store-testkit/README.md),
[`adam-runtime`](../adam-runtime/README.md) (the consumer of `Store`).
