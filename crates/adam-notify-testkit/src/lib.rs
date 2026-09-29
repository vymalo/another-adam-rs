//! Conformance suite for [`adam_runtime::Notifier`] implementations and for the
//! cross-process [`adam_runtime::EventSink`] that usually ships with them.
//!
//! An adapter crate gives [`notifier_conformance!`] an `async fn() ->
//! Option<Pair>`: two [`Side`]s standing for two processes of one deployment,
//! wired to each other and already listening. The generated tests then check
//! what `adam-runtime` relies on: signals cross, in order, to every subscriber;
//! events cross exactly once and without an echo; and an oversize event
//! or a flood of publishes neither breaks the stream nor blocks the caller.
//!
//! ```ignore
//! async fn make_pair() -> Option<adam_notify_testkit::Pair> { /* None: skip */ }
//! adam_notify_testkit::notifier_conformance!(make_pair);
//! ```
//!
//! Every case uses fresh run ids, so cases may share channels and run in
//! parallel; an adapter that talks to a server still gives each pair its own
//! channel prefix so parallel cases stay out of each other's way.

#![warn(missing_docs)]
#![allow(clippy::unwrap_used, clippy::expect_used)] // a test kit asserts by unwrapping

use std::sync::Arc;

use adam_runtime::{BroadcastSink, DynEventSink, DynNotifier};

pub use adam_core::testing::skipped;

/// One process of a deployment, as far as notifications go.
#[derive(Clone)]
pub struct Side {
    /// Publishes this process's signals and receives everyone's.
    pub notifier: DynNotifier,
    /// What this process's runtime emits events into.
    pub sink: DynEventSink,
    /// Where the events of the *other* process arrive (and this process's own,
    /// once, locally): the sink a front's SSE subscriptions attach to.
    pub local: BroadcastSink,
}

impl Side {
    /// A side from its three parts.
    pub fn new(
        notifier: impl adam_runtime::Notifier,
        sink: impl adam_runtime::EventSink,
        local: BroadcastSink,
    ) -> Self {
        Self {
            notifier: Arc::new(notifier),
            sink: Arc::new(sink),
            local,
        }
    }
}

/// Two sides that reach each other, both already listening.
pub type Pair = (Side, Side);

/// Generates one `#[tokio::test]` per case.
///
/// `$make` is a path to `async fn() -> Option<Pair>`; `None` skips the suite
/// (and panics under `ADAM_TEST_REQUIRE_DB=1`, see [`skipped`]). The calling
/// crate needs `tokio` with `macros` and `rt-multi-thread` as a
/// dev-dependency.
#[macro_export]
macro_rules! notifier_conformance {
    ($make:path) => {
        $crate::notifier_conformance!(@cases $make;
            signal_reaches_the_other_side,
            signal_reaches_the_publisher_too,
            signals_keep_order_from_one_publisher,
            every_subscriber_gets_every_signal,
            events_cross_once_without_echo,
            oversize_event_does_not_break_the_stream,
            publish_never_blocks_without_listeners,
        );
    };
    (@cases $make:path; $($case:ident),+ $(,)?) => {
        $(
            #[tokio::test(flavor = "multi_thread", worker_threads = 4)]
            async fn $case() {
                let Some(pair) = $make().await else {
                    $crate::skipped(&format!("{}: notifier not configured", stringify!($case)));
                    return;
                };
                $crate::cases::$case(pair).await;
            }
        )+
    };
}

/// The cases, as plain async functions for harnesses that do not use the macro.
pub mod cases {
    use std::time::Duration;

    use adam_core::{RunId, RunStatus};
    use adam_runtime::{BroadcastSink, Delivery, RunEvent, RunSubscription, Signal};
    use futures::StreamExt;
    use futures::stream::BoxStream;

    use super::{Pair, Side};

    /// How long a case waits for something that must happen.
    const PATIENCE: Duration = Duration::from_secs(5);
    /// How long a case waits to be sure something does not happen.
    const QUIET: Duration = Duration::from_millis(400);

    fn runnable(agent: &str) -> Signal {
        Signal::Runnable {
            run: RunId::new(),
            agent: agent.to_owned(),
        }
    }

    /// The next signal, skipping resyncs (an adapter may resync at any time).
    async fn next_signal(stream: &mut BoxStream<'static, Delivery>) -> Signal {
        tokio::time::timeout(PATIENCE, async {
            loop {
                match stream.next().await {
                    Some(Delivery::Signal(s)) => return s,
                    Some(Delivery::Resync) => {}
                    None => panic!("the subscription ended"),
                }
            }
        })
        .await
        .expect("a signal in time")
    }

    async fn next_event(sub: &mut RunSubscription) -> RunEvent {
        tokio::time::timeout(PATIENCE, sub.recv())
            .await
            .expect("an event in time")
            .expect("the sink is open")
    }

    /// No further event of the run arrives within [`QUIET`].
    async fn no_more_events(sub: &mut RunSubscription) {
        if let Ok(extra) = tokio::time::timeout(QUIET, sub.recv()).await {
            panic!("an event arrived twice or unexpectedly: {extra:?}");
        }
    }

    fn progress(message: &str) -> RunEvent {
        RunEvent::Progress {
            message: message.to_owned(),
        }
    }

    /// A signal published on one side reaches a subscriber on the other.
    pub async fn signal_reaches_the_other_side((a, b): Pair) {
        let mut on_b = b.notifier.subscribe();
        let signal = runnable("coder");
        a.notifier.publish(signal.clone()).await;
        assert_eq!(next_signal(&mut on_b).await, signal);
    }

