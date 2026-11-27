# adam-export-testkit

The conformance suites of the two ports of [`adam-export`](../adam-export/README.md): `RunSource`
(where runs come from) and `RecordSink` (where records go).

## Where it sits

The **testkit** of the exporter's ports, as [`adam-store-testkit`](../adam-store-testkit/README.md)
is the `Store`'s. An adapter crate lists it as a dev-dependency and runs the same cases, so "passes
the testkit" means "the `Exporter` reads from it (or writes to it) like from every other". It is a
separate crate from `adam-export` to avoid a dev-dependency cycle, so the reference adapters
(`RuntimeSource` over the memory store, `JsonLines`) run the suites from this crate's own tests.
The decision is [ADR 0085](../../docs/decisions/0085-runs-are-exported-as-training-records.md),
*Amended* for ports and adapters.

## API at a glance

* `source_conformance!(make)` generates one `#[tokio::test]` per source case (6). `make` is a path
  to `async fn(Vec<SeedRun>) -> Option<Box<dyn RunSource>>`: it puts the seeded runs where a fresh
  source reads them and returns the source. `None` skips the suite (and fails under
  `ADAM_TEST_REQUIRE_DB=1`, see `adam_core::testing`).
* `sink_conformance!(make)` generates one per sink case (5). `make` is a path to
  `async fn() -> Option<SinkFixture>`.
* `SeedRun { id, agent, conversation_id, status, state }`, `SeedRun::new(agent, status, state)`,
  `SeedRun::done(agent, text)`: a run a case needs. Every seed has a conversation id of its own and
  is `done` or `failed`. `SeedState` (closed): `Conversation(Box<Conversation>)` (yielded),
  `Other(Value)` (a readable state that is not a conversation: `not_a_conversation`),
  `Unreadable` (a corrupt document: `unreadable`).
* `conversation(text)`: the conversation of one model call the seeds use.
* `SinkFixture { sink: Box<dyn RecordSink>, readback: Box<dyn Readback> }`; `Readback` (async
  `records()`): the records the destination holds now, as its reader would see them.
* `SharedBuffer`: a `Write` several handles share, and the `Readback` of JSON Lines in it.
* `cases::source::*`, `cases::sink::*`: the cases as plain async functions.
* `skipped`: re-export of `adam_core::testing::skipped`.

| Source case | What it pins down |
|---|---|
| `an_empty_source_yields_nothing` | no run, no skip, `next: None`; a label, if any, is not blank |
| `pages_are_disjoint_and_cover_every_run_once` | seven runs three a page: each once, nothing else; no page lists more than the limit |
| `the_cursor_resumes_after_the_last_item` | the page after a cursor holds nothing before it; the same cursor answers the same page; the rest follows |
| `skipped_runs_are_counted_not_yielded` | two states that are not conversations and an unreadable one are counted, the two conversations yielded |
| `a_page_holds_only_the_query_s_agent_statuses_and_times` | the query's agent and statuses only; `since` inclusive, `until` exclusive |
| `a_run_keeps_its_bookkeeping_and_conversation` | id, agent, conversation id, status and conversation as seeded; no recorded request |

| Sink case | What it pins down |
|---|---|
| `an_empty_sink_finishes_with_nothing` | `finish` on nothing leaves the destination empty |
| `records_arrive_in_order_once_finished` | five records, in order, once, after `finish` |
| `finish_twice_is_harmless` | a second `finish` succeeds and adds nothing |
| `a_put_after_finish_is_refused` | `Internal`, and nothing is written |
| `a_record_reads_back_whole` | every optional field (`source`, `parent`, `sample`, reasoning, line breaks in text) reads back equal |

```rust
async fn make_source(seeds: Vec<adam_export_testkit::SeedRun>) -> Option<Box<dyn adam_export::RunSource>> {
    /* a fresh store or dump holding `seeds`; None when its variable is unset */
}
adam_export_testkit::source_conformance!(make_source);

async fn make_sink() -> Option<adam_export_testkit::SinkFixture> {
    /* a fresh bucket prefix, table or file, and a Readback that lists it */
}
adam_export_testkit::sink_conformance!(make_sink);
```

The calling crate needs `tokio` (features `macros`, `rt-multi-thread`) as a dev-dependency. Each
case names its agents uniquely, so cases may share one database and run in parallel. A source
over a `Store` reached through a `Runtime` needs no suite of its own: `RuntimeSource` passes this
one, and the store passes `store_conformance!`.

## Features and environment

No Cargo features. `ADAM_TEST_REQUIRE_DB=1` turns a skipped suite into a failure.

## Tests

* `tests/runtime_source.rs`: the source suite against `RuntimeSource` over a `MemoryStore` (the
  seeds are written as the runtime's whole-state envelope; an unreadable one has an unknown
  version), and `a_cursor_it_did_not_make_is_invalid`.
* `tests/json_lines.rs`: the sink suite against `JsonLines` over a `BufWriter` on a
  `SharedBuffer`, so the cases also check that `finish` flushes.

## See also

[`adam-export`](../adam-export/README.md),
[`adam-store-testkit`](../adam-store-testkit/README.md),
[Export runs for training](../../docs/guides/export-runs-for-training.md).
