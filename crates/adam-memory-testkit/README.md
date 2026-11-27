# adam-memory-testkit

The conformance suites of [`adam_memory::SiMemory`](../adam-memory/README.md) and
[`adam_memory::Memory`](../adam-memory/README.md), as the `si_memory_conformance!` and
`memory_conformance!` macros, and the doubles the suites must reject. Not published
(`publish = false`): it is test code.

[ADR 0081](https://github.com/vymalo/another-adam-rs/blob/main/docs/decisions/0081-an-agent-keeps-a-self-improvement-memory-near-it-and-what-holds-becomes-the-deployments-memory.md)
decisions 4, 5 and 10; slices M1 and M2a: a double that answers another scope, or writes twice for
one key, fails the suite.

## Using it

For L1:

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

For L2:

```rust
use std::sync::{Arc, OnceLock};

use adam_memory::{DynMemory, InProcessLessonStore, InProcessMemory, MemoryScope};
use adam_memory_testkit::LessonPlan;

fn store() -> &'static InProcessLessonStore {
    static STORE: OnceLock<InProcessLessonStore> = OnceLock::new();
    STORE.get_or_init(InProcessLessonStore::new)
}

fn make(plan: &LessonPlan, scope: &MemoryScope) -> DynMemory {
    // A database adapter here would open one database whatever the scope; this reference shares a
    // store so two scopes really are two scopes on one store.
    Arc::new(InProcessMemory::shared(
        plan.settings.clone(),
        scope.clone(),
        InProcessLessonStore::clone(store()),
    ))
}

adam_memory_testkit::memory_conformance!(crate::make);
```

`$make` is a path to a `fn(&LessonPlan, &MemoryScope) -> DynMemory`. The macro calls it once per
case with the case's scope, and twice with two scopes for `two_scopes_never_see_each_other`; an
adapter whose instances share one database must build both against it.

Both suites assume the store holds nothing under their fixed scopes and audiences. A persistent
adapter must clear those keys before the run, or prefix every scope the macro hands it with a
per-run value.

## `Plan`

`Plan::default()` injects a scrubber that replaces `SECRET` (`"conformance-secret"`) with
`REDACTION_MARKER` — so a text that holds it is refused — and removes `TOKEN`
(`"conformance-token"`) — so a case can prove a text is scrubbed and then stored. The bounds are
`Limits::default()`, the ADR's. `Plan::new(scrubber)` and `Plan::with_limits(scrubber, limits)`
give a plan of your own; `Plan` is `#[non_exhaustive]`.

`scope(agent)` and `repository_scope(agent)` are the suite's scopes; `draft(root_cause, id)` is a
valid draft with a remedy, a rationale and a prediction.

`LessonPlan::default()` injects the same scrubber with `LessonLimits::default()`. `LessonPlan::new`
and `LessonPlan::with_limits` give one of your own. `audience(name)` is an audience,
`memory_scope(name)` a configured scope, and `lesson_draft(&audience, root_cause)` a valid draft
with a remedy, a rationale and two sources.

## What the L1 cases prove

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

## What the L2 cases prove

| Case | What it proves |
|---|---|
| `record_then_recall_round_trips` | every field of the lesson, its provenance and its state comes back as written |
| `a_record_lands_once_for_one_key` | one audience and root cause is one lesson: a retried record is one, an update replaces the text and keeps the creation time, and the freshest update is served first |
| `recall_answers_only_its_audience` | another audience's lessons are never served |
| `an_audience_is_not_scrubbed` | the audience is a key: a record and a recall with the same text find the lesson, and an adapter that scrubs the audience misses; a provenance label is stored scrubbed |
| `two_scopes_never_see_each_other` | two configured scopes on one store never read, overwrite or retire each other's lessons — the decisive case of M2 |
| `a_text_is_scrubbed_before_the_write` | the stored text no longer holds what the scrubber removed |
| `a_text_carrying_the_marker_is_refused` | a text the scrub marked, or one that already holds the marker, is refused and stores nothing |
| `a_field_over_the_bound_is_refused` | a text over its byte bound, too many anchors, and too many provenance sources are refused |
| `a_root_cause_over_the_bound_is_refused` | the character bound is counted in characters, not bytes |
| `a_full_audience_refuses_another_lesson` | the audience bound refuses a new root cause, and retiring a lesson frees a slot |
| `a_lesson_refuses_an_address` | a repository, a path, a URL, a host and an e-mail address are refused whatever the audience |
| `a_retired_lesson_is_kept_and_not_served` | a retired lesson is kept and not served, retiring twice is one retirement, an unknown lesson is `NotFound`, and a fresh record serves it again |
| `a_full_audience_refuses_a_revived_lesson` | the bound counts what is served: reviving needs a free slot, and a full audience refuses it |
| `retire_takes_the_stored_root_cause` | `retire` finds a lesson only by the stored, scrubbed root cause; the raw text is `NotFound` |
| `sixteen_writers_share_one_audience` | the port is `Send + Sync` and one audience keeps every writer's lesson |

## The doubles

The suites ship implementations that are correct but for one rule, so the case that fails names
exactly what it proves:

| Double | What it breaks | The case it fails |
|---|---|---|
| `doubles::ScopeBlindSiMemory` | it stores every scope, but answers the first scope it saw to every recall | `recall_answers_only_its_scope` |
| `doubles::DoubleWritingSiMemory` | it writes every attempt twice, the second under a derived id | `a_record_lands_once_for_one_key` |
| `doubles::ScopeBlindMemory` | it writes every lesson into one bucket whatever scope its adapters are configured with | `two_scopes_never_see_each_other` |
| `doubles::DoubleWritingMemory` | it writes every lesson twice, the second under a derived root cause | `a_record_lands_once_for_one_key` |

The testkit's own tests prove them: `ScopeBlindSiMemory` fails
`recall_answers_only_its_scope`, `DoubleWritingSiMemory` and `DoubleWritingMemory` fail
`a_record_lands_once_for_one_key`, and `ScopeBlindMemory` fails
`two_scopes_never_see_each_other`. Two L1 and two L2 cases are shown able to fail, not the whole
suite. The same cases pass against `InProcessSiMemory` and against two `InProcessMemory` adapters on
one `InProcessLessonStore`. This is M1's and M2a's proof: the suites are not ones that any
implementation passes.

## Tests

`cargo test -p adam-memory-testkit` runs the doubles' rejection tests and the reference's passing
cases. The suites themselves run in `adam-memory` (`tests/in_process.rs`, `tests/folder.rs`,
`tests/lesson.rs`).
