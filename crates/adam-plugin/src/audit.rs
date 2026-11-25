//! The `audit` plugin (ADR 0050, slice H6b): every event of the catalogue it observes, as one JSON
//! line on a writer the deployment gives it.
//!
//! Off unless a deployment registers it (`AgentDef::plugin("audit", ...)`) and an agent file names it
//! in `plugins:`. It is an observer: it never changes a run, a failing writer is a counted
//! [`HookError`] and nothing else, and the events it writes are the bounded, scrubbed ones the loop
//! built ([`HookEvent::bounded`]). A line carries the event's `key`, which is stable across replays,
//! so the plugin writes a key once.

use std::collections::HashSet;
use std::io::Write;
use std::sync::Mutex;

use adam_error::ErrorClass;

use crate::{HookError, HookEvent, Plugin};

/// Keys remembered to skip a replayed event; past this the memory is cleared, so a very long run
/// may repeat a line after a replay, never lose one.
const SEEN_MAX: usize = 65_536;

/// The audit plugin over a writer.
pub struct Audit<W: Write + Send> {
    out: Mutex<State<W>>,
}

struct State<W> {
    writer: W,
    seen: HashSet<String>,
}

impl<W: Write + Send> Audit<W> {
    /// An audit that writes its lines to `writer`.
    pub fn new(writer: W) -> Self {
        Self {
            out: Mutex::new(State {
                writer,
                seen: HashSet::new(),
            }),
        }
    }

    /// The writer, when no other holder of the plugin is left to write to it.
    pub fn into_inner(self) -> W {
        self.out
            .into_inner()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .writer
    }
}

impl<W: Write + Send> Plugin for Audit<W> {
    fn name(&self) -> &str {
        "audit"
    }

    fn observe(&self, event: &HookEvent) -> Result<(), HookError> {
        let mut state = self
            .out
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        if state.seen.contains(&event.header.key) {
            return Ok(());
        }
        let fail = |e: std::io::Error| {
            HookError::new("audit", ErrorClass::Transient, format!("cannot write: {e}"))
        };
        let line = event.to_value().to_string();
        writeln!(state.writer, "{line}").map_err(fail)?;
        state.writer.flush().map_err(fail)?;
        if state.seen.len() >= SEEN_MAX {
            state.seen.clear();
        }
        state.seen.insert(event.header.key.clone());
        Ok(())
    }
}
