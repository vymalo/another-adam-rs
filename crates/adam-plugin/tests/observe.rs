//! ADR 0050, slice H2: what observers receive. A scripted run's events reach a recording observer in
//! order (golden), a replay repeats them with the same keys, a slow or panicking observer changes
//! nothing, and the bounds hold.
#![allow(clippy::unwrap_used, clippy::expect_used)] // integration tests assert by unwrapping

use std::sync::{Arc, Mutex};
use std::time::Duration;

use adam_error::ErrorClass;
use adam_plugin::{
    EventHeader, EventKind, EventQueue, FIELD_MAX, HookError, HookEvent, HookSet, ObserveLimits,
    Plugin,
};
use serde_json::{Value, json};

#[derive(Default)]
struct Recorder(Mutex<Vec<HookEvent>>);

impl Plugin for Recorder {
    fn name(&self) -> &str {
        "recorder"
    }
    fn observe(&self, event: &HookEvent) -> Result<(), HookError> {
        self.0.lock().unwrap().push(event.clone());
        Ok(())
    }
}

fn scrub(text: &str) -> String {
    text.replace("sk-secret-token", "[redacted]")
}

fn header(step: &str, event: EventKind, at: &str) -> EventHeader {
    EventHeader {
        key: format!("run-1/{step}/{}/0", event.name()),
        run: "run-1".into(),
        root_run: "run-1".into(),
        agent: "scripted".into(),
        at: at.into(),
    }
}

/// A scripted run: one turn, one call that succeeds, one that a hook refuses.
fn scripted(at: &str) -> Vec<HookEvent> {
    let ev = |step: &str, kind| HookEvent::new(kind, header(step, kind, at));
    vec![
        ev("model:0", EventKind::TurnStarted).plain("turn", 0),
        ev("tool:c1", EventKind::ToolStarted)
            .text("tool", "read", &scrub)
            .json(
                "arguments",
                &json!({"path": "a.txt", "token": "sk-secret-token"}),
                &scrub,
            ),
        ev("tool:c1", EventKind::ToolCompleted)
            .text("tool", "read", &scrub)
            .text("output", "hello sk-secret-token", &scrub),
        ev("tool:c2", EventKind::ToolStarted).text("tool", "rm", &scrub),
        ev("tool:c2", EventKind::ToolDenied)
            .text("tool", "rm", &scrub)
            .text("reason", "not allowed", &scrub),
        ev("model:0", EventKind::TurnEnded).plain("calls", 2),
    ]
}

fn deliver(events: Vec<HookEvent>) -> Vec<HookEvent> {
    let recorder = Arc::new(Recorder::default());
    let set = HookSet::new().with(recorder.clone());
    let mut queue = EventQueue::new();
    for event in events {
        queue.push(event);
    }
    let report = queue.flush(&set);
    assert_eq!((report.failed, report.dropped), (0, 0));
    recorder.0.lock().unwrap().clone()
}

/// The events as a golden reads them: name, key and fields, without the time.
fn golden_of(events: &[HookEvent]) -> Value {
    Value::Array(
        events
            .iter()
            .map(|e| {
                let mut v = e.to_value();
                v.as_object_mut().unwrap().remove("at");
                v
            })
            .collect(),
    )
}

#[test]
fn a_scripted_run_gives_the_golden_event_list() {
    let got = serde_json::to_string_pretty(&golden_of(&deliver(scripted("t1")))).unwrap() + "\n";
    let path = concat!(env!("CARGO_MANIFEST_DIR"), "/tests/golden/hook_events.json");
    if std::env::var_os("UPDATE_GOLDEN").is_some() {
        std::fs::write(path, &got).unwrap();
    }
    let want = std::fs::read_to_string(path).expect("run once with UPDATE_GOLDEN=1");
    // Compare as values: key order depends on whether `preserve_order` is unified in.
    let want: Value = serde_json::from_str(&want).unwrap();
    let got_value: Value = serde_json::from_str(&got).unwrap();
    assert_eq!(got_value, want);
    assert!(!got.contains("sk-secret-token"), "no secret in an event");
}

#[test]
fn a_replay_repeats_the_events_with_the_same_keys() {
    let first = deliver(scripted("t1"));
    let again = deliver(scripted("t2")); // later, so `at` differs
    let keys = |e: &[HookEvent]| e.iter().map(|e| e.header.key.clone()).collect::<Vec<_>>();
    assert_eq!(keys(&first), keys(&again));
    assert_eq!(golden_of(&first), golden_of(&again));
    let unique: std::collections::BTreeSet<_> = keys(&first).into_iter().collect();
    assert_eq!(unique.len(), first.len(), "a key names one event");
}

struct Panics;
impl Plugin for Panics {
    fn name(&self) -> &str {
        "panics"
    }
    fn observe(&self, _: &HookEvent) -> Result<(), HookError> {
        panic!("on purpose");
    }
}

struct Slow;
impl Plugin for Slow {
    fn name(&self) -> &str {
        "slow"
    }
    fn observe(&self, _: &HookEvent) -> Result<(), HookError> {
        std::thread::sleep(Duration::from_millis(30));
        Ok(())
    }
}

struct Errs;
impl Plugin for Errs {
    fn name(&self) -> &str {
        "errs"
    }
    fn observe(&self, _: &HookEvent) -> Result<(), HookError> {
        Err(HookError::new("errs", ErrorClass::Invalid, "no"))
    }
}

#[test]
fn a_slow_erroring_or_panicking_observer_leaves_the_others_and_the_run_unchanged() {
    let recorder = Arc::new(Recorder::default());
    let set = HookSet::new()
        .with(Arc::new(Panics))
        .with(Arc::new(Errs))
        .with(Arc::new(Slow))
        .with(recorder.clone())
        .with_observe_limits(ObserveLimits {
            per_event: Duration::from_millis(5),
            per_flush: Duration::from_secs(5),
        });
    let events = scripted("t1");
    let n = events.len();
    let mut queue = EventQueue::new();
    for e in events {
        queue.push(e);
    }
    let report = queue.flush(&set);
    assert_eq!(
        report.failed,
        3 * n,
        "each bad observer is counted, per event"
    );
    assert_eq!(report.delivered, n);
    assert_eq!(report.dropped, 0);
    // The well-behaved observer still saw everything, in order.
    assert_eq!(
        golden_of(&recorder.0.lock().unwrap()),
        golden_of(&scripted("t1"))
    );
    assert!(queue.events().is_empty(), "a flush empties the queue");
}

#[test]
fn a_flush_out_of_time_drops_the_rest_and_says_so() {
    let set = HookSet::new()
        .with(Arc::new(Slow))
        .with_observe_limits(ObserveLimits {
            per_event: Duration::from_secs(1),
            per_flush: Duration::from_millis(10),
        });
    let events = scripted("t1");
    let n = events.len();
    let report = set.observe(&events);
    assert!(report.dropped > 0 && report.dropped < n);
    assert_eq!(report.delivered + report.dropped, n);
}

#[test]
fn an_oversized_field_reaches_the_observer_cut_at_4_kib() {
    let big = "z".repeat(FIELD_MAX * 3);
    let event = HookEvent::new(
        EventKind::ToolCompleted,
        header("tool:c1", EventKind::ToolCompleted, "t"),
    )
    .text("output", &big, &scrub);
    let seen = deliver(vec![event]);
    let Value::String(output) = &seen[0].fields["output"] else {
        panic!()
    };
    assert_eq!(output.len(), FIELD_MAX);
    assert_eq!(seen[0].cut, vec!["output".to_owned()]);
}
