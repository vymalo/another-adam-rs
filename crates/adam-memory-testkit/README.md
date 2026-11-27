# adam-memory-testkit

The conformance suite of [`adam_memory::SiMemory`](../adam-memory/README.md), as the
`si_memory_conformance!` macro, and the doubles the suite must reject. Not published
(`publish = false`): it is test code.

[ADR 0081](https://github.com/vymalo/another-adam-rs/blob/main/docs/decisions/0081-an-agent-keeps-a-self-improvement-memory-near-it-and-what-holds-becomes-the-deployments-memory.md)
decision 5 and slice M1: a double that answers another scope or writes twice for one key fails the
suite.

## Using it

```rust
use std::sync::Arc;

use adam_memory::{DynSiMemory, InProcessSiMemory};
use adam_memory_testkit::Plan;

fn make(plan: &Plan) -> DynSiMemory {
    // `InProcessSiMemory` here; `FolderSiMemory::open(path, plan.settings.clone())` elsewhere.
    Arc::new(InProcessSiMemory::new(plan.settings.clone()))
}

adam_memory_testkit::si_memory_conformance!(crate::make);
```

`$make` is a path to a `fn(&Plan) -> DynSiMemory`. In a file under `tests/`, that is `crate::make`;
keep the function out of the generated module. The cases are synchronous: no runtime is needed.

## `Plan`

`Plan::default()` injects a scrubber that replaces `SECRET` (`"conformance-secret"`) with
`REDACTION_MARKER` — so a text that holds it is refused — and removes `TOKEN`
(`"conformance-token"`) — so a case can prove a text is scrubbed and then stored. The bounds are
`Limits::default()`, the ADR's. `Plan::new(scrubber)` and `Plan::with_limits(scrubber, limits)`
give a plan of your own; `Plan` is `#[non_exhaustive]`.

`scope(agent)` and `repository_scope(agent)` are the suite's scopes; `draft(root_cause, id)` is a
valid draft with a remedy, a rationale and a prediction.

## What the cases prove

| Case | What it proves |
|---|---|
| `record_then_recall_round_trips` | every field of the problem and its attempt comes back as written |
| `attempts_with_one_root_cause_share_a_problem` | the root cause is the problem's identity; a second attempt extends it; a failed attempt is kept |
| `a_record_lands_once_for_one_key` | a record with an attempt id the problem holds writes no second attempt |
| `recall_answers_only_its_scope` | another agent's, another caller's and another kind's entries are never served |
| `a_text_is_scrubbed_before_the_write` | the stored text no longer holds what the scrubber removed |
| `a_text_carrying_the_marker_is_refused` | a text the scrub marked, or one that already holds the marker, is refused and stores nothing |
| `a_field_over_the_bound_is_refused` | a text over its byte bound, and too many anchors, are refused |
| `a_root_cause_over_the_bound_is_refused` | the character bound is counted in characters, not bytes |
| `a_full_scope_refuses_another_problem` | the scope bound refuses a new root cause, and a sweep frees the room |
| `a_full_problem_refuses_another_attempt` | the attempt bound refuses one more |
| `an_agent_scope_refuses_an_address` | a repository, a path, a URL, a host and an e-mail address are refused in an agent scope and kept in a repository scope |
| `an_observation_is_stamped_once` | the first observation is the record, a failure is kept, and an unknown problem or attempt is `NotFound` |
| `a_sweep_removes_only_what_is_old` | the cutoff is respected, and recall serves the freshest problem first |
| `sixteen_writers_share_one_scope` | the port is `Send + Sync` and one scope keeps every writer's problem |

## The doubles

The suite ships two implementations that are correct but for one rule, so the case that fails names
exactly what it proves:

| Double | What it breaks | The case it fails |
|---|---|---|
| `doubles::ScopeBlindSiMemory` | it stores every scope, but answers the first scope it saw to every recall | `recall_answers_only_its_scope` |
| `doubles::DoubleWritingSiMemory` | it writes every attempt twice, the second under a derived id | `a_record_lands_once_for_one_key` |

The testkit's own tests run those cases against the doubles and assert they panic, and run the same
cases against `InProcessSiMemory` to assert they pass. This is slice M1's proof: the suite is not
one that any implementation passes.

## Tests

`cargo test -p adam-memory-testkit` runs the doubles' rejection tests and the reference's passing
cases. The suite itself runs in `adam-memory` (`tests/in_process.rs`, `tests/folder.rs`).