    /// The publisher's own subscribers get its signals too (the runtime
    /// relies on this to see its own cancels through the same path).
    pub async fn signal_reaches_the_publisher_too((a, _b): Pair) {
        let mut on_a = a.notifier.subscribe();
        let signal = Signal::Finished { run: RunId::new() };
        a.notifier.publish(signal.clone()).await;
        assert_eq!(next_signal(&mut on_a).await, signal);
    }

    /// Signals of one publisher arrive in the order they were published.
    pub async fn signals_keep_order_from_one_publisher((a, b): Pair) {
        let mut on_b = b.notifier.subscribe();
        let sent: Vec<Signal> = (0..50)
            .map(|i| {
                if i % 3 == 0 {
                    Signal::Finished { run: RunId::new() }
                } else {
                    runnable(&format!("agent-{i}"))
                }
            })
            .collect();
        for signal in &sent {
            a.notifier.publish(signal.clone()).await;
        }
        for signal in &sent {
            assert_eq!(&next_signal(&mut on_b).await, signal);
        }
    }

    /// Every subscriber, on either side, gets every signal, in order.
    pub async fn every_subscriber_gets_every_signal((a, b): Pair) {
        let mut subs = [
            a.notifier.subscribe(),
            b.notifier.subscribe(),
            b.notifier.subscribe(),
        ];
        let sent = [
            runnable("x"),
            Signal::Finished { run: RunId::new() },
            runnable("y"),
        ];
        for signal in &sent {
            a.notifier.publish(signal.clone()).await;
        }
        for sub in &mut subs {
            for signal in &sent {
                assert_eq!(&next_signal(sub).await, signal);
            }
        }
    }

    /// An event emitted on one side is delivered to the local sink of both:
    /// once on the emitting side (directly), once on the other (across), and
    /// never a second time as an echo.
    ///
    /// For an in-process notifier the two sides share one [`BroadcastSink`],
    /// so there is no transport to echo through and the case only checks that
    /// nothing is delivered twice.
    pub async fn events_cross_once_without_echo((a, b): Pair) {
        let run = RunId::new();
        let mut on_a = a.local.subscribe_run(run);
        let mut on_b = b.local.subscribe_run(run);
        let event = progress("crossing");
        a.sink.emit(run, "agent", event.clone()).await;
        assert_eq!(next_event(&mut on_a).await, event);
        assert_eq!(next_event(&mut on_b).await, event);
        no_more_events(&mut on_a).await;
        no_more_events(&mut on_b).await;
    }

    /// An event too big for the transport neither breaks the stream nor
    /// reorders it: what follows still arrives. A `Status` still arrives, with
    /// its detail cut short if need be; a big `Progress` may be dropped.
    pub async fn oversize_event_does_not_break_the_stream((a, b): Pair) {
        let run = RunId::new();
        let mut on_b = b.local.subscribe_run(run);
        let big = "é".repeat(20_000);
        a.sink.emit(run, "agent", progress(&big)).await;
        a.sink
            .emit(
                run,
                "agent",
                RunEvent::Status {
                    status: RunStatus::Failed,
                    detail: Some(big.clone()),
                },
            )
            .await;
        a.sink.emit(run, "agent", progress("after")).await;

        let mut status = None;
        loop {
            match next_event(&mut on_b).await {
                RunEvent::Progress { message } if message == "after" => break,
                RunEvent::Progress { message } => {
                    assert_eq!(message, big, "a big progress arrives whole or not at all");
                }
                RunEvent::Status { status: s, detail } => status = Some((s, detail)),
                other => panic!("unexpected event {other:?}"),
            }
        }
        let (s, detail) = status.expect("an oversize status still arrives");
        assert_eq!(s, RunStatus::Failed);
        assert!(
            big.starts_with(&detail.expect("with a detail")),
            "its detail is a prefix of the original"
        );

        // Signals too: a huge one may vanish, the next one must not.
        let mut sigs = b.notifier.subscribe();
        a.notifier.publish(runnable(&"n".repeat(20_000))).await;
        let small = runnable("small");
        a.notifier.publish(small.clone()).await;
        loop {
            let got = next_signal(&mut sigs).await;
            if got == small {
                break;
            }
            assert!(
                matches!(&got, Signal::Runnable { agent, .. } if agent.len() == 20_000),
                "only the huge signal may come before: {got:?}"
            );
        }
    }

    /// Publishing and emitting return at once, with or without a subscriber
    /// and however many are queued.
    pub async fn publish_never_blocks_without_listeners((a, _b): Pair) {
        let signal = Signal::Finished { run: RunId::new() };
        let run = RunId::new();
        tokio::time::timeout(PATIENCE, async {
            for _ in 0..5_000 {
                a.notifier.publish(signal.clone()).await;
            }
            for _ in 0..5_000 {
                a.sink.emit(run, "agent", progress("flood")).await;
            }
        })
        .await
        .expect("publishing never waits for the transport");
    }

    /// Two sides of an in-process pair: one [`LocalNotifier`](adam_runtime::LocalNotifier)
    /// and one [`BroadcastSink`] shared by both. See
    /// [`events_cross_once_without_echo`] for what that means for events.
    pub fn local_pair() -> Pair {
        let notifier = adam_runtime::LocalNotifier::default();
        let local = BroadcastSink::default();
        let side = || Side::new(notifier.clone(), local.clone(), local.clone());
        (side(), side())
    }
}
