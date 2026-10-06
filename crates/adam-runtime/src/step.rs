//! Steps: what an agent is doing, as a tree.
//!
//! A [`StepEvent`] says that something the run does (a tool call, a sub-agent's work, a command it
//! ran) started, moved, or ended, and **under which step it runs**. A2A serves them with the
//! `steps/v1` extension of the orchestration layer (`adam-a2a-runtime`), which keeps a client's
//! screen to one line per level and the orchestrator's log to a start, a few updates and an end per
//! step. The vocabulary here is that contract's: the same kinds, states and icons, the same bounds.
//!
//! A tool call's step can also carry what the tool was given ([`StepEvent::input`]) and what it
//! answered ([`StepOutput`]), cut to the contract's bounds: see
//! [ADR 0011](https://github.com/vymalo/another-adam-rs/blob/main/docs/decisions/0011-a-tool-calls-step-carries-its-input-and-output.md).
//!
//! Like every [`RunEvent`](crate::RunEvent) a step is best effort and not durable.

use serde::{Deserialize, Serialize};
use serde_json::{Map, Value};

/// The longest a step's id may be, in bytes: the contract's limit.
pub const MAX_STEP_ID_BYTES: usize = 128;

/// The most characters a step's label may hold: the contract's limit. A longer one is cut and ends
/// in `…`.
pub const MAX_STEP_LABEL_CHARS: usize = 200;

/// The most characters a step's detail may hold: the contract's limit. A longer one is cut and ends
/// in `…`.
pub const MAX_STEP_DETAIL_CHARS: usize = 1000;

/// The most bytes a step's `input` may hold once serialized: the contract's limit. A larger one is
/// cut ([`StepEvent::with_input`]).
pub const STEP_INPUT_MAX_BYTES: usize = 4096;

/// The most characters one string inside a step's `input` may hold: the contract's limit. A longer one
/// is cut and ends in `…`.
pub const STEP_INPUT_STRING_MAX_CHARS: usize = 512;

/// The most bytes a step's `output.text` may hold: the contract's limit. A longer one is cut,
/// keeping its head and its tail ([`StepOutput::new`]).
pub const STEP_OUTPUT_MAX_BYTES: usize = 8192;

/// What kind of work a step is.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
#[non_exhaustive]
pub enum StepKind {
    /// An agent working for this one: the steps of its own nest under it.
    Subagent,
    /// A tool call.
    #[default]
    Tool,
    /// A command that was run.
    Command,
    /// Something said that deserves a line of its own.
    Message,
}

impl StepKind {
    /// The word on the wire (`subagent`, `tool`, `command`, `message`).
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Subagent => "subagent",
            Self::Tool => "tool",
            Self::Command => "command",
            Self::Message => "message",
        }
    }

    /// The kind a word names (`step = "subagent"` of `#[tool]`), if it is one.
    pub fn parse(word: &str) -> Option<Self> {
        Some(match word {
            "subagent" => Self::Subagent,
            "tool" => Self::Tool,
            "command" => Self::Command,
            "message" => Self::Message,
            _ => return None,
        })
    }
}

/// Where a step stands. The last three **end** it.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
#[non_exhaustive]
pub enum StepState {
    /// Working.
    Running,
    /// Waiting for a permission, a person, another step.
    Waiting,
    /// Done.
    Completed,
    /// Ended in failure.
    Failed,
    /// Stopped before it finished.
    Canceled,
}

impl StepState {
    /// The word on the wire.
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Running => "running",
            Self::Waiting => "waiting",
            Self::Completed => "completed",
            Self::Failed => "failed",
            Self::Canceled => "canceled",
        }
    }

    /// Whether the state ends the step (`completed`, `failed`, `canceled`).
    pub fn is_end(self) -> bool {
        matches!(self, Self::Completed | Self::Failed | Self::Canceled)
    }
}

