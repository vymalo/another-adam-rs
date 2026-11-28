//! The deployment's hook programs (ADR 0050, decision 15; slices **P2a** and **P2b**):
//! `ADAM_HOOKS_FILE` and `HOOKS_ALLOW_PROGRAMS`.
//!
//! A hook program is declared **once, by the deployment**, in the file the environment names, and
//! nowhere else: an agent file names compiled plugins only, a repository an agent works on cannot
//! declare one, and a message never can. A file inside a directory an agent works on is refused by
//! [`HooksSettings::parse_in`](HooksSettings::parse_in) when the caller passes those roots as
//! `protected` — the refusal is the embedding host's to apply, and no binary passes a root today.
//! The file is JSON:
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
//!
//! **Wired (slice P2b, ANR-1301).** A binary that runs workers turns the programs that apply to
//! the agent it serves into the host's hook set at startup
//! ([`HooksSettings::hook_set_for`](HooksSettings::hook_set_for)), and the assembly gives that set
//! to the root and every subagent. A program that cannot start is exit 78, naming the program.

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
                // A data error names the member serde refused (`unknown field \`alow\``); the
                // test that an unknown member is refused asserts on that name, so keep it.
                let detail = if e.is_data() {
                    e.to_string()
                } else {
                    format!(
                        "it does not parse at line {} column {}",
                        e.line(),
                        e.column()
                    )
                };
                problems.push(format!(
                    "{HOOKS_FILE} {:?} is not a valid file ({detail})",
                    path.display().to_string()
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

    /// The host's hooks for the agent called `agent` (ADR 0050, slice **P2b**): the programs that
    /// apply to it, started and asked hello, in the file's order. Empty when the deployment
    /// declared none, which is the same as no file at all: a process without a file behaves as it
    /// did before this slice.
    ///
    /// `scrub` is the deployment's scrubber, applied to every payload a program is given and to
    /// the reason it answers with (a program sees what a step would, never a secret).
    ///
    /// `agent` is **the root agent's name**, and the set then applies to every agent of the
    /// assembly, the root and its subagents: a set cannot be narrower than the assembly it is
    /// installed on (ADR 0050, slice P2b). A program whose `agents` does not name the root agent is
    /// therefore not started, and a warning names it — an `agents` list naming only a subagent is
    /// not started either.
    ///
    /// The set's [`HookLimits`](adam_plugin::HookLimits) are one retry and **the greatest deadline
    /// among the programs it started**, at least the default's: a program is enforced by the
    /// adapter at its own deadline, and a set shorter than that would refuse a compliant answer
    /// (ADR 0050: the host builds the set with `HookSet::with_limits`).
    ///
    /// # Errors
    ///
    /// The first program that cannot be started: the error names the program and says why (a
    /// path that is not there, a hello that is not answered, another protocol). A binary reports
    /// it as exit 78: a deployment that declared a program and cannot run it does not serve.
    pub fn hook_set_for(
        &self,
        agent: &str,
        scrub: impl Fn(&str) -> String + Send + Sync + 'static,
    ) -> Result<adam_plugin::HookSet, adam_hook_exec::Error> {
        let scrub: std::sync::Arc<dyn Fn(&str) -> String + Send + Sync> =
            std::sync::Arc::new(scrub);
        let mut set = adam_plugin::HookSet::new();
        let mut limits = adam_plugin::HookLimits::default();
        for program in self.programs() {
            if !program.applies_to(agent) {
                tracing::warn!(
                    program = %program.name,
                    agents = ?program.agents,
                    served = agent,
                    "a hook program names agents this process does not serve: it was not started \
                     (the file's filter is the root agent's name, and its set applies to every \
                     agent of the assembly)"
                );
                continue;
            }
            let scrub = std::sync::Arc::clone(&scrub);
            let started = adam_hook_exec::HookExec::start(
                program.config().scrub(move |text: &str| scrub(text)),
            )?;
            limits.deadline = limits.deadline.max(program.deadline);
            set = set.with(std::sync::Arc::new(started));
        }
        if set.is_empty() {
            return Ok(set);
        }
        let set = set.with_limits(limits);
        tracing::info!(
            programs = ?set.names(),
            deadline_ms = limits.deadline.as_millis() as u64,
            retries = limits.retries,
            agent,
            "the deployment's hook programs started"
        );
        Ok(set)
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

    /// Slice P2b: the host's set is the programs that apply to the agent it serves, started; a
    /// program that cannot start names itself and is `Invalid` (exit 78 in a binary).
    #[test]
    fn the_hosts_set_is_the_programs_that_apply_and_a_broken_one_names_itself() {
        let dir = tempfile::tempdir().unwrap();
        let path = write(
            dir.path(),
            "hooks.json",
            r#"{"programs":[{"name":"no-push","command":"./missing","agents":["coder"],
                "points":{"decide":["tool.before"]}}]}"#,
        );
        let (settings, problems) =
            parse(&[(HOOKS_FILE, path.to_str().unwrap()), (HOOKS_ALLOW, "true")]);
        assert!(problems.is_empty(), "{problems:?}");
        // A program filtered to another agent is not started, and the set is empty.
        assert!(
            settings
                .hook_set_for("assistant", str::to_owned)
                .expect("no program applies")
                .is_empty()
        );
        let error = settings
            .hook_set_for("coder", str::to_owned)
            .expect_err("the program is not there");
        assert!(error.to_string().contains("no-push"), "{error}");
        assert_eq!(
            adam_error::Classify::class(&error),
            adam_error::ErrorClass::Invalid
        );
    }

    /// Slice P2b, the set's deadline: a program that keeps its own 2 s is refused by a set built
    /// with `HookLimits::default()` (1 s), so `hook_set_for` gives the set the greatest deadline of
    /// the programs it started. The program here is a shell script that answers the hello and
    /// nothing else; it never answers an event, which this test does not ask it.
    #[cfg(unix)]
    #[test]
    fn the_sets_deadline_covers_the_slowest_program() {
        use std::os::unix::fs::PermissionsExt as _;

        let dir = tempfile::tempdir().unwrap();
        let script = r#"#!/bin/sh
while IFS= read -r line; do
  case $line in
    *hello*) printf '%s\n' '{"protocol":"adam-hooks/1","name":"slow","version":1}' ;;
    *shutdown*) exit 0 ;;
  esac
