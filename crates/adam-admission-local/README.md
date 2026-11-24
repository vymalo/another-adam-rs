# adam-admission-local

The in-process `Admission` of adam-rs: one ceiling, held by one process. A `tokio::sync::Semaphore`
per pool and per conversation, and one token bucket for a model's tokens per minute.

[ADR 0065](https://github.com/vymalo/another-adam-rs/blob/main/docs/decisions/0065-resource-governance-admission-pools-and-os-limits.md)
decision 3, the row `LocalAdmission`.

## What it does

```rust
use std::sync::Arc;

use adam_admission_local::LocalAdmission;
use adam_budget::{Budget, DynAdmission, Ratio};

let budget = Budget::builder()
    .runs(2)
    .tool_calls(4)
    .model_calls(2)
    .per_conversation_share(Ratio::HALF)
    .build();
let admission: DynAdmission = Arc::new(LocalAdmission::new(budget));
```

* One pool per `Pool::ALL`, each a semaphore of `Budget::limit(pool)`. `Pool::CommandMemory`'s
  permits are MiB, so its semaphore is weighted and a command reserves what it is expected to use.
* One semaphore per conversation in each pool, of `per_conversation_share` of the pool (at least
  one), so no conversation holds more than its share while another waits. An ask larger than the
  share is admitted without a seat — a command's memory reservation cannot be split — and the
  pool's own permit still bounds the process.
* When the budget names `tokens_per_minute`, one bucket refills continuously and a
  `Pool::ModelCalls` ask with `tokens` charges its estimate. Tokens are a rate, not a reservation:
  a permit that ends (a cancelled call) does not give them back. A single ask over the whole rate is
  `Refused::TooLarge`.
* An ask whose wait runs out is `Refused::Busy { pool, retry_after: Some(RETRY_AFTER) }`: a run
  parks until `retry_after` and asks again. It never fails a run because a slot was busy.
* `snapshot()` reads the semaphores' free permits: what it says is held is exactly what the live
  permits hold, and a waiting ask is counted while it waits.

## Scope

One process. Nothing is persisted and nothing crosses a process: two replicas each admit up to
the whole budget, so a deployment that runs several workers sets each one's ceiling to its share.
Counting several replicas in the run store is `StoreAdmission`, which ADR 0065 leaves undecided.

## Environment

None. The budget is built by the binary that composes it: `adam-service`'s settings do it from the
`MAX_*` and `MODEL_TOKENS_PER_MINUTE` variables (see `crates/adam-service/README.md`).

## Tests

`cargo test -p adam-admission-local` runs the conformance suite
([`admission_conformance!`](../adam-admission-testkit/README.md)) in `tests/conformance.rs`, which
also adds:

* `twenty_runs_fill_a_limit_of_twenty_and_never_a_twenty_first`: twenty runs held, the twenty-first
  refused with `Busy { retry_after }` and class `RateLimited`, then admitted once one ends;
* `an_ask_that_waits_is_admitted_when_room_frees_in_time`: an ask parks, the report counts it, a
  freed permit wakes it;
* `an_ask_over_the_pool_limit_can_never_fit`: `TooLarge`, class `Invalid`, not retryable;
* `one_conversation_takes_half_of_a_pool_of_twenty`;
* `the_token_bucket_meters_a_model_call` and `a_budget_without_a_token_rate_reports_no_bucket`.

The unit tests cover the share's floor of one, the budget it counts against, and the bucket's
continuous refill.
