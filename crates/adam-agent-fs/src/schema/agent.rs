//! The frontmatter of an agent file: the root `instructions.md`, a directory subagent's
//! `instructions.md` and a flat subagent (`subagents/x.md`, `x.agent.md`).
//!
//! One schema for all three, and a superset of the Claude Code and GitHub Copilot custom-agent
//! formats, so a file copied from `.claude/agents/x.md` or `.github/agents/x.agent.md` reads
//! unchanged. Keys adam does not model land in [`AgentFrontmatter::extra`].

use std::collections::BTreeMap;
use std::fmt;

use serde::de::{self, Deserializer, Visitor};
use serde::{Deserialize, Serialize, Serializer};
use serde_json::Value;

use super::reference::ReferenceDef;
use super::scalar::{StringOrSeq, lenient_map, scalar_map};

/// Which tools an agent gets.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ToolList {
    /// `*` (or `["*"]`): every tool registered for the agent.
    All,
    /// Exactly these names. An empty list means no tools.
    Named(Vec<String>),
}

impl Serialize for ToolList {
    /// `"*"` or the list, the spellings [`Deserialize`] reads back.
    fn serialize<S: Serializer>(&self, s: S) -> Result<S::Ok, S::Error> {
        match self {
            Self::All => s.serialize_str("*"),
            Self::Named(names) => names.serialize(s),
        }
    }
}

impl<'de> Deserialize<'de> for ToolList {
    /// Both spellings of Claude Code and Copilot: `tools: a, b` and `tools: [a, b]`.
    fn deserialize<D: Deserializer<'de>>(d: D) -> Result<Self, D::Error> {
        let names = StringOrSeq::deserialize(d)?.into_comma_list();
        Ok(if names.iter().any(|n| n == "*") {
            Self::All
        } else {
            Self::Named(names)
        })
    }
}

/// Which skills an agent's catalog offers.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum SkillSelection {
    /// `all`: every skill under the agent's own `skills/`.
    All,
    /// Only these.
    Named(Vec<String>),
}

impl Serialize for SkillSelection {
    /// `"all"` or the list, the spellings [`Deserialize`] reads back.
    fn serialize<S: Serializer>(&self, s: S) -> Result<S::Ok, S::Error> {
        match self {
            Self::All => s.serialize_str("all"),
            Self::Named(names) => names.serialize(s),
        }
    }
}

impl<'de> Deserialize<'de> for SkillSelection {
    fn deserialize<D: Deserializer<'de>>(d: D) -> Result<Self, D::Error> {
        Ok(match StringOrSeq::deserialize(d)? {
            StringOrSeq::Text(t) if t.trim() == "all" => Self::All,
            other => Self::Named(other.into_comma_list()),
        })
    }
}

/// The model an agent uses.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ModelRef {
    /// `inherit`: the parent's model (the default for a subagent).
    Inherit,
    /// A gateway alias. Never a key or an endpoint: those come from the environment.
    Alias(String),
}

impl Serialize for ModelRef {
    /// `"inherit"` or the alias, the spellings [`Deserialize`] reads back.
    fn serialize<S: Serializer>(&self, s: S) -> Result<S::Ok, S::Error> {
        match self {
            Self::Inherit => s.serialize_str("inherit"),
            Self::Alias(alias) => s.serialize_str(alias),
        }
    }
}

impl<'de> Deserialize<'de> for ModelRef {
    fn deserialize<D: Deserializer<'de>>(d: D) -> Result<Self, D::Error> {
        struct V;
        impl Visitor<'_> for V {
            type Value = ModelRef;
            fn expecting(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
                f.write_str("a model alias, or `inherit`")
            }
            fn visit_str<E: de::Error>(self, v: &str) -> Result<ModelRef, E> {
                let v = v.trim();
                if v.is_empty() {
                    return Err(E::custom("the model alias is empty"));
                }
                Ok(if v == "inherit" {
                    ModelRef::Inherit
                } else {
                    ModelRef::Alias(v.to_owned())
                })
            }
        }
        d.deserialize_str(V)
    }
}

/// The largest `limits.max_minutes`: 24 hours.
pub const MAX_MINUTES_LIMIT: u32 = 1_440;

/// The most `limits.max_parallel` may say: the bound of `MAX_PARALLEL_TOOL_CALLS`.
pub const MAX_PARALLEL_LIMIT: u32 = 32;

/// The most `limits.max_processes` may say: the bound of `MAX_PROCESSES`.
pub const MAX_PROCESSES_LIMIT: u32 = 256;

/// The largest `limits.max_open_sessions`: the page of one session listing.
pub const MAX_OPEN_SESSIONS_LIMIT: u32 = 100;

/// The largest `limits.max_working_sessions`.
pub const MAX_WORKING_SESSIONS_LIMIT: u32 = 32;

/// The smallest and largest `limits.max_session_rounds`.
pub const MAX_SESSION_ROUNDS_LIMIT: (u32, u32) = (2, 200);

