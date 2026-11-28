//! What a program is and where it is asked: the configuration an embedder passes as a value
//! (ADR 0050, decision 15; the file and the switch that read it are slice P2).

use std::collections::BTreeMap;
use std::path::PathBuf;
use std::str::FromStr;
use std::sync::Arc;
use std::time::Duration;

use adam_plugin::{DeclarationError, Subscriptions};
use serde_json::{Map, Value};

use crate::error::Error;

/// How long a program is given to answer the hello, in milliseconds (ADR 0050, decision 15).
pub const HELLO_DEADLINE_MS: u64 = 5_000;

/// The default deadline of one call, in milliseconds (ADR 0050, decision 8: a program gets 2 s).
pub const DEFAULT_DEADLINE_MS: u64 = 2_000;

/// The longest deadline one call may be given, in milliseconds (ADR 0050, decision 8: at most 30 s).
pub const MAX_DEADLINE_MS: u64 = 30_000;

/// The variables a program gets when it does not inherit the environment: the same list as
/// `adam-mcp`'s `SAFE_VARS`, which is not public (a dependency on `adam-mcp` would bring its whole
/// transport). Keep the two equal: `crates/adam-mcp/src/connection.rs`.
pub const SAFE_VARS: &[&str] = &[
    "PATH",
    "HOME",
    "LANG",
    "TMPDIR",
    #[cfg(windows)]
    "SystemRoot",
    #[cfg(windows)]
    "TEMP",
    #[cfg(windows)]
    "USERPROFILE",
];

/// The variables a program may never be given, whatever the configuration says (ADR 0050,
/// decision 13): the safe ones are the adapter's, and the rest change how a program starts.
/// `adam_workspace::DENIED_ENV` holds the same list for an exec hook; keep the two equal.
const DENIED_ENV: &[&str] = &["PATH", "HOME", "LANG", "TMPDIR", "BASH_ENV", "ENV", "IFS"];

/// Whether `name` is a variable a configuration may add (ADR 0050, decision 13: "a plain
/// upper-case name, never `PATH`, `LD_*`, `BASH_ENV`, `ENV`, `IFS` or a hidden name").
fn check_env_name(name: &str) -> Result<(), String> {
    let plain = !name.is_empty()
        && !name.starts_with(|c: char| c.is_ascii_digit())
        && name
            .chars()
            .all(|c| c.is_ascii_uppercase() || c.is_ascii_digit() || c == '_');
    if !plain {
        return Err(format!(
            "the variable {name:?} is not a plain upper-case name (A-Z, 0-9 and _)"
        ));
    }
    if name.starts_with("LD_") || DENIED_ENV.contains(&name) {
        return Err(format!(
            "the variable {name:?} may not be set for a program: it changes how it starts"
        ));
    }
    Ok(())
}

/// A function that replaces the secrets a deployment holds, as the loop's own scrubber does.
pub type Scrub = dyn Fn(&str) -> String + Send + Sync;

/// A point where a program may decide (ADR 0050, decision 5 and 15). The observe-only events are
/// not here: a program that names one in `decide` is refused when it is configured (exit 78 in a
/// binary).
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub enum DecidePoint {
    /// A person's message is about to reach a model.
    MessageReceived,
    /// A tool call is about to run.
    ToolBefore,
    /// A tool answered.
    ToolAfter,
    /// A model call is about to be made.
    ModelRequest,
    /// A history is about to be summarised.
    CompactionBefore,
    /// The run would end with an answer.
    RunEnding,
}

impl DecidePoint {
    /// Every deciding point, in the loop's order.
    pub const ALL: [DecidePoint; 6] = [
        DecidePoint::MessageReceived,
        DecidePoint::ToolBefore,
        DecidePoint::ToolAfter,
        DecidePoint::ModelRequest,
        DecidePoint::CompactionBefore,
        DecidePoint::RunEnding,
    ];

    /// The point's name, as the protocol and the configuration write it.
    #[must_use]
    pub fn as_str(self) -> &'static str {
        match self {
            DecidePoint::MessageReceived => adam_plugin::MESSAGE_RECEIVED,
            DecidePoint::ToolBefore => "tool.before",
            DecidePoint::ToolAfter => "tool.after",
            DecidePoint::ModelRequest => adam_plugin::MODEL_REQUEST,
            DecidePoint::CompactionBefore => adam_plugin::COMPACTION_BEFORE,
            DecidePoint::RunEnding => adam_plugin::RUN_ENDING,
        }
    }
}

