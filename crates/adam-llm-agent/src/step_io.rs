//! [`StepIo`]: what a tool call's step says of the call's input and output, and what is scrubbed first.
//!
//! The step of a call (`tool:<call id>`, [`RunEvent::Step`](adam_runtime::RunEvent::Step)) carries the
//! arguments the model gave on the report that starts it, and what the tool answered, or the error it
//! ended with, on the report that ends it (ADR 0011). Both are **recorded by whoever reads the steps**
//! (the orchestration layer keeps them in its log), so what an agent sends is the first line of defence:
//!
//! 1. the agent's own [redactor](StepIo::redact) goes over every string first (exact values it knows
//!    to be secret: the model key, a token it minted);
//! 2. then the bounds of the contract cut what is left ([`StepEvent::with_input`], [`StepOutput`]):
//!    4 KiB of input, 8 KiB of output (the head and the tail).
//!
//! Redaction comes first on purpose: a cut can leave the front half of a value that a redactor would
//! no longer recognise.
//!
//! The default is to send both, with no redactor: an agent that holds secrets sets one
//! ([`LlmAgentBuilder::step_io`](crate::LlmAgentBuilder::step_io)), and [`StepIo::off`] sends neither.
//! The orchestration layer redacts patterns too, and has its own switch; this is the agent's half.

use std::fmt;
use std::sync::Arc;

use adam_runtime::{STEP_INPUT_MAX_BYTES, STEP_OUTPUT_MAX_BYTES, StepOutput};
use serde_json::{Map, Value};

type Redact = Arc<dyn Fn(&str) -> String + Send + Sync>;

/// How a tool call's step reports its input and output: what is scrubbed first, and how much is kept.
///
/// ```
/// use adam_llm_agent::StepIo;
///
/// // Scrub a value this process holds, and keep less than the contract allows.
/// let io = StepIo::default()
///     .redact(|text| text.replace("hunter2-hunter2", "[redacted]"))
///     .output_max(2048);
/// # let _ = io;
/// ```
#[derive(Clone)]
pub struct StepIo {
    redact: Option<Redact>,
    input_max: usize,
    output_max: usize,
    on: bool,
}

impl Default for StepIo {
    fn default() -> Self {
        Self {
            redact: None,
            input_max: STEP_INPUT_MAX_BYTES,
            output_max: STEP_OUTPUT_MAX_BYTES,
            on: true,
        }
    }
}

impl fmt::Debug for StepIo {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("StepIo")
            .field("on", &self.on)
            .field("redact", &self.redact.is_some())
            .field("input_max", &self.input_max)
            .field("output_max", &self.output_max)
            .finish()
    }
}

impl StepIo {
    /// Send neither the input nor the output of a call: its step is a label and a state, as it was
    /// before ADR 0011.
    #[must_use]
    pub fn off() -> Self {
        Self {
            on: false,
            ..Self::default()
        }
    }

    /// Scrub every string of a call's arguments (and every key) and of its result with `redact`
    /// before they are sent. It sees one string at a time and must return it with what is secret
    /// replaced (by `[redacted]`, say); it is called on every call of every tool, so it should be
    /// cheap, and it must not fail.
    #[must_use]
    pub fn redact(mut self, redact: impl Fn(&str) -> String + Send + Sync + 'static) -> Self {
        self.redact = Some(Arc::new(redact));
        self
    }

    /// Keep at most `bytes` of a call's input once serialized (default and ceiling:
    /// [`STEP_INPUT_MAX_BYTES`]).
    #[must_use]
    pub fn input_max(mut self, bytes: usize) -> Self {
        self.input_max = bytes.min(STEP_INPUT_MAX_BYTES);
        self
    }

    /// Keep at most `bytes` of a call's output (default and ceiling: [`STEP_OUTPUT_MAX_BYTES`]).
    #[must_use]
    pub fn output_max(mut self, bytes: usize) -> Self {
        self.output_max = bytes.min(STEP_OUTPUT_MAX_BYTES);
        self
    }

    /// The arguments of a call, scrubbed, for the report that starts its step; `None` when the step
    /// is not to carry them (the switch is off, or the model gave none: nothing, an empty object or
    /// something that is not an object). The cut is [`StepEvent::with_input_within`](adam_runtime::StepEvent::with_input_within)'s.
    pub(crate) fn input(&self, arguments: &Value) -> Option<(Map<String, Value>, usize)> {
        if !self.on {
            return None;
        }
        // An object with a member, as the orchestration layer keeps one: no arguments is no input.
        let Value::Object(map) = arguments else {
            return None;
        };
        if map.is_empty() {
            return None;
        }
        let map = match &self.redact {
            Some(redact) => scrub_map(map, redact.as_ref()),
            None => map.clone(),
        };
        Some((map, self.input_max))
    }

