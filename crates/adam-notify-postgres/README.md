# adam-notify-postgres

Run events and wake-up/cancel signals across processes, over PostgreSQL
`LISTEN`/`NOTIFY`.

## Where it sits

An **adapter** of two ports of [`adam-runtime`](../adam-runtime/README.md):
`EventSink` (`PgEventSink`) and `Notifier` (`PgNotifier`). It is a separate crate
because the store must not depend on the runtime
([`adam-store-postgres`](../adam-store-postgres/README.md) stays the store). It
publishes over any `deadpool_postgres::Pool`, normally the one the store already
uses, and listens on a connection of its own built from the same
[`adam-postgres`](../adam-postgres/README.md) `PgConfig` (a pooled client can
never receive a notification). The conformance suite is
[`adam-notify-testkit`](../adam-notify-testkit/README.md).
MongoDB has no equivalent here: a standalone `mongod` has no change streams, so
[`adam-store-mongodb`](../adam-store-mongodb/README.md) keeps polling.

Without it, processes share only the store: a worker finds a run another
process started at its next poll (250 ms by default), and a cancel issued in
one process reaches a step running in another at the step's next cancel poll
(1 s by default). With it, both are immediate, and a front's SSE subscription sees
`Progress` of a run a worker steps as it happens.

**It is a latency optimisation, and nothing more.** `NOTIFY` is at most once and
not durable. Correctness rests on the store's compare-and-swap, the workers'
polling (which stays on) and the durable `Runtime::view`; a run completes with
this crate removed, only later.

## API at a glance

| Item | What |
|---|---|
| `PgNotify::new(pool, pg, local)` | one per process; `pool` is the publisher's pool, `pg` the `adam_postgres::PgConfig` the listener connects with (parse `DATABASE_URL` once, as `adam-service` does), `local` the `BroadcastSink` the events of *other* processes are delivered to (the one the A2A backend subscribes to). **Breaking change**: the signature gained `pg` |
| `.with_channel_prefix(p)` | channels `{p}events`, `{p}signals`; default `adam_`; `[a-z0-9_]`, at most 40, not starting with a digit; call it first |
| `.event_sink()` -> `PgEventSink` | `RuntimeBuilder::event_sink(..)`: local sink first, then `NOTIFY` |
| `.notifier()` -> `PgNotifier` | `RuntimeBuilder::notifier(..)` |
| `.run(stop)` | drives the publisher and the listener until `stop` resolves; `Ok` only on stop |
| `.wait_listening()` | resolves once `LISTEN` is active on the current connection |
| `.local_only(bool)` | keep this process's events local: no encoding, no outbox, no `NOTIFY` (see *Features and environment*); call it first |
| `.statements_sent()` | how many `pg_notify` statements this instance issued, a batch counting once (tests, metrics) |
| `MAX_PAYLOAD_BYTES` | 7 999 |
| `MAX_BATCH_ITEMS` | 32: the most items one `pg_notify` statement carries |
| `NotifyError` | `InvalidPrefix` and `InvalidLocalOnly` (`Invalid`), `AlreadyRunning` (`Internal`), `PoolClosed` (`Rejected`); `#[non_exhaustive]`, `Classify` |

```rust
use adam_runtime::{BroadcastSink, Runtime};
use adam_notify_postgres::PgNotify;

let events = BroadcastSink::default();            // the A2A backend subscribes to this
let pg = adam_postgres::PgConfig::parse(&database_url)?;     // once, shared with the store
let notify = PgNotify::new(store.pool().clone(), pg, events.clone());
let runtime = Runtime::builder(store)
    .event_sink(notify.event_sink())
    .notifier(notify.notifier())
    .build();
// next to the server and the worker, until shutdown:
// notify.run(shutdown_future).await?;
```

Run `notify.run(..)` in the same process as the `Runtime`, for every role. A
worker-only process needs it to hear signals; a front needs it to hear events.
[`adam-coder`](../../bin/adam-coder/README.md) does exactly this: `serve` builds one `PgNotify`
on the store's pool and runs it as the host component `notify` in every role (see its
*Live events and wake-up across processes*).