impl std::fmt::Display for DecidePoint {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(self.as_str())
    }
}

impl FromStr for DecidePoint {
    type Err = String;

    /// The point named `s`, or the reason it is not a deciding point: the observe-only events of
    /// the catalogue are refused here (ADR 0050, decision 15: `decide` at an observe-only point is
    /// exit 78 in a binary).
    fn from_str(s: &str) -> Result<Self, Self::Err> {
        match s {
            adam_plugin::MESSAGE_RECEIVED => Ok(DecidePoint::MessageReceived),
            "tool.before" => Ok(DecidePoint::ToolBefore),
            "tool.after" => Ok(DecidePoint::ToolAfter),
            adam_plugin::MODEL_REQUEST => Ok(DecidePoint::ModelRequest),
            adam_plugin::COMPACTION_BEFORE => Ok(DecidePoint::CompactionBefore),
            adam_plugin::RUN_ENDING => Ok(DecidePoint::RunEnding),
            other => Err(format!(
                "{other:?} is not a deciding point (the deciding points are message.received, \
                 tool.before, tool.after, model.request, compaction.before and run.ending)"
            )),
        }
    }
}

/// Where a program is asked: the deciding points it decides at, and the events it observes.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Points {
    /// The deciding points.
    pub decide: Vec<DecidePoint>,
    /// The events it observes, as exact names (`tool.completed`) or patterns (`tool.*`, `*`).
    pub observe: Vec<String>,
}

impl Points {
    /// Every deciding point and no event: what a policy program usually is.
    #[must_use]
    pub fn all_decide() -> Self {
        Points {
            decide: DecidePoint::ALL.to_vec(),
            observe: Vec::new(),
        }
    }

    /// Whether the program decides at `point`.
    #[must_use]
    pub fn decides(&self, point: DecidePoint) -> bool {
        self.decide.contains(&point)
    }

    /// The points as the hello carries them.
    #[must_use]
    pub fn wire(&self) -> crate::protocol::WirePoints {
        crate::protocol::WirePoints {
            decide: self.decide.iter().map(|p| p.as_str().to_owned()).collect(),
            observe: self.observe.clone(),
        }
    }
}

/// Everything the adapter needs to start one program (ADR 0050, decision 15).
///
/// The deployment owns this value: it names the program and its arguments, the environment it adds
/// to the safe variables, the agents it applies to, its points, its deadline and the scrubber its
/// payloads go through. Nothing here is read from an agent file or a repository.
pub struct Config {
    /// The plugin's name, unique in the set and in the protocol's key.
    pub name: String,
    /// The program to run.
    pub program: PathBuf,
    /// Its arguments.
    pub args: Vec<String>,
    /// The variables added to [`SAFE_VARS`]; never a secret (ADR 0050, decision 15).
    pub env: BTreeMap<String, String>,
    /// The agents this program applies to (`None`: all of them). The wiring decides with
    /// [`Config::applies_to`]; the adapter itself is installed per agent.
    pub agents: Option<Vec<String>>,
    /// The empty private directory the program runs in. `None`: the adapter makes one and removes
    /// it when it goes (ADR 0050, decision 15: never the agent's workspace).
    pub cwd: Option<PathBuf>,
    /// Where it is asked.
    pub points: Points,
    /// How long one call may take, between 1 ms and [`MAX_DEADLINE_MS`].
    pub deadline: Duration,
    /// The scrubber its payloads go through. The default replaces nothing: a deployment that holds
    /// secrets passes the redactor it already gives the loop, or its program reads them.
    pub scrub: Arc<Scrub>,
}

impl std::fmt::Debug for Config {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Config")
            .field("name", &self.name)
            .field("program", &self.program)
            .field("args", &self.args)
            .field("env", &self.env.keys().collect::<Vec<_>>())
            .field("agents", &self.agents)
            .field("cwd", &self.cwd)
            .field("points", &self.points)
            .field("deadline", &self.deadline)
            .finish_non_exhaustive()
    }
}