    /// The result of a call, scrubbed and cut, for the report that ends its step; `None` when the
    /// step is not to carry it. `error`: the call failed and `text` says why.
    pub(crate) fn output(&self, text: &str, error: bool) -> Option<StepOutput> {
        if !self.on {
            return None;
        }
        Some(match &self.redact {
            Some(redact) => StepOutput::within(redact(text), error, self.output_max),
            None => StepOutput::within(text, error, self.output_max),
        })
    }
}

/// `map` with every key and every string inside it passed through `redact`.
fn scrub_map(
    map: &Map<String, Value>,
    redact: &(dyn Fn(&str) -> String + Send + Sync),
) -> Map<String, Value> {
    map.iter()
        .map(|(key, value)| (redact(key), scrub(value, redact)))
        .collect()
}

fn scrub(value: &Value, redact: &(dyn Fn(&str) -> String + Send + Sync)) -> Value {
    match value {
        Value::String(text) => Value::String(redact(text)),
        Value::Array(items) => Value::Array(items.iter().map(|v| scrub(v, redact)).collect()),
        Value::Object(map) => Value::Object(scrub_map(map, redact)),
        other => other.clone(),
    }
}

#[cfg(test)]
mod tests {
    use serde_json::json;

    use super::*;

    fn secret_out(text: &str) -> String {
        text.replace("hunter2-hunter2", "[redacted]")
    }

    #[test]
    fn the_redactor_goes_over_every_string_and_key_of_the_input() {
        let io = StepIo::default().redact(secret_out);
        let (input, max) = io
            .input(&json!({
                "url": "https://u:hunter2-hunter2@h/x",
                "headers": {"hunter2-hunter2": ["a hunter2-hunter2 b", 3]},
                "n": 1
            }))
            .unwrap();
        assert_eq!(max, STEP_INPUT_MAX_BYTES);
        assert_eq!(
            Value::Object(input),
            json!({"url": "https://u:[redacted]@h/x", "headers": {"[redacted]": ["a [redacted] b", 3]}, "n": 1})
        );
    }

    #[test]
    fn the_redactor_goes_over_the_output_before_it_is_cut() {
        // The secret straddles the cut: redacting after the cut would leave half of it.
        let secret = "hunter2-hunter2";
        let filler = "a".repeat(6_000);
        let text = format!("{filler}{secret}{}", "b".repeat(6_000));
        let kept = StepIo::default()
            .redact(secret_out)
            .output(&text, false)
            .unwrap();
        assert!(kept.truncated && !kept.text.contains("hunter2"));
        // Without a redactor nothing is scrubbed (and the agent must have set one).
        let raw = StepIo::default()
            .output("a hunter2-hunter2", false)
            .unwrap();
        assert_eq!(raw.text, "a hunter2-hunter2");
    }

    #[test]
    fn what_is_not_an_object_has_no_input_and_off_sends_neither() {
        let io = StepIo::default();
        assert!(io.input(&Value::Null).is_none());
        assert!(io.input(&json!("a string")).is_none());
        assert!(io.input(&json!([1])).is_none());
        assert!(io.input(&json!({})).is_none(), "no arguments is no input");
        let off = StepIo::off();
        assert!(off.input(&json!({"a": 1})).is_none());
        assert!(off.output("x", false).is_none());
        assert_eq!(
            format!("{off:?}"),
            "StepIo { on: false, redact: false, input_max: 4096, output_max: 8192 }"
        );
    }

    #[test]
    fn the_bounds_can_be_lowered_and_not_raised() {
        let io = StepIo::default().input_max(100).output_max(64);
        assert_eq!(io.input(&json!({"a": 1})).unwrap().1, 100);
        assert!(io.output(&"x".repeat(100), false).unwrap().text.len() <= 64);
        let io = StepIo::default()
            .input_max(usize::MAX)
            .output_max(usize::MAX);
        assert_eq!(io.input(&json!({"a": 1})).unwrap().1, STEP_INPUT_MAX_BYTES);
        let big = io
            .output(&"x".repeat(STEP_OUTPUT_MAX_BYTES + 1), false)
            .unwrap();
        assert!(big.text.len() <= STEP_OUTPUT_MAX_BYTES);
    }
}