/// The picture a step is drawn with: the contract's vocabulary, and nothing else (an icon the screen
/// does not know is ignored there, so a closed set is what an agent can mean).
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
#[non_exhaustive]
pub enum StepIcon {
    /// An agent.
    Agent,
    /// Reading a file.
    Read,
    /// Changing a file.
    Edit,
    /// Deleting.
    Delete,
    /// Moving or renaming.
    Move,
    /// Searching.
    Search,
    /// Running a command.
    Execute,
    /// Thinking, planning.
    Think,
    /// Fetching from somewhere.
    Fetch,
    /// The web.
    Web,
    /// Version control.
    Git,
    /// Tests.
    Test,
    /// A file.
    File,
    /// A generic tool.
    Tool,
    /// OpenCode, the coding agent reached over ACP.
    #[serde(rename = "opencode")]
    OpenCode,
}

impl StepIcon {
    /// Every icon, in the contract's order.
    pub const ALL: [StepIcon; 15] = [
        Self::Agent,
        Self::Read,
        Self::Edit,
        Self::Delete,
        Self::Move,
        Self::Search,
        Self::Execute,
        Self::Think,
        Self::Fetch,
        Self::Web,
        Self::Git,
        Self::Test,
        Self::File,
        Self::Tool,
        Self::OpenCode,
    ];

    /// The word on the wire.
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Agent => "agent",
            Self::Read => "read",
            Self::Edit => "edit",
            Self::Delete => "delete",
            Self::Move => "move",
            Self::Search => "search",
            Self::Execute => "execute",
            Self::Think => "think",
            Self::Fetch => "fetch",
            Self::Web => "web",
            Self::Git => "git",
            Self::Test => "test",
            Self::File => "file",
            Self::Tool => "tool",
            Self::OpenCode => "opencode",
        }
    }

    /// The icon a word names (`icon = "agent"` of `#[tool]`), if it is one.
    pub fn parse(word: &str) -> Option<Self> {
        Self::ALL.into_iter().find(|icon| icon.as_str() == word)
    }
}

/// One report about a step: it started, moved, or ended.
///
/// The first report of an `id` starts the step, later ones update it, and a report whose state
/// [`is_end`](StepState::is_end) ends it; a report after the end starts it again (a retry). Make the
/// event with [`StepEvent::new`] and the builder methods, which keep the bounds of the contract (an
/// id of at most [`MAX_STEP_ID_BYTES`] bytes, a one-line label of at most [`MAX_STEP_LABEL_CHARS`]
/// characters, a detail of at most [`MAX_STEP_DETAIL_CHARS`]).
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[non_exhaustive]
pub struct StepEvent {
    /// The step's id: unique within the run, stable across the reports about it. A tool call's own
    /// step is `tool:<call id>`.
    pub id: String,
    /// The step this one runs under, reported earlier. `None`: at the top, under the agent.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub parent: Option<String>,
    /// What kind of work it is.
    pub kind: StepKind,
    /// A plain one-line label.
    pub label: String,
    /// Where it stands.
    pub state: StepState,
    /// The picture to draw it with.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub icon: Option<StepIcon>,
    /// Plain text: a result, a failure, what it is doing now.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub detail: Option<String>,
    /// What the call was given: the arguments of a tool call, as a JSON object, on the report that
    /// starts the step. Cut to [`STEP_INPUT_MAX_BYTES`] and made printable by
    /// [`with_input`](Self::with_input); the caller redacts what it knows to be secret first.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub input: Option<Map<String, Value>>,
    /// What the call answered (or the error it ended with), on the report that ends the step.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub output: Option<StepOutput>,
}

/// What a step's call answered, bounded by the contract (`steps/v1`'s `output`).
///
/// `text` is at most [`STEP_OUTPUT_MAX_BYTES`] bytes. When the answer was longer, `truncated` is set
/// and `bytes` is its original size: `text` then keeps the start and the end of the answer (an error
/// is usually at the end) with a line between them that says how many bytes are not kept.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[non_exhaustive]
pub struct StepOutput {
    /// The answer as text, or, when `error` is set, the error the call ended with.
    pub text: String,
    /// The answer was cut: `text` is not all of it.
    #[serde(default, skip_serializing_if = "is_false")]
    pub truncated: bool,
    /// The size of the whole answer in bytes, when it was cut.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub bytes: Option<u64>,
    /// The call failed, and `text` is its error.
    #[serde(default, skip_serializing_if = "is_false")]
    pub error: bool,
}

#[allow(clippy::trivially_copy_pass_by_ref)] // serde's `skip_serializing_if` passes a reference
fn is_false(flag: &bool) -> bool {
    !*flag
}