done
"#;
        let program = dir.path().join("slow.sh");
        std::fs::write(&program, script).unwrap();
        let mut mode = std::fs::metadata(&program).unwrap().permissions();
        mode.set_mode(mode.mode() | 0o111);
        std::fs::set_permissions(&program, mode).unwrap();
        let path = write(
            dir.path(),
            "hooks.json",
            r#"{"programs":[{"name":"slow","command":"./slow.sh","deadlineMs":2000,
                "points":{"decide":["tool.before"]}}]}"#,
        );
        let (settings, problems) =
            parse(&[(HOOKS_FILE, path.to_str().unwrap()), (HOOKS_ALLOW, "true")]);
        assert!(problems.is_empty(), "{problems:?}");
        let set = settings
            .hook_set_for("coder", str::to_owned)
            .expect("the script answers the hello");
        assert_eq!(set.names(), vec!["slow"]);
        // The bug this fixes: a default set refuses a compliant 2 s answer before the adapter's
        // own deadline (ADR 0050, the P1 note).
        assert!(
            adam_plugin::HookLimits::default().deadline < std::time::Duration::from_secs(2),
            "a default set no longer undercuts 2 s: this test no longer proves the fix"
        );
        assert!(
            set.limits().deadline >= std::time::Duration::from_secs(2),
            "the set's deadline is {:?}, want at least 2 s",
            set.limits().deadline
        );
        assert_eq!(set.limits().retries, 1);
    }
}
