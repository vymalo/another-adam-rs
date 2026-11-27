//! The deployment's hook programs (ADR 0050, decision 15; slice **P2a**): `ADAM_HOOKS_FILE` and
//! `HOOKS_ALLOW_PROGRAMS`.
//!
//! A hook program is declared **once, by the deployment**, in the file the environment names, and
//! nowhere else: an agent file names compiled plugins only, a repository an agent works on cannot
//! declare one (a file under such a repository is refused), and a message never can. The file is
//! JSON:
//!
//! ```json
//! {
//!   "programs": [
//!     {
//!       "name": "no-push",
//!       "command": "/usr/local/bin/no-push",
//!       "args": ["--strict"],
//!       "env": { "POLICY_MODE": "strict" },
//!       "agents": ["coder"],
//!       "points": { "decide": ["tool.before"], "observe": ["tool.completed"] },
//!       "deadlineMs": 2000
//!     }
//!   ]
//! }
//! ```
//!
//! `agents` is the filter: the names of the agents the program applies to, absent for all of them.
//! `points.decide` names deciding points only (`message.received`, `tool.before`, `tool.after`,
//! `model.request`, `compaction.before`, `run.ending`); an observe-only event there is refused. A
//! relative `command` is resolved against the file's own directory, never the process's, because a
//! program runs in an empty private directory. Every rule of a program's name, deadline, environment
//! and observed events is [`adam_hook_exec::Config::validate`]'s, so the file and an embedder's value
//! cannot disagree.
//!
//! **The switch.** A file that names programs is refused (exit 78 in a binary) unless
//! `HOOKS_ALLOW_PROGRAMS=true`, as a deployment that allows stdio MCP servers must say so.

use std::collections::{BTreeMap, BTreeSet};
use std::path::{Path, PathBuf};
use std::time::Duration;

use adam_hook_exec::{Config, DecidePoint, Points};
use serde::Deserialize;

use crate::config::{ConfigError, parse_flag};

/// `ADAM_HOOKS_FILE`: the deployment's hook programs, a JSON file.
pub const HOOKS_FILE: &str = "ADAM_HOOKS_FILE";

/// `HOOKS_ALLOW_PROGRAMS`: `true` lets the process run hook programs at all.
pub const HOOKS_ALLOW: &str = "HOOKS_ALLOW_PROGRAMS";

/// The deadline a program gets when the file does not say: [`adam_hook_exec::DEFAULT_DEADLINE_MS`].
const DEFAULT_DEADLINE_MS: u64 = adam_hook_exec::DEFAULT_DEADLINE_MS;

/// One hook program, as the deployment's file declares it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Program {
    /// The name the file gives it: the protocol's key and the log line both use it.
    pub name: String,
    /// The program to run; a relative one is resolved against the file's directory.
    pub command: PathBuf,
    /// Its arguments.
    pub args: Vec<String>,
    /// The variables added to the safe ones; never a secret.
    pub env: BTreeMap<String, String>,
    /// The agents it applies to; `None` is all of them.
    pub agents: Option<Vec<String>>,
    /// Where it is asked.
    pub points: Points,
    /// How long one call may take.
    pub deadline: Duration,
}

impl Program {
    /// Whether this program applies to the agent called `agent`.
    #[must_use]
    pub fn applies_to(&self, agent: &str) -> bool {
        match &self.agents {
            None => true,
            Some(agents) => agents.iter().any(|a| a == agent),
        }
    }

    /// The value an embedding host starts: the program, its points and its deadline, with the
    /// deployment's scrubber (the caller sets it; this value replaces nothing).
    #[must_use]
    pub fn config(&self) -> Config {
        let mut config = Config::new(self.name.clone(), self.command.clone())
            .args(self.args.clone())
            .points(self.points.clone())
            .deadline(self.deadline);
        for (name, value) in &self.env {
            config = config.env(name.clone(), value.clone());
        }
        config
    }
}

/// The hook programs a deployment declared, or none.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct HooksSettings {
    programs: Vec<Program>,
}

impl HooksSettings {
    /// The programs of the environment, with no directory treated as a repository the agent works
    /// on. Every problem is a line in `problems`, which a binary turns into a [`ConfigError`].
    #[must_use]
    pub fn parse(lookup: &impl Fn(&str) -> Option<String>, problems: &mut Vec<String>) -> Self {
        Self::parse_in(lookup, &[], problems)
    }

