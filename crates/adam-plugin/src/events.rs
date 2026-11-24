//! What an observer receives (ADR 0050, slice **H2**): the catalogue of core events, the bounds
//! every event is held to, and the ordered queue that is flushed before a transition commits.
//!
//! An observer only reads. It runs outside the journal, it cannot change the run, and a failure of
//! its own (an error, a panic, a missed deadline) is counted and dropped.
//!
//! **No secret in an event.** An event carries only the fields its builder is given: never a run
//! context entry, a header value or an environment value. Every text and every string inside a JSON
//! field passes the caller's scrubber *first* and is cut after, so a secret that straddles the cut is
//! not left half-visible.
//!
//! **Bounds.** Text is cut at [`FIELD_MAX`] (4 KiB) a field on a character boundary, a JSON field at
//! [`JSON_MAX`] (16 KiB), and a whole event at [`EVENT_MAX`] (32 KiB). A cut field says so: its name
//! is listed in [`HookEvent::cut`].

use std::collections::BTreeMap;
use std::panic::{AssertUnwindSafe, catch_unwind};
use std::time::{Duration, Instant};

use serde_json::{Map, Value};

use crate::HookSet;

/// The longest a text field of an event is, in bytes.
pub const FIELD_MAX: usize = 4 * 1024;
/// The longest a JSON field (arguments, an output) of an event is, serialized, in bytes.
pub const JSON_MAX: usize = 16 * 1024;
/// The longest a whole event is, serialized, in bytes.
pub const EVENT_MAX: usize = 32 * 1024;

/// A core event of the catalogue (ADR 0050, decision 5): the ones an observer reads.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
#[non_exhaustive]
pub enum EventKind {
    /// The first transition of a run.
    RunStarted,
    /// Before `model:<turn>`.
    TurnStarted,
    /// A model step returned.
    ModelResponded,
    /// A model step failed.
    ModelFailed,
    /// A model call's usage is reported.
    UsageRecorded,
    /// A call's step is about to run.
    ToolStarted,
    /// A call's step is recorded, and the tool answered.
    ToolCompleted,
    /// A call's step is recorded, and the call failed.
    ToolFailed,
    /// A call's step is recorded, and a hook refused it.
    ToolDenied,
    /// Every call of the turn has a result.
    TurnEnded,
    /// A compaction was recorded.
    CompactionEnded,
    /// The agent saw its cancel during a step.
    RunCancelled,
    /// A one-shot child run was started for a call, which now waits for it.
    ChildStarted,
    /// A one-shot child run ended and its outcome answered the call.
    ChildEnded,
    /// A session was opened (`subagent_start` answered).
    SessionOpened,
    /// A round was started in a session (`subagent_send` answered).
    SessionRound,
    /// A session's ended round was reported to the model.
    SessionReported,
    /// A session was closed (`subagent_close` answered).
    SessionClosed,
}

impl EventKind {
    /// The event's name, as the catalogue and a program's payload spell it.
    #[must_use]
    pub fn name(self) -> &'static str {
        match self {
            EventKind::RunStarted => "run.started",
            EventKind::TurnStarted => "turn.started",
            EventKind::ModelResponded => "model.responded",
            EventKind::ModelFailed => "model.failed",
            EventKind::UsageRecorded => "usage.recorded",
            EventKind::ToolStarted => "tool.started",
            EventKind::ToolCompleted => "tool.completed",
            EventKind::ToolFailed => "tool.failed",
            EventKind::ToolDenied => "tool.denied",
            EventKind::TurnEnded => "turn.ended",
            EventKind::CompactionEnded => "compaction.ended",
            EventKind::RunCancelled => "run.cancelled",
            EventKind::ChildStarted => "child.started",
            EventKind::ChildEnded => "child.ended",
            EventKind::SessionOpened => "session.opened",
            EventKind::SessionRound => "session.round",
            EventKind::SessionReported => "session.reported",
            EventKind::SessionClosed => "session.closed",
        }
    }
}