impl StepOutput {
    /// `text` as an output of at most [`STEP_OUTPUT_MAX_BYTES`] bytes. `error`: the call failed and
    /// `text` is its error.
    pub fn new(text: impl AsRef<str>, error: bool) -> Self {
        Self::within(text, error, STEP_OUTPUT_MAX_BYTES)
    }

    /// As [`new`](Self::new), with a bound of `max` bytes (at most [`STEP_OUTPUT_MAX_BYTES`]: the
    /// contract's limit is never exceeded).
    ///
    /// Control characters other than the line break and the tab are dropped. A text longer than the
    /// bound keeps its head (three quarters of what the bound leaves) and its tail, with
    /// `… n bytes not kept …` on a line of its own between them, so that the whole is within `max`.
    pub fn within(text: impl AsRef<str>, error: bool, max: usize) -> Self {
        let max = max.min(STEP_OUTPUT_MAX_BYTES);
        let clean = printable(text.as_ref());
        let size = clean.len();
        if size <= max {
            return Self {
                text: clean,
                truncated: false,
                bytes: None,
                error,
            };
        }
        Self {
            text: head_and_tail(&clean, max),
            truncated: true,
            bytes: Some(size as u64),
            error,
        }
    }
}

impl StepEvent {
    /// A report about the step `id`: its kind, label and state. The id is cut to
    /// [`MAX_STEP_ID_BYTES`] bytes (control characters become `_`), and the label to one line of at
    /// most [`MAX_STEP_LABEL_CHARS`] characters.
    pub fn new(
        id: impl Into<String>,
        kind: StepKind,
        label: impl AsRef<str>,
        state: StepState,
    ) -> Self {
        Self {
            id: clean_id(&id.into()),
            parent: None,
            kind,
            label: one_line(label.as_ref(), MAX_STEP_LABEL_CHARS),
            state,
            icon: None,
            detail: None,
            input: None,
            output: None,
        }
    }

    /// Run under the step `parent`, which was reported earlier.
    #[must_use]
    pub fn under(mut self, parent: impl Into<String>) -> Self {
        self.parent = Some(clean_id(&parent.into()));
        self
    }

    /// Draw it with `icon`.
    #[must_use]
    pub fn with_icon(mut self, icon: StepIcon) -> Self {
        self.icon = Some(icon);
        self
    }

    /// Say more: a result, a failure, what it is doing now. Cut to [`MAX_STEP_DETAIL_CHARS`]
    /// characters (ending in `…`); line breaks are kept.
    #[must_use]
    pub fn with_detail(mut self, detail: impl AsRef<str>) -> Self {
        self.detail = Some(cut(detail.as_ref(), MAX_STEP_DETAIL_CHARS));
        self
    }

    /// Say what the call was given: `input`, bounded as the contract wants
    /// ([`STEP_INPUT_MAX_BYTES`]): control characters other than the line break and the tab are dropped,
    /// a string longer than [`STEP_INPUT_STRING_MAX_CHARS`] characters is cut (ending in `…`), and an input
    /// that is still larger once serialized is replaced by `{"_cut": true, "bytes": <its size>}`.
    ///
    /// Redact what is secret **before** this: a cut can leave the front half of a value that a redactor
    /// would no longer recognise.
    #[must_use]
    pub fn with_input(self, input: Map<String, Value>) -> Self {
        self.with_input_within(input, STEP_INPUT_MAX_BYTES)
    }

    /// As [`with_input`](Self::with_input), with a bound of `max` bytes (at most
    /// [`STEP_INPUT_MAX_BYTES`]).
    #[must_use]
    pub fn with_input_within(mut self, input: Map<String, Value>, max: usize) -> Self {
        self.input = Some(bound_input(input, max.min(STEP_INPUT_MAX_BYTES)));
        self
    }

    /// Say what the call answered: see [`StepOutput`].
    #[must_use]
    pub fn with_output(mut self, output: StepOutput) -> Self {
        self.output = Some(output);
        self
    }

    /// The same step in another state.
    #[must_use]
    pub fn in_state(mut self, state: StepState) -> Self {
        self.state = state;
        self
    }
}

