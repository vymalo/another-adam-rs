# adam-notify-testkit

The conformance suite for `adam_runtime::Notifier` implementations and the
cross-process `EventSink` that ships with them.

## Where it sits

The **testkit** of the `Notifier` port in [`adam-runtime`](../adam-runtime/README.md).
An adapter crate lists it as a dev-dependency and runs the same cases, so
"passes the testkit" means "behaves like `LocalNotifier`, across processes". It
is a separate crate from `adam-runtime` to avoid a dev-dependency cycle, the
same way [`adam-store-testkit`](../adam-store-testkit/README.md) is separate
from `adam-core`.

## API at a glance

* `notifier_conformance!(make)` generates one `#[tokio::test]` per case (7). `make`
  is a path to `async fn() -> Option<Pair>`; `None` skips the suite (and fails
  under `ADAM_TEST_REQUIRE_DB=1`, see `adam_core::testing`).
* `Side { notifier, sink, local }`, `Side::new(..)`: one process. `notifier`
  publishes this process's signals and receives everyone's; `sink` is what this
  process's runtime emits events into; `local` is the `BroadcastSink` events
  arrive at.
* `Pair`: two sides that reach each other, **both already listening**.
* `cases::*`: the cases as plain async functions taking a `Pair`, and
  `cases::local_pair()`, the in-process pair.
* `skipped`: re-export of `adam_core::testing::skipped`.

| Case | What it pins down |
|---|---|
| `signal_reaches_the_other_side` | a signal crosses |
| `signal_reaches_the_publisher_too` | the publisher's own subscribers get it back |
| `signals_keep_order_from_one_publisher` | 50 signals, in order |
| `every_subscriber_gets_every_signal` | three subscribers on both sides, each gets all, in order |
| `events_cross_once_without_echo` | an event reaches the emitting side's and the other side's local sink once each, and nothing arrives twice |
| `oversize_event_does_not_break_the_stream` | a 20 000-character `Progress`, `Status` and signal: the `Status` still arrives with a prefix of its detail, a big `Progress` arrives whole or not at all, and what follows arrives |
| `publish_never_blocks_without_listeners` | 5 000 publishes and 5 000 emits return within 5 s |

```rust
async fn make_pair() -> Option<adam_notify_testkit::Pair> {
    let url = adam_core::testing::test_env("ADAM_TEST_POSTGRES_URL")?;
    /* two Sides, each with its own pool, PgNotify::run spawned, wait_listening awaited */
}
adam_notify_testkit::notifier_conformance!(make_pair);
```

The calling crate needs `tokio` (features `macros`, `rt-multi-thread`) as a
dev-dependency. Give each pair its own channel or topic so parallel cases do not
hear each other; the cases themselves use fresh run ids. A case may see a
`Delivery::Resync` at any time and skips it.

## The in-process variant

`tests/local.rs` runs the suite against `LocalNotifier` with `cases::local_pair()`:
both sides share one notifier and one `BroadcastSink`, because inside one
process there is no transport. So `events_cross_once_without_echo` is only a
harness check there (each subscriber sees the event once); it is meaningful for
adapters that carry events, where an echo would show as a duplicate. The other
six cases are meaningful as they are.

## Features and environment

No Cargo features. `ADAM_TEST_REQUIRE_DB=1` turns a skipped suite into a failure.

## Tests

`tests/local.rs`: the suite against `LocalNotifier`, always on. The Postgres
adapter runs it from its own `tests/conformance.rs`.

## See also

[`adam-runtime`](../adam-runtime/README.md),
[`adam-notify-postgres`](../adam-notify-postgres/README.md),
[`adam-store-testkit`](../adam-store-testkit/README.md).