/// The largest `limits.max_inline_upload_bytes`: 256 MiB, the orchestrator's ceiling for one file,
/// `artifacts.maxFileBytes` (`sys:docs/api/config.md`, verified 2026-11-25). The same number as
/// `adam_a2a::UPLOADS_INLINE_MAX_BYTES_CEILING`, which this crate does not depend on and so cannot
/// name.
pub const MAX_INLINE_UPLOAD_BYTES_LIMIT: u64 = 268_435_456;

/// The most `retry.<tool>.attempts` may say (ADR 0061, decision 6).
pub const MAX_RETRY_ATTEMPTS: u32 = 20;

/// The longest `retry.<tool>.max_backoff` may say: one hour.
pub const MAX_RETRY_BACKOFF: std::time::Duration = std::time::Duration::from_secs(3_600);

/// The first delay of a `retry` that names no `backoff`: `adam_runtime::RetryPolicy`'s default.
pub const DEFAULT_RETRY_BACKOFF: std::time::Duration = std::time::Duration::from_secs(1);

/// The bound of a `retry` that names no `max_backoff`: `adam_runtime::RetryPolicy`'s default.
pub const DEFAULT_RETRY_MAX_BACKOFF: std::time::Duration = std::time::Duration::from_secs(60);

/// The classes `retry.<tool>.on` may name: `adam_runtime::RetryClasses`' names (`adam-assembly`'s
/// tests check that they are the same).
pub const RETRY_CLASS_NAMES: [&str; 3] = ["transient", "rate-limited", "conflict"];

/// The retry policy of one tool's failures (`retry:` in the frontmatter, ADR 0061): its failures
/// count under `tool:<name>` and are retried under this, not the runtime's policy. Absent fields
/// keep `adam_runtime::RetryPolicy`'s default (1 s, 60 s, 2.0, every class); `attempts` is
/// required. The loader checks the shape and the bounds ([`MAX_RETRY_ATTEMPTS`],
/// [`MAX_RETRY_BACKOFF`]); binding checks the name against the agent's tools.
#[derive(Debug, Clone, Default, PartialEq, Eq, Deserialize, Serialize)]
#[serde(default)]
pub struct RetryDef {
    /// The tries in all, the first included: 1 to [`MAX_RETRY_ATTEMPTS`].
    #[serde(skip_serializing_if = "Option::is_none")]
    pub attempts: Option<u32>,
    /// The delay after the first failure: `500ms`, `1s`, `30m`, `1h` ([`retry_duration`]). A bare
    /// number is kept as its text, so the loader says it lacks a unit.
    #[serde(
        skip_serializing_if = "Option::is_none",
        deserialize_with = "text_or_number"
    )]
    pub backoff: Option<String>,
    /// The bound of any delay, at most [`MAX_RETRY_BACKOFF`] and not under `backoff`. Absent, it is
    /// 60 s, or `backoff` when that is longer (and then the delay does not grow).
    #[serde(
        skip_serializing_if = "Option::is_none",
        alias = "maxBackoff",
        alias = "max-backoff",
        deserialize_with = "text_or_number"
    )]
    pub max_backoff: Option<String>,
    /// The growth factor per further failure, a positive number.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub multiplier: Option<serde_json::Number>,
    /// The classes it retries, of [`RETRY_CLASS_NAMES`]: a list, or one name or a comma-separated
    /// string, as `tools:` takes; absent: all of them.
    #[serde(skip_serializing_if = "Option::is_none", deserialize_with = "names")]
    pub on: Option<Vec<String>>,
    /// Keys this schema does not know.
    #[serde(flatten)]
    pub extra: BTreeMap<String, Value>,
}

impl RetryDef {
    /// The first delay: `backoff`, else [`DEFAULT_RETRY_BACKOFF`]; `None` when `backoff` is not a
    /// duration.
    pub fn initial_backoff(&self) -> Option<std::time::Duration> {
        self.backoff
            .as_deref()
            .map_or(Some(DEFAULT_RETRY_BACKOFF), retry_duration)
    }

    /// The bound of any delay: `max_backoff`, else [`DEFAULT_RETRY_MAX_BACKOFF`] (raised to the
    /// first delay when that is longer); `None` when `max_backoff` is not a duration.
    pub fn bound(&self) -> Option<std::time::Duration> {
        match self.max_backoff.as_deref() {
            Some(text) => retry_duration(text),
            None => Some(
                DEFAULT_RETRY_MAX_BACKOFF
                    .max(self.initial_backoff().unwrap_or(DEFAULT_RETRY_BACKOFF)),
            ),
        }
    }

    /// The growth factor, when it is given and is a finite number.
    pub fn factor(&self) -> Option<f64> {
        self.multiplier
            .as_ref()
            .and_then(serde_json::Number::as_f64)
            .filter(|m| m.is_finite())
    }
}

/// A string, or a number kept as its text.
fn text_or_number<'de, D: serde::Deserializer<'de>>(d: D) -> Result<Option<String>, D::Error> {
    #[derive(Deserialize)]
    #[serde(untagged)]
    enum Text {
        Text(String),
        Number(serde_json::Number),
    }
    Ok(Option::<Text>::deserialize(d)?.map(|t| match t {
        Text::Text(text) => text,
        Text::Number(n) => n.to_string(),
    }))
}