/// `id` as the contract wants it: printable, at most [`MAX_STEP_ID_BYTES`] bytes.
fn clean_id(id: &str) -> String {
    let mut out = String::with_capacity(id.len().min(MAX_STEP_ID_BYTES));
    for c in id.chars() {
        let c = if c.is_control() { '_' } else { c };
        if out.len() + c.len_utf8() > MAX_STEP_ID_BYTES {
            break;
        }
        out.push(c);
    }
    out
}

/// `text` without the control characters other than the line break and the tab.
fn printable(text: &str) -> String {
    text.chars()
        .filter(|c| !c.is_control() || matches!(c, '\n' | '\t'))
        .collect()
}

/// `input` made printable and cut: see [`StepEvent::with_input`].
fn bound_input(input: Map<String, Value>, max: usize) -> Map<String, Value> {
    let original = serialized_len(&input);
    let bounded: Map<String, Value> = input
        .into_iter()
        .map(|(key, value)| (printable(&key), bound_value(value)))
        .collect();
    if serialized_len(&bounded) <= max {
        return bounded;
    }
    let mut cut = Map::new();
    cut.insert("_cut".into(), Value::Bool(true));
    cut.insert("bytes".into(), Value::from(original as u64));
    cut
}

/// `value` with every string (inside arrays and objects too) made printable and cut to
/// [`STEP_INPUT_STRING_MAX_CHARS`] characters.
fn bound_value(value: Value) -> Value {
    match value {
        Value::String(text) => Value::String(cut(&printable(&text), STEP_INPUT_STRING_MAX_CHARS)),
        Value::Array(items) => Value::Array(items.into_iter().map(bound_value).collect()),
        Value::Object(map) => Value::Object(
            map.into_iter()
                .map(|(key, value)| (printable(&key), bound_value(value)))
                .collect(),
        ),
        other => other,
    }
}

fn serialized_len(map: &Map<String, Value>) -> usize {
    // Serializing a `Map` cannot fail: its keys are strings.
    serde_json::to_string(map).map_or(usize::MAX, |json| json.len())
}

/// The head and the tail of `text` (longer than `max`) with a line that says how many bytes are left
/// out between them, the whole within `max` bytes. The head gets three quarters of what the line
/// leaves, the tail the rest, as the contract keeps 6 KiB and 2 KiB of 8 KiB.
fn head_and_tail(text: &str, max: usize) -> String {
    let size = text.len();
    // The line at its widest: the number of bytes left out is below the size of the text.
    let widest = marker(size).len();
    if max < 4 * widest {
        // Too little room to say what was left out: the start alone.
        let mut end = max;
        while !text.is_char_boundary(end) {
            end -= 1;
        }
        return text[..end].to_owned();
    }
    let room = max - widest;
    let mut head = room - room / 4;
    while !text.is_char_boundary(head) {
        head -= 1;
    }
    let mut tail = size - (room / 4);
    while !text.is_char_boundary(tail) {
        tail += 1;
    }
    let mut out = String::with_capacity(max);
    out.push_str(&text[..head]);
    out.push_str(&marker(tail - head));
    out.push_str(&text[tail..]);
    out
}

/// The line between a head and a tail: how many bytes are not kept.
fn marker(left_out: usize) -> String {
    format!("\n… {left_out} bytes not kept …\n")
}

/// `text` with every run of whitespace (line breaks included) as one space, cut to `max` characters.
fn one_line(text: &str, max: usize) -> String {
    let joined = text.split_whitespace().collect::<Vec<_>>().join(" ");
    cut(&joined, max)
}

/// `text` cut to `max` characters, the last of them `…` when something was cut.
fn cut(text: &str, max: usize) -> String {
    if text.chars().count() <= max {
        return text.to_owned();
    }
    let mut out: String = text.chars().take(max.saturating_sub(1)).collect();
    out.push('…');
    out
}

#[cfg(test)]
mod tests {
    use serde_json::json;

    use super::*;