## Channels and payloads

| Channel | Payload |
|---|---|
| `{prefix}events` | `{"v":1,"o":"<origin uuid>","run":"<run id>","agent":"<name>","event":{"type":"progress","message":".."}}` (`event` is a `RunEvent`) |
| `{prefix}signals` | `{"v":1,"type":"runnable","run":"..","agent":".."}` or `{"v":1,"type":"finished","run":".."}` |

`o` is a random id per `PgNotify`: a process skips the events it sent (it
delivered them locally when it emitted them), so nothing is delivered twice and
nothing is ever re-published. Signals carry no origin: a publisher's own
subscribers get them back through the database, like everyone's. A payload of
another `v`, or one that does not parse, is ignored (debug log).

`NOTIFY` rejects a payload of 8000 bytes or more, so nothing over
`MAX_PAYLOAD_BYTES` is sent:

* a `Status` whose payload is too big keeps its status and loses the tail of its
  `detail`, cut at a character boundary as little as possible (the full failure
  text is in the run itself);
* a `Step` without input or output is bounded by its constructors (an id of at most 128 bytes, a label of at most 200
  characters, a detail of at most 1000: about 5 KiB at four bytes a character, a unit test builds the largest), so it
  always fits. A tool call's step can also carry its `input` (up to 4 KiB) and `output` (up to 8 KiB;
  [ADR 0011](../../docs/decisions/0011-a-tool-calls-step-carries-its-input-and-output.md)): one that does not fit
  crosses **without them** (`Encoded::Truncated`, debug log), the step, its state and its label whole, so that an end that
  lost its output is still an end. In a single process (`ROLE=all`) nothing is lost;