/// A list of names, or one string of names separated by commas.
fn names<'de, D: serde::Deserializer<'de>>(d: D) -> Result<Option<Vec<String>>, D::Error> {
    #[derive(Deserialize)]
    #[serde(untagged)]
    enum Names {
        One(String),
        Many(Vec<String>),
    }
    Ok(Option::<Names>::deserialize(d)?.map(|n| match n {
        Names::One(text) => text
            .split(',')
            .map(str::trim)
            .filter(|s| !s.is_empty())
            .map(str::to_owned)
            .collect(),
        Names::Many(list) => list,
    }))
}

/// Whether `name` can name a tool in `retry:`: what a model is shown, `A-Z`, `a-z`, `0-9`, `_` and
/// `-`, at most 64 characters. A registered tool's name, an MCP server's `<server>__<tool>` and a
/// subagent's all are; binding checks it is one of the agent's.
pub fn is_retry_tool_name(name: &str) -> bool {
    (1..=64).contains(&name.len())
        && name
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || c == '_' || c == '-')
}

/// A retry delay: a whole number and one unit, `ms`, `s`, `m` or `h` (`500ms`, `1s`, `30m`,
/// `1h`); `None` for anything else.
pub fn retry_duration(text: &str) -> Option<std::time::Duration> {
    let text = text.trim();
    let split = text.find(|c: char| !c.is_ascii_digit())?;
    let (number, unit) = text.split_at(split);
    let number: u64 = number.parse().ok()?;
    let ms = match unit {
        "ms" => 1,
        "s" => 1_000,
        "m" => 60_000,
        "h" => 3_600_000,
        _ => return None,
    };
    number.checked_mul(ms).map(std::time::Duration::from_millis)
}

/// The limits of the agent loop (`adam_llm_agent::Limits`). Absent fields keep the loop's
/// default. Claude Code's top-level `maxTurns` is read as `max_turns`.
#[derive(Debug, Clone, Default, PartialEq, Eq, Deserialize, Serialize)]
#[serde(default)]
pub struct Limits {
    /// Most model calls in one run.
    #[serde(skip_serializing_if = "Option::is_none", alias = "maxTurns")]
    pub max_turns: Option<u32>,
    /// Most tool calls in one run.
    #[serde(skip_serializing_if = "Option::is_none", alias = "maxToolCalls")]
    pub max_tool_calls: Option<u32>,
    /// `max_output_tokens` passed to the model: the cap this agent asks for, at least 1 (ADR
    /// 0064; `0` is refused). Absent: the model's profile's, then the 128,000 fallback.
    #[serde(skip_serializing_if = "Option::is_none", alias = "maxOutputTokens")]
    pub max_output_tokens: Option<u32>,
    /// Budget for the history sent to the model, in tokens: the run compacts at the smaller of
    /// this and what the model's window leaves; `0` leaves the window's alone. Absent: the loop's
    /// default, 200,000 (`adam_llm_agent::DEFAULT_MAX_HISTORY_TOKENS`, ADR 0086).
    #[serde(skip_serializing_if = "Option::is_none", alias = "maxHistoryTokens")]
    pub max_history_tokens: Option<u32>,
    /// The longest a run of this agent may take **as a subagent**, in minutes; `0` is no limit, at
    /// most [`MAX_MINUTES_LIMIT`]. Absent: 30. Only a subagent has it: on a root agent it is ignored
    /// with a warning.
    #[serde(skip_serializing_if = "Option::is_none", alias = "maxMinutes")]
    pub max_minutes: Option<u32>,
    /// The most tool calls of one model turn that run at the same time, 1 to [`MAX_PARALLEL_LIMIT`]
    /// (`1` is one at a time). It only **lowers** the deployment's `MAX_PARALLEL_TOOL_CALLS`: a
    /// larger value is lowered to the deployment's with a warning at startup. Absent: the
    /// deployment's value. Not a field of `adam_llm_agent::Limits` (ADR 0049).
    #[serde(skip_serializing_if = "Option::is_none", alias = "maxParallel")]
    pub max_parallel: Option<u32>,
    /// The most processes (commands) of the whole process at once, 1 to [`MAX_PROCESSES_LIMIT`]. It
    /// only **lowers** the deployment's `MAX_PROCESSES` (a larger value is the deployment's). Read by
    /// the coder; ADR 0065.
    #[serde(skip_serializing_if = "Option::is_none", alias = "maxProcesses")]
    pub max_processes: Option<u32>,
    /// The memory one command may use, as a size (`2GB`, `512MiB`, or bytes), at least 64 MiB. Only
    /// **lowers** `COMMAND_MEMORY_MAX`.
    #[serde(skip_serializing_if = "Option::is_none", alias = "commandMemory")]
    pub command_memory: Option<String>,
    /// The CPU one command may use: cores (`2`, `1.5`) or millicores (`500m`), at least `100m`. Only
    /// **lowers** `COMMAND_CPU_MAX`.
    #[serde(skip_serializing_if = "Option::is_none", alias = "commandCpu")]
    pub command_cpu: Option<String>,
    /// The most sessions of a conversation open at once (ADR 0045), 1 to
    /// [`MAX_OPEN_SESSIONS_LIMIT`]. **Root agent only**, and it only lowers the deployment's
    /// `SESSION_MAX_OPEN` (a larger value is lowered with a warning at startup); ignored on a
    /// subagent with a warning. Absent: the deployment's value.
    #[serde(skip_serializing_if = "Option::is_none", alias = "maxOpenSessions")]
    pub max_open_sessions: Option<u32>,
    /// The most rounds of a conversation working at once, 1 to [`MAX_WORKING_SESSIONS_LIMIT`] and
    /// at most `max_open_sessions` when both are given. Root agent only; lowers
    /// `SESSION_MAX_WORKING`.
    #[serde(skip_serializing_if = "Option::is_none", alias = "maxWorkingSessions")]
    pub max_working_sessions: Option<u32>,
    /// The most rounds one session may have, a close included, 2 to 200. Root agent only; lowers
    /// `SESSION_MAX_ROUNDS`.
    #[serde(skip_serializing_if = "Option::is_none", alias = "maxSessionRounds")]
    pub max_session_rounds: Option<u32>,
    /// The largest file the agent sends inline (`uploads/v1`; a larger one asks for a slot), in
    /// bytes, 1 to 256 MiB. Root agent only; the card says nothing of it; it wins over `UPLOADS_INLINE_MAX_BYTES`.
    #[serde(
        skip_serializing_if = "Option::is_none",
        alias = "maxInlineUploadBytes"
    )]
    pub max_inline_upload_bytes: Option<u64>,
    /// What the command tools show of an output when a call gives no `output` ([ADR 0088]
    /// decision 3): a default per tool. Root agent only (the tools are the process's); read by
    /// `adam-coder`, the agent that has the three tools.
    ///
    /// [ADR 0088]: https://github.com/vymalo/another-adam-rs/blob/main/docs/decisions/0088-a-tool-result-is-bounded-when-it-is-made-and-a-command-that-succeeds-says-so.md
    #[serde(skip_serializing_if = "Option::is_none")]
    pub output: Option<OutputDef>,
    /// Keys this schema does not know.
    #[serde(flatten)]
    pub extra: BTreeMap<String, Value>,
}

