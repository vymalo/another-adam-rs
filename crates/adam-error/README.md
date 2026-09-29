# adam-error

The error classification model every adam-rs crate shares. A pure leaf crate: no I/O, no async,
no dependencies (`thiserror` is a dev-dependency of its tests).

A library error enum says **what happened** in its variants; [`ErrorClass`] says **what to do**
about it. Retry loops, HTTP or A2A status mapping and process exit codes all decide from the
class, never from a variant, so adding a variant costs one class decision and `#[non_exhaustive]`
costs nothing downstream.

## What is in it

| Item | Purpose |
|---|---|
| `ErrorClass` | The ten classes below. `#[non_exhaustive]`, `Copy`. |
| `Classify` | `class()` (required), `retry_after()` (optional), `is_retryable()` (derived from the class, never hand-written). |
| `BoxError` | `Box<dyn Error + Send + Sync + 'static>`: how an adapter keeps a foreign (driver, SDK) error as a `#[source]` without naming its type in a trait signature. |
| `report(&dyn Error)` | `"a: b: c"`, the error and its whole source chain on one line. The only sanctioned way to flatten a chain. |

## The classes

| Class | Meaning | Retry | Alert | A2A error (adam-a2a) |
|---|---|---|---|---|
| `Transient` | May succeed later: network, 5xx, timeout, pool, crashed child | yes, with backoff | no | `internal("backend temporarily unavailable")` |
| `RateLimited` | Slow down; honour `retry_after()` | yes, after `max(backoff, retry_after)` | no | `internal("backend temporarily unavailable")` |
| `Conflict` | Lost an optimistic-concurrency race: re-read, retry | yes, at once (bounded) | no | `internal("backend temporarily unavailable")` |
| `Invalid` | The input is wrong; the same input never succeeds | no | no | `invalid_params` |
| `NotFound` | Absent, or invisible to this caller | no | no | `task_not_found` |
| `Rejected` | Valid input, but the target's state forbids it (finished, busy, exists) | no | no | `invalid_params`, or `task_not_cancelable` for a cancel |
| `Unauthenticated` | Credentials missing or refused | no | no | `internal("internal error")` |
| `Unsupported` | The peer does not offer this operation | no | no | `internal("internal error")` |
| `Corrupt` | Stored or received data breaks an invariant | no | **yes** | `internal("internal error")` |
| `Internal` | A bug, or unclassified | no | **yes** | `internal("internal error")` |

`ErrorClass::is_retryable()` is true for `Transient`, `RateLimited` and `Conflict`;
`is_permanent()` is its opposite; `should_alert()` is true for `Corrupt` and `Internal`.

## Exit codes

Binaries walk the `anyhow` chain for the root cause and exit with a sysexits.h code: 78
(`EX_CONFIG`) for a configuration error, 69 (`EX_UNAVAILABLE`) for a transient dependency
failure at boot such as an unreachable Postgres, 71 (`EX_OSERR`) for an OS error such as a
listener that cannot bind, 70 (`EX_SOFTWARE`) for an internal error, and 1 otherwise.

## Rules for an error enum

* `#[derive(Debug, thiserror::Error)] #[non_exhaustive]`, and `impl Classify`.
* A message describes its own layer only. Never interpolate a `#[source]` into it: `report()`
  prints the chain, so a message that also printed its source would appear twice.
* Wrap a foreign error as `#[source] source: BoxError`; wrap an in-workspace error with
  `#[from]`. Where a variant has no lower error, do not invent one.
* Test every `Classify` impl with an exhaustive `match` over the variants, so a new variant
  forces a class decision.
* Flatten (`report`, `to_string`) only at a trust or persistence boundary: the journal, a
  response to a client, a log line.

## Tests

`cargo test -p adam-error` runs three unit tests: the class truth table (an exhaustive `match`
over `ErrorClass` against `is_retryable`, `is_permanent` and `should_alert`), `report` on a
three-level chain, and the `Classify` default methods.
