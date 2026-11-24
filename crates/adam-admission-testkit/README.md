# adam-admission-testkit

The conformance suite of [`adam_budget::Admission`](../adam-budget/README.md), as the
`admission_conformance!` macro. Not published (`publish = false`): it is test code.

[ADR 0065](https://github.com/vymalo/another-adam-rs/blob/main/docs/decisions/0065-resource-governance-admission-pools-and-os-limits.md)
decision 3 says what an admission must prove. Each case is a function of this crate, documented
where it lives; the macro writes one `#[tokio::test]` per case into a `mod admission_conformance`
of the crate that calls it, so the suite is compiled once and every implementation runs the same
code.

## Using it

```rust
use std::sync::Arc;

use adam_admission_testkit::Plan;
use adam_budget::DynAdmission;

fn make(plan: &Plan) -> DynAdmission {
    // `LocalAdmission` here; an admission of your own elsewhere.
    Arc::new(adam_admission_local::LocalAdmission::new(plan.budget()))
}

adam_admission_testkit::admission_conformance!(crate::make);
```

`$make` is a path to a `fn(&Plan) -> DynAdmission` ([`adam_budget::DynAdmission`]). In a file under
`tests/`, that is `crate::make`; keep the function out of the generated module. The calling crate
needs `tokio` with the features `macros`, `rt-multi-thread` and `time`, because the macro names
`#[tokio::test(flavor = "multi_thread")]`.

## What the cases prove

| Case | What it proves |
|---|---|
| `never_over_the_limit` | 100 asks at once against a plan's ceiling: exactly the ceiling is admitted, never one more, and the report says so |
| `a_dropped_holder_frees_its_share` | dropping a permit gives the share back at once |
| `a_cancelled_holder_frees_its_share` | a task aborted while it holds a permit gives the share back |
| `a_conversation_keeps_its_share` | one conversation never holds more than `Plan::share` of the pool while another holds its own |
| `a_snapshot_adds_up` | what `snapshot` says is held is what the permits hold, and a waiting ask is counted while it waits |
| `a_refusal_names_a_retry_after` | a full pool refuses with `Refused::Busy { retry_after: Some(..) }`, class `RateLimited`, so the caller parks the run and asks again |
| `the_token_bucket_meters_what_it_cannot_refill` | a metered bucket admits one minute's tokens and then refuses with `Refused::TokensPerMinute`, naming when to come back; a plan that meters no tokens skips it (and the test asserts that the case ran exactly when the plan metered one) |

## `Plan`

`Plan::new(pool, limit)` sets the pool and its ceiling (at most `ASKS`, 100, so the concurrency
case can fill it); `.share(ratio)` sets the most one conversation holds; `.tokens_per_minute(n)`
meters model calls. `Plan::budget()` is the `Budget` the cases count against: that pool at its
ceiling, that share, that rate, every other pool unlimited (the identity of `Budget::lower`).
`Plan::default()` is `Pool::ToolCalls` with four, half to one conversation, no token rate.

`Plan` is `#[non_exhaustive]`: a new field does not break a caller that reads one.

## The cases as functions

`never_over_the_limit(admission, plan)` and its siblings are public, so an implementation's own
test file can call one with a plan of its own (a phone's one run, a server's twenty) beside the
macro's defaults. `fill(admission, plan)` fills a pool and returns the permits, which is what a
case needs when it wants to hold them.

## Tests

The suite has no tests of its own: it is the tests. `cargo test -p adam-admission-local` runs it
against the in-process admission, and the crate's `tests/conformance.rs` adds the cases of its own.