/// What a command's result shows of its output (`limits.output.<tool>`, [ADR 0088] decision 3):
/// the file's word for `adam_code_tools::OutputMode`, which this crate does not depend on.
///
/// The enum is closed on purpose (Rule 3): `adam-coder` maps it onto the tools' with a match that
/// has no wildcard, so a mode added on either side fails to compile there.
///
/// [ADR 0088]: https://github.com/vymalo/another-adam-rs/blob/main/docs/decisions/0088-a-tool-result-is-bounded-when-it-is-made-and-a-command-that-succeeds-says-so.md
#[derive(Debug, Clone, Copy, PartialEq, Eq, Deserialize, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum OutputModeDef {
    /// A command that succeeded is one line, naming the id its output is kept under; one that
    /// failed is shown whole.
    OnError,
    /// The output is shown however the command ended.
    Full,
}

impl OutputModeDef {
    /// The word as the file writes it: `on_error` or `full`.
    #[must_use]
    pub fn as_str(self) -> &'static str {
        match self {
            Self::OnError => "on_error",
            Self::Full => "full",
        }
    }
}

impl fmt::Display for OutputModeDef {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.as_str())
    }
}

/// `limits.output`: the default `output` of each command tool, the one a call that gives none
/// takes ([ADR 0088] decision 3). A tool the block leaves out keeps its own default (`run_checks`
/// and `run`: `on_error`; `run_command`: `full`), and a call's `output` always wins.
///
/// **Unknown keys are refused** (`deny_unknown_fields`): the keys are the three tools' names, and
/// a misspelt one would leave a tool showing what the file says it does not.
///
/// [ADR 0088]: https://github.com/vymalo/another-adam-rs/blob/main/docs/decisions/0088-a-tool-result-is-bounded-when-it-is-made-and-a-command-that-succeeds-says-so.md
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Deserialize, Serialize)]
#[serde(default, deny_unknown_fields)]
pub struct OutputDef {
    /// `run_checks`'s default.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub run_checks: Option<OutputModeDef>,
    /// `run`'s default.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub run: Option<OutputModeDef>,
    /// `run_command`'s default.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub run_command: Option<OutputModeDef>,
}

/// Who may write to a session of a subagent after its first round (`session.mode`, [ADR 0087]
/// decision 2): the file's word for `adam_runtime::SessionMode`, which this crate does not depend
/// on. Narrowest first, as the runtime's: `one_shot`, `sealed`, `open`.
///
/// The enum is closed on purpose (Rule 3): `adam-assembly` maps it onto the runtime's with a match
/// that has no wildcard, so a mode added on either side fails to compile there.
///
/// [ADR 0087]: https://github.com/vymalo/another-adam-rs/blob/main/docs/decisions/0087-every-child-is-a-session-and-the-one-shot-subagent-tool-goes.md
#[derive(Debug, Clone, Copy, PartialEq, Eq, Deserialize, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum SessionModeDef {
    /// The session closes when its first round has ended.
    OneShot,
    /// The session stays open, and only the agent that started it sends it more.
    Sealed,
    /// The session stays open, and the agent and the person send it more.
    Open,
}

