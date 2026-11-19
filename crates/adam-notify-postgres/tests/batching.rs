//! P8: a queue drain is one `pg_notify` statement per batch, and a batch keeps
//! the order of the queue.
//!
//! ```sh
//! ADAM_TEST_POSTGRES_URL=postgres://postgres:postgres@localhost:5432/adam_test \
//!     cargo test -p adam-notify-postgres --test batching
//! ```
//!
//! Skipped when the variable is unset, unless `ADAM_TEST_REQUIRE_DB=1` (then it
//! fails). Each test gets its own channel prefix, `adam_b<8 hex>_`, so cases run
//! in parallel without hearing each other.
#![allow(clippy::unwrap_used, clippy::expect_used)] // integration tests assert by unwrapping

use std::time::Duration;

use adam_core::RunId;
use adam_notify_postgres::{MAX_BATCH_ITEMS, PgNotify};
use adam_runtime::{BroadcastSink, EventSink, RunEvent, RunSubscription};
use sqlx::postgres::PgPoolOptions;
use tokio::sync::oneshot;
use tokio::task::JoinHandle;
use uuid::Uuid;

const PATIENCE: Duration = Duration::from_secs(5);

fn short() -> String {
    Uuid::new_v4().simple().to_string()[..8].to_owned()
}

/// A `PgNotify` that is not running yet: what is emitted before `run` waits in
/// its outbox, which is how a test controls what one statement will carry.
async fn quiet(url: &str, prefix: &str) -> (PgNotify, BroadcastSink) {
    let pool = PgPoolOptions::new()
        .max_connections(4)
        .connect(url)
        .await
        .expect("connect");
    let local = BroadcastSink::default();
    let notify = PgNotify::new(pool, local.clone())
        .with_channel_prefix(prefix)
        .expect("prefix")
        // The ambient environment must not decide what this test measures.
        .local_only(false);
    (notify, local)
}

/// Start `run`; the stop and its task are returned for the end of the test.
fn run(notify: &PgNotify) -> (oneshot::Sender<()>, JoinHandle<()>) {
    let runner = notify.clone();
    let (stop, stopped) = oneshot::channel::<()>();
    let task = tokio::spawn(async move {
        let stopped = async {
            let _ = stopped.await;
        };
        runner.run(stopped).await.expect("notify.run");
    });
    (stop, task)
}

async fn stop(stop: oneshot::Sender<()>, task: JoinHandle<()>) {
    let _ = stop.send(());
    tokio::time::timeout(Duration::from_secs(10), task)
        .await
        .expect("stops in time")
        .expect("task");
}

fn progress(i: usize) -> RunEvent {
    RunEvent::Progress {
        message: format!("{i}"),
    }
}

async fn next_event(sub: &mut RunSubscription) -> RunEvent {
    tokio::time::timeout(PATIENCE, sub.recv())
        .await
        .expect("an event in time")
        .expect("the sink is open")
}

/// One full batch of queued events is one statement; a queue longer than a
/// batch is one statement per batch, the remainder last. The statements are
/// counted after the publisher has returned, so the count is final.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_queued_batch_is_one_statement() {
    let Some(url) = adam_core::testing::test_env("ADAM_TEST_POSTGRES_URL") else {
        return;
    };
    let prefix = format!("adam_b{}_", short());
    let id = RunId::new();

    let (listener, listening) = quiet(&url, &prefix).await;
    let (listener_stop, listener_task) = run(&listener);
    listener.wait_listening().await;
    let mut seen = listening.subscribe_run(id);

    // One full batch, queued before the publisher starts.
    let (sender, _) = quiet(&url, &prefix).await;
    let sink = sender.event_sink();
    for i in 0..MAX_BATCH_ITEMS {
        sink.emit(id, "agent", progress(i)).await;
    }
    let (sender_stop, sender_task) = run(&sender);
    for i in 0..MAX_BATCH_ITEMS {
        assert_eq!(next_event(&mut seen).await, progress(i));
    }
    stop(sender_stop, sender_task).await;
    assert_eq!(
        sender.statements_sent(),
        1,
        "one statement carried the whole batch"
    );

    // More than a batch: two full statements and a remainder of one.
    let (second, _) = quiet(&url, &prefix).await;
    let sink = second.event_sink();
    let count = 2 * MAX_BATCH_ITEMS + 1;
    for i in 0..count {
        sink.emit(id, "agent", progress(1000 + i)).await;
    }
    let (second_stop, second_task) = run(&second);
    for i in 0..count {
        assert_eq!(next_event(&mut seen).await, progress(1000 + i));
    }
    stop(second_stop, second_task).await;
    assert_eq!(second.statements_sent(), 3, "two batches and a remainder");

    stop(listener_stop, listener_task).await;
}

/// The order of the queue survives batching, across statement boundaries too.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_batch_keeps_the_order_of_the_queue() {
    let Some(url) = adam_core::testing::test_env("ADAM_TEST_POSTGRES_URL") else {
        return;
    };
    let prefix = format!("adam_b{}_", short());
    let id = RunId::new();

    let (listener, listening) = quiet(&url, &prefix).await;
    let (listener_stop, listener_task) = run(&listener);
    listener.wait_listening().await;
    let mut seen = listening.subscribe_run(id);

    // More than one batch, so the order is also checked across statements.
    let count = 100;
    assert!(count > MAX_BATCH_ITEMS);
    let (sender, _) = quiet(&url, &prefix).await;
    let sink = sender.event_sink();
    for i in 0..count {
        sink.emit(id, "agent", progress(i)).await;
    }
    let (sender_stop, sender_task) = run(&sender);
    for i in 0..count {
        assert_eq!(
            next_event(&mut seen).await,
            progress(i),
            "event {i} arrives in queue order"
        );
    }
    stop(sender_stop, sender_task).await;
    assert!(
        sender.statements_sent() > 1,
        "the test crossed a batch boundary: {}",
        sender.statements_sent()
    );

    stop(listener_stop, listener_task).await;
}