    /// The programs of the environment, refusing a file that lives under one of `protected` (the
    /// agent's folder, the workspace a run works in). Every problem is a line in `problems`, which a
    /// binary turns into a [`ConfigError`] (exit 78).
    #[must_use]
    pub fn parse_in(
        lookup: &impl Fn(&str) -> Option<String>,
        protected: &[PathBuf],
        problems: &mut Vec<String>,
    ) -> Self {
        let get = |name: &str| lookup(name).filter(|v| !v.trim().is_empty());
        let Some(named) = get(HOOKS_FILE) else {
            return Self::default();
        };
        if !parse_flag(&get, HOOKS_ALLOW, problems) {
            problems.push(format!(
                "{HOOKS_FILE} names hook programs, and {HOOKS_ALLOW} is not true: a deployment that \
                 lets a program decide must say so (ADR 0050)"
            ));
            return Self::default();
        }
        let path = PathBuf::from(named.trim());
        if !path.is_file() {
            problems.push(format!(
                "{HOOKS_FILE} {:?} is not a file (name an existing file, or unset it)",
                path.display().to_string()
            ));
            return Self::default();
        }
        if let Some(root) = inside(&path, protected) {
            problems.push(format!(
                "{HOOKS_FILE} {:?} is inside {:?}: a repository an agent works on never declares a \
                 program (ADR 0050)",
                path.display().to_string(),
                root.display().to_string()
            ));
            return Self::default();
        }
        let raw = match std::fs::read_to_string(&path) {
            Ok(raw) => raw,
            Err(e) => {
                problems.push(format!(
                    "{HOOKS_FILE} {:?} cannot be read: {e}",
                    path.display().to_string()
                ));
                return Self::default();
            }
        };
        Self::parse_file(&path, &raw, problems)
    }

    /// The file's contents, validated. `path` is only for the messages and for resolving a relative
    /// command.
    fn parse_file(path: &Path, raw: &str, problems: &mut Vec<String>) -> Self {
        let file: File = match serde_json::from_str(raw) {
            Ok(file) => file,
            Err(e) => {
                problems.push(format!(
                    "{HOOKS_FILE} {:?} is not a valid file ({} at line {} column {})",
                    path.display().to_string(),
                    if e.is_data() {
                        "a member is wrong"
                    } else {
                        "it does not parse"
                    },
                    e.line(),
                    e.column()
                ));
                return Self::default();
            }
        };
        let base = path.parent().unwrap_or_else(|| Path::new("."));
        let mut programs = Vec::new();
        let mut seen = BTreeSet::new();
        for entry in file.programs {
            let name = entry.name.trim().to_owned();
            if name.is_empty() {
                problems.push(format!(
                    "{HOOKS_FILE} has a program with no name (every program needs one)"
                ));
                continue;
            }
            if !seen.insert(name.clone()) {
                problems.push(format!("{HOOKS_FILE} names the program {name:?} twice"));
                continue;
            }
            if entry.command.as_os_str().is_empty() {
                problems.push(format!(
                    "{HOOKS_FILE}: the program {name:?} names no command"
                ));
                continue;
            }
            let command = if entry.command.is_absolute() {
                entry.command
            } else {
                base.join(entry.command)
            };
            let mut decide = Vec::with_capacity(entry.points.decide.len());
            let mut refused = false;
            for point in &entry.points.decide {
                match point.parse::<DecidePoint>() {
                    Ok(point) => decide.push(point),
                    Err(reason) => {
                        problems.push(format!(
                            "{HOOKS_FILE}: the program {name:?} decides at {reason}"
                        ));
                        refused = true;
                    }
                }
            }
            if let Some(agents) = &entry.agents
                && agents.iter().all(|a| a.trim().is_empty())
            {
                problems.push(format!(
                    "{HOOKS_FILE}: the program {name:?} names no agent in `agents` (leave it out to \
                     apply to every agent)"
                ));
                refused = true;
            }
            if refused {
                continue;
            }
            let program = Program {
                name: name.clone(),
                command,
                args: entry.args,
                env: entry.env,
                agents: entry.agents,
                points: Points {
                    decide,
                    observe: entry.points.observe,
                },
                deadline: Duration::from_millis(entry.deadline_ms),
            };
            // The crate's rules, so the file and an embedder's value cannot disagree: a deadline
            // out of bounds, a variable that would change how the program starts, an observe
            // pattern that is not one.
            if let Err(e) = program.config().validate() {
                problems.push(format!(
                    "{HOOKS_FILE}: the program {name:?} is unusable: {e}"
                ));
                continue;
            }
            programs.push(program);
        }
        HooksSettings { programs }
    }