impl SessionModeDef {
    /// The word as the file writes it: `one_shot`, `sealed` or `open`.
    #[must_use]
    pub fn as_str(self) -> &'static str {
        match self {
            Self::OneShot => "one_shot",
            Self::Sealed => "sealed",
            Self::Open => "open",
        }
    }
}

impl fmt::Display for SessionModeDef {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.as_str())
    }
}

/// `session:` in a subagent's file ([ADR 0087] decision 4): the defaults of a `subagent_start` that
/// does not say, and the ceiling of one that does. `mode` is both the mode a start gets when it
/// names none and the widest it may ask (a start only narrows it: `open` is wider than `sealed`,
/// which is wider than `one_shot`); `wait` is a default only. Both are optional; a file without the
/// block starts `one_shot`, up to `open`, and does not wait.
///
/// **Unknown keys are refused** (`deny_unknown_fields`): a misspelt `mode` would leave a subagent
/// wider than its file says. A subagent's key: on the root agent it is ignored with a warning.
///
/// [ADR 0087]: https://github.com/vymalo/another-adam-rs/blob/main/docs/decisions/0087-every-child-is-a-session-and-the-one-shot-subagent-tool-goes.md
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Deserialize, Serialize)]
#[serde(default, deny_unknown_fields)]
pub struct SessionDef {
    /// The mode of a start that names none, and the widest one a start may name.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub mode: Option<SessionModeDef>,
    /// Whether a start that does not say waits for the first report.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub wait: Option<bool>,
}

/// One skill advertised on the A2A agent card. **Not** an Agent Skill: the two never map
/// implicitly.
#[derive(Debug, Clone, PartialEq, Eq, Deserialize, Serialize)]
pub struct CardSkill {
    /// Stable identifier.
    pub id: String,
    /// Human-readable name.
    pub name: String,
    /// What the skill does.
    pub description: String,
    /// Free-form tags.
    #[serde(default)]
    pub tags: Vec<String>,
    /// Example prompts.
    #[serde(default)]
    pub examples: Vec<String>,
    /// Keys this schema does not know.
    #[serde(flatten)]
    pub extra: BTreeMap<String, Value>,
}

/// What an authenticated caller sees on top of the public card (`card.extended`): A2A's
/// `GetExtendedAgentCard`. The extended card is the public card with these applied
/// (`adam_a2a::ExtendedCardConfig`): the description replaced when given, the skills added (a skill
/// with the id of a public one replaces it). Put nothing in it you would not give every holder of a
/// bearer token, and no secret: it is a file.
#[derive(Debug, Clone, Default, PartialEq, Eq, Deserialize, Serialize)]
#[serde(default)]
pub struct ExtendedCard {
    /// A longer description, replacing the public one.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub description: Option<String>,
    /// Skills only authenticated callers see.
    pub skills: Vec<CardSkill>,
    /// Keys this schema does not know.
    #[serde(flatten)]
    pub extra: BTreeMap<String, Value>,
}

/// The A2A agent card of the root agent (`adam_a2a::AgentCardConfig`, minus the public URL and
/// the version, which the composition root supplies).
#[derive(Debug, Clone, Default, PartialEq, Eq, Deserialize, Serialize)]
#[serde(default)]
pub struct Card {
    /// The card's name; defaults to the agent's.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub name: Option<String>,
    /// The card's description; defaults to the agent's.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub description: Option<String>,
    /// The skills the card advertises.
    pub skills: Vec<CardSkill>,
    /// What authenticated callers see on top of this (`GetExtendedAgentCard`). Absent: the agent
    /// has no extended card.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub extended: Option<ExtendedCard>,
    /// Keys this schema does not know.
    #[serde(flatten)]
    pub extra: BTreeMap<String, Value>,
}

/// How a folder's own tools are offered (ADR 0066, decision 1).
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Deserialize, Serialize)]
#[serde(rename_all = "lowercase")]
pub enum ToolsLoading {
    /// Every tool is in every request, as before the key existed.
    Eager,
    /// The agent starts lean and loads its tools in groups: the default.
    #[default]
    Lazy,
    /// Lazy, with a group whose loader searches it. Accepted for a deployment that writes it; it
    /// behaves as [`Lazy`](Self::Lazy) until searchable groups land (S2).
    Searchable,
}

impl ToolsLoading {
    /// Whether this is the default (`lazy`), so the key is left out of the JSON and the digest.
    #[must_use]
    pub fn is_lazy(&self) -> bool {
        matches!(self, Self::Lazy)
    }
}

