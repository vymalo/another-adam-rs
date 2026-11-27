# adam-memory

The L1 self-improvement memory of adam-rs: the problems an agent met, the attempts it made, and
what it observed. Its only adapter is a folder of plain files the deployment gives the agent.

[ADR 0081](https://github.com/vymalo/another-adam-rs/blob/main/docs/decisions/0081-an-agent-keeps-a-self-improvement-memory-near-it-and-what-holds-becomes-the-deployments-memory.md)
decisions 2 to 5, slice M1.

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

The crate is the port, the values and the two adapters above. A store reached over the network is
L2 (`Memory`, ADR 0081 decision 2), not this crate.

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

## The key

`Scope` is `deployment`, `caller`, `agent` and `Kind`; the key does not name a worker. Two workers
that mount one folder are one L1; two workers with two folders are two, as two cores have two
caches. A repository-scoped entry is served only to a run that prepared that repository (the tool
layer, M3, chooses the scope; this crate stores it).

The in-folder layout is `<root>/<slug>-<digest of the key>/<slug>-<digest of the root cause>.json`.
The digest is the first 16 hexadecimal digits of SHA-256, so a rebuild computes the same names; the
slug is only for a person reading the folder. Every file carries its scope, and a file that belongs
to another scope is `Corrupt` rather than served.

## Bounds

`Limits::default()` is the ADR's: 64 problems per scope, 16 attempts per problem, 1 KiB a text
field, 200 characters a root cause, and 16 anchors an attempt (the ADR bounds every list but names
no anchor count; this slice's value is 16).

A write that would break a bound is **refused**, not evicted, so the caller sees it. A sweep removes
the attempts recorded before its cutoff and the problems it leaves empty, which frees the room
again. Recall serves the freshest problem first.

## Scrubbing and refusal

Every text passes through the injected `Scrubber` before it is stored. A text that still holds
`REDACTION_MARKER` after the scrub is refused (`MemoryError::Redacted`), not stored: the caller
sees the refusal, and nothing is written. An **agent-scoped** text may not name a repository, a
path, a URL, a host or an e-mail address (`MemoryError::NamesAddress`); a repository-scoped one
may. The detector knows these shapes: a scheme (`://`), an address with an `@` and a dotted domain,
a host with a dotted name or `localhost` (with or without a port), an absolute or relative path,
a drive letter, a token with two or more slashes, and a token with a code extension. It is
conservative: a false positive refuses a write, which decision 12 makes the safe side.

`adam-memory` does **not** depend on `adam-llm-agent`: `adam-llm-agent` will depend on this crate
(the `si_recall`/`si_record` tools), and a dependency back would be a cycle. The marker is a
constant here, and a test in `adam-llm-agent` asserts it equals `adam_llm_agent::REDACTED_MARKER`,
so the two cannot drift.

## Errors

`MemoryError` implements `adam_error::Classify`; a caller decides from the class, never from the
variant. A refused text, a bound, an empty field or an address is `Invalid`; a full scope or
problem is `Rejected`; an unknown problem or attempt is `NotFound`; a file that does not parse is
`Corrupt`; a file-system failure is `Transient` (ADR 0081 decision 12: an unreachable memory is a
transient tool error).

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

The unit tests cover the class table, the marker, the address detector, the slug and the digest.

## A new required method

`SiMemory` is a port: a new required method breaks every implementer. Say so in the pull request
and here (CLAUDE.md rule 2). The suite in `adam-memory-testkit` is what proves an implementation.