    /// The programs, in the file's order.
    #[must_use]
    pub fn programs(&self) -> &[Program] {
        &self.programs
    }

    /// The programs that apply to the agent called `agent`, in the file's order.
    pub fn for_agent<'a>(&'a self, agent: &'a str) -> impl Iterator<Item = &'a Program> + 'a {
        self.programs.iter().filter(move |p| p.applies_to(agent))
    }

    /// Whether the deployment declared no program.
    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.programs.is_empty()
    }
}

/// The protected root `path` is under, when it is under one.
fn inside(path: &Path, protected: &[PathBuf]) -> Option<PathBuf> {
    let path = std::fs::canonicalize(path).unwrap_or_else(|_| path.to_path_buf());
    protected
        .iter()
        .find(|root| {
            let root = std::fs::canonicalize(root).unwrap_or_else(|_| root.to_path_buf());
            path.starts_with(root)
        })
        .cloned()
}

/// The file, as it is written.
#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
struct File {
    #[serde(default)]
    programs: Vec<Entry>,
}

#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
struct Entry {
    name: String,
    command: PathBuf,
    #[serde(default)]
    args: Vec<String>,
    #[serde(default)]
    env: BTreeMap<String, String>,
    #[serde(default)]
    agents: Option<Vec<String>>,
    #[serde(default)]
    points: EntryPoints,
    #[serde(default = "default_deadline_ms")]
    deadline_ms: u64,
}

#[derive(Debug, Default, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
struct EntryPoints {
    #[serde(default)]
    decide: Vec<String>,
    #[serde(default)]
    observe: Vec<String>,
}

const fn default_deadline_ms() -> u64 {
    DEFAULT_DEADLINE_MS
}