/// One entry of `plugins:` (ADR 0050, slice H6): the name of a compiled plugin the deployment
/// registered, bare or with `options`.
#[derive(Debug, Clone, PartialEq, Eq, Deserialize, Serialize)]
#[serde(untagged)]
pub enum PluginRef {
    /// `- audit`
    Name(String),
    /// `- name: audit` with `options:` of scalars, read as text.
    With {
        /// The plugin's registered name.
        name: String,
        /// Its options. A value that looks like a secret is refused by the loader.
        #[serde(
            default,
            deserialize_with = "scalar_map",
            skip_serializing_if = "BTreeMap::is_empty"
        )]
        options: BTreeMap<String, String>,
    },
}

impl PluginRef {
    /// The plugin's registered name.
    #[must_use]
    pub fn name(&self) -> &str {
        match self {
            Self::Name(name) | Self::With { name, .. } => name,
        }
    }

    /// The plugin's options; none for a bare name.
    #[must_use]
    pub fn options(&self) -> BTreeMap<String, String> {
        match self {
            Self::Name(_) => BTreeMap::new(),
            Self::With { options, .. } => options.clone(),
        }
    }
}

/// The YAML frontmatter of an agent or subagent file.
///
/// Everything is optional at this level; which keys are required depends on the role of the
/// file (a subagent needs a `description`) and is checked by the loader.
#[derive(Debug, Clone, Default, PartialEq, Eq, Deserialize, Serialize)]
#[serde(default)]
pub struct AgentFrontmatter {
    /// The agent's name. Default: the file or directory name (subagents), the composition
    /// root's default (the root agent).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub name: Option<String>,
    /// What the agent does. Required on a subagent (it is the tool description the parent
    /// reads); the root agent uses it for the card.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub description: Option<String>,
    /// The tools the agent gets, by name: a list or a comma-separated string.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub tools: Option<ToolList>,
    /// How the agent's own tools are offered (ADR 0066): `eager` puts every one in every request,
    /// as before the key existed; `lazy` (the default) lets a run load them in groups.
    #[serde(skip_serializing_if = "ToolsLoading::is_lazy")]
    pub tools_loading: ToolsLoading,
    /// The model alias, or `inherit`.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub model: Option<ModelRef>,
    /// The reasoning level this agent's model calls ask for by default (ADR 0063): one of
    /// `none`, `minimal`, `low`, `medium`, `high`, `xhigh`, `max`. The loader keeps the word as
    /// written; the assembly checks it against the levels of the model's profile and reports an
    /// unknown one with the agent and the file.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub reasoning_effort: Option<String>,
    /// The named preset (`presets:` of the deployment's model profiles, ADR 0064) this agent's
    /// model calls start from. The assembly resolves it and checks its effort against the model's
    /// profile; an undeclared name is a bind error naming the agent.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub model_preset: Option<String>,
    /// The model alias this agent's run continues on, once, when the endpoint refuses its model as
    /// locked (ADR 0064, amended 2026-11-23). It must differ from the agent's own alias and be one
    /// the deployment serves; its output cap and reasoning level come from its own profile. A
    /// subagent follows its own file's value; none is inherited.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub model_fallback: Option<String>,
    /// The skills the catalog offers: `all` or a list. Default: every skill of the agent.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub skills: Option<SkillSelection>,
    /// Skills whose full body is put into the prompt instead of the catalog.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub preload_skills: Option<Vec<String>>,
    /// The limits of the loop, with Claude Code's `maxTurns` folded into `max_turns`.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub limits: Option<Limits>,
    /// Claude Code's spelling of `limits.max_turns`; the loader moves it into `limits`.
    #[serde(rename = "maxTurns", skip_serializing)]
    pub(crate) claude_max_turns: Option<u32>,
    /// Defaults for the `{{placeholders}}` of the body. Scalars only, read as text.
    #[serde(
        deserialize_with = "scalar_map",
        skip_serializing_if = "BTreeMap::is_empty"
    )]
    pub vars: BTreeMap<String, String>,
    /// The retry policy per tool, by the tool's name (`<server>__<tool>` for an MCP server's):
    /// [`RetryDef`] (ADR 0061). Each agent sets its own; none inherits. Left out of the JSON (and so
    /// of the digest) when there is none.
    #[serde(skip_serializing_if = "BTreeMap::is_empty")]
    pub retry: BTreeMap<String, RetryDef>,
    /// The compiled plugins the agent runs, by the name the deployment registered them under; its
    /// subagents inherit them (ADR 0050, slice H6). Left out of the JSON (and so of the digest)
    /// when there are none.
    #[serde(skip_serializing_if = "Vec::is_empty")]
    pub plugins: Vec<PluginRef>,
    /// The A2A card (root agent only).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub card: Option<Card>,
    /// Repositories the agent may read beside the one it works on, and never write (root agent
    /// only; the coder serves them as read-only slots). Left out of the JSON (and so of the
    /// digest) when there are none.
    #[serde(skip_serializing_if = "Vec::is_empty")]
    pub references: Vec<ReferenceDef>,
    /// The agent-card URL of a remote subagent (A2A). Makes the file a remote subagent.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub a2a: Option<String>,
    /// How to authenticate to a remote subagent: `bearer:ENV_VAR`.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub auth: Option<String>,
    /// `files: true` on a remote subagent: the file parts of its answer are shared as files of the
    /// calling run instead of being described (an adam extension; remote subagents only).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub files: Option<bool>,
    /// The defaults and the ceiling of the sessions a parent starts on this subagent
    /// ([`SessionDef`], ADR 0087 decision 4); local and remote subagents alike. Left out of the
    /// JSON (and so of the digest) when absent.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub session: Option<SessionDef>,
    /// Free-form map, passed through (an Agent Skills and Copilot convention). Scalars are
    /// read as text; a list or a map is kept as its JSON text.
    #[serde(
        deserialize_with = "lenient_map",
        skip_serializing_if = "BTreeMap::is_empty"
    )]
    pub metadata: BTreeMap<String, String>,
    /// Keys this schema does not know (Claude Code and Copilot keys among them).
    #[serde(flatten)]
    pub extra: BTreeMap<String, Value>,
}