/// Who an event is about, and when: the part every event carries.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct EventHeader {
    /// The event's key, stable across replays (`<run>/<step>/<event>/<index>`).
    pub key: String,
    /// The run it happened in.
    pub run: String,
    /// The run at the root of the chain of parents (the run itself when it has none).
    pub root_run: String,
    /// The agent's name.
    pub agent: String,
    /// When it was observed, as text. Not part of the key: a replay observes it again later.
    pub at: String,
}

/// What an observer receives: a core event with its header and its bounded, scrubbed fields.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct HookEvent {
    /// Which event.
    pub kind: EventKind,
    /// Who and when.
    pub header: EventHeader,
    /// The event's fields, by name.
    pub fields: BTreeMap<String, Value>,
    /// The names of the fields that were cut (or dropped) to keep the bounds.
    pub cut: Vec<String>,
}

impl HookEvent {
    /// An event of `kind` with no field yet.
    #[must_use]
    pub fn new(kind: EventKind, header: EventHeader) -> Self {
        HookEvent {
            kind,
            header,
            fields: BTreeMap::new(),
            cut: Vec::new(),
        }
    }

    /// The same event with a text field: scrubbed with `scrub`, then cut at [`FIELD_MAX`].
    #[must_use]
    pub fn text(mut self, name: &str, text: &str, scrub: &dyn Fn(&str) -> String) -> Self {
        let (text, cut) = cut_text(&scrub(text), FIELD_MAX);
        if cut {
            self.cut.push(name.to_owned());
        }
        self.fields.insert(name.to_owned(), Value::String(text));
        self
    }

    /// The same event with a JSON field: every string inside (and every key) is scrubbed with
    /// `scrub`, each is cut at [`FIELD_MAX`], and a value that serializes past [`JSON_MAX`] is
    /// replaced by its cut serialization, as text.
    #[must_use]
    pub fn json(mut self, name: &str, value: &Value, scrub: &dyn Fn(&str) -> String) -> Self {
        let mut value = scrub_value(value, scrub);
        let mut cut = false;
        let size = value.to_string();
        if size.len() > JSON_MAX {
            value = Value::String(cut_text(&size, JSON_MAX).0);
            cut = true;
        }
        if cut {
            self.cut.push(name.to_owned());
        }
        self.fields.insert(name.to_owned(), value);
        self
    }

    /// The same event with a number or a flag, which need no bound.
    #[must_use]
    pub fn plain(mut self, name: &str, value: impl Into<Value>) -> Self {
        self.fields.insert(name.to_owned(), value.into());
        self
    }

    /// The event as the JSON a program's payload carries.
    #[must_use]
    pub fn to_value(&self) -> Value {
        let mut map = Map::new();
        map.insert("name".into(), self.kind.name().into());
        map.insert("key".into(), self.header.key.clone().into());
        map.insert("run".into(), self.header.run.clone().into());
        map.insert("root_run".into(), self.header.root_run.clone().into());
        map.insert("agent".into(), self.header.agent.clone().into());
        map.insert("at".into(), self.header.at.clone().into());
        map.insert(
            "fields".into(),
            Value::Object(
                self.fields
                    .iter()
                    .map(|(k, v)| (k.clone(), v.clone()))
                    .collect(),
            ),
        );
        if !self.cut.is_empty() {
            map.insert("cut".into(), self.cut.clone().into());
        }
        Value::Object(map)
    }

    /// The same event held to [`EVENT_MAX`]: while it serializes past it, the largest field is
    /// dropped (its value becomes `null`) and named in [`HookEvent::cut`].
    #[must_use]
    pub fn bounded(mut self) -> Self {
        while self.to_value().to_string().len() > EVENT_MAX {
            let largest = self
                .fields
                .iter()
                .filter(|(_, v)| !v.is_null())
                .max_by_key(|(_, v)| v.to_string().len())
                .map(|(k, _)| k.clone());
            let Some(name) = largest else { break };
            self.fields.insert(name.clone(), Value::Null);
            if !self.cut.contains(&name) {
                self.cut.push(name);
            }
        }
        self
    }
}