    #[test]
    fn the_words_are_the_contracts() {
        let icons: Vec<&str> = StepIcon::ALL.iter().map(|i| i.as_str()).collect();
        assert_eq!(
            icons,
            [
                "agent", "read", "edit", "delete", "move", "search", "execute", "think", "fetch",
                "web", "git", "test", "file", "tool", "opencode"
            ]
        );
        for icon in StepIcon::ALL {
            assert_eq!(StepIcon::parse(icon.as_str()), Some(icon));
            assert_eq!(
                serde_json::to_value(icon).unwrap(),
                json!(icon.as_str()),
                "serde and as_str agree"
            );
        }
        assert_eq!(StepIcon::parse("rocket"), None);
        assert_eq!(StepIcon::parse("Agent"), None);
        for kind in [
            StepKind::Subagent,
            StepKind::Tool,
            StepKind::Command,
            StepKind::Message,
        ] {
            assert_eq!(StepKind::parse(kind.as_str()), Some(kind));
            assert_eq!(serde_json::to_value(kind).unwrap(), json!(kind.as_str()));
        }
        assert_eq!(StepKind::parse("thing"), None);
        assert_eq!(StepKind::default(), StepKind::Tool);
        let ends: Vec<bool> = [
            StepState::Running,
            StepState::Waiting,
            StepState::Completed,
            StepState::Failed,
            StepState::Canceled,
        ]
        .iter()
        .map(|s| {
            assert_eq!(serde_json::to_value(s).unwrap(), json!(s.as_str()));
            s.is_end()
        })
        .collect();
        assert_eq!(ends, [false, false, true, true, true]);
    }

    #[test]
    fn a_step_is_made_within_the_contracts_bounds() {
        let step = StepEvent::new(
            format!("tool:{}\n", "x".repeat(300)),
            StepKind::Command,
            format!("  npm\ttest \n --watch {}", "é".repeat(300)),
            StepState::Running,
        )
        .under("p".repeat(300))
        .with_detail(format!("1 failed\n{}", "y".repeat(2000)));
        assert!(step.id.len() <= MAX_STEP_ID_BYTES && step.id.starts_with("tool:xxx"));
        assert!(!step.id.contains('\n'));
        assert_eq!(step.parent.as_ref().unwrap().len(), MAX_STEP_ID_BYTES);
        assert!(step.label.starts_with("npm test --watch éé"));
        assert!(!step.label.contains(['\n', '\t']));
        assert_eq!(step.label.chars().count(), MAX_STEP_LABEL_CHARS);
        assert!(step.label.ends_with('…'));
        let detail = step.detail.unwrap();
        assert!(detail.starts_with("1 failed\nyyy"), "line breaks are kept");
        assert_eq!(detail.chars().count(), MAX_STEP_DETAIL_CHARS);
        assert!(detail.ends_with('…'));
    }

    #[test]
    fn an_id_is_cut_on_a_character_boundary() {
        let step = StepEvent::new("é".repeat(100), StepKind::Tool, "t", StepState::Running);
        assert_eq!(step.id.len(), MAX_STEP_ID_BYTES);
        assert!(step.id.chars().all(|c| c == 'é'));
        // Short text is untouched.
        let step = StepEvent::new(
            "tool:c1",
            StepKind::Tool,
            "run_checks",
            StepState::Completed,
        );
        assert_eq!(
            (step.id.as_str(), step.label.as_str()),
            ("tool:c1", "run_checks")
        );
        assert_eq!(step.parent, None);
        assert_eq!(step.detail, None);
    }

    fn object(value: Value) -> Map<String, Value> {
        let Value::Object(map) = value else {
            panic!("not an object")
        };
        map
    }

    #[test]
    fn an_input_within_the_bounds_is_kept_as_it_is() {
        let step = StepEvent::new("tool:c1", StepKind::Tool, "t", StepState::Running).with_input(
            object(json!({"query": "rust", "n": 3, "deep": {"a": ["b", null]}})),
        );
        assert_eq!(
            serde_json::to_value(&step).unwrap()["input"],
            json!({"query": "rust", "n": 3, "deep": {"a": ["b", null]}})
        );
    }