impl AgentFrontmatter {
    /// Move Claude's `maxTurns` into `limits.max_turns`. Returns `true` when both were set to
    /// different values (then `limits.max_turns` wins).
    pub(crate) fn fold_claude_max_turns(&mut self) -> bool {
        let Some(claude) = self.claude_max_turns.take() else {
            return false;
        };
        let limits = self.limits.get_or_insert_with(Limits::default);
        match limits.max_turns {
            Some(existing) => existing != claude,
            None => {
                limits.max_turns = Some(claude);
                false
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_retry_reads_its_durations_and_falls_back_to_the_defaults() {
        assert_eq!(
            retry_duration("500ms"),
            Some(std::time::Duration::from_millis(500))
        );
        assert_eq!(
            retry_duration(" 30m "),
            Some(std::time::Duration::from_secs(1_800))
        );
        assert_eq!(retry_duration("1h"), Some(MAX_RETRY_BACKOFF));
        for bad in [
            "",
            "s",
            "5",
            "1.5s",
            "1 s",
            "1d",
            "-1s",
            "99999999999999999999h",
        ] {
            assert_eq!(retry_duration(bad), None, "{bad:?}");
        }
        let def: RetryDef = serde_json::from_value(serde_json::json!({
            "attempts": 3, "maxBackoff": "2m", "multiplier": 1.5, "on": ["rate-limited"]
        }))
        .unwrap();
        assert_eq!(def.attempts, Some(3));
        assert_eq!(def.initial_backoff(), Some(DEFAULT_RETRY_BACKOFF));
        assert_eq!(def.bound(), Some(std::time::Duration::from_secs(120)));
        assert_eq!(def.factor(), Some(1.5));
        assert_eq!(round_trip(&def), def);
        let plain = RetryDef {
            attempts: Some(2),
            backoff: Some("2m".into()),
            ..RetryDef::default()
        };
        assert_eq!(
            plain.bound(),
            Some(std::time::Duration::from_secs(120)),
            "raised to the first delay"
        );
        assert_eq!(plain.factor(), None);
        let bad = RetryDef {
            backoff: Some("soon".into()),
            ..RetryDef::default()
        };
        assert_eq!((bad.initial_backoff(), bad.bound().is_some()), (None, true));
    }

    #[test]
    fn a_retry_takes_the_forms_a_person_writes() {
        let def: RetryDef = serde_json::from_value(serde_json::json!({
            "attempts": 2, "on": "rate-limited, conflict", "backoff": 30, "max-backoff": "2m",
            "multiplier": 2
        }))
        .unwrap();
        assert_eq!(
            def.on.as_deref(),
            Some(&["rate-limited".to_owned(), "conflict".to_owned()][..])
        );
        assert_eq!(
            def.backoff.as_deref(),
            Some("30"),
            "a bare number is kept, for the loader to refuse"
        );
        assert_eq!(def.max_backoff.as_deref(), Some("2m"));
        assert_eq!(def.factor(), Some(2.0));
        for name in [
            "web_search",
            "context7__resolve-library-id",
            "notion__API-post-search",
            "code-reviewer",
            "9lives",
        ] {
            assert!(is_retry_tool_name(name), "{name}");
        }
        for name in ["", "a b", "a.b", "é", &"x".repeat(65)] {
            assert!(!is_retry_tool_name(name), "{name:?}");
        }
    }

    fn round_trip<T>(value: &T) -> T
    where
        T: Serialize + for<'de> Deserialize<'de>,
    {
        serde_json::from_str(&serde_json::to_string(value).unwrap()).unwrap()
    }

    #[test]
    fn the_closed_enums_read_back_what_they_write() {
        for tools in [
            ToolList::All,
            ToolList::Named(vec![]),
            ToolList::Named(vec!["a".into(), "linear__*".into()]),
        ] {
            assert_eq!(round_trip(&tools), tools);
        }
        for skills in [
            SkillSelection::All,
            SkillSelection::Named(vec![]),
            SkillSelection::Named(vec!["all".into()]),
        ] {
            assert_eq!(round_trip(&skills), skills);
        }
        for model in [ModelRef::Inherit, ModelRef::Alias("coder-large".into())] {
            assert_eq!(round_trip(&model), model);
        }
        for loading in [
            ToolsLoading::Eager,
            ToolsLoading::Lazy,
            ToolsLoading::Searchable,
        ] {
            assert_eq!(round_trip(&loading), loading);
        }
        for mode in [
            SessionModeDef::OneShot,
            SessionModeDef::Sealed,
            SessionModeDef::Open,
        ] {
            assert_eq!(round_trip(&mode), mode);
            assert_eq!(serde_json::to_value(mode).unwrap(), mode.as_str());
        }
    }

    #[test]
    fn a_session_block_reads_its_two_keys_and_refuses_any_other() {
        let front: AgentFrontmatter = serde_json::from_value(serde_json::json!({
            "session": { "mode": "sealed", "wait": true }
        }))
        .unwrap();
        let session = front.session.unwrap();
        assert_eq!(
            (session.mode, session.wait),
            (Some(SessionModeDef::Sealed), Some(true))
        );
        assert_eq!(round_trip(&front), front);
        // An empty block is a block: no default, and nothing written but the key.
        let empty: AgentFrontmatter =
            serde_json::from_value(serde_json::json!({ "session": {} })).unwrap();
        assert_eq!(empty.session, Some(SessionDef::default()));
        assert_eq!(serde_json::to_string(&empty).unwrap(), r#"{"session":{}}"#);
        for (bad, needle) in [
            (
                serde_json::json!({ "mode": "wide" }),
                "unknown variant `wide`",
            ),
            (
                serde_json::json!({ "keep_open": true }),
                "unknown field `keep_open`",
            ),
            (serde_json::json!({ "wait": "yes" }), "invalid type"),
        ] {
            let error =
                serde_json::from_value::<AgentFrontmatter>(serde_json::json!({ "session": bad }))
                    .unwrap_err()
                    .to_string();
            assert!(error.contains(needle), "{error}");
        }
    }

    #[test]
    fn tools_loading_reads_its_three_values_and_defaults_to_lazy() {
        for (text, want) in [
            ("eager", ToolsLoading::Eager),
            ("lazy", ToolsLoading::Lazy),
            ("searchable", ToolsLoading::Searchable),
        ] {
            let front: AgentFrontmatter =
                serde_json::from_value(serde_json::json!({ "tools_loading": text })).unwrap();
            assert_eq!(front.tools_loading, want, "{text}");
        }
        // The key is optional, and its absence is `lazy`.
        assert_eq!(
            AgentFrontmatter::default().tools_loading,
            ToolsLoading::Lazy
        );
        let front: AgentFrontmatter = serde_json::from_value(serde_json::json!({})).unwrap();
        assert_eq!(front.tools_loading, ToolsLoading::Lazy);
        // `lazy` is left out of the JSON, so a folder that does not set the key keeps its digest.
        assert_eq!(
            serde_json::to_string(&AgentFrontmatter::default()).unwrap(),
            "{}"
        );
        let eager: AgentFrontmatter =
            serde_json::from_value(serde_json::json!({ "tools_loading": "eager" })).unwrap();
        assert_eq!(
            serde_json::to_string(&eager).unwrap(),
            r#"{"tools_loading":"eager"}"#
        );
        // Anything else is refused, naming the three values it may be.
        let error = serde_json::from_value::<AgentFrontmatter>(
            serde_json::json!({ "tools_loading": "sometimes" }),
        )
        .unwrap_err()
        .to_string();
        assert!(error.contains("unknown variant `sometimes`"), "{error}");
        assert!(
            error.contains("`eager`") && error.contains("`lazy`") && error.contains("`searchable`"),
            "{error}"
        );
    }

    #[test]
    fn a_full_frontmatter_reads_back_what_it_writes() {
        let front = AgentFrontmatter {
            name: Some("coder".into()),
            description: Some("Codes.".into()),
            tools: Some(ToolList::Named(vec!["a".into()])),
            model: Some(ModelRef::Inherit),
            reasoning_effort: Some("high".into()),
            model_preset: Some("quick".into()),
            skills: Some(SkillSelection::All),
            preload_skills: Some(vec!["s".into()]),
            limits: Some(Limits {
                max_turns: Some(3),
                extra: [("other".to_owned(), serde_json::json!({"x": [1, 2.5, null]}))].into(),
                ..Limits::default()
            }),
            vars: [("n".to_owned(), "3".to_owned())].into(),
            card: Some(Card {
                name: Some("c".into()),
                skills: vec![CardSkill {
                    id: "i".into(),
                    name: "n".into(),
                    description: "d".into(),
                    tags: vec!["t".into()],
                    examples: vec![],
                    extra: Default::default(),
                }],
                ..Card::default()
            }),
            metadata: [("owner".to_owned(), "me".to_owned())].into(),
            extra: [("color".to_owned(), serde_json::json!("blue"))].into(),
            ..AgentFrontmatter::default()
        };
        assert_eq!(round_trip(&front), front);
        assert_eq!(
            round_trip(&AgentFrontmatter::default()),
            AgentFrontmatter::default()
        );
        // Absent keys are not written, so the embedded JSON stays small.
        assert_eq!(
            serde_json::to_string(&AgentFrontmatter::default()).unwrap(),
            "{}"
        );
    }
}