/// `text` cut to at most `max` bytes on a character boundary; whether it was cut.
fn cut_text(text: &str, max: usize) -> (String, bool) {
    if text.len() <= max {
        return (text.to_owned(), false);
    }
    let mut end = max;
    while !text.is_char_boundary(end) {
        end -= 1;
    }
    (text[..end].to_owned(), true)
}

fn scrub_value(value: &Value, scrub: &dyn Fn(&str) -> String) -> Value {
    match value {
        Value::String(s) => Value::String(cut_text(&scrub(s), FIELD_MAX).0),
        Value::Array(items) => Value::Array(items.iter().map(|v| scrub_value(v, scrub)).collect()),
        Value::Object(map) => Value::Object(
            map.iter()
                .map(|(k, v)| (scrub(k), scrub_value(v, scrub)))
                .collect(),
        ),
        other => other.clone(),
    }
}

/// How long an observer may take, per event and for a whole flush.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ObserveLimits {
    /// The longest one observer may take for one event before it counts as failed.
    pub per_event: Duration,
    /// The longest a flush may take in all; events after it are dropped, not delivered late.
    pub per_flush: Duration,
}

impl Default for ObserveLimits {
    fn default() -> Self {
        // ADR 0050, decision 8: 2 s an event, 5 s for all of a transition's observers.
        ObserveLimits {
            per_event: Duration::from_secs(2),
            per_flush: Duration::from_secs(5),
        }
    }
}

/// What a flush did, for a log line and a count. It never changes the run.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct FlushReport {
    /// Observer calls that returned `Ok` in time.
    pub delivered: usize,
    /// Observer calls that errored, panicked or missed their deadline.
    pub failed: usize,
    /// Events not offered to anyone because the flush ran out of time.
    pub dropped: usize,
}

/// Events in the order they happened, held until the flush before a commit.
#[derive(Debug, Default)]
pub struct EventQueue {
    events: Vec<HookEvent>,
}

impl EventQueue {
    /// An empty queue.
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }

    /// Appends `event`, bounded to [`EVENT_MAX`]: the queue's order is the order of the pushes.
    pub fn push(&mut self, event: HookEvent) {
        self.events.push(event.bounded());
    }

    /// The events waiting, in order.
    #[must_use]
    pub fn events(&self) -> &[HookEvent] {
        &self.events
    }

    /// Offers every event, in order, to every plugin of `set`, then empties the queue. A plugin's
    /// failure is counted and dropped; nothing here returns an error, so a flush cannot change a run.
    pub fn flush(&mut self, set: &HookSet) -> FlushReport {
        let events = std::mem::take(&mut self.events);
        set.observe(&events)
    }
}

impl HookSet {
    /// The same set with `limits` for its observers.
    #[must_use]
    pub fn with_observe_limits(mut self, limits: ObserveLimits) -> Self {
        self.observe_limits = limits;
        self
    }