impl Config {
    /// A program called `name` at `program`, with the protocol's defaults: a 2 s deadline, every
    /// deciding point, no observed event, no added variable, no agent filter and a scrubber that
    /// replaces nothing.
    #[must_use]
    pub fn new(name: impl Into<String>, program: impl Into<PathBuf>) -> Self {
        Config {
            name: name.into(),
            program: program.into(),
            args: Vec::new(),
            env: BTreeMap::new(),
            agents: None,
            cwd: None,
            points: Points::all_decide(),
            deadline: Duration::from_millis(DEFAULT_DEADLINE_MS),
            scrub: Arc::new(|text: &str| text.to_owned()),
        }
    }

    /// The same configuration, deciding only at `points`.
    #[must_use]
    pub fn points(mut self, points: Points) -> Self {
        self.points = points;
        self
    }

    /// The same configuration, with `deadline` for one call.
    #[must_use]
    pub fn deadline(mut self, deadline: Duration) -> Self {
        self.deadline = deadline;
        self
    }

    /// The same configuration, running the program in `cwd` (an empty private directory the
    /// deployment owns) instead of one the adapter makes.
    #[must_use]
    pub fn cwd(mut self, cwd: impl Into<PathBuf>) -> Self {
        self.cwd = Some(cwd.into());
        self
    }

    /// The same configuration, scrubbing its payloads with `scrub`.
    #[must_use]
    pub fn scrub(mut self, scrub: impl Fn(&str) -> String + Send + Sync + 'static) -> Self {
        self.scrub = Arc::new(scrub);
        self
    }

    /// The same configuration, adding `name=value` to the program's environment.
    #[must_use]
    pub fn env(mut self, name: impl Into<String>, value: impl Into<String>) -> Self {
        self.env.insert(name.into(), value.into());
        self
    }

    /// The same configuration, with `args` for the program.
    #[must_use]
    pub fn args(mut self, args: impl IntoIterator<Item = impl Into<String>>) -> Self {
        self.args = args.into_iter().map(Into::into).collect();
        self
    }

    /// Whether this program applies to the agent called `agent`.
    #[must_use]
    pub fn applies_to(&self, agent: &str) -> bool {
        match &self.agents {
            None => true,
            Some(agents) => agents.iter().any(|a| a == agent),
        }
    }

    /// The subscriptions this configuration's events make, or the reason one can never match.
    ///
    /// # Errors
    ///
    /// [`Error::Config`] for a pattern that is not an exact name, `ns.*` or `*`.
    pub fn subscriptions(&self) -> Result<Subscriptions, Error> {
        Subscriptions::of(self.points.observe.iter().map(String::as_str)).map_err(|e| {
            Error::Config {
                message: match e {
                    DeclarationError::BadPattern { pattern, .. } => {
                        format!(
                            "the observe pattern {pattern:?} is not an exact name, `ns.*` or `*`"
                        )
                    }
                    other => other.to_string(),
                },
            }
        })
    }

    /// Check the configuration before the program is started.
    ///
    /// # Errors
    ///
    /// [`Error::Config`] for an empty name, an empty program, a deadline outside 1 ms to 30 s, or
    /// an observe pattern that is not a name or a pattern.
    pub fn validate(&self) -> Result<(), Error> {
        if self.name.trim().is_empty() {
            return Err(Error::Config {
                message: "the program needs a name".to_owned(),
            });
        }
        if self.program.as_os_str().is_empty() {
            let name = &self.name;
            return Err(Error::Config {
                message: format!("the program {name:?} names no path"),
            });
        }
        let millis = self.deadline.as_millis();
        if millis == 0 || millis > u128::from(MAX_DEADLINE_MS) {
            let name = &self.name;
            return Err(Error::Config {
                message: format!(
                    "the deadline of {name:?} is {millis} ms; it must be 1 to {MAX_DEADLINE_MS}"
                ),
            });
        }
        for name in self.env.keys() {
            check_env_name(name).map_err(|message| Error::Config { message })?;
        }
        self.subscriptions().map(|_| ())
    }
}

/// `value` with every string inside it, and every key, replaced by `scrub`.
pub(crate) fn scrub_value(value: &Value, scrub: &Scrub) -> Value {
    match value {
        Value::String(text) => Value::String(scrub(text)),
        Value::Array(items) => Value::Array(items.iter().map(|v| scrub_value(v, scrub)).collect()),
        Value::Object(entries) => Value::Object(
            entries
                .iter()
                .map(|(key, value)| (scrub(key), scrub_value(value, scrub)))
                .collect::<Map<String, Value>>(),
        ),
        other => other.clone(),
    }
}