/// The settings, or the error a binary turns into exit 78.
///
/// # Errors
///
/// [`ConfigError`] listing every problem of the file and the switch.
pub fn settings(
    lookup: &impl Fn(&str) -> Option<String>,
    protected: &[PathBuf],
) -> Result<HooksSettings, ConfigError> {
    let mut problems = Vec::new();
    let settings = HooksSettings::parse_in(lookup, protected, &mut problems);
    ConfigError::check(problems)?;
    Ok(settings)
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used, clippy::expect_used)]

    use super::*;
    use std::collections::HashMap;

    fn env(pairs: &[(&str, &str)]) -> impl Fn(&str) -> Option<String> {
        let map: HashMap<String, String> = pairs
            .iter()
            .map(|(k, v)| ((*k).to_owned(), (*v).to_owned()))
            .collect();
        move |name: &str| map.get(name).cloned()
    }

    fn write(dir: &Path, name: &str, body: &str) -> PathBuf {
        let path = dir.join(name);
        std::fs::write(&path, body).expect("the file is written");
        path
    }

    fn one_program(dir: &Path) -> PathBuf {
        write(
            dir,
            "hooks.json",
            r#"{"programs":[{"name":"policy","command":"/usr/local/bin/policy",
                "points":{"decide":["tool.before"],"observe":["tool.completed"]},
                "agents":["coder"]}]}"#,
        )
    }

    fn parse(pairs: &[(&str, &str)]) -> (HooksSettings, Vec<String>) {
        let mut problems = Vec::new();
        let settings = HooksSettings::parse(&env(pairs), &mut problems);
        (settings, problems)
    }

    #[test]
    fn a_file_round_trips_and_filters_by_agent() {
        let dir = tempfile::tempdir().unwrap();
        let path = one_program(dir.path());
        let (settings, problems) =
            parse(&[(HOOKS_FILE, path.to_str().unwrap()), (HOOKS_ALLOW, "true")]);
        assert!(problems.is_empty(), "{problems:?}");
        assert_eq!(settings.programs().len(), 1);
        let program = &settings.programs()[0];
        assert_eq!(program.name, "policy");
        assert_eq!(program.deadline, Duration::from_millis(DEFAULT_DEADLINE_MS));
        assert_eq!(program.points.decide, vec![DecidePoint::ToolBefore]);
        assert_eq!(program.points.observe, vec!["tool.completed".to_owned()]);
        assert_eq!(settings.for_agent("coder").count(), 1);
        assert_eq!(settings.for_agent("assistant").count(), 0);
    }

    #[test]
    fn a_file_without_the_switch_is_refused() {
        let dir = tempfile::tempdir().unwrap();
        let path = one_program(dir.path());
        let (settings, problems) = parse(&[(HOOKS_FILE, path.to_str().unwrap())]);
        assert!(settings.is_empty());
        assert_eq!(problems.len(), 1, "{problems:?}");
        assert!(problems[0].contains(HOOKS_ALLOW), "{problems:?}");
        // A binary turns these into exit 78: the class is Invalid.
        let error = ConfigError::check(problems).unwrap_err();
        assert_eq!(
            adam_error::Classify::class(&error),
            adam_error::ErrorClass::Invalid
        );
    }

    #[test]
    fn a_switch_alone_loads_nothing() {
        // Nothing is discovered: no cwd, no agent folder, no message.
        let dir = tempfile::tempdir().unwrap();
        write(dir.path(), "hooks.json", r#"{"programs":[]}"#);
        let (settings, problems) = parse(&[(HOOKS_ALLOW, "true")]);
        assert!(settings.is_empty());
        assert!(problems.is_empty(), "{problems:?}");
    }

    #[test]
    fn a_file_inside_a_repository_the_agent_works_on_is_refused() {
        let dir = tempfile::tempdir().unwrap();
        let path = one_program(dir.path());
        let mut problems = Vec::new();
        let settings = HooksSettings::parse_in(
            &env(&[(HOOKS_FILE, path.to_str().unwrap()), (HOOKS_ALLOW, "true")]),
            &[dir.path().to_path_buf()],
            &mut problems,
        );
        assert!(settings.is_empty());
        assert_eq!(problems.len(), 1, "{problems:?}");
        assert!(
            problems[0].contains("never declares a program"),
            "{problems:?}"
        );
    }

    #[test]
    fn a_decide_point_that_is_not_deciding_is_refused() {
        let dir = tempfile::tempdir().unwrap();
        let path = write(
            dir.path(),
            "hooks.json",
            r#"{"programs":[{"name":"policy","command":"/bin/policy",
                "points":{"decide":["tool.completed"]}}]}"#,
        );
        let (settings, problems) =
            parse(&[(HOOKS_FILE, path.to_str().unwrap()), (HOOKS_ALLOW, "true")]);
        assert!(settings.is_empty());
        assert_eq!(problems.len(), 1, "{problems:?}");
        assert!(problems[0].contains("not a deciding point"), "{problems:?}");
    }

    #[test]
    fn a_variable_that_changes_how_a_program_starts_is_refused() {
        let dir = tempfile::tempdir().unwrap();
        let path = write(
            dir.path(),
            "hooks.json",
            r#"{"programs":[{"name":"policy","command":"/bin/policy","env":{"PATH":"/tmp"}}]}"#,
        );
        let (settings, problems) =
            parse(&[(HOOKS_FILE, path.to_str().unwrap()), (HOOKS_ALLOW, "true")]);
        assert!(settings.is_empty());
        assert_eq!(problems.len(), 1, "{problems:?}");
        assert!(problems[0].contains("PATH"), "{problems:?}");
    }

    #[test]
    fn an_unknown_member_is_refused() {
        let dir = tempfile::tempdir().unwrap();
        let path = write(
            dir.path(),
            "hooks.json",
            r#"{"programs":[{"name":"policy","command":"/bin/policy","alow":true}]}"#,
        );
        let (settings, problems) =
            parse(&[(HOOKS_FILE, path.to_str().unwrap()), (HOOKS_ALLOW, "true")]);
        assert!(settings.is_empty());
        assert_eq!(problems.len(), 1, "{problems:?}");
        assert!(problems[0].contains("alow"), "{problems:?}");
    }

    #[test]
    fn a_relative_command_is_resolved_against_the_files_directory() {
        let dir = tempfile::tempdir().unwrap();
        let path = write(
            dir.path(),
            "hooks.json",
            r#"{"programs":[{"name":"policy","command":"./policy"}]}"#,
        );
        let (settings, problems) =
            parse(&[(HOOKS_FILE, path.to_str().unwrap()), (HOOKS_ALLOW, "true")]);
        assert!(problems.is_empty(), "{problems:?}");
        assert_eq!(settings.programs()[0].command, dir.path().join("./policy"));
    }

    #[test]
    fn a_file_that_is_not_a_file_is_a_problem() {
        let (settings, problems) = parse(&[
            (HOOKS_FILE, "/nonexistent/hooks.json"),
            (HOOKS_ALLOW, "true"),
        ]);
        assert!(settings.is_empty());
        assert_eq!(problems.len(), 1, "{problems:?}");
        assert!(problems[0].contains("is not a file"), "{problems:?}");
    }
}