    /// Offers `events`, in order, to every plugin's `observe`, in declaration order.
    ///
    /// An observer's error, panic or missed deadline is counted in the report and dropped: the
    /// next plugin and the next event are offered as if it had answered. Once the flush has taken
    /// [`ObserveLimits::per_flush`], the events left are dropped and counted.
    #[must_use]
    pub fn observe(&self, events: &[HookEvent]) -> FlushReport {
        let mut report = FlushReport::default();
        let started = Instant::now();
        for (at, event) in events.iter().enumerate() {
            if started.elapsed() > self.observe_limits.per_flush {
                report.dropped = events.len() - at;
                break;
            }
            for plugin in &self.plugins {
                let begun = Instant::now();
                let outcome = catch_unwind(AssertUnwindSafe(|| plugin.observe(event)));
                match outcome {
                    Ok(Ok(())) if begun.elapsed() <= self.observe_limits.per_event => {
                        report.delivered += 1;
                    }
                    _ => report.failed += 1,
                }
            }
        }
        report
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn header(key: &str) -> EventHeader {
        EventHeader {
            key: key.into(),
            run: "r".into(),
            root_run: "r".into(),
            agent: "a".into(),
            at: "t".into(),
        }
    }

    fn keep(s: &str) -> String {
        s.to_owned()
    }

    #[test]
    fn a_text_field_is_cut_at_4_kib_on_a_character_boundary_and_says_so() {
        let long = "é".repeat(FIELD_MAX); // two bytes each: the cut falls on a boundary
        let event =
            HookEvent::new(EventKind::ToolCompleted, header("k")).text("output", &long, &keep);
        let Value::String(kept) = &event.fields["output"] else {
            panic!("a text field is a string");
        };
        assert!(kept.len() <= FIELD_MAX, "{}", kept.len());
        assert!(kept.chars().all(|c| c == 'é'));
        assert_eq!(event.cut, vec!["output".to_owned()]);
    }

    #[test]
    fn a_short_field_is_kept_whole_and_not_marked() {
        let event =
            HookEvent::new(EventKind::ToolCompleted, header("k")).text("output", "ok", &keep);
        assert_eq!(event.fields["output"], Value::String("ok".into()));
        assert!(event.cut.is_empty());
    }

    #[test]
    fn the_scrubber_runs_before_the_cut() {
        // The secret straddles the 4 KiB mark: cutting first would leave half of it.
        let mut text = "a".repeat(FIELD_MAX - 3);
        text.push_str("SECRETVALUE");
        let scrub = |s: &str| s.replace("SECRETVALUE", "[x]");
        let event = HookEvent::new(EventKind::ToolStarted, header("k")).text("t", &text, &scrub);
        let Value::String(kept) = &event.fields["t"] else {
            panic!()
        };
        assert!(!kept.contains("SECRET"), "{kept}");
        assert!(kept.ends_with("[x]"));
    }

    #[test]
    fn json_strings_and_keys_are_scrubbed_and_an_oversized_value_is_cut() {
        let scrub = |s: &str| s.replace("hunter2", "[x]");
        let value = serde_json::json!({"hunter2": "pw hunter2", "list": ["hunter2"]});
        let event =
            HookEvent::new(EventKind::ToolStarted, header("k")).json("arguments", &value, &scrub);
        assert!(!event.to_value().to_string().contains("hunter2"));

        let big = serde_json::json!({"rows": vec!["x".repeat(1000); 40]});
        let event =
            HookEvent::new(EventKind::ToolStarted, header("k")).json("arguments", &big, &keep);
        let Value::String(kept) = &event.fields["arguments"] else {
            panic!("a cut JSON field is text");
        };
        assert!(kept.len() <= JSON_MAX);
        assert_eq!(event.cut, vec!["arguments".to_owned()]);
    }

    #[test]
    fn a_whole_event_is_held_to_32_kib_by_dropping_its_largest_fields() {
        let mut event = HookEvent::new(EventKind::ToolCompleted, header("k"));
        for i in 0..20 {
            event = event.text(&format!("f{i:02}"), &"y".repeat(FIELD_MAX), &keep);
        }
        let event = event.bounded();
        assert!(event.to_value().to_string().len() <= EVENT_MAX);
        assert!(!event.cut.is_empty());
    }

    #[test]
    fn the_queue_keeps_the_order_of_the_pushes() {
        let mut queue = EventQueue::new();
        queue.push(HookEvent::new(EventKind::TurnStarted, header("1")));
        queue.push(HookEvent::new(EventKind::ToolStarted, header("2")));
        let keys: Vec<_> = queue
            .events()
            .iter()
            .map(|e| e.header.key.as_str())
            .collect();
        assert_eq!(keys, ["1", "2"]);
    }
}
