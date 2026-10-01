//! Steps: what an agent is doing, as a tree.
//!
//! A [`StepEvent`] says that something the run does (a tool call, a sub-agent's work, a command it
//! ran) started, moved, or ended, and **under which step it runs**. A2A serves them with the
//! `steps/v1` extension of the orchestration layer (`adam-a2a-runtime`), which keeps a client's
//! screen to one line per level and the orchestrator's log to a start, a few updates and an end per
//! step. The vocabulary here is that contract's: the same kinds, states and icons, the same bounds.
//!
//! Like every [`RunEvent`](crate::RunEvent) a step is best effort and not durable.

use serde::{Deserialize, Serialize};

/// The longest a step's id may be, in bytes: the contract's limit.
pub const MAX_STEP_ID_BYTES: usize = 128;

/// The most characters a step's label may hold: the contract's limit. A longer one is cut and ends
/// in `…`.
pub const MAX_STEP_LABEL_CHARS: usize = 200;

/// The most characters a step's detail may hold: the contract's limit. A longer one is cut and ends
/// in `…`.
pub const MAX_STEP_DETAIL_CHARS: usize = 1000;

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
}

impl StepIcon {
    /// Every icon, in the contract's order.
    pub const ALL: [StepIcon; 14] = [
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
                "web", "git", "test", "file", "tool"
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
