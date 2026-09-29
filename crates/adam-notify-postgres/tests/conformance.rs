//! The `adam-notify-testkit` suite against a real PostgreSQL: two `PgNotify`
//! on separate pools stand for two processes.
//!
//! ```sh
//! ADAM_TEST_POSTGRES_URL=postgres://postgres:postgres@localhost:5432/adam_test \
//!     cargo test -p adam-notify-postgres --test conformance
//! ```
//!
//! Skipped when the variable is unset, unless `ADAM_TEST_REQUIRE_DB=1` (then it
//! fails). Each case gets its own channel prefix, `adam_n<8 hex>_`, so cases
//! run in parallel without hearing each other.
#![allow(clippy::unwrap_used, clippy::expect_used)] // integration tests assert by unwrapping

use adam_notify_postgres::PgNotify;
use adam_notify_testkit::{Pair, Side};
use adam_runtime::BroadcastSink;
use sqlx::postgres::PgPoolOptions;
use uuid::Uuid;

async fn side(url: &str, prefix: &str) -> Side {
    // Each #[tokio::test] has its own runtime, so each side has its own small
    // pool; the listener takes one connection of it.
    let pool = PgPoolOptions::new()
        .max_connections(4)
        .connect(url)
        .await
        .expect("connect");
    let local = BroadcastSink::default();
    let notify = PgNotify::new(pool, local.clone())
        .with_channel_prefix(prefix)
        .expect("prefix");
    let runner = notify.clone();
    tokio::spawn(async move { runner.run(std::future::pending()).await });
    notify.wait_listening().await;
    Side::new(notify.notifier(), notify.event_sink(), local)
}

async fn make_pair() -> Option<Pair> {
    let url = adam_core::testing::test_env("ADAM_TEST_POSTGRES_URL")?;
    let prefix = format!("adam_n{}_", &Uuid::new_v4().simple().to_string()[..8]);
    Some((side(&url, &prefix).await, side(&url, &prefix).await))
}

adam_notify_testkit::notifier_conformance!(make_pair);

/// The largest payload PostgreSQL accepts (7999 bytes) crosses; one byte more
/// is dropped without disturbing the events after it.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn an_event_at_the_size_limit_crosses_and_one_byte_more_does_not() {
    let Some((a, b)) = make_pair().await else {
        return;
    };
    let run = adam_core::RunId::new();
    let mut on_b = b.local.subscribe_run(run);
    // The payload is `{"v":1,"o":<uuid>,"run":<uuid>,"agent":"agent","event":{..}}`;
    // a nil uuid and this run's id have the same length as the real ones.
    let overhead = serde_json::json!({
        "v": 1, "o": Uuid::nil(), "run": run, "agent": "agent",
        "event": {"type": "progress", "message": ""},
    })
    .to_string()
    .len();
    let progress = |len: usize| adam_runtime::RunEvent::Progress {
        message: "x".repeat(len),
    };
    let room = adam_notify_postgres::MAX_PAYLOAD_BYTES - overhead;
    a.sink.emit(run, "agent", progress(room + 1)).await;
    a.sink.emit(run, "agent", progress(room)).await;
    a.sink.emit(run, "agent", progress(1)).await;
    let mut lengths = Vec::new();
    while lengths.len() < 2 {
        let event = tokio::time::timeout(std::time::Duration::from_secs(5), on_b.recv())
            .await
            .expect("events in time")
            .expect("open");
        let adam_runtime::RunEvent::Progress { message } = event else {
            panic!("unexpected event");
        };
        lengths.push(message.len());
    }
    assert_eq!(
        lengths,
        [room, 1],
        "the oversize one is skipped, the rest arrive in order"
    );
}

/// What is still queued when `run` stops goes out before `run` returns: here
/// the items are queued before `run` starts and `stop` is already resolved, so
/// only the drain can send them.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn items_queued_at_stop_are_drained() {
    use adam_runtime::{Delivery, Notifier, Signal};
    use futures::StreamExt;

    let Some(url) = adam_core::testing::test_env("ADAM_TEST_POSTGRES_URL") else {
        return;
    };
    let prefix = format!("adam_n{}_", &Uuid::new_v4().simple().to_string()[..8]);
    let listening = side(&url, &prefix).await;
    let mut on_listening = listening.notifier.subscribe();

    let pool = PgPoolOptions::new()
        .max_connections(4)
        .connect(&url)
        .await
        .expect("connect");
    let stopping = PgNotify::new(pool, BroadcastSink::default())
        .with_channel_prefix(&prefix)
        .expect("prefix");
    let sent: Vec<Signal> = (0..3)
        .map(|_| Signal::Finished {
            run: adam_core::RunId::new(),
        })
        .collect();
    let notifier = stopping.notifier();
    for signal in &sent {
        notifier.publish(signal.clone()).await;
    }
    stopping
        .run(std::future::ready(()))
        .await
        .expect("a stopped run is Ok");

    let mut got = Vec::new();
    while got.len() < sent.len() {
        let delivery = tokio::time::timeout(std::time::Duration::from_secs(5), on_listening.next())
            .await
            .expect("the drained signals arrive in time")
            .expect("open");
        if let Delivery::Signal(signal) = delivery {
            got.push(signal);
        }
    }
    assert_eq!(got, sent, "every queued signal arrives, in order");
}
