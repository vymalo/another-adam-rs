//! The suite against [`LocalNotifier`](adam_runtime::LocalNotifier): always on.
//!
//! Both sides share one notifier and one event sink (there is no transport in
//! one process), so `events_cross_once_without_echo` only checks that nothing
//! is delivered twice; the cross-process meaning is exercised by the adapters.
#![allow(clippy::unwrap_used, clippy::expect_used)] // integration tests assert by unwrapping

async fn make_pair() -> Option<adam_notify_testkit::Pair> {
    Some(adam_notify_testkit::cases::local_pair())
}

adam_notify_testkit::notifier_conformance!(make_pair);