    #[test]
    fn a_long_string_of_an_input_is_cut_and_control_characters_are_dropped() {
        let long = "é".repeat(600);
        let step =
            StepEvent::new("tool:c1", StepKind::Tool, "t", StepState::Running).with_input(object(
                json!({"text": long, "nested": ["x".repeat(513)], "k\u{7}ey": "a\u{0}b\tc\nd\re"}),
            ));
        let input = step.input.unwrap();
        let text = input["text"].as_str().unwrap();
        assert_eq!(text.chars().count(), STEP_INPUT_STRING_MAX_CHARS);
        assert!(text.ends_with('…') && text.starts_with("éé"));
        assert_eq!(
            input["nested"][0].as_str().unwrap().chars().count(),
            STEP_INPUT_STRING_MAX_CHARS
        );
        assert_eq!(
            input["key"],
            json!("ab\tc\nde"),
            "key and value are printable"
        );
        // A string of exactly the limit is not cut.
        let exact = "y".repeat(STEP_INPUT_STRING_MAX_CHARS);
        let step = StepEvent::new("tool:c1", StepKind::Tool, "t", StepState::Running)
            .with_input(object(json!({"s": exact.clone()})));
        assert_eq!(step.input.unwrap()["s"], json!(exact));
    }

    #[test]
    fn an_input_still_too_large_is_replaced_by_its_size() {
        // Ten strings of 500 characters: under the string limit, over 4096 bytes together.
        let big: Map<String, Value> = (0..10)
            .map(|n| (format!("k{n}"), json!("z".repeat(500))))
            .collect();
        let original = serde_json::to_string(&big).unwrap().len();
        assert!(original > STEP_INPUT_MAX_BYTES);
        let step =
            StepEvent::new("tool:c1", StepKind::Tool, "t", StepState::Running).with_input(big);
        assert_eq!(
            serde_json::to_value(step.input.unwrap()).unwrap(),
            json!({"_cut": true, "bytes": original})
        );
        // A bound given by the caller is never above the contract's.
        let one = object(json!({"s": "z".repeat(500)}));
        let step = StepEvent::new("tool:c1", StepKind::Tool, "t", StepState::Running)
            .with_input_within(one.clone(), 100);
        assert_eq!(step.input.as_ref().unwrap()["_cut"], json!(true));
        let step = StepEvent::new("tool:c1", StepKind::Tool, "t", StepState::Running)
            .with_input_within(one, 1_000_000);
        assert_eq!(step.input.unwrap()["s"], json!("z".repeat(500)));
    }

    #[test]
    fn an_output_within_the_bound_is_whole() {
        let output = StepOutput::new("12 passed\nall green", false);
        assert_eq!(output.text, "12 passed\nall green");
        assert!(!output.truncated && output.bytes.is_none() && !output.error);
        assert_eq!(
            serde_json::to_value(&output).unwrap(),
            json!({"text": "12 passed\nall green"}),
            "no member for what is not so"
        );
        let failed = StepOutput::new("no such file", true);
        assert_eq!(
            serde_json::to_value(&failed).unwrap(),
            json!({"text": "no such file", "error": true})
        );
        // Exactly the bound is whole.
        let exact = StepOutput::new("a".repeat(STEP_OUTPUT_MAX_BYTES), false);
        assert!(!exact.truncated);
        assert_eq!(exact.text.len(), STEP_OUTPUT_MAX_BYTES);
    }

    #[test]
    fn an_output_over_the_bound_keeps_its_head_and_its_tail() {
        let text = format!(
            "HEAD{}TAIL: failed at the end",
            "m".repeat(STEP_OUTPUT_MAX_BYTES * 3)
        );
        let size = text.len();
        let output = StepOutput::new(&text, true);
        assert!(output.truncated && output.error);
        assert_eq!(output.bytes, Some(size as u64));
        assert!(
            output.text.len() <= STEP_OUTPUT_MAX_BYTES,
            "{}",
            output.text.len()
        );
        assert!(output.text.starts_with("HEAD"));
        assert!(output.text.ends_with("TAIL: failed at the end"));
        // The head is about three quarters, and the line says what was left out.
        let marker_at = output.text.find("\n… ").expect("the marker");
        assert!(marker_at > STEP_OUTPUT_MAX_BYTES / 2, "{marker_at}");
        let says: usize = output.text[marker_at + "\n… ".len()..]
            .split(' ')
            .next()
            .unwrap()
            .parse()
            .unwrap();
        let line = format!("\n… {says} bytes not kept …\n");
        assert!(output.text.contains(&line));
        assert_eq!(
            output.text.len() - line.len() + says,
            size,
            "head + tail + what the line says = the whole"
        );
    }

