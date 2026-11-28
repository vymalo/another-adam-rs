# adam-memory

The two memories of adam-rs. **L1** is the self-improvement memory next to the agent: the problems
it met, the attempts it made, and what it observed; its only adapter is a folder of plain files the
deployment gives the agent. **L2** is `Memory`: the lessons that held across L1 scopes, shared across
sessions and reached over the network, keyed within the adapter's configured scope by audience.

[ADR 0081](https://github.com/vymalo/another-adam-rs/blob/main/docs/decisions/0081-an-agent-keeps-a-self-improvement-memory-near-it-and-what-holds-becomes-the-deployments-memory.md)
decisions 2 to 5 and 10; slices M1 and M2a (the L2 port and its kit; the L2 adapters are M2b).

## What is in it

| Item | Purpose |
|---|---|
| `SiMemory` | The L1 port: `record`, `observe`, `recall`, `sweep`. `Send + Sync + 'static`; no driver type, no path, no client in a signature. |
| `DynSiMemory` | `Arc<dyn SiMemory>`, what a host or a tool is given. |
| `Scope` | The key: `deployment`, `caller`, `agent` and `Kind`. No entry crosses a caller. |
| `Kind` | `Agent`, or `Repository(Repository)`. A closed set: a new kind must fail to compile everywhere. |
| `Repository` | `host`, `owner`, `name`; the key is `repository:<host>/<owner>/<name>`. |
| `Problem` | A short root cause, its attempts, `created` and `updated`. The root cause is the problem's identity inside the scope. |
| `Attempt` | The remedy, the rationale, the prediction written before the outcome, the observation stamped after it, provenance, anchors as text, and `recorded`. A failed attempt is kept. |
| `AttemptDraft` | What a caller writes; the adapter scrubs it, checks it and stamps it. |
| `AttemptId` | The idempotency key, `<run>/tool:<call id>`: a record with an id the problem holds is a no-op. |
| `Observation`, `Outcome` | What an observer saw (`Success` or `Failure`) and a short account. The first observation is the record. |
| `Provenance` | The run and conversation that wrote the attempt. |
| `Settings`, `Limits`, `Scrubber` | The injected scrub and the bounds an adapter is built with. |
| `FolderSiMemory` | L1's only adapter: one directory, a subdirectory per scope, one JSON file per problem, a temp file and a rename per write. |
| `InProcessSiMemory` | The in-process reference the suite and a test use. |
| `Memory` | The L2 port: `record`, `recall`, `retire`. `Send + Sync + 'static`; no driver type, no URL, no client in a signature. |
| `DynMemory` | `Arc<dyn Memory>`, what a host or a tool is given. |
| `MemoryScope` | The scope an L2 adapter is configured with; every key and query carries it. Two scopes on one store never see each other's lessons. `MemoryScope::validate` is what a call checks first. |
| `Audience` | Who a lesson is served to inside its scope; the promotion pass computes it, the port keeps it opaque. `Audience::validate` is what `recall` and `retire` check first. |
| `Lesson` | A root cause, its remedy and rationale, anchors as text, provenance, `state`, `created`, `updated` and `retired_at`. |
| `LessonDraft` | What a caller writes; the adapter scrubs it, checks it and stamps it. |
| `LessonProvenance` | The L1 scopes (opaque labels) whose evidence made the lesson, and the review that approved it. |
| `LessonState` | `Proposed`, `Active`, `Retired`. A closed set; a recall serves `Active` only. |
| `LessonSettings`, `LessonLimits` | The injected scrub and the bounds an L2 adapter is built with. |
| `prepare_lesson` | Public preparation **for adapters**: scrub, check and stamp a draft into a served lesson with the port's own rules. `InProcessMemory` uses this same function; a database adapter prepares a draft with it and stores the stamped lesson. |
| `InProcessMemory` | The in-process L2 reference the suite and a test use. |
| `InProcessLessonStore` | The storage several `InProcessMemory` adapters share, so a test can put two scopes on one store. |

The crate is the two ports, the values, L1's in-folder adapter and the two in-process references.
The PostgreSQL L2 adapter is [`adam-store-postgres`](../adam-store-postgres/README.md)'s `memory`
feature (M2b, built); the MongoDB and Redis adapters remain unbuilt. A store reached over the
network is L2 (ADR 0081 decision 2). Adapters in other crates use this crate's public
write-path pieces: `prepare_lesson`, `MemoryScope::validate`, `Audience::validate` and
`MemoryError::backend`.

```rust
use adam_core::RunId;
use adam_memory::{
    AttemptDraft, AttemptId, InProcessSiMemory, Provenance, Scope, Scrubber, Settings, SiMemory,
};

let memory = InProcessSiMemory::new(Settings::new(Scrubber::new(|text| text.to_owned())));
let scope = Scope::agent("deployment", "caller", "adam-coder");
memory
    .record(
        &scope,
        AttemptDraft::new(
            "the lockfile was stale",
            AttemptId::new("run-1/tool:call-1"),
            Provenance::new(RunId::new(), "conversation-1"),
        )
        .remedy("regenerate the lockfile")
        .predicted("the build resolves its dependencies"),
    )
    .expect("a valid write");
assert_eq!(memory.recall(&scope).expect("a readable memory").len(), 1);
```

```rust
use adam_memory::{
    Audience, InProcessMemory, LessonDraft, LessonProvenance, LessonSettings, Memory, MemoryScope,
    Scrubber,
};

let memory = InProcessMemory::new(
    LessonSettings::new(Scrubber::new(|text| text.to_owned())),
    MemoryScope::new("release-1"),
);
let audience = Audience::new("adam-coder");
memory
    .record(
        LessonDraft::new(
            audience.clone(),
            "the lockfile was stale",
            LessonProvenance::new(["scope-a", "scope-b"], "the reviewer"),
        )
        .remedy("regenerate the lockfile"),
    )
    .expect("a valid write");
assert_eq!(memory.recall(&audience).expect("a readable memory").len(), 1);
memory
    .retire(&audience, "the lockfile was stale")
    .expect("the lesson is there");
assert!(memory.recall(&audience).expect("a readable memory").is_empty());
```

## The key

`Scope` is `deployment`, `caller`, `agent` and `Kind`; the key does not name a worker. Two workers
that mount one folder are one L1; two workers with two folders are two, as two cores have two
caches. A repository-scoped entry is served only to a run that prepared that repository (the tool
layer, M3, chooses the scope; this crate stores it).

The in-folder layout is `<root>/<slug>-<digest of the key>/<slug>-<digest of the root cause>.json`.
The digest is the first 16 hexadecimal digits of SHA-256, so a rebuild computes the same names; the
slug is only for a person reading the folder. Every file carries its scope, and a file that belongs
to another scope is `Corrupt` rather than served.

### The L2 key

An L2 adapter is configured with one `MemoryScope`; the deployment chooses it (one store several
deployments share, or one per `adam-coder`). Every key and every query carries it, so two adapters
over one database with two scopes never see each other's lessons. Inside the scope a lesson is keyed
by its `Audience` and its root cause; a second record for the same key updates the lesson rather
than opening another, and a record for a retired lesson serves it again when a free slot remains.
The port keeps the audience opaque: the promotion pass computes it, and a lesson never widens inside
an adapter.

## Bounds

`Limits::default()` is the ADR's: 64 problems per scope, 16 attempts per problem, 1 KiB a text
field, 200 characters a root cause, and 16 anchors an attempt (the ADR bounds every list but names
no anchor count; this slice's value is 16).

A write that would break a bound is **refused**, not evicted, so the caller sees it. A sweep removes
the attempts recorded before its cutoff and the problems it leaves empty, which frees the room
again. Recall serves the freshest problem first.

`LessonLimits::default()` gives L2 128 lessons an audience, 1 KiB a text field, 200 characters a
root cause, and 16 anchors and 16 sources an entry. The lesson and field bounds are the ADR's; the
anchor and source counts are not — the ADR bounds every list but names no count, so this slice's
value is 16 each. The bound counts the lessons an audience **serves**; a retired lesson is kept and
does not count, so retiring frees a slot, and a write that would make the audience serve more than
the limit is refused. There is no time-based sweep at L2: a lesson's evidence turns and it is
retired, not deleted.

## Scrubbing and refusal

Every text passes through the injected `Scrubber` before it is stored. A text that still holds
`REDACTION_MARKER` after the scrub is refused (`MemoryError::Redacted`), not stored: the caller
sees the refusal, and nothing is written. An **agent-scoped** text may not name a repository, a
path, a URL, a host or an e-mail address (`MemoryError::NamesAddress`); a repository-scoped one
may. The detector knows these shapes: a scheme (`://`), an address with an `@` and a dotted domain,
a host with a dotted name or `localhost` (with or without a port), an absolute or relative path,
a drive letter, a token with two or more slashes, and a token with a code extension. It is
conservative: a false positive refuses a write, which decision 12 makes the safe side.

An L2 lesson's own text (its root cause, remedy, rationale and anchors) may not name any of those
shapes **whatever its audience** (decision 5). The audience is a key, validated and bounded, never
scrubbed: `recall` and `retire` look it up by the same text, so a scrub would break the lookup. The
provenance labels are scrubbed and bounded; nothing looks them up, so they may name a tool or a
model.

`adam-memory` does **not** depend on `adam-llm-agent`: `adam-llm-agent` will depend on this crate
(the `si_recall`/`si_record` tools), and a dependency back would be a cycle. The marker is a
constant here, and a test in `adam-llm-agent` asserts it equals `adam_llm_agent::REDACTED_MARKER`,
so the two cannot drift.

## Errors

`MemoryError` implements `adam_error::Classify`; a caller decides from the class, never from the
variant. A refused text, a bound, an empty field or an address is `Invalid`; a full scope, problem or
audience is `Rejected`; an unknown problem, attempt or lesson is `NotFound`; a file that does not
parse is `Corrupt`; a storage failure is `Transient` (ADR 0081 decision 12: an unreachable memory is
a transient tool error). `MemoryError::backend` is public **for adapters**: a driver failure maps to
`Backend`, whose class is `Transient`.

## Environment

None. The deployment chooses the folder and injects the scrubber; this crate reads no environment
variable.

## Tests

`cargo test -p adam-memory` runs the conformance suite
[`si_memory_conformance!`](../adam-memory-testkit/README.md) against `InProcessSiMemory` and against
`FolderSiMemory`, plus the folder's own cases:

* `a_restart_on_the_same_folder_reads_what_was_written`, and `a_fresh_folder_reads_nothing`;
* `the_folder_holds_plain_json_files`;
* `two_scopes_share_a_folder_and_a_restart_keeps_them_apart`;
* `a_corrupt_file_is_reported_not_served`.

`cargo test -p adam-memory --test lesson` runs
[`memory_conformance!`](../adam-memory-testkit/README.md) against `InProcessMemory`, over one shared
store so two configured scopes are two scopes on one store. The same suite runs against the
PostgreSQL adapter in [`adam-store-postgres`](../adam-store-postgres/README.md) (`tests/memory.rs`,
feature `memory`).

The unit tests cover the class table, the marker, the address detector, the slug and the digest.

## A new required method

`SiMemory` and `Memory` are ports: a new required method breaks every implementer. Say so in the
pull request and here (CLAUDE.md rule 2). The suites in `adam-memory-testkit` are what prove an
implementation.