* a `TextDelta` (a piece of the model's answer as it is written) holds at most `MAX_TEXT_DELTA_BYTES` of `adam-runtime`
  (1024 bytes) and a stream id of at most 128 bytes: even a piece of control characters, which JSON writes in six bytes
  each, is about 6.5 KiB, and a unit test builds the largest of each kind, so it always fits and crosses whole;
* a `ReasoningDelta` has a `TextDelta`'s bounds (the same piece, the same stream id) and so always fits and crosses whole. A process
  that predates the variant cannot read the event and drops it (debug log), so reasoning is never taken for text in a rolling deploy;
* a `Usage` (a model call's report, `usage/v1`) holds a call id, a step and two labels of at most 128 bytes each and seven
  counts: under 4 KiB even of control characters, so it always fits and crosses whole; a process that predates the variant
  drops it, like a `ReasoningDelta`;
* any other oversize event (`Progress`, `Custom`, `Artifact`) is not sent to
  other processes (debug log). A file artifact whose bytes alone are over a payload is dropped without being serialized
  (a file is up to 4 MiB; only a tiny one fits); Artifacts still reach subscribers through the
  durable poll, which reads `RunView::artifacts`. **There is no events table**: a table would make events replayable but add a
  write, a schema, a sequence and a retention job to every event, for a
  best-effort stream whose durable half (status and artifacts) is already the
  run record. That option was considered and rejected;
* a signal is small unless the agent name is absurd, in which case it is dropped.

## Lifecycle

```mermaid
stateDiagram-v2
    [*] --> Connecting
    Connecting --> Listening: connected, LISTEN active, Resync broadcast
    Connecting --> Reconnecting: connect or LISTEN failed
    Listening --> Listening: notification dispatched
    Listening --> Reconnecting: the connection ended (poll_message returned None or an error)
    Reconnecting --> Connecting: after the backoff, 100 ms doubling to 10 s
    Connecting --> [*]: stop, or the pool is closed
    Listening --> [*]: stop, or the pool is closed
    Reconnecting --> [*]: stop
```

`run` drives one publisher and one listener (`src/lib.rs`).

* **Publisher.** `emit` and `publish` put the encoded item on a queue of 1 024
  with `try_send` and return: they never block and never fail. A full queue
  drops the item and warns (the first drop, then every thousandth). One task
  drains the queue in order with one statement per batch,
  `SELECT pg_notify(channel, payload) FROM unnest($1::text[], $2::text[])` on the
  pool: the item it just received plus whatever is already queued, at most
  `MAX_BATCH_ITEMS` (32), and it never waits for a batch to fill. A burst of N
  queued items is ⌈N/32⌉ round trips instead of N, and the notifications keep
  the queue's order (`NOTIFY` delivers in commit order, and one statement's rows
  are evaluated in row order). A failed statement drops the items in that batch
  and warns with the batch size: the first failed statement, then every
  thousandth. Nothing is retried. When `stop` resolves, the items
  still queued are sent the same way, for up to `DRAIN_ON_STOP` (2 s) before
  `run` returns; what cannot be sent in that time is dropped, as any
  notification may be.
* **Listener.** It opens a connection of its own from the `PgConfig`
  (`config.connect_raw`), sends `LISTEN` on both channels and polls the
  connection's driver (`poll_message`), because `tokio-postgres`'s `Connection`
  future — the one a pool drives — ignores asynchronous messages. Only after
  `LISTEN` is active does it mark itself listening and broadcast
  `Delivery::Resync`, so subscribers catch up after the gap has closed. A lost
  connection ends the session, the loop waits the backoff (reset after a session
  that lasted 5 s) and connects again, and a `Resync` follows the new `LISTEN`:
  what was sent in between is gone. A closed pool ends `run` with
  `NotifyError::PoolClosed`.
* **Resync** is how the runtime learns to look: a worker polls for due runs at
  once and re-reads every run it is stepping, firing the cancel token of any that
  finished. A subscriber that lags behind the 1 024-delivery fan-out gets one
  too.

## Guarantees

* **At most once, in order per publisher, best effort.** A notification may be
  lost (nobody listening, a reconnect, a full queue, an oversize event); none is
  duplicated by this crate. Nothing in the runtime depends on one arriving.
  PostgreSQL folds identical channel-and-payload pairs of one transaction into
  one, so a batch that carries the same payload twice delivers it once; whether
  the second copy is in that batch depends on timing, and a duplicate is
  indistinguishable from the first, so nothing counts them.
* **Events do not echo and are not re-published.**
* **Events are not replayable.** After a gap the durable record
  (`Runtime::view`) is the truth; that is what the SSE subscription polls. The only memory is the
  `BroadcastSink` of each process, which keeps the last 30 seconds (at most 64 events) of a run for a
  subscriber that attaches late; the events this crate delivers into `local` are recorded there like
  local ones, from the moment the process hears them, and a gap in the connection is still a gap.
* **The listener holds one connection of its own for as long as `run` runs**,
  outside the pool, so it does not shrink the store's capacity. `LISTEN` is per
  session, so connect directly or through a session-mode pooler; a
  transaction-mode pooler (PgBouncer default) silently breaks it.
* Signals only trigger a re-read; the version compare-and-swap decides every
  race, so a spurious or late signal costs one poll.

## Features and environment

There are no Cargo features: TLS is always `rustls` on `aws-lc-rs`, chosen by
`DATABASE_URL` (`sslmode`, `sslrootcert`, `sslcert`, `sslkey`) through
[`adam-postgres`](../adam-postgres/README.md#tls).

| Variable | Meaning | Default |
|---|---|---|
| `NOTIFY_LOCAL_ONLY` | `true`/`1` or `false`/`0` (ASCII case ignored): keep this process's events local. `PgEventSink::emit` delivers them to the local sink and does not encode, queue or `NOTIFY` them; every process's listener drops an event of its own origin anyway, so a process with no other process to tell would pay for a round trip and an echo it discards. Signals (wake-ups, cancels) still cross. One **process** (`ROLE=all`, one pod): with `topology: split` (a front and its workers, even one replica each) it silences the live events between them, whose state changes still reach the 2 s fallback poll. It is a deployment choice, never detected. `PgNotify::new` reads it once; `.local_only(bool)` overrides it. Anything else: the binaries' configuration refuses it at startup (exit 78); an embedder that calls `PgNotify::new` itself gets `NotifyError::InvalidLocalOnly` (class `Invalid`) from `run` | `false` |

## Tests

| File | What |
|---|---|
| `src/wire.rs`, `src/lib.rs`, `src/error.rs` | unit tests: payload round trip and layout, the size rule (exact limit, multibyte and escaped truncation, drops, the largest step, the largest piece of streamed text and the largest usage report fit), origin filter, no re-publish, prefix validation, queue overflow, the backoff, the error classes, batching (the cap, the order and the channel of each item; a failed statement counted once whatever its batch carried), local-only (100 events reach the local sink and nothing is queued; the variable's spellings, a bad one fails `run`). Offline |
| `tests/conformance.rs` | `adam-notify-testkit`'s `notifier_conformance!` with two `PgNotify` on separate pools (each with its own listener connection), plus one case at the exact 7 999-byte limit |
| `tests/batching.rs` | one statement per queued batch, counted by `statements_sent` (a full batch is 1, two batches and a remainder 3), 100 queued events arriving in queue order across batches, the stop-drain batching what is queued, and a signal still crossing from a `local_only(true)` process |
| `tests/two_runtimes.rs` | a front (starter only) and a worker, each with its own pools, `PgNotify` and `Runtime`, poll interval 30 s and cancel watch 30 s, so only the notifier can meet the 2 s bound: a start and a deliver wake an idle worker; a step's `Progress` reaches the front exactly once and before the `Parked` status; a cancel reaches the step within 2 s; terminating the listener's backend gives a `Resync` within 5 s, a run started meanwhile completes, and a later signal arrives |

| Variable | Meaning |
|---|---|
| `ADAM_TEST_POSTGRES_URL` | enables the database tests (a superuser, so `pg_terminate_backend` works) |
| `ADAM_TEST_REQUIRE_DB` | `1`: an unset URL fails instead of skipping |

Every conformance case takes a channel prefix `adam_n<8 hex>_`; every
`two_runtimes` test takes its own too and its own tables (`adam_nt<8 hex>_`,
dropped at the end), so they run in parallel against one database.

```sh
ADAM_TEST_POSTGRES_URL=postgres://postgres:postgres@localhost:5432/adam_test \
  cargo test -p adam-notify-postgres
```

## Verified and unverified

* *Verified 2026-09-29, PostgreSQL 16 documentation (`NOTIFY`) and a
  16.13 server:* a payload must be shorter than 8000 bytes (`pg_notify` with 8000
  bytes fails with `payload string too long`, 7 999 succeeds, which
  `tests/conformance.rs` asserts); notifications are delivered on commit, in commit
  order; identical payloads of one transaction are folded into one, so the same
  payload twice in one batch arrives once (events are a latency hint and a
  duplicate is indistinguishable, so nothing depends on the count); a statement's
  rows are evaluated in row order, which is what keeps a batch in queue order
  (`tests/batching.rs`).
* *Verified 2026-11-20, `tokio-postgres` 0.7.18 source:* `Connection::poll_message`
  is the only way to receive an `AsyncMessage::Notification`; the `Connection`
  future (`impl Future`) drops them, and deadpool drives the connection as that
  future, which is why the listener owns its connection. `poll_message` returns
  `None` (or an error) when the connection ends, which is the session's end;
  notifications sent during the reconnect are lost.
* *Unverified:* behaviour behind transaction-mode poolers and on managed
  Postgres offerings (from general knowledge of `LISTEN`, not tested here).
  The CI matrix runs PostgreSQL 12 and 17.

## See also

[`adam-runtime`](../adam-runtime/README.md),
[`adam-notify-testkit`](../adam-notify-testkit/README.md),
[`adam-store-postgres`](../adam-store-postgres/README.md),
[`adam-a2a-runtime`](../adam-a2a-runtime/README.md).

**Breaking (from `adam-runtime`):** `RunEvent::Artifact` has a member `id`, carried across processes when set and absent otherwise; a payload from before reads as before.