    #[test]
    fn an_output_is_cut_on_character_boundaries_and_printable() {
        let text = format!("\u{1b}[31m{}\u{7}", "é€😀".repeat(STEP_OUTPUT_MAX_BYTES));
        let output = StepOutput::new(&text, false);
        assert!(output.truncated);
        assert!(output.text.len() <= STEP_OUTPUT_MAX_BYTES);
        assert!(
            !output.text.contains(['\u{1b}', '\u{7}']),
            "controls dropped"
        );
        assert!(output.text.starts_with("[31mé€😀"));
        // The size is of the printable text, which is what was cut.
        assert!(output.bytes.unwrap() < text.len() as u64);
        // Line breaks and tabs stay; a carriage return goes.
        assert_eq!(StepOutput::new("a\r\nb\tc", false).text, "a\nb\tc");
    }

    #[test]
    fn a_small_bound_keeps_the_start_alone_and_a_big_one_is_the_contracts() {
        let output = StepOutput::within("a".repeat(500), false, 40);
        assert!(output.truncated);
        assert_eq!(output.text, "a".repeat(40));
        let output = StepOutput::within("a".repeat(STEP_OUTPUT_MAX_BYTES + 1), false, usize::MAX);
        assert!(output.truncated && output.text.len() <= STEP_OUTPUT_MAX_BYTES);
        // An empty output is still an output.
        let empty = StepOutput::new("", false);
        assert_eq!(serde_json::to_value(&empty).unwrap(), json!({"text": ""}));
    }

    #[test]
    fn no_output_exceeds_the_bound_whatever_its_size() {
        for size in [8191, 8192, 8193, 9_000, 20_000, 1_000_000] {
            for text in [
                "x".repeat(size),
                "é".repeat(size / 2 + 1),
                "😀".repeat(size / 4 + 1),
            ] {
                let output = StepOutput::new(&text, false);
                assert!(
                    output.text.len() <= STEP_OUTPUT_MAX_BYTES,
                    "{size}: {}",
                    output.text.len()
                );
                assert_eq!(output.truncated, text.len() > STEP_OUTPUT_MAX_BYTES);
            }
        }
    }

    #[test]
    fn input_and_output_round_trip_and_stay_out_of_a_step_that_has_none() {
        let full = StepEvent::new("tool:c1", StepKind::Tool, "Search", StepState::Failed)
            .with_input(object(json!({"q": "x"})))
            .with_output(StepOutput::new("boom", true));
        let json = serde_json::to_value(&full).unwrap();
        assert_eq!(json["input"], json!({"q": "x"}));
        assert_eq!(json["output"], json!({"text": "boom", "error": true}));
        assert_eq!(serde_json::from_value::<StepEvent>(json).unwrap(), full);
        // A step written before the members existed decodes without them.
        let old: StepEvent = serde_json::from_value(
            json!({"id": "tool:c1", "kind": "tool", "label": "t", "state": "running"}),
        )
        .unwrap();
        assert_eq!((old.input, old.output), (None, None));
    }

    #[test]
    fn a_step_round_trips_and_leaves_out_what_it_does_not_say() {
        let bare = StepEvent::new("tool:c1", StepKind::Tool, "run_checks", StepState::Running);
        assert_eq!(
            serde_json::to_value(&bare).unwrap(),
            json!({"id": "tool:c1", "kind": "tool", "label": "run_checks", "state": "running"})
        );
        let full = StepEvent::new("acp:c2:1", StepKind::Command, "npm test", StepState::Failed)
            .under("tool:c2")
            .with_icon(StepIcon::Execute)
            .with_detail("1 failed");
        let json = serde_json::to_value(&full).unwrap();
        assert_eq!(
            json,
            json!({"id": "acp:c2:1", "parent": "tool:c2", "kind": "command", "label": "npm test",
                   "state": "failed", "icon": "execute", "detail": "1 failed"})
        );
        assert_eq!(serde_json::from_value::<StepEvent>(json).unwrap(), full);
        assert_eq!(
            full.clone().in_state(StepState::Running).state,
            StepState::Running
        );
    }
}
