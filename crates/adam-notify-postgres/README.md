# adam-notify-postgres

Run events and wake-up/cancel signals across processes, over PostgreSQL
`LISTEN`/`NOTIFY`.

## Where it sits

An **adapter** of two ports of [`adam-runtime`](../adam-runtime/README.md):
`EventSink` (`PgEventSink`) and `Notifier` (`PgNotifier`). It is a separate crate
because the store must not depend on the runtime
([`adam-store-postgres`](../adam-store-postgres/README.md) stays the store), and
it works over any `PgPool`, normally the one the store already uses. The
conformance suite is [`adam-notify-testkit`](../adam-notify-testkit/README.md).
MongoDB has no equivalent here: a standalone `mongod` has no change streams, so
[`adam-store-mongodb`](../adam-store-mongodb/README.md) keeps polling.

Without it, processes share only the store: a worker finds a run another
process started at its next poll (250 ms by default), and a cancel issued in
one process reaches a step running in another at the worker's next read of the
run. With it, both are immediate, and a front's SSE subscription sees
`Progress` of a run a worker steps as it happens.

**It is a latency optimisation, and nothing more.** `NOTIFY` is at most once and
not durable. Correctness rests on the store's compare-and-swap, the workers'
polling (which stays on) and the durable `Runtime::view`; a run completes with
this crate removed, only later.

## API at a glance

| Item | What |
|---|---|
| `PgNotify::new(pool, local)` | one per process; `local` is the `BroadcastSink` the events of *other* processes are delivered to (the one the A2A backend subscribes to) |
| `.with_channel_prefix(p)` | channels `{p}events`, `{p}signals`; default `adam_`; `[a-z0-9_]`, at most 40, not starting with a digit; call it first |
| `.event_sink()` -> `PgEventSink` | `RuntimeBuilder::event_sink(..)`: local sink first, then `NOTIFY` |
| `.notifier()` -> `PgNotifier` | `RuntimeBuilder::notifier(..)` |
| `.run(stop)` | drives the publisher and the listener until `stop` resolves; `Ok` only on stop |
| `.wait_listening()` | resolves once `LISTEN` is active on the current connection |
| `MAX_PAYLOAD_BYTES` | 7 999 |
| `NotifyError` | `InvalidPrefix` (`Invalid`), `AlreadyRunning` (`Internal`), `PoolClosed` (`Rejected`); `#[non_exhaustive]`, `Classify` |

```rust
use adam_runtime::{BroadcastSink, Runtime};
use adam_notify_postgres::PgNotify;

let events = BroadcastSink::default();            // the A2A backend subscribes to this
let notify = PgNotify::new(pool.clone(), events.clone());   // pool: sqlx::PgPool
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
    Listening --> Resyncing: try_recv returned None, sqlx has reconnected and listened again
    Resyncing --> Listening: Resync broadcast
    Listening --> Reconnecting: try_recv failed
    Reconnecting --> Connecting: after the backoff, 100 ms doubling to 10 s
    Connecting --> [*]: stop, or the pool is closed
    Listening --> [*]: stop, or the pool is closed
    Reconnecting --> [*]: stop
```

`run` drives one publisher and one listener (`src/lib.rs`).

* **Publisher.** `emit` and `publish` put the encoded item on a queue of 1 024
  with `try_send` and return: they never block and never fail. A full queue
  drops the item and warns (the first drop, then every thousandth). One task
  drains the queue in order with `SELECT pg_notify($1, $2)` on the pool, one at a
  time, so one process's notifications keep their order (and `NOTIFY` delivers
  in commit order). A failed `pg_notify` drops its item and warns the same way.
  When `stop` resolves, the items still queued are sent for up to
  `DRAIN_ON_STOP` (2 s) before `run` returns; what cannot be sent in that time
  is dropped, as any notification may be.
* **Listener.** `PgListener::connect_with(&pool)` then `LISTEN` on both channels.
  Only after `LISTEN` is active does it mark itself listening and broadcast
  `Delivery::Resync`, so subscribers catch up after the gap has closed. When sqlx
  reports a lost connection (`try_recv` returns `Ok(None)`) it has already
  reconnected and listened again, and what was sent in between is gone: another
  `Resync`. Any other error drops the listener, waits the backoff (reset after a
  session that lasted 5 s) and connects again. A closed pool ends `run` with
  `NotifyError::PoolClosed`.
* **Resync** is how the runtime learns to look: a worker polls for due runs at
  once and re-reads every run it is stepping, firing the cancel token of any that
  finished. A subscriber that lags behind the 1 024-delivery fan-out gets one
  too.

## Guarantees

* **At most once, in order per publisher, best effort.** A notification may be
  lost (nobody listening, a reconnect, a full queue, an oversize event); none is
  duplicated by this crate. Nothing in the runtime depends on one arriving.
* **Events do not echo and are not re-published.**
* **Events are not replayable.** After a gap the durable record
  (`Runtime::view`) is the truth; that is what the SSE subscription polls. The only memory is the
  `BroadcastSink` of each process, which keeps the last 30 seconds (at most 64 events) of a run for a
  subscriber that attaches late; the events this crate delivers into `local` are recorded there like
  local ones, from the moment the process hears them, and a gap in the connection is still a gap.
* **The listener holds one connection of the pool for as long as `run` runs.**
  Size the pool for it (the store's default is 16). `LISTEN` is per session, so
  connect directly or through a session-mode pooler; a transaction-mode pooler
  (PgBouncer default) silently breaks it.
* Signals only trigger a re-read; the version compare-and-swap decides every
  race, so a spurious or late signal costs one poll.

## Features and environment

Cargo features mirror the store's: `tls-rustls` (default) and `tls-native-tls`,
forwarded to `sqlx`. No environment variables at runtime.

## Tests

| File | What |
|---|---|
| `src/wire.rs`, `src/lib.rs`, `src/error.rs` | unit tests: payload round trip and layout, the size rule (exact limit, multibyte and escaped truncation, drops, the largest step and the largest piece of streamed text fit), origin filter, no re-publish, prefix validation, queue overflow, the backoff, the error classes. Offline |
| `tests/conformance.rs` | `adam-notify-testkit`'s `notifier_conformance!` with two `PgNotify` on separate pools, plus one case at the exact 7 999-byte limit |
| `tests/two_runtimes.rs` | a front (starter only) and a worker, each with its own pools, `PgNotify` and `Runtime`, poll interval 30 s: a start and a deliver wake an idle worker; a step's `Progress` reaches the front exactly once and before the `Parked` status; a cancel reaches the step within 2 s; terminating the listener's backend gives a `Resync` within 5 s, a run started meanwhile completes, and a later signal arrives |

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
  order; identical payloads of one transaction are folded into one (each item
  here is its own statement, so this crate does not depend on it).
* *Verified 2026-09-29, `sqlx-postgres` 0.9.0 source:* `PgListener::connect_with`
  takes one pooled connection; with `eager_reconnect` on (the default) a lost
  connection makes `try_recv` reconnect, `LISTEN` again and return `Ok(None)`;
  notifications sent during the gap are lost.
* *Unverified:* behaviour behind transaction-mode poolers and on managed
  Postgres offerings (from general knowledge of `LISTEN`, not tested here).
  The CI matrix runs PostgreSQL 12 and 17.

## See also

[`adam-runtime`](../adam-runtime/README.md),
[`adam-notify-testkit`](../adam-notify-testkit/README.md),
[`adam-store-postgres`](../adam-store-postgres/README.md),
[`adam-a2a-runtime`](../adam-a2a-runtime/README.md).
