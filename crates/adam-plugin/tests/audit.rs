//! ADR 0050, slice H6b: the `audit` plugin writes the observed events of a scripted run as lines
//! (golden), once per key, scrubbed as the loop built them, and writes nothing when it sees nothing.
#![allow(clippy::unwrap_used, clippy::expect_used)] // integration tests assert by unwrapping

use std::io::{self, Write};
use std::sync::{Arc, Mutex};

use adam_plugin::{Audit, EventHeader, EventKind, EventQueue, HookEvent, HookSet};
use serde_json::{Value, json};

/// A writer the test can read after the plugin has it.
#[derive(Clone, Default)]
struct Shared(Arc<Mutex<Vec<u8>>>);

impl Write for Shared {
    fn write(&mut self, buf: &[u8]) -> io::Result<usize> {
        self.0.lock().unwrap().extend_from_slice(buf);
        Ok(buf.len())
    }
    fn flush(&mut self) -> io::Result<()> {
        Ok(())
    }
}

impl Shared {
    fn lines(&self) -> Vec<String> {
        String::from_utf8(self.0.lock().unwrap().clone())
            .unwrap()
            .lines()
            .map(str::to_owned)
            .collect()
    }
}

fn scrub(text: &str) -> String {
    text.replace("sk-secret-token", "[redacted]")
}

fn scripted(at: &str) -> Vec<HookEvent> {
    let ev = |step: &str, kind: EventKind| {
        HookEvent::new(
            kind,
            EventHeader {
                key: format!("run-1/{step}/{}/0", kind.name()),
                run: "run-1".into(),
                root_run: "run-1".into(),
                agent: "scripted".into(),
                at: at.into(),
            },
        )
    };
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
        ev("model:0", EventKind::TurnEnded).plain("calls", 1),
    ]
}

fn run(set: &HookSet, events: Vec<HookEvent>) {
    let mut queue = EventQueue::new();
    for event in events {
        queue.push(event);
    }
    let report = queue.flush(set);
    assert_eq!((report.failed, report.dropped), (0, 0));
}

#[test]
fn the_audit_lines_of_a_scripted_run_match_the_golden() {
    let out = Shared::default();
    let set = HookSet::new().with(Arc::new(Audit::new(out.clone())));
    run(&set, scripted("t1"));
    let lines: Vec<Value> = out
        .lines()
        .iter()
        .map(|l| serde_json::from_str(l).unwrap())
        .collect();
    let got = serde_json::to_string_pretty(&Value::Array(lines)).unwrap() + "\n";
    let path = concat!(env!("CARGO_MANIFEST_DIR"), "/tests/golden/audit_lines.json");
    if std::env::var_os("UPDATE_GOLDEN").is_some() {
        std::fs::write(path, &got).unwrap();
    }
    let want: Value = serde_json::from_str(
        &std::fs::read_to_string(path).expect("run once with UPDATE_GOLDEN=1"),
    )
    .unwrap();
    assert_eq!(serde_json::from_str::<Value>(&got).unwrap(), want);
    assert!(!got.contains("sk-secret-token"), "no secret in a line");
}

#[test]
fn a_replay_writes_each_key_once() {
    let out = Shared::default();
    let set = HookSet::new().with(Arc::new(Audit::new(out.clone())));
    run(&set, scripted("t1"));
    run(&set, scripted("t2")); // the same keys, observed again later
    assert_eq!(out.lines().len(), 4);
}

#[test]
fn off_by_default_a_set_without_audit_writes_no_line() {
    let out = Shared::default();
    let _unused = Audit::new(out.clone()); // built but not part of the set the run uses
    run(&HookSet::new(), scripted("t1"));
    assert!(out.lines().is_empty());
}
